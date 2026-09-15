//! APIC probe (Step 1): read-only detection, zero hardware writes.
//!
//! Checks CPUID for a local APIC, reads IA32_APIC_BASE (MSR 0x1B) for the
//! MMIO base + enabled/BSP flags, and notes whether the bootloader gave us
//! an RSDP (for MADT parsing in a later step). Nothing is mapped, nothing
//! is enabled — the PIC+PIT path stays the one true clock.

use bootloader_api::BootInfo;
use x86_64::{
    registers::model_specific::Msr,
    structures::paging::{FrameAllocator, OffsetPageTable, Size4KiB},
};

/// IA32_APIC_BASE MSR index.
const IA32_APIC_BASE: u32 = 0x1B;

/// Base-address mask: bits 12..=51 (physical base, 4KiB-aligned).
/// Bits 0..=7 are flags (BSP, x2APIC, global enable), 8..=11 reserved.
/// The old mask 0xFFFF_F000_0000 ate bits 12..=27 — on our q35 it turned
/// the real 0xFEE00000 into 0xF0000000. Caught by adversarial review.
const APIC_BASE_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// QEMU q35 well-known fallback base when no MADT is consulted yet.
pub const FALLBACK_BASE: u64 = 0xFEE0_0000;

/// LAPIC EOI register offset from the MMIO base.
const EOI_OFFSET: u64 = 0xB0;

/// Result of the read-only probe, kept for later steps.
#[derive(Debug, Clone, Copy)]
pub struct Probe {
    /// CPUID.01H:EDX bit 9 — a local APIC exists.
    pub present: bool,
    /// MMIO base from IA32_APIC_BASE (or the q35 fallback if disabled).
    /// Consumed by Step 3 (LAPIC mapping) — unused until then.
    #[allow(dead_code)]
    pub base: u64,
    /// IA32_APIC_BASE bit 11 — APIC globally enabled.
    /// Consumed by Step 3 — unused until then.
    #[allow(dead_code)]
    pub enabled: bool,
    /// IA32_APIC_BASE bit 8 — this CPU is the bootstrap processor.
    /// Consumed by the SMP step (Phase 4) — unused until then.
    #[allow(dead_code)]
    pub bsp: bool,
    /// Bootloader handed us an RSDP address for future MADT parsing.
    /// Consumed by the MADT step — unused until then.
    #[allow(dead_code)]
    pub have_rsdp: bool,
}

/// Probe APIC presence. Read-only: CPUID + one MSR read, no MMIO, no writes.
pub fn probe(boot_info: &BootInfo) -> Probe {
    // `__cpuid` is a safe wrapper in this toolchain — no unsafe needed.
    let cpuid = core::arch::x86_64::__cpuid(1);
    let present = cpuid.edx & (1 << 9) != 0;

    // SAFETY: reading IA32_APIC_BASE changes nothing.
    let msr = unsafe { Msr::new(IA32_APIC_BASE).read() };
    let base = msr & APIC_BASE_MASK;
    let enabled = msr & (1 << 11) != 0;
    let bsp = msr & (1 << 8) != 0;

    let have_rsdp = boot_info.rsdp_addr.into_option().is_some();

    // If the MSR reports no base (APIC soft-disabled), name the fallback
    // explicitly instead of printing a zero that looks like a real address.
    let (base, src) = if base == 0 {
        (FALLBACK_BASE, "fallback-q35")
    } else {
        (base, "msr")
    };

    crate::serial_println!(
        "apic: probe present={} base={:#x} enabled={} bsp={} rsdp={}; base={}",
        present,
        base,
        enabled,
        bsp,
        if have_rsdp { "yes" } else { "no" },
        src,
    );

    Probe {
        present,
        base,
        enabled,
        bsp,
        have_rsdp,
    }
}

/// LAPIC register offsets from the MMIO base.
const ID_OFFSET: u64 = 0x20;
const VER_OFFSET: u64 = 0x30;
const TPR_OFFSET: u64 = 0x80;
const SVR_OFFSET: u64 = 0xF0;
const LVT_TIMER_OFFSET: u64 = 0x320;

