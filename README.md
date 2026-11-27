# VIctory on NVIDIA

## Quick start: how to run it

**The easy way: one file, nothing to build**

1. Download `victory.exe` from the
   [latest release](https://github.com/trustno1-coder/VIctory-NVIDIA/releases/latest).
   (That binary is the upstream build, without FSR 3 frame generation; to get
   FSR 3, build it yourself below.)
2. Put it into the folder of **your own dump** of the title's Xbox Series
   build v1.0.2.1, next to `game_scarlett_master.exe`.
3. Double-click it. Windows may say the publisher is unknown (the file is not
   signed): "More info", then "Run anyway".

- The first start asks once whether to download NVIDIA's DLSS and frame
  generation files (about 70 MB) from NVIDIA's own GitHub. They are NVIDIA's
  and are not part of this program; say no and the game still runs, with its
  own upscaling and no frame generation.
- Nothing is written into your game folder: settings, saves, caches, logs and
  NVIDIA's files are kept under `%LOCALAPPDATA%\victory`.
- You need Windows 10 or 11, 32 GB of RAM or more, an SSD and an NVIDIA
  GeForce RTX card with a recent driver. This repository and its releases
  contain no game files.

**In the game**

- **F1** brings up the settings menu (arrow keys pick and change, Enter does
  it): DLSS, frame generation, frame rate cap and more.
- **F3** shows the stats panel (frame rate, frame generation, draws, GPU and
  memory numbers).

**Building it yourself instead**

Install [Rust](https://rustup.rs) and the
[Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
with "Desktop development with C++", then in PowerShell:

```powershell
git clone https://github.com/EhRabz/Claude9
cd Claude9
git checkout fsr3-framegen-3080ti
.\Get-NVIDIA-DLSS.cmd
.\Get-FSR3.cmd
powershell -ExecutionPolicy Bypass -File scripts\play-foxhound.ps1 -Exe 'D:\Games\MyDump\game_scarlett_master.exe' -Fsr3FrameGen
```

Put the path to **your** `game_scarlett_master.exe` in the last line. The
first start builds the layer, which takes a few minutes. `Get-FSR3.cmd`
fetches AMD's FSR 3 files (see
[FSR 3 frame generation](#fsr-3-frame-generation)); leave off
`-Fsr3FrameGen` to run the DLSS path instead.

Options, requirements in full and the details are under
[How to run it](#how-to-run-it).

A fork of [VIctory](https://github.com/Marvisak/VIctory), a compatibility layer
that runs an Xbox Series title natively on Windows from files **you** supply,
tuned for **NVIDIA RTX** cards.

> **This repository contains no game files, code or assets.** You need your
> own legally obtained copy of the title. It is not affiliated with or endorsed
> by the title's developer or publisher, nor by NVIDIA. See
> [docs/asset-policy.md](docs/asset-policy.md).

## Built from many projects, and from running every one of them

This repository did not come from one place, and the base it stands on today
is the end of a long comparison, not the start of the work. Over several weeks
we built, ran and measured many projects on the same NVIDIA PC: Leonida,
[Marvisak's VIctory](https://github.com/Marvisak/VIctory),
[fxceless's fork](https://github.com/fxceless/victory-fork),
[the 7900 XTX fork](https://github.com/avatarmanz/VIctory-7900XTX),
[Belobug's](https://github.com/Belobug/VIctory-Belobug-AMD),
[DexNom's](https://github.com/DexNom/VIctory),
[CGBJ's cool_fox](https://github.com/codec-d/cool_fox), Kelvaroq's, Neptune,
xcl (another layer for the same title) and then
[foxhound](https://github.com/readmet3xt-lab/foxhound). On each we found what
stopped it on an RTX card, fixed or worked around it, measured the result, and
carried what we learned to the next. Every pull request sent here was built
and run on the same card before it was merged. No single project got here
alone, and choosing the base that runs best was only one result of that work.

### What this fork brings

Not one fix but a long line of them: days of work, much of it around the clock,
across every one of those codebases, more than 300 measured runs on the same
RTX PC, and each fault chased towards its cause before the next build. In rendering, in
speed, in the picture and in simply being able to play it:

**Rendering fixes**

- **Draws no longer skipped on NVIDIA.** NVIDIA caps the view heap below what
  the title asks for; thousands of draws a frame were dropped, the screen
  rarely updated and the character jumped from place to place. The pool of
  descriptor slots was reworked until almost none were.
- **Menus, loading screens and subtitles on Series S**, black until then (the
  interface plane laid over the game), and **faces that move** (their network
  carried through as integers): ported from fxceless's pull requests 1189 and
  927 and made to work on this card.
- **The intro video without flicker**: its frames came out of order.
- **Black blobs gone**: a single bad number from a shader, spread by blur and
  the upscaler into patches on faces and in the air, is caught where it is
  written.
- **White, hazy rooms explained and tamed**: the title's ray tracing was
  running with its structure builder off, so every ray missed. Which passes
  can be left out without filling the picture with rainbow glitter was worked
  out pass by pass.
- **The ray-tracing structure builder running on NVIDIA**, on CGBJ's renderer:
  the draws dropped for it went from thousands a frame to one.
- **White-outs with black holes further into the game** followed up in frames
  captured in play: thousands of draws a frame were being dropped there for
  that same missing structure.

**Speed**

- **Memory compares moved to the card**: 11 to about 22 frames a second on the
  base of the time.
- **Settings measured, not guessed**: one switch at a time, on the same scene,
  run after run.
- **The base itself chosen by measurement**: the same rooms went from 15-17
  frames a second to 26-30, at a higher resolution.
- **The 30 frames a second were the title's own frame interval**, not the
  layer's limit: found, and lifted.

**Picture and frames**

- **DLSS in place of the title's upscaler**, with its model and mode to choose.
- **DLSS frame generation with up to six frames shown for each one rendered**,
  switched and changed while playing, without a restart (foxhound's is 2x, set
  at the start). By Streamline's own count it shows exactly 2, 4 and 6.
- **Why frame generation showed nothing on a full 12 GB card**, found and
  fixed: the card's memory is now shared out so that it has room.
- **The frame rate at the top left of the picture**: frames rendered a second
  and, with frame generation, frames shown (`FPS: 28.4  FG: 170.4`).

**Playing it**

- **The F1 settings menu**, in every build: DLSS, frame generation, frame rate
  cap, console profile, picture size, the light estimates and the main speed
  switches, saved to `victory-settings.txt` and applied at the next start
  ("SAVE AND RESTART NOW" does that for you; lines marked LIVE change at once).
- **The F3 stats panel**, in every build: frame rate and frame time, frames
  shown with frame generation, draws, how busy the GPU and the submitting
  thread are, copies, RAM and VRAM. The menu switches it too.
- **Open water without dark blocks.** Two of the title's water light programs
  run on 32-lane cards only in a form that left whole tiles of the sea dark
  (around boats, along shores, under bridges) and cost about 15 ms of the
  card a frame. They are left out there: no blocks and 30 fps instead of 25
  on the first boat ride (RTX 5070), with water that is darker and bluer.
  WATER LIGHT in the menu (`VICTORY_WATER_LIGHT=1`) runs them again.
- **One command to play**: the launcher builds the layer the first time and
  applies what the menu saved.
- **`Get-NVIDIA-DLSS.cmd`**: the eight NVIDIA files that are loaded, about
  70 MB, taken out of NVIDIA's 263 MB release without downloading the rest.
  NVIDIA's licence does not let a repository carry them, so they come from
  NVIDIA.

**Tested, not assumed**

- Every pull request sent here was read line by line and, where it touched the
  game, built and run on this card before it was merged: ten so far.

Where each of these is today: this fork's own code in this tree is the F1
menu, the stats panel in every build, the water light switch, frame
generation from 2x to 6x with its live switching and memory handling, the
lifted frame rate cap, the frame rate line, the launcher and the NVIDIA file
fetcher. The foxhound line carries the descriptor slots, the
compares on the card, the interface plane and the face network in its own
form. The black-blob scrub, the graphics card picker and the ray-tracing
builder path are in this repository's history, and come over as this base
turns out to need them.

### The base since 2026-11-26: foxhound

On 2026-11-26 this fork moved onto
[readmet3xt-lab/foxhound](https://github.com/readmet3xt-lab/foxhound)
(`02fc18a`, GPL-3.0-or-later). On the same PC it ran the same rooms at close to
twice the frame rate of what this fork had, at a higher resolution, so its
tree is now this one's, with its history kept, and this fork's own additions
are on top. What foxhound is and how it got there is in its own README, kept as
[docs/README-foxhound.md](docs/README-foxhound.md); its measurements are in
[docs/findings.md](docs/findings.md).

What foxhound brings:

- the Series X profile at a native 2560x1440;
- DLSS Super Resolution and DLSS Frame Generation through NVIDIA Streamline,
  loaded when the game runs (nothing of NVIDIA's is needed to build);
- an estimate of indirect light and of reflections in place of the title's ray
  tracing, and fewer stalls at scene cuts.

What this fork has on top of it is under
[What this fork brings](#what-this-fork-brings) above.

## Where it stands

Tested on one PC: Windows 11, Ryzen 7 9800X3D, 64 GB RAM, **GeForce RTX 5070
12 GB**; the title's Xbox Series build v1.0.2.1, Series X profile, 2560x1440
output, DLSS Quality. Frame rates are the layer's own count, in the opening
rooms.

| | |
|---|---|
| Opening rooms | mostly 26-30 frames a second rendered (33-38 ms a frame) |
| Frame generation | 2x to 6x on top of that: by Streamline's own count, exactly 2, 4 and 6 frames shown for each one rendered |
| Device losses, crashes | none in the measured runs |
| Before this base (Series S, 720p to 1080p by DLSS) | 15-17 frames a second in the same rooms |

foxhound's own measurements, on an RTX 4070 SUPER, are in its README.

What is known to be wrong:

- **No ray tracing.** The title's own is off (`VICTORY_BVH_BUILDER=1`, or the
  menu's RAY TRACING, turns it on: slow and with a broken picture on this
  base). An estimate stands in for indirect light and reflections.
- **Frame generation needs room in the card's memory.** The layer fills the
  card with textures up to its budget; with frame generation on from the start
  the launcher holds a fifth of the budget back (a little less while it is only
  ready), so a few more textures can come late. Switched on from the menu on a
  full 12 GB card it can still say that memory is past the budget; it then
  runs, perhaps unevenly.
- **Frames are only generated while the game's window has the focus**, and
  about a second passes after a change before they come.
- **The first run of every new build stutters** while the driver compiles
  pipelines (foxhound's README, *Warming up*, says how to make them ahead).
- "SAVE AND RESTART NOW" can leave the old game's process behind without a
  window; it goes when the PC restarts.
- What foxhound lists as wrong (the heat-haze pass left out, hitches in
  cutscenes) applies here too.

## How to run it

You need Windows 10 or 11 (x64), a CPU with AVX2, 32 GB of RAM or more, an
SSD, an NVIDIA GeForce RTX card with a recent driver (RTX 40 or later for DLSS
frame generation, with hardware-accelerated GPU scheduling on; more than 2x
needs an RTX 50; RTX 30-series can do frame generation through FSR 3 instead,
see [FSR 3 frame generation](#fsr-3-frame-generation)),
[Rust](https://rustup.rs) (the toolchain is pinned), the Visual Studio Build
Tools with "Desktop development with C++", and **your own dump** of the title's
Xbox Series build v1.0.2.1 (the folder with `game_scarlett_master.exe`).

```powershell
git clone https://github.com/EhRabz/Claude9
cd Claude9
git checkout fsr3-framegen-3080ti
copy victory.example.toml victory.toml      # then set game_dir to your dump's folder, outside this one
.\Get-NVIDIA-DLSS.cmd                        # once: NVIDIA's DLSS and frame generation files, about 70 MB
.\Get-FSR3.cmd                               # once: AMD's FSR 3 files, for frame generation on RTX 30-series
scripts\play-foxhound.ps1 -Exe 'D:\Games\MyDump\game_scarlett_master.exe' -Fsr3FrameGen
```

The first start builds the layer (a few minutes). `Get-NVIDIA-DLSS.cmd` gets
NVIDIA's files from
[NVIDIA-RTX/Streamline](https://github.com/NVIDIA-RTX/Streamline) into
`nvidia-sdk\` (git-ignored). They are NVIDIA's, under NVIDIA's licences, which
are saved beside them; this repository does not carry them and may not. Nothing
is downloaded unless you run that file; `VICTORY_STREAMLINE_DIR` names another
folder if you have the Streamline SDK already. `Get-FSR3.cmd` gets AMD's FSR 3
files the same way (see [FSR 3 frame generation](#fsr-3-frame-generation));
nothing is downloaded unless you run it either.

`scripts\play-foxhound.ps1` runs the game; its options:

| Option | What |
|---|---|
| *(none)* | Series X profile, 2560x1440, DLSS Quality, no frame rate cap, frame generation ready but off |
| `-FrameGen`, `-FrameGenX 4` | frame generation on from the start, 2 to 6 frames shown for each one rendered |
| `-Fsr3FrameGen` | FSR 3 frame generation on from the start, 2x, with DLSS upscaling — for RTX 30-series and other GPUs DLSS frame generation cannot run on (run `Get-FSR3.cmd` once first; see [FSR 3 frame generation](#fsr-3-frame-generation)) |
| `-Dlss off` / `dlaa` / `balanced` / `performance` | the title's own upscaling, or another DLSS mode |
| `-Fps 60` | a frame rate cap (30 is the title's own) |
| `-SeriesS` | the Series S profile: 1280x720 inside, faster, softer |
| `-Res 1080to1440` / `4k` | scene at 1080p upscaled by the title / the stock 3840x2160 output |
| `-NoFrameGenReady` | frame generation only with `-FrameGen` (it is otherwise made ready at every start, so the menu can switch it) |
| `-Build diag` | the diagnostics build (slower; captures, timelines, recording time in the F3 panel) |
| `-More @{VICTORY_X='1'}` | add or override any switch |

What the F1 menu saved (`victory-settings.txt`) goes over these at every start;
delete that file to go back to them. Full screen: start the game, then run
`.\scripts\fullscreen.ps1` in another window. Logs go to
`%LOCALAPPDATA%\victory\logs`, saves to `%LOCALAPPDATA%\victory\data`.

## FSR 3 frame generation

DLSS frame generation needs an RTX 40-series card or later. On an RTX
30-series card (or any other GPU DLSS frame generation cannot run on), this
branch can do frame generation with AMD's FSR 3 instead. It is frame
generation only, 2x: DLSS Super Resolution still does the upscaling, and only
DLSS frame generation is off. The F1 menu shows it as FSR 3 FRAME GENERATION,
OFF or 2X, and it can be switched while playing.

### Getting the FSR 3 files

The FSR 3 files are AMD's, under AMD's licences; this repository does not
carry them and may not. `Get-FSR3.cmd` fetches them once:

```powershell
.\Get-FSR3.cmd
```

It downloads the FidelityFX SDK (version 1.1.4, the FSR 3.1 release) from
GPUOpen, builds the small C++ bridge in `tools/fsr3-bridge` against it with
CMake and the Visual Studio compiler, and puts the two files the game needs
into `fsr3-sdk\` (git-ignored):

- `fsr3_bridge.dll` — the bridge, built here
- `amd_fidelityfx_dx12.dll` — AMD's prebuilt, signed FSR 3.1 library

It needs CMake: if it is not on PATH, the script finds the copy Visual Studio
ships with (through `vswhere` or the usual install folders). If the SDK is
already extracted somewhere, point at it instead of downloading it again:

```powershell
.\Get-FSR3.cmd -SdkDir 'E:\Claude9\fsr3-sdk\sdk-src\FidelityFX-SDK-1.1.4'
```

`victory get-fsr3` does the same thing, and `victory upscaler-check` reports
whether the bridge loads. `VICTORY_FSR3_DIR` names another folder if the files
are somewhere else. DLSS is still needed for the upscaling, so run
`Get-NVIDIA-DLSS.cmd` too (or set `VICTORY_STREAMLINE_DIR` to a Streamline
SDK you already have).

FSR 3.0 has no prebuilt library (it is source-only), and the 2.0 SDK splits
the library into several files, so 3.1 (SDK 1.1.4) is the one this targets.
Rust never touches AMD's structures directly: it talks only to the bridge
over a small C interface, and all of AMD's types live in the bridge, compiled
on Windows against the real SDK headers. The details are in
[docs/FSR3_INTEGRATION.md](docs/FSR3_INTEGRATION.md) and the bridge's own
[tools/fsr3-bridge/README.md](tools/fsr3-bridge/README.md).

### Building and running it

```powershell
git clone https://github.com/EhRabz/Claude9
cd Claude9
git checkout fsr3-framegen-3080ti
cargo build --release --locked -p victory
.\scripts\play-foxhound.ps1 -Exe 'E:\GTA\game_scarlett_master.exe' -Fsr3FrameGen
```

Put the path to **your** `game_scarlett_master.exe` in the last line. The
script only builds `victory.exe` if it is missing: after changing any `.rs`
file, run the `cargo build` line again yourself. `-Fsr3FrameGen` sets DLSS
Quality upscaling on, DLSS frame generation ready but off
(`VICTORY_FRAMEGEN=1`, `VICTORY_FRAMEGEN_X=0`), and FSR 3 frame generation on
(`VICTORY_FSR3_FG=1`), with a fifth of the texture budget held back
(`VICTORY_TEXTURE_ROOM_PERCENT=80`) so the card has room for the extra frames.
If both DLSS frame generation and FSR 3 frame generation are asked for, DLSS
wins and FSR 3 stays off. The start-up line says which upscaling is in use;
with DLSS on it reads `FSR 3 frame generation on (2x, DLSS upscaling), scene
at the DLSS quality scale`.

The game runs at 2560x1440. At that size the title's own upscaling cannot
run: its shipping FSR4 upscaler only takes 1920x1080 out, so at 2560x1440 it
fails to make and the title then divides by zero on its first upscale. DLSS
upscaling is therefore required at 1440p, and `-Fsr3FrameGen` keeps it on.
Close the F1 menu while frame generation is on: the interface is copied into
the scene as-is, so the menu would be interpolated too. Frame generation is
SDR only, and the camera vectors are approximated (the title gives no camera
matrices), so depth and motion tagging are best-effort.

### Where it stands

Tested on Windows 11, GeForce RTX 3080 Ti 12 GB, 2560x1440, DLSS Quality.
As of 2026-11-27 the frame-generation context is created and the game runs
clean for minutes at a time with DLSS upscaling, but the swap-chain wrap
fails (`FFX_API_RETURN_ERROR_RUNTIME_ERROR`, code 3), so frame generation
stays off and the game presents as before. The wrap has been traced with
diagnostics written beside the bridge (`fsr3-sdk\fsr3_wrap_diag.txt`, and
`fsr3-sdk\fsr3_debug.txt` once the debug checker is on): the chain, its
device, the game queue, fences, allocators, textures, descriptor heap,
`IDXGISwapChain3` and `IDXGISwapChain4` all check out, so the failure is in
a call none of those cover. That is the open problem; everything up to it
works.

### Troubleshooting

- **DLSS is off even though `-Fsr3FrameGen` was passed.** What the F1 menu
  saved (`victory-settings.txt`) goes over the script's options at every
  start, and an old file still holds DLSS off. Delete it:
  `Remove-Item 'victory-settings.txt'`.
- **The build finishes in a fraction of a second.** Nothing recompiled: the
  script only builds `victory.exe` if it is missing. Run
  `cargo build --release --locked -p victory` yourself after replacing any
  `.rs` file (about 20 seconds); if it still comes back instant,
  `cargo clean -p victory` first.
- **The script will not run: "not digitally signed".** A downloaded script
  is marked as from the internet. Unblock it once:
  `Unblock-File 'scripts\play-foxhound.ps1'`.
- **`Get-FSR3.cmd` fails on `Expand-Archive`.** Older .NET has no
  three-argument extract; the script now uses the two-argument form. Get the
  latest `scripts/get-fsr3.ps1`.
- **`Get-FSR3.cmd` fails with "cmake is not on PATH".** The script now finds
  Visual Studio's own CMake. Get the latest `scripts/get-fsr3.ps1`.
- **The game crashes with a divide-by-zero.** That is the title's own
  upscaler at 1440p, not FSR 3: it only happens when DLSS upscaling is off.
  Delete `victory-settings.txt` and run with `-Fsr3FrameGen` so DLSS stays on.
- **Logs** go to `%LOCALAPPDATA%\victory\logs` (`foxhound-*.log`); the wrap
  diagnostics to `fsr3-sdk\fsr3_wrap_diag.txt`.

## How it works, and the findings

- [docs/README-foxhound.md](docs/README-foxhound.md): foxhound's own README
  (set-up by hand, warming up, its switches).
- [docs/architecture.md](docs/architecture.md): the layer's parts.
- [docs/findings.md](docs/findings.md): measurements behind the choices.
- [docs/dlss.md](docs/dlss.md): DLSS and Frame Generation.
- [docs/fork-ray-tracing.md](docs/fork-ray-tracing.md): the title's ray tracing
  and the light estimate.
- [docs/asset-policy.md](docs/asset-policy.md): what may and may not be in this
  repository.

## Credits

A tree of other people's work with a little on top; the history is kept.

- **[foxhound](https://github.com/readmet3xt-lab/foxhound)** by
  readmet3xt-lab: the base of this fork since 2026-11-26, and through it:
- **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and its
  contributors: the loader, the runtime services, the Direct3D 12 back end and
  the shader recompiler.
- **[fxceless/victory-fork](https://github.com/fxceless/victory-fork)**,
  **[avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX)**,
  **[codec-d/cool_fox](https://github.com/codec-d/cool_fox)**,
  **[DexNom/VIctory](https://github.com/DexNom/VIctory)**,
  **[Belobug/VIctory-Belobug-AMD](https://github.com/Belobug/VIctory-Belobug-AMD)**
  and the others foxhound's README names.
- Everyone who sent pull requests to this fork before the move; that work is
  in the history, on the branch that was `main` until then.
- NVIDIA Streamline, DLSS and Reflex are NVIDIA's; they are loaded at run time
  from a folder you supply.
- Much of this fork's code was written with
  [Claude Code](https://claude.com/claude-code).

## License

GPL-3.0-or-later, as VIctory and foxhound. See [LICENSE](LICENSE); the one file
that is not covered is named in [docs/asset-policy.md](docs/asset-policy.md).
