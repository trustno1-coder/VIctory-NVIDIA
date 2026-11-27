# 05 - Asset streaming and preloading: why the world keeps loading for minutes

Scope: DirectStorage emulation, file reads, BCPack/zlib, texture creation, detiling, uploads, loading
detection, preload design. Read-only investigation, no source edited.
Evidence tags: **[static]** read in code, **[measured]** read from logs, **[hypothesis]** inferred.
Main log: `%LOCALAPPDATA%\victory\logs\play-20261127-094655-17644.log` (world entry 09:48:45, when the title opens
`core_restofworld.rpf`). Cross-checks: `play-...093037-23504.log`, `play-...094254-19348.log`,
`runs\20261126-123718-...\victory.log` (has the debug dstorage lines).

## 1. Root cause in one paragraph

DirectStorage is **not** the late-loading thread. It is cheap and keeps pace with the title (measured in the 11-26 run:
3-400 MB/s, `bcpack_workers` 0.1-1.5 of 16, reading workers under 1). What runs for minutes is the **GPU-side adoption of the
title's textures**. After entering the world the title writes about 1.8M placed-texture descriptor slots
(`texture slots written so far: 1821737 placed`) and fills the pages itself. Each slot goes into a queue worked by
`heap_textures` (`crates/victory-gpu/src/draw.rs`) **on the frame-recording thread, once per frame**: create host texture, detile the
title's tiled memory, upload, flip the slot from the neutral texture to the real one. That pass is throttled **per frame, by a
16 ms time slice, with catch-up disabled** (launcher overrides), while frames run at 3-13 fps. The drain rate is therefore
frames/s x 16 ms, about 80-200 ms of pass time per wall second, for 26,000 queued slots. The same minutes also have a slow frame
(4,400 draws, preparing thread 34-66 ms, ~2,800 pipeline first-binds at 09:49:15-26), which shrinks the budget further.

## 2. Measured timeline (play-094655)

| Time | Event | Evidence |
|---|---|---|
| 09:47:00-09:48:44 | Menu / loading screen: 4-8 ms/frame, 1-11 draws/frame, **0 slots queued** | fps lines, queue line |
| 09:48:45 | World entry: `pack\core_restofworld`, `shared_minigame000_bvh4`, `story000_script` .rpf opened | dstorage `file opened` |
| 09:48:49 | First slots queued (99); 1,713 at 09:49:04; 18,489 at 09:49:20; **peak 26,414 at 09:49:52** | `textures picked by index, the queue` |
| 09:49:14 | 36,453 slots served "at their first look" in 5 s, 2.5 frames late (up to 37 frames) | `served for the first time` |
| 09:49:15-26 | ~2,800 first binds; 4,085 shadow host-copy lines in the minute | counts |
| 09:49:20-09:50:23 | **4.8-13.7 fps**, 50-208 ms/frame, preparing 50-66 ms, copies 18-30 ms, 40-97 MB/frame | fps lines |
| 09:50:02-08 | Due backlog gone (520 due, oldest 533 frames old) | queue line |
| 09:50:29 | Second wave: 19,133 first looks, 313 ms of host-copy creation per 5 s | pass line |
| 09:50:39-09:52:27 | Steady 20 fps / 46-51 ms, 4,420 draws, preparing 33 ms, copies 10-12 ms and 27 MB **every frame** | fps lines |
| 09:52:42 | Third wave (area change): +8,000 uploads / 0.9 GB, card 84 ms/frame | uploaded 74.6k -> 82.7k |

Totals at 09:52:53: 82,658 textures, 4,962 MB uploaded. 115,000 slots shown after upload, with "the neutral texture meanwhile
in 196,984" (the same slot counted per show). Other runs agree: `093037` queue 22.7k at 09:31:54, 29.3k at 09:32:29, empty
09:34:39 (3 min at 3.5 fps); `094254` 24.7k at 09:44:06, drained 09:46:18.

Reading it: the pure upload backlog takes about **80-100 s** at today's rate (about 700 textures/s early). The rest of the "5
minutes" is later waves, the recheck tail (3.4), and a world that stays at 20 fps. The player sees all of it as "still loading"
because surfaces are neutral/grey and frames are slow.

## 3. What is deferred, who runs it, what throttles it

