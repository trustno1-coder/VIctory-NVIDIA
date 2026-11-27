# 07 Profiling and experimental validation

Scope: scripts and docs only; no `.rs` edited, nothing built, no game run. Numbers: `docs/performance/BASELINE.md`.

## 1. Tool delivered

`scripts/perf/summarize_run.py` (Python 3 standard library only).

    python scripts/perf/summarize_run.py <run dir | victory.log | play-*.log | frametime.csv> [...] [--out DIR] [--label TEXT]
    python scripts/perf/summarize_run.py --compare <A> <B>        # run dirs or *.summary.json, prints a delta table
    python scripts/perf/summarize_run.py --latest 3               # newest runs under %LOCALAPPDATA%\victory\runs
    options: --stall-ms 200 --stall-rel 3 --heavy-draws 1000 --warmup-s 30

It writes `<run>.summary.json` and `.md` (and `index.md` for several inputs, with median/min/max rows). It emits: fps mean,
mean/median/P90/P99 frame ms, 1% low (two definitions), worst frame, stall windows (threshold and lower-bound duration),
watchdog stalls grouped by cause, phases (pipelines done, scene start, heavy-scene entry, "stabilised"), frame-weighted CSV
columns (CPU prepare, GPU, draws, MB copied/textured/copied back/sent/fetched, RAM, CPU %), log subsystem means after world
entry (submission preparing, frame activity, descriptor sync and slots, card clock/busy %, relay, sent-ahead, buffer refresh,
card copies, GPU spans, DirectStorage MB/s), submissions per presented frame, and run labels. The `definitions` list inside
every summary says exactly how each figure is computed. Without a CSV (play logs) it rebuilds windows from the 5 s frame line
and says that tail figures are unavailable.

Labels it fills from the files: console profile, build profile (launcher line), DLSS mode, frame-gen factor, native-1440p switch,
panel on/off, `VICTORY_AUTO_A`, shader-translation cache cold/warm (`shaders from the cache and translated so far hits= misses=`),
final pass/graphics-method switch state, hardware header. `--label` stores free text (scenario, A press time, cold/warm).
Not derivable from any file today: scenario/route, A-press time, commit, driver pipeline-cache state (see section 4).

## 2. Findings that change how the data may be used

1. **The recorded runs are not from this branch's instrumentation.** `VICTORY_FRAMETIME_DUMP`, `VICTORY_AUTO_A`,
   `VICTORY_GPU_SPANS`, the `stall:` watchdog, `passswitch`/`gfxswitch`, "submission preparing per frame", "frame activity" are
   absent from the source (`grep`, `git log --all -S`) and from the strings of `target\release\victory.exe` and
   `Extracted\victory.exe`. They live in a fork victory.exe driven by `C:\Users\mathe\orca\workspaces\AnyXboxX\squirrelfish`
   (`--auto-a` = `VICTORY_AUTO_A=150`, `--frametime-dump`, `--override KEY=VALUE`, `--diag`, `--warm`). On this branch a
   run produces only `logs\play-*.log` with one frame line per 5 s plus the in-tree per-frame lines. Consequence: after a change
   to the branch you cannot reproduce the CSV-based baseline until the writer is ported (proposal P1).
2. `build` is `unknown` and the CSV `game:` is `unknown` in every file; no commit is recorded anywhere. Runs cannot be tied to
   code state, so "before" in an A/B must be re-measured, not taken from history.
3. CSV `gpu_ms`/`gpu_pct` are ~0 while the log's card clock says 41 % busy: do not use the CSV GPU columns.
4. `victory-settings.txt` (F1 menu, `%LOCALAPPDATA%\victory`) is applied over launcher settings at every start. It currently holds
   `VICTORY_MENU_DLSS=balanced`, `VICTORY_MENU_FRAMEGEN=2`, `VICTORY_PANEL=1`, `VICTORY_PORT_CONSTANTS=` (unset),
   `VICTORY_MENU_CONSOLE=series-x`. The panel (`VICTORY_PANEL=1`) "costs some" (`docs/findings.md`). A run is only comparable if this
   file is identical or deleted; copy it next to the run.
5. Some Nov 26 evening runs have core passes switched off from the F1 menu (descriptors, heap_buffers, heap_textures,
   buffer_refresh, texture_refresh). The `passes off at end` column flags them. Never mix them into a baseline.
6. Heavy scene is only reached after 64-324 s (median 137 s) because of boot, the A press and the title's loading; and A-to-stable
   takes a median 76 s more. Sessions shorter than about 6 minutes give few steady windows (median 136 heavy windows over 36 runs).

