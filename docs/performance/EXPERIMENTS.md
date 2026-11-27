# Performance experiments

## perf/pipeline-order: early queue by first use, loading boost, make-time log (UNMEASURED IN-GAME)

Status: implemented, builds, unit tests pass. **Not run in the game. No frame-time or load-time result exists yet.** Opportunity #2 of `AGENT_REPORTS/04-shader-pipeline.md` plus item 4 (instrumentation). Touches `pipeline/{make,library}.rs` only; victory-shader and the static-sampler library bypass are unchanged (no translator hash change, no cold start).

What changed
- New `pipeline-order.txt` (cache folder): `key ms` per line, first-used first; line number is the rank. Written with the library/early-list save. `early-pipelines.txt` and `slow-pipelines.txt` keep their formats (the early file is now written in first-use order, still readable as a set by older builds). A missing file means no order (creation order, as before).
- EARLY and slow queues are binary heaps by recorded rank; unranked pipelines go last in creation order. `VICTORY_PIPELINE_ORDER=0` restores FIFO.
- Boost: when 32 or more pipelines wait, workers (not the screen workers) go from LOWEST to BELOW_NORMAL (`VICTORY_PIPELINE_BOOST=normal` for NORMAL, `0` to disable) and the slow cap (workers/4) rises to workers-1. Cleared when the queue drains ("all made").
- Release log: `host pipelines {loaded from the library | made | made, static samplers (never in the library)}` with count, p50/p90/p99/max ms, total_s. Emitted at "all made" and at most once a minute with saves. The static-sampler line answers whether the driver cache makes those compiles quick.

How to measure (to do)
1. Run once to write `pipeline-order.txt` (the first run only records; ordering applies from the second).
2. Short-loading start, compare frames over 200 ms after world entry and `mk` at world entry against run 095317 (250-340 ms frames for ~2.5 min).
3. Read the new `made, static samplers` p50/p99 lines. p50 in the low ms means the driver cache hits.
4. A/B `VICTORY_PIPELINE_BOOST=0` and `VICTORY_PIPELINE_ORDER=0`.

Risks: BELOW_NORMAL workers may take CPU from the title's loading threads (unmeasured); a worker keeps its priority until its current pipeline ends; with the cap lifted all but one worker can sit in 10-58 s compiles.
# Experiments log

Status words: MEASURED (in-game numbers), UNMEASURED (built and unit-tested, not run in-game), NOT DONE (reason given).

## perf/descriptor-sync: report 03 item C (gate and coalesce heap rounds, skip ring pages)

### Step 0: counters (UNMEASURED in-game)
Added, logged with the existing "descriptor records ..." frame line (`descriptor.rs` `sync_line`):
- "the heaps' records, rounds per frame": rounds run, rounds with no changed page, kernel write-watch asks
  (`Tracked::take`, one per heap per round), ms in asks that found nothing vs found pages (E4, as far as
  `descriptor.rs` can see: the kernel call and the journal work inside `watch::writes` are not separable
  without editing `watch.rs`, which this branch does not touch), ring pages (more than 90% of the changed
  records on the page were "of draws' tables"), MB read on them, records changed on them (E5).
- "view creation per frame": buffer views and raw twins created in `buffer_srv` with their time in
  `CreateShaderResourceView` (E2; `VICTORY_DESCRIPTOR_TIMING=0` counts without timing; clock read about 25 ns
  twice per view, so about 1 ms thread time a frame at 17k twins; unmeasured).
- E1 (switch gating the heap buffer pass) and E3 (picked bitmap) need `draw.rs` / shader emit: not done here.
What to read: ring MB vs total MB read ("MB read" in the records line) says how much of the 16.5-17.8 MB a frame
step 1 could skip; "no changed page" rounds say how many of the 17 rounds a frame are pure overhead.

### Step 1, skip ring pages: NOT DONE (not behaviour-preserving without the picked bitmap)
A new record on a ring page that no draw has read yet is indistinguishable from one a shader will pick by
computed index; `copied_tabled` is only stamped after a draw reads the slot, i.e. after the round that would
have mirrored it. Skipping the page's read/compare leaves a bindless-picked slot with a stale or null host
descriptor, which violates R4 (a slot must not show a texture that is not uploaded, or freed memory;
ARCHITECTURE_AUDIT.md 2.1). The report itself lists this as the medium-risk step and makes it depend on
opportunity A (GPU-written picked bitmap) or a periodic sweep, and a sweep changes when picked slots update,
which is not provably identical. Needs the in-game ring-page counter above first.

### Step 2, rounds only for submissions with pick-by-index draws: NOT DONE (needs draw.rs)
The signal ("a list recorded a draw whose shader has SLOT_STRIDES / SLOT_TABLE_STRIDES, or uses `lent_heap`")
exists only in `draw.rs` (`heap_buffers`, `heap_strides`, around draw.rs:2046-2080, and 1878). Setting a flag
there is out of this branch's scope. Without it, gating on anything else (frame, page counts) would skip
rounds that other readers need: draws read their tables directly, but compute and `lent_heap` dispatches
read heap slots by index, and a descriptor the title copies after its last API call is mirrored only by a
round. Suggested follow-up: a `crate::heap_marks::note_pick()` thread-local set from `heap_buffers`,
`heap_strides` and the lent-heap path, read by `host.rs submit` to choose `begin_sync_heaps_after(mark)` or
reuse the previous round (dirty ranges accumulate in `Tracked::seen`, so one later round still sees all
writes). Keep the invariant that a round is never skipped for a submission whose lists have the flag.

### Step 3, coalesce write-watch asks: NOT DONE (would corrupt shared journal state)
The existing coalescing tool, `watch::ask_ahead`, writes the global journal's `ahead` set and
`ask_ahead_done` clears it. That state belongs to the submitting thread's refresh groups; the heap-sync
thread runs concurrently and would clobber it (and `journal().kept` makes it a no-op while a submission's
`ask_once` stands). A coalesced ask across both heaps needs a new entry in `watch.rs` (and a flat per-page
`seen` instead of the `BTreeMap` mutex), which is out of scope. Expected gain is also small: a round is
one kernel call per heap (usually one or two shader-visible heaps), not one per page.

