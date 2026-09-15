#![no_std]
#![no_main]

use bootloader::{entry_point, BootInfo};
use core::panic::PanicInfo;

entry_point!(kernel_main);

fn kernel_main(boot_info: &'static BootInfo) -> ! {
    let _ = boot_info;
    // Phase 0: early init (GDT, IDT, UART, alloc, sched) lands here next.
    loop {
        x86_64::instructions::hlt();
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        x86_64::instructions::hlt();
    }
}

/// Symbol expected by `linker.ld` for freestanding link setups.
#[no_mangle]
pub extern "C" fn _start() -> ! {
    loop {
        x86_64::instructions::hlt();
    }
}
