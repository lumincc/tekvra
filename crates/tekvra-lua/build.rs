//! Portable link-search helper for `luau0-src` (`-lstdc++`) when `rustc` defaults to `rust-lld` on Linux.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=CXX");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }

    let cxx = std::env::var("CXX").unwrap_or_else(|_| "g++".to_string());
    for candidate in [cxx.as_str(), "c++", "g++"] {
        if let Ok(output) = Command::new(candidate)
            .arg("-print-file-name=libstdc++.so")
            .output()
            && output.status.success()
        {
            let raw = String::from_utf8_lossy(&output.stdout);
            let path = PathBuf::from(raw.trim());
            if path.is_absolute()
                && let Some(dir) = path.parent()
            {
                println!("cargo:rustc-link-search=native={}", dir.display());
                break;
            }
        }
    }
}
