# 04 - Shader and pipeline architecture

Scope: `crates/victory-shader`, `crates/victory-gpu` (`pipeline.rs`, `pipeline/{make,library,known,host}.rs`, `shader_cache.rs`, `indirect*.rs`, `device.rs`), `apps/victory/src/warm.rs`.
Machine in the logs: RTX 4070 (driver 621.1), Series X profile, 1440p, DLSS balanced + frame gen. Read-only analysis; no source edited.
Labels: **[S]** static (read from code), **[M]** measured (from `%LOCALAPPDATA%\victory\logs\play-20261127-*.log` and the `cache\pipelines` files), **[H]** hypothesis.

## 1. Headline

The "~5 minute late loading" is the host driver compiling about 1,360 pipelines after the world starts, on the render path's wait budget. It is not shader translation.

- **[M]** Translation is fully cached. Warm runs show `hits=1276 misses=0` about 18 s after start, and all 2,731 shader translations are logged within the first seconds of loading. The cold run (093037) showed `hits=0 misses=2717`.
- **[M]** The pipeline library is persistent (428 MB, mapped, loads fine: `loaded=1139`). Yet every warm run still reports `made ~1,360` host pipelines, the same as a cold run (cold: `made=1991 loaded=495`; warm: `made=1362 loaded=1139`).
- **[S]** Cause, commit `7fa0e95` ("Pipelines with static samplers are not loaded from the pipeline library"): `library::pipeline()` ignores the library whenever `STATIC_SAMPLERS` is set, so those pipelines are compiled by the driver every run and never stored (`pipeline/library.rs:572`, `pipeline/host.rs:604`).
- **[M]** The numbers agree: 1,362 / (1,362 + 1,139) = 54% of used pipelines. Of the 2,731 translated shaders, about 1,250 have `samplers>0` (pixel 5/2/1/7/3/6/4, compute 1/2/7/...). It fits, but I have not matched pipeline by pipeline.

## 2. What the logs show (measured)

| Run | Loading to world | Pipelines still being made at world entry | Frames after world entry |
|---|---|---|---|
| 093037 (cold) | ~60 s | 924 | 290-313 ms for ~2 min (09:31:48-09:33:14), then 56-62 ms. "all made" at +5m11s |
| 094255 (warm) | ~2 min | ~0 | 49-80 ms |
| 094655 (warm) | ~2 min | 0 | 48-81 ms (2,700 draws dropped for 20 s, then 260) |
| 095317 (warm) | ~60 s | 525 | 250-340 ms for ~2.5 min (09:54:20-09:56:30), 78-80 ms until mk=2 (09:57:54), 59-72 ms after |

- **[M]** `made` grows steadily through the run: 451 at +26 s, 863 at +66 s, 1,191 at +2m30s, 1,362 at +4m40s (run 095317). `host pipelines all made` fires at +4m44s.
- **[M]** Whether the stall appears depends on how long the title's own loading screen lasts. Run 094655 had a 2-minute loading screen and the workers finished first (`mk=0` at world entry). Run 095317 had 1 minute and did not. The compile work is the same in both, about 125-280 s of 8 workers at lowest priority.
- **[M]** The 250-340 ms frames coincide with `mk` falling from 525 to ~150 and with 290-1,300 draws dropped per frame (`dropped N without a host pipeline`). Steady state afterwards is 50-80 ms.
- **[M]** `slow-pipelines.txt` has 907 keys of 1 s or more, summing to 4,380 thread-seconds (maximum seen per key). 236 are 5 s or more, 149 are 10 s or more (2,350 s), and the worst is 58.8 s. All 907 are in `early-pipelines.txt` (5,539 keys). Those figures are maxima, so they include cold-run contention.
- **[M]** `known-pipelines.bin` holds 83,555 title pipelines from 19,100 shaders (50 MB, plus 198 MB of containers). About 2,500 are started early per run.
- **[M]** `victory warm` has never run on this machine: `warm-done.txt` and `warm-bad.txt` are 0 bytes.
- **[M]** The NVIDIA driver cache `%LOCALAPPDATA%\NVIDIA\DXCache` is 7.9 GB. The comment in `library.rs` says the driver's own cache "keeps that quick" for static-sampler pipelines.
- **[H]** The logs do not show whether the driver cache really hits for these. The 125-280 s of work for 1,362 pipelines on 8 workers (~0.7-1.6 s each) suggests many are not quick. A per-pipeline `made in N ms` line is only emitted through `host::trail` (diagnostics builds), so release logs cannot confirm it. See item 1 (instrumentation).

