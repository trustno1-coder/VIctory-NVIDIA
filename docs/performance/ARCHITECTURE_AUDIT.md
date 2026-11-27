# Architecture audit: the VICTORY_* mechanism stack

Scope: read-only review of branch `perf/radical-refactor`. Sources: `docs/findings.md`, `docs/bug-journal.md`,
`docs/architecture.md`, `docs/replay.md`, `crates/victory-gpu/src/switch.rs`, module headers of the host layer, a grep of
every `VICTORY_*` name in `crates/` and `apps/`, and `Play-VIctory.cmd`. Nothing was built or run; no `.rs` file was edited.
The aim is to make deep refactors safe to attempt, not to argue against them.

## 0. Size of the problem

- 201k lines of Rust, 1,118 `#[test]`s. In `victory-gpu`: `host.rs` 11.5k lines, `descriptor/host.rs` 7.0k, `draw.rs` 6.0k,
  `shadow.rs` 5.9k, `watch.rs` 5.3k, `resource.rs` 4.9k, `indirect.rs` 4.1k.
- **425 distinct `VICTORY_*` names** appear in code. About 60 are paths, counts or diagnostics output; roughly 340 are
  behaviour switches of the host layer. By a heuristic read of the default idiom: about 130 default-on (`=0` disables), about
  100 default-off (`=1` enables), the rest numbers, modes, paths.
- **There is no single default table.** Defaults live in four places and they disagree:
  1. the code default at each read site, in at least six idioms (`toggled!`, `asked_for!`, `is_none_or(v != "0")`,
     `is_some_and(v == "1")`, `map_or(...)`, `match` on strings). Only two idioms use the macros in `switch.rs`, which are also
     the only ones `VICTORY_AB` can flip live;
  2. `Play-VIctory.cmd`, which sets about 21 further variables before launch: `RELAY=2`, `THIN=1`, `SHOW_AFTER=1`,
     `CARD_STEPS=2`, `INDIRECT_BATCH=2`, `INDIRECT_WINDOW=1`, `JOIN_COPIES=1`, `JOIN_PARTS=8`, `RELAY_COPY=1`, `WIDE_BACK=1`,
     `CARD_REHASH_RUNS=1`, `UPLOAD_AHEAD=1`, `LEVELS_ON_DEMAND=1`, `CARD_BUFFERS_MIN_KB=64`, `TEXTURE_NO_WAIT=1`,
     `MAP_THREADS=1`, `CARD_AWAKE=1`, `AMBIENT=0.8`, `AMBIENT_AIR=1.5`;
  3. `host/menu.rs` (in-game launcher menu; first value of each item is what the launcher starts with);
  4. other scripts under `scripts/`.
  A reader of the code sees `THIN`, `SHOW_AFTER`, `CARD_STEPS`, `JOIN_COPIES`, `INDIRECT_WINDOW` as off; the shipped
  configuration has them on. **Refactors must be validated against the launcher configuration, not the code default.**
- Documentation drift found:
  - `host.rs` calls relay stage 1 "off by default", but `shadow::relay()` is `!cfg!(test)` unless `VICTORY_RELAY` is set,
    i.e. on; `LATE_WAIT` unset derives stage 3 from it.
  - `findings.md` lists `VICTORY_THIN` as a gain without saying it is off in code and on only in the launcher; likewise
    `SHOW_AFTER` as the device-loss fix while its code default is off.
  - The bug journal says `VICTORY_WAVE_WIDE` is "intentionally kept off", but `victory-shader/src/emit/mod.rs:747` enables the
    widening path unless `=0` (the `wide_option` read at :624 is policy-gated differently). **Needs verification** before any
    translator refactor.
  - `findings.md` calls `INDIRECT_REST=0` "the experiment"; the journal (2026-11-25 21:30) made 0 the default.
- Shader translations are cached. `translation_fingerprint()` (`victory-shader/src/emit/mod.rs:365`) hashes the source hash,
  every switch named in the build-time `VICTORY_SHADER_SWITCHES`, and wave/RDNA/view-limit/raw-view values; `gpu/shader_cache.rs`
  also lists the `WAVE_*` switches. A new shader-affecting switch not added there leaves stale translations in
  `cache-pareto` and an A/B compares the same cached shaders.

## 1. Inventory by subsystem

"Protects against" is the failure the mechanism exists to prevent. The full name list per subsystem, split by detected
default, is in Appendix A (auto-extracted; the default class is a heuristic from the line that reads the variable and is
wrong in places; confirm at the read site).

### 1.1 The core: title memory vs. card memory

The one idea (findings.md): console CPU and GPU share memory; here they do not. Whatever one writes and the other reads must
be noticed and moved. Costs: noticing title writes, noticing shader writes, moving, waiting.

