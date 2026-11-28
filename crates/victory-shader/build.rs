//! Fingerprint of the translator's own source, for the shader cache key
//! (`victory-gpu`'s `shader_cache`): a rebuilt executable whose translator did
//! not change keeps its cached translations. Also lists every `VICTORY_*`
//! switch the translator reads, whose values join the key at run time.
//! Reads only this crate's `src` folder; writes nothing but cargo directives.
//! Also sets `cfg(agility_runtime)` for the unit-test executable when the D3D12
//! runtime staged by `cargo xtask agility` is present.

use std::path::{Path, PathBuf};

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// FNV-1a, 64 bits, two lanes with different offsets: a mismatch only costs
/// a cache miss, so this need not be cryptographic.
fn fnv(state: &mut [u64; 2], bytes: &[u8]) {
    for &b in bytes {
        for (i, s) in state.iter_mut().enumerate() {
            *s ^= u64::from(b) ^ (i as u64 * 0x9e);
            *s = s.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

fn main() {
    println!("cargo::rustc-check-cfg=cfg(agility_runtime)");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    let mut list = Vec::new();
    files(&src, &mut list);
    list.sort();
    let mut state = [0xcbf2_9ce4_8422_2325u64, 0x8422_2325_cbf2_9ce4u64];
    let mut switches = std::collections::BTreeSet::new();
    for p in &list {
        let rel = p.strip_prefix(&src).unwrap_or(p).to_string_lossy().replace('\\', "/");
        let text = std::fs::read(p).unwrap_or_default();
        fnv(&mut state, rel.as_bytes());
        fnv(&mut state, &(text.len() as u64).to_le_bytes());
        fnv(&mut state, &text);
        let s = String::from_utf8_lossy(&text);
        let mut rest = &s[..];
        while let Some(at) = rest.find("\"VICTORY_") {
            let tail = &rest[at + 1..];
            let end = tail
                .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                .unwrap_or(tail.len());
            if tail[end..].starts_with('"') {
                switches.insert(tail[..end].to_string());
            }
            rest = &tail[end..];
        }
    }
    println!("cargo:rustc-env=VICTORY_SHADER_SOURCE_HASH={:016x}{:016x}", state[0], state[1]);
    let joined: Vec<_> = switches.into_iter().collect();
    println!("cargo:rustc-env=VICTORY_SHADER_SWITCHES={}", joined.join(","));

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
