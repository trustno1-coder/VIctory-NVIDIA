# VIctory-NVIDIA-RT

A fork of [VIctory-NVIDIA](https://github.com/trustno1-coder/VIctory-NVIDIA), released on its own:
higher frame rates, 32:9 and HDR10, a one-click launcher with every option, and the groundwork for the title's ray
tracing on the RTX cores.

![PLAY.cmd, the launcher: resolution, window, console, DLSS, frame generation, Reflex, HDR10, frame cap, wide interface and speed on one screen](https://github.com/Kemerd/VIctory-NVIDIA-RT/releases/download/rt-test-7/launcher.png)

**Download:** [latest test build](https://github.com/Kemerd/VIctory-NVIDIA-RT/releases) (`victory.exe`, `victory-win10.exe` and `PLAY.cmd`).
Put them next to your own `game_scarlett_master.exe` and double-click `PLAY.cmd`.

| RTX 5090, 2560x1440, DLSS Quality, open world, no frame generation | official settings | this fork (fast) |
|---|---|---|
| median | 11.8 fps | **21.9 fps** |
| best | 18.3 fps | **31.9 fps** |

## Bug ##
1. If faces invisible restart game once
2. If low FPS turn down DLSS, or turn off RT
3. If crash post .dmp and .log
4. If the crash says `DXGI_ERROR_...` (device removed, device hung, driver reset), the card stopped answering.
   This layer pushes the GPU harder and longer than most games do, so:
   - **Take off any GPU overclock or undervolt first** (MSI Afterburner, the NVIDIA app's auto-tune). Settings that
     are stable in other games often aren't here. Test at stock clocks.
   - **Give Windows more time before it resets the GPU (TDR).** Long shader and BVH work can pass Windows' 2-second
     limit. In `regedit`, open `HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Control\GraphicsDrivers`, add a
     `DWORD (32-bit)` named `TdrDelay` with the value `10` (decimal), and reboot. Delete it to go back.
   - Still crashing at stock clocks with `TdrDelay`? Post the `.dmp` and `.log` as an issue.

## The launcher, `PLAY.cmd`

Enter starts with the recommended settings, or with your last ones. **C** walks through ten choices:

1. **Resolution:** 2560x1440, 1920x1080 upscaled to 1440, 3840x2160, **5120x1440 (32:9)**, **3440x1440 (21:9)**, or any width from 2560 to 7680 at 1440 high. (The game's output can only be patched at 1440 high so far: 16:10 and 2560x1080 are not possible yet.)
2. **Window:** fullscreen with no border (default) or in a window.
3. **Console profile:** Series X, the default and the one to use. Series S (1280x720 inside, much softer) is only for weaker cards.
4. **DLSS:** Quality, Balanced, Performance, DLAA or off.
5. **Frame generation:** ready to switch on in F1, **2x to 6x** from the start (3x and up need an RTX 50), or not loaded.
6. **NVIDIA Reflex:** on + boost (default), on, or off (frame generation turns it on anyway).
7. **HDR10** on or off (turn Windows HDR on first).
8. **Frame cap:** none, 30, 60, 120, 144 or any.
9. **Interface on ultrawide screens:** kept 16:9 in the middle, or stretched.
10. **Speed:** fast (recommended), plain (the official release's settings), or fast with the title's own ray tracing (experimental, 5-8 fps).

The choices are remembered; `PLAY.cmd last` starts with no questions. The launcher's resolution, DLSS, frame
generation and HDR choices win over the ones the F1 menu saved; every other F1 choice is kept.

## What this fork adds

- **Pipelines from pieces.** The title builds many pipelines from loose pieces (console code, metadata and
  register packets) instead of finished containers. They are now assembled and translated, so those draws happen.
  A stage that does not translate refuses its pipeline when it is created and the title falls back, so it is never
  worse than before. Graphics only for now; compute from pieces is being fixed.
- **The submitting thread in parallel:** one refresh step per list instead of about 52,000 single steps a frame,
  title writes that clear only the refreshes they overlap, merges that read only the 4 KiB parts written, and
  `ExecuteCommandLists` on a thread of its own.
- **Faster loading:** pipelines with static samplers are stored as a sampler table and kept in the pipeline
  library, instead of about 1,100 being made again by the driver every run (1.4 s each).
- **32:9 and HDR10.** The display mode is patched wider, so the camera, culling and motion vectors are made for
  the wide picture; the interface stays 16:9 in the middle. The upload memory that crashed 32:9 runs is fixed.
- **NVIDIA Reflex.** Low latency mode is on by itself whenever frame generation is on, which takes back much of the delay generated frames add. The launcher turns it on with boost by default.
- **Ray tracing.** The title's own builder calls are interposed (`VICTORY_RT_NATIVE=1`). It works, at 5-8 fps.
  Next: a builder of our own and the title's two traversal loop shapes on RayQuery, then DLSS Ray Reconstruction.

## How the frame pipeline was rebuilt

The title was written for a console where the CPU and the GPU share one memory. A PC with an RTX card has two, with
a bus between them. Almost every slow frame came down to that, so the path from the title's draw calls to the picture
on your screen was rebuilt stage by stage. Each stage is behind its own `VICTORY_*` switch, so it can be compared
against the old path, and `PLAY.cmd` turns on the set that measured fastest.

1. **Recording: the title's command lists are written down, not run.** On the console the title records straight
   into GPU memory. Here each list call is noted in a compact list of words (`VICTORY_THIN=1`), and threads of ours
   turn those words into D3D12 calls, in parallel with the title's own threads. The title's recording threads never
   wait for D3D12.
2. **Shaders: console GPU code becomes DXIL.** Every program the title has (vertex, pixel, compute, mesh) is decoded
   from the console's own machine code and translated to DXIL for the RTX card. Wave64 code runs on NVIDIA's 32-lane
   waves. The translated programs are cached on disk, and the host pipelines go into the driver's pipeline library,
   so a second start loads fast.
3. **Pipelines from pieces.** Many of the title's pipelines are not finished containers but loose pieces: a stage's
   code, its metadata and raw register packets. Those are now assembled, the registers read (blend, depth, cull,
   export formats), and the result translated into one host pipeline. A piece that does not translate refuses its
   pipeline when it is made, so the title takes its own fallback instead of drawing garbage.
4. **Memory: where the bytes are, and who moves them.** A title buffer can exist in four places: its own memory, our
   copy in PC memory, an upload heap, and the card. The rebuilt path moves each byte at most once:
   - the title's writes are watched page by page, and only the 4 KiB pages it touched are copied;
   - a buffer the card wrote and the card reads stays on the card (`VICTORY_CARD_STEPS`, the relay), without a trip
     back through the CPU;
   - shaders that read memory by address (the console's 40-bit pointers) find it through a directory of title
     memory on the card, kept in step frame by frame;
   - textures go up in order of use, with room held back for DLSS and frame generation.
5. **Submitting: one thread used to do everything; now it prepares, and others send.** The submitting thread was the
   limit at 32:9. It now takes one refresh step per list instead of about 52,000 single steps a frame, merges only
   the parts that changed, and hands `ExecuteCommandLists` and the fence signals to a thread of their own
   (`VICTORY_SEND_THREAD=1`). It moves straight on to the next submission.
6. **Presenting.** The frame, the interface plane and HDR10 are composed into the swap chain's buffer on the card.
   NVIDIA Streamline adds DLSS Super Resolution, Frame Generation (2x-6x) and Reflex there, before the present.
   Since test 8 the present also waits for the send thread, so the frame's last copy always reaches the card first.

The numbers behind each choice are in [docs/findings.md](docs/findings.md). How the parts fit together is in
[docs/architecture.md](docs/architecture.md).

## Building it

```powershell
git clone https://github.com/Kemerd/VIctory-NVIDIA-RT
cd VIctory-NVIDIA-RT
.\Get-NVIDIA-DLSS.cmd
powershell -ExecutionPolicy Bypass -File scripts\play-nvidia.ps1 -Exe 'D:\Games\MyDump\game_scarlett_master.exe' -Fast
```

Everything below is the upstream README.

---

# VIctory on NVIDIA

## Quick start: how to run it

**The easy way: one file, nothing to build**

1. Download `victory.exe` from the
   [latest release](https://github.com/trustno1-coder/VIctory-NVIDIA/releases/latest).
2. Put it into the folder of **your own dump** of the title's Xbox Series
   build v1.0.2.1, next to `game_scarlett_master.exe`.
3. Double-click it. Windows may say the publisher is unknown (the file is not
   signed): "More info", then "Run anyway".

- The first start asks once whether to download NVIDIA's DLSS and frame
  generation files (about 70 MB) from NVIDIA's own GitHub. They are NVIDIA's
  and are not part of this program; say no and the game still runs, with its
  own upscaling and no frame generation.
- Nothing is written into your game folder but crash files: settings, saves,
  caches, logs and NVIDIA's files are kept under `%LOCALAPPDATA%\victory`.
- **If it crashes,** it leaves `crash-<time>.log` (the run's log, ending with
  the crash report) and `crash-<time>.dmp` (a small minidump) next to
  `PLAY.cmd`, and the launcher names them. Send both.
- **Windows 10:** the launcher starts `victory-win10.exe`, built with the
  `agility` feature: it loads Microsoft's D3D12 Agility SDK (shader model
  6.6) from a `D3D12` folder next to it, which the launcher offers to fetch
  from Microsoft's NuGet and checks. Windows 11 keeps `victory.exe`, which has
  no such exports: with them and no `D3D12` folder, Windows 11 refuses to
  make the device (0x887E0003), which is why upstream PR #10 stayed out.
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
git clone https://github.com/trustno1-coder/VIctory-NVIDIA
cd VIctory-NVIDIA
.\Get-NVIDIA-DLSS.cmd
powershell -ExecutionPolicy Bypass -File scripts\play-nvidia.ps1 -Exe 'D:\Games\MyDump\game_scarlett_master.exe'
```

Put the path to **your** `game_scarlett_master.exe` in the last line. The
first start builds the layer, which takes a few minutes.

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
- **32:9 (-Res wide, VICTORY_DISPLAY_WIDE) has not been run on the title.** It
  is written (the image is patched for the wide output, the interface composite
  is placed in the middle, DLSS and frame generation take the wide sizes from
  the title) and compiled, not seen. What a first run has to show is listed in
  docs/dlss.md's 32:9 notes. If the world looks stretched, the title kept a
  16:9 camera: try VICTORY_ASPECT=5120x1440 with it (it widens projections in
  our copy only, so the title's own culling stays narrow).
- "SAVE AND RESTART NOW" can leave the old game's process behind without a
  window; it goes when the PC restarts.
- What foxhound lists as wrong (the heat-haze pass left out, hitches in
  cutscenes) applies here too.

## How to run it

You need Windows 10 or 11 (x64), a CPU with AVX2, 32 GB of RAM or more, an
SSD, an NVIDIA GeForce RTX card with a recent driver (RTX 40 or later for frame
generation, with hardware-accelerated GPU scheduling on; more than 2x needs an
RTX 50), [Rust](https://rustup.rs) (the toolchain is pinned), the Visual Studio
Build Tools with "Desktop development with C++", and **your own dump** of the
title's Xbox Series build v1.0.2.1 (the folder with
`game_scarlett_master.exe`).

```powershell
git clone https://github.com/trustno1-coder/VIctory-NVIDIA
cd VIctory-NVIDIA
copy victory.example.toml victory.toml      # then set game_dir to your dump's folder, outside this one
.\Get-NVIDIA-DLSS.cmd                        # once: NVIDIA's DLSS and frame generation files, about 70 MB
scripts\play-nvidia.ps1 -Exe 'D:\Games\MyDump\game_scarlett_master.exe'
```

The first start builds the layer (a few minutes). `Get-NVIDIA-DLSS.cmd` gets
NVIDIA's files from
[NVIDIA-RTX/Streamline](https://github.com/NVIDIA-RTX/Streamline) into
`nvidia-sdk\` (git-ignored). They are NVIDIA's, under NVIDIA's licences, which
are saved beside them; this repository does not carry them and may not. Nothing
is downloaded unless you run that file; `VICTORY_STREAMLINE_DIR` names another
folder if you have the Streamline SDK already.

`scripts\play-nvidia.ps1` is `scripts\play-foxhound.ps1` under the name this
fork has always had; its options:

| Option | What |
|---|---|
| *(none)* | Series X profile, 2560x1440, DLSS Quality, no frame rate cap, frame generation ready but off |
| `-FrameGen`, `-FrameGenX 4` | frame generation on from the start, 2 to 6 frames shown for each one rendered |
| `-Dlss off` / `dlaa` / `balanced` / `performance` | the title's own upscaling, or another DLSS mode |
| `-Fps 60` | a frame rate cap (30 is the title's own) |
| `-SeriesS` | the Series S profile: 1280x720 inside, faster, softer |
| `-Res 1080to1440` / `4k` | scene at 1080p upscaled by the title / the stock 3840x2160 output |
| `-Res wide` | Series X at 5120x1440 (32:9): the title's own camera, culling and motion vectors made for the wide picture, the interface kept 16:9 in the middle; another wide size: `-Res 1440 -More @{VICTORY_DISPLAY_WIDE='3440x1440'}` |
| `-NoFrameGenReady` | frame generation only with `-FrameGen` (it is otherwise made ready at every start, so the menu can switch it) |
| `-Build diag` | the diagnostics build (slower; captures, timelines, recording time in the F3 panel) |
| `-More @{VICTORY_X='1'}` | add or override any switch |

The first wide start patches and writes its own copy of the image
(`images\native-1440p-5120x1440-...`), like the other pictures.

What the F1 menu saved (`victory-settings.txt`) goes over these at every start;
delete that file to go back to them. Full screen: start the game, then run
`.\scripts\fullscreen.ps1` in another window. Logs go to
`%LOCALAPPDATA%\victory\logs`, saves to `%LOCALAPPDATA%\victory\data`.

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
