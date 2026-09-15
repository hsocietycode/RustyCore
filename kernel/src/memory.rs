//! Physical memory management: frame allocator + kernel heap.
//!
//! The bootloader maps all physical memory at [`PHYS_MEM_OFFSET`] and hands
//! us the memory map. Usable regions feed the frame allocator; a slice of
//! mapped frames becomes the kernel heap.

mod bump;
mod heap;

pub use bump::BootFrameAllocator;

use bootloader_api::{info::MemoryRegionKind, BootInfo};
use x86_64::{
    structures::paging::{
        FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PhysFrame, Size4KiB,
    },
    VirtAddr,
};

/// Virtual address where the bootloader maps all physical memory
/// (see `BOOTLOADER_CONFIG` in main.rs — must match).
pub const PHYS_MEM_OFFSET: u64 = 0xFFFF_8000_0000_0000;

/// Kernel heap: virtual start + size. Backed by freshly allocated frames.
pub const HEAP_START: u64 = 0xFFFF_9000_0000_0000;
pub const HEAP_SIZE: usize = 1024 * 1024; // 1 MiB — heap_size_kb from kernel_config.toml

/// Initialize frame allocator + heap, print a memory summary.
pub fn init(boot_info: &'static mut BootInfo) {
    let phys_offset = VirtAddr::new(boot_info.physical_memory_offset.into_option().expect(
        "physical_memory_offset missing — is mappings.physical_memory set in BOOTLOADER_CONFIG?",
    ));

    let mut mapper = unsafe { mapper(phys_offset) };
    let mut frame_allocator = unsafe { BootFrameAllocator::new(&boot_info.memory_regions) };

    heap::init(&mut mapper, &mut frame_allocator).expect("heap init failed");

    let usable_bytes: u64 = boot_info
        .memory_regions
        .iter()
        .filter(|r| r.kind == MemoryRegionKind::Usable)
        .map(|r| r.end - r.start)
        .sum();
    crate::serial_println!(
        "memory: {} MiB usable, heap {} KiB at {:#x}, phys offset {:#x}",
        usable_bytes / 1024 / 1024,
        HEAP_SIZE / 1024,
        HEAP_START,
        phys_offset.as_u64(),
    );

    // Smoke test: Box + Vec prove the heap actually works.
    let b = alloc::boxed::Box::new(0xC0FFEEu64);
    let mut v = alloc::vec::Vec::new();
    v.push(*b);
    v.push(boot_info.memory_regions.len() as u64);
    crate::serial_println!(
        "memory: heap smoke test ok (box={:#x}, vec_len={})",
        v[0],
        v.len()
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

/// Allocate frames backing `[start, start + size)` and map them into the heap area.
pub fn map_heap(
    mapper: &mut OffsetPageTable,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    start: u64,
    size: usize,
) {
    use x86_64::structures::paging::PageTableFlags;
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    let start_page = Page::<Size4KiB>::containing_address(VirtAddr::new(start));
    let end_page = Page::<Size4KiB>::containing_address(VirtAddr::new(start + size as u64 - 1));
    for page in Page::range_inclusive(start_page, end_page) {
        let frame: PhysFrame<Size4KiB> = frame_allocator
            .allocate_frame()
            .expect("out of frames for heap");
        unsafe {
            mapper
                .map_to(page, frame, flags, frame_allocator)
                .expect("heap map_to failed")
                .flush();
        }
    }
}
