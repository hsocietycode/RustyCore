//! APIC bring-up: probe → map → enable → calibrate (Steps 1–4).
//!
//! Step 1 is read-only detection (CPUID + IA32_APIC_BASE MSR). Step 3 maps
//! the LAPIC MMIO page into [`crate::memory::MMIO_WINDOW`] and enables via
//! SVR (LVT timer stays masked — PIC+PIT remains the one true clock until
//! Step 5). Step 4 calibrates the LAPIC timer against the PIT ruler.
//! Nothing here fires LAPIC interrupts yet — that is Step 5.

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
    /// Consumed by Step 3 (LAPIC mapping).
    pub base: u64,
    /// IA32_APIC_BASE bit 11 — APIC globally enabled.
    /// Consumed by Step 3 (soft-disabled APIC is a loud Err).
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
/// Timer divide config (0x3E0), initial count (0x380), current count (0x390).
const DCR_OFFSET: u64 = 0x3E0;
const ICR_OFFSET: u64 = 0x380;
const CCR_OFFSET: u64 = 0x390;

/// LAPIC register pointer: window base + offset, checked.
///
/// Single helper for `init`/`calibrate`/`eoi` so the checked-add discipline
/// lives in one place. `Ok` addresses always land inside the mapped 4KiB
/// window (all offsets are < 0x400); `Err` on overflow, never wrap.
fn lapic_reg(win_base: u64, off: u64) -> Result<*mut u32, &'static str> {
    win_base
        .checked_add(off)
        .map(|a| a as *mut u32)
        .ok_or("apic register address overflows")
}

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
    // emit every read/write exactly once, in order. Addresses go through
    // `lapic_reg` — checked, never wrapping.
    let reg = |off: u64| lapic_reg(base.as_u64(), off);
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

/// Calibrated LAPIC-timer ticks per ~10 ms, stored by [`calibrate`].
/// `0` = not calibrated yet. Read by Step 5 via [`ticks_per_10ms`].
static TICKS_PER_10MS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Calibrated ticks per ~10 ms window (`0` = not calibrated yet).
/// Step 5 programs the periodic rate from this; a zero means
/// [`calibrate`] failed and the APIC timer stays off.
#[allow(dead_code)] // consumed by Step 5 (dual-clock soak) — not yet written
pub fn ticks_per_10ms() -> u64 {
    TICKS_PER_10MS.load(core::sync::atomic::Ordering::Relaxed)
}

/// PIT rate (Hz) and ruler length (ticks) used by [`calibrate`].
/// 10 ticks at 100 Hz ≈ 100 ms ≈ ten 10 ms windows.
const PIT_HZ: u32 = 100;
const CALIB_PIT_TICKS: u32 = 10;

/// Pure tick math for [`calibrate`]: APIC ticks elapsed during `pit_ticks`
/// PIT ticks at `pit_hz`, scaled to a 10 ms window. Saturating + div0-guarded
/// so garbage inputs yield 0 (Step 5 treats 0 as "calibration failed",
/// never as a rate). Pure function — no MMIO, no globals — so it is
/// unit-testable whenever the host test harness lands.
pub fn calibrate_ticks(delta: u32, pit_hz: u32, pit_ticks: u32) -> u64 {
    if pit_hz == 0 || pit_ticks == 0 {
        return 0;
    }
    // ticks_per_10ms = delta * (10ms worth of PIT ticks) / pit_ticks,
    // where 10ms of PIT = pit_hz / 100. All u64, saturating.
    let per_10ms = pit_hz as u64 / 100;
    (delta as u64)
        .saturating_mul(per_10ms)
        .checked_div(pit_ticks as u64)
        .unwrap_or(0)
}

/// Calibrate the LAPIC timer against the PIT ruler (Step 4).
///
/// One-shot, no LAPIC IRQs: DCR = divide-by-16, ICR = 0xFFFFFFFF, then
/// burn ~100 ms of CPU waiting for `TIMER_TICKS` to advance
/// `CALIB_PIT_TICKS` PIT ticks, and read CCR.
///
/// Requires IF=1 — the PIT ruler advances via IRQ0, and with IF=0 no ticks
/// ever arrive. Busy-wait (not `hlt`) deliberately: if the PIT itself is
/// dead, `hlt` would sleep forever past the spin-out guard below, while a
/// busy loop trips the guard in seconds and reports it. Main enables
/// interrupts before calling.
///
/// Stores `ticks_per_10ms`; prints the delta for the log. A zero delta
/// (MMIO cached? DCR/ICR write lost?) is a loud `Err`, not a silent zero
/// rate — Step 5 must never divide by it.
///
/// Call after [`init`] (window mapped) and `timer::init` (PIT ticking) —
/// only the PIT counter is read, no LAPIC IRQ involved.
pub fn calibrate() -> Result<u64, &'static str> {
    use core::sync::atomic::Ordering;
    let win = LAPIC_VIRT.load(Ordering::Relaxed);
    if win == 0 {
        return Err("apic: calibrate before init — LAPIC window not mapped");
    }
    let reg = |off: u64| lapic_reg(win, off);
    unsafe {
        // Divide-by-16: DCR bits 0,1,3 = 0b1011. One-shot mode: LVT masked
        // already (Step 3), so the counter just runs down without firing.
        core::ptr::write_volatile(reg(DCR_OFFSET)?, 0x3);
        core::ptr::write_volatile(reg(ICR_OFFSET)?, 0xFFFF_FFFF);

        let start = crate::idt::TIMER_TICKS.load(Ordering::Relaxed);
        // ~100 ms of PIT, then a spin-out guard: 100M busy iterations is
        // seconds of wall time — if the PIT is dead we say so instead of
        // hanging. Busy `spin_loop` ON PURPOSE here, not `hlt`: with a
        // dead PIT no IRQ ever arrives, so `hlt` would sleep forever and
        // the guard below would never run. Burning ~100 ms of CPU once
        // per boot is the price of a guard that actually guards.
        let mut spins: u64 = 0;
        loop {
            if crate::idt::TIMER_TICKS
                .load(Ordering::Relaxed)
                .wrapping_sub(start)
                >= CALIB_PIT_TICKS as u64
            {
                break;
            }
            spins += 1;
            if spins > 100_000_000 {
                return Err("apic: PIT produced no ticks during calibration — timer dead?");
            }
            core::hint::spin_loop();
        }

        let cur = core::ptr::read_volatile(reg(CCR_OFFSET)? as *const u32);
        let delta = 0xFFFF_FFFFu32.wrapping_sub(cur);
        if delta == 0 {
            return Err("apic: CCR delta is zero — MMIO writes lost (NO_CACHE missing?)");
        }
        let per_10ms = calibrate_ticks(delta, PIT_HZ, CALIB_PIT_TICKS);
        TICKS_PER_10MS.store(per_10ms, Ordering::Relaxed);
        crate::serial_println!(
            "apic-timer: calibrate ccr_delta={} (~{} ticks/ms)",
            delta,
            per_10ms / 10
        );
        Ok(per_10ms)
    }
}

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
    // SAFETY: EOI is write-only-0 by spec; the address goes through
    // `lapic_reg` (checked — the last unchecked `+` in the APIC path).
    // Offsets are < 4KiB, so Ok always lands inside the mapped window.
    let eoi = lapic_reg(win, EOI_OFFSET).expect("apic: EOI address overflow");
    unsafe {
        core::ptr::write_volatile(eoi, 0);
    }
}
