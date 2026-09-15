#![no_std]
#![no_main]

extern crate alloc;

mod interrupts;
mod memory;
mod serial;

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
    // No IDT yet — any interrupt (timer, spurious, UART) with IF=1
    // would vector through garbage and triple-fault. Keep IF=0
    // until Phase 2 installs a real IDT.
    x86_64::instructions::interrupts::disable();

    serial::init();
    interrupts::init();
    serial_println!("RustyCore v0.1 - serial online, PIC remapped.");

    memory::init(boot_info);

    serial_println!("RustyCore v0.1 - memory online, halting.");
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
