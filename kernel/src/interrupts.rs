//! 8259 PIC setup — remap hardware IRQs away from CPU exceptions.
//!
//! CPU exceptions occupy vectors 0..=31, so the master/slave PICs are
//! remapped to 32..=47. Kept minimal on purpose: Phase 2 (APIC/IDT)
//! builds on top of this.

use pic8259::ChainedPics;
use spin::Mutex;

pub const PIC_1_OFFSET: u8 = 32;
pub const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

pub static PICS: Mutex<ChainedPics> =
    Mutex::new(unsafe { ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) });

/// Remap the PICs and mask everything until IDT handlers exist.
pub fn init() {
    unsafe {
        let mut pics = PICS.lock();
        pics.initialize();
        // Mask all IRQs for now — unmasked one by one in Phase 2.
        pics.write_masks(0xFF, 0xFF);
    }
}