| Mechanism (module) | Key switches (default) | Protects against |
|---|---|---|
| Write watch + read-only guards on physical-page views (`watch.rs`) | `VIEW_GUARD`, `OPEN_ONE_VIEW`, `PAGE_PARTS`, `TAKE_PARTS`, `GUARD_HOLES`, `GUARD_REST`, `KNOWN_HOLES`, `LIGHT_REFRESH`, `ASK_ONCE` (on); `GUARD_ASK`, `FAULT_UNLOCKED` (off) | Stale host copies: title writes a page a copy already took; one physical page mapped at several addresses (a write through any alias must open all) |
| Host copies of title buffers, "shadow" (`shadow.rs`, `host/send.rs`, `host/spans.rs`) | `CARD_BUFFERS` (on), `CARD_BUFFERS_MIN_KB` (64 in launcher), `EXACT_BACK`, `WIDE_BACK`, `UNDER_WIDE`, `UNDER_WORDS`, `SEND_OWN`, `LAND_ECHO` (on); `KEEP_SNAPSHOTS`, `WINDOWS_OWN`, `WC_NO_BACK`, `WC_BAR`, `CARD_ALL`, `CARD_SPANS` (off) | Card cannot read title memory: give draws/dispatches a coherent copy; merge shader output without overwriting newer title bytes (`merge_under_writes`) |
| Landing shader writes into title memory (`land.rs`, relay worker in `host.rs`) | `LAND`, `LAND_BESIDE`, `LAND_ANYTIME`, `LAND_TAKEN` (on); `LAND_SKIP` (off) | Title CPU code reading what shaders computed (culling lists, skinning, ocean heights) sees old data |
| Relay: a thread lands flights while the submitter prepares the next (`host.rs`, `queue/behind.rs`, `heap_marks.rs`) | `RELAY` (on; stage 2 in launcher), `LATE_WAIT*`, `RELAY_COPY` (launcher), `SPLIT_WAITS`, `CUT_WAITS`, `WAIT_FORCE`, `LATE_WAIT_WRITES`, `SUBMIT_DEPTH`, `SUBMIT_FRAMES` | Latency (serial prepare/run/land was ~220 ms of a frame); and ordering: anything observing title memory, a fence or a present must see earlier flights merged |
| Texture read-back (`texback.rs`) | `TEXTURE_BACK` (on; four water formats) | **Ocean height/displacement maps never reaching title memory: boats sink, sea plane drops** |
| Picked-view writes (`picked.rs`) | `PICKED_WRITES` (off) | Shader writes through views chosen by computed heap index never landing (4.6 MB/frame in one compute shader) |
| In-place title memory as heap (`inplace.rs`) | `INPLACE`, `INPLACE_PROBE` (off) | Experiment to remove copies; constrained by driver rules in the file header |
| Title-side write interception (`titlewrite.rs`, `safecopy.rs`) | `COPY_BESIDE`, `STORE_BESIDE` (off); `SAFE_COPY`, `SAME_RECORDS` (on) | Cost of protection flips per write fault (590 faults/frame measured); emulate `memcpy`/stores without opening blocks |
| xmem page pool / views (`victory-hle/modules/xmem.rs`) | `MAP_LEAN`, `SECTION_WINDOW`, `STREAM_NOTICE` (on); `SOFT_VIEWS`, `LAZY_VIEWS`, `MAP_RUNS` (off); `MAP_THREADS` (launcher 1) | Remapping cost; keeps guard state consistent when views are mapped/unmapped |

### 1.2 Descriptors and views (`descriptor.rs`, `descriptor/host.rs`; 27 names)

Console descriptors are plain bytes in title memory, mutated by the CPU and read by shaders by index. The host layer mirrors
them, builds a raw twin of every buffer view, and keeps "dynamic slots" (262k pool on NVIDIA).
On: `HEAP_SYNC`, `HEAP_SYNC_ASIDE`, `RAW_VIEWS`, `RAW_VIEW_CLAMP`, `VIEW_INLINE`, `VIEW_EXTENT`, `VIEW_BYTES`, `KEEP_VIEWS`,
`LIST_VIEWS`, `QUICK_MIRROR`, `QUEUE_MARKS`, `QUEUE_HOST_TEXTURES`, `TABLED_TEXTURES`, `SLOT_HOLDS`, `LENT_HEAP`. Off:
`SHOW_AFTER` (launcher on), `SHOW_NEW_LATER`, `HEAP_SYNC_AT_SUBMIT`, `SYNC_FINE`, `SYNC_IN_PLACE`, `SPANS_QUICK`, `TABLED_REREAD`.
Protects against: shaders reading a slot whose texture is not uploaded (device loss about every 16 minutes); out-of-bounds raw
loads (console returns zeros past a view, PC does not); dynamic slot exhaustion (journal 21:30: completed submissions pinned
slots across `bury_lag()`).

### 1.3 Textures, residency, upload (`texture.rs`, `resource.rs`, `pool.rs`, `tiles.rs`, `residency.rs`, `texture/ahead.rs`; ~31 names)

On: `LAZY_TEXTURES`, `TEXTURE_POOL`, `TEXTURE_GUARD`, `TEXTURE_HOT`, `TEXTURE_SAME`, `TEXTURE_ONCE`, `TEXTURE_FILL`,
`TEXTURE_SWEEP`, `HELD_LEVELS`, `DEPTH_TWINS`, `POOL_SMALL`, `UPLOAD_LOW`. Off (first three launcher-on): `UPLOAD_AHEAD`,
`LEVELS_ON_DEMAND`, `TEXTURE_NO_WAIT`, `PRELUDE_UPLOADS`, `TEXTURE_COPY`, `MAPPED_BY_PAGE`. Plus budgets (`TEXTURE_ROOM_PERCENT`,
`TEXTURE_BUDGET_MB`, `CARD_ROOM_FOR_*`) and, per the journal, a commit-headroom state machine: mild pressure vs
`critical_commit()` (< 1536 MiB), pressure entry 3 GiB RAM / 4 GiB commit, recovery 5/6 GiB, staging caps 16/32/64 MiB per
frame, `REST = 2500 ms` growth gate, 500 ms creation backoff, bake-specific reserves and a synchronous bake upload list.
Protects against: 40+ GB private memory, `E_OUTOFMEMORY` storms, texture starvation and flicker, pop-in, bake passes
(ped/decoration) needing exact title bytes at that moment.

### 1.4 Draws and indirect execution (`draw.rs`, `indirect.rs`; ~40 names)

`INDIRECT_BATCH` (launcher 2), `INDIRECT_WINDOW` (launcher 1), `INDIRECT_MERGE`, `INDIRECT_REST` (off), `INDIRECT_WIDE`,
`INDIRECT_CARD_BUDGET`, `THREAD_DIMENSIONS`, `WRITE_SKIP`, `BAKE_EXACT`, `BAKE_SOURCES`, `FULL_CARD_PASS`, `HEAP_RECHECK`,
`HEAP_SPREAD`, `READY_ONCE`, `RECHECK_SOON`, `QUEUE_BATCH`. Protects against: console ExecuteIndirect semantics on
GPU-produced arguments (counts clamped to buffer size and a per-list budget so a garbage record cannot hang the card), dispatch
groups over 65,535 (blocky patches), the ray dispatch "counts are threads" flag ("every ray missed"). In-tree `BENCH_*`
measurement hooks.

