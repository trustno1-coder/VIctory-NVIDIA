# FSR 3 bridge (`fsr3_bridge.dll`)

The only place in this repository that knows AMD's FidelityFX API
structs. Rust (`crates/victory-gpu/src/fsr3.rs`) talks to it over the
minimal C ABI in `fsr3_bridge.h` — opaque pointers and scalars only —
so no AMD struct layout is ever hand-mirrored in Rust.

## What it does

Per GPUOpen's FSR 3.1 docs ("AMD FSR Frame Generation API",
"FSR 3.1 Integration Overview"):

1. Loads AMD's signed DLL at run time
   (`amd_fidelityfx_dx12.dll`, SDK 2.0 fallback
   `amd_fidelityfx_loader_dx12.dll`) and resolves the five `ffx*`
   functions with `GetProcAddress`.
2. `fsr3_fg_create`: frame-generation context
   (`ffxCreateContextDescFrameGeneration` + backend desc, version
   struct chained when the SDK provides it).
3. `fsr3_fg_wrap_swapchain`: wraps the swap chain in FSR 3's
   frame-interpolation swap chain
   (`ffxCreateContextDescFrameGenerationSwapChainWrapDX12`).
4. `fsr3_fg_prepare`: prepare pass
   (`ffxDispatchDescFrameGenerationPrepareV2`) from depth + motion
   vectors, recorded into the caller's command list.
5. `fsr3_fg_configure`: per-frame configure
   (`ffxConfigureDescFrameGeneration`), with the frame-generation
   callback (dispatches interpolation during Present) and the
   present callback (copies the back buffer, UI included, to the
   output buffer).
6. `fsr3_fg_recreate`: on a resize, remakes only the
   frame-generation context at the new size, in place — the wrapped
   swap chain stays alive (its swapchain context's callbacks point
   at the same state), because the host `ResizeBuffers` it and
   keeps presenting through it.

## Building

Do not build by hand unless you need to — `Get-FSR3.cmd` in the
repository root does the whole thing: fetches FidelityFX SDK 1.1.4
(FSR 3.1), builds this bridge with MSVC/CMake, copies
`fsr3_bridge.dll` and AMD's signed DLL into `fsr3-sdk\`.

Manual build (Windows, MSVC, CMake, SDK checkout at
`E:\sdk\FidelityFX-SDK`):

```powershell
cmake -S tools\fsr3-bridge -B build\fsr3-bridge -DFSR3_SDK_DIR=E:\sdk\FidelityFX-SDK
cmake --build build\fsr3-bridge --config Release
copy build\fsr3-bridge\Release\fsr3_bridge.dll fsr3-sdk\
```

If the build fails on a struct field or macro name, that is the
system working: the SDK headers changed, and the bridge must be
updated to the SDK's documented layout — do not rename fields to
make it compile against a different SDK version without checking
the GPUOpen docs for that version.

## Known approximations

- Camera vectors are (0,0,0) / up (0,1,0) / right (1,0,0) /
  forward (0,0,-1): the title gives no camera matrices, same as
  DLSS in this layer.
- UI is copied as-is in the present callback; `HUDLessColor` (the
  scene texture) is passed so FSR 3 can tell UI from scene. Close
  the F1 menu while frame generation is on.
- SDR only, async workloads off, 2x only.
