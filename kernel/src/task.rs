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
//! - Preemption: the LAPIC 0xEF vector is a `#[unsafe(naked)]` stub
//!   ([`crate::preempt::lapic_preempt_stub`]) that yanks a running task
//!   mid-step, saves its full frame into a static table slot, and resumes it
//!   exactly where the timer hit. [`PREEMPT_SWITCHES`](crate::preempt::PREEMPT_SWITCHES)
//!   counts real yanks — the driver reads the delta and logs each one.
//! - A boot demo: three tasks + a napper, stacks printed at spawn, the log
//!   shows the interleave AND the real yanks, the ledger proves nobody
//!   starved.
//!
//! Still ring 0 only: `TSS.rsp0` is updated on every switch as proof of the
//! path, but no hardware reads it yet (ring transitions don't happen at
//! CPL 0).

use alloc::{boxed::Box, collections::VecDeque, string::String, vec::Vec};
use core::arch::naked_asm;
use x86_64::instructions::segmentation::Segment as _;

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

// The cooperative preempt-clock (QUANTUM_TICKS / NEED_RESCHED /
// PREEMPT_TICKS / timer_tick / take_preempt_flag) is GONE — Step 4's naked
// stub IS the preemption now. The stub bumps APIC_TICKS (the driver's
// pacemaker) and PREEMPT_SWITCHES (yank count); the driver reads those
// directly instead of polling a cooperative quantum flag.

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
/// Layout at `top` (16-byte aligned, stacks grow down), 7 slots + 1 pad
/// = 64 bytes:
/// ```
/// [top-0x40] saved r15 = 0   <-- Context.rsp points here
/// [top-0x38] saved r14 = 0
/// [top-0x30] saved r13 = 0
/// [top-0x28] saved r12 = 0
/// [top-0x20] saved rbx = 0
/// [top-0x18] saved rbp = 0
/// [top-0x10] fake return address -> task_trampoline
/// [top-0x08] PAD — never read; exists purely for ABI parity
/// [top-0x00] (RSP lands here after the first `ret`)
/// ```
/// The first switch loads RSP = top-0x40, pops six zeros, `ret`s into the
/// trampoline with RSP = top-0x08.
///
/// Alignment proof (the reason for the 8-byte PAD): the resumed rsp is
/// `saved + 64` (48 bytes of pops + 8 of `ret`). ABI requires it to be
/// `8 (mod 16)` — i.e. exactly the state a callee sees right after `call`
/// pushed its return address. `top` is 16-byte aligned, so the START of the
/// frame must be `0 (mod 16)` and the frame must be 64 bytes: `saved + 64`
/// = `top - 64 + 64` = `top - 8` → `8 (mod 16)`. The old 56-byte frame
/// resumed at `top` → `0 (mod 16)`: a silent ABI violation (caught on QEMU
/// by the entry assert in [`task_trampoline_body`]) that would misalign
/// every 16-byte SSE spill in the task's call chain.
fn init_task_context(top: u64) -> Context {
    assert_eq!(top % 16, 0, "task stack top must be 16-byte aligned");
    let rsp = top - 64;
    assert_eq!(
        rsp % 16,
        0,
        "bootstrap frame must start 16-aligned to resume at 8 (mod 16)"
    );
    assert_eq!((rsp + 56) % 16, 8, "resumed rsp would break the ABI parity");
    unsafe {
        let slots = rsp as *mut u64;
        // Six zeroed saved registers...
        for i in 0..6 {
            slots.add(i).write(0);
        }
        // ...plus the fake return address (pad slot stays untouched).
        slots.add(6).write(task_trampoline as *const () as u64);
        slots.add(7).write(0);
    }
    Context { rsp }
}

/// First-run (and every-run) entry point on a task's own stack.
///
/// Reached by `ret` from [`context_switch`] with RSP = task top. Finds the
/// current task via its slot, enables interrupts (the driver disabled
/// them around the switch — IRQ time must never see half-switched stacks),
/// runs ONE step, disables interrupts again, and switches back to the
/// scheduler. When the step returns `false` the task is marked finished —
/// the scheduler drops it, and this trampoline is never entered again for
/// it (its `Context` is discarded with the `Box<Task>`).
///
/// `-> !`: it never returns through the normal path — every exit is a
/// `context_switch` back. If a bug ever falls through, the trailing
/// `hlt`-loop is the backstop, not a return into garbage.
/// RSP captured the moment the first switch lands in [`task_trampoline`].
/// The ABI demands `rsp % 16 == 8` at a function's entry point; a bootstrap
/// frame that resumes a task at the wrong parity corrupts EVERY frame in the
/// task's call chain (any 16-byte-aligned SSE spill becomes misaligned), so
/// the invariant is captured here and asserted instead of argued about.
static mut TRAMPOLINE_ENTRY_RSP: u64 = 0;

