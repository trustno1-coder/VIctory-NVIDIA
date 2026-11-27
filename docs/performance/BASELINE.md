# Baseline from the sessions already recorded

Every number below was computed by `scripts/perf/summarize_run.py` from files on this PC; nothing is estimated.
Per-run output: `docs/performance/baseline-data/<run>.summary.{md,json}`, one-row-per-run table:
`docs/performance/baseline-data/index.md`. Regenerate with

    python scripts/perf/summarize_run.py "%LOCALAPPDATA%\victory\runs\2026*" "%LOCALAPPDATA%\victory\logs\play-*.log" ^
        "%LOCALAPPDATA%\victory\cache\frametime.csv" --out docs\performance\baseline-data

Times in the files are UTC; the PC is UTC-3 (run folder names are local time).

## Read this first: what these sessions are, and are not

- **There is no controlled baseline.** No two sessions are labelled as the same scenario, the build/commit is not
  recorded (`build unknown` in every log, `game: unknown` in every CSV header), and the route after world entry is not
  recorded. Treat the figures as a survey of what the user's machine did, not as a reference build.
- **Two different builds produced the data.**
  - Dataset A, 46 run folders `%LOCALAPPDATA%\victory\runs\20261126-123718 ... 20261126-211141` (Nov 26, 12:37 to 21:17
    local) plus `cache\frametime.csv`. These have `frametime.csv` (1 s windows) and a rich `victory.log`. They came from a
    victory.exe with instrumentation that is **not in this branch**: `VICTORY_FRAMETIME_DUMP`, `VICTORY_AUTO_A`,
    `VICTORY_GPU_SPANS`, the `stall:` watchdog, the `passswitch`/`gfxswitch` lines, "submission preparing per frame" and
    "frame activity" lines exist neither in the source tree (`git log --all -S` finds none) nor in the strings of
    `target\release\victory.exe` or `Extracted\victory.exe`. The launcher that sets them is the sibling project
    `C:\Users\mathe\orca\workspaces\AnyXboxX\squirrelfish` (`--auto-a`, `--frametime-dump`, `--override KEY=VALUE`).
  - Dataset B, 5 logs `%LOCALAPPDATA%\victory\logs\play-20261127-*.log` (Nov 27, 06:30 to 06:57 local), from the
    `victory play` build of this lineage. No CSV; only the frame-rate line every ~5 s and the in-tree per-frame lines.
    Tail figures (P99, 1% low, stall windows) therefore cannot be computed for B; its "P99" is the P99 of 5 s averages.
- Hardware (CSV headers of dataset A; Streamline lines of B agree on the GPU): AMD Ryzen 7 5700X (16 logical), NVIDIA
  GeForce RTX 4070 12 GB, driver 621.01, 32 GB RAM, Windows 11 25H2 (build 26200), window 1707x1067. This is **not** the
  machine of `docs/findings.md` (i9-12900K, RX 7900 XTX); its numbers are not comparable.
- The game dump is `C:\Users\mathe\Downloads\GTA 6\Extracted` (`game_scarlett_master.exe`).
- Labels the files do give: console profile (`# console=` line / log `console` line), DLSS mode and frame-gen (B only,
  from the switch list), the first-window switch list (`# switches:` in the CSV, which lists only variables set in the
  environment), shader-translation cache hits/misses (log), and ablations switched while running (A only, `passswitch`
  lines). What they do **not** give: cold/warm for the driver's pipeline cache (D3DSCache), which route was played, when A
  was pressed, whether `victory-settings.txt` (F1 menu choices, applied over the launcher at every start) held overrides.

## Definitions used

