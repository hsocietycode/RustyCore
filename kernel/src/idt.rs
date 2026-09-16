//! IDT: CPU exception handlers + hardware IRQ handlers.
//!
//! Vectors 0..=31 are CPU exceptions (breakpoint, double fault, page fault,
//! ...). Vectors 32..=47 are the remapped 8259 PIC (timer on 32, keyboard
//! on 33). High vectors are APIC sidecars (Step 2 registers them, Step 5
//! fires them): 0x80 syscall stub, 0xEF LAPIC timer, 0xFE LAPIC error.
//! Anything else that fires lands in the GP handler and halts —
//! loud failure beats silent corruption.

use crate::gdt;
use lazy_static::lazy_static;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

/// PIC-remapped hardware IRQ vectors.
pub const TIMER_VECTOR: u8 = 32;
pub const KEYBOARD_VECTOR: u8 = 33;

/// APIC sidecar vectors (high range — clear of CPU exceptions and the PIC).
pub const SYSCALL_VECTOR: u8 = 0x80;
pub const LAPIC_TIMER_VECTOR: u8 = 0xEF;
pub const LAPIC_ERROR_VECTOR: u8 = 0xFE;

/// Spurious vectors that can fire while masked — they MUST have handlers.
/// The APIC spurious vector (`apic::init` programs SVR=0x1FF → vector 0xFF)
/// and the classic PIC phantom IRQs (master IRQ7 → 39, slave IRQ15 → 47).
/// Any OTHER unregistered vector that fires #GPs into the loud GP handler —
/// that containment is by design, but these three are architecturally
/// expected noise, not faults.
pub const SPURIOUS_APIC_VECTOR: u8 = 0xFF;
pub const SPURIOUS_PIC_MASTER_VECTOR: u8 = 39;
pub const SPURIOUS_PIC_SLAVE_VECTOR: u8 = 47;

/// Timer ticks since `timer::init`. Written by the IRQ handler, read by main.
pub static TIMER_TICKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// LAPIC timer ticks since Step 5. Same counter discipline as TIMER_TICKS.
pub static APIC_TICKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// `int 0x80` hits since boot. Kernel-only stub for now (Ring0, no STAR/LSTAR).
pub static SYSCALL_HITS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Soak/promotion thresholds: how many ticks on EACH clock prove dual-life
/// (Step 5 gate) and LAPIC mastery (Step 7 watch). Single source of truth —
/// main reads these, never magic 100s.
pub const SOAK_TICKS_EACH: u64 = 100;

/// Spurious IRQ hits since boot (0xFF APIC + PIC phantom IRQ7/IRQ15).
/// Expected noise, not faults — but counted, so silence-vs-noise is visible.
pub static SPURIOUS_HITS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault_handler)
                .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
        }
        idt.page_fault.set_handler_fn(page_fault_handler);
        idt.general_protection_fault.set_handler_fn(gp_handler);
        idt.stack_segment_fault
            .set_handler_fn(stack_segment_handler);
        idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
        idt.divide_error.set_handler_fn(divide_error_handler);
        idt[TIMER_VECTOR].set_handler_fn(timer_handler);
        idt[KEYBOARD_VECTOR].set_handler_fn(keyboard_handler);
        // APIC sidecars (Step 2): registered but silent until Step 5 maps
        // and enables the LAPIC. DPL stays Ring0 — no userspace yet.
        idt[SYSCALL_VECTOR].set_handler_fn(syscall_stub_handler);
        idt[LAPIC_TIMER_VECTOR].set_handler_fn(apic_timer_handler);
        idt[LAPIC_ERROR_VECTOR].set_handler_fn(apic_error_handler);
        // Spurious sinks (architecturally expected noise, not faults): the
        // 0xFF APIC spurious vector SVR points at, and the two classic PIC
        // phantom IRQs (master IRQ7 → 39, slave IRQ15 → 47). Count and EOI
        // as spurious — never let phantom noise become a GP panic.
        idt[SPURIOUS_APIC_VECTOR].set_handler_fn(spurious_handler);
        idt[SPURIOUS_PIC_MASTER_VECTOR].set_handler_fn(spurious_pic_master_handler);
        idt[SPURIOUS_PIC_SLAVE_VECTOR].set_handler_fn(spurious_pic_slave_handler);
        idt
    };
}

/// Load the IDT. Call after [`crate::gdt::init`].
pub fn init() {
    IDT.load();
}

extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    crate::serial_println!("EXCEPTION: breakpoint\n{:#?}", stack_frame);
}

extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) -> ! {
    panic!("EXCEPTION: double fault (code {error_code})\n{stack_frame:#?}");
}

extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    use x86_64::registers::control::Cr2;
    panic!(
        "EXCEPTION: page fault accessing {:#x} ({:?})\n{stack_frame:#?}",
        Cr2::read_raw(),
        error_code,
    );
}

extern "x86-interrupt" fn gp_handler(stack_frame: InterruptStackFrame, error_code: u64) {
    panic!("EXCEPTION: general protection fault (code {error_code:#x})\n{stack_frame:#?}");
}

extern "x86-interrupt" fn stack_segment_handler(stack_frame: InterruptStackFrame, error_code: u64) {
    panic!("EXCEPTION: stack segment fault (code {error_code:#x})\n{stack_frame:#?}");
}

