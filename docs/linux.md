# Running on Linux (Wine), with DLSS and frame generation

VIctory is a Windows program, but it runs under Wine on Linux with an NVIDIA
card, DLSS Super Resolution and DLSS Frame Generation included. Tested on one
PC: Nobara 44, Ryzen 5 9600X, 64 GB RAM, **GeForce RTX 5070 Ti 16 GB**, NVIDIA
driver 615.78.08, wine-staging 11.22, vkd3d-proton and DXVK's `dxgi.dll` from
GE-Proton11-14, the release build of this repository, Series X profile,
`VICTORY_NATIVE_1440P=1`, the switches of `scripts/play-foxhound.ps1`.

| Same saved game, world frames | Median | p10 |
|---|---|---|
| no DLSS | 11.8 fps | 7.5 |
| DLSS Quality | 22.7 fps | 17.9 |
| DLSS Quality + frame generation 2x | 21.8 fps rendered, 42-44 shown | 20.1 |

## What you need

- Wine (tested: wine-staging 11.22) and a 64-bit prefix.
- vkd3d-proton's `d3d12.dll` / `d3d12core.dll` and DXVK's `dxgi.dll`, beside
  `victory.exe`, with `WINEDLLOVERRIDES="d3d12,d3d12core,dxgi=n"`. A recent
  GE-Proton has both (`files/lib/vkd3d-proton`, `files/lib/wine/dxvk`).
- `VKD3D_DISABLE_EXTENSIONS=VK_NV_raw_access_chains` on NVIDIA.
- For DLSS: the Streamline files that `Get-NVIDIA-DLSS.cmd` fetches (on Linux,
  download `streamline-sdk-v2.14.16.zip` from
  <https://github.com/NVIDIA-RTX/Streamline/releases> and copy the eight DLLs of
  its `bin/x64` that the script lists into a folder; name it with
  `VICTORY_STREAMLINE_DIR`, a Wine path such as `Z:\home\you\sl`).

## The pipeline library under Wine

Before this change an empty D3D12 pipeline library was asked for with
`&[]`, whose pointer is not null; vkd3d-proton refuses a non-null blob of
length 0 with `E_INVALIDARG`, so under Wine the log said `no host pipeline
library: pipelines are not kept` on every start, nothing was saved and every
pipeline was made again at each start (cutscenes with missing draws while
they were being made). It is now made from a null blob, as on Windows, and the
log says `host pipeline library saved`.

## DLSS: NVAPI in the prefix's System32

Streamline loads `nvapi64.dll` from System32 only. Beside `victory.exe` it is
not found and the log says `NVAPI failed to initialize` and `driver 0.0.0`.

1. `DXVK_ENABLE_NVAPI=1` (else DXVK's DXGI reports the card as AMD, and the
   layer also takes an AMD-only path in `indirect.rs`).
2. Copy dxvk-nvapi's `nvapi64.dll` and `nvofapi64.dll` (GE-Proton:
   `files/lib/wine/nvapi/x86_64-windows/`) into
   `$WINEPREFIX/drive_c/windows/system32/`, and add `nvapi64,nvofapi64` to the
   native overrides.
3. Copy the driver's NGX files (`/usr/lib64/nvidia/wine/nvngx.dll`,
   `_nvngx.dll`, `nvngx_dlssg.dll`; the folder differs by distribution) into
   the same System32, and tell NGX where they are:

   ```
   REGEDIT4

   [HKEY_LOCAL_MACHINE\SOFTWARE\NVIDIA Corporation\Global\NGXCore]
   "FullPath"="C:\\Windows\\System32"
   ```

   (`wine regedit ngx.reg`).

With these the log says `DLSS Super Resolution loaded ... ngx=310.9.16`, and
`DLSS per frame: 1.00 upscalings by DLSS`.

## Frame generation: hardware GPU scheduling

Streamline enables DLSS Frame Generation only when Windows says hardware GPU
scheduling is on. Wine 11.22 does not answer that question
(`D3DKMTQueryAdapterInfo` with `KMTQAITYPE_WDDM_2_7_CAPS` is not implemented),
so the log says `DLSS Frame Generation is not supported here: OS hardware
scheduling disabled`. Proton answers it; plain Wine needs a rebuilt
`win32u.so` (the Unix side only) with this case added to
`NtGdiDdDDIQueryAdapterInfo` in `dlls/win32u/d3dkmt.c`, from the same source
as the installed Wine:

```c
    case KMTQAITYPE_WDDM_2_7_CAPS:
    {
        UINT *value = desc->pPrivateDriverData; /* D3DKMT_WDDM_2_7_CAPS */

        if (desc->PrivateDriverDataSize < sizeof(*value))
            return STATUS_INVALID_PARAMETER;

        *value = 0x7; /* HwSchSupported | HwSchEnabled | HwSchEnabledByDefault */
        return STATUS_SUCCESS;
    }
```

`make dlls/win32u/win32u.so` in a configured tree builds it; copy it over
`lib64/wine/x86_64-unix/win32u.so` of a copy of your Wine (keep the
original). Then the log says `DLSS Frame Generation loaded` and, in play,
`DLSS Frame Generation: 22 presents with it on, 0 off, 44 frames shown by it`.

## Other things that matter on Linux

- **NVIDIA's shader disk cache** is capped at 1 GB by default and the layer's
  pipelines go past it: set `__GL_SHADER_DISK_CACHE_SIZE=12000000000` and
  `__GL_SHADER_DISK_CACHE_SKIP_CLEANUP=1`. If you give it its own
  `__GL_SHADER_DISK_CACHE_PATH`, create the folder first: the driver does not,
  and then keeps nothing.
- **`victory warm`** with several processes loses entries of that cache when
  they write it at once; `victory warm --used --jobs 1` keeps them (a second
  run should then take about 1 ms a pipeline).
- **wineserver**: the layer maps around 100,000 views, and wineserver's view
  list is a linked list walked on each map and unmap (Wine bug 60868). A
  wineserver with the view-tree patch from that bug (CachyOS carries it) takes
  it from about 85% of a core to under 2%.
- **Mouse**: `VICTORY_MOUSE_CAPTURE=1` at start (the F1 menu's MOUSE CAPTURE
  needs a restart, and the menu restarts through the PowerShell launcher).

## A launch line

From the folder with `victory.exe` and the DLLs above, with the switches of
`scripts/play-foxhound.ps1` exported as environment variables:

```sh
export WINEPREFIX=$HOME/.local/share/victory-wine DXVK_ENABLE_NVAPI=1 \
  WINEDLLOVERRIDES="d3d12,d3d12core,dxgi,nvapi64,nvofapi64=n" \
  VKD3D_DISABLE_EXTENSIONS=VK_NV_raw_access_chains \
  __GL_SHADER_DISK_CACHE_SIZE=12000000000 __GL_SHADER_DISK_CACHE_SKIP_CLEANUP=1 \
  VICTORY_GAME_DIR='Z:\path\to\your\dump' VICTORY_CACHE_DIR='Z:\path\to\cache' \
  VICTORY_NATIVE_1440P=1 VICTORY_MOUSE_CAPTURE=1 VICTORY_FRAME_INTERVAL_US=1000 \
  VICTORY_DLSS=1 VICTORY_DLSS_MODE=quality VICTORY_STREAMLINE_DIR='Z:\path\to\sl' \
  VICTORY_FRAMEGEN=1 VICTORY_FRAMEGEN_X=2 VICTORY_TEXTURE_ROOM_PERCENT=80
wine victory.exe run 'Z:\path\to\your\dump\game_scarlett_master.exe' --console series-x --execute
```
