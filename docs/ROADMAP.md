# RustyCore ROADMAP

## Phase 0 — Scaffolding (v0.1) — сейчас
- [x] `no_std` kernel entry + panic handler
- [x] `xtask` with `build`, `run-qemu`
- [x] `config/kernel_config.toml`
- [ ] VGA + serial logging macros
- [ ] QEMU boot screenshot в README

## Phase 1 — Memory (v0.2)
- [ ] 4-level paging, HHDM offset map
- [ ] Frame allocator: bump → buddy (фича `alloc-buddy`)
- [ ] Kernel heap (`linked_list_allocator`)
- [ ] Host unit-tests

## Phase 2 — Interrupts (v0.3)
- [ ] GDT + TSS + IDT
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