### 3.1 The texture pass (main deferral) [static + measured]
- `crates/victory-gpu/src/draw.rs::heap_textures` (about lines 3040-3330), called from list recording, also "early"
  (`early_textures`, ~2810) and before bakes (`PASS_BUDGET`). Runs **on the recording thread** (`IN_TEXTURE_PASS`).
- Queue: `descriptor/host.rs::HostHeap::texture_slots_due_known(frame, 4096)` (65,536 when baking), most recently written
  first. Only "picked by index" slots; slots read through tables are not queued.
- Per slot: `texture::fingerprint` (48 KiB), `mapped_bytes`, `resource::host_resource`, `texture/ahead.rs::take`, else record the
  upload in the list.
- Throttles in effect:
  1. `apps/victory/src/play.rs` lines 56-57: **`VICTORY_TEXTURE_CATCHUP=0`**, **`VICTORY_TEXTURE_STAGE_MS=16`**
     (same in `scripts/play-foxhound.ps1:71-72`). Code defaults are 250 ms and 3x bytes/time when >= 1,024 slots are due
     (`draw.rs` ~3178-3188). The comment there explains the origin: 1.6-2.2 s hitches in the first frames at 192 MB.
  2. Bytes: `staging_budget_per_frame` (`draw.rs:5853`): 384 MB at >= 20 GB VRAM budget, capped to 128 MB if `limited_host_ram()`.
     Not binding here (4-5 GB over 3 min), time is.
  3. `residency::prefetch_limit` (`residency.rs:151`): 64 MB/frame under pressure, 16 MB when commit is critical.
  4. `resource::room` (`resource.rs:1803`): no new textures when the card has no room (80 % line, `VICTORY_TEXTURE_ROOM_PERCENT`). Not hit.
  5. One pass per frame: the budget is in **frame** units while frames take 100-300 ms at the peak.
- Cost of the pass: 540-1,130 ms per 5 s (11-22 % of wall). Only 20-140 ms is staging; **450-600 ms is "the rest"** (queue locking,
  `Tells`, slot records, bookkeeping). Most of the budget goes to per-slot overhead, not moving bytes. [measured]

### 3.2 Uploads ahead (copy queue) [static]
`crates/victory-gpu/src/texture/ahead.rs`: 2 threads ("uploads ahead N"), `MOST` = 96 MB of title memory in flight (**32 MB if
`limited_host_ram()`, i.e. total RAM <= 32 GB**, `residency.rs:93`), batch 16 MB / 64 textures. `claim` turns textures down for many
reasons (not fresh, tiled copy, grown, aliased, host-written...); a turned-down texture is uploaded inside the frame's list instead.
The turned-down count is logged only every 5,000 uploads and the line appeared once in this run (n == 1), so the fraction that
bypasses the frame thread is **unmeasured**. Right design, small capacity versus a 26k backlog on 16 cores.

### 3.3 Neutral-texture placeholder [static + measured]
`descriptor/host.rs` ~332-400 (`VICTORY_SHOW_AFTER=1`, `VICTORY_SHOW_NEW_LATER=1`): a slot shows the neutral texture until its upload's
submission finished (fix for the ~16-minute device loss in findings.md). Mean delay 0.2-1.4 frames, but frames are 100-300 ms at the
peak, so grey surfaces last seconds. Not the cause of minutes; it is what the player sees.

### 3.4 Rechecks counted in frames [static + measured]
`draw.rs:842 RECHECK_FRAMES = [4, 12, 40, 120, 400]`: a slot uploaded whole is fingerprinted again after 4, 12, 40, 120, 400 **frames**
(the title may fill pages after writing the view). At 3-10 fps the 400-frame stage is 40-130 s. The log shows 12-17k "uploaded whole
and waiting to be looked at again" at 09:49:36-09:50:08, 7.6k at 09:50:39-54, and a queue of ~1,000 for the rest of the session.
Costs 5-110 ms per 5 s and makes "queue length" look like unfinished loading.

### 3.5 Title memory residency [static]
DirectStorage writes into pages the title mapped (`xmem.rs`; `VICTORY_MAP_LEAN`, `lift_thread` for `VICTORY_STREAM_PRIORITY`,
`VICTORY_MAP_THREADS=1`). `14,163 passed over unmapped`: textures not mapped yet are skipped and retried (`Told::Later`). That is the
title's own streaming order and cannot be advanced without its requests.