## 3. Inventory of instrumentation in this tree (what exists, how to use it)

Measuring and recording

| switch / facility | what it gives | cost / notes |
|---|---|---|
| frame-rate line (always, every ~5 s, `host.rs` ~9406) | fps, ms/frame, preparing, copies/textures ms+MB, GPU ms, copy-back, draws, dropped, pipelines being made | in `logs\play-*.log`; the only per-run time series on this branch |
| `VICTORY_FPS_LINE=0/1` | fps shown at the top of the picture | F1 menu |
| `VICTORY_PANEL=1`, F3 | on-screen numbers (updates twice a second); in screenshots | costs a little; keep off for A/B |
| per-frame log lines (`host.rs`, `descriptor`, `queue`) | card clock/busy, relay, submissions sent ahead, shader cache hits/misses, copies in card memory, buffer refresh | in-tree subset only |
| `VICTORY_LOG`, `VICTORY_LOG_FILE` | log filter / path | |
| `VICTORY_TIMELINE=<file>` (+ `_AT` s, `_FRAMES`), diag build | Chrome/Perfetto trace of N frames: spans, waits and who woke them, card timestamps, text chain of work behind each present | `--features diagnostics`; the best stall-attribution tool, but manual and triggered by time, not by a bad frame |
| `VICTORY_GPU_TIMING` (diag) | each list timed on its own, slow ones logged | disables `VICTORY_LATE_WAIT` (changes behaviour) |
| `VICTORY_TIMER_LOG=1` | frame timer statistics | |
| `VICTORY_MEMORY_MAP` | memory listing (on unless 0) | |
| `VICTORY_RECORD=<folder>` (+`_AT`, `_FRAMES`, `go` file) and `victory replay <folder> [--loops N] [--timing detailed] [--compare ...]` | one captured frame replayed without the title: card-side times per submission/command, structural experiments in seconds (`--skip-hash-passes`, `--no-barriers`, `--buffers vram`, `--skip-shader`, `--shaders`, ...) | diag build; capture stands still ~5 s and is GBs; cannot test watch/guard/landing logic or multi-frame state (`docs/replay.md`, review 08) |
| `VICTORY_DSTORAGE_LOG/_TRACE`, `VICTORY_STREAMLINE_LOG`, `VICTORY_D3D12_DRED` | streaming, DLSS, device-removed traces | |
| `VICTORY_UPCOPY_CHECK`, `faultbook`, `scripts/faultbook.py` | correctness checks and fault catalogue | |

Ablation and A/B

| mechanism | use | limits |
|---|---|---|
| process-level env switches | `play-foxhound.ps1 -More @{VICTORY_X='0'}`; `victory play` settings; squirrelfish `--override KEY=VALUE` (repeatable) | ~340 behaviour switches (review 08), six default idioms; the effective value is not logged |
| `VICTORY_AB=<seconds>,<SWITCH>[,<SWITCH>...]` (diag build) | alternates up to 8 switches on/off inside one run every N seconds and logs each phase's frame times (first second dropped); all 2^n combinations in turn | only switches read through `toggled!` (10 today: GUARD_REST, HELPER_PRIORITY, LAND_ECHO, LIST_VIEWS, OPEN_ONE_VIEW, QUICK_FIND, READY_ONCE, SAME_RECORDS, TAKE_PARTS, UNDER_WIDE); scene must be static, so it measures steady state only, not loading |
| F1 menu | live toggles of DLSS, frame-gen, fps cap, panel, ambient, WC_NO_BACK, RELAY, CARD_STEPS, INDIRECT_BATCH, CARD_BUFFERS_MIN_KB, LEAVE_OUT, SOFT_VIEWS, TEXTURE_NO_WAIT, EARLY_DEPTH ... | changes persist into `victory-settings.txt` (confounder) |
| `VICTORY_LEAVE_OUT=<hashes>`, `VICTORY_SKIP_PIPELINES=<hashes>` | leave out shader programs | picture changes |
| structural ablations known from docs | `VICTORY_THIN=0`, `RELAY=0/1/2`, `LATE_WAIT=0/1/2`, `ASYNC_SUBMIT=0`, `WC_NO_BACK`, `TEXTURE_BACK=0`, `JOIN_COPIES`, `CARD_STEPS`, `CARD_BUFFERS_MIN_KB`, `INDIRECT_REST`, `LEVELS_ON_DEMAND`, `PIPELINE_WAIT_MS`, `TEXTURE_BACKLOG_MS`, `SHADER_OPT` | each is described where read; the audit (`ARCHITECTURE_AUDIT.md` section 6, C1-C9) lists which are real invariants |
| pass-level switch (`passswitch`/`gfxswitch`, `VICTORY_GFX`, `gfx.txt`) | turns whole emulator passes (descriptors, heap_buffers, heap_textures, buffer_refresh, texture_refresh) and graphics methods off at run time | **fork build only**; exists in dataset A logs, not here. Valuable for "how much of the frame is pass X" (lower bound of its cost, picture may break) |

