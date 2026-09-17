//! Preemptive switch-from-IRQ — Phase 3, Step 4.
//!
//! The cooperative switch (`task::context_switch`) only yields at step
//! boundaries: a task that never returns keeps the CPU forever. This module
//! lets the LAPIC timer yank a running task MID-step and resume another one
//! exactly where IT was yanked — real preemption, not a timer hint.
//!
//! Shape (the architects' hybrid A+C, after the adversarial pass):
//! - The 0xEF IDT entry is NOT an `x86-interrupt` fn — the compiler's matched
//!   `iretq` epilogue would fight a stack switch by construction. It is a
//!   `#[unsafe(naked)]` stub installed via `set_handler_addr`, interrupt gate
//!   (IF=0 through the whole stub), DPL 0, IST 0 (stay on the task stack).
//! - Ring-0 IRQ pushes only THREE words (rip/cs/rflags — no rsp/ss without a
//!   privilege change, no error code). The stub spills all 15 GPRs and stashes
//!   the exact frame into a static slot (`TASK_FRAMES`), then restores and
//!   `iretq`s — the yanked task resumes precisely where the timer hit it.
//! - NO `call` inside the stub: SysV alignment at IRQ entry is arbitrary
//!   (mid-step compiler state minus 24 bytes of HW push), so a `call` would
//!   need an explicit `and rsp,-16` dance with fixup math that forgets bytes.
//!   The stub is pure asm end-to-end; the Rust side reads the static slots
//!   through [`snapshot`] / [`frame_ok`] with the fence down.
//! - Cross-task resume (skip the cooperative driver entirely) is the NEXT
//!   increment (Step 5b, see `docs/preemption.md`): a ring-0 `iretq` does NOT
//!   restore RSP, so resuming a DIFFERENT task's frame needs an explicit
//!   `mov rsp, [frame.rsp]` over that task's own stack. TODAY the stub proves
//!   save/restore on the SAME task — every offset, the table indexing, and the
//!   register round-trip are exercised for real, and Step 5a's slot identity
//!   (`TASK_PTRS` + `SLOT_BOUNDS`) already records everything that switch arm
//!   will need to validate a foreign frame.
//! - `PREEMPTIBLE` fence: `1` only inside the trampoline's step window
//!   (after `sti`, before the yield `cli`). The stub checks it first — if the
//!   CPU is on the boot stack, in the driver, or mid-switch, nothing is
//!   stashed: ticks counted, EOI sent, `iretq` back to the SAME stack.
//!   Driver-vs-IRQ ownership race removed by construction.
//! - Cooperative `ctx` (ret-based) and preempt frame (iret-based) are SEPARATE
//!   slots: a `rsp` saved by one path is garbage to the other, so they never
//!   share. Fresh tasks get a SYNTHESIZED hw frame (rip=trampoline,
//!   cs=kernel CS, rflags=0x202) for the eventual cross-task `iretq`.
//! - Zero printing, zero blocking-lock, zero allocation in the stub.
//!   `APIC_TICKS` is bumped with `lock inc` — the driver's `hlt` loop waits on
//!   it, so a silent stub would stall boot.

use core::arch::naked_asm;
use core::sync::atomic::{AtomicU8, Ordering};

/// Magic at the base of every preempt frame. Checked before `iretq` — a
/// wrong RSP + `iretq` pops garbage RIP and triple-faults with no log, so
/// the magic turns "silent death" into "loud halt".
pub const PREEMPT_MAGIC: u64 = 0x5052_4545_4D50_5434;

/// Full preempt frame: everything needed to resume EXACTLY where the timer
/// fired. HW part (what the CPU pushed) + SW part (what the stub spills).
///
/// Field order matches the stub's push sequence (r15 first) so the stub can
/// store with plain `mov [slot + off], reg` — offsets are `index * 8`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FullFrame {
    /// GPRs in spill order (r15 first — matches the stub's push sequence).
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rax: u64,
    /// Task RSP *before* the IRQ (spill RSP + 15*8 spill + 24 HW words).
    pub rsp: u64,
    /// What the CPU pushed: interrupted RIP/CS/RFLAGS (3 words, ring 0).
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    /// Integrity word — must equal [`PREEMPT_MAGIC`].
    pub magic: u64,
}

