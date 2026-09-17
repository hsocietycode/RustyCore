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
//!   privilege change, no error code). The stub spills all 15 GPRs, stashes
//!   the frame into a static slot, picks next via the scheduler's
//!   try-only IRQ path, and `iretq`s into the next task's saved frame.
//! - NO `call` inside the stub: SysV alignment at IRQ entry is arbitrary
//!   (mid-step compiler state minus 24 bytes of HW push), so a `call` would
//!   need an explicit `and rsp,-16` dance with fixup math that forgets bytes.
//!   Instead the stub is pure asm end-to-end; policy runs in `pick_next_irq`
//!   (plain Rust, `try_lock` only, no alloc/print) operating on static slots.
//! - The scheduler's IRQ-visible state lives in statics (fixed slots,
//!   runqueue of indices — no `Box`, no alloc, no move in IRQ context). The
//!   driver allocates and reaps with IF=0; the handler only swaps indices
//!   and copies bytes.
//! - `PREEMPTIBLE` fence: `true` only inside the trampoline's step window
//!   (after `sti`, before the yield `cli`). The stub checks it first — if the
//!   CPU is on the boot stack, in the driver, or mid-switch, the frame is
//!   discarded: ticks counted, EOI sent, `iretq` back to the SAME stack.
//!   Driver-vs-IRQ ownership race removed by construction.
//! - Cooperative `ctx` (ret-based) and preempt frame (iret-based) are SEPARATE
//!   slots: a `rsp` saved by one path is garbage to the other, so they never
//!   share. Fresh tasks get a SYNTHESIZED hw frame (rip=trampoline,
//!   cs=kernel CS, rflags=0x202) and resume through the same `iretq`.
//! - Zero printing, zero blocking-lock, zero allocation in the stub. Ticks
//!   still counted here (`APIC_TICKS`/`PREEMPT_TICKS`/`timer_tick`) — the
//!   driver's `hlt` loop waits on them, so a silent stub would stall boot.

use core::arch::naked_asm;

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

/// Words in the SW spill (15 GPRs) — the stub and the math share this.
const SPILL_WORDS: usize = 15;
/// Words the CPU pushes on a ring-0 IRQ (rip/cs/rflags — no rsp/ss, no code).
const HW_WORDS: usize = 3;

/// Layout lock: the stub spills exactly [`SPILL_WORDS`] GPRs, then the frame
/// carries pre-IRQ rsp + 3 HW words + magic. If anyone adds a field, this
/// assert screams at compile time — the stub's offsets would silently lie.
const _: () = assert!(
    core::mem::size_of::<FullFrame>() == (SPILL_WORDS + 1 + HW_WORDS + 1) * 8,
    "FullFrame layout drifted from the stub's spill math"
);

impl FullFrame {
    /// Fresh-task frame: first `iretq` lands in `entry` with `stack_top` as
    /// RSP, IF=1 (0x202 = IF + reserved bit 1, which must stay set).
    pub fn fresh(entry: u64, cs: u16, stack_top: u64) -> Self {
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
            rsp: stack_top,
            rip: entry,
            cs: cs as u64,
            rflags: 0x202,
            magic: PREEMPT_MAGIC,
        }
    }

    /// Validate before `iretq`: magic intact, RSP canonical + in the task's
    /// `[bottom, top]` window (top INCLUSIVE — a fresh frame's rsp IS the
    /// stack top; nothing pushed yet). Returns `false` → halt loudly, never
    /// `iretq` into garbage.
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
/// to SAME. A single byte (not `AtomicBool`): the stub reads it with one
/// `cmp`, no atomic prefix needed — single CPU, IF=0 through the stub, the
/// trampoline flips it only with IF=0 around the flip... except the set-after-
/// `sti` flip, which races the timer by design (a tick landing in that window
/// sees fence=false and defers one quantum — correct, not lost).
///
/// `static mut` (NOT plain `static`): the trampoline WRITES this, and a plain
/// `static` lands in `.rodata` — the first fence flip page-faults (caught on
/// QEMU: write violation at the static's address). Single writer (the running
/// task) + single reader (the stub, IF=0) — no data race, but `static mut`
/// access stays in `unsafe` blocks as the language demands.
pub static mut PREEMPTIBLE_BYTE: u8 = 0;

