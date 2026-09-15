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

/// Timer ticks since `timer::init`. Written by the IRQ handler, read by main.
pub static TIMER_TICKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// LAPIC timer ticks since Step 5. Same counter discipline as TIMER_TICKS.
pub static APIC_TICKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// `int 0x80` hits since boot. Kernel-only stub for now (Ring0, no STAR/LSTAR).
pub static SYSCALL_HITS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

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

/// LAPIC timer ( Step 5 fires this). Counts ticks, then EOI straight to the
/// local APIC — the 8259 never sees LAPIC vectors, so no PIC ack here.
///
/// Cannot fire before Step 5 by hardware contract: LVT entries reset masked
/// and SVR resets disabled, so no local-APIC source can raise until bring-up
/// unmasks them. A stray fire earlier would #PF inside `eoi()` (window not
/// yet mapped) — loud, with the faulting address, by our page-fault handler.
extern "x86-interrupt" fn apic_timer_handler(_stack_frame: InterruptStackFrame) {
    use core::sync::atomic::Ordering;
    APIC_TICKS.fetch_add(1, Ordering::Relaxed);
    crate::apic::eoi();
}

/// LAPIC error (ESR). Loud by design: print and continue — a masked error
/// vector that nobody reads is how silent interrupt loss starts.
extern "x86-interrupt" fn apic_error_handler(_stack_frame: InterruptStackFrame) {
    crate::serial_println!("apic: error interrupt (ESR unread in this step)");
    crate::apic::eoi();
}
