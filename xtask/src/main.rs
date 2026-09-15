use std::process::Command;

fn usage() -> ! {
    eprintln!("usage: cargo xtask <build|run-qemu> [--release]");
    std::process::exit(1);
}

fn build_kernel(release: bool) {
    let mut cmd = Command::new("cargo");
    cmd.arg("build").arg("-p").arg("rustycore-kernel");
    cmd.arg("--target").arg("x86_64-unknown-none");
    if release {
        cmd.arg("--release");
    }
    println!("> {:?}", cmd);
    let status = cmd.status().expect("failed to run cargo build");
    assert!(status.success(), "kernel build failed");
}

fn run_qemu(release: bool) {
    build_kernel(release);
    let kernel_bin = if release {
        "target/x86_64-unknown-none/release/rustycore-kernel"
    } else {
        "target/x86_64-unknown-none/debug/rustycore-kernel"
    };
    let mut cmd = Command::new("qemu-system-x86_64");
    cmd.args([
        "-machine", "q35",
        "-cpu", "qemu64",
        "-m", "512M",
        "-smp", "2",
        "-nographic",
        "-serial", "stdio",
        "-display", "none",
        "-kernel", kernel_bin,
        "-no-reboot", "-no-shutdown",
    ]);
    println!("> {:?}", cmd);
    let status = cmd.status().expect("failed to launch qemu-system-x86_64");
    println!("QEMU exited with {status}");
}

fn main() {
    let mut args = std::env::args().skip(1);
    let sub = args.next().unwrap_or_else(|| usage());
    let release = std::env::args().any(|a| a == "--release");
    match sub.as_str() {
        "build" => build_kernel(release),
        "run-qemu" => run_qemu(release),
        _ => usage(),
    }
}