extern "x86-interrupt" fn invalid_opcode_handler(stack_frame: InterruptStackFrame) {
    panic!("EXCEPTION: invalid opcode\n{stack_frame:#?}");
}

extern "x86-interrupt" fn divide_error_handler(stack_frame: InterruptStackFrame) {
    panic!("EXCEPTION: divide error\n{stack_frame:#?}");
}

extern "x86-interrupt" fn timer_handler(_stack_frame: InterruptStackFrame) {
    use core::sync::atomic::Ordering;
    // Relaxed is honest here: single-CPU kernel (APs not started yet), the
    // counter is write-only from this handler, read-only from main. When SMP
    // lands, revisit — cross-CPU tick reads will want Acquire.
    TIMER_TICKS.fetch_add(1, Ordering::Relaxed);
    unsafe {
        crate::interrupts::PICS
            .lock()
            .notify_end_of_interrupt(TIMER_VECTOR);
    }
}

extern "x86-interrupt" fn keyboard_handler(_stack_frame: InterruptStackFrame) {
    use x86_64::instructions::port::Port;
    // Drain the scancode so the controller stops asserting IRQ1, then EOI.
    // (No decoding yet — Phase 4 shell will do that.)
    let scancode = unsafe { Port::<u8>::new(0x60).read() };
    crate::serial_println!("keyboard: scancode {:#x} (ignored for now)", scancode);
    unsafe {
        crate::interrupts::PICS
            .lock()
            .notify_end_of_interrupt(KEYBOARD_VECTOR);
    }
}

/// Kernel-only syscall stub (`int 0x80`). Counts hits; real dispatch with
/// STAR/LSTAR + ring-3 entry lands in a later step. EOI is a PIC no-op by
/// design: vector 0x80 is software-raised, the PIC never sees it.
extern "x86-interrupt" fn syscall_stub_handler(_stack_frame: InterruptStackFrame) {
    use core::sync::atomic::Ordering;
    let n = SYSCALL_HITS.fetch_add(1, Ordering::Relaxed) + 1;
    crate::serial_println!("syscall: stub hit count={}", n);
}

/// LAPIC timer (Step 5 fires this). Counts ticks, then EOI straight to the
/// local APIC — the 8259 never sees LAPIC vectors, so no PIC ack here.
///
/// Cannot fire before Step 5 by hardware contract: LVT entries reset masked
/// and SVR resets disabled, so no local-APIC source can raise until bring-up
/// unmasks them. If one ever does slip through early (stray 0xEF in the gap
/// between `idt::init` and `apic::init`), the `Err` path logs loudly instead
/// of panicking inside the interrupt — a panic here would double-panic.
extern "x86-interrupt" fn apic_timer_handler(_stack_frame: InterruptStackFrame) {
    use core::sync::atomic::Ordering;
    APIC_TICKS.fetch_add(1, Ordering::Relaxed);
    // Phase 3 Step 2b: feed the preemption policy its clock. Cheap atomics
    // only — no locking, no printing at IRQ time (serial reentrancy with
    // task prints would garble the log; the handler also runs on task
    // stacks now, so minimal frame usage matters). The driver observes it.
    crate::task::timer_tick();
    if let Err(e) = crate::apic::eoi() {
        crate::serial_println!("apic-timer: stray fire ({}), ignored", e);
    }
}

/// LAPIC error (ESR). Loud by design: print and continue — a masked error
/// vector that nobody reads is how silent interrupt loss starts.
extern "x86-interrupt" fn apic_error_handler(_stack_frame: InterruptStackFrame) {
    crate::serial_println!("apic: error interrupt (ESR unread in this step)");
    if let Err(e) = crate::apic::eoi() {
        crate::serial_println!("apic-error: stray fire ({}), ignored", e);
    }
}

/// Spurious sinks: 0xFF (APIC), IRQ7/IRQ15 phantoms (PIC). Count and EOI —
/// never panic on expected hardware noise.
///
/// The one asymmetry that matters: a master-IRQ7 phantom needs NO EOI (the
/// 8259 never really raised it — acking would corrupt the in-service state
/// machine), so `spurious_pic_master_handler` skips the ack deliberately,
/// while the slave-IRQ15 phantom DOES need a master EOI (the cascade reached
/// the slave). This is the classic 8259 errata dance, not an omission.
extern "x86-interrupt" fn spurious_handler(_stack_frame: InterruptStackFrame) {
    use core::sync::atomic::Ordering;
    SPURIOUS_HITS.fetch_add(1, Ordering::Relaxed);
    let _ = crate::apic::eoi();
}

extern "x86-interrupt" fn spurious_pic_master_handler(_stack_frame: InterruptStackFrame) {
    use core::sync::atomic::Ordering;
    SPURIOUS_HITS.fetch_add(1, Ordering::Relaxed);
    // IRQ7 phantom: NO EOI by 8259 errata contract.
}

extern "x86-interrupt" fn spurious_pic_slave_handler(_stack_frame: InterruptStackFrame) {
    use core::sync::atomic::Ordering;
    SPURIOUS_HITS.fetch_add(1, Ordering::Relaxed);
    // IRQ15 phantom: EOI to MASTER only (the slave never raised).
    unsafe {
        crate::interrupts::PICS
            .lock()
            .notify_end_of_interrupt(SPURIOUS_PIC_MASTER_VECTOR);
    }
}
