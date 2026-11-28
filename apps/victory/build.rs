//! victory.exe exports `D3D12SDKVersion` and `D3D12SDKPath` (main.rs): Windows'
//! Direct3D 12 reads them from the process's executable to load Microsoft's
//! newer runtime from `D3D12\` beside it. An executable exports nothing by
//! itself, so the linker is told to.
//!
//! Direct3D 12 reads `D3D12SDKVersion` when `d3d12.dll` is loaded. Imported the
//! usual way, it is loaded before `main` runs, so the version `main` sets from
//! the `D3D12` folder (or Windows' own, without one) came too late and every
//! start asked for 614. Loaded on its first use instead, it reads that version.
//!
//! The commit the program is built from goes into it too (`victory --version`,
//! the launchers' log header): a run logged as one branch's once measured an
//! older build of another.
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/EXPORT:D3D12SDKVersion,DATA");
        println!("cargo:rustc-link-arg-bins=/EXPORT:D3D12SDKPath,DATA");
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:d3d12.dll");
        println!("cargo:rustc-link-arg-bins=delayimp.lib");
    }
    source();
}

/// The program's sources, as paths from the workspace's root: what a change
/// not committed is counted in, and what has this asked again. (The
/// launchers count the same: scripts\victory-build.ps1, src\play.rs.)
const SOURCES: [&str; 4] = ["apps", "crates", "Cargo.toml", "Cargo.lock"];

/// What `git` says in `root`, or nothing (no git, or not a checkout: a source archive).
fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(root).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// `VICTORY_BUILD_COMMIT` (the full hash, or `unknown`), `VICTORY_BUILD_DIRTY`
/// (`1` when the sources differed from it) and `VICTORY_BUILD_VERSION` (what
/// `--version` says after the name).
fn source() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let root = Path::new(&manifest).join("..").join("..");
    let git = |args: &[&str]| git(&root, args);
    let commit = git(&["rev-parse", "HEAD"]).filter(|c| c.len() == 40);
    // Tracked files of the sources only: an edited note or a stray log is not a change to the program.
    let mut status = vec!["status", "--porcelain", "--untracked-files=no", "--"];
    status.extend(SOURCES);
    let dirty = commit.is_some() && git(&status).is_some_and(|s| !s.is_empty());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let said = match &commit {
        Some(c) if dirty => format!("{version} (built from {c}, with changes not committed)"),
        Some(c) => format!("{version} (built from {c})"),
        None => format!("{version} (built from a commit not known)"),
    };
    println!("cargo:rustc-env=VICTORY_BUILD_COMMIT={}", commit.as_deref().unwrap_or("unknown"));
    println!("cargo:rustc-env=VICTORY_BUILD_DIRTY={}", u8::from(dirty));
    println!("cargo:rustc-env=VICTORY_BUILD_VERSION={said}");

    // Asked again when the commit or the sources change, not at every build: HEAD, the branch it
    // names (loose or packed) and the folders of sources. (A path that does not exist would have
    // this asked at every build, so only those that do are named.)
    let mut watched: Vec<PathBuf> = SOURCES.iter().map(|s| root.join(s)).collect();
    if commit.is_some() {
        let git_path = |p: &str| git(&["rev-parse", "--path-format=absolute", "--git-path", p]);
        watched.extend(git_path("HEAD").into_iter().map(Into::into));
        watched.extend(git_path("packed-refs").into_iter().map(Into::into));
        if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
            watched.extend(git_path(&branch).into_iter().map(Into::into));
        }
    }
    for path in watched.iter().filter(|p| p.exists()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}
