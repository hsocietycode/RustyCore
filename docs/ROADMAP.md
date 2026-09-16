# RustyCore ROADMAP

## Phase 0 — Scaffolding (v0.1) — DONE, boots in QEMU
- [x] `no_std` kernel entry (`bootloader_api` 0.11) + panic handler
- [x] `xtask` with `build`, `image` (BIOS disk), `run-qemu`
- [x] `config/kernel_config.toml`
- [x] COM1 polling serial — `RustyCore v0.1 - serial online, PIC remapped, halting.`
- [x] 8259 PIC remapped to 32..47, all IRQs masked
- [x] IF=0 at entry (no IDT yet — any IRQ would triple-fault)
- [x] `-cpu max` in xtask (kernel is x86-64-v2: `qemu64` lacks POPCNT → #UD → triple fault)
- [x] CI: fmt + xtask clippy + kernel check (debug/release) + image build

## Phase 1 — Memory (v0.2) — DONE, boots in QEMU
- [x] `physical_memory` mapping at `0xFFFF_8000_0000_0000` via BOOTLOADER_CONFIG
- [x] Frame allocator: bump (`BootFrameAllocator` over Usable regions)
- [x] Kernel heap 1 MiB (`linked_list_allocator`, Box+Vec smoke test ok)
- [x] Boot proof: `memory: 505 MiB usable, heap 1024 KiB ... smoke test ok`
- [ ] Buddy allocator (`alloc-buddy` feature) — next
- [ ] Host unit-tests
- [ ] UEFI image path (`bootloader` uefi feature + OVMF in xtask)

## Phase 2 — Interrupts (v0.3) — DONE, boots in QEMU
- [x] GDT + TSS (5-page double-fault IST stack) + IDT (exceptions + IRQ vectors)
- [x] PIT @ ~100 Hz on IRQ0, `TIMER_TICKS` counter, keyboard IRQ1 stub
- [x] IF=1 with live self-tests: `int3` returns, 100 timer ticks seen
- [x] Boot proof: `self-test: breakpoint handler returned, IDT works.` /
      `self-test: 100 timer ticks seen, IRQs work. Phase 2 online. Halting.`
- [ ] APIC + timer (замена PIC) — next
- [ ] Syscall stub

## Phase 3 — Process (v0.4)
- [x] Step 1: `trait Scheduler` + `sched-rr` (cooperative, all-asleep fix, napper witness)
- [x] Step 2a: per-task `TaskStack` (128K heap-backed, 16B-aligned top) + preemption clock (`timer_tick` from LAPIC handler, `NEED_RESCHED` every 10 ticks, preempt points interleaved in demo log)
- [ ] Step 2b: real context switch (`asm!`, register save/restore, `TSS.rsp0`) under the same `Scheduler` trait
- [ ] `sched-cfs` policy behind the same trait
- [ ] User mode ring3 + ELF loader
- [ ] RAMFS → FAT32

## Phase 4 — Extras
- [ ] SMP bring-up
- [ ] VirtIO blk/net (`drv-virtio`)
- [ ] Userspace shell
- [ ] `opt-speed` (x86-64-v3) профиль
