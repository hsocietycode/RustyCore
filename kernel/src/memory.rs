//! Physical memory management: frame allocator + kernel heap.
//!
//! The bootloader maps all physical memory at [`PHYS_MEM_OFFSET`] and hands
//! us the memory map. Usable regions feed the frame allocator; a slice of
//! mapped frames becomes the kernel heap.
//!
//! Which allocator backs this is chosen at compile time via Cargo features:
//! `alloc-bump` (bring-up, no reclaim) or `alloc-buddy` (production).
//! Heap size comes from `config/kernel_config.toml` (`heap_size_kb`).

mod bump;
mod heap;

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

/// Initialize frame allocator + heap, print a memory summary.
///
/// Returns the page-table mapper and frame allocator so later boot steps
/// (APIC MMIO mapping) can reuse them without rebuilding from raw tables.
pub fn init(boot_info: &'static mut BootInfo) -> (OffsetPageTable<'static>, BootFrameAllocator) {
    // Honesty gate, enforced at COMPILE time: `alloc-buddy` isn't
    // implemented yet — selecting it (or nothing, or both) refuses to
    // build instead of silently booting the wrong allocator.
    #[cfg(all(feature = "alloc-bump", feature = "alloc-buddy"))]
    compile_error!("select exactly one of alloc-bump / alloc-buddy, not both");
    #[cfg(not(any(feature = "alloc-bump", feature = "alloc-buddy")))]
    compile_error!("select one of alloc-bump / alloc-buddy");
    #[cfg(all(feature = "alloc-buddy", not(feature = "alloc-bump")))]
    compile_error!("alloc-buddy is selected but not implemented yet — build with alloc-bump");

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
    let mut frame_allocator = unsafe { BootFrameAllocator::new(&boot_info.memory_regions) };

    heap::init(&mut mapper, &mut frame_allocator).expect("heap init failed");

    let (usable_bytes, usable_regions) = usable_summary(&boot_info.memory_regions);
    crate::serial_println!(
        "memory: {} MiB usable in {} regions, heap {} KiB at {:#x}, phys offset {:#x}",
        usable_bytes / 1024 / 1024,
        usable_regions,
        HEAP_SIZE / 1024,
        HEAP_START,
        phys_offset.as_u64(),
    );

    self_test(boot_info.memory_regions.len() as u64);

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

/// Heap self-test: Box + Vec prove allocation, growth, and values work.
/// Runs at every boot — a broken heap must scream, never limp along.
fn self_test(region_count: u64) {
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
    use x86_64::structures::paging::PageTableFlags;
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
        unsafe {
            mapper
                .map_to(page, frame, flags, frame_allocator)
                .map_err(|_| "heap page already mapped")?
                .flush();
        }
    }
    Ok(())
}
