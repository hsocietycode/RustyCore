//! Preemptive foundations — Phase 3, Step 2a.
//!
//! The scene so far: the LAPIC ticks at ~100 Hz into `APIC_TICKS`, memory
//! is bump-allocated, interrupts are live, and Step 1 proved the scheduler
//! logic cooperatively. What we DON'T have yet: a saved register file and
//! a real context switch — that hardware needs `TSS.rsp0`, a per-task page
//! table story, and a sweaty afternoon with `asm!` (Step 2b). This step
//! builds everything UP TO that line:
//!
//! - [`Task`]: an id, a name, a state, a step function, a run counter —
//!   plus its OWN [`TaskStack`] (heap-backed, 16-byte-aligned top).
//! - [`Scheduler`]: the trait Phase 3 promises (`sched-rr` now, `sched-cfs`
//!   later) — pick-next + tick accounting live here, behind one interface.
//! - [`RoundRobin`]: a VecDeque of ready tasks, FIFO with requeue.
//! - Preemption clock: [`timer_tick`] (called from the LAPIC handler, IRQ
//!   context, atomics only) raises [`NEED_RESCHED`] every [`QUANTUM_TICKS`]
//!   ticks; main observes it via [`take_preempt_flag`] and logs each
//!   preempt point. The handler never schedules — it only raises the flag.
//! - A boot demo: three tasks + a napper, stacks printed at spawn, the log
//!   shows the interleave AND the preempt points, the ledger proves nobody
//!   starved.

use alloc::{collections::VecDeque, string::String, vec::Vec};

/// Per-task kernel stack size in bytes, wired from `stack_size = "128K"` in
/// `config/kernel_config.toml` by build.rs (decimal bytes string).
pub const STACK_SIZE_BYTES: usize = parse_stack_bytes();

const fn parse_stack_bytes() -> usize {
    let s = env!("KERNEL_CONFIG_STACK_SIZE_BYTES").as_bytes();
    let mut n: usize = 0;
    let mut i = 0;
    while i < s.len() {
        let d = s[i].wrapping_sub(b'0');
        assert!(d < 10, "stack_size must be digits");
        n = n * 10 + d as usize;
        i += 1;
    }
    n
}

/// Preemption quantum in LAPIC ticks. The LAPIC ticks at ~100 Hz, so 10 ticks
/// ≈ 100 ms per task before the timer asks for a reschedule. Named, not
/// magic — Step 2b will enforce this in the switch path; Step 2a only
/// accounts (sets the flag, main observes it).
pub const QUANTUM_TICKS: u64 = 10;

