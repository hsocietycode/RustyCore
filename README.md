# RustyCore

Minimal `no_std` x86_64 kernel in Rust. 64-bit only, UEFI boot, customizable via Cargo features + `config/kernel_config.toml`.

Org: https://github.com/hsocietycode

## Prereqs

- Rust nightly + `rust-src`
- `qemu-system-x86_64`

```sh
rustup component add rust-src llvm-tools-preview
```

## Build

```sh
cargo xtask build
cargo xtask build -- --release
```

Custom features:

```sh
cargo build -p rustycore-kernel --target x86_64-unknown-none --features "arch-x86_64,sched-rr,alloc-buddy,drv-uart,drv-virtio"
```

## Run

```sh
cargo xtask run-qemu
```

Config lives in `config/kernel_config.toml`.

## Layout

```
Cargo.toml           # workspace + release profile (opt3, LTO fat)
rust-toolchain.toml  # nightly + rust-src
.cargo/config.toml   # x86_64-unknown-none, target-cpu x86-64-v2
config/kernel_config.toml
kernel/              # no_std kernel
xtask/               # dev tasks (build, run-qemu)
docs/                # ROADMAP, design notes
```

## License

MIT OR Apache-2.0
