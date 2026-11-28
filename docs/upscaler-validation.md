# Deterministic DLSS SR check

`victory upscaler-check --validate --output 1920x1080` runs an original
synthetic fixture without title files. Set `VICTORY_STREAMLINE_DIR` to an
official, signed NVIDIA Streamline runtime and `VICTORY_DLSS_MODE` to `dlaa`,
`quality`, `balanced`, `performance` or `ultraperformance`.

Unlike the original `--device` smoke check, validation initializes all inputs
and reads back the entire FP16 output. The fixture has two constant RGB regions
(0.125/0.25/0.5 and 0.75/0.5/0.25), depth 0.5, zero motion and explicit exposure
1. The output starts as NaN. Every output RGB component must be finite and in
[-0.1, 2.0]; means in the outer quarters must match all three reference channels
within 0.02. Alpha and the split's transition region have no colour oracle.
The BLAKE3 digest excludes row padding; it is evidence, not a portable expected
hash across drivers or models.

The requested mode is queried once and its exact SDK-recommended render size
is used. The check does not query unsupported Ultra Quality. Normal application
mode selection is unchanged. SR-only validation cannot be combined with
`--present`, and it does not exercise FG/MFG. Output dimensions are bounded to
7680 per axis; unsupported SDK sizes fail with a nonzero exit status.

## Timing and memory

Default `--iterations 12` performs three warm-up evaluations and nine measured
evaluations in one process, with resources allocated outside the timed loop.
Accepted counts are 4 through 48. Each submission is synchronously completed.
Reported p50/p95 include CPU recording, submission and waiting for GPU
completion, but exclude texture setup, readback and output validation. These
are **not GPU timestamp measurements**, throughput estimates or game frame
times. Initialization and all three warm-up times are reported separately.
The camera is reset for each evaluation; temporal convergence is not measured.

The process-local DXGI local/nonlocal budget and usage snapshots before fixture
allocation and after evaluation include SDK and fixture allocations. They are
not proof of a leak or of steady-state memory reclamation.

## Failure and lifecycle checks

Missing runtime, unsupported SR, invalid output, device failure and failed
shutdown are errors with a nonzero exit status. Successful checks print both
`Readback PASS` and `Streamline shut down: ok (0)`. The original capability-only
check now also calls `slShutdown`, without creating a device just to shut down.
Use a fresh process per invocation: the underlying host/SDK globals do not
support same-process reinitialization.

For a negative control, set `VICTORY_CHECK_SKIP_EVALUATE=1` and run `--validate`.
The command still initializes the fixture but skips evaluation; the untouched
NaN output must fail. Unset it for actual validation. CPU tests also reject flat
output, poisoned output, wrong channel values and incomplete readbacks.

This is a bounded SR data-path and coarse colour-reference check. It does not
validate moving scenes, jitter conventions, edge reconstruction, presentation,
temporal image quality, device-loss recovery or compatibility with any title.

## Recorded validation (2026-11-28)

On upstream main `64d4931b3dd90305134512b805fc98f04119fb6e`, implementation
`f9eecdf37fdaaa0f8a2a3012582460c950199e07` was tested on Windows 26300,
RTX 5090 (driver 621.24), Ryzen 9 9950X3D, Streamline 2.14.16 / NGX 310.9.16
(official release; all 16 runtime DLL signatures valid).

- 1920x1080, 2560x1440 and 3840x2160, each with DLAA, Quality, Balanced,
  Performance and Ultra Performance: 15/15 passes, 12 evaluations per run.
- Two additional 1080p Quality processes passed: 17 successful processes and
  204 evaluations total, final full-output readback per process. All successful
  processes shut down cleanly, without runtime WARN/ERROR messages or timeouts.
- Missing runtime and skipped evaluation each returned 1 as expected. The latter
  detected NaN at pixel 0 and still shut down cleanly. Capability-only query
  returned 0 and reported successful shutdown. All 20 case expectations passed.
- The three independent 1080p Quality readbacks had identical BLAKE3
  `66afde1c27fdb5af577813bbb2951134bb6c69da3dbf5e129affffa7adf39d75`.
  Their synchronized host p50/p95 milliseconds were 0.585/0.832, 0.537/0.617
  and 0.545/0.608. These nine-sample measurements are not comparative FPS claims.
- The 15-case matrix's p50 values ranged from 0.484 to 1.108 ms. Initialization
  was about 9.9–10.0 seconds; first evaluation/model startup is reported separately
  and excluded from those p50s. This change amortizes setup within the probe; it
  does not claim to accelerate NVIDIA's initialization or application rendering.
- `cargo test -p victory-gpu --lib --locked streamline:: -- --test-threads=1`:
  15 passed, including two new checker tests. Scoped all-targets Clippy exited 0
  with existing warnings, none in the new probe modules. Asset policy passed.

Related #237 addresses FG option switching and refusal messages. This change
does not include those fixes. A merge-tree check against #237 retained only its
existing conflict with main in `streamline/fsr.rs`; the probe reads shared plan
state so it does not depend on #237's changed initialization return type.
After that return-type decoupling, the final source was rebuilt: all 15 focused
unit tests passed again, and a fresh 1080p Quality process passed 12 evaluations,
full readback (the same digest above) and shutdown. The full 15-mode/size matrix
was not repeated for this capability-state lookup change.