/// Set by [`timer_tick`] when a quantum expires, cleared by main when it
/// observes a reschedule point. The LAPIC handler never schedules directly
/// (no locking, no queue surgery at IRQ time) — it only raises the flag.
pub static NEED_RESCHED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Ticks seen by the scheduler layer (mirrors `APIC_TICKS`, counted here so
/// the preemption policy owns its own clock reading).
pub static PREEMPT_TICKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Called from the LAPIC timer handler (IRQ context): cheap atomics only,
/// no locking, no printing — serial from IRQ time risks reentrancy with
/// main's own prints. Every [`QUANTUM_TICKS`]-th tick raises NEED_RESCHED.
pub fn timer_tick() {
    use core::sync::atomic::Ordering;
    let t = PREEMPT_TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    if t.is_multiple_of(QUANTUM_TICKS) {
        NEED_RESCHED.store(true, Ordering::Relaxed);
    }
}

/// Take the reschedule flag (swap to false): `true` means a quantum expired
/// since the last check. Main calls this at schedule points — IRQ time only
/// ever SETS the flag, never clears it, so no tick is lost between checks.
pub fn take_preempt_flag() -> bool {
    NEED_RESCHED.swap(false, core::sync::atomic::Ordering::Relaxed)
}

/// A per-task kernel stack: a heap-backed region whose top is 16-byte
/// aligned (SysV ABI contract — Step 2b's context switch will `ret` onto
/// it, and a misaligned stack is a silent SSE fault waiting to happen).
pub struct TaskStack {
    /// Owning backing store — dropping the task frees its stack.
    _backing: alloc::boxed::Box<[u8]>,
    /// Virtual address of the stack TOP (stacks grow down; the switch
    /// path loads this into RSP on first run).
    pub top: u64,
    /// Virtual address of the stack bottom (for guard-page math later).
    pub bottom: u64,
}

impl TaskStack {
    /// Allocate a fresh stack. Panics loudly on OOM — a task without a
    /// stack is not a task, and booting past it would corrupt memory.
    pub fn new() -> Self {
        use alloc::vec;
        let backing: alloc::boxed::Box<[u8]> = vec![0u8; STACK_SIZE_BYTES].into_boxed_slice();
        let bottom = backing.as_ptr() as u64;
        // Top aligned down to 16 — Vec backing is already aligned, but
        // assert the contract instead of assuming the allocator.
        let top = (bottom + STACK_SIZE_BYTES as u64) & !0xF;
        assert!(
            top > bottom,
            "task stack top underflowed bottom — size misconfigured?"
        );
        Self {
            _backing: backing,
            top,
            bottom,
        }
    }
}

impl Default for TaskStack {
    fn default() -> Self {
        Self::new()
    }
}

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

/// A task: a named step function with an id, a ledger, and its own stack.
///
/// The step function runs once per schedule and returns `true` to stay
/// ready or `false` to finish. Step 2a: every task OWNS a [`TaskStack`]
/// (heap-backed, 16-byte-aligned top) — allocated at spawn, freed on drop.
/// The switch path doesn't use it yet (Step 2b's `asm!`), but allocation
/// under heap pressure is proven today: 4 demo tasks × 128 KiB = 512 KiB
/// of the 1 MiB heap, and the boot log prints each stack top.
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
    /// The task's private kernel stack. Unused by the Step-1 schedule path
    /// (the closure runs on main's stack) — Step 2b's context switch will
    /// load `stack.top` into RSP. Kept alive here so the mapping stays owned.
    pub stack: TaskStack,
    /// The work. `FnMut` — tasks may mutate captured state across runs.
    step: alloc::boxed::Box<dyn FnMut() -> bool>,
}

impl Task {
    /// Build a task. Starts [`TaskState::Ready`], zero runs, fresh stack.
    pub fn new(id: usize, name: &str, step: impl FnMut() -> bool + 'static) -> Self {
        let stack = TaskStack::new();
        crate::serial_println!(
            "sched: task {}/{} stack top {:#x} ({} KiB)",
            id,
            name,
            stack.top,
            STACK_SIZE_BYTES / 1024
        );
        Self {
            id,
            name: String::from(name),
            state: TaskState::Ready,
            runs: 0,
            stack,
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
        let stack = TaskStack::new();
        crate::serial_println!(
            "sched: task {}/{} stack top {:#x} ({} KiB)",
            id,
            name,
            stack.top,
            STACK_SIZE_BYTES / 1024
        );
        Self {
            id,
            name: String::from(name),
            state: TaskState::Sleeping { until_tick },
            runs: 0,
            stack,
            step: alloc::boxed::Box::new(step),
        }
    }

    /// Run one step. Returns `true` = still ready, `false` = finished.
    /// Increments the run ledger either way — an attempt is an attempt.
    ///
    /// Step 2a stack check: the closure still runs on main's stack (the
    /// switch lands in 2b), but every step re-validates the owned stack's
    /// invariants — 16-byte-aligned top, top above bottom — so a corrupt or
    /// misconfigured stack screams HERE, not deep inside future `asm!`.
    /// This also keeps `stack`/`bottom` honestly read, never `allow`ed.
    fn run_step(&mut self) -> bool {
        debug_assert_eq!(
            self.stack.top % 16,
            0,
            "task {}/{} stack top misaligned",
            self.id,
            self.name
        );
        debug_assert!(
            self.stack.top > self.stack.bottom,
            "task {}/{} stack top below bottom",
            self.id,
            self.name
        );
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
