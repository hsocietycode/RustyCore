//! PIT (i8253/i8254) timer: ~100 Hz square wave on IRQ0.
//!
//! The PIT is the only periodic interrupt source we have before APIC in
//! Phase 3. Channel 0, mode 3 (square wave), divisor 11931 → 100.0 Hz.
//! IRQ0 (PIC line 0) is unmasked here; everything else stays masked.

use x86_64::instructions::port::Port;

const PIT_CMD: u16 = 0x43;
const PIT_CH0: u16 = 0x40;

/// PIT input clock: 1.193182 MHz.
const PIT_HZ: u32 = 1_193_182;
/// Target tick rate.
const TARGET_HZ: u32 = 100;

/// Program channel 0 for a ~100 Hz square wave and unmask IRQ0 on the PIC.
pub fn init() {
    let divisor = (PIT_HZ / TARGET_HZ) as u16;
    unsafe {
        // Channel 0, lobyte/hibyte, mode 3 (square wave), binary.
        Port::<u8>::new(PIT_CMD).write(0x36);
        Port::<u8>::new(PIT_CH0).write((divisor & 0xFF) as u8);
        Port::<u8>::new(PIT_CH0).write((divisor >> 8) as u8);

        // Unmask IRQ0 (timer) only: master mask bit 0 → 0, rest stay masked.
        let mut pics = crate::interrupts::PICS.lock();
        let mut masks = pics.read_masks();
        masks[0] &= !0x01;
        pics.write_masks(masks[0], masks[1]);
    }
    crate::serial_println!("timer: PIT @ ~100 Hz, IRQ0 unmasked");
}
