//! Fingerprints the shader translator, so that cached translations
//! (`src/shader_cache.rs`) last until the translator itself changes and not
//! only until the next build of anything. Also sets `cfg(agility_runtime)` for
//! the unit-test executable when the D3D12 runtime staged by
//! `cargo xtask agility` is present.

use std::path::{Path, PathBuf};

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn main() {
    println!("cargo::rustc-check-cfg=cfg(agility_runtime)");
    let here = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let crates = here.join("..");
    let root = crates.join("..");
    // The translator, what it is built from, and the form entries are kept in.
    let mut all = vec![
        crates.join("victory-shader/Cargo.toml"),
        here.join("src/shader_cache.rs"),
        root.join("Cargo.lock"),
        root.join("rust-toolchain.toml"),
    ];
    let source = crates.join("victory-shader/src");
    println!("cargo:rerun-if-changed={}", source.display());
    files(&source, &mut all);
    all.sort();
    let mut hash = blake3::Hasher::new();
    for path in &all {
        println!("cargo:rerun-if-changed={}", path.display());
        // Names and line endings as they are in every checkout.
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let bytes: Vec<u8> = std::fs::read(path).unwrap_or_default().into_iter().filter(|&b| b != 13).collect();
        hash.update(name.as_bytes());
        hash.update(&(bytes.len() as u64).to_le_bytes());
        hash.update(&bytes);
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-env=VICTORY_TRANSLATOR={}", hash.finalize().to_hex());

    // D3D12 uses the runtime an executable names only if it is there: with the
    // exports but no D3D12Core.dll, device creation fails. So the tests export
    // them only when the runtime is staged; without it they use the in-box
    // runtime. The exports are a linker directive in src/agility.rs, which
    // only the unit-test executable has: a link argument from here would
    // reach the integration tests too, which lack the symbols.
    // OUT_DIR is <target>/<profile>/build/<package>-<hash>/out.
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let Some(profile) = out.ancestors().nth(3) else {
        return;
    };
    let core = profile.join("D3D12").join("D3D12Core.dll");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if msvc && core.exists() {
        println!("cargo::rerun-if-changed={}", core.display());
        println!("cargo::rustc-cfg=agility_runtime");
    } else {
        // Watching a missing file would rebuild the crate every time;
        // `cargo xtask agility` touches this file instead.
        println!("cargo::rerun-if-changed=build.rs");
    }
}
