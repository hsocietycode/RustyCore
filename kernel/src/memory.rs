//! Physical memory management: frame allocator + kernel heap.
//!
//! The bootloader maps all physical memory at [`PHYS_MEM_OFFSET`] and hands
//! us the memory map. Usable regions feed the frame allocator; a slice of
//! mapped frames becomes the kernel heap.
//!
//! Which allocator backs this is chosen at compile time via Cargo features:
//! `alloc-bump` (bring-up, no reclaim) or `alloc-buddy` (production).
//! Heap size comes from `config/kernel_config.toml` (`heap_size_kb`).

#[cfg(feature = "alloc-buddy")]
mod buddy;
#[cfg(feature = "alloc-bump")]
mod bump;
mod heap;

#[cfg(feature = "alloc-buddy")]
pub use buddy::BuddyFrameAllocator;
#[cfg(feature = "alloc-bump")]
pub use bump::BootFrameAllocator;

use bootloader_api::{info::MemoryRegionKind, BootInfo};
use x86_64::{
    structures::paging::{
        FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PhysFrame, Size4KiB, Translate,
    },
    VirtAddr,
};

/// Virtual address where the bootloader maps all physical memory
/// (see `BOOTLOADER_CONFIG` in main.rs — must match).
pub const PHYS_MEM_OFFSET: u64 = 0xFFFF_8000_0000_0000;

/// Kernel heap: virtual start + size in bytes (wired from `heap_size_kb` in
/// `config/kernel_config.toml` by build.rs — a stale literal here would be a lie).
pub const HEAP_START: u64 = 0xFFFF_9000_0000_0000;
pub const HEAP_SIZE: usize = parse_heap_bytes();

/// Dedicated virtual window for one MMIO page (today: the LAPIC).
/// Deliberately OUTSIDE the bootloader's phys-offset mapping: that window
/// uses huge pages with RAM flags, and a 4KiB flag-upgrade inside a huge
/// page is impossible by hardware design. Our own window maps exactly one
/// 4KiB page with device-correct flags — no aliasing, no flag surgery on
/// somebody else's tables.
pub const MMIO_WINDOW: u64 = 0xFFFF_A000_0000_0000;

/// `const` decimal parser for the build-time heap-size-bytes env string.
const fn parse_heap_bytes() -> usize {
    let s = env!("KERNEL_CONFIG_HEAP_SIZE_BYTES").as_bytes();
    let mut n: usize = 0;
    let mut i = 0;
    while i < s.len() {
        let d = s[i].wrapping_sub(b'0');
        assert!(d < 10, "heap_size_kb must be digits");
        n = n * 10 + d as usize;
        i += 1;
    }
    n
}

/// One concrete allocator behind a type alias: bump for bring-up, buddy for
/// production. An enum would work but bloats every stack slot that holds a
/// mapper pair (`Buddy` carries 13 free-lists ≈ 320 bytes vs `Bump`'s ~24);
/// the alias keeps `init`'s return type honest with zero indirection cost.
/// Buddy additionally reclaims via `FrameDeallocator`, which `memory::init`
/// exercises loudly.
#[cfg(feature = "alloc-bump")]
pub type FrameAlloc = BootFrameAllocator;
#[cfg(feature = "alloc-buddy")]
pub type FrameAlloc = BuddyFrameAllocator;

