//! APIC bring-up: probe → map → enable → calibrate → soak → promote.
//!
//! Step 1 is read-only detection (CPUID + IA32_APIC_BASE MSR). Step 3 maps
//! the LAPIC MMIO page into [`crate::memory::MMIO_WINDOW`] and enables via
//! SVR (LVT timer stays masked — PIC+PIT remains the one true clock until
//! Step 7). Step 4 calibrates the LAPIC timer against the PIT ruler
//! (three windows, median wins). Step 5 runs the periodic dual-clock soak.
//! Step 7 promotes the LAPIC to master clock with PIC-rollback on failure.

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
/// the LVT timer (`0x320 |= 1 << 16` — PIT stays master until Step 7
/// promotes the LAPIC), zeroes TPR (`0x80 = 0`), and reports ID/VER/SVR.
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
    // Publish the EOI register address to the naked preempt stub (Phase 3
    // Step 4): the stub EOIs with pure asm (no Rust call, no stack use), so
    // it reads this static. Single writer here (IF=0, no task running).
    crate::preempt::set_eoi_base(base.as_u64() + EOI_OFFSET);

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
pub fn ticks_per_10ms() -> u64 {
    TICKS_PER_10MS.load(core::sync::atomic::Ordering::Relaxed)
}

/// PIT rate (Hz) and ruler length (ticks) used by [`calibrate`].
/// 10 ticks at 100 Hz ≈ 100 ms ≈ ten 10 ms windows.
const PIT_HZ: u32 = 100;
const CALIB_PIT_TICKS: u32 = 10;

/// Calibration rounds for [`calibrate`]: three windows, median wins.
/// PIT IRQ jitter (±1 tick on each edge) shifts single-window deltas by
/// ~1–2%; the median of three shrugs it off. Three rounds cost ~300 ms of
/// boot — cheap against a clock that lies for the rest of uptime.
const CALIB_ROUNDS: usize = 3;

/// Busy-spin guard for the calibration windows (and the soak/watch loops
/// in main, which mirror this discipline): 10M iterations is ~a second of
/// wall time on QEMU TCG. The healthy path exits in microseconds, so a
/// tight guard costs nothing when clocks live — and a guard that fires in
/// seconds instead of minutes is the difference between a loud error and
/// a CI timeout. (Was 100M: minutes of wall time on weak CI before the
/// error surfaced.)
const SPIN_GUARD: u64 = 10_000_000;

/// Spin guard for the soak + promotion-watch loops in main: 100M
/// iterations, ~tens of seconds worst-case on TCG. Wider than
/// [`SPIN_GUARD`] on purpose — these loops watch TWO clocks through IRQ
/// jitter, and the healthy path still exits in ~1–2s, so the width costs
/// nothing when clocks live and buys patience when one leg stutters.
pub const SOAK_SPIN_GUARD: u64 = 100_000_000;

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