### How to run a structural ablation (process level)

1. Fix everything else (section 5), pick arm A (current) and arm B (one switch changed; never two).
2. `scripts\play-foxhound.ps1 -Exe '<Extracted\game_scarlett_master.exe>' -Dlss off -More @{VICTORY_RELAY='1'}` for B; plain for A.
   (or `victory play ... ` with the same settings file; or squirrelfish `--auto-a --frametime-dump --override VICTORY_RELAY=1`.)
3. Interleave A B A B A B (3 each minimum). Summarize each (`--out docs\performance\runs\<tag>`, `--label "A, cold=no"`), then
   `--compare` the median-ish runs, and compare the **per-run heavy medians across arms** rather than one pair.
4. Expect each run to spend 2-5 minutes in boot/load, so for steady-state-only questions use `VICTORY_AB` in one run, or record
   a frame and use `victory replay --compare` (card-side only).

## 4. What is missing, and the minimal additions proposed (not implemented; `.rs` is out of scope)

| # | gap | evidence | minimal addition |
|---|---|---|---|
| P1 | per-run time series on this branch | frametime.csv writer, stall watchdog, GPU spans not in tree; branch logs are 5 s averages | port the fork's CSV writer (1 row/s: frames, avg, max, cpu_prepare, draws, MB columns, ram, cpu %) behind `VICTORY_FRAMETIME_DUMP`; same file format, so `summarize_run.py` works unchanged |
| P2 | true per-frame times | CSV keeps one max per second; P99 and 1% low are reconstructed | ring buffer of present-to-present microseconds (u32 per frame, ~10 MB per hour), dumped at exit as `frames.bin/csv`; summarize_run would use it when present |
| P3 | load-phase timeline | "time to heavy scene" can only be inferred from `draws`; A press and loading screen end are unknown | one `phase:` log line (epoch ms) at: process start, device ready, first present, first A/button input seen by GameInput, first frame with draws >= 1000, texture backlog empty (`HEAP_TEXTURES_WAITING == 0`), pipelines being made == 0, DirectStorage idle. Also add `tex_waiting`, `making`, `dstorage_mb_s` columns to the CSV so the drain curve is visible |
| P4 | stall attribution | watchdog fires only on "frame has not advanced" >= 400 ms; 1096 of ~1600 stalls say "no section of ours" and give no cause | (a) on any present delta over a threshold (e.g. 250 ms) log the threads' current spans/waits (data the timeline already has); (b) auto-trigger `VICTORY_TIMELINE` on the first frame over 500 ms, once per minute; (c) tag waits in the title's imports (file open, `CreateGraphicsPipelineState`, DirectStorage, fence) with a counter per frame |
| P5 | GPU time | CSV gpu columns ~0; `VICTORY_GPU_TIMING` is diag-only and alters behaviour | two timestamp queries per present around the title's submissions (first list start, last list end) resolved a frame later: GPU busy ms and idle gap ms per frame, always on, in the CSV |
| P6 | run manifest | `build unknown`, `game unknown`, no commit, no resolved switch list (the CSV lists only env-set ones) | first log line: git describe + dirty flag, exe hash, profile, **all effective `VICTORY_*` values after defaults and `victory-settings.txt`** (this is also the "resolved config dump" of review 08), driver, adapter, window size |
| P7 | scenario markers | no way to say "route started/ended" | hotkey (e.g. F9) writing `mark:` line with epoch ms into the log/CSV; summarize_run would split spans on marks |
| P8 | A press | manual and untimed; `VICTORY_AUTO_A=150` exists only in the fork | port `VICTORY_AUTO_A` (tap A for N s) so runs are reproducible, and log the first accepted press |
| P9 | cold/warm of the driver pipeline cache | only the shader translation cache is reported | log whether `cache\pipelines` held a library at start and its size; count driver pipeline compiles after world entry (ROOT_CAUSES item 2) with ms |
| P10 | frame-gen aware metrics | frame line fps is rendered fps | also log presented fps and frame-gen factor so DLSS-FG runs compare |

