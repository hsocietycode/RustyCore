//! Kernel-only `int 0x80` stub self-test (Step 2 registers, Step 6 fires).
//!
//! No STAR/LSTAR, no swapgs, no ring-3 — just a software interrupt whose
//! handler counts hits. Proves the 0x80 IDT entry is a live gate before any
//! real syscall machinery is built on top of it.

use core::sync::atomic::Ordering;

/// Fire `int 0x80` twice; the handler prints `count=1` then `count=2`.
/// Call with IF=1 after the IDT is loaded.
pub fn self_test() {
    crate::serial_println!("self-test: firing int 0x80 (syscall stub)...");
    unsafe {
        core::arch::asm!("int 0x80", options(nostack, preserves_flags));
        core::arch::asm!("int 0x80", options(nostack, preserves_flags));
    }
    let hits = crate::idt::SYSCALL_HITS.load(Ordering::Relaxed);
    assert_eq!(hits, 2, "syscall stub fired twice but handler saw {hits}");
    crate::serial_println!("self-test: syscall stub ok (2 hits).");
}
