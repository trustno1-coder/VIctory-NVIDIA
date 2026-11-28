# RTX 3060 Ti / i7-11700KF: stable 30 FPS experiment

This is a measurement profile, not a promise that every scene will hold 30 FPS.
The target is **30 rendered frames per second** (33.3 ms per frame), with
frame generation disabled until the base render rate is stable.

## Hardware target

- GPU: GeForce RTX 3060 Ti, 8 GB
- CPU: Intel Core i7-11700KF (8 cores / 16 threads)
- RAM: 32 GB DDR4-3200
- Display: 2560x1440; render at a lower internal resolution

## First test profile (F1 menu)

1. Frame rate cap: **30**.
2. Console: **Series S (Faster)**.
3. Series S picture size: **540p / Fastest** (the lowest documented profile).
4. Ray tracing: **Off**.
5. Water light: **Off** (the project documents a costly water-light path).
6. Reflection estimate: **Off**.
7. Keep frame generation **Off** for the first benchmark.
8. Use DLSS upscaling only if the selected profile supports it and the image is
   still acceptable. Do not confuse DLSS upscaling with DLSS frame generation:
   RTX 30 cards use FSR 3 for supported frame generation, at 2x maximum.

The menu saves settings to `victory-settings.txt`; use its own save/restart
action when a setting requires restart.

## Benchmark procedure

Use the same gameplay location and camera for each test. Let shader warming
finish before recording. Open the F3 stats panel and record at least three
60-second runs:

- rendered FPS and frame time;
- GPU busy time versus submitting-thread/CPU time;
- draw count;
- VRAM and system memory;
- whether stutters occur during traversal or only on first encounter.

A 30 FPS cap is a ceiling, not a performance boost. If the uncapped render rate
falls below 30, lowering output resolution further may not help when the
submitting thread is the bottleneck.

## How to interpret results

- **GPU busy near/above 33.3 ms, CPU submission lower:** reduce internal
  resolution and expensive GPU passes first.
- **CPU/submission near/above 33.3 ms, GPU has headroom:** resolution changes
  are unlikely to solve it. Investigate draw submission, descriptor work,
  synchronization and worker scheduling with a trace before changing thread
  priorities.
- **Both below 33.3 ms but pacing is uneven:** investigate shader compilation,
  cache warm-up and synchronization spikes.
- **VRAM near its limit:** test texture/mip and allocation behavior; do not
  blindly shrink every texture because that can increase streaming churn.

## Frame generation (optional follow-up)

Only after the base rendered frame rate is consistently near 30, test FSR 3
frame generation at 2x. RTX 3060 Ti does not support NVIDIA DLSS frame
generation natively. Frame generation can make motion appear smoother, but
does not turn a low or unstable base render rate into true 30 FPS and adds
latency/artifacts. Compare it on and off in the same scene.

## CPU-specific note

The i7-11700KF is an 8-core/16-thread CPU without Intel hybrid P/E cores.
The project's fast-core selection is designed for processors exposing
different efficiency classes; on a homogeneous CPU it should find no separate
fast-core set. Do not force affinity or raise every worker's priority without
measurements: the game and translation workers compete for the same cores.

## Safety and scope

This profile changes no game assets and does not promise a 30 FPS result.
Keep the original executable and game dump untouched. The repository does not
contain the game itself; use only files you lawfully obtained.
