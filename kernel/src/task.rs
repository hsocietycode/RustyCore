//! Cooperative tasks — Phase 3, Step 1.
//!
//! The scene so far: the LAPIC ticks at ~100 Hz into `APIC_TICKS`, memory
//! is bump-allocated, interrupts are live. What we DON'T have yet: a stack
//! per task, a saved register file, a real context switch — that hardware
//! needs `TSS.rsp0`, a per-task page table story, and a sweaty afternoon
//! with `asm!`. This step builds everything UP TO that line:
//!
//! - [`Task`]: an id, a name, a state, a step function, a run counter.
//! - [`Scheduler`]: the trait Phase 3 promises (`sched-rr` now, `sched-cfs`
//!   later) — pick-next + tick accounting live here, behind one interface.
//! - [`RoundRobin`]: a VecDeque of ready tasks, FIFO with requeue.
//! - A boot demo: three tasks yield to each other N rounds, the log shows
//!   the interleave, the counters prove nobody starved.
//!
//! Why cooperative first: a preemptive switch needs the full context
//! machinery, but the SCHEDULER (who runs next? who starved? who hogs?)
//! is pure logic — testable today, on top of LAPIC ticks, with zero asm.
//! Step 2 will add stacks + context switch UNDER this same trait, and the
//! demo tasks won't even notice.

use alloc::{collections::VecDeque, string::String, vec::Vec};

/// What a task is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// Sitting in the ready queue, waiting for its turn.
    Ready,
    /// Currently executing (exactly one task per CPU is ever here).
    Running,
    /// Asked to be skipped until a later tick (sleep-until).
    /// Woken by [`Scheduler::schedule_once`] when `APIC_TICKS` reaches
    /// `until_tick` — the Step-1 demo parks the napper this way.
    Sleeping { until_tick: u64 },
    /// Done — never scheduled again. The scheduler drops it loudly
    /// (a finish line in the log, not a silent vanish).
    Finished,
}

/// A cooperative task: a named step function with an id and a ledger.
///
/// The step function runs once per schedule and returns `true` to stay
/// ready or `false` to finish. No stacks, no registers yet — the "context"
/// is whatever the closure captures. Real stacks land in Step 2 UNDER this
/// same shape: `step` keeps its signature, the scheduler keeps its trait.
pub struct Task {
    /// Stable number, handed out by the scheduler. Log lines carry it so
    /// the interleave is readable (`task 1/alpha`, not anonymous noise).
    pub id: usize,
    /// Human name for the log. Short — serial is slow.
    pub name: String,
    /// Current lifecycle state.
    pub state: TaskState,
    /// How many times this task has run. Starvation detector: at the end
    /// of the demo every task's count must be equal (round-robin promise).
    pub runs: u64,
    /// The work. `FnMut` — tasks may mutate captured state across runs.
    step: alloc::boxed::Box<dyn FnMut() -> bool>,
}

impl Task {
    /// Build a task. Starts [`TaskState::Ready`], zero runs.
    pub fn new(id: usize, name: &str, step: impl FnMut() -> bool + 'static) -> Self {
        Self {
            id,
            name: String::from(name),
            state: TaskState::Ready,
            runs: 0,
            step: alloc::boxed::Box::new(step),
        }
    }

    /// Build a task parked asleep until `until_tick` (APIC_TICKS domain).
    /// The scheduler carries it through the queue untouched until the tick
    /// arrives — no busy-wait, the caller hlt-sleeps between passes.
    pub fn sleeping(
        id: usize,
        name: &str,
        until_tick: u64,
        step: impl FnMut() -> bool + 'static,
    ) -> Self {
        Self {
            id,
            name: String::from(name),
            state: TaskState::Sleeping { until_tick },
            runs: 0,
            step: alloc::boxed::Box::new(step),
        }
    }

    /// Run one step. Returns `true` = still ready, `false` = finished.
    /// Increments the run ledger either way — an attempt is an attempt.
    fn run_step(&mut self) -> bool {
        self.state = TaskState::Running;
        self.runs += 1;
        let alive = (self.step)();
        self.state = if alive {
            TaskState::Ready
        } else {
            TaskState::Finished
        };
        alive
    }
}

/// The scheduler contract Phase 3 promises.
///
/// `RoundRobin` implements this today; `sched-cfs` will implement the SAME
/// trait when it lands — main picks by Cargo feature with a `compile_error`
/// gate (like the alloc-bump/buddy gate in `memory::init`), so selecting
/// an unimplemented policy refuses to build instead of silently booting
/// the wrong scheduler. One interface, two policies, zero `if feature`
/// in the hot path.
pub trait Scheduler {
    /// Add a task to the ready set. Ids are assigned by the caller
    /// (main hands out 1, 2, 3...) — the scheduler never invents identity.
    fn spawn(&mut self, task: Task);