## 3. How pipelines are made today (static)

- **[S]** The title's `CreateGraphicsPipelineState` and `CreateComputePipelineState` only record the shader containers and the graphics state. `pipeline.rs:569 hand_out` builds a `make::Pending` (key = BLAKE3 of stage, shader hash and `GraphicsState::describe()`).
- **[S]** `known::note` appends the pipeline to the book when `VICTORY_PIPELINE_BOOK=1` (`play.rs` sets it). `make::early` starts it at creation only if its key is in `early-pipelines.txt`, or it is a hard-coded `scene_input` shader or a bake pixel shader (`make.rs:129`).
- **[S]** Everything else is queued on first draw: `make::ready()` starts it as `REQUESTED` and waits on the title's render thread. The wait is a shared 500 ms per frame (`VICTORY_PIPELINE_WAIT_MS`) and 10 s for screen passes (`VICTORY_SCREEN_WAIT_MS`, below the title's 30 s watchdog). Past the budget the draw is dropped and retried next frame.
- **[S]** Workers: `worker_count_for` gives `processors*2/3` clamped 2-16, or `processors*3/4` clamped 2-8 under `residency::limited_host_ram()`. The log shows 8 (limited-RAM path). They run at `THREAD_PRIORITY_LOWEST` (`make.rs:566`), below the title's loading threads. Two extra workers take only screen passes.
- **[S]** Queue order is screen, then `requested`, then `early` (FIFO in title creation order, not first-use order), then `slow`. Known-slow keys (`library::slow`) are capped at `slow_limit()` = workers/4 = 2 concurrent unless a draw promotes them.
- **[S]** `HostPipeline::new` (`host.rs:417`) does translate (disk-cached), then builds a root signature per pipeline (no cache), then `library::pipeline` (load, else create then `StorePipeline`). Library file name = BLAKE3 of root-signature blob, shader code and state, so it changes with any translator change.
- **[S]** The translation cache key (`shader_cache.rs:44 build()`) includes `VICTORY_TRANSLATOR`, the BLAKE3 of the victory-shader sources, plus about 15 `VICTORY_*` env settings. Any edit to victory-shader, or changing one of those switches, means a full cold start: ~2,700 translations plus all pipelines.
- **[S]** `victory warm` (`apps/victory/src/warm.rs`, `known::make`) builds pipelines from the book in idle-priority child processes (40 at a time, 180 s hang kill, `warm-bad.txt`). It writes to the driver cache and to `slow-pipelines.txt`. Pipelines with static samplers now bypass the library there too, so warm only helps if the driver cache hits. It also has to be run by hand.
- **[S]** Emulator helper pipelines are few and separate from the title's. Examples: `indirect.rs` argument clamp, batched clamp and compare passes, `host.rs` card-work and depth-plane pipelines, `inplace.rs`, `compose.rs`, `ambient.rs`. They are HLSL compiled at first use with `D3DCompile` (FXC, `cs_5_1`) plus `CreateComputePipelineState`, with no library. Command signatures are made at first use (`draw.rs:5445`, `indirect.rs:827,2695,2882`).
- **[H]** The helper pipelines are probably tens of ms each once, not the 5-minute tail. They run on the render or submit thread at first use, so they can add single-frame hitches. They are unquantified: the release logs have no timing for them.
- **[S]** Commit `80c70b7` / PR #22 ("helper-priority") raises the submitting thread's helper threads (wide pool, landing pools, heap records) to above-normal. It is CPU thread scheduling and does not touch shaders or pipelines.

## 4. Ranked structural opportunities

Estimates are for this machine. "Stall" means seconds of frames over 200 ms after world entry.

