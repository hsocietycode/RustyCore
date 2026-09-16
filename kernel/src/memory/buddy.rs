//! Buddy frame allocator — Phase 1 production path.
//!
//! The bump allocator never reclaims; this one does. Physical RAM is split
//! into power-of-two blocks: order `n` owns `2^n` contiguous 4KiB frames.
//! Splitting serves small requests from big blocks; freeing merges a block
//! with its free buddy back up — fragmentation heals instead of piling up.
//!
//! - Orders `0..=BUDDY_MAX_ORDER` (order 0 = one 4KiB frame, order 12 = 16 MiB).
//! - Free lists are plain `Vec<u64>` of block base addresses — heap-backed,
//!   which creates a bring-up paradox: the allocator must serve frames BEFORE
//!   the heap exists (the heap itself is mapped from those frames). Solved
//!   with two phases: `new` starts in bump mode (a cursor walks the map, zero
//!   heap use — `Vec::new` never allocates), and `activate` carves the
//!   REMAINDER of the map into real free lists once the heap is live.
//!   `memory::init` calls `activate` between `heap::init` and first use.
//! - The zero page (`< 0x1000`) is never handed out: real-mode IVT/BDA live
//!   there. Merging can never resurrect it — a merge needs its buddy FREE,
//!   and the zero page is never pushed, so its neighbor stays unmerged.
//! - Single CPU, no locking: boot is single-threaded, IRQ-time never touches
//!   this (the LAPIC handler only ticks atomics).

use bootloader_api::info::{MemoryRegionKind, MemoryRegions};
use x86_64::{
    structures::paging::{FrameAllocator, FrameDeallocator, PhysFrame, Size4KiB},
    PhysAddr,
};

/// Highest order kept. Order 12 = 2^12 frames × 4KiB = 16 MiB per block —
/// big enough that a 512 MiB QEMU guest is ~32 blocks, small enough that
/// splitting to order 0 costs at most 13 pops.
pub const BUDDY_MAX_ORDER: usize = 12;

/// Number of free lists (orders 0 through `BUDDY_MAX_ORDER` inclusive).
pub const BUDDY_ORDERS: usize = BUDDY_MAX_ORDER + 1;

/// Frame size every order is built from.
const FRAME_SIZE: u64 = 4096;

/// First address this allocator will ever hand out (zero page is firmware land).
const MIN_ADDR: u64 = 0x1000;

pub struct BuddyFrameAllocator {
    regions: &'static MemoryRegions,
    /// Bump-mode cursor (pre-`activate`): next candidate address inside
    /// `regions[region_idx]`. `0` means "not yet positioned".
    region_idx: usize,
    next_addr: u64,
    /// `true` once `activate` carved the free lists — dispatch flips from
    /// cursor-walk to split/merge. Never flips back.
    active: bool,
    /// `free_lists[o]` holds base addresses of free order-`o` blocks.
    /// Empty (zero heap use) until `activate`.
    free_lists: [alloc::vec::Vec<u64>; BUDDY_ORDERS],
    /// Frames currently handed out (allocate − deallocate). Ledger, not machinery.
    allocated: u64,
}

impl BuddyFrameAllocator {
    /// Build from the bootloader memory map. Starts in BUMP MODE — safe to
    /// call before the heap exists (no allocation happens here: `Vec::new`
    /// is just a dangling pointer + zero length).
    ///
    /// # Safety
    /// `regions` must be the bootloader-provided map, valid for `'static`,
    /// whose `Usable` entries describe RAM the kernel owns exclusively.
    pub unsafe fn new(regions: &'static MemoryRegions) -> Self {
        Self {
            regions,
            region_idx: 0,
            next_addr: 0,
            active: false,
            free_lists: [(); BUDDY_ORDERS].map(|_| alloc::vec::Vec::new()),
            allocated: 0,
        }
    }