/// Layout lock: the stub spills exactly [`SPILL_WORDS`] GPRs, copies the
/// 3 hardware words, computes the pre-IRQ rsp, and stamps the magic. All of
/// that must fit the struct, and the struct must be exactly one
/// [`FRAME_STRIDE`] long — the stub indexes `TASK_FRAMES` by stride, so a
/// mismatch would send frames to overlapping slots. If anyone adds a field,
/// this assert screams at compile time instead of the stub lying silently.
const _: () = assert!(
    core::mem::size_of::<FullFrame>() == (SPILL_WORDS + DOWN_WORDS + HW_WORDS) * 8,
    "FullFrame layout drifted from the stub's spill math"
);

impl FullFrame {
    /// All-zero frame (magic=0 — INVALID by design, so an unregistered slot
    /// can never be `iretq`-d: `valid_for` rejects magic≠PREEMPT_MAGIC). Used
    /// to initialize the static table at const-eval time.
    pub const ZERO: Self = Self {
        r15: 0,
        r14: 0,
        r13: 0,
        r12: 0,
        r11: 0,
        r10: 0,
        r9: 0,
        r8: 0,
        rdi: 0,
        rsi: 0,
        rbp: 0,
        rbx: 0,
        rdx: 0,
        rcx: 0,
        rax: 0,
        rsp: 0,
        rip: 0,
        cs: 0,
        rflags: 0,
        magic: 0,
    };

    /// Fresh-task frame: the first `iretq` lands in `entry` with IF=1
    /// (0x202 = IF + reserved bit 1, which must stay set).
    ///
    /// `stack_top` is the 16-aligned top of the task's stack; the RSP this
    /// frame restores is `stack_top - 8`, NOT `stack_top`. The reason is the
    /// same ABI parity [`crate::task::init_task_context`] fights for: a
    /// function is entered with `rsp % 16 == 8` (the state right after a
    /// `call` pushed its return address), and an `iretq` into a fresh frame
    /// pushes NOTHING — so the frame must supply that missing 8 bytes by
    /// pointing 8 below the aligned top. An `rsp % 16 == 0` entry into the
    /// trampoline is a silent misalignment of every 16-byte SSE spill in the
    /// task's whole call chain (and the trampoline's own entry assert would
    /// fire on it, loudly, on the first Step-5b cross-task resume).
    pub fn fresh(entry: u64, cs: u16, stack_top: u64) -> Self {
        assert_eq!(
            stack_top % 16,
            0,
            "fresh preempt frame needs a 16-aligned stack top (got {stack_top:#x})"
        );
        let rsp = stack_top - 8;
        Self {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            r11: 0,
            r10: 0,
            r9: 0,
            r8: 0,
            rdi: 0,
            rsi: 0,
            rbp: 0,
            rbx: 0,
            rdx: 0,
            rcx: 0,
            rax: 0,
            rsp,
            rip: entry,
            cs: cs as u64,
            rflags: 0x202,
            magic: PREEMPT_MAGIC,
        }
    }

    /// Validate before `iretq`: magic intact, RSP canonical + in the task's
    /// `[bottom, top]` window. `top` is INCLUSIVE and `bottom` too: a task
    /// legitimately touches both edges (a fresh frame's rsp is `top - 8`, and
    /// the canary lives at `bottom`), so an exclusive bound would reject a
    /// perfectly good frame. Returns `false` → halt loudly, never `iretq` into
    /// garbage.
    pub fn valid_for(&self, bottom: u64, top: u64) -> bool {
        if self.magic != PREEMPT_MAGIC {
            return false;
        }
        if self.rsp < bottom || self.rsp > top {
            return false;
        }
        // Canonical-address check: bits 63:47 must match bit 47.
        let sign = (self.rsp >> 47) & 1;
        if sign == 0 && (self.rsp >> 48) != 0 {
            return false;
        }
        if sign == 1 && (self.rsp >> 48) != 0xFFFF {
            return false;
        }
        true
    }
}

