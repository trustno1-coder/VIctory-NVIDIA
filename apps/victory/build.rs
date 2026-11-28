//! victory.exe exports `D3D12SDKVersion` and `D3D12SDKPath` (main.rs): Windows'
//! Direct3D 12 reads them from the process's executable to load Microsoft's
//! newer runtime from `D3D12\` beside it. An executable exports nothing by
//! itself, so the linker is told to.
//!
//! Direct3D 12 reads `D3D12SDKVersion` when `d3d12.dll` is loaded. Imported the
//! usual way, it is loaded before `main` runs, so the version `main` sets from
//! the `D3D12` folder (or Windows' own, without one) came too late and every
//! start asked for 614. Loaded on its first use instead, it reads that version.
fn main() {
    // The commit this build is of, for the log's first lines (a launcher that
    // builds the repository by itself says nothing of which one it built).
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "--short=10", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=VICTORY_COMMIT={commit}");
    for git in ["../../.git/HEAD", "../../.git/refs/heads", "../../.git/packed-refs"] {
        println!("cargo:rerun-if-changed={git}");
    }
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/EXPORT:D3D12SDKVersion,DATA");
        println!("cargo:rustc-link-arg-bins=/EXPORT:D3D12SDKPath,DATA");
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:d3d12.dll");
        println!("cargo:rustc-link-arg-bins=delayimp.lib");
    }
}