### Invariants kept
No behaviour changed: descriptor slots, dynamic-slot release on GPU completion and upload-before-show are
untouched; only atomics and clock reads were added.

## Branch perf/memory-footprint (report 02: measurement, R4, R3)

All three items are UNMEASURED in-game. Build: `cargo build --release -p victory` ok; `cargo test -p victory-gpu`
415 pass, 2 fail (`indirect::tests::host_list_budget_starts_empty_at_each_submission`, `warp_batch_charges_every_execution`),
both fail identically on the base branch.

### 1. Footprint counters (measurement only) - `copystat_footprint.rs`
Per host copy, distinct 64 KiB blocks (since the copy was made): written by shaders (bytes that landed in title
memory: `land_card` / `land_back`, merged > 0), changed by the title's CPU after shaders had written them
(`Shadow::take` runs, `take_landed` non-echo, `copy`), and written by the title while pending
(`merge_under_writes` runs). Logged every ~10 s from the frame-line block in `host.rs` (not diagnostics-gated):
one total line and the top 8 copies. `VICTORY_FOOTPRINT=0` turns it off. Cost: one global mutex per event
(hundreds per frame), 1 bit per block.
Read it for: shader-written % of the 174 MB heap's 2,784 blocks (report 02 R1 decision: under ~20 % means a learned
footprint pays), and `pending title` vs `also shaders'` (pending-and-never-shader blocks are the false sharing behind
the 9.6 ms of merge-under-writes).
Caveats: "then title" is an upper bound where a refresh copies whole pages (`copy()`), and for copies in PC memory a
landing's own write shows as a title write (the card path filters those with the echo compare, the PC path does not).

### 2. R4, batched write-watch ask: mostly already there; one small change
Per-submission batching exists and is on by default (`VICTORY_ASK_ONCE`: `watch::ask_once` -> `ask_ahead` merges
ranges within 8 MiB, `ask_rest` takes what is left). Report 02's many asks therefore come from asks that miss the
standing journal (after `ask_once_end`, `merge_under_writes` -> `writes`, flights' late merges), not from the
submission's own refresh groups. Done: `ask_rest` sorts ranges by start, widest first, so a range contained in one
just asked is answered by it (before, refresh order could ask the small range and then the big one holding it).
Same pages found, same journal, never more kernel calls than before. Expected gain: small, unmeasured.
Not done: one ask per region per FRAME. A title write between two submissions of a frame would then be seen a frame
late by the later submission's refresh (stale bytes on the card), which breaks "title writes reach the copy before
the submission that reads them". A real gain needs the miss counts first (diagnostics build: `copystat` watches per
frame, split by caller).

### 3. R3, per-copy write epoch so `defer_refresh` is one compare: NOT DONE
`defer_refresh` only records a `Step::Refresh` (which also keeps the copy alive until the list is reset); the work
(`nothing_new`, `update`) runs at submission. Skipping the step at record time on an epoch compare would read the
copy's state before the title's later writes in the same frame: a write between recording and submission would be
missed (audit invariant R2). Doing the compare at submission instead needs a per-copy epoch bumped on every journal
`note()` for pages of that copy, i.e. a page -> copy map on the fault/ask path (`watch.rs on_write`, `Journal::note`);
a global epoch is useless since the title writes every frame. That is a new structure on the hottest path and needs
the step 1 counters and a diagnostics run first.

## perf/submit-thread: R8 sweep without drain, R3 joined card-step copies (UNMEASURED IN-GAME), R3 gather and R7 not done

Status: implemented, `cargo test -p victory-gpu` passes except the 2 known indirect failures. **Not run in the game.**