/// Map + enable the local APIC (Step 3).
///
/// Maps the LAPIC MMIO page into the dedicated [`crate::memory::MMIO_WINDOW`]
/// (device flags, NO_CACHE — see `map_mmio_window` for why not phys-offset),
/// caches the window address for `eoi()`, then via volatile MMIO: enables
/// via SVR (`0xF0 = 0x1FF`: spurious vector `0xFF` + enable bit 8), masks
/// the LVT timer (`0x320 |= 1 << 16` — PIC stays the one true clock),
/// zeroes TPR (`0x80 = 0`), and reports ID/VER/SVR.
///
/// # Errors
/// `Err` (never panics) when `probe.present` is false, the APIC is
/// soft-disabled in the MSR (bit 11 clear), the base is unaligned, the
/// window is occupied, or any address computation overflows.
pub fn init(
    mapper: &mut OffsetPageTable,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    probe: &Probe,
) -> Result<(), &'static str> {
    if !probe.present {
        return Err("no local APIC present (CPUID bit 9 clear)");
    }
    if !probe.enabled {
        return Err("APIC soft-disabled (IA32_APIC_BASE bit 11 clear) — refusing to pretend");
    }
    let base = crate::memory::map_mmio_window(mapper, frame_allocator, probe.base)?;
    LAPIC_VIRT.store(base.as_u64(), core::sync::atomic::Ordering::Relaxed);

    // SAFETY: `base` is the mapped LAPIC window (one MMIO page); offsets
    // are architecturally fixed u32 registers. Volatile: the compiler must
    // emit every read/write exactly once, in order. Checked arithmetic:
    // a garbage probe base must Err here, never wrap into чужую memory.
    let reg = |off: u64| -> Result<*mut u32, &'static str> {
        base.as_u64()
            .checked_add(off)
            .map(|a| a as *mut u32)
            .ok_or("apic register address overflows")
    };
    unsafe {
        let svr = reg(SVR_OFFSET)?;
        core::ptr::write_volatile(svr, 0x1FF);

        let lvt_timer = reg(LVT_TIMER_OFFSET)?;
        let lvt = core::ptr::read_volatile(lvt_timer);
        core::ptr::write_volatile(lvt_timer, lvt | (1 << 16));

        let tpr = reg(TPR_OFFSET)?;
        core::ptr::write_volatile(tpr, 0);

        let id = core::ptr::read_volatile(reg(ID_OFFSET)? as *const u32) >> 24;
        let ver = core::ptr::read_volatile(reg(VER_OFFSET)? as *const u32);
        let svr_val = core::ptr::read_volatile(svr as *const u32);
        crate::serial_println!(
            "apic: mapped + enabled id={} ver={:#x} svr={:#x}",
            id,
            ver,
            svr_val
        );
    }
    Ok(())
}

/// Cached LAPIC window address, stored by [`init`]. `0` = not mapped yet.
/// `eoi()` reads this instead of re-reading the MSR on every timer tick —
/// cheaper, and immune to MSR-vs-mapping drift.
static LAPIC_VIRT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// End-of-interrupt to the local APIC: write 0 to the EOI register.
///
/// Uses the cached [`MMIO_WINDOW`](crate::memory::MMIO_WINDOW) address from
/// [`init`] — no MSR read, no arithmetic in the hot path. A zero cache
/// (called before Step 3 mapped the window) panics loudly instead of
/// writing to address 0xB0.
///
/// # Safety
/// Caller must be the LAPIC timer/error handler (or Step-5 bring-up):
/// writing EOI with no interrupt in service is harmless, but writing to a
/// wrong address is not. Only call after [`init`] succeeded.
pub fn eoi() {
    let win = LAPIC_VIRT.load(core::sync::atomic::Ordering::Relaxed);
    if win == 0 {
        panic!("apic: eoi before init — LAPIC window not mapped");
    }
    // SAFETY: EOI is write-only-0 by spec; the address is the mapped
    // MMIO window + 0xB0 (offsets < 4KiB, cannot overflow the page).
    unsafe {
        core::ptr::write_volatile((win + EOI_OFFSET) as *mut u32, 0);
    }
}
