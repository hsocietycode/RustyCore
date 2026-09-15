# RustyCore

Minimal `no_std` x86_64 kernel in Rust. 64-bit only, BIOS boot (UEFI lands in
Phase 1), customizable via Cargo features + `config/kernel_config.toml`.

Org: https://github.com/hsocietycode

## Status

Boots in QEMU today:

```
RustyCore v0.2 - serial online.
RustyCore v0.2 - GDT+TSS loaded (double-fault IST ready).
RustyCore v0.2 - IDT loaded (exceptions + IRQ vectors live).
RustyCore v0.2 - PIC remapped to 32..=47, all masked.
memory: 248 MiB usable in 3 regions, heap 1024 KiB at 0xffff900000000000, phys offset 0xffff800000000000
memory: heap self-test ok (len=66, sum=0xc107da)
timer: PIT @ ~100 Hz, IRQ0 unmasked
self-test: firing int3 breakpoint...
self-test: breakpoint handler returned, IDT works.
self-test: enabling interrupts, waiting for 100 timer ticks...
self-test: 100 timer ticks seen, IRQs work. Phase 2 online. Halting.
```

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
kernel/              # no_std kernel (bootloader_api entry, serial, PIC, GDT/IDT, PIT)
  src/main.rs        # entry: GDT+TSS → IDT → PIC → memory → PIT → self-tests → IF=1
  src/serial.rs      # COM1 polling driver (IER=0 — no UART IRQs before IDT)
  src/interrupts.rs  # 8259 remap to 32..47, all masked for now
  src/gdt.rs         # GDT + TSS with double-fault IST escape stack
  src/idt.rs         # exception handlers (breakpoint, DF/IST, PF, GP…) + IRQ0/IRQ1
  src/timer.rs       # PIT channel 0 @ ~100 Hz, unmasks IRQ0
xtask/               # host tools: build, image (BIOS), run-qemu
docs/                # ROADMAP, design notes
```

## License

MIT OR Apache-2.0
