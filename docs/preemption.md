# Preemption design notes (Phase 3, Step 4 → Step 5)

The LAPIC 0xEF vector is a `#[unsafe(naked)]` stub, not an `x86-interrupt`
function: the compiler emits an `iretq` epilogue that pops exactly the frame
the CPU pushed, and a task switch needs to leave on a *different* frame.

## Frame layout the stub relies on

```
                 +0x00 .. +0x70   15 spilled GPRs (push order r15, r14, … rax)
   rsp at entry  +0x78            rflags   ┐
                 +0x80            cs       │ pushed by the CPU (ring-0 IRQ)
                 +0x88            rip      ┘
                 +0x90            = the task's rsp before the interrupt
```

`FullFrame` mirrors exactly this (20 u64 = 160 bytes, stride 0xA0), and a
`const _` assert ties `size_of::<FullFrame>()` to the asm offsets so the two
can never drift silently.

## What is already proven (Step 4)

* save/restore round-trip on the *same* task, mid-step, verified by 3 real
  yanks per boot;
* the stashed frame validates against the slot's recorded stack window;
* `SLOT_USED` occupancy bitmap + `TASK_PTRS` slot→task identity, so a stale
  pointer can never name the wrong task after an IRQ-driven switch;
* the ABI parity of the bootstrap frame (`rsp % 16 == 8` at task entry),
  asserted in the trampoline rather than argued about.

## Why cross-task is not just "point rsp at the other frame"

A ring-0 `iretq` pops only `rip/cs/rflags` — it never restores `rsp`, because
no privilege change happened. Loading another task's frame into the stub's
registers and `iretq`-ing would therefore resume *that* task's rip with *our*
rsp: the first `push` in its code writes into the wrong stack. The switch has
to restore `rsp` explicitly, and at that moment the stub is still plain asm
with a live `iretq` tail.

## Plan for Step 5b (cross-task yank)

1. **Halt the cooperative driver while preempting.** `task::switch_to_task`
   must not be interrupted holding a `Box`-ownership decision; the fence
   (`PREEMPTIBLE`) is the switch: set only inside the trampoline's step
   window, cleared before the yield. The stub already tests it first.
2. **Cancel `ctx` in favour of frames.** The cooperative `Context` (ret-based,
   6 regs) and the preempt `FullFrame` (iret-based, 15 regs + hw) cannot both
   describe one task after a yank. Either refresh `ctx` from the frame on
   every yank, or retire `ctx` and make every resume go through `iretq`.
   Retiring `ctx` is simpler and has one resume path — preferred.
3. **Resume path.** In the stub's switch arm: EOI, then load the *next* slot's
   frame into registers, then `mov rsp, [frame.rsp]`, `push rflags/cs/rip` in
   that order, `push` the 15 GPRs, `iretq`. The frame's saved rsp already
   points *above* its own hw words, so this reconstructs the exact state.
4. **Never run on the boot stack.** With `ctx` retired, `main` must not use
   `schedule_once`; instead it becomes a task (`slot 0`) and steps aside.
5. **Guards.** Before `iretq`: magic, canonical rsp, `rsp` inside the slot's
   recorded window, and `cs` equal to the kernel code selector.

### ABI parity applies to `FullFrame` too

`iretq` restores `rsp` verbatim and pushes nothing, whereas a task that is
entered by `call` inherits the `rsp % 16 == 8` the pushed return address left
behind. So a *fresh* frame must carry `rsp = stack_top - 8`, not
`stack_top` — otherwise the very first cross-task resume enters the
trampoline at `rsp % 16 == 0`, which misaligns every 16-byte SSE spill in the
task's call chain (and trips the trampoline's own entry assert, loudly).
`FullFrame::fresh` asserts the 16-aligned `stack_top` and subtracts the 8
itself, so the invariant lives in one place instead of at every call site.
Corollary: frames stashed by the *stub* need no such fixup — they record the
interrupted `rsp` exactly as it was, alignment and all.

Each of these is independently testable, which is why they land one at a time
with a boot proof rather than as one big switch.
