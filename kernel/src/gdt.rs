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

/// Double-fault IST stack: 5 pages, static (no heap at the time it is used —
/// a double fault can fire before memory init, so this must not depend on
/// the allocator).
const DF_STACK_SIZE: usize = 4096 * 5;
static mut DF_STACK: [u8; DF_STACK_SIZE] = [0; DF_STACK_SIZE];

/// The one TSS. `static mut` + `TSS::new()` (a `const fn`) — no `lazy_static`
/// and, crucially, no `&raw mut` derived from a SHARED reference: writing
/// `privilege_stack_table[0]` through a pointer laundered from `&TSS` would
/// be UB (a shared reference promises immutability). Every access goes
/// through an explicit raw pointer to this static instead.
///
/// `unsafe` reads are concentrated in the two big `unsafe` blocks below, both
/// of which own their access completely (single CPU; the only writer of
/// `rsp0` is the scheduler, which runs with IF=0).
static mut TSS: TaskStateSegment = TaskStateSegment::new();

/// Initialize the TSS fields that need addresses (the double-fault IST slot).
/// Must run before [`init`] builds the GDT descriptor for the TSS — the
/// descriptor copies the base/limit, so a later write to the structure is
/// still visible, but the IST pointer must be non-zero before a fault can
/// vector through `DOUBLE_FAULT_IST_INDEX`.
fn init_tss_fields() {
    // SAFETY: single writer, before the IDT is loaded (so no fault can run
    // concurrently), and `DF_STACK` is a static the kernel owns exclusively.
    unsafe {
        let tss = &raw mut TSS;
        let stack_start = VirtAddr::from_ptr(&raw const DF_STACK);
        (*tss).interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] =
            stack_start + DF_STACK_SIZE as u64;
    }
}

lazy_static! {
    static ref GDT: (GlobalDescriptorTable, Selectors) = {
        init_tss_fields();
        let mut gdt = GlobalDescriptorTable::new();
        let code_selector = gdt.append(Descriptor::kernel_code_segment());
        let data_selector = gdt.append(Descriptor::kernel_data_segment());
        // SAFETY: `TSS` is a live static; the descriptor only records its
        // address and length, so it stays valid for the whole boot. The
        // reference is transient (the descriptor is returned by value) and
        // read-only — the raw pointer keeps us off `&TSS` (static-mut-refs).
        let tss_ptr: *const TaskStateSegment = &raw const TSS;
        let tss_selector = gdt.append(unsafe { Descriptor::tss_segment(&*tss_ptr) });
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
    // SAFETY: single writer (the scheduler, IF=0), 64-bit aligned store, no
    // aliasing reader. Raw pointer straight to the static — no shared
    // reference is ever formed, so this is sound.
    unsafe {
        let tss = &raw mut TSS;
        (*tss).privilege_stack_table[0] = VirtAddr::new(top);
    }
    // Deliberately NO serial print here: this runs on every task switch, and
    // one line per switch drowns the scheduler log (16 lines for a 16-switch
    // demo). The proof of the path is the value itself; when ring 3 lands and
    // hardware starts reading rsp0, a wrong value surfaces as a fault on the
    // first syscall — a much better witness than a log line nobody reads.
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