/// Calibrate the LAPIC timer against the PIT ruler (Step 4, refined Step 7).
///
/// One-shot, no LAPIC IRQs: DCR = divide-by-16, ICR = 0xFFFFFFFF, then
/// burn ~100 ms of CPU per round waiting for `TIMER_TICKS` to advance
/// `CALIB_PIT_TICKS` PIT ticks, and read CCR.
///
/// Three rounds, median wins: PIT IRQ jitter (±1 tick on window edges)
/// skews single-window runs by ~1–2% — that skew IS the ~11-tick drift we
/// watched all through Step 5 (`pic=100 apic=~89`). The median of three
/// shrugs the jitter off and programs a quantum both clocks agree on.
/// Requires IF=1 — the PIT ruler advances via IRQ0, and with IF=0 no ticks
/// ever arrive. Busy-wait (not `hlt`) deliberately: if the PIT itself is
/// dead, `hlt` would sleep forever past the spin-out guard below, while a
/// busy loop trips the guard in seconds and reports it. Main enables
/// interrupts before calling.
///
/// Stores `ticks_per_10ms`; prints all three deltas plus the median for the
/// log. A zero median (MMIO cached? DCR/ICR writes lost?) is a loud `Err`,
/// not a silent zero rate — Step 5 must never divide by it.
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

        let mut deltas = [0u32; CALIB_ROUNDS];
        for slot in deltas.iter_mut() {
            // Fresh full counter every round — each window stands alone.
            core::ptr::write_volatile(reg(ICR_OFFSET)?, 0xFFFF_FFFF);

            let start = crate::idt::TIMER_TICKS.load(Ordering::Relaxed);
            // ~100 ms of PIT, then the shared spin guard. Busy `spin_loop`
            // ON PURPOSE here, not `hlt`: with a dead PIT no IRQ ever
            // arrives, so `hlt` would sleep forever past the guard — a guard
            // that can never fire is dead code wearing a guard's name.
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
                if spins > SPIN_GUARD {
                    return Err("apic: PIT produced no ticks during calibration — timer dead?");
                }
                core::hint::spin_loop();
            }

            let cur = core::ptr::read_volatile(reg(CCR_OFFSET)? as *const u32);
            let delta = 0xFFFF_FFFFu32.wrapping_sub(cur);
            if delta == 0 {
                return Err("apic: CCR delta is zero — MMIO writes lost (NO_CACHE missing?)");
            }
            *slot = delta;
        }

        // Median of three by hand — no sorting dependency in the kernel.
        // min-max dance: max(min(a,b), min(max(a,b),c)) is the middle value.
        let (a, b, c) = (deltas[0], deltas[1], deltas[2]);
        let median = a.min(b).max(c.min(a.max(b)));
        let spread = a.max(b).max(c) - a.min(b).min(c);
        let per_10ms = calibrate_ticks(median, PIT_HZ, CALIB_PIT_TICKS);
        if per_10ms == 0 {
            return Err("apic: calibrated rate is zero — refusing to program it");
        }
        TICKS_PER_10MS.store(per_10ms, Ordering::Relaxed);
        crate::serial_println!(
            "apic-timer: calibrate rounds=[{} {} {}] median={} spread={} (~{} ticks/ms)",
            deltas[0],
            deltas[1],
            deltas[2],
            median,
            spread,
            per_10ms / 10
        );
        Ok(per_10ms)
    }
}

/// Start the LAPIC timer in periodic mode (Step 5).
///
/// Programs LVT entry `0x320 = vector | periodic(1<<17) | unmasked` with
/// the 0xEF sidecar vector registered in Step 2, sets ICR to one 10 ms
/// quantum from [`ticks_per_10ms`], and returns. From here the LAPIC fires
/// IRQ 0xEF every ~10 ms alongside the PIT's IRQ0 — the dual-clock soak.
///
/// # Errors
/// `Err` when the window is unmapped (before [`init`]) or the rate is
/// zero (calibration failed) — a zero ICR would fire continuously and
/// livelock the kernel, so refusal is the only safe shape.
pub fn start_periodic() -> Result<u64, &'static str> {
    use core::sync::atomic::Ordering;
    let win = LAPIC_VIRT.load(Ordering::Relaxed);
    if win == 0 {
        return Err("apic: start_periodic before init — LAPIC window not mapped");
    }
    let quantum = ticks_per_10ms();
    if quantum == 0 || quantum > u32::MAX as u64 {
        return Err("apic: no calibrated rate — refusing to program a zero/huge ICR");
    }
    let reg = |off: u64| lapic_reg(win, off);
    unsafe {
        // LVT timer: vector 0xEF, periodic mode, unmasked.
        // (Delivery mode bits are 0 = fixed; mask bit 16 CLEAR to unmask.)
        core::ptr::write_volatile(reg(LVT_TIMER_OFFSET)?, 0xEF | (1 << 17));
        core::ptr::write_volatile(reg(ICR_OFFSET)?, quantum as u32);
    }
    crate::serial_println!(
        "apic-timer: periodic @ ~100 Hz (icr={} / 10ms quantum)",
        quantum
    );
    Ok(quantum)
}

/// End-of-interrupt to the local APIC: write 0 to the EOI register.
///
/// Uses the cached [`MMIO_WINDOW`](crate::memory::MMIO_WINDOW) address from
/// [`init`] — no MSR read, no arithmetic in the hot path.
///
/// Returns `Err` (instead of panicking) when the window is not mapped yet —
/// the IDT (with the LAPIC handlers) loads before [`init`] maps the window,
/// so a stray 0xEF in that gap must degrade to a loud log in the handler,
/// not a panic inside an interrupt (which would double-panic the kernel).
///
/// # Safety
/// Caller must be the LAPIC timer/error handler (or Step-5 bring-up):
/// writing EOI with no interrupt in service is harmless, but writing to a
/// wrong address is not. Only call after [`init`] succeeded.
pub fn eoi() -> Result<(), &'static str> {
    let win = LAPIC_VIRT.load(core::sync::atomic::Ordering::Relaxed);
    if win == 0 {
        return Err("apic: eoi before init — LAPIC window not mapped");
    }
    // SAFETY: EOI is write-only-0 by spec; the address goes through
    // `lapic_reg` (checked — the last unchecked `+` in the APIC path).
    // Offsets are < 4KiB, so Ok always lands inside the mapped window.
    let eoi = lapic_reg(win, EOI_OFFSET).expect("apic: EOI address overflow");
    unsafe {
        core::ptr::write_volatile(eoi, 0);
    }
    Ok(())
}