### 1. Make static-sampler pipelines library-storable again (removes ~1,360 compiles per run)
- **Gain [H]:** eliminates the 125-280 s of compile work per run and most of the 130-150 s of 250-340 ms frames after world entry; the warm run would look like run 094655 even with a short loading screen. The library already loads 1,139 pipelines in a few seconds, so loading 1,360 more is cheap.
- **Risk:** medium-high. The reason for `7fa0e95` is real: a pipeline loaded in another process can read through the wrong static sampler (posterized colour grading on the 4070 SUPER, `crates/victory-gpu/examples/lut3d_probe.rs`).
- **Design (the shortcut note in `library.rs:282` already names it):**
  1. Persist a "static sampler registry" (sorted distinct `D3D12_STATIC_SAMPLER_DESC` values seen, at most 2,032 per the probe) next to the library.
  2. At host init, before any title pipeline, create dummy root signatures and trivial PSOs for every registry entry in sorted order. The driver's table then gets the same assignment each process.
  3. Add the registry's hash to the library file name (`library::name` input) so a registry change invalidates only the affected entries.
  4. Add a boot-time canary: a probe pipeline (point, linear, trilinear, as in `lut3d_probe`) is loaded from the library and a texel is checked against a made one. On mismatch set `STATIC_SAMPLERS` handling back to "make".
  5. A new sampler found at runtime is appended, never reordered, and its pipelines are made.
  - **Files:** `pipeline/library.rs` (`pipeline()`, `with_static_samplers`, `name`), `pipeline/host.rs:576-604`, new `pipeline/samplers.rs`, the host init in `host.rs`.
  - **Gate:** a visual A/B of PS_CompositeNMB (the original failure) and the lut3d probe, on NVIDIA and AMD.
- **Alternative with no driver-order assumption:** replace static samplers with a sampler descriptor table bound from a fixed sampler heap (library-safe by construction). It costs a heap binding per draw and touches the root-signature layout in `host.rs` and `draw.rs`, so it is much larger.

### 2. Manifest-driven prebuild during loading (order by first use, all workers, higher priority)
- **Gain [H]:** 50-100% of the post-entry stall when item 1 is not done; with item 1 done it also fixes the ~54% of pipelines that need a real compile on a new translator build or driver.
- **Risk:** low.
- **Design:**
  1. **Extend the manifest.** `early-pipelines.txt` is only an unordered set. Replace it with `pipelines.manifest` (versioned, keyed by `{translator hash, driver version, adapter LUID}`). Per key store: `first_use_ms` since process start, `first_use_frame`, `phase` (loading, cutscene or world, from `draws/frame` thresholds or frame number), `compile_ms` for every pipeline (not only 1 s or more), `static_samplers` and `from_library` flags. Writer is `library::note_used` and `library::save`; reader is `library::used()`. Keep the old file readable.
  2. **Order the EARLY queue by `first_use_ms`.** `make::Queue::early` is a FIFO in the title's creation order, which is unrelated to use order. Make it a priority queue (`BinaryHeap` keyed by rank) so the first 300 ms of the world are made first and everything used only late comes last.
  3. **Lift the slow cap while loading.** `slow_limit()` = workers/4 starves 907 known-slow pipelines during loading, which is the best time to compile them. Allow all workers on `slow` until the first world frame, then drop back to `workers/4`. The frame signal is `frame::current()` or a draws/frame threshold.
  4. **Raise worker priority during loading.** `THREAD_PRIORITY_LOWEST` is right once the world runs, but while the title loads with a spare CPU budget a short `BELOW_NORMAL` or `NORMAL` window up to the first world frame costs nothing visible. Measure on a short loading screen. Use all `processors*2/3`, not the limited-RAM 8, when RAM allows.
  5. **Loading gate (optional, only if 1-4 are not enough).** At the first frame where `waiting()` is above N and draws/frame jumps (world entry), raise the shared wait budget for pipelines in the manifest's world-entry set to a one-off value (for example 15-20 s, below the 30 s watchdog) instead of 500 ms a frame. This trades ~2.5 min of 280 ms frames and dropped draws for one stall of seconds.
  - **Files:** `pipeline/library.rs` (`used`, `wanted_early`, `note_used`, `save`), `pipeline/make.rs` (`Queue`, `early`, `slow_limit`, worker priority, `wait_budget`), `frame.rs`.
  - **Cold run, hard case.** With a new translator or driver the 83,555-key book (item 4) is the only way to fill the library before play.

### 3. Run `victory warm --used` automatically (launcher or `play`)
- **Gain [H]:** on a new translator or driver build, moves ~2,700 translations plus the compile of ~2,500 used pipelines (4,380 thread-seconds worst case) out of the first play session. Currently never run (`warm-done.txt` empty).
- **Risk:** low.
- **Design:**
  - `apps/victory/src/play.rs` spawns `warm::run(used=true)` as an idle-priority child while the menu is up, or immediately after a fingerprint change (`known::fingerprint()`).
  - The warm children must write to the library too: `known::make` runs `HostPipeline::new`, which does `StorePipeline`, but each child process holds its own empty library and `use_pipeline_cache` is not called there. Pass the library directory and merge by name, or have children store and serialise each batch.
  - Until item 1 lands, warm only helps through the driver cache for static-sampler pipelines.

