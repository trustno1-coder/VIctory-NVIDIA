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
