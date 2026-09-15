# RustyCore

Minimal `no_std` x86_64 kernel in Rust. 64-bit only, BIOS boot (UEFI lands in
Phase 1), customizable via Cargo features + `config/kernel_config.toml`.

Org: https://github.com/hsocietycode

## Status

Boots in QEMU today:

```
RustyCore v0.1 - serial online, PIC remapped.
memory: 505 MiB usable, heap 1024 KiB at 0xffff900000000000, phys offset 0xffff800000000000
memory: heap smoke test ok (box=0xc0ffee, vec_len=2)
RustyCore v0.1 - memory online, halting.
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
kernel/              # no_std kernel (bootloader_api entry, serial, PIC)
  src/main.rs        # entry: IF=0 (no IDT yet), serial+PIC init, halt
  src/serial.rs      # COM1 polling driver (IER=0 — no UART IRQs before IDT)
  src/interrupts.rs  # 8259 remap to 32..47, all masked for now
xtask/               # host tools: build, image (BIOS), run-qemu
docs/                # ROADMAP, design notes
```

## License

MIT OR Apache-2.0
