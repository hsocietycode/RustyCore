//! Cooperative context switch — Phase 3, Step 2b.
//!
//! The scene so far: the LAPIC ticks at ~100 Hz into `APIC_TICKS`, memory
//! is bump-allocated, interrupts are live, Step 1 proved the scheduler
//! logic, Step 2a gave every task its own stack plus a preemption clock.
//! This step spends the sweaty afternoon with `naked_asm!`: tasks now run
//! ON their own stacks, and the scheduler switches stacks for real.
//!
//! - [`Task`]: id, name, state, step function, run counter, OWN [`TaskStack`]
//!   (heap-backed, 16-byte-aligned top) — plus its OWN [`Context`].
//! - [`context_switch`]: naked-asm save/restore of callee-saved registers
//!   plus RSP. No prologue, no Rust inside — raw text only.
//! - First-run bootstrap: [`init_task_context`] builds a fake frame on the
//!   fresh stack so the first switch `ret`s into [`task_trampoline`], which
//!   runs one step and switches back to the scheduler. Every later switch
//!   resumes the same trampoline loop — no special cases.
//! - [`Scheduler`]: the trait Phase 3 promises (`sched-rr` now, `sched-cfs`
//!   later) — unchanged. The demo tasks never noticed the floor move.
//! - [`RoundRobin`]: a VecDeque of `Box<Task>` — boxed, because a `VecDeque`
//!   reallocates and shuffles addresses, and a saved RSP pointing at a moved
//!   `Task` is a triple fault. The queue holds stable heap addresses only.
//! - Lock discipline: NO lock is ever held across the `asm!` switch. The
//!   queue is popped/pushed with plain `&mut self` (main drives the loop,
//!   no Mutex involved); the switch itself takes only raw pointers.
//! - Preemption clock: [`timer_tick`] (called from the LAPIC handler, IRQ
//!   context, atomics only) raises [`NEED_RESCHED`] every [`QUANTUM_TICKS`]
//!   ticks; the driver observes it via [`take_preempt_flag`].
//! - A boot demo: three tasks + a napper, stacks printed at spawn, the log
//!   shows the interleave AND the preempt points, the ledger proves nobody
//!   starved.
//!
//! Still cooperative (the driver paces on LAPIC ticks, tasks yield by
//! returning) — preemptive switch-from-IRQ is a later step. Still ring 0
//! only: `TSS.rsp0` is updated on every switch as proof of the path, but no
//! hardware reads it yet (ring transitions don't happen at CPL 0).

use alloc::{boxed::Box, collections::VecDeque, string::String, vec::Vec};
use core::arch::naked_asm;

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
/// ≈ 100 ms before the timer asks for a reschedule. Named, not magic — the
/// flag is still observed-only (the driver logs preempt points); ENFORCING
/// the quantum (yanking a running task mid-step) is the preemptive step's
/// job, not this cooperative one's.
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

/// Saved CPU state for one side of a cooperative switch.
///
/// Only RSP — the naked-asm switch pushes/pops the six callee-saved
/// registers (rbp, rbx, r12–r15) ON THE STACKS themselves, so the struct
/// never goes stale. RIP is implicit: RSP points at the return address the
/// `call context_switch` pushed (or the fake one the bootstrap planted),
/// and the switch's final `ret` restores it.
///
/// `repr(C)`, one field — the asm does `mov [rdi], rsp`, offset 0, no math.
#[repr(C)]
pub struct Context {
    /// Stack pointer saved at the switch point. Invariant: `rsp % 16 == 8`
    /// (just after a `call` pushed 8 bytes — see the alignment proof in
    /// [`init_task_context`]).
    pub rsp: u64,
}

impl Context {
    /// Zero slot — never switched TO (rsp=0 would load garbage and jump
    /// nowhere). The scheduler side starts here and is filled by the first
    /// switch away from the boot stack.
    pub const fn empty() -> Self {
        Self { rsp: 0 }
    }
}

/// The scheduler side of every switch: main's boot-stack context, saved on
/// the first switch TO a task and restored when the task yields back.
/// Written only by the driver (interrupts disabled around the switch), read
/// only by the trampoline — single CPU, cooperative, no race.
static mut SCHED_CTX: Context = Context::empty();