### 3.6 DirectStorage emulation [static + measured]
`crates/victory-hle/src/modules/dstorage.rs`: one scheduler, workers = `available_parallelism` (16), staging floor 256 MB
(`VICTORY_DSTORAGE_STAGING_MB`, in `set_staging_buffer_size`), weighted round-robin lanes, handle per worker (`Io::Handles`), zlib +
BCPack (`victory-formats/src/bcpack.rs`), statuses completed in queue order. Page-mapping calls cost 200-400 us per page in the world
(xmem comment) and throttle the **title's** streaming thread, not our workers. Not the bottleneck.

### 3.7 Pipelines [measured; shared with other reports]
`pipeline/make.rs`: workers = 2/3 of logical processors (8 here) at lowest priority. Pipeline book exists
(`pipeline/known.rs`, `VICTORY_PIPELINE_BOOK=1`, `victory warm`). ~2,800 first binds / translations at 09:49:15-26 coincide with the
4.8 fps trough. `dropped 260 draws/frame without a host pipeline (0 being made)` is steady afterwards (6 % of 4,420), not a loading effect.

## 4. Not the cause
- Nothing is prepared during the loading screen because the title only creates texture views when streaming into the world: the queue is
  empty for the 95 s before entry. "Preload at the end of the loading screen" is impossible without knowing what the world will need (section 6.3).
- The card is not limiting during the drain (0-13 ms of 50-200 ms), nor VRAM (4.9 of 24 GB), nor DirectStorage.

## 5. Ranked opportunities

| # | Change | Expected effect | Risk | Where |
|---|---|---|---|---|
| 1 | Wall-clock, adaptive pass budget: catch-up back on in play, slice 60-120 ms **only while the gate says Loading/Settling**, 16 ms in Gameplay | drain 3-6x faster [hypothesis]: ~90 s -> 15-30 s | hitches while settling (hidden by overlay) | `play.rs` 56-57, `draw.rs` ~3178-3200 |
| 2 | Move the per-slot pass (decision, `host_resource`, staging) off the recording thread onto a pool; frame thread only flips | frees 11-22 % of frame time permanently, removes fps dependence | medium: lock order with lists, `show_after`, `ahead::drain` | `draw.rs::heap_textures`, `texture/ahead.rs` |
| 3 | Scale `ahead`: workers ~cores/4, `MOST` 512 MB when RAM > 32 GB (128 MB otherwise), log turned-down counts every 5 s | more textures bypass the frame list | RAM on 32 GB | `texture/ahead.rs` consts and `workers()`, `residency.rs` |
| 4 | Recheck schedule in ms, not frames; skip rechecks for pages written by a completed DS batch | less pass time, honest queue length | small | `draw.rs:842`, `Told::Recheck` |
| 5 | LoadGate (6.1) plus "Preparing the world" overlay | user sees progress instead of a grey, 5 fps world | medium (heuristics) | new `gate.rs` in victory-gpu, `host/menu.rs`/`panel.rs` |
| 6 | Persistent texture book: per scene record DS source ranges and texture layouts; next run pre-read ranges into the OS cache and cache decoded BCPack | removes real I/O + decode from the burst [hypothesis, moderate: I/O is not the bottleneck today] | policy: hashes/offsets only, in `cache_dir` | `dstorage.rs`, `victory-pipeline/src/cache.rs`, book beside `pipeline/known.rs` |
| 7 | Pre-create host textures + staging from the book during Loading, so first-look cost (`host copies` 313 ms, `staging uploads` 223 ms per 5 s in the second wave) leaves the burst | cuts waves 2 and 3 | needs a per-run identity (DS destination size + layout hash + first-level hash) | `texture.rs::staging`, `resource::host_resource` |
| 8 | Steady 27 MB/frame "copies" (10-12 ms) after the drain | better "after" fps | other report (preparing thread) | `host.rs` |
| 9 | Auto-warm from the pipeline book at start | removes the 09:49:15-26 trough | small | `apps/victory/src/translate.rs::warm`, `pipeline/known.rs` |

