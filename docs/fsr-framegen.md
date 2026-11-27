# FSR 3.1 frame generation (prototype)

Select FSR or DLSS frame generation independently of DLSS upscaling.
The default remains DLSS. FSR uses AMD's FidelityFX SDK v1.1.4 runtime
(FSR 3.1.4), including its D3D12 interpolation swap chain.

## Install and run

```powershell
.\Get-AMD-FSR.cmd
cargo build --release --locked -p victory
.\scripts\play-foxhound.ps1 -Exe 'D:\YourGame\game_scarlett_master.exe' -FrameGen -FrameGenBackend fsr
```

Keep NVIDIA's runtime installed to combine DLSS upscaling with FSR frame
generation. The FSR runtime is downloaded from AMD's official repository,
verified against a pinned SHA-256, and placed in `amd-sdk`. To use another
location, set `VICTORY_FSR_DIR`.

The F1 menu has **FRAME GEN BACKEND (RESTART)**. Changing between FSR and DLSS
requires restarting because each owns a different swap chain. Off/on changes
remain live. FSR offers off/2x; DLSS retains its existing 2–6x choices, subject
to GPU/driver support. Saved settings override launcher arguments as before.

| Environment variable | Meaning |
| --- | --- |
| `VICTORY_FRAMEGEN_BACKEND=dlss\|fsr` | Backend; omitted means DLSS |
| `VICTORY_FRAMEGEN=1` | Make the selected backend available |
| `VICTORY_FRAMEGEN_X=0\|2` | FSR ready/off or 2x; larger values are treated as 2x |
| `VICTORY_FSR_DIR` | Folder containing `amd_fidelityfx_dx12.dll` |
| `VICTORY_DLSS=1` | DLSS upscaling, independently of the FG backend |

Game integration requires `VICTORY_THIN=1` or `VICTORY_ASYNC_SUBMIT=1`;
the launcher sets the former. Without DLSS upscaling, the title's own
FidelityFX output-size restrictions still apply (see [dlss.md](dlss.md)).

## Standalone GPU check

```powershell
$env:VICTORY_FRAMEGEN_BACKEND = 'fsr'
.\target\release\victory.exe upscaler-check --present --output 1280x720
```

This does not require the title or NVIDIA's runtime. It initializes depth,
motion and animated colour textures, presents 120 frames, toggles generation,
omits inputs to simulate menus/loading, exercises HUD-less extraction and
two-plane UI composition, resizes the swap chain, and tears it down.
Success requires generated-frame dispatches, no SDK errors and a healthy
D3D12 device, plus presentation callbacks for every real/generated frame.
Set `VICTORY_FSR_CHECK_FRAME_MS=80` or `125` to exercise slow rendering and
capture the standalone window with PresentMon. The zero motion vectors make this a lifecycle/dispatch check,
not an image-quality benchmark. Read the final PASS/FAIL line and the log.

Validated on an RTX 3070 8 GB, driver 610.88: 120 presents and 100
generated-frame dispatches, zero SDK errors and zero D3D12 debug-layer errors,
including both UI paths, toggling, missing inputs, resize and teardown.
In-game image quality still needs broader validation; display-pacing results follow.

The pacing revision also passed that lifecycle check with 120 real and 100
generated presentation callbacks and zero D3D12 error/corruption messages.
At a simulated 125 ms render interval, PresentMon captured 218 displayed presents
with no missing display timings: median interval 66.1 ms, 10th percentile 56.7 ms.
Only five intervals were below 17 ms (startup/off-on transitions); the test also
includes deliberately ungenerated sections. This validates slow-frame spacing in
the standalone test, not the final game's image quality or pacing.

An in-game 30-second capture after this revision recorded 906 presents, all with
display timing: 30.28 displayed FPS, 33.29 ms median interval, 22.80/41.72 ms
10th/90th percentiles and 62.69 ms 99th percentile. The user also reported visibly
improved smoothness. The earlier FG-on capture recorded 18.31 displayed FPS,
74.99 ms median and 75/623 presents without display timing, with repeated
~12 ms/~85 ms pairs. These are separate gameplay runs, not a controlled rendering
throughput benchmark; the observed pacing improvement is the relevant result.

## Measuring actual display pacing

The overlay's FSR rate is a dispatch-based estimate. To check whether generated
frames reach the display at useful intervals, capture Windows display events with
the official [PresentMon console tool](https://github.com/GameTechDev/PresentMon/releases).
The capture script is intended for PresentMon 2.6.0.

With the game already in a repeatable scene, turn FG off, close the menu, and run:

```powershell
.\scripts\measure-framegen.ps1 -PresentMon 'C:\Tools\PresentMon-2.6.0-x64.exe' -Mode off
```

Then enable FG and repeat with `-Mode on`. Each capture waits five seconds for
you to foreground the game, then records 30 seconds. Repeat the same steady camera
pan; keep resolution, display mode, frame cap and other settings unchanged.
`-Mode` labels the capture; it does not change the game setting.

CSV files and phase metadata go to `%LOCALAPPDATA%\victory\logs\display-timing`.
The Python summary reports each swapchain/display plane separately: observed
display-duration rate, median and tail durations, and missing/zero durations.
Preserve all rows, including dropped frames. A doubled dispatch counter with
unchanged displayed cadence, or alternating very short/long durations, is not
evidence of evenly paced 2x output. ETW measures presentation timing, not whether
interpolated images have correct motion.

## Prototype scope

- SDR RGBA8/BGRA8 output, one interpolated frame per rendered frame.
- Uses the same submitted depth/motion tracking and HUD-less capture as DLSS.
  The SDK extracts UI from the composited back buffer and HUD-less colour.
- Interpolation is submitted on the game queue (async compute is disabled), so
  later game submissions are ordered after its input reads. Resource references
  remain held until the next prepare drains outstanding presents; resize,
  reconfiguration and teardown also drain before destroying resources.
- Does not drain presentations immediately after `Present`: the next game frame
  can render while the SDK presenter spaces the previous generated/real pair.
  A presentation callback copies the SDK's already-composited colour and delays
  the real partner by half the observed generated-pair interval. Off/loading
  frames without a generated partner are not delayed.
- The callback replaces the SDK v1.1.4 timing average, which resets on intervals
  over 100 ms and requires ten samples to warm up again. SDK pacing is suppressed
  through its documented tuning interface to avoid applying two pacing delays.
  Pair timing resets after loading pauses and off/on transitions; it does not
  promise to hide genuinely irregular rendering or fix motion-vector quality.
- Missing fresh inputs disables generation for that present. Resizing drains
  work and recreates the effect. SDK errors are logged and generation disabled
  for the affected present.
- Camera position/orientation is not exposed by the existing upscale hook.
  The optional 3.1.4 camera-info extension is not supplied; real camera cuts,
  disocclusions, motion-vector conventions and UI need in-game validation.
- FSR's FPS multiplier counts successful interpolation dispatch submissions,
  not GPU completions or measured monitor scanouts.

The runtime is not committed to the repository. Public ABI layouts are adapted
from AMD's headers under the [MIT license](licenses/fidelityfx.txt).