/// How many times the stub actually switched tasks (IRQ-side counter, read by
/// the driver for the boot log — the stub itself never prints).
pub static PREEMPT_SWITCHES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Flip the fence. Called by the trampoline only (IF=0 at both flip points —
/// see the race note on [`PREEMPTIBLE_BYTE`]: the set flip happens right
/// after `sti`, so a tick in that 2-instruction window correctly defers).
///
/// Single writer (the running task), single-CPU, byte store is atomic —
/// plain write, no `unsafe` needed (callers already run in an `unsafe` fn,
/// but the operation itself is a volatile byte store, always sound).
pub fn set_preemptible(on: bool) {
    unsafe {
        let p = &raw mut PREEMPTIBLE_BYTE;
        p.write_volatile(if on { 1 } else { 0 });
    }
}

/// Naked LAPIC preempt stub — installed at 0xEF via `set_handler_addr`.
///
/// Entry state (interrupt gate, IF=0): CPU pushed rip/cs/rflags (24 bytes) on
/// the CURRENT stack (task stack when preemptible, boot stack otherwise).
/// No prologue, no Rust, no `call` — pure asm:
///
/// 1. Fence: `cmpb PREEMPTIBLE_BYTE, 0` → `je same_stack` (skip spill).
/// 2. Spill 15 GPRs; stash rip/cs/rflags/rsp+magic into the current task's
///    preempt slot via `CURRENT_TASK` + field offsets (raw stores only).
/// 3. `pick_next_irq`: try-lock scheduler, rotate, stash next frame words
///    into the NEXT task's stack... — full path lands with the static table
///    commit in `task.rs` (slots + index runqueue + driver alloc/reap).
///    UNTIL THEN the stub counts ticks, EOIs, and `iretq`s to SAME — a
///    correct fence+tick handler, never a half-switch.
///
/// Interim discipline (this commit): the IDT keeps the `x86-interrupt`
/// handler — this stub is NOT installed yet. It must still ASSEMBLE (dead
/// code that doesn't build is debt), and its fence math is reviewed here so
/// the table commit only flips the IDT entry, not the stub.
#[unsafe(naked)]
pub unsafe extern "C" fn lapic_preempt_stub() {
    naked_asm!(
        // --- 1. fence check (single byte, rip-relative, no stack use) ---
        // Intel syntax (the kernel builds with `-C llvm-args=-x86-asm-syntax=intel`
        // via? NO — default is AT&T. `cmpb` is AT&T-only; in Intel syntax the
        // mnemonic is `cmp` with an explicit BYTE PTR size. One-byte compare,
        // no flags preserved needed (IF=0 already, stub owns all flags).
        "cmp BYTE PTR [rip + {flag}], 0",
        "je 2f",
        // --- PREEMPTIBLE path (table commit fills the stash/pick/switch) ---
        // Spill 15 GPRs (order matches FullFrame: r15 first).
        "push r15",
        "push r14",
        "push r13",
        "push r12",
        "push r11",
        "push r10",
        "push r9",
        "push r8",
        "push rdi",
        "push rsi",
        "push rbp",
        "push rbx",
        "push rdx",
        "push rcx",
        "push rax",
        // STASH + PICK + SWITCH land with the static table (task.rs commit):
        // the stub will store rip/cs/rflags (at [rsp+120..144]) + computed
        // pre-IRQ rsp (rsp+144) + magic into CURRENT_TASK's preempt slot,
        // rotate the index runqueue under try_lock, validate next frame,
        // EOI, mov rsp + pop + iretq. Until then: restore the spill and
        // fall into the common tail — ticks + EOI + same-stack iretq.
        "pop rax",
        "pop rcx",
        "pop rdx",
        "pop rbx",
        "pop rbp",
        "pop rsi",
        "pop rdi",
        "pop r8",
        "pop r9",
        "pop r10",
        "pop r11",
        "pop r12",
        "pop r13",
        "pop r14",
        "pop r15",
        // --- 2. common tail: EOI the LAPIC + iretq to SAME stack ---
        // EOI = volatile dword 0 at APIC base + 0xB0. The base is a static
        // here (set once by apic::init) — no Rust call, no stack use.
        // (Table commit wires EOI_BASE; until then this address is zero and
        // the stub is not installed — the tail assembles but never runs.)
        "mov rax, [rip + {eoi_base}]",
        "test rax, rax",
        "jz 2f",
        "mov dword ptr [rax], 0",
        "2:",
        "iretq",
        flag = sym PREEMPTIBLE_BYTE,
        eoi_base = sym EOI_BASE,
    )
}

/// LAPIC EOI register address, set once by `apic::init` (base + 0xB0).
/// Zero until then — the stub is not installed before the table commit, so a
/// zero base can only mean "don't touch the MMIO", tested with `jz`.
/// `static mut` (same `.rodata` trap as [`PREEMPTIBLE_BYTE`]): written once
/// during bring-up, read by the stub with IF=0.
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