/// The task currently executing on its own stack (null while the driver
/// runs on the boot stack). Set by the driver just before switching in,
/// read by the trampoline to find whose step to run. Raw pointer, not a
/// reference — no borrow crosses the `asm!` boundary, ever.
///
/// Lifetime protocol: the pointer is valid only while its `Box<Task>` is
/// alive in the driver's hand. A finished task's Box is dropped at the end
/// of the `schedule_once` iteration, leaving this dangling — but nobody
/// reads it in that window: the trampoline reads it ONLY right after a
/// switch that the driver set up, and every switch sets it fresh first.
/// Single CPU + cooperative = no preemptive reader can slip between.
static mut CURRENT_TASK: *mut Task = core::ptr::null_mut();

/// Naked context switch: save callee-saved + RSP into `*old`, load them
/// from `*new`, `ret` into the new side.
///
/// `rdi = old: *mut Context`, `rsi = new: *const Context` (SysV AMD64 first
/// two args). No prologue, no epilogue, no Rust — one `naked_asm!` block.
/// Six pushes (48 bytes, 48 % 16 == 0) keep the `rsp % 16 == 8` invariant
/// on both sides: save point and restore point are symmetric.
///
/// What is NOT saved: caller-saved regs (rax, rcx, rdx, rsi, rdi, r8–r11),
/// xmm, rflags. The compiler treats `call context_switch` as a normal call —
/// caller-saved are dead by definition. FPU/SSE task state is a preemptive
/// step's problem (space reserved in `Task` later, zeroed today).
///
/// # Safety
/// - Both pointers must be valid, `*old` writable, `*new` readable.
/// - `new.rsp` must point at a live stack with a valid return address on
///   top (real one from a previous switch, or the bootstrap's fake one).
/// - No lock may be held across the call (see module docs — a guard living
///   on a suspended stack is a single-CPU deadlock).
/// - Caller must run with interrupts in a known state (driver disables
///   around the switch; the trampoline re-enables — see [`switch_to_task`]).
#[unsafe(naked)]
pub unsafe extern "C" fn context_switch(old: *mut Context, new: *const Context) {
    naked_asm!(
        // Save: rdi = old. Push callee-saved ONTO the current stack (fewer
        // asm offsets to keep in sync than storing each into the struct),
        // then save the post-push RSP — single store, no stale value.
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi + 0x00], rsp",
        // Load: rsi = new.
        "mov rsp, [rsi + 0x00]",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
    )
}

/// Build the fake first frame on a fresh task stack.
///
/// Layout at `top` (16-byte aligned, stacks grow down), 7 slots = 56 bytes:
/// ```
/// [top-0x38] saved r15 = 0   <-- Context.rsp points here
/// [top-0x30] saved r14 = 0
/// [top-0x28] saved r13 = 0
/// [top-0x20] saved r12 = 0
/// [top-0x18] saved rbx = 0
/// [top-0x10] saved rbp = 0
/// [top-0x08] fake return address -> task_trampoline
/// [top-0x00] (nothing — RSP lands here after the first `ret`)
/// ```
/// The first switch loads RSP = top-0x38, pops six zeros, `ret`s into the
/// trampoline with RSP = top (16-byte aligned — the trampoline's `call`s
/// re-establish the 8-mod inside the callee, as SysV requires).
///
/// Alignment proof: `top % 16 == 0` (TaskStack contract), 56 % 16 == 8, so
/// `(top - 56) % 16 == 8` — exactly the post-`call` invariant the switch's
/// restore path demands.
fn init_task_context(top: u64) -> Context {
    assert_eq!(top % 16, 0, "task stack top must be 16-byte aligned");
    let rsp = top - 56;
    assert_eq!(rsp % 16, 8, "bootstrap math broke the rsp%16==8 invariant");
    unsafe {
        let slots = rsp as *mut u64;
        // Six zeroed saved registers...
        for i in 0..6 {
            slots.add(i).write(0);
        }
        // ...plus the fake return address on top.
        slots.add(6).write(task_trampoline as *const () as u64);
    }
    Context { rsp }
}

