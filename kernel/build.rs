use std::{env, fs, path::PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    // config lives at workspace root: ../config/kernel_config.toml
    let config_path = manifest_dir
        .parent()
        .unwrap()
        .join("config/kernel_config.toml");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", config_path.display());

    let config = fs::read_to_string(&config_path).unwrap_or_default();

    for line in config.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim().replace(['-', '.'], "_");
            let v = v
                .trim()
                .trim_matches('"')
                .trim_end_matches("KB")
                .trim()
                .to_string();
            let upper = k.to_uppercase();
            println!("cargo:rustc-env=KERNEL_CONFIG_{upper}={v}");
            // Export heap size in BYTES for `env!` parsing (u64 from digits only).
            if upper == "HEAP_SIZE_KB" {
                let kb: u64 = v.parse().unwrap_or_else(|_| {
                    panic!("kernel_config.toml: heap_size_kb must be a plain number, got {v:?}")
                });
                println!(
                    "cargo:rustc-env=KERNEL_CONFIG_HEAP_SIZE_BYTES={}",
                    kb * 1024
                );
            }
        }
    }
}