### 1.5 Submission and lists (`list.rs`, `list/thin.rs`, `host.rs`, `queue.rs`; ~60 names)

`THIN`/`THIN_THREADS` (lists written down, translated by eight threads), `JOIN_COPIES`, `JOIN_PARTS`, `JOIN_LISTS`,
`REFRESH_AHEAD`, `REFRESH_FRONT`, `WIDE_FILLS`, `WIDE_TEXTURES`, `WIDE_MERGE`, `LAZY_PRELUDE`, `FRESH_OPEN`, `LIST_POOL`,
`SCRATCH_POOL`, `CARD_STEPS` (do title-buffer copies and fills on the card), `ASYNC_SUBMIT`/`ASYNC_QUEUE` (measured not worth
it), `FAST_CORES`, `HELPER_PRIORITY`. Throughput mechanisms; all could in principle be replaced if the plain path stays as the
oracle.

### 1.6 Pipelines and shader translation (`pipeline/*`, `victory-shader`; ~41 names)

Pipeline workers, early (unordered) pipelines, `PIPELINE_LOWEST`, `PIPELINE_WAIT_*`, `SKIP_PIPELINES`, library mapping
(`LIBRARY_MAP/HANDLE/TRIM`), BVH builder (`BVH_BOUNDS`, `BVH_FENCES` on; `BVH_BUILDER`, `BVH_QUARANTINE` off). Output-affecting
translator knobs (cache-fingerprinted): `WAVE_WIDE`, `WAVE_LOCAL`, `WAVE_DEEP_SKIPS`, `WAVE_OWN_BITS`, `HELPER_OWN`,
`FIRST_HALF`, `FIRSTLANE_OWN`, `F16_NEAREST`, `FILTER_3D`, `STRICT_JOINS`, `MESH_OUTPUT_STRICT`, `SHADER_OPT`,
`SHADER_LOOP_LIMIT`, `RDNA_FLOAT`, `EARLY_DEPTH`, `RAW_VIEWS`, `VIEW_LIMIT`. Protects against: wave semantics mismatch (wave64
vs wave32, helper lanes in quad reads, loop-widening TDR), DXIL `Log` base, normalized codes, shadow artefacts.

### 1.7 Presentation and approximations (`streamline.rs`, `dxgi.rs`, `compose.rs`, `ambient.rs`)

`DLSS*`, `FRAMEGEN*`, `STREAMLINE_*`, `DISPLAY_*` are optional features. `AMBIENT*` is a stand-in for a missing
probe/lighting contribution, an explicit fidelity approximation; `AMBIENT_AIR=1.5` was added "to clear milky indoor haze",
a tuning knob on a workaround.

### 1.8 HLE, loader, formats (55 names)

Mostly debug, path and test hooks (`KEYS`, `SAVE_*`, `STACKS`, `XMA_*`, `BCPACK_*`, `DSTORAGE_*`). Real behaviour: `PATCH_WAITS`
(monitorx/mwaitx), `PATCH_CLZERO`, `CLZERO_ASK`, `ONE_PAD`, `SKIP_BINK`, `FRAME_INTERVAL_*`. These emulate console-only
instructions and are fidelity-required, not optimisations.

### 1.9 Diagnostics (replay, capture, book, timeline, pop-watch, fault-book, panel; ~37 names)

Mostly `diagnostics`-feature only. Not a refactor risk except as the **validation toolbox** (section 5).

## 2. Compatibility invariants

### 2.1 REAL invariants (violating one produces visible or physical wrongness)

- **R1. Shader-written data the title CPU reads reaches title memory in order, before the CPU can observe it** through a fence,
  present or memory wait. Instances: ocean height/displacement maps (`texback.rs`, tiled back in console layout), culling and
  skinning lists (`land.rs`), BVH sizes, ped/decoration bakes (`BAKE_EXACT`). Enforced by one landing thread, flights landed
  in `LINE` order, a landing completed whole before the title is told anything (`Host::relay_land`).
- **R2. Title writes are never overwritten by stale shader results or older copies.** A merge never stores into memory the card
  may still be writing, and bytes the title wrote after a flight was prepared survive (`merge_under_writes`, `SEND_OWN`,
  numbered sends). Failure seen: "rare lost write", 6 of 200 stress runs, fixed (0 in 603).
- **R3. A page shown at several addresses is clean only while every view is read-only** (`watch.rs` `cover`/`open`); a write
  through any alias invalidates all; a later-mapped view re-checks the others (`views_mapped`). Exempt: pages shown in more
  than `MANY_VIEWS` places.
- **R4. A descriptor slot never names a texture whose upload has not run, nor freed card memory** (`SHOW_AFTER`, `KEEP_VIEWS`,
  bake slot publication, `KEEP_FREED`, `release_tables`). Failure: device loss.
- **R5. Raw/structured loads past a view's end return zeros** (shaders rely on it): raw twins, `ROOT_SRV_DIRECT`,
  `RAW_VIEW_CLAMP`; the translator's `RAW_VIEWS`/`VIEW_LIMIT` must match the host's heap layout.
- **R6. Indirect execution is bounded** so one garbage record cannot lose the device, without zeroing every later draw
  (`INDIRECT_REST=0`).
- **R7. Wave and lane semantics:** helper lanes in quads (`HELPER_OWN`), wave ops with inactive lanes (`WAVE_OWN_BITS`),
  waterfall loops, dual-source blend, dead-lane join edges (journal 21:30). Translator passes must be exact.
- **R8. Dynamic descriptor capacity returns on GPU completion**, not 16 submissions later (NVIDIA 262k pool).
- **R9. Console-only instructions are emulated** (clzero, monitorx/mwaitx, TLS redirection) and the title's `memcpy` family is
  safe on guarded pages.
