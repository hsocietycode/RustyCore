//! GDT + TSS: segments and the double-fault escape stack.
//!
//! The double-fault handler runs on IST index 0 — its own dedicated stack —
//! so a kernel stack overflow (which is what usually *causes* double faults)
//! doesn't triple-fault the machine before the handler prints anything.

use lazy_static::lazy_static;
use x86_64::{
    instructions::{
        segmentation::{CS, DS, ES, SS},
        tables::load_tss,
    },
    structures::{
        gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector},
        tss::TaskStateSegment,
    },
    VirtAddr,
};

/// IST index used by the double-fault handler (0-based software index).
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

lazy_static! {
    static ref TSS: TaskStateSegment = {
        let mut tss = TaskStateSegment::new();
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = {
            const STACK_SIZE: usize = 4096 * 5;
            static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];
            let stack_start = VirtAddr::from_ptr(&raw const STACK);
            stack_start + STACK_SIZE as u64
        };
        tss
    };
}

lazy_static! {
    static ref GDT: (GlobalDescriptorTable, Selectors) = {
        let mut gdt = GlobalDescriptorTable::new();
        let code_selector = gdt.append(Descriptor::kernel_code_segment());
        let data_selector = gdt.append(Descriptor::kernel_data_segment());
        let tss_selector = gdt.append(Descriptor::tss_segment(&TSS));
        (
            gdt,
            Selectors {
                code_selector,
                data_selector,
                tss_selector,
            },
        )
    };
}

struct Selectors {
    code_selector: SegmentSelector,
    data_selector: SegmentSelector,
    tss_selector: SegmentSelector,
}

/// Update `TSS.rsp0` — the ring-0 stack the CPU loads on a ring transition.
///
/// No hardware reads it today (everything runs at CPL 0 — the CPU keeps the
/// current RSP on interrupts instead). The scheduler calls this on EVERY
/// switch as proof of the path: the hook exists, the value is right, and
/// when ring 3 arrives the exact same call becomes the privilege-stack
/// switch. Deferred until then: separate user stacks, syscall MSRs, guard
/// pages, IST for NMI/#MC.
///
/// # Safety
/// Writes a live TSS field. Single CPU + cooperative switch (interrupts
/// disabled around the call) — no concurrent reader can observe a half
/// write on a 64-bit aligned store.
pub fn set_rsp0(top: u64) {
    unsafe {
        // `&raw mut` through the lazy_static — the TSS lives for the whole
        // boot, and this is the only writer (no aliasing reader exists).
        let tss_ptr = &raw mut *(&raw const *TSS as *mut TaskStateSegment);
        (*tss_ptr).privilege_stack_table[0] = VirtAddr::new(top);
    }
    crate::serial_println!("gdt: rsp0 -> {:#x}", top);
}

/// Load the GDT, reload segment registers, and load the TSS.
pub fn init() {
    use x86_64::instructions::segmentation::Segment as _;
    GDT.0.load();
    unsafe {
        CS::set_reg(GDT.1.code_selector);
        DS::set_reg(GDT.1.data_selector);
        ES::set_reg(GDT.1.data_selector);
        SS::set_reg(GDT.1.data_selector);
        load_tss(GDT.1.tss_selector);
    }
}