/// Promote the LAPIC timer to master clock (Step 7).
///
/// Gates: the soak in main must have proven both clocks (each reaching
/// [`crate::idt::SOAK_TICKS_EACH`]) before this is called — promoting on an
/// unproven APIC would trade a working PIT for a silent box. This function
/// trusts the caller on the gate (it cannot see the counters' history) and
/// enforces the part it CAN see: window mapped, rate non-zero.
///
/// What it does: masks IRQ0 on the PIC (IRQ1 keyboard stays — the LAPIC
/// knows nothing of keystrokes), keeps the LAPIC periodic rate untouched,
/// and returns. From here 0xEF is the one true tick; IRQ0 is silent.
///
/// Rollback lives in the caller (main): if the LAPIC stalls post-promotion,
/// [`rollback_to_pit`] unmasks IRQ0 and the PIT carries the kernel again.
/// Promotion without a rollback plan is a leap, not engineering.
pub fn promote() -> Result<(), &'static str> {
    use core::sync::atomic::Ordering;
    if LAPIC_VIRT.load(Ordering::Relaxed) == 0 {
        return Err("apic: promote before init — LAPIC window not mapped");
    }
    if ticks_per_10ms() == 0 {
        return Err("apic: promote without a calibrated rate — refusing");
    }
    // SAFETY: `read_masks`/`write_masks` are `unsafe` in pic8259 0.11
    // because they poke real hardware ports — but here that IS the job:
    // IRQ0/IRQ1 masks on the master PIC, nothing else moves. The PIC was
    // remapped to 32..=47 at boot, so mask bits map to real IRQ lines.
    // Mask IRQ0 (timer), keep IRQ1 (keyboard): read the live masks and
    // set bit 0 of the master.
    //
    // CRITICAL: the read-modify-write runs with INTERRUPTS DISABLED.
    // `PICS` is a `spin::Mutex`, which is not interrupt-safe, and this
    // function is called with IF=1 and IRQ0 still unmasked. If the PIT fired
    // between `lock()` and the guard's drop, `timer_handler` would call
    // `PICS.lock()` on the SAME CPU and spin forever waiting for a lock that
    // the interrupted code can never release — a single-CPU deadlock that
    // never even reaches the EOI, so the PIC then stops delivering anything.
    // (The old code argued the interleaving was harmless because "EOI never
    // touches masks" — true, and irrelevant: the lock, not the mask, is what
    // deadlocks.) `without_interrupts` closes the window completely.
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let mut pics = crate::interrupts::PICS.lock();
        let masks = pics.read_masks();
        pics.write_masks(masks[0] | 0x01, masks[1]);
    });
    crate::serial_println!("apic-timer: promoted to master clock (PIC IRQ0 masked, IRQ1 kept)");
    Ok(())
}

/// Roll back to the PIT: unmask IRQ0 on the PIC.
///
/// The inverse of [`promote`]: master mask bit 0 → 0 (IRQ1 and everything
/// else untouched). Called when the post-promotion watch sees the LAPIC
/// stall — the PIT, never disabled, resumes ticking immediately.
pub fn rollback_to_pit() {
    // SAFETY: same contract as `promote` — mask bit 0 → 0 on the master
    // PIC, everything else untouched.
    //
    // Same lock discipline as `promote`: `PICS` is a non-interrupt-safe
    // spinlock and IRQ0 may be live here, so the read-modify-write runs with
    // interrupts disabled (a PIT tick landing inside the window would spin
    // for a lock the interrupted code still holds — see the note in
    // `promote`). The callers (`main`'s rollback) run with IF=1.
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let mut pics = crate::interrupts::PICS.lock();
        let masks = pics.read_masks();
        pics.write_masks(masks[0] & !0x01, masks[1]);
    });
    crate::serial_println!("apic-timer: ROLLED BACK to PIT (IRQ0 unmasked)");
}