/// `true` only while a task step runs with IF=1 (set by the trampoline after
/// `sti`, cleared before the yield `cli`). The stub's first check — `false`
/// means "not on a task stack or mid-switch": count ticks, EOI, `iretq` back
/// to SAME. An `AtomicU8` so the stub reads it with one `cmp` and the
/// trampoline flips it without `unsafe` — single CPU, IF=0 through the stub,
/// the trampoline flips it only with IF=0 around the flip... except the
/// set-after-`sti` flip, which races the timer by design (a tick landing in
/// that window sees fence=0 and defers one quantum — correct, not lost).
pub static PREEMPTIBLE: AtomicU8 = AtomicU8::new(0);

/// Max task slots in the static table. Four demo tasks (alpha/beta/gamma/
/// napper) plus headroom — a real growable table is a later step (today the
/// driver spawns a fixed set, so a fixed array is honest, not a limitation
/// worth a dynamic allocator in IRQ context).
pub const MAX_TASKS: usize = 8;

/// Words in the SW spill (15 GPRs) — the stub and the layout assert share this.
const SPILL_WORDS: usize = 15;
/// Words the CPU pushes on a ring-0 IRQ (rip/cs/rflags — no rsp/ss, no code).
const HW_WORDS: usize = 3;
/// Words the stub writes below the spill: pre-IRQ rsp + magic. NOT part of
/// `FullFrame` (the frame struct starts at the r15 slot), but they occupy
/// stack space the layout assert must account for.
const DOWN_WORDS: usize = 2;

/// Byte offset of one frame inside [`TASK_FRAMES`]. Derived from the type —
/// the stub indexes the table with `imul rax, rax, {stride}`, and the const
/// assert above ties `size_of::<FullFrame>()` to the spill math the stub
/// performs, so a struct change can never silently desync the indexing
/// (today: 20 words = 160 bytes = 0xA0).
const FRAME_STRIDE: u64 = core::mem::size_of::<FullFrame>() as u64;

/// Static task table: the IRQ-visible scheduling state. Fixed slots, indexed
/// by `CURRENT_IDX` — no `Box`, no `Vec`, no alloc, no move in IRQ context.
/// The driver fills a slot at spawn (IF=0); the stub only reads/writes bytes
/// in place. `FullFrame` is `repr(C)` so field offsets are stable for asm.
pub static mut TASK_FRAMES: [FullFrame; MAX_TASKS] = [const { FullFrame::ZERO }; MAX_TASKS];

/// Per-slot kernel-stack window `(bottom, top)`, filled by [`register_task`].
/// The IRQ path needs these to decide whether a frame is safe to `iretq` into
/// — a cross-task switch jumps to a stack this code never touched, so the
/// bounds check is the difference between "resume somebody" and "triple fault
/// with no log". `(0, 0)` for a free slot.
static mut SLOT_BOUNDS: [(u64, u64); MAX_TASKS] = [(0, 0); MAX_TASKS];

/// How many times the stub actually switched tasks (IRQ-side counter, read by
/// the driver for the boot log — the stub itself never prints).
pub static PREEMPT_SWITCHES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Index into [`TASK_FRAMES`] of the currently running task, or `MAX_TASKS`
/// (one past the end) when the driver is on the boot stack. Set by the driver
/// before `sti`-ing into a task; advanced by the stub on every real yank.
/// `AtomicU8` (MAX_TASKS=8 fits): the stub does a single `movzx`+`add`+`cmp`,
/// no lock — single CPU, IF=0 through the stub.
pub static CURRENT_IDX: AtomicU8 = AtomicU8::new(MAX_TASKS as u8);

/// Slot occupancy bitmap: bit `i` set ⇔ `TASK_FRAMES[i]` holds a live task.
/// A bitmap (not a count) because slots are released individually when a task
/// finishes — a finish order of 1,3,2 leaves holes, and "index < count" would
/// happily accept a freed slot. `u8` covers [`MAX_TASKS`] = 8 exactly; the
/// stub tests the bit with a single `bt`, no lock (single CPU, IF=0).
pub static SLOT_USED: AtomicU8 = AtomicU8::new(0);

/// Flip the fence. Called by the trampoline only (IF=0 at both flip points —
/// see the race note on [`PREEMPTIBLE`]: the set flip happens right after
/// `sti`, so a tick in that 2-instruction window correctly defers).
pub fn set_preemptible(on: bool) {
    PREEMPTIBLE.store(if on { 1 } else { 0 }, Ordering::Relaxed);
}