#[unsafe(naked)]
extern "C" fn task_trampoline() -> ! {
    naked_asm!(
        "mov [rip + {slot}], rsp",
        "jmp {body}",
        slot = sym TRAMPOLINE_ENTRY_RSP,
        body = sym task_trampoline_body,
    )
}

/// Slot → task pointer. The trampoline resolves "which task owns the stack I
/// am standing on" from the SLOT the stub published, never from a cached
/// pointer: after an IRQ-driven cross-task switch, any pointer captured
/// before the switch describes the task that was yanked OUT, not the one now
/// running.
///
/// Lifetime: the driver owns each `Box<Task>` while it is queued, and the box
/// POINTER (`*mut Task`) stays valid across `VecDeque` moves — only the Box
/// handle moves, never the `Task` it owns. `unregister` clears the entry in
/// the same breath as the slot bit, so a retired task can never be re-found.
static mut TASK_PTRS: [*mut Task; crate::preempt::MAX_TASKS] =
    [core::ptr::null_mut(); crate::preempt::MAX_TASKS];

/// Find the live task occupying slot `idx`.
pub fn task_at(idx: usize) -> Option<&'static mut Task> {
    if idx >= crate::preempt::MAX_TASKS {
        return None;
    }
    // SAFETY: single CPU. The pointer is written by the driver with IF=0 and
    // read here while the fence is down (or by the trampoline, which is the
    // task itself). `unregister` clears it before the Box is dropped, so a
    // live entry always points at a live `Task`.
    let p = unsafe { TASK_PTRS[idx] };
    if p.is_null() {
        None
    } else {
        Some(unsafe { &mut *p })
    }
}

/// Record which task occupies a slot (called at spawn, before any switch).
fn set_task_ptr(idx: usize, task: *mut Task) {
    assert!(
        idx < crate::preempt::MAX_TASKS,
        "slot overflow in TASK_PTRS"
    );
    unsafe {
        TASK_PTRS[idx] = task;
    }
}

/// Drop the slot's task pointer (called when the task finishes, before the
/// `Box` is dropped — the order matters: pointer first, memory second).
fn clear_task_ptr(idx: usize) {
    if idx < crate::preempt::MAX_TASKS {
        unsafe {
            TASK_PTRS[idx] = core::ptr::null_mut();
        }
    }
}

/// Look up the running task by SLOT rather than by a cached pointer.
///
/// Slot identity is the whole point: the preempt stub can resume a task this
/// code never set up, and a pointer cached before an IRQ-driven switch would
/// then describe the WRONG task (or a dangling `Box`). The table is the
/// single source of truth for "which task owns the stack I am standing on",
/// so every trampoline iteration re-resolves it. Panics if the slot is not
/// registered — that means the stack we are running on has no owner, which is
/// a bug worth stopping for, not papering over.
fn current_task() -> &'static mut Task {
    let idx = crate::preempt::current_idx();
    task_at(idx).unwrap_or_else(|| panic!("trampoline on stack of unregistered slot {idx}"))
}