### 4. Instrument release builds so the above can be measured
- **Risk:** none; it is a prerequisite for deciding between items 1 and 2.
- **Add to `library::pipeline()` / `make.rs`:**
  - `pipelines made: N, loaded: N, static_sampler_made: N, driver_ms_p50/p90/p99` once a minute.
  - Per frame, the pipeline wait (today `copystat::C::PipelineWaitUs` exists only with `diagnostics`).
  - A log line when a worker starts or finishes a pipeline of 1 s or more.
- This tells whether the driver cache hits for static-sampler pipelines (the central unknown, see section 2).

### 5. Lower the render-thread wait, or make it adaptive
- **Gain [H]:** when many pipelines are being made, a shared 500 ms wait per frame is most of a 285 ms frame.
- **Quick experiment, no code:** `VICTORY_PIPELINE_WAIT_MS=50` against 500 on a short-loading start; compare ms/frame and dropped draws for the first 2 minutes. Dropped draws show as pop-in or missing lighting; the code comments say screen passes need their own budget (`VICTORY_SCREEN_WAIT_MS`).
- **Design:** adaptive budget = `min(500 ms, f(waiting()))` and zero for non-screen pipelines in `ready()` once `waiting()` > 200.

### 6. Cache root signatures and avoid per-pipeline serialisation work
- **Gain [H]:** small. `host.rs:595 root_signature()` is built and serialised per pipeline, so ~1,360 times per run during the tail. Key by the `params + samplers` bytes and share the `ID3D12RootSignature`.
- **Risk:** low.
- **Constraint:** it must not change the library name, which uses the blob bytes; they would stay identical.

### 7. Reduce unnecessary compiles in the title's own set
- **Gain [H]:** unknown, per-feature.
  - `left_out()` already skips ~1,000 `CS_RaytracingResolve` shaders without BVH, plus the two water-light shaders and the decal defrag. Extending the same mechanism to pipelines that never produce visible output on this profile (list from the manifest: created but never drawn in N runs, never `used`) shrinks the 2,500 early set.
  - 83,555 known vs ~2,500 used (3%) also means `early()` already prunes well; the remaining waste is the unused slow ones queued before used ones (item 2.2).

### 8. Check the unimplemented pipeline creation slots (`0x1c8`, `0x218`)
- **Fact [M]:** every run logs `a pipeline creation method that is not implemented: the title goes without this pipeline`. Slot `0x1c8` reached at least 8,192 calls and `0x218` at least 8 (counts are logged at powers of two, so 8,192-16,383 actual calls) (`device.rs:349-400`).
- **[S]:** the description holds six `(pointer, size)` pairs of 0xb0-0x1c0 bytes; it is not a plain DXIL container.
- **[H]:** this could be real content the title never gets (extended pipelines or PSOs built from raw hardware state). It is a correctness question first; if it is the source of the ~300-460 draws dropped per frame in steady state, it matters for FPS too. Cheap first step: dump a few of those descriptions and run them through the shader parser.

### 9. Smaller points
- **FXC helpers.** The emulator's own compute and pixel helpers use `D3DCompile` on first use and no persistent cache. Pre-compile them at host init (a thread at startup) or store the DXBC beside the library keyed by source hash. Likely tens of ms each on the render thread; **[H]** and unquantified.
- **ExecuteIndirect command signatures.** Made at first use. They are cheap, but creating the common ones at init keeps their `CreateCommandSignature` off the first draw.
- **Cold-start cost.** Any edit to victory-shader changes `VICTORY_TRANSLATOR` and forces a ~5 min cold start (the 093037 run). Caching by container hash is already good; cache the translator-independent front-end (IR) separately, or keep the old translation as a fallback while the new one is built, if development builds matter.

## 5. Suggested order

1. Item 4 (a day or less): instrumentation, then re-run one short-loading session.
2. Item 2 steps 2-4 (priority queue by first use, slow cap lifted during loading, worker priority) plus item 5's `VICTORY_PIPELINE_WAIT_MS` experiment: no correctness risk.
3. Item 3: hook `warm` into play.
4. Item 1 with the canary: the big win, needs the visual gate on NVIDIA and AMD.

## 6. Open questions

- Does the NVIDIA driver cache hit for the 1,362 static-sampler pipelines? If yes, the tail is mostly CPU contention with the title's loading; if no, only item 1 removes it.
- What are the `0x1c8` and `0x218` creations?
- What is the real per-frame pipeline wait in release (instrumentation, item 4)?
- Why are 5,539 early keys listed but only ~2,500 created early per run? Probably the rest are created only in other areas.
