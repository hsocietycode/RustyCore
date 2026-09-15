//! Bump frame allocator — Phase 1 bring-up.
//!
//! Walks the bootloader memory map once, hands out `Usable` 4KiB frames in
//! order, never reclaims. Simple, correct, and enough until the buddy
//! allocator (`alloc-buddy` feature) lands.

use bootloader_api::info::{MemoryRegionKind, MemoryRegions};
use x86_64::{
    structures::paging::{FrameAllocator, PhysFrame, Size4KiB},
    PhysAddr,
};

pub struct BootFrameAllocator {
    regions: &'static MemoryRegions,
    next: usize,
}

impl BootFrameAllocator {
    /// Build from the bootloader memory map. The `Usable` regions are used
    /// in order; frames already owned by the bootloader are never touched.
    pub unsafe fn new(regions: &'static MemoryRegions) -> Self {
        Self { regions, next: 0 }
    }

    fn usable_frames(&self) -> impl Iterator<Item = PhysFrame<Size4KiB>> + '_ {
        self.regions
            .iter()
            .filter(|r| r.kind == MemoryRegionKind::Usable)
            .flat_map(|r| {
                let start = r.start.next_multiple_of(4096);
                let end = r.end;
                (start..end)
                    .step_by(4096)
                    .map(|addr| PhysFrame::containing_address(PhysAddr::new(addr)))
            })
    }
}

unsafe impl FrameAllocator<Size4KiB> for BootFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        let frame = self.usable_frames().nth(self.next)?;
        self.next += 1;
        Some(frame)
    }
}
