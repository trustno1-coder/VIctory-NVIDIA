# Repository Guidelines

## Project Overview

VIctory-NVIDIA runs an Xbox Series X|S title's **own x86-64 executable natively on Windows**: no CPU emulation. The title image is mapped into our process, its console imports are bound to our implementations, its D3D12.X calls become host D3D12, and its RDNA2 shader binaries are translated to PC DXIL. This checkout is the NVIDIA fork of upstream VIctory (DLSS, DLSS frame generation, AMD FSR 3 frame generation, fork ray tracing).

Rule #1, before anything else: **nothing from the game is ever committed** — no assets, no executable bytes, no disassembly, no shader dumps, no screenshots (`docs/asset-policy.md:3-15`). The repository is text-only, enforced statically by `cargo xtask policy` and at run time by `victory_core::ensure_outside_repo`.

## Architecture & Data Flow

```
game executable ─► victory-loader ──► title code runs natively (x86-64)
                   prepare/map/bind        │ imports
                                           ▼
                                      victory-hle ──► host Windows DLLs (HostPolicy)
                                           │ D3D12.X
                                           ▼
                                      victory-gpu ──► host D3D12 (+ Streamline/DLSS, FSR 3)
                                           │ shader bytes at pipeline creation
                                           ▼
                                      victory-shader ◄─► cache_dir (content-addressed)
```

- `victory run --execute` (`apps/victory/src/run.rs:106`): CPU check → `Config::load` → `streamline::Asked::from_env` → `victory_loader::prepare` (rewrites the PE: clears `FORCE_INTEGRITY`, zeroes the checksum, neuters the TLS callback and import directory) → `load` (map via `LoadLibraryExW`) → `bind_imports` → `write_sites::apply_asked` → crash reporter → `spawn_main`.
- **Import resolution order** (`docs/architecture.md`, "Import resolution"): our `victory-hle` registry wins → host DLL if `HostPolicy` allows it → a DLL the title ships → generated stub (`--unimplemented trap` breaks and aborts, `zero` logs once and returns 0).
- **Console objects are not COM objects.** A "pointer" the title holds is a Rust struct whose first field is a `Table<N>` of raw `extern "system"` fn pointers; `QueryInterface` goes through `object::answer` (`crates/victory-gpu/src/object.rs`). Layouts are `repr(C)` with `const _: () = assert!(size_of::<T>() == N)` checks, and fields the title's inlined code reads stay zero on purpose (`List::put`, `List::reserved`: "all zero, so every inline check fails").
- **Shaders are translated when the title creates a pipeline**, from the bytes it passes, not from files (`docs/architecture.md:24-26`). `pipeline.rs` hashes the container (blake3) → `shader_cache` → `victory_shader::emit::translate`. Offline `victory translate`/`warm` only pre-fill the same cache.
- Shader stages: `Container::parse` → `native_programs` (XDAT/XBCH/FSSM, zlib) → `gfx10::decode` → `ir::build` (SSA) → `structure` (removes EXEC: lane masks become per-lane booleans) → `resource::track` (descriptor → root parameter) → `io::raise` → `emit::lower` → DXIL container + optional `dxil.dll` validation.
- Present path (`crates/victory-gpu/src/host.rs:10219`): `present_planes` → swap chain (FSR-3 / Streamline proxy / plain) → Series-S two-plane compose → `streamline::present_*` → F3 panel and F1 menu drawn onto the back buffer.

## Key Directories