    /// Run the next ready task once. Returns `false` when nothing is
    /// ready (all finished, or all sleeping — see below) — the caller
    /// halts or hlt-waits, never spins.
    ///
    /// All-sleeping subtlety: sleeping tasks stay QUEUED (they are alive),
    /// but if a whole pass finds nobody awake, this returns `false` so the
    /// caller can `hlt` until the next timer tick instead of burning CPU
    /// re-checking sleepers. The next call retries — sleepers wake by tick.
    fn schedule_once(&mut self) -> bool;

    /// How many tasks are still alive (ready + running + sleeping).
    /// Zero means the demo is over — main prints the ledger and halts.
    fn alive(&self) -> usize;

    /// Snapshot of (id, name, runs) for the final ledger. Read-only —
    /// the log, not the machinery.
    fn ledger(&self) -> Vec<(usize, String, u64)>;
}

/// Round-robin: FIFO ready queue, finished tasks dropped, one step each.
///
/// The simplest fair policy: whoever has waited longest runs next, every
/// task gets an equal slice of schedule calls. No priorities, no vruntime
/// — that sophistication is `sched-cfs`'s chapter, behind this same trait.
///
/// Honesty gate (same style as `memory::init`): `sched-cfs` doesn't exist
/// yet — selecting it (or nothing, or both) refuses to build instead of
/// silently booting round-robin under a CFS name.
#[cfg(all(feature = "sched-rr", feature = "sched-cfs"))]
compile_error!("select exactly one of sched-rr / sched-cfs, not both");
#[cfg(not(any(feature = "sched-rr", feature = "sched-cfs")))]
compile_error!("select one of sched-rr / sched-cfs");
#[cfg(all(feature = "sched-cfs", not(feature = "sched-rr")))]
compile_error!("sched-cfs is selected but not implemented yet — build with sched-rr");
pub struct RoundRobin {
    ready: VecDeque<Task>,
    finished_runs: Vec<(usize, String, u64)>,
    next_id: usize,
}

impl RoundRobin {
    /// Empty scheduler. Tasks arrive via [`Scheduler::spawn`].
    pub fn new() -> Self {
        Self {
            ready: VecDeque::new(),
            finished_runs: Vec::new(),
            next_id: 1,
        }
    }

    /// Hand out the next task id (1, 2, 3...). Main uses this so task
    /// numbering lives in one place, not scattered magic numbers.
    pub fn next_id(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

impl Default for RoundRobin {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler for RoundRobin {
    fn spawn(&mut self, task: Task) {
        crate::serial_println!("sched: spawned task {}/{}", task.id, task.name);
        self.ready.push_back(task);
    }

    fn schedule_once(&mut self) -> bool {
        // F1 fix: the old code pushed a still-sleeping task back and
        // returned `true` — with ALL tasks asleep, the caller spun forever
        // (`alive() > 0` forever, `schedule_once` never false). Now: one
        // full pass over the queue; if nobody was awake, return `false`
        // so the caller hlt-waits for the next tick instead of burning CPU.
        let len = self.ready.len();
        if len == 0 {
            return false;
        }
        for _ in 0..len {
            let mut task = match self.ready.pop_front() {
                Some(t) => t,
                None => return false,
            };
            // Sleeping tasks park until their tick — cooperative, so the
            // check is here, not in a timer hook.
            if let TaskState::Sleeping { until_tick } = task.state {
                let now = crate::idt::APIC_TICKS.load(core::sync::atomic::Ordering::Relaxed);
                if now < until_tick {
                    self.ready.push_back(task);
                    continue;
                }
                task.state = TaskState::Ready;
            }
            let alive = task.run_step();
            if alive {
                self.ready.push_back(task);
            } else {
                crate::serial_println!(
                    "sched: task {}/{} finished after {} runs",
                    task.id,
                    task.name,
                    task.runs
                );
                self.finished_runs.push((task.id, task.name, task.runs));
            }
            return true;
        }
        // Whole pass, nobody awake — all sleeping. Caller hlt-waits.
        false
    }

    fn alive(&self) -> usize {
        self.ready.len()
    }

    fn ledger(&self) -> Vec<(usize, String, u64)> {
        let mut out: Vec<(usize, String, u64)> = self
            .ready
            .iter()
            .map(|t| (t.id, t.name.clone(), t.runs))
            .collect();
        out.extend(self.finished_runs.iter().cloned());
        out.sort_by_key(|(id, _, _)| *id);
        out
    }
}
