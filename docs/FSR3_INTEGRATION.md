# FSR 3 Frame Generation for VIctory (RTX 30-series focus)

## Why this exists

NVIDIA DLSS Frame Generation (Streamline / `sl.dlss_g`) is limited to RTX 40-series and newer.
An RTX 3080 Ti (Ampere) cannot use it. AMD FSR 3 Frame Generation works on Ampere (and most other GPUs)
because it is a software optical-flow + interpolation solution with a proxy swap chain for pacing.

This document describes what is actually built (branch `fsr3-framegen-3080ti`), not a plan.
Milestone: **frame generation only, 2x, with the title's own upscaling**. DLSS is untouched.

## What FSR 3 actually is (the scaffold got this wrong)

- FSR 3.0 has **no prebuilt DLL**: it is source-only C++ in the FidelityFX SDK.
  There is no `ffx_fsr3.dll` to load. Do not look for one.
- FSR **3.1** is the FidelityFX API: **one prebuilt signed DLL, `amd_fidelityfx_dx12.dll`**,
  in the SDK's `/PrebuiltSignedDLL`, exposing exactly five functions
  (`ffxCreateContext`, `ffxDestroyContext`, `ffxDispatch`, `ffxQuery`, `ffxConfigure`).
  Upscaling and frame generation are separate contexts; frame generation additionally needs
  a frame-generation swap-chain context, a per-frame prepare dispatch, `ffxConfigure` for FG,
  and a UI-composition callback. FG works with any upscaling technology, including the title's own.
- SDK 2.0 (FSR 4) splits the API into several DLLs (`amd_fidelityfx_loader_dx12.dll`,
  `amd_fidelityfx_framegeneration_dx12.dll`, `amd_fidelityfx_upscaler_dx12.dll`).
  **Do not target 2.0 first.** This work pins SDK **1.1.4** (FSR 3.1).

## Architecture: Rust talks to a C++ bridge, never to AMD's structs

Rust does **not** hand-mirror AMD's `ffx-api` structs. Guessing at that ABI would corrupt memory
silently; instead all AMD structs and macros live only in the bridge, compiled on Windows against
the real SDK 1.1.4 headers, so a wrong name fails the build loudly instead of at run time.

```
Title ffxDispatch (upscale)
        │
        ▼
fidelityfx.rs interceptor
        │
        ├── VICTORY_DLSS / VICTORY_FRAMEGEN → existing Streamline path (untouched)
        │
        └── VICTORY_FSR3_FG → fsr3.rs (FG-only: notes depth/MV/camera, title's upscaler runs)
                        │
                        ▼
                fsr3_bridge.dll (C++ bridge, tools/fsr3-bridge)
                        │
                        ▼
                amd_fidelityfx_dx12.dll (AMD, FSR 3.1 API)
                        │
                        ▼
                FSR frame-generation swap chain (wraps the host's plain swap chain)
```

