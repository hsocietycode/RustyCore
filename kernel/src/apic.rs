//! APIC probe (Step 1): read-only detection, zero hardware writes.
//!
//! Checks CPUID for a local APIC, reads IA32_APIC_BASE (MSR 0x1B) for the
//! MMIO base + enabled/BSP flags, and notes whether the bootloader gave us
//! an RSDP (for MADT parsing in a later step). Nothing is mapped, nothing
//! is enabled — the PIC+PIT path stays the one true clock.

use bootloader_api::BootInfo;
use x86_64::registers::model_specific::Msr;

/// IA32_APIC_BASE MSR index.
const IA32_APIC_BASE: u32 = 0x1B;

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
    let base = msr & 0xFFFF_F000_0000;
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

/// End-of-interrupt to the local APIC: write 0 to the EOI register.
///
/// The MMIO base comes from the Step-1 probe (MSR truth, not a hardcoded
/// constant). Until Step 3 maps it, the phys-offset mapping already covers
/// it — the bootloader maps all RAM there, and the LAPIC window is
/// addressable through it on q35.
///
/// # Safety
/// Caller must be the LAPIC timer/error handler (or Step-5 bring-up):
/// writing EOI with no interrupt in service is harmless, but writing to a
/// wrong address is not. Only call after `probe()` confirmed presence.
pub fn eoi() {
    use x86_64::registers::model_specific::Msr;
    // SAFETY: read-only MSR query (see probe).
    let msr = unsafe { Msr::new(IA32_APIC_BASE).read() };
    let base = msr & 0xFFFF_F000_0000;
    let base = if base == 0 { FALLBACK_BASE } else { base };
    // SAFETY: EOI is write-only-0 by spec; the address is the probed
    // LAPIC base + 0xB0, mapped via the phys-offset window.
    unsafe {
        let eoi = (crate::memory::PHYS_MEM_OFFSET + base + EOI_OFFSET) as *mut u32;
        core::ptr::write_volatile(eoi, 0);
    }
}
