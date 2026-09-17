# RustyCore ROADMAP

> **How to read the boot-proof quotes.** Strings quoted as boot proof are the
> lines the kernel ACTUALLY printed at the time that phase was signed off —
> taken from the serial log of a real boot, not paraphrased. They are
> evidence for a phase, not a promise about HEAD: a later step may stop
> printing an earlier step's line (Phase 2's tick-proof fell away when the
> LAPIC displaced the PIT, Phase 0's line when Phase 1 changed the boot
> banner). Where a quote no longer matches HEAD, it says so inline. To check
> the CURRENT output, read a fresh boot log rather than this file.

## Phase 0 — Scaffolding (v0.1) — DONE, boots in QEMU
- [x] `no_std` kernel entry (`bootloader_api` 0.11) + panic handler
- [x] `xtask` with `build`, `image` (BIOS disk), `run-qemu`
- [x] `config/kernel_config.toml`
- [x] COM1 polling serial — printed `RustyCore v0.1 - serial online, PIC remapped, halting.`
      at the time (`2add5a4`); the banner has since grown into the v0.2
      multi-line bring-up above, so this exact string is history, not HEAD output
- [x] 8259 PIC remapped to 32..47, all IRQs masked
- [x] IF=0 at entry (no IDT yet — any IRQ would triple-fault)
- [x] `-cpu max` in xtask (kernel is x86-64-v2: `qemu64` lacks POPCNT → #UD → triple fault)
- [x] CI: fmt + xtask clippy + kernel check (debug/release) + image build

## Phase 1 — Memory (v0.2) — DONE, boots in QEMU
- [x] `physical_memory` mapping at `0xFFFF_8000_0000_0000` via BOOTLOADER_CONFIG
- [x] Frame allocator: bump (`BootFrameAllocator` over Usable regions)
- [x] Kernel heap 1 MiB (`linked_list_allocator`, Box+Vec smoke test ok)
- [x] Boot proof (`4bacd4d`): `memory: 504 MiB usable in 3 regions, heap 1024 KiB at
      0xffff900000000000, phys offset 0xffff800000000000 (frames: buddy)` +
      `memory: heap self-test ok (len=66, sum=0xc107da)`
- [x] Buddy allocator (`alloc-buddy` feature, now default): two-phase bring-up (bump cursor pre-heap → free lists post-heap), split/merge + reclaim, boot self-test (alloc+free round-trip) — QEMU proof: `memory: buddy self-test ok (free 128841 frames, alloc+free round-trips, allocated=259)`
- [ ] Host unit-tests
- [ ] UEFI image path (`bootloader` uefi feature + OVMF in xtask)

> Free-frame counts and the usable-RAM figure move whenever the bootloader,
> the QEMU memory size or an allocator change lands — the ones quoted above
> are from HEAD `4bacd4d` at `[qemu] memory = "512M"`. Re-read them from a
> real boot log before treating them as current; they are evidence of a
> working path, not constants of the design.

## Phase 2 — Interrupts (v0.3) — DONE, boots in QEMU
- [x] GDT + TSS (5-page double-fault IST stack, 16-byte aligned) + IDT (exceptions + IRQ vectors)
- [x] PIT @ ~100 Hz on IRQ0, `TIMER_TICKS` counter, keyboard IRQ1 stub
- [x] IF=1 with live self-tests: `int3` returns, PIT and LAPIC both count past the gate
- [x] Boot proof (healthy path, HEAD `4bacd4d`): `self-test: breakpoint handler returned, IDT works.` /
      `self-test: syscall stub ok (2 hits).` /
      `self-test: LAPIC master proven (apic +100 like the gate, pic frozen at 131).`
- [x] APIC + timer (замена PIC) — probe → MMIO map → enable → calibrate (median of 3) → dual-clock soak → promote with rollback
- [x] Syscall stub (`int 0x80` gate, kernel-only, `SYSCALL_HITS`)

> The string `Phase 2 online` is NOT printed on the healthy path at HEAD: it
> appears only in the two degraded branches (promotion skipped, LAPIC stalled
> after promotion). It was the healthy path's own line earlier in the phase
> (`c6f3a9f`: `self-test: 100 timer ticks seen, IRQs work. Phase 2 online.
> Halting.`), and `bb9dbdb` removed it when the LAPIC timer displaced the PIT
> as master — the PIT-tick proof it narrates stopped being the thing that
> matters. A healthy boot now ends Phase 2 by promoting the LAPIC and saying
> so in the `LAPIC master proven` line above. Do not gate CI on a marker the
> good path never emits — see the note in `.github/workflows/ci.yml`.

## Phase 3 — Process (v0.4)
- [x] Step 1: `trait Scheduler` + `sched-rr` (cooperative, all-asleep fix, napper witness)
- [x] Step 2a: per-task `TaskStack` (128K heap-backed, 16B-aligned top) + timer clock
- [x] Step 2b: real context switch (`#[unsafe(naked)]` save/restore callee-saved + RSP, fake-frame bootstrap, `task_trampoline`, `Box<Task>` queue, `TSS.rsp0` proof-of-path, stack canary, `-C no-redzone=yes`) — boot proof: interleaved steps on own stacks, rsp0 per switch, ledger 5-5-5-1
- [x] Step 3: `sched-cfs` policy behind the same trait (min-vruntime pick, weight-proportional accrual, sleeper floor clamp; gamma@2048 finishes first — QEMU boot proof)
- [x] Step 4: preemptive switch-from-IRQ — LAPIC 0xEF entry is a `#[unsafe(naked)]` stub (interrupt gate, DPL0), `PREEMPTIBLE` fence, static `TASK_FRAMES` table (index-addressed, no alloc in IRQ), `FullFrame` (15 GPRs + hw rip/cs/rflags + pre-IRQ rsp + magic, 160B, layout-locked by `const _`), `lock inc` tick/yank counters — boot proof: 3 real yanks, each resumed exactly, IRQ-stashed frame validated per task, ledger 5-5-5-1
- [x] Step 5a: slot identity — `TASK_PTRS` (slot → task, stable box address, cleared before the drop), `SLOT_BOUNDS` (per-slot stack window for IRQ-side validation), trampoline resolves itself by slot (cached `CURRENT_TASK` pointer deleted), `frame_ok` checks the table rather than the task
- [ ] Step 5b: cross-task yank — retire the cooperative `Context` (one resume path through `iretq`), explicit `mov rsp, [frame.rsp]`, `main` becomes a task instead of a driver. Design notes: [`docs/preemption.md`](preemption.md)
- [ ] User mode ring3 + ELF loader
- [ ] RAMFS → FAT32

## Phase 4 — Extras
- [ ] SMP bring-up
- [ ] VirtIO blk/net (`drv-virtio`)
- [ ] Userspace shell
- [ ] `opt-speed` (x86-64-v3) профиль