    /// Flip from bump mode to real buddy mode. Call AFTER the heap is live
    /// (`Vec::push` needs it) and BEFORE any free — carves every region the
    /// bump cursor hasn't consumed into the largest aligned blocks that fit.
    ///
    /// Regions at/below the cursor are skipped wholesale: their prefix was
    /// handed out during bring-up (heap pages, MMIO tables), and re-listing
    /// those frames would double-own live memory. Regions the cursor skipped
    /// as non-`Usable` are skipped here for the same reason the cursor
    /// skipped them — they aren't ours.
    pub fn activate(&mut self) {
        assert!(!self.active, "buddy activate called twice");
        for (i, r) in self.regions.iter().enumerate() {
            if r.kind != MemoryRegionKind::Usable {
                continue;
            }
            if r.end <= r.start {
                continue;
            }
            let mut start = r.start.next_multiple_of(FRAME_SIZE).max(MIN_ADDR);
            if i == self.region_idx {
                // Cursor's region: resume AFTER the handed-out prefix.
                start = start.max(self.next_addr.next_multiple_of(FRAME_SIZE));
            } else if i < self.region_idx {
                // Fully consumed (or skipped as unusable) — nothing left.
                continue;
            }
            let end = r.end & !(FRAME_SIZE - 1);
            while start + FRAME_SIZE <= end {
                let remaining = end - start;
                let mut order = BUDDY_MAX_ORDER;
                loop {
                    let size = 1u64 << (order + 12);
                    if size <= remaining && start % size == 0 {
                        break;
                    }
                    if order == 0 {
                        break;
                    }
                    order -= 1;
                }
                let size = 1u64 << (order + 12);
                self.free_lists[order].push(start);
                start += size;
            }
        }
        // Bump cursor retires — every later request goes through the lists.
        self.region_idx = self.regions.len();
        self.next_addr = u64::MAX;
        self.active = true;
    }

    /// Bump-mode handoff: one frame from the cursor. Identical discipline to
    /// `BootFrameAllocator` (aligned, skips non-usable, never the zero page).
    fn bump_one(&mut self) -> Option<u64> {
        loop {
            let region = self.regions.get(self.region_idx)?;
            if region.kind != MemoryRegionKind::Usable {
                self.region_idx += 1;
                self.next_addr = 0;
                continue;
            }
            let aligned = match region.start.checked_next_multiple_of(FRAME_SIZE) {
                Some(a) => a,
                None => {
                    self.region_idx += 1;
                    self.next_addr = 0;
                    continue;
                }
            };
            let addr = aligned.max(self.next_addr).max(MIN_ADDR);
            let frame_end = addr.checked_add(FRAME_SIZE)?;
            if frame_end > region.end {
                self.region_idx += 1;
                self.next_addr = 0;
                continue;
            }
            self.next_addr = frame_end;
            return Some(addr);
        }
    }

    /// Frames still free across all orders (for the boot log). Zero before
    /// `activate` — the cursor owns everything until then, and the cursor
    /// doesn't count (it counts UP, not down).
    pub fn free_frames(&self) -> u64 {
        let mut n = 0u64;
        for (order, list) in self.free_lists.iter().enumerate() {
            n += list.len() as u64 * (1u64 << order);
        }
        n
    }

    /// Frames currently checked out.
    pub fn allocated_frames(&self) -> u64 {
        self.allocated
    }

    /// Take one block of exactly `order`, splitting a bigger one if needed.
    fn take_block(&mut self, order: usize) -> Option<u64> {
        if order > BUDDY_MAX_ORDER {
            return None;
        }
        if let Some(addr) = self.free_lists[order].pop() {
            return Some(addr);
        }
        // Nothing this size — split one block of the next order up: keep the
        // low half, park the high half back on this order's list.
        let higher = self.take_block(order + 1)?;
        let half = 1u64 << (order + 12);
        self.free_lists[order].push(higher + half);
        Some(higher)
    }
}

unsafe impl FrameAllocator<Size4KiB> for BuddyFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        let addr = if self.active {
            self.take_block(0)?
        } else {
            self.bump_one()?
        };
        debug_assert_eq!(addr % FRAME_SIZE, 0);
        debug_assert!(addr >= MIN_ADDR, "buddy handed out the zero page");
        self.allocated += 1;
        Some(PhysFrame::containing_address(PhysAddr::new(addr)))
    }
}

impl FrameDeallocator<Size4KiB> for BuddyFrameAllocator {
    unsafe fn deallocate_frame(&mut self, frame: PhysFrame<Size4KiB>) {
        assert!(
            self.active,
            "buddy free before activate — nothing handed out in bump mode may be freed yet"
        );
        let mut addr = frame.start_address().as_u64();
        assert_eq!(addr % FRAME_SIZE, 0, "buddy free of unaligned frame");
        assert!(addr >= MIN_ADDR, "buddy free of the zero page");
        let mut order = 0usize;
        loop {
            if order == BUDDY_MAX_ORDER {
                self.free_lists[order].push(addr);
                break;
            }
            // The ONLY block this one can merge with: same size, XOR neighbor.
            let buddy = addr ^ (1u64 << (order + 12));
            if let Some(pos) = self.free_lists[order].iter().position(|&a| a == buddy) {
                self.free_lists[order].swap_remove(pos);
                addr = addr.min(buddy);
                order += 1;
            } else {
                self.free_lists[order].push(addr);
                break;
            }
        }
        self.allocated = self.allocated.saturating_sub(1);
    }
}
