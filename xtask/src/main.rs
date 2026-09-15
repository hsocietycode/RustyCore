use anyhow::{Context, Result};
use std::{path::PathBuf, process::Command};

const TARGET: &str = "x86_64-unknown-none";

fn usage() -> ! {
    eprintln!("usage: cargo xtask <build|image|run-qemu> [--release]");
    std::process::exit(1);
}

fn kernel_elf_path(release: bool) -> PathBuf {
    let profile = if release { "release" } else { "debug" };
    PathBuf::from("target")
        .join(TARGET)
        .join(profile)
        .join("rustycore-kernel")
}

fn image_path(release: bool) -> PathBuf {
    let profile = if release { "release" } else { "debug" };
    PathBuf::from("target")
        .join(TARGET)
        .join(profile)
        .join("rustycore-bios.img")
}

fn build_kernel(release: bool) {
    let mut cmd = Command::new("cargo");
    cmd.arg("build").arg("-p").arg("rustycore-kernel");
    cmd.arg("--target").arg(TARGET);
    if release {
        cmd.arg("--release");
    }
    println!("> {:?}", cmd);
    let status = cmd.status().expect("failed to run cargo build");
    assert!(status.success(), "kernel build failed");
}

fn build_image(release: bool) -> Result<PathBuf> {
    build_kernel(release);
    let kernel = kernel_elf_path(release);
    let image = image_path(release);
    bootloader::DiskImageBuilder::new(kernel)
        .create_bios_image(&image)
        .context("failed to build BIOS disk image")?;
    println!("image: {}", image.display());
    Ok(image)
}

/// Minimal `[qemu]` reader for `config/kernel_config.toml`.
///
/// Hand-rolled line parser on purpose — xtask stays dependency-lean:
/// understands `key = value` under `[qemu]`, strips comments and quotes.
/// Unknown/missing file → `fallback`, never an error.
fn qemu_cfg(key: &str, fallback: &str) -> String {
    let text = std::fs::read_to_string("config/kernel_config.toml").unwrap_or_default();
    let mut in_qemu = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_qemu = line == "[qemu]";
            continue;
        }
        if !in_qemu {
            continue;
        }
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == key {
                return v.trim().trim_matches('"').to_string();
            }
        }
    }
    fallback.to_string()
}

fn run_qemu(release: bool) -> Result<()> {
    let image = build_image(release)?;
    let image = image.to_str().expect("non-utf8 image path");
    // QEMU flags come from config/kernel_config.toml [qemu] — xtask is the
    // only consumer, nothing else pretends to own them.
    // NOTE: `-nographic` already wires the first serial port to stdio —
    // passing `-serial stdio` on top makes QEMU abort with
    // "cannot use stdio by multiple character devices".
    //
    // NOTE: `-cpu max`, not `qemu64`: the kernel targets x86-64-v2
    // (SSE4.2, POPCNT, ...). The ancient `qemu64` model lacks POPCNT,
    // so core's alignment checks raise #UD → no IDT yet → triple fault.
    // (`max` is a superset of v2, so the v2 baseline always runs.)
    let machine = qemu_cfg("machine", "q35");
    let memory = qemu_cfg("memory", "512M");
    let smp = qemu_cfg("smp", "2");
    let display = qemu_cfg("display", "none");
    let extra: Vec<String> = qemu_cfg("extra_args", "-no-reboot -no-shutdown")
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let mut cmd = Command::new("qemu-system-x86_64");
    cmd.args([
        "-machine",
        &machine,
        "-cpu",
        "max",
        "-m",
        &memory,
        "-smp",
        &smp,
        "-nographic",
        "-display",
        &display,
        "-drive",
        &format!("format=raw,file={image}"),
    ]);
    for a in &extra {
        cmd.arg(a);
    }
    println!("> {:?}", cmd);
    let status = cmd.status().expect("failed to launch qemu-system-x86_64");
    println!("QEMU exited with {status}");
    Ok(())
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let sub = args.next().unwrap_or_else(|| usage());
    let release = args.any(|a| a == "--release");
    match sub.as_str() {
        "build" => {
            build_kernel(release);
            Ok(())
        }
        "image" => {
            build_image(release)?;
            Ok(())
        }
        "run-qemu" => run_qemu(release),
        _ => usage(),
    }
}