- **R10. Memory admission never fails hard:** backoff on `E_OUTOFMEMORY`, a split between mild and critical pressure.

### 2.2 INCIDENTAL invariants (true of today's implementation, not needed by the title)

- I1. Which thread lands (relay worker, `land` pool). Only the ordering in R1/R2 matters.
- I2. Granularity: 64 KiB blocks, 4 KiB kernel dirty bits, 32-byte compare units, 8-snapshot pool. Contract is "no stale byte
  survives" (R2); `PAGE_PARTS`, `UNDER_WORDS`, `CARD_HASH_LEN` are tunings.
- I3. Card-side hash passes as the way to find shader writes. The requirement is "know what shaders wrote"; alternatives:
  declared writes from descriptor analysis (as `PICKED_WRITES` does for one class), or in-place heaps if the driver permits.
- I4. Preparing every host list on the submit path (`THIN`, `ASYNC_QUEUE=plan`, `JOIN_*`, `REFRESH_FRONT` all rearrange it).
- I5. Per-slot descriptor mirroring and the sixteenth-pass re-look cadence (`HEAP_SPREAD`, `RECHECK_SOON`, `READY_ONCE`);
  partial ports of a journal (`HEAP_SYNC_AT_SUBMIT`, Belobug port 7) already exist.
- I6. `AMBIENT=0.8`, `AMBIENT_AIR=1.5` values (tuned visually).
- I7. Pool/block sizes: ripe blocks 12, staging block 64 MiB, `SNAPSHOT_POOL=8`.
- I8. Env-var configuration itself (read once per process in `OnceLock`s).

## 3. Workarounds on workarounds and over-conservative areas

1. **"Did the title write this?" has five detectors:** kernel dirty bits, read-only guards plus fault handler, a hash of
   textures "when the guard saw no write although the hash found one" (`TEXTURE_HOT`, an admission the guards miss writes),
   re-looking at slots every 16th pass (`HEAP_SPREAD`, `RECHECK_SOON`), and diagnostic checks (`REFRESH_CHECK`, `FETCH_CHECK`,
   `UPCOPY_CHECK`) that exist because the rest are not fully trusted. The cleanest simplification is one title-write
   notification contract with an oracle (V3, C2).
2. **Hide or delay a bad slot rather than make it correct:** `SHOW_AFTER`, `SHOW_NEW_LATER`, `KEEP_VIEWS`, `KEEP_SHOWN`,
   `neutral()`, bake slot publication, `STALE_TEXTURES`, `KEEP_FREED`. Each fixes the previous one's corner (journal 21:30:
   bake slots overwritten with `neutral()`). Underlying cause: slot lifetime not tied to upload and submission lifetime. One
   "slot publishes at upload-fence completion, releases at GPU completion" rule would subsume most.
3. **Pressure throttling** (`residency.rs`, `pool.rs`, `texture/ahead.rs`): about ten hand-tuned constants from two machines
   (32 GiB and 64 GiB RAM). Over-conservative on big hosts, fragile on small. Candidate: one budget model with pressure
   sources as inputs.
4. **Indirect clamps:** `INDIRECT_BATCH/WINDOW/MERGE/REST/CARD_BUDGET/WIDE`. `INDIRECT_REST` is the clearest
   workaround-on-workaround: the budget bounds garbage, then its leftover mode emptied all later records (suspected outdoor
   flicker), fixed by turning the leftover mode off. Replay `--skip-clamps`/`--batch-clamps` measure time; correctness cost is
   measured less precisely.
5. **Relay stack:** `RELAY` 1/2, `LATE_WAIT` 0..3, `SPLIT_WAITS`, `CUT_WAITS`, `WAIT_FORCE`, `LATE_WAIT_WRITES`,
   `SUBMIT_DEPTH/FRAMES`, `HEAP_SYNC_FIRST/AT_SUBMIT/ASIDE/EARLY`, `REFRESH_AHEAD/FRONT`, `JOIN_*`: four interacting
   pipelining levels, each with an "as before" off switch. Only the launcher corner is exercised in practice; other
   combinations are likely broken. The code comment that walls of the first room were "drawn in every run with this set and in
   fewer than half without it" shows these switches carry correctness, not only speed.
6. **Texture read-back by kind, not by texture** (`texback.rs`): any matching compute-written texture is copied back (~1 MB per
   list). Cheap and correct; keep until a title-read tracker exists (`READ_WATCH=1` is the instrument that found it).
7. **Where does a buffer's copy live:** `CARD_BUFFERS`, `CARD_ALL`, `CARD_SPANS`, `WINDOWS_OWN`, `WC_BAR`, `WC_NO_BACK`,
   `COPIES_IN_PLACE`: a design space explored one switch at a time; several are dead experiments kept with their code.
8. **Shader translation is already exact and best-tested** (`valu_tests.rs` 2.4k lines, differential tests against console
   `dxc`); findings say under 1 ms left there. Little to gain, little to fear.
9. **Measurement/dead switches in production paths:** `BENCH_*`, `BLOCK_WAY`, `MESH_TEST`, `SCISSOR_TEST`, `BARRIER_TEST`,
   `COPY_MODE`, `COPY_OVER_TITLE`, `TILES_TEST_CARD`, `WAIT_ASK`, `CUT_WRITES`, `FILL_TITLE`, `SEND_CHANGED`. Each is a branch
   a reader must prove dead.

## 4. Risks of deep refactors

### 4.1 Concurrency, lifetime, ownership

- **Threads (from module headers):** title threads (write faults arrive in the vectored handler, first in the process, shared
  with `safecopy`); the submitting thread holding the submission lock; THIN translate threads; the relay/landing thread plus
  the `land` pool; the heap-sync thread (and ASIDE); pipeline workers; raised-priority helpers (`FAST_CORES`,
  `HELPER_PRIORITY`); settle/sweep timer; DirectStorage threads. Cross-thread state is mostly `OnceLock` statics and global
  `Mutex`es in `watch.rs`, `shadow.rs`, `descriptor/host.rs`; ordering rules are in prose and enforced by convention.
