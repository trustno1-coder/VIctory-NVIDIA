# Ryzen 9 9900X, RTX 3070 Ti 8 GB, 32 GB DDR5

What the layer does for this kind of PC on its own, and the settings to
start from. Not measured on this exact PC yet: compare with a log of each run
(F3 shows frame rate, VRAM and RAM) and report what you find.

## What the code does by itself

- **The layer's frame threads stay on one die of the 9900X.** Its twelve
  cores sit on two dies (CCDs) of six, each with its own 32 MB L3. The
  threads a frame waits for hand their work to each other; across the two
  dies every hand-over goes over the link between them, at several times the
  latency. They are now kept on the die of the system's preferred cores
  (`crates/victory-gpu/src/cores.rs`, `one_cache_of`). The title's own
  threads and the list translators keep both dies. `VICTORY_ONE_CCD=0`
  turns it off, to compare. Single-die Ryzens (9700X, 9800X3D) and Intel's
  hybrid cores are not affected.
- **8 GB cards keep less room for render targets.** The texture queue held
  back a flat 3,072 MB for render targets not made yet, measured on 12 GB
  cards. On an 8 GB card that is 40-60 % of the budget, and the picture
  stayed black while textures waited for room (docs/windows-memory.md). It
  is now at most three tenths of the budget (about 2.1 GB of a 7.2 GB
  budget); 12 GB cards and up are unchanged.
  `VICTORY_CARD_ROOM_FOR_TARGETS_MB=<n>` still sets it by hand.
- **32 GB of RAM counts as limited**, so 8 pipeline workers at most and
  smaller idle caches (already so before).

## Settings to start from

| | |
|---|---|
| Console profile | **Series S** (F1, CONSOLE). Series X at 1440p needs more than 8 GB of VRAM for its targets and textures together |
| DLSS | **Quality** (720p inside, 1080p out); Performance (540p) if the GPU is the limit |
| Frame generation | **FSR 3, 2x** (DLSS frame generation needs an RTX 40). It takes 1.5-2 GB of VRAM: off if textures come late or the F3 panel shows VRAM over the budget |
| Frame rate cap | 60, or your monitor's rate with frame generation |
| Build | `-Build native` (below) |

```powershell
.\scripts\play-nvidia.ps1 -Exe 'D:\Games\MyDump\game_scarlett_master.exe' -SeriesS -Dlss quality -Build native
```

`victory.exe` alone: `victory.exe play --series-s --dlss quality --fps 60`.

## A build for this CPU

`-Build native` builds `target\native\victory.exe` with the CPU's own
instructions (`-C target-cpu=native`: AVX-512 on Zen 5) and one optimisation
over the whole program (`[profile.native]` in `Cargo.toml`: fat LTO, one code
generation unit). It takes several minutes more to build and runs only on a
CPU with the same instructions. It helps the CPU side (shader translation,
hashing, copies), not the GPU. By hand:

```powershell
$env:RUSTFLAGS = '-C target-cpu=native'; cargo build --locked --profile native -p victory
```

## Windows

- **A page file**, managed by Windows. With 32 GB of RAM and none, the first
  scene load ran out of commit (docs/windows-memory.md).
- **AMD chipset driver** installed: it tells Windows which cores are
  preferred, and so which die the layer's threads keep.
- **Resizable BAR** on in the BIOS (an RTX 30 supports it) and
  **hardware-accelerated GPU scheduling** on (needed for frame generation).
- **EXPO** on for the DDR5, so it runs at its rated speed.
- Close programs that hold VRAM (browsers with video, overlays): every MB of
  the 8 GB counts.
