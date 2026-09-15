#![no_std]
#![no_main]

mod interrupts;
mod serial;

use bootloader_api::{entry_point, BootInfo};
use core::panic::PanicInfo;

entry_point!(kernel_main);

fn kernel_main(_boot_info: &'static mut BootInfo) -> ! {
    // No IDT yet — any interrupt (timer, spurious, UART) with IF=1
    // would vector through garbage and triple-fault. Keep IF=0
    // until Phase 2 installs a real IDT.
    x86_64::instructions::interrupts::disable();

    serial::init();
    interrupts::init();
    serial_println!("RustyCore v0.1 - serial online, PIC remapped, halting.");
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