/// First-run (and every-run) entry point on a task's own stack.
///
/// Reached by `ret` from [`context_switch`] with RSP = task top. Finds the
/// current task via `CURRENT_TASK`, enables interrupts (the driver disabled
/// them around the switch — IRQ time must never see half-switched stacks),
/// runs ONE step, disables interrupts again, and switches back to the
/// scheduler. When the step returns `false` the task is marked finished —
/// the scheduler drops it, and this trampoline is never entered again for
/// it (its `Context` is discarded with the `Box<Task>`).
///
/// `-> !`: it never returns through the normal path — every exit is a
/// `context_switch` back. If a bug ever falls through, the trailing
/// `hlt`-loop is the backstop, not a return into garbage.
extern "C" fn task_trampoline() -> ! {
    unsafe {
        let task = &mut *CURRENT_TASK;
        // Resume lands HERE (after the yield-switch below), not at function
        // top: the loop re-runs the next step on every re-entry. First entry
        // arrives via the bootstrap's fake `ret`; every later entry arrives
        // via `context_switch` returning into the previous iteration's yield.
        loop {
            // Overflow tripwire FIRST: if the stack grew into its own
            // bottom, the canary is gone — halt before heap corruption.
            if !task.stack.canary_ok() {
                x86_64::instructions::interrupts::disable();
                crate::serial_println!(
                    "sched FAILED: task {}/{} stack overflow (canary trashed)",
                    task.id,
                    task.name
                );
                loop {
                    x86_64::instructions::hlt();
                }
            }
            // IRQs back on: we are on a real task stack now, handlers run.
            x86_64::instructions::interrupts::enable();
            let alive = (task.step)();
            task.runs += 1;
            task.state = if alive {
                TaskState::Ready
            } else {
                TaskState::Finished
            };
            // IRQs off before switching stacks — IRQ time must never see a
            // half-switched RSP.
            x86_64::instructions::interrupts::disable();
            context_switch(&mut task.ctx as *mut Context, &raw const SCHED_CTX);
        }
    }
}