/// The real trampoline body (see [`task_trampoline`] for the entry shim).
///
/// Entered by `jmp` with the same RSP the shim captured — so this function's
/// own prologue sees exactly the state the ABI promises. Asserts that parity
/// on the first entry: the bootstrap frame is the only place in the kernel
/// that fabricates a function-entry state out of thin air, and getting it
/// wrong is silent until some unrelated task declares an `f64`.
extern "C" fn task_trampoline_body() -> ! {
    let entry_rsp = unsafe { core::ptr::read_volatile(&raw const TRAMPOLINE_ENTRY_RSP) };
    assert_eq!(
        entry_rsp % 16,
        8,
        "ABI violation: task entered with rsp%16 == {} (must be 8) — \
         the bootstrap frame's alignment math is wrong",
        entry_rsp % 16
    );
    // Resume lands HERE (after the yield-switch below), not at function
    // top: the loop re-runs the next step on every re-entry. First entry
    // arrives via the bootstrap's fake `ret`; every later entry arrives
    // via `context_switch` returning into the previous iteration's yield.
    loop {
        let task = current_task();
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
        // Fence UP: from here until the yield `cli` below, the CPU is on
        // a task stack with IF=1 — the preempt stub may yank us mid-step.
        // (Set AFTER sti: a tick in the 2-instruction window sees
        // fence=false and defers one quantum — correct, not lost.)
        x86_64::instructions::interrupts::enable();
        crate::preempt::set_preemptible(true);
        let alive = (task.step)();
        task.runs += 1;
        task.state = if alive {
            TaskState::Ready
        } else {
            TaskState::Finished
        };
        // Fence DOWN first, then IRQs off: from here the stack is
        // half-switch territory — IRQ time must not touch it.
        crate::preempt::set_preemptible(false);
        // IRQs off before switching stacks — IRQ time must never see a
        // half-switched RSP.
        x86_64::instructions::interrupts::disable();
        unsafe {
            context_switch(&mut task.ctx as *mut Context, &raw const SCHED_CTX);
        }
    }
}