| What | Where | Notes |
|------|-------|-------|
| Bridge loader + state | `crates/victory-gpu/src/fsr3.rs` | Loads `fsr3_bridge.dll` only (never any AMD DLL directly), over a C ABI defined in `tools/fsr3-bridge/fsr3_bridge.h` (`BRIDGE_ABI_VERSION = 1`). |
| C++ bridge | `tools/fsr3-bridge/` | `fsr3_bridge.cpp` + `CMakeLists.txt`. Loads `amd_fidelityfx_dx12.dll` from its own folder, creates the FG context, wraps the swap chain, prepare/configure per frame. |
| Interceptor | `crates/victory-hle/src/modules/fidelityfx.rs` | Interposes only when `streamline::serving() \|\| fsr3::serving()`. FSR path notes the upscale and returns the title's own upscaler; the `ffxCreateContext` size clamp stays DLSS-only (clamping would break the title's own upscaler that FG-only relies on). |
| Present path | `crates/victory-gpu/src/host.rs` | `presenter()` wraps the plain swap chain via `fsr3::wrap_swapchain` when Streamline did not proxy and FSR is serving; `present_planes()` runs `fsr3::present_begins` / `present_ends` around Streamline's. Every failure falls back to the plain swap chain and the title's own upscaling. |
| Init | `apps/victory/src/run.rs` | `fsr3::init()` next to `streamline::init()`. Refuses to serve if Streamline is serving (mutual exclusion in code, not just launchers). |
| Launchers | `scripts/play-foxhound.ps1`, `apps/victory/src/play.rs` | `-Fsr3FrameGen` / `victory play --fsr3-framegen` / `victory get-fsr3`. DLSS FG wins if both are asked; FSR FG-only forces DLSS off for the run. |
| F1 menu | `crates/victory-gpu/src/host/menu.rs` | `VICTORY_MENU_FSR3_FG`: OFF / 2X, live when `fsr3::framegen_live()`. |
| Fetcher | `scripts/get-fsr3.ps1`, `Get-FSR3.cmd` | Downloads SDK 1.1.4, builds the bridge, copies both DLLs into `fsr3-sdk\`. Fails loudly if the tag does not exist or the AMD DLL is not in the archive. |

## Getting it (one-time, on Windows)

```
Get-FSR3.cmd
```
or `victory get-fsr3`. This downloads the FidelityFX SDK 1.1.4 source archive, builds
`tools/fsr3-bridge` with CMake/MSVC against it, and puts `fsr3_bridge.dll` and
`amd_fidelityfx_dx12.dll` into `fsr3-sdk\` (git-ignored; nothing AMD is committed, same policy
as Streamline). CMake and Visual Studio with C++ are needed for the one-off build.
`-SdkTag` names another SDK tag, but a 2.0+ SDK is not what the bridge expects and the build
step says so. A pre-downloaded SDK checkout can be passed as `-SdkDir`.

## Running it

```
.\scripts\play-foxhound.ps1 -Exe '<path>\game_scarlett_master.exe' -Fsr3FrameGen
```
or `victory play --fsr3-framegen` (after `victory get-fsr3`). `VICTORY_FSR3_FG=1` and
`VICTORY_FSR3_DIR` are set by the launcher; `VICTORY_TEXTURE_ROOM_PERCENT=80` reserves a fifth
of the texture budget for frame generation (same value DLSS FG uses; an estimate until measured
on 12 GB). The F1 menu can turn it on/off live (`FSR 3 FRAME GENERATION`, 2X).
`VICTORY_FSR3_QUALITY` (quality/balanced/performance/dlaa) is read for the FG context's render
size; `VICTORY_FSR3` (upscaling) is accepted and logged as not implemented — this milestone is
FG-only. Never set `VICTORY_FRAMEGEN` and `VICTORY_FSR3_FG` together.

## What is handed to FSR 3 per frame

From the existing `DispatchUpscale` layout in `fidelityfx.rs` (already parsed for DLSS):

- Color buffer (prefer hudless / without interface plane when available)
- Depth buffer, motion vectors, exposure / pre-exposure
- Jitter, motion-vector scale, render size, output size
- Camera near/far/FOV, frame time delta, reset
- `frameID` increments by exactly 1 each frame (the API requires it)

## Known approximations (in the bridge header — read them before debugging)

- Camera vectors are position (0,0,0), up (0,1,0), right (1,0,0), forward (0,0,-1):
  the title gives no camera matrices (same as DLSS here).
- UI is copied as-is, with the scene texture passed as HUDLessColor: **close the F1 menu
  while FG is on**, or the menu lands in interpolated frames.
- SDR only; async workloads off.
- `shown_per_frame()` reports the asked count (2), not a measured count.

## Testing checklist (must be done on real hardware + title; none of it is checkable on Linux)

- [ ] `Get-FSR3.cmd` builds the bridge against SDK 1.1.4 headers (macro/type names in the bridge are compile-gated: a wrong one fails here, loudly)
- [ ] `amd_fidelityfx_dx12.dll` loads and the FG context creates on a 3080 Ti (optical-flow support, driver, HAGS)
- [ ] Color / depth / MV correctly bound (no black, frozen, or warped frames)
- [ ] Frame generation produces ~2x visual rate while input latency stays at the real render rate
- [ ] No crashes on scene cuts, menu transitions, resolution changes, or fullscreen switches
- [ ] Coexistence: DLSS path still works when FSR 3 env vars are off; DLSS FG wins if both are asked
- [ ] VRAM headroom on 12 GB (`VICTORY_TEXTURE_ROOM_PERCENT=80` is an estimate until measured)
- [ ] F1 live toggle works; `victory fsr3-check`-style standalone check is still to be added
      (`streamline::check()` is Streamline-specific)

## Licensing

FidelityFX SDK source is MIT (see GPUOpen). Keep the "download at runtime / do not commit binaries"
policy that the repo already uses for NVIDIA Streamline.

## Suggested next milestone

FSR 3 upscaling (`VICTORY_FSR3`) as a separate FSR context, after FG-only is proven in-game.