/// Register a task's preempt frame in the static table at index `idx`. Called
/// by the driver at spawn with IF=0 (no task running yet) — a plain store,
/// no lock. Sets the occupancy bit so the stub will accept the slot. The
/// frame's `rip` is the trampoline; the first yank overwrites it with the
/// exact interrupted state.
pub fn register_task(idx: usize, frame: FullFrame) {
    assert!(idx < MAX_TASKS, "preempt: task slot overflow");
    assert!(
        SLOT_USED.load(Ordering::Relaxed) & (1 << idx) == 0,
        "preempt: slot {idx} registered twice — slot accounting is broken"
    );
    // NOTE: no rsp sanity check here. Kernel stacks live in the high half
    // (0xFFFF_9000_...), so any bound picked here would either admit nothing
    // or prove nothing — the real check is `valid_for(bottom, top)` against
    // the window recorded by `set_slot_bounds`, which is where the frame is
    // actually consumed.
    unsafe {
        core::ptr::addr_of_mut!(TASK_FRAMES)
            .cast::<FullFrame>()
            .add(idx)
            .write(frame);
    }
    SLOT_USED.fetch_or(1 << idx, Ordering::Relaxed);
}

/// Record the stack window a slot's frame belongs to. Split from
/// [`register_task`] because the bounds come from [`TaskStack`](crate::task)
/// (the frame itself carries no bottom/top), and keeping the two facts
/// together at the call site is what makes the IRQ-side check honest.
pub fn set_slot_bounds(idx: usize, bottom: u64, top: u64) {
    assert!(idx < MAX_TASKS, "preempt: bounds slot overflow");
    assert!(bottom < top, "preempt: empty stack window for slot {idx}");
    unsafe {
        SLOT_BOUNDS[idx] = (bottom, top);
    }
}

/// Stack window recorded for a slot (or `None` if the slot is free).
pub fn slot_bounds(idx: usize) -> Option<(u64, u64)> {
    if idx >= MAX_TASKS {
        return None;
    }
    // SAFETY: single CPU; the driver writes only with IF=0 and the IRQ path
    // reads only while the fence is down or for a slot it is switching away
    // from. A 16-byte aligned pair of u64s cannot tear.
    let b = unsafe { SLOT_BOUNDS[idx] };
    (b.0 != 0).then_some(b)
}

/// Does the frame in slot `idx` look safe to `iretq` into? Magic intact,
/// rsp inside the slot's recorded window. The IRQ path's only guard against
/// jumping into a freed or half-written stack.
pub fn frame_ok(idx: usize) -> bool {
    match slot_bounds(idx) {
        Some((bottom, top)) => {
            let f = snapshot(idx);
            f.valid_for(bottom, top)
        }
        None => false,
    }
}

/// Release a task's slot when it finishes. Clears the occupancy bit and
/// wipes the frame back to its invalid template (magic 0) so a late stray
/// yank — or a `snapshot` of a retired slot — sees "unregistered", never a
/// stale rip pointing at a freed stack. Called by the scheduler after the
/// task's final switch, with the fence down.
pub fn unregister_task(idx: usize) {
    assert!(idx < MAX_TASKS, "preempt: task slot overflow on release");
    assert!(
        SLOT_USED.load(Ordering::Relaxed) & (1 << idx) != 0,
        "preempt: releasing slot {idx} that was never registered"
    );
    unsafe {
        core::ptr::addr_of_mut!(TASK_FRAMES)
            .cast::<FullFrame>()
            .add(idx)
            .write(FullFrame::ZERO);
    }
    SLOT_USED.fetch_and(!(1 << idx), Ordering::Relaxed);
}

/// Publish the running-task index. Called by the driver right before it
/// switches into a task (so the stub knows which slot to stash the yanked
/// frame into) and set back to `MAX_TASKS` when the task yields back to the
/// boot stack.
pub fn set_current(idx: usize) {
    CURRENT_IDX.store(idx as u8, Ordering::Relaxed);
}

/// Which slot is running right now. The trampoline resolves its own `Task`
/// through this rather than through a cached pointer: after an IRQ-driven
/// switch the cached pointer would name the task that was yanked OUT, while
/// the slot always names the task whose stack is under our feet.
pub fn current_idx() -> usize {
    CURRENT_IDX.load(Ordering::Relaxed) as usize
}

