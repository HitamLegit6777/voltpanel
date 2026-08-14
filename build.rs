//! Cache-busting asset version.
//!
//! Static assets (`app.css`, `app.js`, `icons.js`) are served with
//! `Cache-Control: immutable`, so their URL must change whenever their content
//! does — otherwise a browser keeps a stale bundle for a year. `ASSET_VERSION`
//! was historically `CARGO_PKG_VERSION`, which stays fixed across local edits.
//!
//! This script folds a deterministic FNV-1a hash of the three version-stamped
//! assets into the version string and writes it to `OUT_DIR/asset_version.rs`.
//! Any edit to those files triggers a re-run (`cargo:rerun-if-changed`) and thus
//! a fresh `?v=` on the next deploy, no manual version bump required.

use std::env;
use std::fs;
use std::path::PathBuf;

/// FNV-1a 64-bit over `data` (dependency-free, stable).
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let assets = ["static/css/app.css", "static/js/app.js", "static/js/icons.js"];

    let mut combined = Vec::new();
    for rel in assets {
        let path = root.join(rel);
        println!("cargo:rerun-if-changed={}", path.display());
        let bytes = fs::read(&path).unwrap_or_else(|e| panic!("failed to read {rel}: {e}"));
        combined.extend_from_slice(&bytes);
    }

    let h = fnv1a(&combined);
    let h32 = (h ^ (h >> 32)) as u32;
    let pkg = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0".into());
    let version = format!("{pkg}-{h32:08x}");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    // `format!("{:?}", s)` emits a Rust string literal (quoted + escaped), so
    // the file is directly includable as an expression.
    fs::write(out_dir.join("asset_version.rs"), format!("{:?}", version))
        .expect("failed to write asset_version.rs");
}
