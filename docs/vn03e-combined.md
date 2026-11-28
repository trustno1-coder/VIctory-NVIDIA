# Combined SR and frame-generation validation

Run `upscaler-check --validate-framegen --combined --output 1920x1080 --frames 120 --focus-wait 60`. Keep the test window in front. Select SR with `VICTORY_DLSS_MODE` and supported FG multipliers with `VICTORY_FRAMEGEN_X`; `--lifecycle` exercises the existing seven phases. Combined runs accept 30..18000 frames; FG-only keeps its 600-frame bound.

Each frame evaluates SR at the SDK optimal render extent. The FP16 output feeds both HUD-less FG and the existing two-plane compositor with transparent UI, producing the RGBA8 swapchain input. Depth 0.5, zero motion and stationary split colours are initialized. History resets at phase start. First/final SR readbacks check all RGB pixels for finite/range validity and compare outer-quarter means with references. `VICTORY_CHECK_SKIP_EVALUATE=1` must fail on poisoned output. FG focus/count/DXGI acceptance is inherited from PR #527 (head 111c14ee32b0ab2165b99f78a06aeef4a0264597).

Each frame uses one frame-token index for the SR constants, the FG constants and the present markers (`VICTORY_CHECK_SPLIT_TOKEN=1` restores the earlier separate SR counter, for A/B only). SR is waited for by default, as every default-mode host submission is; `VICTORY_CHECK_SR_ASYNC=1` sends it unwaited on the direct queue, ordered before the compositor's present and fenced before its list is reset or its textures released. Each phase also logs its five slowest measured intervals with the advance/present/settle split, sleep lateness and SDK frames shown, the sampled local-VRAM usage peak, and the process private commit, peak private commit and peak working set (process-lifetime peaks).

Readback reserves packed output storage before mapping and restores the actual entry resource state in the copy submission. Combined readback uses one synchronized submission instead of three. The SR-only wrapper retains its UAV entry state. Boundary logs distinguish readback and CPU inspection from measured presents.

## Validation

Owner-run Windows RTX 5090 synthetic validation on a local integration of #527 plus this patch: debug build and four focused probe unit tests pass; scoped all-targets Clippy exits 0 with warnings; asset policy passes (293 files). Positive 120-frame combined smoke and expected skip-SR failure pass. Baseline and optimized matrices each pass five processes: 600-frame 2x and 6x, seven lifecycle phases, and two 3600-frame 2x runs. Matching frame-count SR hashes agree. Optimized executable SHA256: AB38669B2FBB6FB238497D2B1BF4D9DBBC4022E3AC464E4199666D5BAD61D3EC.

The 1 Hz sampled end-stage private-memory increment drops from 46.78 MiB to 31.62/31.63 MiB, approximately 15.15 MiB less. This is not a percentage reduction in whole-process memory or VRAM, and instantaneous peak memory is not established. Long-run host p50 is 0.920/0.922 ms before and 0.923/0.919 ms after; no FPS gain is claimed. One optimized lifecycle interval maximum of 42.739 ms is retained. Readback plus CPU checking takes about 0.53 s in this debug build outside measured frames; no matched before/after speed claim.

## Limits

Static synthetic output does not establish generated-image quality, per-frame pixel correctness, physical scanout or leak freedom. Errors fail strictly; graceful fallback and device-loss recovery are not implemented. Repeat runs use fresh processes, not same-process SDK reinitialization. Lifecycle fixture replacement disables FG between phases and does not establish uninterrupted production toggling. Fullscreen, multi-minute stress, full-workspace/all-features suites and the 18000-frame maximum were not tested. Public contribution transplantation is checked separately from the tested local integration; it is not a new hardware run.

## Follow-up integration

SR and FG now share a frame-token index. `VICTORY_CHECK_SR_ASYNC=1` opts into submitting SR without an immediate CPU wait; the default remains synchronous. The fixture waits on its own fence before reusing its command allocator or destroying resources. A Signal failure after submission, a failed fence wait, or device removal uses the existing fatal GPU failure path. These failures are not graceful fallback or recovery: continuing teardown without established completion would be unsafe.

Slow-interval and process peak-memory diagnostics are included. The earlier 21-case RTX 5090 matrix on the follow-up source covered SR modes, 1080p/1440p/4K, supported FG multipliers, repeated processes, lifecycle, debug layer/GBV, and one 18000-frame run. That evidence supersedes the earlier untested-maximum statement for that revision only. Final integration checks are recorded separately; no temporal-quality or leak-free claim is added.

## Opt-in acceptance extensions

`VICTORY_CHECK_DEGRADE=1` permits two initialization-time degradations: an FG request above the observed SDK maximum continues SR with FG off, and an SR fixture initialization error continues the existing native-resolution FG fixture. Default remains strict. Probe controls `VICTORY_CHECK_FG_CAP_2=1` restrict observed FG capability to at most2x, and `VICTORY_CHECK_SR_INIT_FAIL=1` injects an SR initialization error. These are controlled test scenarios, not proof of every driver/runtime failure mode. Runtime SR evaluation failures and unavailable FG runtime initialization still fail strictly.

`VICTORY_CHECK_MOTION=1` moves a split-colour boundary one render pixel per frame with matching horizontal motion vectors. Every frame's SR output is checked for finite values and reference mismatch away from an8-output-pixel edge band (RGB tolerance0.05, at most0.1% bad checked channels). A unit negative control rejects stale/frozen reference positions and NaN. This is a narrow moving SR correctness check; the ignored edge band, disocclusion quality and generated FG image pixels are not validated. Full readback makes it unsuitable for timing/performance measurements.

`tools/vn03e-recover.ps1 -Exe <victory.exe>` supervises fresh probe processes, bounded to1..3 attempts and10..300seconds per child. Success requires a zero exit, SR readback, FG PASS and orderly shutdown. Timeout children are terminated and reaped; exhaustion returns1. Configure official runtime and reserve GPU before using it. Optional `-FaultExe`/`-PersistentFault` are test controls for a separately built fault-injection executable; they are not a production device-reset mechanism.

Final RTX5090 checks: FG-cap strict/degraded, SR-init strict/degraded and60-frame moving-reference cases all met expected verdicts. Recovery fault->healthy passed on attempt2; persistent fault stopped after2 attempts with exhaustion.5focused CPU tests, strict scoped all-target Clippy, build, and asset policy306files passed on the private integration. New GPU results do not include GBV or generated-frame image capture. Process-level recovery was owner-selected; same-process SDK/device reconstruction remains a separate future project.
