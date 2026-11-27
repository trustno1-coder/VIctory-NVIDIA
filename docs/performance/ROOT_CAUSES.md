# Root causes (consolidated from agent reports 02-05, 08)

Labels: [M] measured in logs, [S] static code reading, [H] hypothesis. Source: AGENT_REPORTS/. Nothing here
has been re-measured in-game after a change yet.

## Steady-state frame (48-51 ms, ~20 fps) [M, report 02]
~42 of 48 ms is noticing and moving memory, not GPU work (GPU ~7 ms busy).
- Over-wide shader-write declarations (`draw.rs:2167`, `draw.rs:5589`: whole UAV view declared written) cause
  hash/compare passes, "title bytes under flights' writes" (9.6 ms) and refreshes (9.8 ms, 89% copy nothing). [S+M]
- Landing is ~96% waste: ~18 MB fetched/frame, 0.77 MB actually changed. [M]
- Descriptor mirror scans ~280k slots/frame to find 8-10k new records (3%). [M, report 03]

## Late loading after world entry (~3-5 min) - three overlapping causes
1. Texture adoption throttle [M+S, report 05]: `play.rs` forced `TEXTURE_CATCHUP=0`, 16 ms slice, counted per
   frame while frames take 100-300 ms. Backlog of up to 26k slots. -> commit c84d5e9 (UNMEASURED in-game).
2. Pipeline compiles [M+S, report 04]: ~1,360 pipelines compiled by the driver after entry, because
   static-sampler pipelines (54%) bypass the persistent library (commit 7fa0e95, posterized-light bug).
   Whether this bites depends on the title's own loading-screen length (60 s: stall; 120 s: hidden).
3. Card-memory line [M, report 02]: "no room, new textures wait", 8.5 GB vs 7.9 GB line (80% of budget,
   `play.rs:213`), `VICTORY_LEVELS_ON_DEMAND=0`. [H that this bounds the drain]

## Ranked work
| # | Item | Est. | Risk | Needs in-game A/B |
|---|------|------|------|---|
| 1 | Backlog slice (done, c84d5e9) | loading | low | yes |
| 2 | Pipelines: order early queue by first use, raise worker priority while loading, record compile ms | loading | low | yes |
| 3 | Memory: measurement patch (GPU-written blocks per copy), per-copy epoch, batched write-watch ask | 3-6 ms | low | yes |
| 4 | Descriptors: ask heap rounds only for pick-by-index submissions, coalesce write-watch asks | 1-2.5 ms | low-med | yes |
| 5 | Learn/persist GPU-write footprint per copy (R1) | 10-18 ms | med | yes + soak |
| 6 | Static-sampler pipelines back in library with registry+canary | ~1,360 compiles | med-high | visual A/B |
| 7 | GPU-written slot-use bitmap (descriptors A) | 4-10 ms | med-high | yes |
| 8 | Land only CPU-read blocks (R2) | 5-11 ms | high | soak |

Config-only experiments for the user (no code): `VICTORY_LEVELS_ON_DEMAND=1`, `VICTORY_PIPELINE_WAIT_MS=50`,
`VICTORY_TEXTURE_BACKLOG_MS=400`. Press **A** to leave the menu at start.
