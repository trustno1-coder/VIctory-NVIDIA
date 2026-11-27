# A lighter profile: 8 GB cards and 16 GB of RAM

This fork is tuned and measured on 12 GB cards and 32-64 GB of RAM, and its README asks for 32 GB. This page is
what it took to make the layer **playable and stable on half of that**: an 8 GB NVIDIA card and 16 GB of RAM. It
gives every change, the reason for it, and what each step measured, so that the defaults (or a "small PC" switch
in the launcher and the F1 menu) can be built from it.

The result, as one command (`PLAY_SMALL.cmd`, or `scripts\play-small.ps1`): about **21 frames a second rendered
(median), 42 shown with frame generation 2x**, the VRAM flat at about 5.7 GB of a 7 GB budget, no device loss,
no out-of-memory and no dropped DLSS evaluation over the measured runs. Without these changes the same PC lost
the device after 8 to 13 minutes, or ran with whole sets of textures (characters' heads included) never drawn.

## Contents

- [The test machine and how it was measured](#the-test-machine-and-how-it-was-measured)
- [Build](#build)
- [Why the defaults do not fit](#why-the-defaults-do-not-fit)
- [What was changed, and why](#what-was-changed-and-why)
- [Every run, in order](#every-run-in-order)
- [The launcher and its options](#the-launcher-and-its-options)
- [What is left, and ideas](#what-is-left-and-ideas)

## The test machine and how it was measured

| | |
|---|---|
| CPU | AMD Ryzen 5 7600 (6 cores, 12 threads, Zen 4, AVX-512) |
| GPU | NVIDIA GeForce RTX 4060, **8 GB** GDDR6 (budget given to the process: 7.0-7.2 GB), driver 621.40, resizable BAR on (BAR1 8192 MiB), hardware-accelerated GPU scheduling on |
| RAM | **16 GB** (15.6 GB usable; shared GPU segment capped at ~7.8 GB) |
| Storage | 240 GB NVMe SSD (system, and the paging file from run J' on); 1 TB SATA QLC SSD (the dump, and the paging file before J') |
| Paging file | before J': 8 GB system-managed on the NVMe + 16-32 GB on the SATA SSD; from J': 24-40 GB on the NVMe only |
| Screen | 1920x1080, 240 Hz |
| OS | Windows 11 (24H2, build 26100) |
| Toolchain | Rust 1.98.1 (pinned), Visual Studio 2026 Build Tools |

Frame rates are the layer's own count: the `N fps, M ms/frame (...), D draws/frame` line it logs every few
seconds, taken only where `D > 2000` (in the world, not menus or loading screens), and given as the median and
the 10th / 90th percentile of a run. Memory figures are the layer's `the texture queue's room:` line (VRAM in
use, the line, the budget Windows gives the process) and its warnings. Each run was 8 to 15 minutes of normal
play from the same save. Rendered frames are what the title produces; with frame generation 2x every one of them
is shown twice.

All runs were made on commit `d12eb1b` (before the FSR 3 / MFG Unlocker updates); the launcher was checked against
the newer `play-foxhound.ps1`, whose options it uses did not change.

Useful log searches for anyone repeating this: `out of VRAM` (DLSS could not evaluate), `no upload buffer` /
`no shared buffer` (the shared GPU segment is full), `no free dynamic descriptor slots`, `device removed`,
`texture queue's room` and `waiting` (textures held back by the line).

## Build

Nothing in `Cargo.toml` changes. The release build was made with whole-program optimisation and the host CPU's
instruction set, through cargo's environment variables:

```powershell
$env:RUSTFLAGS = '-C target-cpu=native'
$env:CARGO_PROFILE_RELEASE_LTO = 'fat'
$env:CARGO_PROFILE_RELEASE_CODEGEN_UNITS = '1'
cargo build --release --locked -p victory
```

About 2.5 minutes on the 6-core CPU above (16 GB is enough for fat LTO). `target-cpu=native` makes a binary for
that CPU only: do not ship it. The layer is CPU-bound on this machine (the submitting thread spends about 30 ms
of a frame preparing), so this is the cheap part of the win; it was not measured on its own against the plain
release build. The launcher only builds when `target\release\victory.exe` is missing, so build first.

## Why the defaults do not fit

Three budgets run out, not one:

1. **VRAM (8 GB card, about 7.0 GB budget for the process).** Frame generation alone takes 1.5 to 2 GB. The
   Series X profile is out of the question; even on Series S the layer's own reserves and its texture pool fill
   the budget. Past the budget, textures wait (characters without heads, missing surfaces), DLSS fails with
   `out of VRAM` (old frames shown), and the driver eventually faults.
2. **The shared GPU segment (system memory the card may use).** Windows caps it at **half the RAM**: about 7.8 GB
   on a 16 GB PC. Title windows, upload and read-back buffers live there. When it fills, every allocation fails
   (`E_OUTOFMEMORY`, tens of thousands of them) and the device is lost a few seconds later.
3. **RAM itself.** The process commits about 20 GB with a working set near 8 GB, so on 16 GB Windows pages, and
   frames drop to single digits while it does.

The changes below take each of these in turn. They pull against each other (memory moved off the shared segment
goes to the VRAM, frame generation needs VRAM that textures want), which is why their order and sizes matter.

## What was changed, and why

### 1. Series S profile

`-SeriesS`. Title memory is 7.6 GB instead of 13.5 GB (Series X), the only way the title fits next to the layer in
16 GB, and its 1080p output is the screen's. Series X was not tried on this PC.

### 2. DLSS Performance

`-Dlss performance`. With the VRAM freed by the steps below the card was 77-97 % busy; Performance instead of
Quality, with the BAR windows of step 3, took the same scenes from a median of 20.7 to 23.3 fps rendered (+13 %,
without frame generation).

### 3. Title windows in BAR memory: `VICTORY_WINDOWS_BAR_MB=1024`

The switch already in `shadow.rs` (and in the old Series S launcher, commit cda4987, for a 16 GB PC with a 3060).
Windows of title memory go to CPU-mapped card memory instead of the shared segment, as long as a fifth of the
VRAM budget is free. Without it (run D) the shared segment filled after 13 minutes and the device was lost. It
costs VRAM, so it is sized at 1 GB and only works because steps 5 to 7 leave that fifth free.

### 4. A repo bug on Series S: the render target reserve is never given back

`VICTORY_CARD_ROOM_FOR_TARGETS_MB=1536` (default 3072).

`resource.rs` `targets_room_kept` keeps VRAM back from the texture queue for the render targets a world makes in
its first frame, and gives it up once the targets hold **half of the reserve or more** and have stopped growing for
20 s. That rule was measured at 1440p, where targets reach 2.1-2.9 GB. On the Series S profile they level off
near **1.3 GB, below half of 3072**, so the condition is never met and **about 1.75 GB of VRAM stays reserved for
the whole run** (`1753 MB kept for render targets` in every room line of runs C-D). With 1536 the reserve is gone
within a minute of the first world frame (`0 MB kept`).

Suggested fix in the code: size the reserve from the console profile and output size, or give it up once targets
stop growing, whatever their size.

### 5. Texture pool off: `VICTORY_TEXTURE_POOL=0`

The pool places textures in 64 MB blocks. A texture let go leaves a hole that only another texture can fill, and a
block goes back to Windows only when empty. In play, **1.2 to 1.7 GB of the VRAM in use was such holes**
(`MB free in texture blocks`), counted by Windows against the budget but usable by nothing else. With the line at
its default 90 % (run F) the process went 0.5-0.7 GB over the budget at 6-8 minutes; frames fell to 8 per second,
dynamic descriptor slots ran out (`no free dynamic descriptor slots`) as submissions backed up, and the driver
faulted (access violation in `nvwgf2umx.dll`).

With the pool off (run H), the holes are gone (`0 MB free in texture blocks`), VRAM in use dropped from 7.2 to
5.9 GB in the same play, and it stayed under the budget. The median frame rate did not drop (22.8 fps against 19.1
for run G).

Suggested fix in the code: give back the free part of partly used blocks under budget pressure, or turn the pool
off on cards under 10 GB.

### 6. Texture quality: leave out the largest levels of big textures (new code)

`VICTORY_TEXTURE_SKIP_LEVELS=1`, `VICTORY_TEXTURE_SKIP_MIN=2048`: `crates/victory-gpu/src/resource.rs`,
`skip_levels`, `skipped`, `levels_left_out`, used in `base_level`. Off by default (0): nothing changes unless
the switch is set.

Why it is needed: **nothing frees a host copy of a texture while a descriptor slot still shows it**
(`let_go_idle` requires that no slot views it), and the title's bindless heap keeps slots of most of a world's
textures. With frame generation the remaining VRAM cannot hold a world's textures: with the line at 75 % (which
leaves frame generation its 1.75 GB) more than 20,000 textures waited in every new area (run H). A real eviction
(least recently used, under pressure) would need to know which slots shaders actually read; the layer does not
see that for textures picked by index.

What it does: the host copy of a block-compressed texture that the title fills is made from a later level, as
`VICTORY_HELD_LEVELS` already does for levels no view shows. Up to `N` levels are left out, only while the level's
longer side is at least `SKIP_MIN` texels, and never so many that fewer than two smaller levels remain. A view
that starts at a level left out is clamped by `descriptor::host::from_base` to the first level the copy has, the
same path the held levels take. Each level left out is a quarter of that texture's memory. A unit test
(`texture_quality_leaves_out_only_big_compressed_levels`) covers the rule.

Measured: with 1 level of textures 1024 and more (run I) the VRAM sat at 5.3-5.6 GB with frame generation and no
texture waited, but textures were visibly soft. With 1 level of textures **2048 and more** (run J), 5.7 GB, all
textures in, and the picture close to the full one. 2048 is the recommended floor; 1024 is for 6 GB cards.

### 7. The texture line: `VICTORY_TEXTURE_ROOM_PERCENT=82`

The share of the budget past which new textures wait. The repo sets 80 with frame generation (86 ready-but-off,
90 otherwise); on 8 GB those leave no room once the reserves of steps 3-4 count. With steps 4-6 in place, 82 % with
frame generation (85 % without) keeps about 1.3 GB free for frame generation and DLSS while textures still come
in. Below that (60-75 %) textures waited by the thousand; above it frame generation runs out of memory.

### 8. Frame generation 2x

Only once steps 3-7 are in. With the repo's settings it was the direct cause of the missing textures and, with the
BAR windows on, of `out of VRAM` from DLSS within two minutes (run E). With them, 2x presented frames for the whole
run and no evaluation failed.

### 9. Paging file on the NVMe drive (system setting)

Not a layer setting, but it matters on 16 GB: the process commits about 20 GB, so Windows pages. The paging file
was on a SATA QLC SSD; moved to the NVMe system drive (24-40 GB fixed), the **10th percentile went from 11.7 to
16.4 fps** (+40 %) with the same median (runs J and J'). The shared GPU segment's cap (half the RAM) cannot be
changed by any setting.

### Kept from the repo's launcher

Everything else is `play-foxhound.ps1`'s set, plus `VICTORY_PLACED_ORDER=1` as the `PLAY_FRAMEGEN.cmd` launcher
sets it.
Borderless full screen is the layer's own (`VICTORY_FULLSCREEN=1`, made when the window is created), not
`scripts\fullscreen.ps1`, which gives up after 3 minutes and so missed the game after a `-Preload`.

## Every run, in order

All on Series S. "FG" is frame generation 2x. fps are rendered frames, 10th percentile / median / 90th percentile
of the run's world lines (method above); with FG twice as many are shown. The 10th percentile includes scene loads
and paging, which is why it moves the most. Runs that end in a fault are counted up to it.

| Run | Settings (beyond the repo's) | Rendered fps p10 / median / p90 | Memory | Outcome |
|---|---|---|---|---|
| 0 | DLSS Quality, FG (repo launcher defaults, line 80 %) | 3.4 / 14.0 / 14.0 | VRAM full, textures waiting | characters drawn without heads |
| A | DLSS Quality, no FG | 3.5 / 20.7 / 26.8 | under budget | heads back: the cause was VRAM |
| B | DLSS Performance, BAR 1 GB, placed order | 3.6 / 23.3 / 26.8 | textures waiting at the 90 % line | stable over 5 min, median +13 % |
| C | B + FG, line 65 % | 16.4 / 20.3 / 22.1 | over budget at 2 min | 2,500+ DLSS `out of VRAM`, old frames shown |
| D | FG, BAR off, line 60 % | 11.1 / 19.2 / 23.6 | 1.75 GB stuck in the target reserve; shared segment full at 13 min | 43,000 `E_OUTOFMEMORY`, device lost |
| E | FG, BAR 1 GB, targets 1536, line 75 % | 16.8 / 20.3 / 22.9 | 7.2 GB in use vs 6.97 GB budget at 2 min | DLSS `out of VRAM`: BAR windows and FG compete |
| F | no FG, BAR 1 GB, targets 1536, line 90 % | 11.9 / 22.9 / 31.6 | 7.7 GB vs 7.0 budget at 6-8 min (1.2-1.7 GB of pool holes) | 8 fps, descriptor slots exhausted, driver fault at 8 min |
| G | F + line 75 % | 11.1 / 19.1 / 29.9 | 7.24 GB, at the budget | no crash in 14 min, 12,000 textures waiting in new areas |
| H | G + pool off | 7.7 / 22.8 / 31.1 | 5.9-6.1 GB (holes gone); shared segment full once at 8 min, recovered | no crash in 11 min, 20,000 textures waiting (line too low without FG) |
| I | H + FG + skip 1 level >= 1024 | 7.3 / 20.1 / 21.9 | 5.3-5.6 GB | no error in 10 min, textures too soft |
| **J** | **H + FG + skip 1 level >= 2048, line 82 %** | **11.7 / 21.4 / 24.7** | **5.7 GB of 7.03, flat** | **no error in 8 min, all textures in** |
| J' | J, paging file on NVMe | 16.4 / 20.5 / 23.6 | same | no error; dips 40 % shallower |

## The launcher and its options

`PLAY_SMALL.cmd` (one click, arguments go on) runs `scripts\play-small.ps1`, which sets the switches above and goes
on to `scripts\play-foxhound.ps1` (it builds the layer if `target\release\victory.exe` is missing). The game folder
comes from `game_dir` in `victory.toml` (or `VICTORY_GAME_DIR`, or `-Exe`).

| Option | Default | Switch | What |
|---|---|---|---|
| `-Exe <path>` | from `victory.toml` | | the title's executable |
| `-Dlss off\|dlaa\|quality\|balanced\|performance` | `performance` | `VICTORY_DLSS_MODE` | DLSS mode (step 2) |
| `-NoFrameGen` | FG 2x on | `VICTORY_FRAMEGEN` | frame generation off; the line then defaults to 85 % |
| `-RoomPct <n>` | 82 (85 without FG) | `VICTORY_TEXTURE_ROOM_PERCENT` | texture line (step 7) |
| `-SkipLevels <0-4>` | 1 | `VICTORY_TEXTURE_SKIP_LEVELS` | largest levels left out (step 6); 0: full textures |
| `-SkipMin <texels>` | 2048 | `VICTORY_TEXTURE_SKIP_MIN` | smallest level side left out (step 6) |
| `-Pool` | pool off | `VICTORY_TEXTURE_POOL` | turn the texture pool back on (step 5) |
| `-BarMB <MB>` | 1024 | `VICTORY_WINDOWS_BAR_MB` | title windows in BAR memory (step 3); 0: off |
| `-TargetsMB <MB>` | 1536 | `VICTORY_CARD_ROOM_FOR_TARGETS_MB` | render target reserve (step 4) |
| `-Windowed` | full screen | `VICTORY_FULLSCREEN` | leave the window as the title makes it |
| `-High` | normal | | High process priority once the game is up |
| `-Preload` | off | | `victory preload` with these switches first (every shader translated, the known pipelines made, the loading ones stored in the pipeline library), then the game; once per new build or driver |
| `-More @{...}` | | any | any other switch, as `play-foxhound.ps1 -More` |

Things to know:

- The F1 menu's saved choices (`victory-settings.txt`) still go over these at every start. On such a PC do not
  switch the console to Series X or frame generation above 2x from the menu; delete the file to go back.
- On a 16 GB PC, close the browser and other large programs before starting: every GB of RAM is paging avoided.
- The first start after a new build or driver stutters while pipelines are made. `PLAY_SMALL.cmd -Preload` makes
  them first, with the same switches (the translations depend on them): on the test machine 88,608 pipelines in
  22 minutes plus 5,462 into the pipeline library, 38.6 minutes in all, once. Stopped, it goes on where it was. The
  NVIDIA driver keeps its cache per program path: keep `target\release\victory.exe` where it is.
- A 6 GB card would start from `-SkipMin 1024 -RoomPct 75`, or `-NoFrameGen`; not tested.

## What is left, and ideas

- **RAM is the remaining limit.** Dips to 10-16 fps are paging; 32 GB removes them and doubles the shared segment
  cap. On 16 GB, the paging file belongs on the fastest drive.
- **An eviction of textures under VRAM pressure** would replace step 6 with something that keeps full quality where
  it is seen: the shader translator could record, per frame, which bindless slots a draw reads (a feedback buffer
  written by the translated `image_sample`/`image_load` with the slot index), and `let_go_idle` could then let go of
  copies not read for some seconds even while a slot still names them, putting the neutral texture in the slot until
  it is read again.
- **Step 4's reserve** and **step 5's pool holes** may be worth a look on bigger cards too: the holes do not depend
  on the card's size, and the reserve is never given back wherever the targets stay under half of it (not measured
  here on anything but this 8 GB card).
- Defaults could be picked from the adapter's budget at start (`card_limits` already scales some by it): below
  10 GB, pool off, target reserve from the profile, line 82 % with FG, skip 1 level >= 2048.