- **Fault-handler context.** The handler and `safecopy` run inside faults of any title thread and of our own copies; they must
  not take a lock the faulting thread may hold, nor allocate. Moving state across a lock hierarchy can deadlock only under
  specific title behaviour (the lost-write race needed ~200 runs to show).
- **Descriptor lifetime.** `Detached._state`, dynamic slots released on GPU completion, stride buffers held by the list,
  `bury_lag=16`. Changing who owns a list or slot table changes when slots return. Guard: the "dropped draw: exhausted dynamic
  descriptor slots" log line must stay at 0.
- **Flights and declared ranges.** `shadow::relay` proves merge safety from declared ranges per flight. Changing what is
  declared (`WRITE_SKIP`, `PICKED_WRITES`, `CARD_STEPS`) changes that proof.
- **Slow-burn failures.** The 16-minute device loss and the 1-in-30 lost write do not appear in a 5 minute check. A soak run is
  part of validation.
- **`OnceLock` switches** cannot vary inside a process; two configurations need two processes (or `VICTORY_AB` for the two macro
  idioms in a diagnostics build).
- **Cache poisoning:** translations and pipeline libraries persist on disk; a translator behaviour change must change the
  fingerprint.

### 4.2 Fidelity-regression risks and what catches them

| Failure | Looks like | Caught by |
|---|---|---|
| Ocean maps not read back | boats sink, sea plane under (minutes) | boat mission run; `READ_WATCH=1` counts of CPU reads of water kinds |
| Lost title write (merge race) | rare world corruption | repeated stress test (603 clean runs was the bar) |
| Stale buffer/texture copy | flicker, pop-in, wrong materials | `REFRESH_CHECK`, `UPCOPY_CHECK`, `FETCH_CHECK`, `POP_WATCH` |
| Slot names unready texture | device loss in ~16 min | 30+ min soak, `book`, `SLOT_CHECK=1` |
| Wrong indirect clamp | outdoor flicker, empty draws | frame-line counters of clamped/empty records; replay compare |
| Translator regression | one shader black or blocky | `translate --validate --pipelines` (43/43 graphics, 96/111 compute baseline), differential tests |
| Landing order/timing changed | physics/AI drift (boat jitter) with identical pictures | not visible in pictures: needs title-memory digest (V4) |
| Ambient stand-in | haze or light level | screenshot compare only |

The last two rows are the dangerous ones: a refactor that reorders landing relative to title reads can pass every picture
test and still change simulation.

## 5. What validation exists, and what it proves

**Replay** (`replay/`, `docs/replay.md`, `tests/replay.rs`, `tests/replay_big.rs` ignored): records one frame of host D3D12
work and the memory it reads; replays without the title. Tools: `--compare`, `--images`, `--pixel`, `--diverge`,
`--check-written`, `--upcopies`, `--watch`, `--flow`, `--slots`, `--retranslate`, `--shaders`/`--shaders-b`, `--skip-*`,
`--no-barriers`, `--wait-all`, `--debug-layer`. The integration test runs a small synthetic program (clears, indirect draws of
textures picked from a heap, compute, buffers changed between submissions, descriptors replaced, aliased textures) with and
without the recorder, replays one frame, and requires every picture and buffer to match byte for byte.

It **proves**: a change to what is sent to the card (translator output, barriers, clamps, buffer residence, card-side hash
passes, PSO choice) gives the same picture and card memory for a recorded frame (up to the intrinsic non-determinism of atomics:
use `--diverge`/`--no-merge` to separate).

It **cannot prove**:
- anything on the CPU side of the host layer: write watch, guards, landing, descriptor sync, and the timing/ordering of landings
  (the replay re-injects recorded CPU writes between submissions; the logic that decided what to send is bypassed);
- multi-frame state (read-back chains, ocean, streaming, residency, pressure);
- real-time behaviour: races, TDR, stalls (the title stands still while recording);
- what `notes.txt` lists as not recorded: reserved resources, bundles, ray tracing, predication, stream output,
  `ClearUnorderedAccessView`, newer creation methods; addresses in indirect arguments are not rewritten.

The synthetic test is 128x128 and covers the record/replay path more than title regression. Other existing instruments:
`VICTORY_AB`, `VICTORY_TIMELINE`, `GPU_TIMING`, `POP_WATCH`/`POP_FOLDER`, `FAULT_BOOK`, `BOOK`/`SLOT_CHECK`, `REFRESH_CHECK`,
`UPCOPY_CHECK`, `FETCH_CHECK`, `READ_WATCH`, `WALLS_TRACE`, frame-line counters, `victory translate --validate --pipelines`,
differential translator tests (need `VICTORY_CONSOLE_DXC`, skipped otherwise). Capture screenshots with Windows Graphics
Capture (GDI returns white).

## 6. Validation checklist per candidate deep refactor

**Common baseline (once).** Golden configuration = `Play-VIctory.cmd` environment. Fixed save, a fresh copy per run, same cache.
`VICTORY_RECORD` captures of 3-4 named scenes (street, indoor, boat on the ocean, vehicle chase). Per run keep the log's
frame lines and counters: lost-write, dropped draws (descriptor slots), `E_OUTOFMEMORY`, device-lost. Land each refactor
behind a new default-off switch, pass the checklist, flip the default, delete the old path in a separate commit.

**C1. One default table (config-only).** Generate (name, default, owner, launcher override) and route all reads through it.
Proof: log the resolved value of all 425 names in old and new builds under the launcher env and under an empty env; diff must
be empty. Shader cache fingerprint identical; `translate` on the 154-shader set byte-identical.

**C2. Replace the guard/hash/recheck detector stack with one title-write notification layer.**
- Differential detector in one diagnostics process: the set of pages "written since last look" by old and new for every
  refresh; any page old marks and new misses fails. `REFRESH_CHECK`/`UPCOPY_CHECK` are the byte-level oracle.