R8 (`host.rs`, submit's sweep): the forced `settle(WHY_SYNC)` every 1.5 s behind flights is gone. The sweep only drops what no list or slot names (`strong_count == 1`, COM refcount 1) and does it through `retire()`, which holds objects until every submission started so far has finished (submission numbers are the fence values; `bury_lag` adds 16/64 on top). A flight's lists keep their objects counted until landed. `VICTORY_SWEEP_SETTLE=1` restores the drain (also kept when the up-check is on). Measure: the 10-30 ms spike every 1.5 s in frame times, and `a synchronous submission or present` in the late line. Risk: a missed holder is a freed-memory fault (device loss); soak with `SLOT_CHECK`.

R3 (`card_steps.rs::record`): not a gather kernel. Reading the code shows the report's premise is too high: a fill that goes to a descriptor slot (`To::Slot`, stage 1) records no command at all; reads are deduplicated per place (a few hundred, "2,800 fills of a few hundred places"), only copies and stage-2 buffer fills write. With `VICTORY_FILL_TITLE` on by default the small ring fills are mostly no longer taken here. The 4.6-8.6 ms "planning" is CPU planning (`does_of`, `plan`, `holder`, `mapped` page-table probes, 5.4k `constant_slot` writes), which a kernel would not touch. A real gather also needs a new PSO, a mapped upload table per `Own`, and a per-source/per-destination state scheme (NON_PIXEL_SHADER_RESOURCE/UAV instead of COPY_SOURCE/DEST, with the "both" case), and the scatter kernel in `send.rs` is one destination UAV per dispatch, so it is not reusable as is. Not done until a capture shows the command count. Done instead: runs touching in both buffers (reads may cross equal under-256-byte padding) go in one `CopyBufferRegion`; `VICTORY_JOIN_STEPS=0` restores one each. Measure with `--timing detailed` on the card-steps list.

R7 not done. Present cannot simply move: the next `present_planes` reads `GetCurrentBackBufferIndex` and records into that buffer, so it must come after the previous `Present` call, which makes the move a hand-off with a wait; Streamline's `present_begins/called/returned/ends` and the tags belong to one thread per frame (frame-gen semantics); and the resize path settles and calls `ResizeBuffers` under the presenter lock. What would be left to overlap is ~1.5 ms/frame (`Present`) plus `panel/menu/meter.frame`, which are bookkeeping (a once-a-second log). Not worth the ordering risk without an in-game Streamline test.

## perf/read-wait: unnamed "title memory read" settles (UNMEASURED, NOT COMPILED)

Status: written and read through only; the user compiles. Log play-20261127-101425-23388.log (VICTORY_CARD_STEPS=2, relay on): 0.5 settles/frame for WHY_READ, ~30 ms each, while the three named counters (copy, fill, texture) are 0. So the wait comes from a `Some(WHY_READ)` that is not counted: `place::read_by` (host.rs, end of `blocked_by`) or `card_steps::copied_read` (`late_blocked`). `asked_sizes` (host.rs send_late) is off unless `VICTORY_BVH_SIZES_WAIT=1`. Both waits end in `relay_wait` until `line.written` no longer touches the read, and `written` includes the title's 174 MB heap while a skinning flight is in the air, so the wait drains those flights (tens of ms).

Change 1 (counters): the frame line now names two more sources inside "title memory read": `a step that reads where a copy at its place writes` (LATE_STATS 18) and `a refresh or host read of where a copy done on the card writes` (19). Read the next log to see which dominates.

Change 2 (fix for the first): `place::read_by` asked whether ANY step of the submission reads the destination of a copy at its place whose flights are in the air, wherever the step stands. Submission runs steps in list order (`run_deferred`), and a copy at its place is not put into title memory when flights may write it (`Placed::prepare(free=false)`). A step that stands before the copy reads the old bytes with or without the wait, so waiting for it changes nothing. `blocked_by` now checks each step against the destinations of the copies at their place ahead of it only (`place::reads`). Invariants kept: a reader behind the copy still waits; the readers' own source checks against `written` are unchanged.

Not changed: `copied_read`. A refresh of another host copy over a card-copy destination really needs the landed bytes (the draw reads that host copy), so the wait is correct until the copy also writes that other host copy or the refresh is given a card-side order; that needs the landing to be ordered on the queue and is a larger change. If the next log shows counter 19 dominating, that is the next work item.
## perf/tight-writes: learned footprint for root UAV declarations (UNCOMPILED, UNMEASURED; default OFF)

`VICTORY_TIGHT_WRITES=1` enables `tight.rs`. Root UAVs (`Draw::root_uav`) have no size; they declared the rest of the
title's buffer as shader-written. Now, per (pipeline, root parameter), the footprint relative to the root address is
learned from what lands (hook in `copystat_footprint::gpu_wrote`, which `land_card`/`land_back` already call). Learn:
full declaration, noted per copy for 400 ms, landings inside widen the footprint. After 8 landings and 32
declarations: tight, footprint +-256 KiB in 64 KiB blocks. Tight keys declare the footprint only, except every 16th
declaration (`VICTORY_TIGHT_VERIFY`) which is full and noted; a landing outside the footprint in a noted full
declaration makes the key wide for good (warn log). Keys that never saw a landing never tighten.
Not done: table UAV views in `readable` (sized, no pipeline identity there), persistence across runs, static shader
analysis. The view and `refresh` stay wide; only `gpu_writes` is narrowed.
Why default OFF: a write outside the footprint between two verifications is not declared, so it stays on the card
until the next declared write of its 64 KiB or verification; that violates "must reach title memory before the title
is told" for that frame. An exact fallback needs a card-side whole-range compare declared as compare-and-land (host.rs
card steps, not touched here) or a shader-side bounds flag. Attribution is a superset (any noted declaration of the
copy overlapping a landing), so errors widen, never narrow. Measure: log of `footprint of 64 KiB blocks`, and
shadow/flight lines with the switch on vs off. Code was not compiled or tested (instruction); unit tests in tight.rs.

## perf/prewarm-pipelines: earlier runs' pipelines made from process start, not at the title's creation (NOT COMPILED, UNMEASURED)

Status: written by reading only; **not compiled, not run**. Targets root cause 2 of `ROOT_CAUSES.md` (about 1,360 pipelines compiled after world entry) and the idle loading screen (~30 s of static images in which nothing was prepared). Based on `perf/radical-refactor`; builds on the pipeline-order entry above. victory-shader untouched (translator hash unchanged, no cold start).

What changed
- `known::prewarm()` (called from `apps/victory/src/run.rs` right after `use_shader_cache`, only with `--execute`, so not by `victory warm`/`translate`): a thread reads `pipeline-order.txt` + `early-pipelines.txt` (`library::plan()`, first-use order), looks each key up in `known-pipelines.bin`/`known-shaders.bin`, rebuilds the `Pending` (same key, checked) and queues it as EARLY on the existing workers (translation, library load or compile, `note_time`). It runs before the title image is even loaded, so the menu and the loading screen are the time used. First run (no order/early file or no book): logs "nothing recorded" and does nothing.
- `make::adopt` (one line in `pipeline.rs::hand_out`): the title's CreatePipelineState gets the prewarmed `Arc<Pending>` for its key (made, or in the queue; a draw promotes it to REQUESTED as before). This matters for the static-sampler pipelines (54%, never in the library): they were recompiled every run, and a made `HostPipeline` is now kept and handed out instead of thrown away. If the title creates one first, its own is used and the prewarm skips the key.
- `LOADING` phase from the prewarm's start: workers other than worker 0 at NORMAL priority (usual: LOWEST), known-slow cap raised from workers/4 to workers-1, and the early and slow heaps are drained together in first-use order (slow ones were behind all others). The loader counts as one waiting pipeline so "all made" does not fire between its entries.
- Loading over (priority back to usual, one log line): a world's frame (300 or more draws and dispatches, twice in a row; menu and loading screens draw 1 to 11) or everything queued made, or nothing to make. Constants only, no env switches added (existing ones still apply: `VICTORY_PIPELINE_ORDER=0`, `VICTORY_PIPELINE_SLOW=0`, `VICTORY_PIPELINE_WORKERS`).
- `victory warm --used` now makes the pipelines in first-use order too (reads `pipeline-order.txt`).

