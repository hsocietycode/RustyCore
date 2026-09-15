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

    let mut section = String::new();
    let mut frame_allocator_cfg: Option<String> = None;

    for raw in config.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // [section] headers scope every key below them — without this,
        // same-named keys in different sections would collide silently.
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.trim().replace(['-', '.'], "_").to_uppercase();
            continue;
        }
        // Strip inline comments BEFORE splitting: `default = "rr" # rr | cfs`
        // would otherwise keep a stray quote in the value.
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim().replace(['-', '.'], "_");
            let v = v.trim().trim_matches('"').trim().to_string();
            let upper = k.to_uppercase();
            // Section-scoped (authoritative) + legacy flat name.
            if !section.is_empty() {
                println!("cargo:rustc-env=KERNEL_CONFIG_{section}_{upper}={v}");
            }
            println!("cargo:rustc-env=KERNEL_CONFIG_{upper}={v}");

            if upper == "HEAP_SIZE_KB" {
                let kb: u64 = v.parse().unwrap_or_else(|_| {
                    panic!("kernel_config.toml: heap_size_kb must be a plain number, got {v:?}")
                });
                println!(
                    "cargo:rustc-env=KERNEL_CONFIG_HEAP_SIZE_BYTES={}",
                    kb * 1024
                );
            }
            // Generic byte-size values: "128K", "512M", "1G" (also KB/MB/GB).
            if upper == "STACK_SIZE" {
                let bytes = parse_size_bytes(&v).unwrap_or_else(|| {
                    panic!("kernel_config.toml: stack_size must be like \"128K\", got {v:?}")
                });
                println!("cargo:rustc-env=KERNEL_CONFIG_STACK_SIZE_BYTES={bytes}");
            }
            if section == "MEMORY" && upper == "FRAME_ALLOCATOR" {
                frame_allocator_cfg = Some(v.to_lowercase());
            }
        }
    }

    // The TOML `frame_allocator` is not decorative: it must agree with the
    // Cargo feature, otherwise the config lies about what boots.
    let bump = env::var("CARGO_FEATURE_ALLOC_BUMP").is_ok();
    let buddy = env::var("CARGO_FEATURE_ALLOC_BUDDY").is_ok();
    match frame_allocator_cfg.as_deref() {
        Some("bump") if bump && !buddy => {}
        Some("buddy") if buddy && !bump => {}
        Some(other) => panic!(
            "kernel_config.toml [memory] frame_allocator={other:?} disagrees \
             with Cargo features (alloc-bump={bump}, alloc-buddy={buddy}): \
             select exactly one, in both places"
        ),
        None => {}
    }
}

/// Parse `"128K"` / `"512M"` / `"1G"` (K/M/G, optional trailing B) to bytes.
fn parse_size_bytes(v: &str) -> Option<u64> {
    let v = v.trim();
    let (digits, mult) = if let Some(n) = v.strip_suffix(['K', 'k']) {
        (n.trim_end_matches(['B', 'b']), 1024u64)
    } else if let Some(n) = v.strip_suffix(['M', 'm']) {
        (n.trim_end_matches(['B', 'b']), 1024 * 1024)
    } else if let Some(n) = v.strip_suffix(['G', 'g']) {
        (n.trim_end_matches(['B', 'b']), 1024 * 1024 * 1024)
    } else {
        (v, 1)
    };
    digits.parse::<u64>().ok()?.checked_mul(mult)
}