/// Snapshot an IRQ-saved frame after the task has yielded back to the driver.
/// Caller is on the boot stack with the fence down, so the stub cannot write
/// task slots concurrently. This validates the ACTUAL asm stash, not the
/// fresh `Task.preempt` template stored in the heap object.
pub fn snapshot(idx: usize) -> FullFrame {
    assert!(idx < MAX_TASKS, "preempt: snapshot slot out of range");
    unsafe {
        core::ptr::addr_of!(TASK_FRAMES)
            .cast::<FullFrame>()
            .add(idx)
            .read_volatile()
    }
}

/// Naked LAPIC preempt stub — installed at 0xEF via `set_handler_addr`.
///
/// Entry state (interrupt gate, IF=0): CPU pushed rip/cs/rflags (24 bytes) on
/// the CURRENT stack (task stack when preemptible, boot stack otherwise).
/// No prologue, no Rust, no `call` — pure asm. Ring-0 IRQ = 3 HW words only
/// (rip/cs/rflags — no rsp/ss, no error code).
///
/// THIS INCREMENT (part 2): same-task save/restore. When the fence is up and
/// the current index names an occupied slot, the stub spills 15 GPRs, STASHES
/// the full frame into `TASK_FRAMES[CURRENT_IDX]` (the table + offset math
/// proven for real — Step 5a reads it back through [`frame_ok`] on every
/// schedule), EOI's, RESTORES the 15 GPRs, and `iretq`s to the SAME rip the
/// CPU pushed. The yanked task resumes exactly where it was. The jump to `2f`
/// (skip the stash, still EOI + `iretq`) is taken when the fence is down or
/// the slot is unoccupied: the boot stack, the driver, and mid-switch states
/// are never stashed. `PREEMPT_SWITCHES` counts every yank that landed on a
/// registered preemptible task — >0 proves the timer→stub→resume path is live
/// and the frame didn't corrupt the task.
///
/// WHY NOT cross-task YET: ring-0 `iretq` does NOT restore RSP (no privilege
/// change → no SS/RSP pop). Resuming a DIFFERENT task's frame from a static
/// slot would leave RSP pointing at the stub's own spill, not at the task's
/// real stack — a triple fault. Step 5b restores RSP explicitly (`mov rsp,
/// [frame.rsp]`), which is exactly what `FullFrame.rsp` already stores (the
/// pre-IRQ rsp, i.e. the address above the CPU's 3 hw words). The stash here
/// is the bridge: it proves the table + offset math is right so Step 5b only
/// swaps the resume source, not the save math.
#[unsafe(naked)]
pub unsafe extern "C" fn lapic_preempt_stub() {
    naked_asm!(
        // --- 0. Save EVERY GPR before using even one as scratch. ---
        // CPU frame below the spill is [rip, cs, rflags] at +0x78/+0x80/+0x88.
        "push r15", "push r14", "push r13", "push r12",
        "push r11", "push r10", "push r9", "push r8",
        "push rdi", "push rsi", "push rbp", "push rbx",
        "push rdx", "push rcx", "push rax",
        // --- 1. ALWAYS bump APIC_TICKS; driver and sleepers depend on it. ---
        // Atomic memory RMW: honest for AtomicU64 and already SMP-safe. No GPR
        // scratch, so the saved task register image stays untouched.
        "lock inc qword ptr [rip + {apic_ticks}]",
        // Fence down means boot/driver stack: skip the task-table stash.
        "cmp BYTE PTR [rip + {fence}], 0",
        "je 2f",
        // Refuse a corrupt/sentinel index BEFORE touching TASK_FRAMES — and
        // refuse a slot nobody owns: the occupancy bitmap is the truth, so a
        // freed-or-never-registered slot can never receive a yank (its frame
        // is the invalid template, magic 0).
        "movzx eax, BYTE PTR [rip + {cur_idx}]",
        "cmp eax, {max_tasks}",
        "jae 2f",
        "movzx ecx, BYTE PTR [rip + {slot_used}]",
        "bt ecx, eax",
        "jnc 2f",
        // Count this real yank (fence up + registered current task index).
        "lock inc qword ptr [rip + {switches}]",
        // --- 2. STASH FullFrame into TASK_FRAMES[index]. ---
        // Stride is `FRAME_STRIDE` (checked against size_of::<FullFrame>() by
        // a const assert — the two can never drift).
        "imul rax, rax, {stride}",
        "lea rdi, [rip + {frames}]",
        "add rdi, rax",
        // Stack spill is reverse of FullFrame fields: [rsp]=rax ..
        // [rsp+0x70]=r15. Copy explicitly in the correct direction.
        "mov rax, [rsp + 0x70]", "mov [rdi + 0x00], rax", // r15
        "mov rax, [rsp + 0x68]", "mov [rdi + 0x08], rax", // r14
        "mov rax, [rsp + 0x60]", "mov [rdi + 0x10], rax", // r13
        "mov rax, [rsp + 0x58]", "mov [rdi + 0x18], rax", // r12
        "mov rax, [rsp + 0x50]", "mov [rdi + 0x20], rax", // r11
        "mov rax, [rsp + 0x48]", "mov [rdi + 0x28], rax", // r10
        "mov rax, [rsp + 0x40]", "mov [rdi + 0x30], rax", // r9
        "mov rax, [rsp + 0x38]", "mov [rdi + 0x38], rax", // r8
        "mov rax, [rsp + 0x30]", "mov [rdi + 0x40], rax", // rdi
        "mov rax, [rsp + 0x28]", "mov [rdi + 0x48], rax", // rsi
        "mov rax, [rsp + 0x20]", "mov [rdi + 0x50], rax", // rbp
        "mov rax, [rsp + 0x18]", "mov [rdi + 0x58], rax", // rbx
        "mov rax, [rsp + 0x10]", "mov [rdi + 0x60], rax", // rdx
        "mov rax, [rsp + 0x08]", "mov [rdi + 0x68], rax", // rcx
        "mov rax, [rsp + 0x00]", "mov [rdi + 0x70], rax", // rax
        "lea rax, [rsp + 0x90]", "mov [rdi + 0x78], rax", // pre-IRQ rsp
        "mov rax, [rsp + 0x78]", "mov [rdi + 0x80], rax", // rip
        "mov rax, [rsp + 0x80]", "mov [rdi + 0x88], rax", // cs
        "mov rax, [rsp + 0x88]", "mov [rdi + 0x90], rax", // rflags
        "mov rax, {magic_const}", "mov [rdi + 0x98], rax", // magic
        // --- 3. Common EOI + exact same-task restore. ---
        "2:",
        "mov rax, [rip + {eoi_base}]",
        "test rax, rax",
        "jz 3f",
        "mov dword ptr [rax], 0",
        "3:",
        "pop rax", "pop rcx", "pop rdx", "pop rbx",
        "pop rbp", "pop rsi", "pop rdi", "pop r8",
        "pop r9", "pop r10", "pop r11", "pop r12",
        "pop r13", "pop r14", "pop r15",
        "iretq",
        fence = sym PREEMPTIBLE,
        cur_idx = sym CURRENT_IDX,
        slot_used = sym SLOT_USED,
        frames = sym TASK_FRAMES,
        eoi_base = sym EOI_BASE,
        magic_const = const PREEMPT_MAGIC,
        max_tasks = const MAX_TASKS,
        stride = const FRAME_STRIDE,
        apic_ticks = sym crate::idt::APIC_TICKS,
        switches = sym PREEMPT_SWITCHES,
    )
}

/// LAPIC EOI register address, set once by `apic::init` (base + 0xB0).
/// Zero means "don't touch the MMIO" — the stub tests it with `jz`, so the
/// handler is harmless even before the APIC is mapped.
/// `static mut` (same `.rodata` trap as [`PREEMPTIBLE`]): written once during
/// bring-up, read by the stub with IF=0.
pub static mut EOI_BASE: u64 = 0;

/// Publish the EOI address to the stub. Called once by `apic::init`.
///
/// Single writer during bring-up (IF=0, no task running), read by the stub
/// with IF=0 — no torn read on a 64-bit aligned store. Plain fn (volatile
/// store, always sound — callers wrap in their own `unsafe` block).
pub fn set_eoi_base(addr: u64) {
    unsafe {
        let p = &raw mut EOI_BASE;
        p.write_volatile(addr);
    }
}
