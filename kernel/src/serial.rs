//! Early COM1 serial driver — the kernel's first voice.
//!
//! QEMU wires COM1 (0x3F8) to stdio with `-nographic`, so everything
//! printed here shows up in the terminal running QEMU.
//!
//! Design: fresh `Port` values on every call, no statics, no locks.
//!
//! NOTE: registers are poked directly instead of using
//! `uart_16550::SerialPort::init()`, because that init enables UART
//! interrupts (IER = 0x01) while we have no IDT yet — the first
//! transmitted byte raises IRQ4 into the void and the kernel hangs.
//! We keep IER = 0 (polling only) until Phase 2 installs handlers.

use x86_64::instructions::port::Port;

const COM1: u16 = 0x3F8;

/// Initialize COM1: 115200 8-N-1, FIFO on, DTR/RTS, IRQs OFF (polling).
pub fn init() {
    unsafe {
        Port::<u8>::new(COM1 + 1).write(0x00); // disable all UART interrupts (!!)
        Port::<u8>::new(COM1 + 3).write(0x80); // enable DLAB (divisor latch)
        Port::<u8>::new(COM1).write(0x01); // divisor low byte  = 1 → 115200 baud
        Port::<u8>::new(COM1 + 1).write(0x00); // divisor high byte = 0
        Port::<u8>::new(COM1 + 3).write(0x03); // 8 bits, no parity, one stop bit
        Port::<u8>::new(COM1 + 2).write(0xC7); // FIFO on, clear queues, 14-byte watermark
        Port::<u8>::new(COM1 + 4).write(0x0B); // DTR + RTS + OUT2
                                               // INT_EN stays 0x00 — polling only until the IDT exists.
    }
}

fn write_byte(b: u8) {
    unsafe {
        while Port::<u8>::new(COM1 + 5).read() & 0x20 == 0 {
            core::hint::spin_loop();
        }
        Port::<u8>::new(COM1).write(b);
    }
}

fn write_str(s: &str) {
    for b in s.bytes() {
        if b == b'\n' {
            write_byte(b'\r');
        }
        write_byte(b);
    }
}

#[doc(hidden)]
pub fn _print(args: core::fmt::Arguments) {
    use core::fmt::Write;
    struct Sink;
    impl core::fmt::Write for Sink {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            write_str(s);
            Ok(())
        }
    }
    Sink.write_fmt(args).expect("serial write failed");
}

/// Print to COM1 without a newline.
#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => {
        $crate::serial::_print(core::format_args!($($arg)*))
    };
}

/// Print to COM1 with a newline.
#[macro_export]
macro_rules! serial_println {
    () => {
        $crate::serial_print!("\n")
    };
    ($fmt:expr) => {
        $crate::serial_print!(concat!($fmt, "\n"))
    };
    ($fmt:expr, $($arg:tt)*) => {
        $crate::serial_print!(concat!($fmt, "\n"), $($arg)*)
    };
}