Priority for validating the planned work: P1, P6, P3 (cheap, unlock every comparison), then P4 and P8, then P2, P5.

## 5. Manual protocol for comparable runs (the user follows this exactly)

Fixed conditions
1. Reboot, or at least close browsers, launchers, overlays, capture software and anything GPU-heavy. Same power plan (high
   performance), same driver (621.01 recorded), same monitor/window size, controller connected before launch, no alt-tab, no
   screenshots, no recording software during the run.
2. Settings file: decide once. For A/B delete `%LOCALAPPDATA%\victory\victory-settings.txt` (or keep it identical for all runs and
   copy it to the run folder). Keep `VICTORY_PANEL` off and the F1 menu closed after the start.
3. Same build for both arms except the one change under test. Write down the commit (`git rev-parse --short HEAD`, `git status`)
   and the exe path/timestamp; the files do not record it.
4. Same arguments every time: record the exact command line. Series X, one DLSS mode, one frame-gen setting; do not change
   them mid-session.
5. Do not start another run within 2 minutes of the last one ending (let the system settle); never run two at once.

Cold and warm
6. **Warm** = shader and pipeline caches populated by a previous run of the same build. **Cold** = point `VICTORY_CACHE_DIR` at an
   empty folder (the shader and pipeline caches live under `cache\`), or `victory warm` before for a deliberately warmed state.
   Run order for any new build: 1 discarded warm-up run (label `warmup`, excluded), then measured runs. Report cold and warm
   separately; never mix them in one median. The summary prints the shader-translation cache state; the driver cache state is not
   logged yet (P9), so write "warm"/"cold" in `--label`.

Per run (about 8 minutes)
7. Start the game with the fixed command. Note the wall-clock time.
8. **When the title reaches its start screen/menu, press A on the controller once to leave it** (the game does not proceed
   until A is pressed; the same press may be needed again on the next screens). Press it as soon as the screen is up, and note the
   wall-clock time of the press. If a second prompt appears, press A at once again and note it. Do not touch any other
   input until the world is loaded.
9. Wait for the world to load without input. When the first controllable frame appears, start a stopwatch (this is "entry").
10. Do exactly the same scenario every run. Suggested minimum, scenario S1 (no input): stand still for 3 minutes after entry,
    camera untouched. Optional scenario S2: after S1, follow one fixed route of 2 minutes (same path, same speed, same
    time of day). Do not fight, drive or trigger cutscenes unless the scenario says so.
11. Quit by the same route each time (Alt+F4 or the menu) after exactly 3 min (S1) or 5 min (S1+S2) from entry; the run must
    be at least 6 minutes of heavy scene to give 100+ steady windows (the median past run had 136).
12. Immediately copy `%LOCALAPPDATA%\victory\runs\<newest>\` (frametime.csv, victory.log) or `logs\play-<newest>.log`, plus
    `victory-settings.txt`, into `docs\performance\runs\<tag>\<arm>-<n>\` and write a one-line `notes.txt`: commit, command,
    cold/warm, A press time, entry time, scenario, anything odd (loading hang, crash, popup).

Repeats and analysis
13. At least 3 measured runs per arm per condition, order interleaved (A B A B A B), same time of day if possible. Five if the
    expected effect is under 10 %.
14. Summarize each: `python scripts\perf\summarize_run.py <run> --out docs\performance\runs\<tag> --label "<arm> warm S1 A@14s"`.
    Compare medians of per-run steady-state frame ms and P99, stall windows, time from entry to stabilised, and the log
    subsystem means; use `--compare` for a single pair only when explaining a mechanism.
15. Accept a difference only if it exceeds the A-to-A spread of your own repeats and is above ~5 % (`docs/findings.md`).
    The existing dataset shows ordinary spread is large: consecutive runs `20261126-210725` and `-211141` differ 24 % in heavy fps
    (and the second had passes off), and eras of the day differ 2x. A result that is not outside the repeat spread is a null result.
16. Discard and label (do not average) runs with a device loss, a crash, a visible hang over 10 s that was not in the other
    arm's runs, or a different loading path.

## 6. Open items for the owner

- Decide where the CSV writer comes from (port from the fork, P1) before any perf commit is validated with numbers on this branch.
- Confirm which victory.exe the runs of Nov 26 used (`VICTORY_REPO=...\VIctory`); its commit is the real "before" for those numbers.
- The generated `docs/performance/baseline-data/` (105 files, about 1 MB) can be dropped from version control if unwanted; it is
  reproducible from the logs with the command at the top of `BASELINE.md`.