What to look for in the log (release)
- `pipeline prewarm: pipelines of earlier runs queued, first used first` : planned / queued / not_in_the_books / unreadable / key_changed (must be 0: the rebuilt key differs from the recorded one) / created_by_the_title_first / ms.
- `pipeline prewarm: loading over ...` : why ("the title draws a world", "everything queued is made", "nothing to make ahead"), seconds, made, found_by_the_title (hand-outs that found a prewarmed one), asked_before_made (draws that asked before it was made: the remaining stall), waiting.
- At world entry: `waiting`/`mk` near 0, the existing `host pipelines made` lines (count made after entry vs the 1,360 baseline), frames over 200 ms after entry, `a pass over the screen was waited for`.
- How to test: run twice (the first only records; needs `VICTORY_PIPELINE_BOOK=1`, set by `play.rs`); compare the second against run 095317 (250-340 ms frames for ~2.5 min). A/B a changed translator (cold driver cache) to see how much of the compile fits in the loading screen.

Risks
- Normal-priority workers compete with the title's loading threads (unmeasured); a worker keeps its priority until its current pipeline ends (up to 58 s for the worst).
- The host D3D12 device is created on the prewarm thread at process start (after Streamline init, before the title's device creation); the title's `D3D12XboxCreateDevice` finds it. If something in the title's device setup must precede `host()`, this is where it shows (not found by reading).
- Two title pipelines with one key now share one `Pending` (and `host` Arc); builder pipelines may be listed twice in `BUILDER.pipelines`. Prewarmed pipelines the title never creates stay alive (bounded by the recorded set, about 5,500).
- Compiler crash/hang now costs the process, not a child; but these are the same pipelines the title queued at creation before, so no new class of crash.
- The world threshold (300 draws) is a guess from report 05's numbers; if a menu draws more, loading ends early and the prewarm continues at the usual low priority.
## perf/texture-book: textures delivered as the title's DirectStorage requests land (UNCOMPILED, UNMEASURED; on by default)

Problem (report 05): the texture queue is empty for the ~95 s loading screen, then 26k slots / 82k textures / 5 GB are
made, detiled and uploaded on the frame-recording thread at 4-13 fps for ~3 min.

Principle: watch what the title requests and deliver it translated, in real time; nothing the title did not ask for.

1. Live, request-driven (`texture/ledger.rs`, re-exported as `victory_gpu::texture_book`). `dstorage.rs::work` calls
   `landed` after every successful request: the destination address (a plain address, or for page destinations the one
   run of title addresses `xmem::shown_at` returns for the written pages) and the request key go to
   `request_done`. If the description of the texture at that address is known and the request covers its bytes, the
   "texture book" thread (after 120 ms, and only if no other request overwrote that memory since) checks card room
   (`resource::room`), host commit (`admit_texture_pass`) and that the memory is mapped, makes the placed resource
   (`resource::texture_at`) and hands the upload to `texture/ahead.rs` (copy queue, detile on its workers). When the
   title later names the slot, `texture::unchanged` finds the host copy holding exactly the memory's bytes and the pass
   adopts it with no upload (outcome SAME; `adopted_by_slots` in the book line).
   Which case holds for the metadata (evidence): the streamed textures are views of memory without a resource object.
   Their description arrives only with the descriptor write: `descriptor.rs:1659 create_placed_srv` (device 0x230,
   `device.rs:457`) records `View::Placed(false, resource_desc, view_desc)`; `resource.rs:3269 texture_at` makes the
   resource lazily "the first time a draw reads one". Only `resource.rs:3129 create_at_address` (device 0x428) gives a
   description at creation, for resources the title makes as objects. So at request completion the format/size/mips/
   tiling are usually NOT known yet on the first fill of an address; they are known when (a) a view named that address
   earlier in the run (`ledger::desc_known` from `texture_at`, `ledger::named` from the pass), which is the common case
   for memory the title streams out and back in, or (b) the book on disk recorded that this very request (file, offset,
   sizes, options) filled a texture a slot named and kept, in an earlier run. (b) only fills in metadata for a request
   the title has just made; it never starts a load. Without either, nothing is done for that request.
2. Book on disk: `<cache>/textures/book.bin` ("VTB2", run number, entries of request key -> 14 resource words, first
   level, use count, last run). An entry is written only when a slot named the texture whole and resident; entries whose
   memory was seen overwritten by another request or streamed out (and not named again) are dropped at save; entries
   unused for 6 runs age out; at most 150,000, most used first. Saved every 30 s by a thread once 256 textures were
   named in the session. Offsets, hashes and layout words only (asset policy).
3. Converted-asset cache, write-through (`texture/converted.rs`, `victory_gpu::converted_cache`): in
   `dstorage.rs::execute`, a BCPack request (or zlib from 1 MiB) from a file with an output of 64 KiB or more is looked
   up by key; the entry is checked against a digest of the source bytes read (so a changed pack misses); a hit is read
   straight into the destination (no zlib, no BCPack); a miss decodes as before and queues the output to a writer
   thread (at most 512 MB waiting). Disk bound 24 GiB, trimmed to 90 % by least recent use (file time, touched on each
   hit). Folder `<cache>/textures/converted/<xx>/<hex>`, beside the shader cache (`d3d12x::use_shader_cache` sets it
   from the checked cache root), never in the repo or game folder.
   Entry format: `"VCV1"`, u32 version 1, u64 file (FNV-1a of the lower-cased opened path), u64 offset, u32 source
   size, u32 destination size, u64 options, u128 source digest (`texture::digest`), u64 output bytes, then the bytes
   exactly as written to the destination. File name: 32 hex digits of `texture::digest` over the 32 key bytes.
   What is cached is the decoded (still tiled) output, not the detiled texture: the detile now runs on the copy-queue
   workers off the frame thread, and reading a detiled file is not cheaper than detiling RAM-resident memory; the
   decode is what costs.
4. Loading gate from the request stream: `ledger::tick` (from the pass and from finished requests, twice a second):
   requests completing (>= 4 or >= 4 MB in the half second, or a `.rpf` opened in the last 8 s) while under 200 draws a
   frame (or no frame) -> loading, held 3 s after the last sign. While loading the pass gets 4x bytes and a slice of
   at least 120 ms; the copy queue may have half of free RAM in flight (a third otherwise).
5. Budgets from the real working set: `residency::textures_limited_by_ram` (< 24 GB installed) replaces
   `limited_host_ram` (<= 32 GB) for the texture pass byte cap and prefetch limit, so a 32 GB machine is no longer
   capped at 128 MB/frame and 32 MB in flight. `residency::staging_ceiling`: uploads in flight = free RAM / 3 (/ 2 while
   loading), 96 MB to 6 GiB; 32 MB on < 24 GB machines, <= 64 MB under pressure. Ahead workers: cores/4, 2 to 4;
   batch 32 MB / 128 textures. VRAM line: the launcher no longer sets `VICTORY_TEXTURE_ROOM_PERCENT` 80/86;
   `resource::room_line` uses constants: 88 % of the budget with frame generation on, 90 % ready but off,
   `line_for` (>= 90 %) without it; the render-target reserve (`targets_room_kept`) is unchanged on top.
6. Rechecks in time: `RECHECK_MS = [200, 600, 2000, 6000, 20000]`; the wait is the shorter of the frame count and the
   time at the measured frame rate (so 400 frames at 5 fps is 100 frames, 20 s).

Not done: moving the per-slot pass itself off the recording thread (the make-ready work and detiles now are); an
overlay. No env switches added. Log lines to check:
- `texture book: read what earlier runs kept` (entries, run) at start;
- `texture book: loading screen seen in the request stream; staging budgets raised` / `... is over` (seconds);
- every 5 s `texture book: the title's requests since the start`: `requests_done`, `texture_known`, `made_ready`,
  `adopted_by_slots` (the goal: most of `made_ready`), `unnamed_mb`, `no_room`, `no_host_ram`, `not_mapped`,
  `not_taken`, `overwritten`;
- every 30 s `converted cache: since the start` (hits, hit_mb, stored, evicted, on_disk_mb) and once
  `converted cache: decoded DirectStorage outputs kept on disk`;
- `textures uploaded ahead of the lists` (turned down count) and the queue line `textures picked by index, the queue`
  (peak should drop well under 26k on the second run), time from `core_restofworld.rpf` open to queue < 500.

### perf/read-wait, step 2: constants behind a copy at its place read on the card (NOT COMPILED, UNMEASURED)

Measured with step 1 (play-20261127-104020-14256.log, chase): "title memory read 0.8", all of it "a step that reads where a copy at its place writes", 18.6 ms/frame waiting. The readers really stand behind the copy. Most likely they are constant-buffer fills: the ring copy of a few hundred bytes a frame, then draws whose constants read it (`draw.rs` table CBVs -> `defer_constants`, misaligned root CBVs -> `fill_scratch`). With the copy's source or destination under flights' declared writes, `Placed::prepare(free=false)` leaves title memory alone, so a CPU fill behind it would read old bytes and must wait.

Change (`host/place.rs`, `host.rs`): `place_copy` notes its destination (this frame and the one before). `defer_constants` and `fill_scratch` check whether the constants overlap a noted destination. If they do, `HostList::placed_constants` records at this place in the list a refresh of the range, then a copy out of the host copy that shaders write into a 256-aligned piece of a per-list buffer of ours (1 MiB, then readable as constants). It does not record a CPU fill. A root view gets that address. A table slot is pointed at it when the list is submitted (a host step, `PointSlot`), after the draw pointed it at upload memory. The read is now in queue order, after the copy and every flight ahead of it, so no step reads the destination on the CPU and `blocked_by` has nothing to wait for. The value is the card's: what the copy and every write ahead of it left there. That is the same thing every buffer view and aligned root CBV of the heap reads already. Title memory is not written; the landing is unchanged.

Counter: "N constants read on the card behind them" on the line "title buffer copies at their place". Watch "a step that reads where a copy at its place writes" (should fall to ~0) and "title memory changed under shader writes" (WHY_STALE). The refresh of a range under declared writes can be settled there instead.

Not covered: readers that are textures, title buffer copies done at submission, or host reads of the destination. They still wait, and the counter would stay above 0.

## perf/coherence: refresh coherence at the root - asked-range lookup by halves, per-4 KiB part notes (NOT COMPILED, UNMEASURED)

Status: written by reading only (instruction: no cargo). Files: `watch.rs`, `shadow.rs`, `copystat_footprint.rs`. No env switch added; nothing in `host.rs`, `draw.rs`, textures or pipelines touched.

Evidence (logs 101425, 104020): "copies" 15-28 ms for 32-60 MB a frame in steady play; the 10:49 chase: refreshes 42.9 ms in 56 flushes of 12,800 spans, 10,977 known to copy nothing, "copies" 45.6 ms / 116 MB.

Root causes found
1. Every question "is this range asked already?" walked the submission's asked ranges one by one (`Journal::ahead`, a Vec in ask order). `ask_rest` pushes one entry per copy span not covered by `ask_ahead`'s 8 MiB runs, so it holds hundreds to thousands of ranges, and it is walked by every `nothing_new` (12,800 a frame in the chase, on 6 threads under the journal's read lock), every `writes` (twice), every `asked`, and inside `ask_rest` itself (quadratic). The "known to copy nothing" spans were paying this, not the copy.
2. A host copy's notes (`Shadow::copied`) were per 64 KiB page and only set when a refresh held the whole page. A page held in part - at every gap beside a range flights declare written (`update` refreshes around `pending`), at range ends - kept the note of its last whole take; once the title wrote any 4 KiB of it, every later refresh of the gap took its parts again, written or not, until some refresh held it whole, which beside ranges flights keep declaring is never. `ranged` (two remembered ranges) hid this for at most two ranges per copy. The same held for `merge_under_writes` (`stale_pages_in` page-level, pages noted only if whole): pages beside the declared ranges were merged again every submission and counted as reasons to settle.

Changes
- `watch.rs`: `ahead` is kept sorted and joined (`joined_pages`), looked up by halves (`covers`, O(log n)) in `writes`, `asked`, `nothing_new`, and `ask_rest` checks only the last joined run (ranges come sorted). Union semantics: every page of a joined range was asked under the standing number, so a range across two touching asked ranges is as asked as one inside either.
- `shadow.rs`: new `parts_taken` per copy (BTreeMap page index -> 16 stamps, locked inside `copied`). `stale_runs` (pure) decides what a refresh takes per 4 KiB part: a part's note is max(page note, part note, `ranged`), its write the journal's per-part write (`watch::part_writes`) or the page's. `stamp_taken` (pure) notes whole pages as before and, of pages held in part, each part held whole; a page all of whose parts are noted is promoted to the earliest. `copy_stale`, `nothing_new` (watch side), `stale_pages_in`/`stale_under_writes`/`merge_under_writes` use them. Without part notes the runs are exactly the old `by_parts` runs (test `runs_without_notes_of_parts_are_as_before`).
- Release log line every 10 s (from `copystat_footprint::log`): `refreshes that looked at title memory, per second: ..., MB never taken before, MB taken again ..., ends of ranges in the middle of a page ..., questions answered from the submissions' askings ..., ranges asked on average`.

Not done, and why
- Skipping refresh spans at record time (per-copy epoch / page->copy map): a span recorded before a title write that lands before submission must still copy it (audit "title writes reach the copy before the submission that reads them"); a list may be submitted again without re-recording. The check has to run at submission, so it was made cheap there instead (cause 1). A page->copy map on the fault/journal path would also have to track 4,000 overlapping copies (heap views) and their lifetimes on the hottest path, to save a lookup that is now O(log n) plus the pages written in the range.
- Landing ("copying back" 10-12 ms for 0.7 MB): it already fetches only the pieces the card compare flagged, in one batched list and one wait per landing (`Host::relay_card_back`). Its fixed cost is the round trip per landing (11-13 a frame, ~0.4 ms each, "waits for fetches") plus building the piece list over all ~6,000 compared pieces (1-2 ms). Both live in `host.rs` (card plan/fetch), not this branch's files; the next step is a speculative copy of last landing's flagged pieces into the read-back buffer inside the flight's own list, behind its compare, so most landings need no second round trip.

Expected (unmeasured): the chase's 10,977 known-nothing spans from ~3 us to well under 1 us each (several ms wall of the 42.9); "copies" MB down by the boundary-page re-copies (size unknown: read "MB taken again" and "ends of ranges in the middle of a page" against "copies MB" in the frame line). Also fewer "title bytes put in under flights' shader writes: pages looked at".

Risks: a wrong part stamp is a stale host copy (old geometry/constants). The notes only ever claim a part current when it was held whole by a refresh or a merge at that asking, the same rule pages had. Lock order: `copied` -> `parts_taken` (-> journal read); `ranged` never held while `parts_taken` is taken.

What to check in the log: the new `refreshes that looked at title memory` line; frame line "copies ms/MB"; `preparing per frame` "refreshes X in N flushes" and "asking the write watch"; "title bytes put in under flights' shader writes: pages looked at"; `settled for: title memory changed under shader writes` must not rise. With `VICTORY_REFRESH_CHECK=1` no "stale after its refresh, three times in a row" warnings.
## perf/thin-contention: per-call shared writes in list translation (NOT COMPILED, UNMEASURED; no switch)

Measured before (log play-20261127-104020-14256, chase 10:49, Ryzen 7 5700X 8C/16T, card 25 % busy): 70,418 list
calls in 812.9 ms of thread time a frame = 11.5 us a call, against 5.3 us in plain gameplay and 3.5 us in the
`list/thin.rs` header. The cost per call doubles under load: what grows with the threads is what they share. Read
from code (line numbers before the change): every call or draw wrote lines that every translating thread writes.

Removed (each was an atomic RMW or lock on one line for the whole process, per call/draw/lookup):
1. `list.rs:1934` `DRAWS.fetch_add` (`count_draw`, every draw, mesh dispatch and indirect execution) and
   `list.rs:2013` `DISPATCHES.fetch_add`: now `crate::sharded::Counter` (one 128-byte place per thread, summed by
   `take_counts` for the frame line). Same totals.
2. `draw.rs:653/1112/1124` `DRAWN`, `DROPPED`, `DISPATCHED` (`fetch_add` at draw.rs 3749-3781, 3816, 4361, 4367,
   4851, 4854, 5034, 5037, 5423, 5425: every draw/dispatch): `sharded::Counter`; `drawn()`, `dropped_now()`,
   `take_dropped()`, `dispatched()` sum the places. Same totals.
3. `descriptor.rs:1173` `VIEWS: RwLock<Views>`: a read lock (an RMW on the lock word) per view look-up, i.e. per
   `view_at` (descriptor.rs:1584), reached from `HostHeap::mirror`'s quick path (descriptor/host.rs:3903, every table
   entry of every draw, `draw.rs:4460`), `make_listed` (descriptor/host.rs:3480) and `descriptor::view` in
   `State::tables` (draw.rs:4584, 4593, 4699): tens a draw. Report 03 measured 410 ms / 5 s waiting for it. Now
   `sharded::ReadMostly`: a reader takes only its own thread's lock (no shared line), a writer takes all 32 in order.
   Same exclusion as the `RwLock`; lock waits still counted in `LOCK_WAITS[2..4]`.
4. `descriptor.rs:1562` `forget`: took the writer's lock (stopping every reader) for every released resource; now
   looks first (reader's lock) and returns if the resource has no entry, as the old code did after the lock.
5. `host.rs:7900` `next_epoch` (one process-wide `fetch_add` per `HostList::changed_something`: state transitions
   `host.rs:6741/6747`, `twins_written` 6977, `defer_copy` 7245, `defer` 7680, i.e. per draw that moves a texture):
   each thread takes numbers from its own block of 1024 (`sharded::Numbers`). Epochs are only compared for equality
   (draw.rs 4573/4606/4664, indirect/merge.rs, descriptor/host.rs:2882, host.rs:6597); they stay unique and never 0.
6. `pipeline.rs:442` `bound`: `p.bound.swap(true)` on every `SetPipelineState` (list.rs:1618) wrote the pipeline
   object's line, which `host_pipeline` reads on every draw on every thread; now a load first, swap only the first
   time.
7. `tight.rs:149` one `Mutex<State>` taken by `declare` for every root UAV a draw/dispatch writes (draw.rs:2232) on
   every translating thread, and by `landed`: now 16 mutexes by key and 16 by copy; a declaration takes its key's,
   then (full ones) its copy's; a landing takes its copy's, collects the hits, then each key's. Never two at once.
   Same footprint state machine and order per key.

Not changed (hot, but out of this branch's files or not exactly behaviour-preserving), in likely order of cost:
- `resource.rs:1656` `hosted()`: a per-resource `Mutex` and an `Arc` clone/drop per call, 2-4 calls per texture
  per draw (`ensure_state` -> `HostList::transition` -> `host_copy`, `twin_use` -> `twin` + `host_copy`, and
  `readable`'s `host_copy`, draw.rs 1861, 1865, 5668, 5681, 5691). Render targets, depth and shared textures are the
  same objects on all threads: the same lines bounce. Fix: one `hosted` per texture per draw passed through
  (needs `HostList::transition_made`/`twin_use_made`), or a lock-free snapshot of `made` (it is replaced by
  `want_level`/`let_go_idle`, so needs an arc-swap or epoch scheme). Touches the texture let-go logic.
- `shadow.rs:5155/5175` `find`/`find_any` return `Arc<Shadow>`: a refcount RMW pair per vertex/index slot, root view
  and buffer view per draw on buffers every list uses (shadow.rs is another agent's).
- `descriptor/host.rs:2249` `HostHeap::dynamic`: the heap's `Mutex` per fresh table (draw.rs:4616), one heap for all.
- `pipeline/make.rs:849` `note_draw`: `DRAW_COUNT.fetch_add` per `host_pipeline` call (2-4 a draw) while loading.
  Not cached per draw: the count decides when loading ends, and calling it fewer times would move that.
- `host.rs:7228` `COPIES_JOINED` per deferred copy; `copystat::recorded` per call in diagnostics builds.
- `list/thin.rs` itself is fine: its shared counters and queues are touched per 1024-word chunk, not per call.

Expected: unmeasured. Items 1-6 take 3-5 shared RMWs per draw and tens per draw from VIEWS off shared lines; at
7k draws and 70k calls a frame and 100-300 ns per contended RMW that is roughly 15-40 ms of the 813 ms thread time
(a few percent), more where the VIEWS lock was waited for (report 03: 6 ms a frame in waits alone). The larger
remaining shares are `hosted()` and the shadow `Arc`s above. Check: the thin line's us per call
(`list calls translated by threads of ours per frame`) in the same chase, and `LOCK_WAITS` views share in the
descriptor line. Risks: `ReadMostly` writers now take 32 locks (a new view, or the release of a resource with
entries): fine at hundreds a second, slower than before at a burst of tens of thousands; epochs are no longer in
order across threads (nothing orders them).

## perf/raw-constants: every constant buffer read as raw bytes, in place at any title alignment (NOT COMPILED, UNMEASURED; no switch)

Measured before (log play-20261127-111731-14036): ~14 fps in every scene; the submitting thread works 71 ms of a
71.7 ms frame (title waits 29.6 ms at presents). Port #1 opened (`1 regions ... in place (185 MB)`), but per frame
only 371 constant views were read in place and 6,185 were "not in place: not 256-aligned", so "6149 fills read out
of them" still ran on the CPU every frame with their refreshes, card copies and bookkeeping (lists' steps 29.8 ms,
copies in card memory 10.9 ms). Cause: a D3D12 CBV must begin on 256 bytes; the title places constants on 16.

Change (structural, no switch; constants in code):
1. Translator (`victory-shader`, changes its hash: one cold retranslation and pipeline rebuild on the next run).
   Every constant buffer load (`s_buffer_load_dword*` through a CBV, `buffer_load_dword*` through a CBV, the
   heap-picked fallback) goes through `Lower::constant_view` / `constant_dword` (emit/lower.rs):
   - root constant buffers: raw loads (`rawBufferLoad`, DXIL kind 11) through a new raw root SRV `t<slot>` in
     `ROOT_CBV_SPACE` (`Shader::root_constants_handle`, replaces `root_cbv_handle`); byte 0 is the title's address;
     the address is masked to a dword and reads at or past `ROOT_CONSTANT_BYTES` (64 KiB, what a CBV shows) give 0
     without touching memory (a root view has no size);
   - constant buffers in tables, and picked from the heap: raw loads through the raw twin of the slot
     (`raw_twin`, props 11), which the host already kept for every slot (`raw_views`); the twin is sized like the CBV
     (`constant_bytes`: size rounded to 256, at most 64 KiB, cut at the buffer's end), so reads past it give 0;
   - without raw twins (`VICTORY_RAW_VIEWS=0` or a heap of 2x the twins refused): the CBV, as before.
   Metadata: `HostShader::root_cbvs` are now raw SRVs (listed with the root SRVs, PSV kind 4).
2. Root signature (`pipeline/host.rs`): `root_cbvs` are `D3D12_ROOT_PARAMETER_TYPE_SRV` root descriptors (never
   one-view tables). `draw.rs` `bind` sets them with `Set*RootShaderResourceView`.
3. Binding: `draw.rs` `root_view` asks `port::constants(address, 64 KiB, ROOT_ALIGN = 4)`; fallback binds the
   host copy at any 4-byte address (the 64 KiB scratch fill is left only for an address off 4 bytes).
   `draw.rs` `tables` asks `port::constants(address, size, HostHeap::constant_align())` (16 with twins, 256
   without); `HostHeap::constant_at` makes the CBV only on 256 (else a null CBV) and the twin as a raw view through
   `raw_view_range` (16-byte start, cut at the buffer's end). Fallback is unchanged (scratch + `defer_constants`).
4. Title-slot mirror (`make_twinned`, `View::Constant`): the twin of a CBV slot is now a raw view of the same bytes
   (was a second CBV), so heap-picked constant buffers read correctly as raw bytes; a host copy off 256 but on 16
   now gets a null CBV + a raw twin instead of a null slot.

Residual misalignment: none is passed. A raw root SRV binds at the exact title address (4-byte granularity covers
every dword address), so a root view has no residual. A raw view in a slot needs a 16-byte start; the console's
CBVs are on 16, so a table constant buffer off 16 (should be ~0) takes the old fill path and is counted as "not
aligned". A per-slot offset table would cost every table constant load an extra dependent load for that case.

Expected: the `renderer port 1` frame line shows thousands of "constant views read in place", "not aligned for
their raw view" ~0, "fills read out of them" ~0; the frame line's "copies ms/MB", card-step fills, list steps and
"preparing per frame ... refreshes" fall by the share the 6,149 fills had (lists' steps 29.8 ms, card copies
10.9 ms). GPU side: constants are read over PCIe from title memory (L2-cached) through raw loads; NVIDIA serves
uniform raw loads through its L1, not the constant cache: watch the card's busy % in the frame line.

Risks: (a) a wrong DXIL/root-signature mismatch shows as "host pipeline refused" / "host root signature refused"
on the first run (check the log after retranslation); (b) a table CBV read by a shader whose host heap has no twins
for that index (index past `twins.len()`) reads zeros; (c) twins are made on every CBV slot already, so no new
descriptor traffic, but the in-place path adds one `HostList::keep` (QueryInterface/AddRef) per bound constant
buffer, ~6.5k a frame; (d) same coherence assumptions as the port (ring discipline; first shader write to a page
read before it lands), now over ~17x more views.

## perf/raw-constants, revision: table constants as structured views of dwords (NOT COMPILED, UNMEASURED; no switch)

Measured after the first version (merged as perf/radical-refactor f597b66; log play-20261127-114321-28084): no root
signature or pipeline refusals, but per frame only ~108 constant views read in place, 2,841-4,075 "not aligned for
their raw view (4 root, 16 table)", 2,574-3,669 fills read out of ported memory, submitting thread 69.8 ms, 13.8 fps.

Cause, from code and log: (b), not (a). `descriptor/host.rs` `probe_view_limit` gives raw twins only when a heap of
2 x `RAW_TWINS` (2 x 2^21) descriptors can be made; NVIDIA caps shader-visible view heaps at 1,000,000
(`PORTABLE_VIEWS`), and the log says `host view heaps hold 1000000 views ... raw_twins=false`. With no twins,
`HostHeap::constant_align` returned 256, so every table constant fell back (and the merged `make_twinned` asked for
256 without a twin too). Twins cannot be made to fit: they sit at `2^21 + index` for every title slot, and the
title's heap alone is ~1,000,000 slots.

Fix (no twins needed, no residual offset needed):
- A constant buffer's slot (fresh table slots, `constant_at`; title slots, `make_twinned`; null ones,
  `null_constant_at` / `null_for`) is a structured view (`DXGI_FORMAT_UNKNOWN`, stride 4) of the CBV's bytes
  (`constant_bytes`: rounded up to 256, at most 64 KiB, cut at the buffer's end): `descriptor/host.rs`
  `constant_views`. A structured view's first element is any dword, so it begins at the title's own address (any 4
  bytes): no residual exists to pass. Its raw twin (AMD, where twins fit) is still made for picked reads.
- Translator (`emit/lower.rs` `constant_view`): constants loaded through tables read element `byte / 4` with a
  structured load (props 12, stride 4) of their own slot; picked ones read the raw twin where twins exist, else the
  same structured slot; root ones are unchanged (raw root SRV). The CBV path is left only for descriptors neither
  in a root view nor in the heap (none the host binds).
- `port::constants(address, len, Use)`: one alignment, `ALIGN` = 4 bytes, for every binder; `Use::{Root, Table,
  Mirror}` splits the counts.

Why not the per-slot residual table the request named: with structured views there is no residual (the view begins
at the exact dword); a residual table would need a root SRV in every shader that reads table constants, the host to
keep a per-slot dword up to date in both heaps' tables, and a dependent load before every constant read.

Log line now: `renderer port 1: ... per frame: N constant views read in place (... T in tables' slots, M in title
slots, the rest root views) ... not in place: off 4 bytes R root, T' in tables' slots, M' in title slots, S on pages
shaders write, O outside the regions; on the CPU F fills read out of them`. Expected: N in the thousands, R/T'/M'
~0, F ~0 (only constants on pages shaders write, or outside the region, still fill).

Risks: (a) structured loads of a slot that holds something else (a picked index from wrong data on a host without
twins) mismatch the descriptor kind, as the CBV reads did before; (b) mirror slots made in place hold the region
buffer until the title rewrites them; (c) translator hash changes again: one more cold retranslation.