## 6. Design: LoadGate + texture book

### 6.1 Detecting loading and scene transitions
Evaluate once per presented frame (the 5-second stats in `host.rs` already read most of these):

| Signal | Source | Meaning |
|---|---|---|
| draws per frame | host frame stats (1-11 menu/loading, 600-6,900 world) | menu vs world |
| DS bytes in flight + queue depth | `dstorage.rs` `Lanes` (add an atomic like `READING`) | title streaming |
| `pack\*.rpf` opened | `dstorage.rs::open_file` (`core_restofworld`, `story000`...) | scene transition marker |
| texture queue due count + oldest age | `texture_slots_due_known`, `texture_slots_waiting` | adoption backlog |
| pipelines being made | `pipeline::make` queue (the `(N being made)` figure) | shader stalls pending |
| uploads ahead in flight | `ahead::IN_FLIGHT` | copy backlog |

States: `Menu` (draws < 50) -> `WorldEntry` (draws jump > 500, or a `pack\*.rpf` opens) -> `Settling` while any of (due > 256, ahead in
flight > 0, pipelines making > 0, DS bytes in flight > 0) -> `Gameplay` after **3 s** all quiet (hysteresis). A new `pack\*.rpf`, or a
draw-count collapse, re-enters Loading/Settling (fast travel, missions, cutscenes).

### 6.2 What the gate does
- `Settling`: pass slice 120 ms with catch-up; `ahead` uncapped within the RAM rule; pipeline workers at normal priority
  (`VICTORY_PIPELINE_LOWEST` off); rechecks suspended; one progress log line a second (due, ahead in flight, pipelines making, MB uploaded).
- Overlay fade "Preparing the world n %" (progress = 1 - due/peak_due) with a 90 s timeout so a miscount never blocks. The title is
  not paused (unsafe); the player just stands still behind the overlay for 15-30 s instead of 3-5 minutes.
- `Gameplay`: restore the 16 ms slice, rechecks on.
- Memory bounds, no blind loading: only slots the title **names** are uploaded (4-5 GB measured), under `resource::room` (80 % of the
  VRAM budget) and `prefetch_limit`; staging capped at 60 % of free host RAM.

### 6.3 Texture book (persistent, offline)
- Record during play (like `VICTORY_PIPELINE_BOOK`): per scene (set of `pack\*.rpf` open), each DS request as
  `(file id, offset, source size, destination size, options)` and each texture the pass uploads as
  `(layout hash, first-level bytes, mapped bytes)`. File: `cache_dir\texture-book\<build id>.bin`. Offsets and hashes only, no asset
  bytes (asset-policy rule 1; `ensure_outside_repo`).
- Use on the next run: when a scene pack opens, a low-priority thread reads the recorded ranges in file order so the OS cache holds them
  before the title asks; decoded BCPack output can be cached content-addressed via `victory-pipeline` cache; host textures are
  pre-created from layouts into the pool (`VICTORY_TEXTURE_POOL=1`) so a slot write only needs staging.
- Bounds: only items seen in that scene on this machine; same VRAM/RAM caps; evict entries unused for 5 runs.

### 6.4 Rollout
1. Wall-clock slice + catch-up under the gate (opportunities 1, 4) with the 1 s progress log; target: due backlog < 30 s.
2. Gate state machine + overlay (5).
3. `ahead` scaling and turned-down logging (3), then pass off the frame thread (2).
4. Book recording (no behavior change), then cache warming (6), then pre-created textures (7).

## 7. Measure first (cheap, confirms the diagnosis)
1. Run with `VICTORY_TEXTURE_CATCHUP=1 VICTORY_TEXTURE_STAGE_MS=120`: compare time from `core_restofworld.rpf` open to `queue < 500
   slots`, and the longest frame. A drop to ~30 s confirms it.
2. Log `ahead` STATS (sent vs turned down) every 5 s.
3. If the player has 32 GB RAM, `limited_host_ram()` (`<= 32 GB`) is true: `ahead` is capped at 32 MB and the pass at 128 MB/frame.
   Log it once at start; the logs here do not say which RAM class they came from.
4. Time `texture::detile` and `refresh` per MB on fast vs slow cores (`cores.rs` pins only the helpers).
