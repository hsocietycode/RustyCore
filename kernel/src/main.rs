#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

mod apic;
mod gdt;
mod idt;
mod interrupts;
mod memory;
mod serial;
mod syscall;
mod timer;

use bootloader_api::{entry_point, BootInfo, BootloaderConfig};
use core::panic::PanicInfo;

pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(bootloader_api::config::Mapping::FixedAddress(
        memory::PHYS_MEM_OFFSET,
    ));
    config
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    // IF=0 until the IDT is loaded — any interrupt before that
    // vectors through garbage and triple-faults.
    x86_64::instructions::interrupts::disable();

    serial::init();
    serial_println!("RustyCore v0.2 - serial online.");

    // Segments + TSS first (double-fault IST must exist before any
    // handler can run), then the IDT, then the PIC remap.
    gdt::init();
    serial_println!("RustyCore v0.2 - GDT+TSS loaded (double-fault IST ready).");
    idt::init();
    serial_println!("RustyCore v0.2 - IDT loaded (exceptions + IRQ vectors live).");
    interrupts::init();
    serial_println!("RustyCore v0.2 - PIC remapped to 32..=47, all masked.");

    // APIC probe is read-only (CPUID + MSR), so it runs before memory
    // init — memory::init takes boot_info by move, and the probe must
    // not depend on heap or page tables anyway.
    let apic = apic::probe(boot_info);
    // Hard gate: every x86_64 machine worth booting has a local APIC.
    // No APIC → no timer future → say so now, not three phases later.
    assert!(
        apic.present,
        "no local APIC (CPUID bit 9 clear) — cannot continue"
    );

    let (mut mapper, mut frame_allocator) = memory::init(boot_info);

    // Step 3: map the LAPIC MMIO page + enable the local APIC (volatile
    // SVR/LVT/TPR). PIC+PIT stays the one true clock — LVT timer masked.
    // Boot must fail loudly here: a half-mapped APIC is worse than none.
    apic::init(&mut mapper, &mut frame_allocator, &apic).expect("apic init failed");

    timer::init();

    // IF=1 from here on: the IDT is fully loaded, the PIC is remapped, and
    // Step 4 NEEDS hardware IRQs — the PIT ruler only advances via IRQ0.
    // (int3/syscall self-tests below don't need IF, but IRQs enabled early
    // is exactly the state Step 5 inherits.)
    serial_println!("self-test: enabling interrupts...");
    x86_64::instructions::interrupts::enable();

    // Step 4: calibrate the LAPIC timer against the PIT ruler (one-shot,
    // MMIO only — the LAPIC LVT stays masked, no APIC IRQ involved).
    // Loud failure: no rate means Step 5 has nothing to program.
    apic::calibrate().expect("apic calibrate failed");

    // Self-test #1: software breakpoint. If the IDT is wired right,
    // the handler prints the stack frame and we continue here.
    serial_println!("self-test: firing int3 breakpoint...");
    x86_64::instructions::interrupts::int3();
    serial_println!("self-test: breakpoint handler returned, IDT works.");

    // Self-test #1b: syscall stub (Step 2). `int 0x80` twice — software
    // interrupts don't need IF, but it's already on. Proves the 0x80 gate.
    syscall::self_test();

    // Step 5: dual-clock soak. Start the LAPIC timer in periodic mode
    // (~100 Hz, sidecar vector 0xEF) and let it run ALONGSIDE the PIT.
    // PIC+PIT stays master — if the APIC never fires, we say so loudly
    // and continue on the PIT alone. Never brick on a sidecar.
    apic::start_periodic().expect("apic start_periodic failed");

    // Self-test #2: dual clock. Wait until BOTH clocks saw ~100 ticks
    // (~1 second at 100 Hz each) — IRQ0 through the PIC and 0xEF from
    // the LAPIC, side by side on the same serial line.
    serial_println!("self-test: dual-clock soak (pic + apic, 100 ticks each)...");

    let mut spins: u64 = 0;
    let mut last_printed: u64 = 0;
    loop {
        use core::sync::atomic::Ordering;
        let pic = idt::TIMER_TICKS.load(Ordering::Relaxed);
        let apic_ticks = idt::APIC_TICKS.load(Ordering::Relaxed);
        // Progress every 20 PIC ticks — one line per ~200 ms, cheap.
        if pic >= last_printed + 20 {
            last_printed = pic;
            serial_println!("tick: pic={} apic=~{}", pic, apic_ticks);
        }
        if pic >= 100 && apic_ticks >= 100 {
            break;
        }
        // APIC silent but PIC alive at 100? Loud fallback, PIT carries on.
        if pic >= 100 && apic_ticks == 0 {
            serial_println!("apic-timer: NO IRQs (PIC ok, continuing)");
            break;
        }
        spins += 1;
        // ~10M hlt-spins ≈ way past 1s even at 100 Hz; if we get here
        // both clocks are dead and we say so instead of hanging forever.
        if spins > 10_000_000 {
            serial_println!("self-test FAILED: no ticks from either clock (spun out). Halting.");
            loop {
                x86_64::instructions::hlt();
            }
        }
        x86_64::instructions::hlt(); // sleep until the next IRQ
    }

    serial_println!("self-test: dual-clock soak done, both clocks live. Phase 2 online. Halting.");
    loop {
        x86_64::instructions::hlt();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Polling-only serial: safe even with IF=0.
    serial_println!("PANIC: {}", info);
    loop {
        x86_64::instructions::hlt();
    }
}