- Stress: lost-write test, at least 600 runs (previous bar); aliased-views test (one physical page at N addresses; a thread
  writing a guarded alias while a submission prepares).
- 60 minute soak with `FAULT_BOOK` and `SLOT_CHECK=1`: zero device-lost, no increase in `POP_WATCH` blips.
- Handler audit: no allocation or new lock in the fault path.

**C3. Declared shader writes instead of card hash compare (extend `PICKED_WRITES`).**
- Keep the compare running in a diagnostics build and assert declared is a superset of compare-found for every frame of a
  10 minute drive (`--check-written`; `--upcopies` second view: "pieces shaders wrote that were not declared").
- Replay `--images` identical on all scenes; boat floats a full mission; V4 digest unchanged.

**C4. Collapse relay/late-wait/pipelining into one pipeline.**
- Characterise first: run relay x late-wait x launcher combinations 5 minutes each logging `WALLS_TRACE`, dropped draws, pops;
  find which are equivalent.
- Ordering test (new, needs a title-thread stand-in): a deterministic compute writes known words per flight; a CPU observer
  reads title memory at a fence, a present and a memory wait; it never sees flight N+1 before N and always sees N after the
  fence. `tests/replay.rs` already has the GPU program.
- Stress at `SUBMIT_DEPTH=1` and `64`, `QUEUE_BATCH=0`.
- Perf: median of at least 15 intervals with at least 1,000 draws/frame (journal method) vs baseline; stalls over 200 ms
  counted; differences under 5 % are noise (findings).

**C5. Descriptor slot lifetime (publish at upload completion, release at GPU completion).**
- Peak dynamic slots in use and dropped draws must be 0; remove `bury_lag` as a test case.
- 30+ minute soak plus a texture-heavy flythrough; no black/neutral textures (screenshots, `POP_WATCH`); on NVIDIA (262k pool)
  and AMD.
- Replay `--slots` and `--flow` before/after: heap slot to resource mapping identical.

**C6. Texture/upload budget model.**
- Reproduce the `E_OUTOFMEMORY` storm with the failing-allocator test from the journal (8 upload sizes) and a job-object memory
  limit at 16/24/32/64 GiB equivalents.
- Zero `E_OUTOFMEMORY`/aborts, peak commit not above prior, pending-upload count and median ms/frame not worse.

**C7. Delete dead/measurement switches and "as before" branches.**
- Resolved-config diff (C1); replay `--compare` picture identical on one capture; `cargo test -p victory-gpu --features
  diagnostics` and workspace tests (journal reference: 335 passed, 34 ignored). Delete by subsystem to allow bisect.

**C8. Translator changes (wave/helper/join).**
- `translate --validate --pipelines` on the VFX/particle/eye set (43/43 graphics, 96/111 compute; the 15 must not grow),
  `victory-shader` suite (281), console-dxc differential tests, replay `--retranslate` + `--compare --shaders-b` on all scenes,
  screenshots of shadow edges, cubemaps, transmission. Confirm the fingerprint changes.

**C9. Multi-queue / async.**
- D3D12 debug layer and GBV (`D3D12_DEBUG`, `D3D12_GBV`, `REPLAY_GBV`, `VICTORY_TEST_D3D_DEBUG=1`); replay `--wait-all` /
  `--wait-lists`. Previously measured not worth it on the RX 7900 XTX; require a measured NVIDIA win first.

### Validation tooling gaps (cheapest enablers)

- **V1. Resolved-config dump:** one startup log line with every switch's value and source. Without it runs are unattributable.
- **V2. Golden scene set:** stored `--images` outputs and buffer digests plus a diff script; include an ocean scene.
- **V3. Title-write notification oracle:** at each submission re-read pages claimed unchanged and flag missed writes (extend
  `REFRESH_CHECK` across all consumers).
- **V4. Title-memory digest per present:** hash fixed regions (water maps, entity transforms, culling lists) on a deterministic
  scripted input (`VICTORY_KEYS=<seconds>:<key>` exists); compare across builds. Catches landing-order changes pictures miss.
- **V5. Soak harness:** 60 minute scripted drive with fault-book and slot-check; summary of device-lost, dropped draws, pops,
  stalls over 200 ms.
- **V6. Process-level A/B:** launch configs A and B on the same save and compare frame-line stats and blips; extend `VICTORY_AB`
  to all switch idioms.

## 7. Suggested order

1. V1, C1, C7 (clearly dead switches): no behavioural risk, makes every later comparison attributable.
2. V2, V4, V5: the harness that makes C2-C5 safe.
3. C5 (slot lifetime), C6 (budget): where most journal bugs live, most contained.
4. C2/C3 (detectors) behind a new switch, old detectors kept as the diagnostic oracle.
5. C4 last: the relay stack is the throughput core and R1/R2 are hardest to test.

## Appendix A. Switch names by subsystem (auto-extracted)

Default class is a heuristic from the reading line (`is_none_or(v != "0")` read as on, `is_some_and` as off); `mode` = unclear.
Treat as a map, not truth; launcher overrides in section 0 apply. Known misreads: `WAVE_WIDE` listed on (see section 0),
`UPLOAD_AHEAD` and `KEEP_SHOWN` are off/launcher-driven.

**Shader translator (victory-shader)** (18)

- default off (needs `=1`/set): CONSOLE_DXC, DIFF_ONLY, EARLY_DEPTH, WARP
- value/number/path: RDNA_FLOAT, REF_PS, REF_VS, SHADER_LOOP_LIMIT, SHADER_OPT
- mode or unclear from the read site: DESC_WHY, DXIL, EMIT_DUMP, GROUP_SYNC, PROBE, SHADER_CENSUS, SHADER_SOURCE_HASH, SHADER_SWITCHES, WAVE_WHY

**Formats/audio/input/HLE (non-GPU)** (55)