/// Switch from the driver (boot stack) to `task`, running one step.
///
/// Contract: interrupts are disabled by the caller around this call (the
/// trampoline re-enables on entry, disables before switching back). Sets
/// the task's SLOT (the trampoline and the preempt stub both resolve their
/// task through it — never through a cached pointer, which an IRQ-driven
/// switch would invalidate), updates `TSS.rsp0` to the task's stack top as
/// proof of the path (no hardware reads it at CPL 0 — ring 3 will), and calls
/// the naked switch. Returns when the task yields back.
unsafe fn switch_to_task(task: *mut Task) {
    unsafe {
        // Publish the preempt-table index so the LAPIC stub knows which slot
        // to yank THIS task's frame into, and so the trampoline can resolve
        // its own `Task` from the stack it was resumed onto. Set BEFORE sti
        // (the trampoline re-sti's) — the stub reads CURRENT_IDX only while
        // the fence is up.
        crate::preempt::set_current((*task).slot as usize);
        // rsp0 proof-of-path: updated on EVERY switch, read by hardware only
        // once ring 3 arrives. Today it is bookkeeping with a purpose — the
        // hook exists, the value is right, the consumer comes later.
        crate::gdt::set_rsp0((*task).stack.top);
        x86_64::instructions::interrupts::disable();
        context_switch(&raw mut SCHED_CTX, &(*task).ctx as *const Context);
        // Back on the boot stack: the task disabled IRQs before switching.
        // Clear the index — the stub must NOT yank into the driver.
        crate::preempt::set_current(crate::preempt::MAX_TASKS);
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

/// A task: a named step function with an id, a ledger, its own stack, its
/// own cooperative [`Context`], and its own preempt [`FullFrame`](crate::preempt::FullFrame).
///
/// The step function runs once per switch and returns `true` to stay ready
/// or `false` to finish. The closure NEVER runs on the driver's stack — the
/// scheduler switches onto the task's stack, the [`task_trampoline`] runs
/// one step there, and switches back. `runs`/`state` are updated by the
/// trampoline, not by the driver.
///
/// Two saved states, NEVER shared: `ctx` (cooperative, ret-based — the yield
/// path) and `preempt` (preemptive, iret-based — the yank path). A `rsp`
/// saved by one path is garbage to the other (6 pushed regs + `ret` vs 15
/// spilled regs + hw frame + `iretq`), so they live in separate slots. The
/// cooperative path touches only `ctx`; the preempt stub touches only
/// `preempt`. Fresh tasks synthesize `preempt` at spawn (`FullFrame::fresh`
/// into the trampoline) so the FIRST resume already goes through `iretq` —
/// one resume path, zero special cases in the stub.
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
    /// Virtual runtime (CFS domain): nanoseconds-free tick units, scaled by
    /// weight — a task accrues `CFS_BASE_SLICE * 1024 / weight` per step.
    /// Equal-weight tasks accrue equally (CFS degenerates to fair RR); a
    /// double-weight task accrues half as fast and is picked twice as often.
    /// Compiled out under `sched-rr` (RR never reads it — no dead fields).
    #[cfg(feature = "sched-cfs")]
    pub vruntime: u64,
    /// Scheduling weight (CFS domain). 1024 = nice-0 default; higher means
    /// MORE cpu (vruntime accrues slower). Must be > 0 — zero would divide
    /// by zero in the vruntime update (guarded by assert at spawn).
    /// Compiled out under `sched-rr`.
    #[cfg(feature = "sched-cfs")]
    pub weight: u32,
    /// The task's private kernel stack. Owns the mapping — dropping the
    /// task frees its stack. The switch path runs ON this stack.
    pub stack: TaskStack,
    /// Saved switch state, built by [`init_task_context`] at spawn. The
    /// driver switches TO this; the trampoline switches AWAY from it.
    /// Cooperative path ONLY — the preempt stub never touches this (it has
    /// its own [`preempt`](crate::preempt::FullFrame) slot below).
    pub ctx: Context,
    /// Preempt slot: the yank-mid-step state. Synthesized at spawn
    /// (`FullFrame::fresh` into the trampoline — the first resume already
    /// goes through `iretq`), overwritten by the stub on every yank with
    /// the EXACT interrupted state (15 GPRs + hw rip/cs/rflags + pre-IRQ
    /// rsp + magic). Validated by `valid_for(stack.bottom, stack.top)`
    /// before every `iretq` — a bad frame halts loudly, never jumps.
    /// Cooperative `ctx` above and this slot NEVER share: 6-reg `ret`
    /// layout vs 15-reg + hw `iretq` layout.
    pub preempt: crate::preempt::FullFrame,
    /// Preempt table slot index ([`crate::preempt::TASK_FRAMES`]), assigned
    /// at spawn. `u8::MAX` = unregistered (never scheduled — spawn registers
    /// before that). Published to `CURRENT_IDX` by [`switch_to_task`] so the
    /// stub knows which slot to yank into.
    pub slot: u8,
    /// The work. `FnMut` — tasks may mutate captured state across runs.
    /// Called ONLY by the trampoline, on the task's own stack.
    step: Box<dyn FnMut() -> bool>,
}

/// Default CFS weight (nice-0 equivalent): every task is equal until the
/// caller says otherwise via [`Task::with_weight`]. CFS-only (RR has no
/// weights — the field doesn't exist there).
#[cfg(feature = "sched-cfs")]
pub const CFS_DEFAULT_WEIGHT: u32 = 1024;

/// CFS accounting slice per step in vruntime units. One step accrues
/// `CFS_BASE_SLICE * 1024 / weight` — a default-weight task accrues exactly
/// one slice per step; weights scale it inversely. Named, not magic.
/// CFS-only.
#[cfg(feature = "sched-cfs")]
pub const CFS_BASE_SLICE: u64 = 100;

/// Allocate a free preempt-table slot from the occupancy bitmap. Lazy-first-
/// free, NOT a monotone counter: a monotone counter never reuses slots, so a
/// long-lived kernel that spawns and reaps tasks would exhaust the table
/// while slots sat free (the bitmap in `preempt` is already the source of
/// truth for occupancy, so the allocator asks it directly).
/// Panics when [`crate::preempt::MAX_TASKS`] is genuinely full — a refused
/// spawn must be loud, never a silent overwrite of a live slot.
fn alloc_slot() -> u8 {
    use crate::preempt::{MAX_TASKS, SLOT_USED};
    let used = SLOT_USED.load(core::sync::atomic::Ordering::Relaxed);
    for idx in 0..MAX_TASKS {
        if used & (1 << idx) == 0 {
            return idx as u8;
        }
    }
    panic!("preempt: task table full ({MAX_TASKS} slots) — reap finished tasks or raise MAX_TASKS");
}

impl Task {
    /// Build a task. Starts [`TaskState::Ready`], zero runs, fresh stack +
    /// bootstrapped context (first switch `ret`s into the trampoline) +
    /// synthesized preempt frame (first `iretq` lands in the trampoline —
    /// one resume path, validated by `valid_for`). Registers the frame in
    /// the static preempt table at a fresh slot (the stub yanks by index).
    /// Default CFS weight ([`CFS_DEFAULT_WEIGHT`]); override with
    /// [`Task::with_weight`] before spawn.
    pub fn new(id: usize, name: &str, step: impl FnMut() -> bool + 'static) -> Self {
        let stack = TaskStack::new();
        let ctx = init_task_context(stack.top);
        // Fresh preempt frame: rip = trampoline, cs = current code segment,
        // rsp = stack top. First yank-resume goes through iretq like any other.
        let cs = x86_64::instructions::segmentation::CS::get_reg().0;
        let preempt =
            crate::preempt::FullFrame::fresh(task_trampoline as *const () as u64, cs, stack.top);
        assert!(
            preempt.valid_for(stack.bottom, stack.top),
            "fresh preempt frame failed its own validation"
        );
        let slot = alloc_slot();
        crate::preempt::register_task(slot as usize, preempt);
        crate::preempt::set_slot_bounds(slot as usize, stack.bottom, stack.top);
        crate::serial_println!(
            "sched: task {}/{} stack top {:#x} ({} KiB) ctx.rsp={:#x} preempt rip={:#x} slot={}",
            id,
            name,
            stack.top,
            STACK_SIZE_BYTES / 1024,
            ctx.rsp,
            preempt.rip,
            slot,
        );
        Self {
            id,
            name: String::from(name),
            state: TaskState::Ready,
            runs: 0,
            #[cfg(feature = "sched-cfs")]
            vruntime: 0,
            #[cfg(feature = "sched-cfs")]
            weight: CFS_DEFAULT_WEIGHT,
            stack,
            ctx,
            preempt,
            slot,
            step: Box::new(step),
        }
    }

    /// Override the CFS weight before spawn. Consumes + returns Self so the
    /// call chains at the spawn site (`Task::new(...).with_weight(...)`).
    /// Panics on zero — a zero weight would divide by zero in vruntime math.
    /// CFS-only (the field doesn't exist under RR).
    #[cfg(feature = "sched-cfs")]
    pub fn with_weight(mut self, weight: u32) -> Self {
        assert!(weight > 0, "task weight must be > 0");
        self.weight = weight;
        self
    }

    /// Build a task parked asleep until `until_tick` (APIC_TICKS domain).
    /// The scheduler carries it through the queue untouched until the tick
    /// arrives — no busy-wait, the caller hlt-sleeps between passes.
    /// Stack + context + preempt frame are bootstrapped at spawn like any
    /// other task.
    pub fn sleeping(
        id: usize,
        name: &str,
        until_tick: u64,
        step: impl FnMut() -> bool + 'static,
    ) -> Self {
        let stack = TaskStack::new();
        let ctx = init_task_context(stack.top);
        let cs = x86_64::instructions::segmentation::CS::get_reg().0;
        let preempt =
            crate::preempt::FullFrame::fresh(task_trampoline as *const () as u64, cs, stack.top);
        assert!(
            preempt.valid_for(stack.bottom, stack.top),
            "fresh preempt frame failed its own validation"
        );
        let slot = alloc_slot();
        crate::preempt::register_task(slot as usize, preempt);
        crate::preempt::set_slot_bounds(slot as usize, stack.bottom, stack.top);
        crate::serial_println!(
            "sched: task {}/{} stack top {:#x} ({} KiB) ctx.rsp={:#x} preempt rip={:#x} slot={}",
            id,
            name,
            stack.top,
            STACK_SIZE_BYTES / 1024,
            ctx.rsp,
            preempt.rip,
            slot,
        );
        Self {
            id,
            name: String::from(name),
            state: TaskState::Sleeping { until_tick },
            runs: 0,
            #[cfg(feature = "sched-cfs")]
            vruntime: 0,
            #[cfg(feature = "sched-cfs")]
            weight: CFS_DEFAULT_WEIGHT,
            stack,
            ctx,
            preempt,
            slot,
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

    /// Hand out the next task id (1, 2, 3...). Lives on the trait (not just
    /// the concrete type) so the driver can number tasks through
    /// `Box<dyn Scheduler>` without knowing which policy is compiled in.
    fn next_id(&mut self) -> usize;

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
/// Feature gate (same style as `memory::init`): exactly one scheduler
/// policy must be selected — both or neither refuses to build instead of
/// silently booting the wrong one.
#[cfg(all(feature = "sched-rr", feature = "sched-cfs"))]
compile_error!("select exactly one of sched-rr / sched-cfs, not both");
#[cfg(not(any(feature = "sched-rr", feature = "sched-cfs")))]
compile_error!("select one of sched-rr / sched-cfs");
#[cfg(feature = "sched-rr")]
pub struct RoundRobin {
    /// Boxed tasks: a `VecDeque` reallocates and shuffles addresses on
    /// push/pop — a saved `Context.rsp` pointing at a MOVED `Task` would
    /// resume into garbage and triple-fault. The queue holds stable heap
    /// addresses; the `Task` body (with its `ctx` + stack) never moves.
    ready: VecDeque<Box<Task>>,
    finished_runs: Vec<(usize, String, u64)>,
    next_id: usize,
}

#[cfg(feature = "sched-rr")]
impl RoundRobin {
    /// Empty scheduler. Tasks arrive via [`Scheduler::spawn`].
    pub fn new() -> Self {
        Self {
            ready: VecDeque::new(),
            finished_runs: Vec::new(),
            next_id: 1,
        }
    }
}

#[cfg(feature = "sched-rr")]
impl Default for RoundRobin {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "sched-rr")]
impl Scheduler for RoundRobin {
    fn spawn(&mut self, task: Task) {
        crate::serial_println!("sched: spawned task {}/{}", task.id, task.name);
        let slot = task.slot as usize;
        let boxed = Box::new(task);
        // Register the STABLE box address now — only the Box handle moves
        // when the queue reallocates, never the `Task` it owns, so the
        // slot→task pointer stays valid for the task's whole life.
        set_task_ptr(slot, &*boxed as *const Task as *mut Task);
        self.ready.push_back(boxed);
    }

    fn next_id(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn schedule_once(&mut self) -> bool {
        // F1 fix: the old code pushed a still-sleeping task back and
        // returned `true` — with ALL tasks asleep, the caller spun forever
        // (`alive() > 0` forever, `schedule_once` never false). Now: one
        // full pass over the queue; if nobody was awake, return `false`
        // so the caller hlt-waits for the next tick instead of burning CPU.
        //
        // Step 2b: the step runs ON THE TASK'S STACK. Pop the Box (stable
        // heap address — the slot→task table entry stays valid),
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
            // the task's stack and set state+runs — read the verdict. Step 4
            // proof: the preempt slot must still validate (magic intact, rsp
            // in-window) — the cooperative path never touches it, so a broken
            // frame here means someone scribbled across slots.
            assert!(
                task.preempt.valid_for(task.stack.bottom, task.stack.top),
                "task {}/{} fresh preempt template trashed",
                task.id,
                task.name,
            );
            // Validate the IRQ-stashed frame THROUGH THE TABLE (bounds
            // recorded at spawn), not through the task's own fields: the
            // table is what the stub reads, so checking the task instead
            // would validate a different fact than the one that matters.
            assert!(
                crate::preempt::frame_ok(task.slot as usize),
                "task {}/{} IRQ-stashed frame invalid or unregistered",
                task.id,
                task.name,
            );
            if task.state == TaskState::Ready {
                self.ready.push_back(task);
            } else {
                crate::serial_println!(
                    "sched: task {}/{} finished after {} runs",
                    task.id,
                    task.name,
                    task.runs
                );
                // Release the preempt-table slot NOW: the pointer goes first
                // (so the trampoline can no longer find this task), then the
                // frame is wiped to its invalid template. A stray yank or a
                // later snapshot can never read a rip into the freed stack.
                clear_task_ptr(task.slot as usize);
                crate::preempt::unregister_task(task.slot as usize);
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

/// Completely Fair Scheduler: always run the lowest-vruntime task.
///
/// Same [`Scheduler`] trait as [`RoundRobin`], same switch machinery
/// (tasks are `Box<Task>`, steps run on own stacks, no lock across `asm!`).
/// The ONLY difference is pick-next: RR pops the head, CFS scans for the
/// minimum vruntime. After each step the task accrues
/// `CFS_BASE_SLICE * 1024 / weight` — equal weights accrue equally (CFS
/// degenerates to fair round-robin), heavier weights accrue slower and get
/// picked more often. Sleepers keep their vruntime while parked (no accrual
/// without running — a sleeper must not gain unfair advantage), and a woken
/// task is clamped to at most the current minimum (a long nap must not bank
/// an unbeatable deficit — classic CFS sleeper-fairness).
///
/// O(n) scan today (four demo tasks — a linear pass is honest, not lazy).
/// A real rbtree lands when the task count earns it.
#[cfg(feature = "sched-cfs")]
pub struct Cfs {
    ready: VecDeque<Box<Task>>,
    finished_runs: Vec<(usize, String, u64)>,
    next_id: usize,
}

#[cfg(feature = "sched-cfs")]
impl Cfs {
    /// Empty scheduler. Tasks arrive via [`Scheduler::spawn`].
    pub fn new() -> Self {
        Self {
            ready: VecDeque::new(),
            finished_runs: Vec::new(),
            next_id: 1,
        }
    }

    /// Index of the awake task with the smallest vruntime. `None` when the
    /// queue is empty or every task is still sleeping.
    fn pick_min(&self, now: u64) -> Option<usize> {
        let mut best: Option<(usize, u64)> = None;
        for (i, t) in self.ready.iter().enumerate() {
            // Sleepers are invisible to pick-next until their tick arrives.
            if let TaskState::Sleeping { until_tick } = t.state {
                if now < until_tick {
                    continue;
                }
            }
            match best {
                None => best = Some((i, t.vruntime)),
                Some((_, v)) if t.vruntime < v => best = Some((i, t.vruntime)),
                _ => {}
            }
        }
        best.map(|(i, _)| i)
    }

    /// Smallest vruntime among QUEUED tasks (sleepers included — they hold
    /// their value while parked). Used to clamp a woken sleeper so a long
    /// nap never banks an unbeatable deficit.
    fn min_vruntime(&self) -> u64 {
        self.ready.iter().map(|t| t.vruntime).min().unwrap_or(0)
    }
}

#[cfg(feature = "sched-cfs")]
impl Default for Cfs {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "sched-cfs")]
impl Scheduler for Cfs {
    fn spawn(&mut self, task: Task) {
        assert!(task.weight > 0, "cfs: refusing zero-weight task");
        crate::serial_println!(
            "sched: spawned task {}/{} (weight {})",
            task.id,
            task.name,
            task.weight
        );
        let slot = task.slot as usize;
        let boxed = Box::new(task);
        set_task_ptr(slot, &*boxed as *const Task as *mut Task);
        self.ready.push_back(boxed);
    }

    fn next_id(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn schedule_once(&mut self) -> bool {
        use core::sync::atomic::Ordering;
        let now = crate::idt::APIC_TICKS.load(Ordering::Relaxed);
        let idx = match self.pick_min(now) {
            Some(i) => i,
            None => return false, // empty, or all sleeping — caller hlt-waits
        };
        let mut task = self.ready.remove(idx).expect("cfs pick_min lied");
        // A woken sleeper rejoins at worst at the current minimum — never
        // ahead of everyone by a banked nap.
        if let TaskState::Sleeping { .. } = task.state {
            let floor = self.min_vruntime();
            if task.vruntime < floor {
                task.vruntime = floor;
            }
            task.state = TaskState::Ready;
        }
        task.state = TaskState::Running;
        unsafe {
            switch_to_task(&mut *task as *mut Task);
        }
        // Step 4 proof (same as RR): the cooperative path must not scribble
        // the preempt slot — a trashed frame here means crossed wires.
        assert!(
            task.preempt.valid_for(task.stack.bottom, task.stack.top),
            "task {}/{} fresh preempt template trashed",
            task.id,
            task.name,
        );
        // Same table-side validation as RR (see the comment there).
        assert!(
            crate::preempt::frame_ok(task.slot as usize),
            "task {}/{} IRQ-stashed frame invalid or unregistered",
            task.id,
            task.name,
        );
        // Back on the boot stack: accrue vruntime for the step just run.
        // Saturating + divide-guarded: weight > 0 by spawn assert, but
        // belt and suspenders — a zero here would be a div-by-zero in IRQ
        // context's aftermath, and kernels don't do aftermaths.
        let w = task.weight.max(1) as u64;
        task.vruntime = task
            .vruntime
            .saturating_add(CFS_BASE_SLICE.saturating_mul(1024) / w);
        if task.state == TaskState::Ready {
            let v = task.vruntime;
            crate::serial_println!("sched: task {}/{} vruntime={}", task.id, task.name, v);
            self.ready.push_back(task);
        } else {
            crate::serial_println!(
                "sched: task {}/{} finished after {} runs (vruntime={})",
                task.id,
                task.name,
                task.runs,
                task.vruntime
            );
            // Same slot release as RR: pointer first, then the frame —
            // both before the Box (and with it the stack) is dropped, so no
            // stale rip and no dangling task pointer survive.
            clear_task_ptr(task.slot as usize);
            crate::preempt::unregister_task(task.slot as usize);
            self.finished_runs.push((task.id, task.name, task.runs));
        }
        true
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