| Path | Purpose |
|---|---|
| `apps/victory` | the `victory` CLI: `run`, `play`, `census`, `inspect`, `translate`, `warm`, `preload`, `replay`, `get-*` |
| `crates/victory-core` | `Config`, `logging::init`, `ensure_outside_repo` |
| `crates/victory-pe` | read-only PE32+/COFF import-library parsing (patching lives in the loader) |
| `crates/victory-loader` | prepare/map/bind/start the title; TLS redirect, threads, crash reporter, Zen 2-only instruction emulation |
| `crates/victory-hle` | console API surface, one module per DLL under `src/modules/` |
| `crates/victory-gpu` | 117K lines, the bulk: object model + command path, and `host*`/`streamline*`/`replay*` runtime |
| `crates/victory-shader` | 51K lines: GFX10 → PC DXIL translator |
| `crates/victory-formats` | texture layouts, BCPack, XMA — **zero dependencies**, platform-level |
| `crates/victory-pipeline` | `Converter` trait + content-addressed `Cache` |
| `crates/victory-game` | the only title-specific crate (build identities, SGD5) |
| `xtask`, `.githooks` | asset policy, git hooks |
| `scripts/`, `*.cmd` | PowerShell/cmd launchers and vendor-SDK fetchers |

## Development Commands

```bash
cargo clippy --workspace --all-targets --locked -- -D warnings      # CI gate 1
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings   # CI gate 2
cargo test --workspace --locked                                     # CI gate 3 (no game data needed)

cargo build --release --locked -p victory                           # target\release\victory.exe
cargo build --locked --profile diag -p victory --features diagnostics   # target\diag\victory.exe

cargo xtask policy            # asset policy over the tree
cargo xtask policy --staged   # what the pre-commit hook runs
cargo xtask install-hooks     # git config core.hooksPath .githooks
```

Narrower checks while iterating: `cargo test -p victory-shader --lib`, `cargo test -p victory-gpu`, `cargo clippy -p victory-gpu --all-targets -- -D warnings`. There is **no `cargo fmt` gate** anywhere (CI, hook, docs), and the tree is far from rustfmt-clean: `cargo fmt --all -- --check` reports 3777 diff hunks, starting in `apps/victory/src/main.rs:73`. Never run `cargo fmt` here, and do not reformat lines you did not otherwise change.

Running a title: `victory run <exe> --console series-s|series-x --execute`, or the launchers `scripts\play-foxhound.ps1 -Exe <exe>` (`play-nvidia.ps1` is the same script under its old name) and `PLAY_FRAMEGEN.cmd`.

## Code Conventions & Common Patterns