/// Switch from the driver (boot stack) to `task`, running one step.
///
/// Contract: interrupts are disabled by the caller around this call (the
/// trampoline re-enables on entry, disables before switching back). Sets
/// `CURRENT_TASK`, updates `TSS.rsp0` to the task's stack top as proof of
/// the path (no hardware reads it at CPL 0 — ring 3 will), and calls the
/// naked switch. Returns when the task yields back.
unsafe fn switch_to_task(task: *mut Task) {
    unsafe {
        CURRENT_TASK = task;
        // rsp0 proof-of-path: updated on EVERY switch, read by hardware only
        // once ring 3 arrives. Today it is bookkeeping with a purpose — the
        // hook exists, the value is right, the consumer comes later.
        crate::gdt::set_rsp0((*task).stack.top);
        x86_64::instructions::interrupts::disable();
        context_switch(&raw mut SCHED_CTX, &(*task).ctx as *const Context);
        // Back on the boot stack: the task disabled IRQs before switching.
        x86_64::instructions::interrupts::enable();
    }
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

/// Magic planted at the very bottom of every task stack. The trampoline
/// re-reads it on every step: stacks grow DOWN from the top, so the bottom
/// slot is touched only by an overflow. A trashed canary screams BEFORE the
/// corruption spreads silently into the heap — no guard pages yet, so this
/// cheap sentinel is the overflow detector (safety-review mitigation).
pub const STACK_CANARY: u64 = 0xDEAD_BEEF_CAFE_F00D;

impl TaskStack {
    /// Allocate a fresh stack. Panics loudly on OOM — a task without a
    /// stack is not a task, and booting past it would corrupt memory.
    /// Plants the [`STACK_CANARY`] at the bottom slot.
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
        // Plant the canary BEFORE anyone runs: bottom slot is ours (heap
        // gave us the whole region), and nothing legitimate writes there —
        // the bootstrap lives at the top, 128 KiB away.
        unsafe {
            (bottom as *mut u64).write(STACK_CANARY);
        }
        Self {
            _backing: backing,
            top,
            bottom,
        }
    }

    /// Verify the bottom canary is intact. Called by the trampoline on every
    /// step — a mismatch means the stack overflowed downward into the heap.
    fn canary_ok(&self) -> bool {
        unsafe { (self.bottom as *const u64).read() == STACK_CANARY }
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

/// A task: a named step function with an id, a ledger, its own stack, and
/// its own [`Context`].
///
/// The step function runs once per switch and returns `true` to stay ready
/// or `false` to finish. The closure NEVER runs on the driver's stack — the
/// scheduler switches onto the task's stack, the [`task_trampoline`] runs
/// one step there, and switches back. `runs`/`state` are updated by the
/// trampoline, not by the driver.
pub struct Task {
    /// Stable number, handed out by the scheduler. Log lines carry it so
    /// the interleave is readable (`task 1/alpha`, not anonymous noise).
    pub id: usize,
    /// Human name for the log. Short — serial is slow.
    pub name: String,
    /// Current lifecycle state. Set by the trampoline after each step;
    /// read by the scheduler after the switch returns.
    pub state: TaskState,
    /// How many times this task has run. Starvation detector: at the end
    /// of the demo every task's count must be equal (round-robin promise).
    pub runs: u64,
    /// The task's private kernel stack. Owns the mapping — dropping the
    /// task frees its stack. The switch path runs ON this stack.
    pub stack: TaskStack,
    /// Saved switch state, built by [`init_task_context`] at spawn. The
    /// driver switches TO this; the trampoline switches AWAY from it.
    pub ctx: Context,
    /// The work. `FnMut` — tasks may mutate captured state across runs.
    /// Called ONLY by the trampoline, on the task's own stack.
    step: Box<dyn FnMut() -> bool>,
}

impl Task {
    /// Build a task. Starts [`TaskState::Ready`], zero runs, fresh stack +
    /// bootstrapped context (first switch `ret`s into the trampoline).
    pub fn new(id: usize, name: &str, step: impl FnMut() -> bool + 'static) -> Self {
        let stack = TaskStack::new();
        let ctx = init_task_context(stack.top);
        crate::serial_println!(
            "sched: task {}/{} stack top {:#x} ({} KiB) ctx.rsp={:#x}",
            id,
            name,
            stack.top,
            STACK_SIZE_BYTES / 1024,
            ctx.rsp,
        );
        Self {
            id,
            name: String::from(name),
            state: TaskState::Ready,
            runs: 0,
            stack,
            ctx,
            step: Box::new(step),
        }
    }

    /// Build a task parked asleep until `until_tick` (APIC_TICKS domain).
    /// The scheduler carries it through the queue untouched until the tick
    /// arrives — no busy-wait, the caller hlt-sleeps between passes.
    /// Stack + context are bootstrapped at spawn like any other task.
    pub fn sleeping(
        id: usize,
        name: &str,
        until_tick: u64,
        step: impl FnMut() -> bool + 'static,
    ) -> Self {
        let stack = TaskStack::new();
        let ctx = init_task_context(stack.top);
        crate::serial_println!(
            "sched: task {}/{} stack top {:#x} ({} KiB) ctx.rsp={:#x}",
            id,
            name,
            stack.top,
            STACK_SIZE_BYTES / 1024,
            ctx.rsp,
        );
        Self {
            id,
            name: String::from(name),
            state: TaskState::Sleeping { until_tick },
            runs: 0,
            stack,
            ctx,
            step: Box::new(step),
        }
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
    /// Boxed tasks: a `VecDeque` reallocates and shuffles addresses on
    /// push/pop — a saved `Context.rsp` pointing at a MOVED `Task` would
    /// resume into garbage and triple-fault. The queue holds stable heap
    /// addresses; the `Task` body (with its `ctx` + stack) never moves.
    ready: VecDeque<Box<Task>>,
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
        self.ready.push_back(Box::new(task));
    }

    fn schedule_once(&mut self) -> bool {
        // F1 fix: the old code pushed a still-sleeping task back and
        // returned `true` — with ALL tasks asleep, the caller spun forever
        // (`alive() > 0` forever, `schedule_once` never false). Now: one
        // full pass over the queue; if nobody was awake, return `false`
        // so the caller hlt-waits for the next tick instead of burning CPU.
        //
        // Step 2b: the step runs ON THE TASK'S STACK. Pop the Box (stable
        // heap address — the trampoline's CURRENT_TASK pointer stays valid),
        // switch in, and read back the state the trampoline set. No lock
        // exists here at all (plain `&mut self` from the driver) — nothing
        // CAN be held across the `asm!`, by construction.
        let len = self.ready.len();
        if len == 0 {
            return false;
        }
        for _ in 0..len {
            let mut task: Box<Task> = match self.ready.pop_front() {
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
            task.state = TaskState::Running;
            unsafe {
                switch_to_task(&mut *task as *mut Task);
            }
            // Back on the boot stack. The trampoline ran exactly one step on
            // the task's stack and set state+runs — read the verdict.
            if task.state == TaskState::Ready {
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
