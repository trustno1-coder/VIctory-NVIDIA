# Synthetic shader correctness and pressure reclamation

**Pending revalidation:** upstream advanced to `64d4931b3dd90305134512b805fc98f04119fb6e` after the results below. Its allocator/list trimming, prelude pooling and shader changes have been merged, preserving both sets of changes. The new merge is being pushed before testing at the owner's request; results below apply only to the older pinned revisions. Upstream age-based idle trimming remains available; the new pressure-only path also increments its trim counter.

This work supplements existing upstream fixes with four shader corrections and pressure-only reclamation of idle host lists and oversized fetch buffers. The new shader/memory tests use original synthetic fixtures; no game, captured asset, shader dump, game offset or private runtime log was supplied. Existing repository test data and its licensing caveats are unchanged; these commits add or modify no binary/table blob. No real-game correctness or FPS claim is made.

## Revisions and attribution

Upstream baseline: `df2daafeb1b2170931cf7a6eddff6de254841d9c`.
Combined production implementation: `1e050de66330baef94db9c972ea0b774f5eda2c1`. The subsequent test-only refinement is `6e2171f2d98847dd083ea1409822aead90528dec`; it refreshes simulated pressure and samples burst capacity before later frames can shrink it.

This is a stacked contribution. These existing upstream submissions are prerequisites, with their original commits and authors retained, not newly authored fixes:

| Prerequisite | Pinned head | Author | Purpose |
|---|---|---|---|
| #108 | `245cb85145bfd083e3df6769adf68dbdbb670340` | Caasares | Numeric wave64 mask halves |
| #112, including #111 | `4ad8b9b4dd133bf1b2cfe5cb9ef25754f8fff38e` | simbaui4life | Landed scratch release and memory benchmark |
| #116, including #70 | `d212aea94fe54c0bd2f616e3a7a30ae11b1cfc5f` | simbaui4life | Compare-mask correction and differential oracle |
| #71 | `6f4813ca7337abedf4cbb7f02457e653de1562cc` | simbaui4life | Scalar condition codes and lane-index wrapping |
| #73 | `6e16352fa263b634c9d5a9c88451999fdb27c01a` | simbaui4life | Scalar values across lane-divergent branches |
| #425 | `7331b07bde8d7beca973f24a0575ca1866d034d4` | Togicx | Existing workspace CI and budget-test repairs |

The #73/#116 integration preserves both lowerer changes and both regression suites. #70/#71/#73/#111/#112/#116 were closed without merging when inspected; closure is not presented as approval or rejection. The branch can be rebased as prerequisites land. #425 supplies the CI repairs; this contribution does not introduce another CI implementation.

New implementation commits: `beec109a68ae4cd876a4a57b797157259538ca8d`, `53867f60f77d2e45c6bec44d864ceda71bfd708d`, and `1e050de66330baef94db9c972ea0b774f5eda2c1`. AI-assisted by OpenAI Codex; reviewed and tested as described below.

## Changes

- A standalone first-lane read requests guest-wave layout/cross-half exchange when needed. Descriptor waterfall reads and groups of at most 32 threads retain their existing lowering; unnecessarily fixing the wave size broke WARP pipeline creation and is avoided.
- Enable scalar/compare branch-state preservation by default for one complete 64-thread guest wave. Explicit `VICTORY_WAVE_WIDE=0` remains an opt-out; larger groups retain the existing opt-in rules.
- Execute WRITELANE updates even when the destination lane is inactive in EXEC, including divergent regions. Preserve existing lane wrapping and empty-branch handling.
- Keep intermediate mask phis live through uniform loop joins. This fixes numeric mask values silently turning into zero without globally enabling strict joins.
- Under memory pressure, retire reset/GPU-complete idle host lists older than two seconds, retaining the newest 16 and all recently used lists. Release retired objects outside the pool lock.
- Shrink a peak fetch allocation only under pressure after two seconds below that capacity, between completed fetch/CPU-merge operations. Retain 16 MiB allocation granularity; zero demand can release the allocation. Failed shrinking retains the usable old buffer. `VICTORY_IDLE_RECLAIM=0` disables the new reclamation for matched diagnostics.

The new byte-readback test forces every source piece to change in each of two bursts, requires fetch capacity to grow to at least 32 MiB each time, compares the complete 32 MiB result, then verifies that pressure reclaims lists and peak fetch capacity. It runs separately with and without relay. Reclamation depends on subsequent submissions/fetch calls; this is not a background idle-process collector.

## Environment and reproduction

Windows 11 build 26300, Rust 1.98.1 MSVC, RTX 5090 (32 GiB), driver 621.24, Ryzen 9 9950X3D, 192 GiB system RAM. Existing WARP cases also run. Exclusive GPU use, four Cargo/Rayon workers, serial tests, locked/offline dependencies, empty synthetic configuration and fresh per-process cache/data directories. Clear inherited `VICTORY_*` switches before each independent run.

```text
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo xtask policy
cargo test --workspace --locked -- --test-threads=1
cargo test --workspace --all-features --locked -- --test-threads=1
```