| term | meaning |
|---|---|
| window | one CSV row (~1 s) or one log frame line (~5 s in B) |
| heavy scene | windows whose mean draws/frame is 1000 or more. Boot and menus sit at 0-13 draws; the title's loading screen at about 300 in some runs. "World entry" = three such windows in a row |
| frame time percentiles (A) | reconstructed per window from its mean and its **max** (one frame at the max, the rest at the remaining mean): exact in frame count, total time and max, a model in between. P99 and "1% low" (= 1000 / P99 ms) are therefore indicative |
| stall window (A) | a window whose slowest frame is at least max(200 ms, 3 x the span's median ms/frame). The stalled time sums the maxima, a lower bound |
| watchdog stall (A) | `stall: the frame has not advanced` lines (first fires at 400 ms), grouped by frame number; length = largest `stalled_ms`; cause = the `inside:` text |
| stabilised | first window at least 30 s after world entry whose next 10 windows are all within +-25 % of the second-half median of the heavy windows |
| per-subsystem ms | summed over all threads (they can exceed the frame time); mean over the log lines at or after world entry |

## Dataset A, 46 runs: where a frame goes (medians over runs; min-max in brackets; n = runs that have the figure)

36 of the 46 runs reached the heavy scene (the others were quit in boot or loading).

| metric | median | range | n | source |
|---|---|---|---|---|
| heavy scene, mean fps | 6.25 | 1.7 - 18.3 | 36 | CSV `frames`, `ms_per_frame` |
| heavy scene, median frame ms | 122 | 49 - 480 | 36 | CSV (reconstructed) |
| heavy scene, P90 frame ms | 219 | 63 - 1107 | 36 | CSV (reconstructed) |
| heavy scene, P99 frame ms | 833 | 116 - 1744 | 36 | CSV (reconstructed) |
| heavy scene, 1% low fps (1000/P99) | 1.2 | 0.6 - 8.6 | 36 | CSV (reconstructed) |
| heavy scene, stall windows per run | 35 | 0 - 142 | 36 | CSV `frame_ms_max` |
| session length | 322 s | 66 - 857 s | 46 | CSV |
| seconds from log start to heavy scene | 137 | 64 - 324 | 34 | CSV `draws` (includes the wait for A and the title's loading) |
| seconds from entry to "stabilised" | 76 | 30 - 433 | 27 | CSV; 9 of 36 heavy runs never met the rule |
| seconds until `making` (pipelines being made) reaches 0 for good | 43 | 28 - 685 | 46 | CSV `making` |
| CPU prepare ms/frame (the "preparing" figure) | 84 | 11 - 181 | 36 | CSV `cpu_prepare_ms` |
| draws per frame | 4331 | 1085 - 5808 | 36 | CSV `draws` |
| title copies MB/frame | 159 | 4.7 - 268 | 36 | CSV `copies_mb` |
| textures MB/frame | 14.6 | 0 - 156 | 36 | CSV `textures_mb` |
| copy-back MB/frame | 40.6 | 0 - 50.8 | 36 | CSV `copyback_mb` |
| card sent-up MB/frame | 57.3 | 7.3 - 103 | 36 | CSV `card_sent_mb` |
| card fetched MB/frame | 61.4 | 0 - 101 | 36 | CSV `card_fetched_mb` |
| RAM max (process, MB) | 20692 | 14008 - 24108 | 36 | CSV `ram_mb` |
| submissions per presented frame | 25.8 | 2.6 - 29.7 | 46 | log `frame activity` counters |
| descriptor slots looked at per frame | 205,872 | 7,028 - 406,781 | 40 | log `descriptor records ...` |
| descriptor slots mirrored per frame | 9,238 | 352 - 34,410 | 40 | same |
| card busy % ("by this count") | 41 | 4.7 - 67 | 46 | log `the card's clock per frame` |
| card title-list time ms/frame (GPU timestamps) | 32.3 | 2.1 - 231 | 42 | log `gpuspan` |
| prep: before the steps (write watch etc.) ms | 12.7 | 0 - 38 | 46 | log `submission preparing` |
| prep: card copies ms | 16.4 | 0 - 126 | 46 | same |
| relay ms (thread sum) | 37.0 | 0.2 - 111 | 46 | log `frame activity` |
| present ms (thread sum, includes waits) | 21.8 | 1.9 - 114 | 46 | same |
| watchdog stalls per run | 31.5 | 8 - 136 | 46 | log `stall:` |
| watchdog stalled ms per run | 34,020 | 7,656 - 75,587 | 46 | same |

Notes on these figures:

- `gpu_ms` / `gpu_pct` in the CSV are near zero in every run (median 0.13 ms, 0.07 %), while the log says the card was
  41 % busy by count and 32 ms of title lists ran per frame. The CSV GPU columns are **not usable**; use the log lines.
- GPU is not the limit in the last run: its heavy-scene mean of the log's "card idle" figure is 154 ms per frame against a
  193.8 ms mean frame (`the card's clock per frame` line, run `20261126-211141`). The pattern (CPU-side noticing and moving memory) matches
  `docs/performance/ROOT_CAUSES.md`.
- Thread-summed subsystem times do not add to the frame; use them to rank, not to subtract.
- Different eras of the day differ more than run noise (heavy-scene mean fps, from `index.md`): 12:37-14:56 runs 5.2-7.6,
  15:33-17:22 runs 3.0-3.8, 17:53-18:32 runs 6.1-7.5. Code changed during the day and the build is not recorded, so the
  cause is unknown.
- Late runs have ablations on (`passswitch` lines): 20:11-21:11 runs switch passes such as `buffer_refresh`,
  `heap_textures`, `texture_refresh`, `descriptors`, `heap_buffers` off from the F1 menu. The run's final state is the
  `passes off at end` column in `index.md`. Two runs finished with everything on: `20261126-201209-588-...` (heavy 9.6 fps,
  only 22 heavy windows) and `20261126-210725-018-...` (6.8 fps, 35 heavy windows).

### Stall causes (watchdog, summed over the 46 runs)

| inside (what the threads stood in) | stalls | total ms |
|---|---|---|
| no section of ours (the title or the system is the wait) | 1096 | 739,697 |
| heap pass: buffers + mirror loop | 36 | 324,434 |
| heap pass: buffers + mirror loop + descriptor heap sync | 14 | 124,222 |
| relay: landing flights | 289 | 117,868 |
| flush of refreshes | 90 | 86,148 |
| present | 79 | 70,504 |
| settle (wait for the card) + write-watch dependencies | 2 | 38,107 |
| card copies (card_before) | 26 | 17,789 |

Of the "no section of ours" stalls, the watchdog's own text says the title or the system is waiting; nothing in the log
says on what (file, pipeline creation, DirectStorage). The 5-8 s heap-pass stalls are a single bad frame each.

### One latest run in full (`20261126-211141-508-15168-6d931aa6`, series-x, no DLSS, passes off at end: heap_textures, buffer_refresh, texture_refresh)

| span | fps | ms mean | ms median | P90 | P99 | worst |
|---|---|---|---|---|---|---|
| whole session, 322 s | 11.5 | 87.0 | 68.2 | 187 | 280 | 6790 |
| heavy scene, 110 s | 5.16 | 193.8 | 189.3 | 209 | 280 | 724 |
| entry + 30 s onward, 79 s | 5.35 | 187.0 | 186.0 | 197 | 213 | 240 |

Time to heavy scene 214 s, stabilised 31 s after entry. CPU prepare 180.9 ms/frame, 27 submissions/frame, 2797 draws/frame.
Do **not** use this run as "the baseline": three core passes were off, which changes what it measures.

## Dataset B, 5 sessions of Nov 27 (`play-*.log`, frame line every ~5 s, rendered fps)

| log | profile | DLSS | frame-gen | shader cache | heavy windows | heavy fps | heavy ms mean | P99 of 5 s means | s to heavy | warn lines |
|---|---|---|---|---|---|---|---|---|---|---|
| play-20261127-093037-23504 | series-x, native 1440p | quality | off (ready) | cold (2717 translated) | 77 | 12.35 | 81.0 | 314.9 | 69.5 | 414 |
| play-20261127-093851-14116 | series-x, native 1440p | quality | off (ready) | warm (1 miss) | 33 | 8.48 | 117.9 | 380.3 | 70.9 | 392 |
| play-20261127-094254-19348 | series-s | balanced | x2 | warm (5 misses) | 26 | 7.97 | 125.5 | 529.6 | 50.4 | 335 |
| play-20261127-094655-17644 | series-s | balanced | x2 | warm (0 misses) | 44 | 16.76 | 59.7 | 125.8 | 141.9 | 319 |
| play-20261127-095317-11396 | series-x, native 1440p | balanced | x2 | warm (0 misses) | 50 | 8.25 | 121.2 | 594.1 | 87.9 | 259 |

Frame-line means from world entry (same logs): GPU ms/frame 12-21, copies 35-48 MB/frame, textures 25-43 MB/frame,
copy-back 0.4-0.8 MB/frame, draws 3109-6062/frame, dropped draws 440-763/frame, card busy 48-79 % by count.
Only the series-s run 094655 got above 15 fps heavy. `fps` here is rendered fps (the panel multiplies by the frame-gen
factor). The cold-cache log (093037) ran 12.35 fps heavy and still needed 129 s after entry to stabilise, so a cold
shader-translation cache is not what dominated it; the driver pipeline cache state is unknown for all five.

## What can and cannot be claimed from this

- Heavy-scene frame time on this PC is 100-200 ms (5-10 fps) in the typical run, with a median P99 of 833 ms (range
  116-1744 ms) across runs; the in-world frame is CPU/submission-bound (card idle about four fifths of the frame in the latest run).
- World entry to steady frame rate takes about 30-430 s (median 76 s, 27 runs), consistent with "late loading after
  world entry" in `ROOT_CAUSES.md`; the first ~64-324 s of each session is boot, menu and the title's own loading.
- Run-to-run spread is wider than any single optimisation is expected to give. Any A/B needs the protocol in
  `docs/performance/AGENT_REPORTS/07-profiling.md`, section 5: same scenario, at least 3 interleaved runs per
  arm, compare medians, and treat differences under about 5 % as noise (`docs/findings.md`).