- default on (unset = on, `=0` disables): CLZERO_ASK, MAP_LEAN, ONE_PAD, PATCH_CLZERO, PATCH_WAITS, SECTION_WINDOW, STREAM_NOTICE, STREAM_PRIORITY
- default off (needs `=1`/set): DSTORAGE_HALF, EFFICIENCY_MODE, LAZY_VIEWS, MAP_RUNS, MOUSE_WARP, PROCESS_PRIORITY, SOFT_VIEWS, TITLE_PRIORITIES
- value/number/path: AUDIO_CAPTURE, BCPACK_CASES, BCPACK_TABLES, CACHE_DIR, DATA_DIR, DSTORAGE_RETRIES, DSTORAGE_STAGING_MB, DSTORAGE_TRACE, FULLSCREEN_RES, KEYS, LOG_FILE, MAP_THREADS, MOUSE_CAPTURE, SKIP_BINK, WINDOW_MAX
- mode or unclear from the read site: AUDIO_CAPTURE_OF, CONFIG, DSTORAGE_IO, DSTORAGE_LOG, DUALSENSE, EXIT_CHILD, FULLSCREEN, GAME_DIR, HOST_LOCK_CHILD, IMAGE_WATCH, IME_ENGLISH, KEYS_FILE, LOG, PROFILE, RELAUNCHED_FROM, SAVE_CHILD, SAVE_OP, SAVE_STOP, STACKS, TEST_TITLES, TRAP_LOG, XMA_DUMP, XMA_REFERENCE, XMA_STREAMS

**Replay / capture / diagnostics** (37)

- default on (unset = on, `=0` disables): BOOK, MEMORY_MAP
- default off (needs `=1`/set): DUMP_TARGETS, FETCH_CHECK, LATE_WAITS, PANEL, POP_ALL, RECORD_ALWAYS, SLOT_CHECK, TIMER_LOG, UPCOPY_CHECK
- value/number/path: A, AB, B, CAPTURE_FRAMES, CAPTURE_WORLD_ALSO, FAULT_BOOK, FAULT_BOOK_AT, FRAME_INTERVAL_MS, FRAME_INTERVAL_US, KEEP_SHOWN, POP_WATCH, RECORD, RECORD_AT, RECORD_FRAMES, TIMELINE, TIMELINE_AT, TIMELINE_FRAMES, UPLOAD_AHEAD
- mode or unclear from the read site: BARRIER_TEST, CAPTURE, CAPTURE_WORLD, COPY_MODE, NONE_SUCH, POP_FOLDER, REPLAY_GBV, REPLAY_PEEK

**Streamline/DLSS/framegen/display** (22)

- default on (unset = on, `=0` disables): DLSS_JITTER, DLSS_MV, DLSS_REACTIVE, FRAMEGEN_HUDLESS, STREAMLINE_LOG
- default off (needs `=1`/set): AMBIENT_ONCE, CHECK_TEARDOWN, DLSS, FRAMEGEN, STREAMLINE_PRESENT, THIN
- value/number/path: AMBIENT, AMBIENT_AIR, AMBIENT_SHADE, AMBIENT_SPECULAR, DISPLAY_MODE, DISPLAY_REFRESH, FRAMEGEN_CYCLE, FRAMEGEN_X, STREAMLINE_DIR
- mode or unclear from the read site: DLSS_MODE, DLSS_PRESET

**Pipeline creation** (23)

- default on (unset = on, `=0` disables): ACTIVATE_RAYS, LIBRARY_HANDLE, LIBRARY_MAP, LIBRARY_TRIM, PIPELINE_LOWEST, PIPELINE_WAIT_SHARED, RT_RESOLVE
- default off (needs `=1`/set): BVH_BUILDER, BVH_QUARANTINE, EARLY_ALL, LIBRARY_LOCK, LIBRARY_STATIC_SAMPLERS, PIPELINE_BOOK, PIPELINE_SLOW, WATER_LIGHT
- value/number/path: LEAVE_OUT, PIPELINE_WAIT_MS, PIPELINE_WORKERS, SCREEN_WAIT_MS, SKIP_PIPELINES
- mode or unclear from the read site: PC_DXC, ROOT_SRV_DIRECT, SHADER_SAVE

**Indirect/draw/bind** (40)

- default on (unset = on, `=0` disables): BAKE_EXACT, BAKE_SOURCES, FULL_CARD_PASS, HEAP_RECHECK, HEAP_SPREAD, HEAP_TEXTURES, INDIRECT_WIDE, QUEUE_BATCH, READY_ONCE, RECHECK_SOON, REUPLOAD_WHEN_FULL, TEXTURE_CATCHUP, THREAD_DIMENSIONS, WRITE_SKIP
- default off (needs `=1`/set): AMBIENT_SPECULAR_ALPHA, BENCH_WARP, CARD_ACCOUNTING, INDIRECT_MERGE, INDIRECT_REST, INDIRECT_WINDOW
- value/number/path: BENCH_CS, BENCH_N, BENCH_S, BENCH_THREADS, BIND_CACHE, LIST_SLOTS, MESH_TEST, PICKED_MB, ROOT_UAV_MB, ROOT_VIEW_MB, TEXTURE_STAGE_MB, TEXTURE_STAGE_MS, UNWATCHED_FRAMES
- mode or unclear from the read site: BAKE_TEXTURES_MS, BLOCK_WAY, EARLY_TEXTURES_MS, INDIRECT_BATCH, NATIVE_1440P, PICKED_WRITES, SCISSOR_TEST

**Descriptors/heaps/views** (27)

- default on (unset = on, `=0` disables): HEAP_SYNC, HEAP_SYNC_ASIDE, KEEP_VIEWS, LENT_HEAP, LIST_VIEWS, QUEUE_HOST_TEXTURES, QUEUE_MARKS, QUICK_MIRROR, RAW_VIEWS, RAW_VIEW_CLAMP, SLOT_HOLDS, TABLED_TEXTURES, VIEW_BYTES, VIEW_EXTENT, VIEW_INLINE
- default off (needs `=1`/set): HEAP_SYNC_AT_SUBMIT, SHOW_AFTER, SHOW_NEW_LATER, SPANS_QUICK, SYNC_FINE, SYNC_IN_PLACE, TABLED_REREAD, VIEW_COPY
- value/number/path: DYNAMIC_SLOTS, HEAP_SYNC_EARLY, TEXTURE_AWAY_MS
- mode or unclear from the read site: STALE_TEXTURES