For the oracle, set `VICTORY_ORACLE_RUNS=200` and `VICTORY_ORACLE_SEED=1` or `2`, leave `VICTORY_ORACLE_AVOID` unset, then run:

```text
cargo test -p victory-shader --lib --locked oracle_fuzz -- --ignored --nocapture --test-threads=1
```

For each memory test use its own process; repeat with relay switches unset and with `VICTORY_RELAY=2`, `VICTORY_RELAY_COPY=1`:

```text
cargo test -p victory-gpu --lib --locked memory_bench_pressure_lets_idle_go -- --ignored --nocapture --test-threads=1
cargo test -p victory-gpu --lib --locked memory_bench_reclaims_and_regrows_with_correct_bytes -- --ignored --nocapture --test-threads=1
cargo test -p victory-gpu --lib --locked memory_bench_steady_state -- --ignored --nocapture --test-threads=1
```

Set `VICTORY_MEMBENCH_STRICT=1` for the pressure test. It simulates 1 GiB free RAM in the layer's test-only accounting; it does not exhaust physical RAM. Repeat with `VICTORY_IDLE_RECLAIM=0` for the negative control. Do not run these singleton/allocator benchmarks together in one process.

## Results

- Default workspace at `1e050de`: **1074 passed, 0 failed, 50 ignored**. Counts exclude the HLE test's nested child-process result, which would otherwise be counted twice. This includes **299 shader tests**, with 5 ignored.
- All-features workspace at `6e2171f`: **1127 passed, 0 failed, 51 ignored**, also excluding the nested HLE child result. Production source matches `1e050de`; the later commit changes only the isolated regrowth fixture.
- Both workspace/all-targets Clippy configurations with `-D warnings`: pass. The subsequent fixture-only refinement also passes all-targets/all-features GPU Clippy. Asset policy passes, including this report (290 files).
- Unfiltered oracle on `6e2171f`, 200 generated programs per wave size per seed: seed 1 has wave32 199 matches and 1 unexamined equal-input min/max SCC case; wave64 default and exact both 200 matches. Seed 2 has 200 matches in each of wave32, wave64 default, and wave64 exact. **1199 semantic comparisons match; 1 is not checked; no refusals or differences in these two runs.**
- Each new targeted fixture fails on its corresponding pre-fix control and passes after repair: standalone first-lane (16 cases), single-wave branch state (16), inactive WRITELANE destinations (80), and uniform-loop masks (6). Development unfiltered seed 1 exposed 16 failing assertions before the WRITELANE repair; seed 2 exposed 3 before the mask-phi repair. WARP size (8) and waterfall (22) regression subsets pass after narrowing first-lane wave sizing.
- Strict pressure tests pass on `6e2171f` in relay and synchronous modes. Steady-state memory tests also pass in each mode.
- Forced regrowth and complete byte comparisons pass on `6e2171f` in both modes: two bursts of 32 MiB each per process, four complete readbacks altogether. An earlier version of this test failed its synchronous capacity assertion after later frames; the fixture now refreshes the cached simulated pressure state between rounds and samples capacity immediately after the burst. Its data comparisons were not relaxed.

Pressure census on `6e2171f` (single process per mode; after spike -> after 3.5 seconds of simulated pressure):

| Mode | Idle host lists | Fetch allocation |
|---|---:|---:|
| Relay | 68 -> 19 | 32 -> 16 MiB |
| Synchronous | 64 -> 16 | 32 -> 16 MiB |

Recent lists may remain above the 16-list warm reserve. A matched negative control on `6e2171f` with `VICTORY_IDLE_RECLAIM=0` fails the strict test and retains 68 lists and a 32 MiB relay fetch allocation. This does not claim that all process memory returns to the pre-spike baseline, or that all pressure savings are caused by this patch: existing window and scratch reclamation also runs.

The five default-ignored shader entries are: an explicit bitcode benchmark, the directory-input probe tool, the fuzz oracle (run separately above), strict-join propagation, and legacy denormal-mode expectations. The last two remain limitations below, not claimed passes. Other default-ignored workspace tests not expressly listed as separately executed here were not run.
## Limits

Finite synthetic fuzzing does not establish general shader correctness. Refused programs and ambiguous equal-input SCC cases are disclosed separately from matches. The pre-existing strict-join ignored test still fails with default settings and passes by rejecting an unsupported scalar-memory value with `VICTORY_STRICT_JOINS=1`; globally enabling rejection is not a semantic implementation. The legacy FP64-to-FP32 ignored test assumes flushed denormals, whereas the active test checks IEEE behavior; no target-mode evidence justified changing either expectation. Multi-wave synchronization and unsupported instructions retain upstream limitations.

Memory reclamation does not return every process allocation to its warm baseline. Process private bytes are noisy and include allocator/driver retention. The bounded list/fetch counters are stronger evidence than a single process-memory difference. Exact wave semantics may require additional synchronization, so these results do not demonstrate an FPS gain. No allocation-failure injection or real-game workload was performed.
