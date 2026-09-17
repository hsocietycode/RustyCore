# RustyCore

Minimal `no_std` x86_64 kernel in Rust. 64-bit only, BIOS boot first (UEFI is
on the roadmap), customizable via Cargo features + `config/kernel_config.toml`.

Org: https://github.com/hsocietycode

## Status

Boots in QEMU today. Phase 1 (memory), Phase 2 (interrupts) and Phase 3
(processes) are done — the kernel owns a buddy frame allocator, a heap, a live
LAPIC, a Round-Robin/CFS scheduler behind one trait, real per-task stacks, and
a `#[unsafe(naked)]` LAPIC-timer stub that preempts a running task mid-step.

Tail of a real boot (abridged):

```
RustyCore v0.2 - serial online.
RustyCore v0.2 - GDT+TSS loaded (double-fault IST ready).
RustyCore v0.2 - IDT loaded (exceptions + IRQ vectors live).
RustyCore v0.2 - PIC remapped to 32..=47, all masked.
apic: probe present=true base=0xfee00000 enabled=true bsp=true rsdp=yes; base=msr
memory: 504 MiB usable in 3 regions, heap 1024 KiB at 0xffff900000000000, phys offset 0xffff800000000000 (frames: buddy)
memory: buddy self-test ok (free 128841 frames, alloc+free round-trips, allocated=259)
apic: mapped + enabled id=0 ver=0x50014 svr=0x1ff
apic-timer: calibrate rounds=[6205998 6243191 6253336] median=6243191
self-test: syscall stub ok (2 hits).
apic-timer: promoted to master clock (PIC IRQ0 masked, IRQ1 kept)
self-test: LAPIC master proven (apic +100 like the gate, pic frozen at 131).
preempt: fence=down eoi-base=0xffffa000000000b0 stub=0x1000000dc24 switches=0 (proof block ok)
sched: task 1/alpha stack top 0xffff900000020320 (128 KiB) ctx.rsp=... preempt rip=... slot=0
sched: preempt yank 1 (switches 0->1) at apic tick ~...
sched ledger: task 1/alpha runs=5
sched: round-robin fair (3x5 + napperx1). Phase 3 online. Halting.
```

Per-phase status lives in [`docs/ROADMAP.md`](docs/ROADMAP.md). The last line
is the CI gate: `Phase 3 online` is printed only on the healthy path, and the
run is also required to show at least one `sched: preempt yank` — a boot
where the LAPIC stub never yanked a running task is a regression, not a pass.
(Note that `Phase 2 online` is deliberately NOT part of the gate: it is
emitted only by the two degraded fallback branches, never by a good boot.)

## Prereqs

- Rust nightly + `rust-src` + `llvm-tools-preview` (+ target `x86_64-unknown-none`)
- `qemu-system-x86_64` with `-cpu max` (kernel targets **x86-64-v2**: needs POPCNT/SSE4.2,
  the ancient `qemu64` model lacks them and triple-faults)
- a host C linker (`cc`) — only for dependency *build scripts*, not the kernel itself

```sh
rustup component add rust-src llvm-tools-preview
rustup target add x86_64-unknown-none
```

## Build

```sh
cargo xtask build              # debug kernel ELF
cargo xtask image              # + BIOS disk image (target/.../rustycore-bios.img)
cargo xtask run-qemu           # build image + boot it in QEMU
```

Custom features:

```sh
cargo build -p rustycore-kernel --target x86_64-unknown-none --features "arch-x86_64,sched-rr,alloc-buddy,drv-uart,drv-virtio"
```

## Layout

```
Cargo.toml           # workspace + release profile (opt3, LTO fat)
rust-toolchain.toml  # nightly + rust-src
.cargo/config.toml   # x86-64-v2 rustflags, `cargo xtask` alias
config/kernel_config.toml
kernel/              # no_std kernel (bootloader_api entry, serial, PIC/APIC, GDT/IDT, PIT, alloc, sched)
  src/main.rs        # entry: GDT+TSS → IDT → PIC → APIC probe → memory → PIT → self-tests → sched demo
  src/serial.rs      # COM1 polling driver (IER=0 — no UART IRQs before IDT)
  src/interrupts.rs  # 8259 remap to 32..47
  src/gdt.rs         # GDT + TSS with double-fault IST escape stack
  src/idt.rs         # exception handlers (breakpoint, DF/IST, PF, GP…) + IRQ vectors
  src/apic.rs        # LAPIC probe/map/enable/calibrate/soak/promote (+ EOI)
  src/timer.rs       # PIT channel 0 @ ~100 Hz, unmasks IRQ0
  src/memory/*.rs    # bump + buddy frame allocators, heap, MMIO window mapping
  src/task.rs        # Task/TaskStack, naked context switch, RR + CFS schedulers
  src/preempt.rs     # naked LAPIC-timer stub: full-frame preemption from IRQ
xtask/               # host tools: build, image (BIOS), run-qemu
docs/                # ROADMAP, design notes
```

## License

MIT OR Apache-2.0
