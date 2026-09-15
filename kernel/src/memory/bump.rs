//! Bump frame allocator — Phase 1 bring-up.
//!
//! Hands out `Usable` 4KiB frames in map order, never reclaims. Simple,
//! correct, and enough until the buddy allocator (`alloc-buddy`) lands.
//! Allocation is O(1) amortized: a cursor walks forward through the map
//! instead of re-scanning from region zero on every call.

use bootloader_api::info::{MemoryRegionKind, MemoryRegions};
use x86_64::{
    structures::paging::{FrameAllocator, PhysFrame, Size4KiB},
    PhysAddr,
};

pub struct BootFrameAllocator {
    regions: &'static MemoryRegions,
    region_idx: usize,
    /// Next candidate frame address inside `regions[region_idx]`.
    /// `0` means "not yet positioned" — positioned on first use.
    next_addr: u64,
}

impl BootFrameAllocator {
    /// Build from the bootloader memory map.
    ///
    /// # Safety
    /// `regions` must be the bootloader-provided map, valid for `'static`,
    /// whose `Usable` entries describe RAM the kernel owns exclusively.
    /// Handed-out frames are never reclaimed — aliasing them anywhere
    /// else is undefined behavior.
    pub unsafe fn new(regions: &'static MemoryRegions) -> Self {
        Self {
            regions,
            region_idx: 0,
            next_addr: 0,
        }
    }

    fn advance(&mut self) {
        self.region_idx += 1;
        self.next_addr = 0;
    }
}

unsafe impl FrameAllocator<Size4KiB> for BootFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        loop {
            let region = self.regions.get(self.region_idx)?;
            if region.kind != MemoryRegionKind::Usable {
                self.advance();
                continue;
            }
            let aligned = match region.start.checked_next_multiple_of(4096) {
                Some(a) => a,
                None => {
                    self.advance();
                    continue;
                }
            };
            // Never hand out the zero page — real-mode IVT/BDA live there.
            let addr = aligned.max(self.next_addr).max(0x1000);
            let frame_end = addr.checked_add(4096)?;
            if frame_end > region.end {
                self.advance();
                continue;
            }
            self.next_addr = frame_end;
            return Some(PhysFrame::containing_address(PhysAddr::new(addr)));
        }
    }
}