/// Initialize frame allocator + heap, print a memory summary.
///
/// Returns the page-table mapper and frame allocator so later boot steps
/// (APIC MMIO mapping) can reuse them without rebuilding from raw tables.
pub fn init(boot_info: &'static mut BootInfo) -> (OffsetPageTable<'static>, FrameAlloc) {
    // Honesty gate, enforced at COMPILE time: exactly one allocator must be
    // selected — both or neither refuses to build instead of silently
    // booting the wrong one.
    #[cfg(all(feature = "alloc-bump", feature = "alloc-buddy"))]
    compile_error!("select exactly one of alloc-bump / alloc-buddy, not both");
    #[cfg(not(any(feature = "alloc-bump", feature = "alloc-buddy")))]
    compile_error!("select one of alloc-bump / alloc-buddy");

    let phys_offset = VirtAddr::new(
        boot_info
            .physical_memory_offset
            .into_option()
            .expect("physical_memory_offset missing — is mappings.physical_memory set?"),
    );
    assert_eq!(
        phys_offset.as_u64(),
        PHYS_MEM_OFFSET,
        "bootloader phys offset drifted from PHYS_MEM_OFFSET"
    );

    let mut mapper = unsafe { mapper(phys_offset) };
    #[cfg(feature = "alloc-bump")]
    let mut frame_allocator = unsafe { BootFrameAllocator::new(&boot_info.memory_regions) };
    #[cfg(feature = "alloc-buddy")]
    let mut frame_allocator = unsafe { BuddyFrameAllocator::new(&boot_info.memory_regions) };

    heap::init(&mut mapper, &mut frame_allocator).expect("heap init failed");

    // Buddy bring-up, phase two: the heap is live, so carve the REMAINDER of
    // the map (everything the pre-heap bump cursor didn't consume) into real
    // free lists. Before this point the allocator was a bump cursor — no heap
    // use, no reclaim; after it, split/merge with reclaim. Bump skips this
    // (nothing to activate — no reclaim by design).
    #[cfg(feature = "alloc-buddy")]
    frame_allocator.activate();

    let (usable_bytes, usable_regions) = usable_summary(&boot_info.memory_regions);
    #[cfg(feature = "alloc-bump")]
    let alloc_name = "bump";
    #[cfg(feature = "alloc-buddy")]
    let alloc_name = "buddy";
    crate::serial_println!(
        "memory: {} MiB usable in {} regions, heap {} KiB at {:#x}, phys offset {:#x} (frames: {})",
        usable_bytes / 1024 / 1024,
        usable_regions,
        HEAP_SIZE / 1024,
        HEAP_START,
        phys_offset.as_u64(),
        alloc_name,
    );

    // Buddy proof: reclaim actually works — allocate a frame, free it, and
    // confirm the free count returns. Bump has no reclaim (by design), so it
    // only logs its one-way cursor. Loud either way, silent never.
    self_test_reclaim(&mut frame_allocator);

    self_test(
        boot_info.memory_regions.len() as u64,
        env!("KERNEL_CONFIG_MEMORY_SELF_TEST") == "true",
    );

    (mapper, frame_allocator)
}

/// Sum usable RAM with saturating math — garbage firmware tables must not
/// wrap the counter around to zero. Inverted regions (`end < start`) are
/// malformed: skipped loudly, not counted, not added.
fn usable_summary(regions: &[bootloader_api::info::MemoryRegion]) -> (u64, usize) {
    let mut bytes: u64 = 0;
    let mut count = 0;
    for r in regions
        .iter()
        .filter(|r| r.kind == MemoryRegionKind::Usable)
    {
        if r.end < r.start {
            crate::serial_println!(
                "memory: skipping malformed region [{:#x}..{:#x})",
                r.start,
                r.end
            );
            continue;
        }
        bytes = bytes.saturating_add(r.end - r.start);
        count += 1;
    }
    (bytes, count)
}

/// Buddy reclaim proof: allocate one frame, free it, confirm the free count
/// comes back. Runs at every boot under `alloc-buddy` — a non-reclaiming
/// buddy is a bump with extra steps, and that lie dies here, loudly.
#[cfg(feature = "alloc-buddy")]
fn self_test_reclaim(alloc: &mut BuddyFrameAllocator) {
    use x86_64::structures::paging::{FrameAllocator, FrameDeallocator};
    let before = alloc.free_frames();
    // SAFETY: the frame is freshly allocated by us, exclusively owned, and
    // freed before anyone maps it — no aliasing window exists.
    let frame = alloc
        .allocate_frame()
        .expect("buddy self-test: no frame to take");
    let mid = alloc.free_frames();
    assert_eq!(
        before,
        mid + 1,
        "buddy self-test: allocate didn't consume a frame"
    );
    unsafe { alloc.deallocate_frame(frame) };
    let after = alloc.free_frames();
    assert_eq!(
        before, after,
        "buddy self-test: free didn't return the frame"
    );
    crate::serial_println!(
        "memory: buddy self-test ok (free {} frames, alloc+free round-trips, allocated={})",
        after,
        alloc.allocated_frames(),
    );
}

/// Bump has no reclaim — say so in the log instead of faking a reclaim test.
#[cfg(feature = "alloc-bump")]
fn self_test_reclaim(_alloc: &mut FrameAlloc) {
    crate::serial_println!("memory: bump allocator (no reclaim by design)");
}
/// Gated by `[memory] self_test` in the config — `true` screams on a broken
/// heap at every boot, `false` skips it for speed (heap still initializes).
fn self_test(region_count: u64, enabled: bool) {
    if !enabled {
        crate::serial_println!("memory: heap self-test skipped by config");
        return;
    }
    let b = alloc::boxed::Box::new(0xC0F_FEEu64);
    let mut v = alloc::vec::Vec::new();
    v.push(*b);
    v.push(region_count);
    v.extend(0..64u64);
    assert_eq!(v[0], 0xC0F_FEE, "heap readback mismatch");
    assert_eq!(v.len(), 66, "heap vec length mismatch");
    let sum: u64 = v.iter().sum();
    crate::serial_println!(
        "memory: heap self-test ok (len={}, sum={:#x})",
        v.len(),
        sum
    );
}