**Textures/residency/pools** (31)

- default on (unset = on, `=0` disables): ALIAS_COPY, DEPTH_TWINS, EXACT_NO_WAIT, HELD_LEVELS, LAZY_TEXTURES, MAPPED_BY_VIEWS, POOL_SMALL, TEXTURE_FILL, TEXTURE_GUARD, TEXTURE_HOT, TEXTURE_ONCE, TEXTURE_POOL, TEXTURE_SAME, TEXTURE_SWEEP, UPLOAD_LOW
- default off (needs `=1`/set): COMMITTED_QUERY, LEVELS_ON_DEMAND, MAPPED_BY_PAGE, PLACED_FILLER, PRELUDE_UPLOADS, STAGED_ASK_ALL, TEXTURE_COPY, TEXTURE_NO_WAIT, TILES_TEST_CARD
- value/number/path: CARD_ROOM_FOR_TARGETS_MB, CARD_ROOM_FOR_TARGETS_SETTLE_MS, LEVELS_MIN_KB, RIPE_BLOCKS, TEXTURE_BUDGET_MB, TEXTURE_ROOM_PERCENT
- mode or unclear from the read site: BENCH_FILE

**Write-watch/guards/landing/title writes** (31)

- default on (unset = on, `=0` disables): ASK_ONCE, GUARD_HOLES, GUARD_REST, KNOWN_HOLES, LAND, LAND_ANYTIME, LAND_BESIDE, LAND_TAKEN, LIGHT_REFRESH, OPEN_ONE_VIEW, PAGE_PARTS, SAFE_COPY, SAME_RECORDS, TAKE_PARTS, VIEW_GUARD
- default off (needs `=1`/set): COPY_BESIDE, FAULT_UNLOCKED, GUARD_ASK, LAND_SKIP, STORE_BESIDE
- value/number/path: ASK_EARLY, GUARD_HOT, INPLACE, INPLACE_MB, INPLACE_PROBE, INPLACE_PROBE_MB, LAND_THREADS, UNDER_THREADS, VIEW_THREADS
- mode or unclear from the read site: PAGE_WRITES, WRITE_SITES

**Host copies/card buffers (shadow)** (34)

- default on (unset = on, `=0` disables): CARD_BUFFERS, CARD_HASH_LEN, CARD_ROOM_FOR_BUFFERS, COPIES_IN_PLACE, EXACT_BACK, HEAP_PASS_ASIDE, LAND_ECHO, QUICK_FIND, SEND_OWN, UNDER_WIDE, UNDER_WORDS, WIDE_BACK
- default off (needs `=1`/set): CARD_ALL, CARD_SPANS, KEEP_SNAPSHOTS, PLACED_ORDER, REFRESH_CHECK, SEND_BAR, WC_BAR, WC_NO_BACK, WINDOWS_OWN, WINDOW_TAILS
- value/number/path: CARD_BUFFERS_MIN_KB, CARD_WINDOWS_MB, KEEP_SNAPSHOTS_MB, MEMORY_CARD_MB, SNAPSHOT_POOL, WC_NO_BACK_MIN_KB, WIDE_SPIN, WINDOWS_BAR_MB, WINDOW_KB
- mode or unclear from the read site: CARD_BUFFERS_MB, CARD_BUFFERS_MIN_MB, CARD_STEPS

**Queue/submission/relay/lists** (61)

- default on (unset = on, `=0` disables): BENCH_HOLES, CUT_WAITS, DIRECT_WRITES, FRESH_OPEN, INDIRECT_CARD_BUDGET, KEEP_FREED, LATE_WAIT_WRITES, LAZY_PRELUDE, LIST_POOL, QUEUE_WAIT, READ_BACK, REFRESH_AHEAD, RELAY, SCRATCH_POOL, SPLIT_WAITS, SWEEP, TEXTURE_KEEP, TRAIL, WAIT_FORCE, WAIT_RESIDENT, WIDE_FILLS, WIDE_TEXTURES
- default off (needs `=1`/set): ASYNC_SUBMIT, BVH_SIZES_WAIT, BVH_TRACE, CARD_REHASH_RUNS, COPY_ASK, CUT_WRITES, D3D12_BACKGROUND, D3D12_DEBUG, D3D12_DRED, D3D12_GBV, D3D12_WARP, FILL_TITLE, GPU_TIMING, HEAP_SYNC_FIRST, JOIN_COPIES, REFRESH_FRONT, RELAY_COPY, SEND_CHANGED, WAIT_ASK, WALLS_TRACE, WIDE_MERGE
- value/number/path: BURY_LAG, DECLARE_ONCE, FAST_CORES, GPU_LOAD_MS, HELPER_PRIORITY, JOIN_LISTS, JOIN_PARTS, LATE_WAIT_LISTS, SUBMIT_DEPTH, SUBMIT_FRAMES, TEST_ROUNDS, THIN_THREADS
- mode or unclear from the read site: ASYNC_QUEUE, CARD_AWAKE, COPY_OVER_TITLE, LATE_WAIT, MEMORY_DUMP, WIDE_REFRESH

**Other** (16)

- default on (unset = on, `=0` disables): BVH_BOUNDS, BVH_FENCES, FIRST_HALF, HELPER_OWN, WAVE_DEEP_SKIPS, WAVE_LOCAL, WAVE_OWN_BITS, WAVE_WIDE
- default off (needs `=1`/set): F16_NEAREST, FILTER_3D, FIRSTLANE_OWN, PORT_CONSTANTS
- mode or unclear from the read site: MESH_OUTPUT_STRICT, STRICT_JOINS, TEXTURE_BACK, TRANSLATOR