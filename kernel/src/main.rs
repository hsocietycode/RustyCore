#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

mod apic;
mod gdt;
mod idt;
mod interrupts;
mod memory;
mod preempt;
mod serial;
mod syscall;
mod task;
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
                    "self-test: LAPIC master proven (apic +{} like the gate, pic frozen at {}).",
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
    // Phase 3, Step 2b/4: tasks run ON their own stacks via a real naked-asm
    // switch, AND the LAPIC 0xEF vector is a naked preempt stub that yanks a
    // running task mid-step (saving its full frame) and resumes it where it
    // was. The demo logs the interleave, each stack top + ctx.rsp at spawn,
    // rsp0 per switch, and every real yank with its tick.
    // Step 4 proof block (runs BEFORE the demo): the fence must be DOWN on
    // the boot stack (nobody set it — no task runs yet), the EOI base must be
    // published (apic::init ran), and the stub address must be non-null.
    {
        use core::sync::atomic::Ordering;
        let fence = crate::preempt::PREEMPTIBLE.load(Ordering::Relaxed);
        assert_eq!(
            fence, 0,
            "preempt fence UP on boot stack — trampoline leaked it"
        );
        let eoi = unsafe { core::ptr::read_volatile(&raw const crate::preempt::EOI_BASE) };
        assert_ne!(
            eoi, 0,
            "preempt EOI base unpublished — apic::init didn't run?"
        );
        // Touch the stub's address so a stub that stops assembling (or gets
        // GC'd as dead code) breaks the build HERE, loudly, not silently.
        let stub = crate::preempt::lapic_preempt_stub as *const () as u64;
        assert_ne!(stub, 0, "preempt stub address is null");
        crate::serial_println!(
            "preempt: fence=down eoi-base={:#x} stub={:#x} switches={} (proof block ok)",
            eoi,
            stub,
            crate::preempt::PREEMPT_SWITCHES.load(Ordering::Relaxed),
        );
    }
    serial_println!("sched: phase 3 demo — 3 tasks + napper, own stacks, preempt clock...");
    {
        use crate::task::{Scheduler, Task};
        use core::sync::atomic::Ordering;
        /// Runaway guard: 3 tasks × 5 steps = 15 schedules healthy.
        /// Anything past 100 is a stuck task (never returns false) —
        /// halt loudly instead of spinning forever.
        const SCHED_RUNAWAY_GUARD: u64 = 100;
        /// Fairness bar: promises equal slices — every task must show
        /// exactly this many runs at the ledger (both policies: CFS with
        /// equal weights degenerates to fair round-robin).
        const SCHED_FAIR_RUNS: u64 = 5;
        // Policy pick by Cargo feature (build.rs guarantees the TOML agrees):
        // one driver, two policies, zero `if feature` in the hot loop.
        #[cfg(feature = "sched-rr")]
        let mut sched = crate::task::RoundRobin::new();
        #[cfg(feature = "sched-cfs")]
        let mut sched = crate::task::Cfs::new();
        // Each task prints its own step and quits after SCHED_FAIR_RUNS —
        // the interleave in the log IS the proof of fair rotation. The
        // counter lives INSIDE the closure (mut move) — no shared state,
        // no Arc, each task owns its ledger line.
        //
        // Weight witness: under CFS gamma carries double weight (2048 vs
        // 1024) — it must accrue vruntime half as fast and finish FIRST,
        // proving weights actually steer pick-next instead of decorating
        // the struct. Under RR weights don't exist (compiled out), so the
        // spawn stays plain there — same demo shape, policy-specific proof.
        for name in ["alpha", "beta", "gamma"] {
            let id = sched.next_id();
            let tag = alloc::string::String::from(name);
            let mut n: u64 = 0;
            let step = move || {
                n += 1;
                crate::serial_println!("task {}/{} step {}", id, tag, n);
                // Step 4 yank witness: keep the FIRST step inside the
                // fence-up window until a fresh LAPIC tick arrives. The naked
                // 0xEF stub must interrupt this loop, save all 15 GPRs + the
                // HW frame, EOI, restore, and resume EXACTLY here. If any
                // register or frame offset is wrong, we never reach step 2.
                if n == 1 {
                    let before = crate::idt::APIC_TICKS.load(Ordering::Relaxed);
                    while crate::idt::APIC_TICKS.load(Ordering::Relaxed) == before {
                        core::hint::spin_loop();
                    }
                }
                n < SCHED_FAIR_RUNS
            };
            #[cfg(feature = "sched-rr")]
            sched.spawn(Task::new(id, name, step));
            #[cfg(feature = "sched-cfs")]
            if name == "gamma" {
                sched.spawn(Task::new(id, name, step).with_weight(2048));
            } else {
                sched.spawn(Task::new(id, name, step));
            }
        }
        // F3: the Sleeping arm needs a live witness — dead code in a kernel
        // is debt with interest. Task 4 naps until a LAPIC tick, proving
        // the sleep/wake path works. It wakes, prints once, and finishes —
        // ledger must show runs==1 for it.
        {
            let id = sched.next_id();
            let wake_at = crate::idt::APIC_TICKS.load(Ordering::Relaxed) + 20;
            let mut woke = false;
            crate::serial_println!(
                "sched: task {}/napper naps until apic tick ~{}",
                id,
                wake_at
            );
            sched.spawn(Task::sleeping(id, "napper", wake_at, move || {
                if !woke {
                    woke = true;
                    crate::serial_println!("task {}/napper woke at tick", id);
                }
                false
            }));
        }
        // Drive the scheduler until every task finishes. Still driven from
        // main (the switch lands in 2b), but every pass observes the
        // preemption flag the LAPIC handler raises: a taken flag logs a
        // preempt point with the tick, proving the timer→policy path live.
        //
        // F2 fix: `schedule_once() == false` with `alive() > 0` means ALL
        // tasks are sleeping — NOT done. The old code `break`ed straight
        // to the ledger and screamed "unfair". Now: hlt-wait for the next
        // LAPIC tick (which wakes sleepers) and retry. `hlt` is safe here
        // — the LAPIC is unmasked and ticking, unlike the dead-clock trap.
        // A consecutive-false counter trips the runaway guard if ticks die.
        let mut schedules: u64 = 0;
        let mut asleep_passes: u64 = 0;
        let mut preempts: u64 = 0;
        // Step 4: the naked stub bumps PREEMPT_SWITCHES every time it yanks a
        // running task (fence up). The driver reads the DELTA after each
        // schedule — a rising counter means the stub fired mid-step and the
        // task RESUMED cleanly (a corrupt frame would have triple-faulted
        // long before we got here). Baseline = switches seen so far.
        let mut last_switches =
            crate::preempt::PREEMPT_SWITCHES.load(core::sync::atomic::Ordering::Relaxed);
        // Pace the demo on real LAPIC ticks: wait for a FRESH tick before
        // every schedule. IF=1 here (enabled before calibrate, never
        // disabled). The stub runs during the hlt too (fence down → just
        // ticks+EOI+iretq, no yank) — so APIC_TICKS keeps climbing.
        let mut last_tick = crate::idt::APIC_TICKS.load(Ordering::Relaxed);
        while sched.alive() > 0 {
            // Wait for the next LAPIC tick (hlt-sleep; the LAPIC is live).
            loop {
                let cur = crate::idt::APIC_TICKS.load(Ordering::Relaxed);
                if cur != last_tick {
                    last_tick = cur;
                    break;
                }
                x86_64::instructions::hlt(); // sleep until the next LAPIC tick
            }
            if !sched.schedule_once() {
                asleep_passes += 1;
                if asleep_passes > SCHED_RUNAWAY_GUARD {
                    serial_println!("sched FAILED: tasks asleep forever (ticks dead?). Halting.");
                    loop {
                        x86_64::instructions::hlt();
                    }
                }
                continue;
            }
            asleep_passes = 0;
            schedules += 1;
            // Step 4 proof: did the stub yank this task mid-step? A rising
            // switch count = the timer fired on a preemptible task AND the
            // resume worked (we're still here, the ledger is still fair).
            let switches =
                crate::preempt::PREEMPT_SWITCHES.load(core::sync::atomic::Ordering::Relaxed);
            if switches != last_switches {
                preempts += 1;
                serial_println!(
                    "sched: preempt yank {} (switches {}→{}) at apic tick ~{}",
                    preempts,
                    last_switches,
                    switches,
                    crate::idt::APIC_TICKS.load(Ordering::Relaxed)
                );
                last_switches = switches;
            }
            if schedules > SCHED_RUNAWAY_GUARD {
                serial_println!(
                    "sched FAILED: runaway ({} schedules, tasks still alive). Halting.",
                    SCHED_RUNAWAY_GUARD
                );
                loop {
                    x86_64::instructions::hlt();
                }
            }
        }
        // Step 4 gate: at least one yank must have landed on a preemptible
        // task and resumed cleanly. Zero means the stub never fired mid-step
        // (fence never up during a tick, or the timer→stub path is dead) —
        // say so loudly, not silently.
        if preempts == 0 {
            serial_println!("sched FAILED: preemption stub silent (0 yanks in demo). Halting.");
            loop {
                x86_64::instructions::hlt();
            }
        }
        // The ledger: alpha/beta/gamma must show exactly SCHED_FAIR_RUNS
        // (round-robin fairness made visible — unequal counts mean the
        // queue lied), napper must show exactly 1 (napped, woke, done).
        let mut fair = true;
        for (id, name, runs) in sched.ledger() {
            serial_println!("sched ledger: task {}/{} runs={}", id, name, runs);
            let want = if name == "napper" { 1 } else { SCHED_FAIR_RUNS };
            if runs != want {
                fair = false;
            }
        }
        if !fair {
            serial_println!("sched FAILED: unfair ledger. Halting.");
            loop {
                x86_64::instructions::hlt();
            }
        }
        #[cfg(feature = "sched-rr")]
        serial_println!("sched: round-robin fair (3×5 + napper×1). Phase 3 online. Halting.");
        #[cfg(feature = "sched-cfs")]
        serial_println!(
            "sched: cfs fair (3×5 + napper×1, gamma first by weight). Phase 3 online. Halting."
        );
    }
    loop {
        x86_64::instructions::hlt();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Polling-only serial: lock-free (fresh Ports, no statics), safe from
    // any stack — including a task stack mid-switch. Never touches the
    // scheduler queue (its Box<Task>s may be mid-mutation) or the heap
    // beyond the formatter's stack buffer.
    serial_println!("PANIC: {}", info);
    // A panicked kernel is DEAD — and dead means the interrupt machinery must
    // stop, not narrate. Two things are wrong by default at panic time:
    //
    // 1. IF may be 1 (a panic inside a task step runs with interrupts on), so
    //    every LAPIC tick would wake the hlt just to re-halt (wake-spam at
    //    100 Hz) — and worse, the naked 0xEF stub would keep running.
    // 2. `PREEMPTIBLE` may still be UP (a step panicked before its yield
    //    lowered the fence), while `CURRENT_IDX` still names the task that
    //    just died. The stub would then believe it is yanking a live task and
    //    STASH A FRAME into the dead task's slot at 100 Hz, so any post-mortem
    //    `snapshot` would read a bogus frame.
    //
    // Order matters: lower the fence FIRST (with IF still off it cannot race
    // the stub), then cli, then halt. No IRQ path runs after this point.
    crate::preempt::set_preemptible(false);
    x86_64::instructions::interrupts::disable();
    loop {
        x86_64::instructions::hlt();
    }
}
