#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

mod apic;
mod gdt;
mod idt;
mod interrupts;
mod memory;
mod serial;
mod syscall;
mod timer;

use bootloader_api::{entry_point, BootInfo, BootloaderConfig};
use core::panic::PanicInfo;

pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(bootloader_api::config::Mapping::FixedAddress(
        memory::PHYS_MEM_OFFSET,
    ));
    config
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    // IF=0 until the IDT is loaded — any interrupt before that
    // vectors through garbage and triple-faults.
    x86_64::instructions::interrupts::disable();

    serial::init();
    serial_println!("RustyCore v0.2 - serial online.");

    // Segments + TSS first (double-fault IST must exist before any
    // handler can run), then the IDT, then the PIC remap.
    gdt::init();
    serial_println!("RustyCore v0.2 - GDT+TSS loaded (double-fault IST ready).");
    idt::init();
    serial_println!("RustyCore v0.2 - IDT loaded (exceptions + IRQ vectors live).");
    interrupts::init();
    serial_println!("RustyCore v0.2 - PIC remapped to 32..=47, all masked.");

    // APIC probe is read-only (CPUID + MSR), so it runs before memory
    // init — memory::init takes boot_info by move, and the probe must
    // not depend on heap or page tables anyway.
    let apic = apic::probe(boot_info);
    // Hard gate: every x86_64 machine worth booting has a local APIC.
    // No APIC → no timer future → say so now, not three phases later.
    assert!(
        apic.present,
        "no local APIC (CPUID bit 9 clear) — cannot continue"
    );

    let (mut mapper, mut frame_allocator) = memory::init(boot_info);

    // Step 3: map the LAPIC MMIO page + enable the local APIC (volatile
    // SVR/LVT/TPR). PIC+PIT stays the one true clock — LVT timer masked.
    // Boot must fail loudly here: a half-mapped APIC is worse than none.
    apic::init(&mut mapper, &mut frame_allocator, &apic).expect("apic init failed");

    timer::init();

    // IF=1 from here on: the IDT is fully loaded, the PIC is remapped, and
    // Step 4 NEEDS hardware IRQs — the PIT ruler only advances via IRQ0.
    // (int3/syscall self-tests below don't need IF, but IRQs enabled early
    // is exactly the state Step 5 inherits.)
    serial_println!("self-test: enabling interrupts...");
    x86_64::instructions::interrupts::enable();

    // Step 4: calibrate the LAPIC timer against the PIT ruler (one-shot,
    // MMIO only — the LAPIC LVT stays masked, no APIC IRQ involved).
    // Loud failure: no rate means Step 5 has nothing to program.
    apic::calibrate().expect("apic calibrate failed");

    // Self-test #1: software breakpoint. If the IDT is wired right,
    // the handler prints the stack frame and we continue here.
    serial_println!("self-test: firing int3 breakpoint...");
    x86_64::instructions::interrupts::int3();
    serial_println!("self-test: breakpoint handler returned, IDT works.");

    // Self-test #1b: syscall stub (Step 2). `int 0x80` twice — software
    // interrupts don't need IF, but it's already on. Proves the 0x80 gate.
    syscall::self_test();

    // Step 5: dual-clock soak. Start the LAPIC timer in periodic mode
    // (~100 Hz, sidecar vector 0xEF) and let it run ALONGSIDE the PIT.
    // PIC+PIT stays master — if the APIC never fires, we say so loudly
    // and continue on the PIT alone. Never brick on a sidecar.
    apic::start_periodic().expect("apic start_periodic failed");

    // Self-test #2: dual clock. Wait until BOTH clocks reach the gate
    // (SOAK_TICKS_EACH each, ~1 second at 100 Hz) — IRQ0 through the PIC
    // and 0xEF from the LAPIC, side by side on the same serial line.
    serial_println!("self-test: dual-clock soak (pic + apic, gate each)...");

    let mut spins: u64 = 0;
    let mut last_pic: u64 = 0;
    let mut last_apic: u64 = 0;
    loop {
        use crate::idt::SOAK_TICKS_EACH;
        use core::sync::atomic::Ordering;
        let pic = idt::TIMER_TICKS.load(Ordering::Relaxed);
        let apic_ticks = idt::APIC_TICKS.load(Ordering::Relaxed);
        // Progress every 20 ticks on EITHER leg — one line per ~200 ms.
        // Either clock alone proves life, so watch both, not just the PIC:
        // a dead PIC with a live APIC must still narrate, not go silent.
        if pic >= last_pic + 20 || apic_ticks >= last_apic + 20 {
            last_pic = pic;
            last_apic = apic_ticks;
            serial_println!("tick: pic={} apic=~{}", pic, apic_ticks);
        }
        if pic >= SOAK_TICKS_EACH && apic_ticks >= SOAK_TICKS_EACH {
            break;
        }
        // APIC silent but PIC alive at the gate? Loud fallback, PIT carries on.
        if pic >= SOAK_TICKS_EACH && apic_ticks == 0 {
            serial_println!("apic-timer: NO IRQs (PIC ok, continuing)");
            break;
        }
        // APIC whispering but lapped twice by the PIC without reaching gate?
        // Degraded, not dead — report with counts, continue on the PIT.
        // (apic==0 already broke above, so this arm is strictly 0<apic<gate.)
        if pic >= SOAK_TICKS_EACH * 2 && apic_ticks < SOAK_TICKS_EACH {
            serial_println!(
                "apic-timer: DEGRADED (pic={} apic=~{} after ~2s, PIC ok, continuing)",
                pic,
                apic_ticks
            );
            break;
        }
        spins += 1;
        // Busy `spin_loop`, NOT `hlt`: with both clocks dead no IRQ ever
        // arrives, so `hlt` would sleep forever past this guard — the old
        // code did exactly that (a guard that can never fire is dead code
        // wearing a guard's name). Healthy path exits in ~1s; the guard
        // width (tens of seconds worst-case on TCG) costs nothing when
        // clocks live — and actually guards when they don't. Same
        // discipline as `apic::calibrate`, wider for two-clock jitter.
        if spins > crate::apic::SOAK_SPIN_GUARD {
            serial_println!(
                "self-test FAILED: clocks stalled (pic={} apic=~{}), spun out. Halting.",
                pic,
                apic_ticks
            );
            loop {
                x86_64::instructions::hlt();
            }
        }
        core::hint::spin_loop();
    }

    serial_println!(
        "self-test: dual-clock soak done (spurious={}).",
        idt::SPURIOUS_HITS.load(core::sync::atomic::Ordering::Relaxed)
    );

    // Step 7: promotion. The soak above is the gate — we only get here
    // with BOTH clocks proven, or via the loud PIC-only fallback. Promote
    // only on proven dual-life: if the APIC leg never reached the gate,
    // promoting trades a working PIT for a silent box. Check the counters,
    // not our hopes.
    {
        use crate::idt::SOAK_TICKS_EACH;
        use core::sync::atomic::Ordering;
        let pic = idt::TIMER_TICKS.load(Ordering::Relaxed);
        let apic_ticks = idt::APIC_TICKS.load(Ordering::Relaxed);
        if pic < SOAK_TICKS_EACH || apic_ticks < SOAK_TICKS_EACH {
            serial_println!(
                "apic-timer: promotion SKIPPED (pic={} apic=~{} — soak did not prove dual-life). PIT stays master. Phase 2 online. Halting.",
                pic,
                apic_ticks
            );
            loop {
                x86_64::instructions::hlt();
            }
        }
    }

    apic::promote().expect("apic promote failed");

    // Post-promotion watch: ~1s of LAPIC-only ticks. PIC IRQ0 is masked,
    // so TIMER_TICKS must freeze while APIC_TICKS climbs by a full gate.
    // If the LAPIC stalls, roll back to the PIT loudly — a promotion
    // without a rollback plan is a leap, not engineering.
    serial_println!("self-test: promotion watch (LAPIC-only, ~1s)...");
    {
        use crate::idt::SOAK_TICKS_EACH;
        use core::sync::atomic::Ordering;
        let apic_base = idt::APIC_TICKS.load(Ordering::Relaxed);
        let mut spins: u64 = 0;
        let mut last_apic: u64 = apic_base;
        loop {
            let apic_ticks = idt::APIC_TICKS.load(Ordering::Relaxed);
            if apic_ticks >= last_apic + 20 {
                last_apic = apic_ticks;
                serial_println!(
                    "promoted tick: apic=~{} (+{})",
                    apic_ticks,
                    apic_ticks - apic_base
                );
            }
            if apic_ticks - apic_base >= SOAK_TICKS_EACH {
                let pic_now = idt::TIMER_TICKS.load(Ordering::Relaxed);
                serial_println!(
                    "self-test: LAPIC master proven (apic +{} like the gate, pic frozen at {}). Phase 2 online. Halting.",
                    apic_ticks - apic_base,
                    pic_now
                );
                break;
            }
            spins += 1;
            // Same guard discipline as the soak: busy-spin (IRQs still
            // arrive — the LAPIC is unmasked — but a STALLED lapic means
            // `hlt` sleeps past the rollback forever). Healthy watch exits
            // in ~1s; the width is patience for jitter, not sloth.
            if spins > crate::apic::SOAK_SPIN_GUARD {
                serial_println!(
                    "self-test: LAPIC STALLED post-promotion (apic +{} in guard window) — rolling back to PIT.",
                    apic_ticks - apic_base
                );
                apic::rollback_to_pit();
                serial_println!("self-test: PIT master again. Phase 2 online (degraded). Halting.");
                break;
            }
            core::hint::spin_loop();
        }
    }
    loop {
        x86_64::instructions::hlt();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Polling-only serial: safe even with IF=0.
    serial_println!("PANIC: {}", info);
    loop {
        x86_64::instructions::hlt();
    }
}
