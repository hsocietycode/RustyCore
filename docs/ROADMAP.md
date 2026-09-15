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

## Phase 1 — Memory (v0.2)
- [ ] 4-level paging, HHDM offset map
- [ ] Frame allocator: bump → buddy (фича `alloc-buddy`)
- [ ] Kernel heap (`linked_list_allocator`)
- [ ] Host unit-tests
- [ ] UEFI image path (`bootloader` uefi feature + OVMF in xtask)

## Phase 2 — Interrupts (v0.3)
- [ ] GDT + TSS + IDT (then IF=1, UART IRQs on)
- [ ] APIC + timer (замена PIC)
- [ ] Syscall stub

## Phase 3 — Process (v0.4)
- [ ] `trait Scheduler` + `sched-rr`, потом `sched-cfs`
- [ ] User mode ring3 + ELF loader
- [ ] RAMFS → FAT32

## Phase 4 — Extras
- [ ] SMP bring-up
- [ ] VirtIO blk/net (`drv-virtio`)
- [ ] Userspace shell
- [ ] `opt-speed` (x86-64-v3) профиль