- **Module docs explain *why*, with evidence.** 231 of 238 tracked `.rs` files open with a `//!` essay citing measured runs, dates, hardware, issue numbers and upstream commits (`(VIctory upstream 0b35b7f)`, `(From fxceless/victory-fork c36971b2.)`). Keep that voice; `shortcut:` tags (170 in-tree) mark deliberate simplifications. Spelling leans British in prose (`colour` 186 vs `color` 132 occurrences, the latter mostly in API identifiers like `Target::Color`).
- **Errors by layer.** Binaries and the pipeline use `anyhow::Result` with `.with_context(|| format!("reading {}", path.display()))`; leaf crates define `thiserror` enums (`PeError`, `PolicyError`, `ShaderError::{NotDxbc, Malformed, Isa, Unsupported}`); HLE and GPU code facing the title return **`HRESULT` `i32`** (`S_OK`, `E_NOTIMPL`) and otherwise `tracing::warn!` and degrade. A fatal GPU loss goes through `host::fail` → `tracing::error!` → `abort()` or the hook given to `on_failure`. Do not `unwrap()` on a title thread; poisoned mutexes are recovered with `.unwrap_or_else(|e| e.into_inner())` (620 uses in `victory-gpu`).
- **Logging** is `tracing` only (`info!`/`warn!`/`debug!` with structured fields such as `hr = format_args!("{:#010x}", hr)`); title output goes to `target: "title"`. `victory_core::logging::init` reads `VICTORY_LOG`, falling back to `RUST_LOG`; the level is capped at INFO unless the `diagnostics` feature is on (`crates/victory-core/src/logging.rs:16-28`).
- **Env switches are the configuration system.** 482 distinct `VICTORY_*` names exist (`grep -rhoE 'VICTORY_[A-Z0-9_]+' crates apps --include='*.rs' | sort -u | wc -l`), 389 in `victory-gpu` alone; there is no central registry, only the F1 menu's curated `ITEMS` (`crates/victory-gpu/src/host/menu.rs`) and the table in `docs/dlss.md`. Read them through the established helpers, never ad hoc: `toggled!("VICTORY_X")` = on unless the value is exactly `"0"`; `asked_for!("VICTORY_X")` = on only when `"1"` (`crates/victory-gpu/src/switch.rs:5-69`). Values are cached in a `OnceLock` and read **once**, so a knob cannot change mid-run except via `VICTORY_AB` or the two `live: true` menu items.
- **No async runtime anywhere.** `std::thread::Builder` with named threads (`victory-relay`, `victory-window`, `victory-heap-sync`, `victory-pipelines-{i}`, `victory-list-{i}`), `Mutex`/`Condvar`/`mpsc`/`OnceLock`/atomics. Async-shaped console APIs are implemented on top of threads: `xasync::pending_with` spawns a thread and completes the `XOVERLAPPED` call it began.
- **No dependency injection**: crates call each other directly (`crates/victory-hle/src/modules/d3d12x.rs` does `use victory_gpu::device;` and calls `device::create`), and process-wide state lives in `static` `OnceLock`/`Mutex`/`RwLock` (`DEVICE`, `RETIRED`, `SHADER_HEAPS`).
- **Unsafe is pervasive and documented**: 2444 `// SAFETY:` comments for ~2461 `unsafe` blocks. `unsafe_op_in_unsafe_fn` and `unused_must_use` are **denied** workspace-wide; `undocumented_unsafe_blocks` is allowed because, as `Cargo.toml` records, eleven blocks in the fork's newer GPU code still lack a comment — do not add a twelfth.
- **Naming**: crates `victory-*`, files single-word snake_case (`descriptor.rs`, `copystat.rs`); a module with children is either `foo.rs` + `foo/` (`host`, `pipeline`, `streamline`) or `foo/mod.rs` (`replay`, `emit`, `gfx10`, `hle`'s `modules`) — follow the directory you are in; `extern "system"` for Win32-shaped functions and `extern "C"` for CRT ones (`__p___argc`); `SCREAMING_CASE` consts; test functions as snake_case sentences without a `test_` prefix (`loads_binds_and_calls_probe`, `rejects_paths_inside_this_repo`).
- **Adding a console import** (`crates/victory-hle/src/modules/mod.rs:3-5`): write a `pub extern "system" fn` named exactly like the import, then list it with `hle_export!(Name)` in that module's `exports` in its `module()` function; `HleModule::serves` matches DLL names, with a trailing `*` as a prefix (API sets), and ordinals resolve as `"#42"`. A new module also needs its `pub mod` line and a place in `all()`. `victory census` lists what is still missing; registering nothing is what makes an import fall through to the host, a title DLL, or a stub.

## Important Files

| File | Why it matters |
|---|---|
| `docs/architecture.md` | the authoritative 99-line overview; read it before any cross-crate change |
| `docs/asset-policy.md`, `xtask/src/policy.rs` | what may exist in the repo — the policy source is the authority |
| `apps/victory/src/main.rs` | the clap `Command` enum (`:198`); doc comments are the `--help` text |
| `crates/victory-core/src/config.rs` | `Config::load`: env > `victory.toml` (`VICTORY_CONFIG`) > defaults |
| `crates/victory-gpu/src/switch.rs` | `toggled!`/`asked_for!`, the only correct way to read a switch |
| `crates/victory-gpu/src/host.rs` | the one host device, queue, window, submission and present path |
| `crates/victory-shader/src/emit/mod.rs` | `translate`, `emit`, `translation_fingerprint` |
| `victory.example.toml` | copy to `victory.toml` (git-ignored); every path must be outside the repo |
| `.github/workflows/ci.yml` | the full definition of "green" |

## Runtime/Tooling Preferences

- **Rust, pinned.** `rust-toolchain.toml` pins `1.98.1` with target `x86_64-pc-windows-msvc` and components rustfmt + clippy; `rust-version = 1.88` is the declared MSRV. Workspace `resolver = "3"`, `edition = "2024"` — 2024 syntax is in use (`#[unsafe(no_mangle)]`, `unsafe extern`, let-chains).
- **Windows-only in practice.** Nothing fails to compile elsewhere (there is no `compile_error!`), but `cfg(not(windows))` paths are stubs and CI only runs `windows-latest`. Never claim Linux support from green CI; `docs/linux-proton.md` covers cross-building with `cargo xwin` plus Proton.
- **Cargo only; `--locked` everywhere.** Commit `Cargo.lock` with any dependency change. Note that the lockfile is hashed into `VICTORY_TRANSLATOR` (`crates/victory-gpu/build.rs:24-28`), so churning it invalidates every user's shader cache.
- **`cargo xtask` is the repo tool** (`.cargo/config.toml` aliases it to `run --quiet --package xtask --`).
- **Vendor SDKs are never in the tree.** `Get-NVIDIA-DLSS.cmd` (Streamline, `nvngx_dlss.dll`, `sl.interposer.dll` → `nvidia-sdk\`), `Get-AMD-FSR.cmd` (FidelityFX → `amd-sdk\`) and `victory get-d3d12` (Agility SDK → `D3D12\` beside the exe) fetch them at run time. They are not needed to build.
- **Line endings**: `*.ps1` and `*.cmd` are CRLF, everything else LF (`.gitattributes`).

## Testing & QA

- Plain `cargo test` with the built-in harness: **1233 `#[test]` functions**, no tokio, criterion, proptest or snapshot tooling; mostly inline `#[cfg(test)] mod tests` beside the code, plus ten sibling `*_tests.rs` files in `crates/victory-shader/src/emit/` included as `#[cfg(all(test, windows))] mod valu_tests;`.
- Integration tests: `crates/victory-loader/tests/{load_probe,title_dlls}.rs`, `crates/victory-hle/tests/no_dll_planting.rs`, `crates/victory-gpu/tests/replay{,_big}.rs`, `apps/victory/tests/census.rs`. `load_probe` builds `tests/fixtures/hle-probe` (a `cdylib` standalone crate, excluded from the workspace) by shelling out to `$CARGO` with its own `--target-dir`, because the outer cargo holds the lock on the main one.
- **The suite must pass on a bare runner**: no game data, no GPU, no fixtures. Tests that need real files take paths from env vars and return early when unset; tests needing the adapter return early when `host()` is `None`, WARP ones when `WarpHarness::new()` is `None`. Never add a test that needs a title, and never commit reference data: a golden-style test pins its `(len, hash)` pairs in source (`crates/victory-shader/src/emit/bitcode.rs:1248-1257`).
- `#[ignore]` means "needs a rig", and the reason is the runbook: `#[ignore = "benchmark"]`, `#[ignore = "a measurement, not a check"]`, `#[ignore = "wants VICTORY_RELAY=2 VICTORY_RELAY_COPY=1 and a process of its own"]`. Run them as `cargo test --release -p <crate> <name> -- --ignored --nocapture`. The only `[[bench]]` target is `cargo bench -p victory-shader` (`guard_runs`, `harness = false`, not run by CI).
- **Coverage trap**: `cargo test --workspace` never compiles `crates/victory-gpu/tests/replay*.rs` — they are `#![cfg(all(windows, feature = "diagnostics"))]`, and CI only type-checks them in the `--all-features` clippy step. Use `cargo test -p victory-gpu --features diagnostics` when touching replay.
- Before committing, assume `cargo xtask policy --staged` will run: a new `.bin`, `.png`, `.log` or any file over 640 KiB is refused, as is any long hex/base64 run. Allowed extensions are listed in `xtask/src/policy.rs:17-19`; the single exception in the tree is `crates/victory-formats/src/bcpack-tables.bin`, and `docs/asset-policy.md` says nothing else of that kind is accepted. A violation that lands needs a history rewrite, not a follow-up commit.
