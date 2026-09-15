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

    memory::init(boot_info);

    timer::init();

    // Self-test #1: software breakpoint. If the IDT is wired right,
    // the handler prints the stack frame and we continue here.
    serial_println!("self-test: firing int3 breakpoint...");
    x86_64::instructions::interrupts::int3();
    serial_println!("self-test: breakpoint handler returned, IDT works.");

    // Self-test #1b: syscall stub (Step 2). `int 0x80` twice, still IF=0 —
    // software interrupts don't need the flag. Proves the 0x80 gate is live.
    syscall::self_test();

    // Self-test #2: hardware timer. Enable IF, wait for ~100 ticks
    // (~1 second at 100 Hz), all driven by IRQ0 through the PIC.
    serial_println!("self-test: enabling interrupts, waiting for 100 timer ticks...");
    x86_64::instructions::interrupts::enable();

    let mut spins: u64 = 0;
    loop {
        use core::sync::atomic::Ordering;
        if idt::TIMER_TICKS.load(Ordering::Relaxed) >= 100 {
            break;
        }
        spins += 1;
        // ~10M hlt-spins ≈ way past 1s even at 100 Hz; if we get here
        // the timer is dead and we say so instead of hanging forever.
        if spins > 10_000_000 {
            serial_println!("self-test FAILED: timer produced no ticks (spun out). Halting.");
            loop {
                x86_64::instructions::hlt();
            }
        }
        x86_64::instructions::hlt(); // sleep until the next IRQ
    }

    serial_println!("self-test: 100 timer ticks seen, IRQs work. Phase 2 online. Halting.");
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
