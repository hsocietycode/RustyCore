//! Kernel heap: `linked_list_allocator` over freshly mapped frames.

use super::{map_heap, HEAP_SIZE, HEAP_START};
use linked_list_allocator::LockedHeap;
use x86_64::structures::paging::{FrameAllocator, OffsetPageTable, Size4KiB};

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// Map heap pages and initialize the global allocator.
///
/// # Errors
/// Returns a message when frames run out or a page is already mapped
/// (e.g. heap range colliding with something the bootloader mapped).
pub fn init(
    mapper: &mut OffsetPageTable,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<(), &'static str> {
    map_heap(mapper, frame_allocator, HEAP_START, HEAP_SIZE)?;
    unsafe {
        ALLOCATOR.lock().init(HEAP_START as *mut u8, HEAP_SIZE);
    }
    Ok(())
}