/// Build an OffsetPageTable from the bootloader's level-4 table.
unsafe fn mapper(phys_offset: VirtAddr) -> OffsetPageTable<'static> {
    use x86_64::registers::control::Cr3;
    let (level_4_frame, _) = Cr3::read();
    let phys = level_4_frame.start_address();
    let virt = phys_offset + phys.as_u64();
    let table: *mut PageTable = virt.as_mut_ptr();
    unsafe { OffsetPageTable::new(&mut *table, phys_offset) }
}

/// Map ONE 4KiB MMIO page into the dedicated [`MMIO_WINDOW`]:
/// phys frame -> `MMIO_WINDOW`, with device-correct flags
/// `PRESENT | WRITABLE | NO_CACHE | NO_EXECUTE`.
///
/// Why a separate window instead of reusing the phys-offset address?
/// The bootloader maps phys space with huge pages + RAM flags, and x86
/// cannot change flags on a 4KiB sub-page of a huge page. A private
/// window maps exactly one small page — no flag surgery, no aliasing.
/// Single-window today (one LAPIC); a bump allocator over windows comes
/// with the IOAPIC step if more MMIO appears.
///
/// # Errors
/// `Err` when `phys` is unaligned, the window is already occupied, or
/// frames run out mid-map (fresh page-table levels).
pub fn map_mmio_window(
    mapper: &mut OffsetPageTable,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    phys: u64,
) -> Result<VirtAddr, &'static str> {
    use x86_64::{
        structures::paging::{PageTableFlags, PhysFrame},
        PhysAddr,
    };
    if !phys.is_multiple_of(4096) {
        return Err("mmio phys address is not 4KiB-aligned");
    }
    let virt = VirtAddr::new(MMIO_WINDOW);
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::NO_CACHE
        | PageTableFlags::NO_EXECUTE;
    let page = Page::<Size4KiB>::containing_address(virt);
    if mapper.translate_addr(virt).is_some() {
        return Err("mmio window already occupied — single-window kernel, one MMIO page max");
    }
    let frame = PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(phys));
    unsafe {
        mapper
            .map_to(page, frame, flags, frame_allocator)
            .map_err(|_| "mmio window map failed (frames out?)")?
            .flush();
    }
    crate::serial_println!(
        "memory: mmio window {:#x} -> phys {:#x} (NO_CACHE|NO_EXECUTE)",
        virt.as_u64(),
        phys
    );
    Ok(virt)
}

/// Allocate frames backing `[start, start + size)` and map them.
///
/// # Errors
/// `Err` when frames run out or a target page is already mapped —
/// callers must fail boot loudly rather than continue half-mapped.
pub fn map_heap(
    mapper: &mut OffsetPageTable,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    start: u64,
    size: usize,
) -> Result<(), &'static str> {
    use x86_64::structures::paging::{mapper::MapToError, PageTableFlags};
    if size == 0 {
        return Err("heap size is zero");
    }
    let end_addr = start
        .checked_add(size as u64)
        .ok_or("heap range overflows")?;
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    let start_page = Page::<Size4KiB>::containing_address(VirtAddr::new(start));
    let end_page = Page::<Size4KiB>::containing_address(VirtAddr::new(end_addr - 1));
    for page in Page::range_inclusive(start_page, end_page) {
        let frame: PhysFrame<Size4KiB> = frame_allocator
            .allocate_frame()
            .ok_or("out of frames for heap")?;
        // Name the failure honestly: out-of-frames mid-map and an already-
        // mapped page are different disasters with different fixes. The old
        // code reported both as "already mapped" — a lie that sends the next
        // debugger to the wrong crime scene.
        unsafe {
            mapper
                .map_to(page, frame, flags, frame_allocator)
                .map_err(|e| match e {
                    MapToError::FrameAllocationFailed => "heap map: out of frames for page tables",
                    MapToError::ParentEntryHugePage => "heap map: huge page blocks heap range",
                    MapToError::PageAlreadyMapped(_) => "heap page already mapped",
                })?
                .flush();
        }
    }
    Ok(())
}
