# Combined SR and frame-generation validation

Run `upscaler-check --validate-framegen --combined --output 1920x1080 --frames 120 --focus-wait 60`. Keep the test window in front. Select SR with `VICTORY_DLSS_MODE` and supported FG multipliers with `VICTORY_FRAMEGEN_X`; `--lifecycle` exercises the existing seven phases. Combined runs accept 30..18000 frames; FG-only keeps its 600-frame bound.

Each frame evaluates SR at the SDK optimal render extent. The FP16 output feeds both HUD-less FG and the existing two-plane compositor with transparent UI, producing the RGBA8 swapchain input. Depth 0.5, zero motion and stationary split colours are initialized. History resets at phase start. First/final SR readbacks check all RGB pixels for finite/range validity and compare outer-quarter means with references. `VICTORY_CHECK_SKIP_EVALUATE=1` must fail on poisoned output. FG focus/count/DXGI acceptance is inherited from PR #527 (head 111c14ee32b0ab2165b99f78a06aeef4a0264597).

Readback reserves packed output storage before mapping and restores the actual entry resource state in the copy submission. Combined readback uses one synchronized submission instead of three. The SR-only wrapper retains its UAV entry state. Boundary logs distinguish readback and CPU inspection from measured presents.

## Validation

Owner-run Windows RTX 5090 synthetic validation on a local integration of #527 plus this patch: debug build and four focused probe unit tests pass; scoped all-targets Clippy exits 0 with warnings; asset policy passes (293 files). Positive 120-frame combined smoke and expected skip-SR failure pass. Baseline and optimized matrices each pass five processes: 600-frame 2x and 6x, seven lifecycle phases, and two 3600-frame 2x runs. Matching frame-count SR hashes agree. Optimized executable SHA256: AB38669B2FBB6FB238497D2B1BF4D9DBBC4022E3AC464E4199666D5BAD61D3EC.

The 1 Hz sampled end-stage private-memory increment drops from 46.78 MiB to 31.62/31.63 MiB, approximately 15.15 MiB less. This is not a percentage reduction in whole-process memory or VRAM, and instantaneous peak memory is not established. Long-run host p50 is 0.920/0.922 ms before and 0.923/0.919 ms after; no FPS gain is claimed. One optimized lifecycle interval maximum of 42.739 ms is retained. Readback plus CPU checking takes about 0.53 s in this debug build outside measured frames; no matched before/after speed claim.

## Limits

Static synthetic output does not establish generated-image quality, per-frame pixel correctness, physical scanout or leak freedom. Errors fail strictly; graceful fallback and device-loss recovery are not implemented. Repeat runs use fresh processes, not same-process SDK reinitialization. Lifecycle fixture replacement disables FG between phases and does not establish uninterrupted production toggling. Fullscreen, multi-minute stress, full-workspace/all-features suites and the 18000-frame maximum were not tested. Public contribution transplantation is checked separately from the tested local integration; it is not a new hardware run.
