# 03 - Descriptor and resource architecture

Scope: `crates/victory-gpu` descriptor.rs, descriptor/host.rs, heap_marks.rs, the descriptor
parts of draw.rs, watch.rs. root.rs, signature.rs, sampler.rs, external.rs and port.rs are
not hot (small, set up once or per pipeline). Read-only study, no source edited.

Evidence labels: **[S]** static (read from code), **[M]** measured (log line), **[H]** hypothesis.
Main log: `%LOCALAPPDATA%\victory\logs\play-20261127-095317-11396.log` (steady state 09:56:34-09:58,
72-80 ms/frame, 7,100 draws; loading phase 09:54:10-09:56:40, 250-380 ms/frame).

## 1. How the machinery works (S)

- The console's descriptors are 32-byte records in plain title memory (descriptor.rs:1-8). The title
  creates them with API calls (`create_srv` etc., descriptor.rs:1557) and also builds a table per draw
  by **copying records as bytes** into its own shader-visible heap, with no API call (descriptor.rs:418-427).
- Our record is only `{VIEW_TAG, i, generation}` (12 bytes, descriptor.rs:1466-1475), so bytes survive being
  copied. `i`/`generation` name an interned `View` in the global `VIEWS` RwLock table (1079-1103), or inline
  the constant/placed address (1105-1192).
- Translated shaders index a **host heap that mirrors the title heap slot for slot** (`HostHeap`,
  host.rs:126). Every title slot must therefore have a host descriptor before a shader can pick it.
  Three routes make one:
  1. API-call route: `record()` -> `mirror(handle)` (descriptor.rs:193) -> `HostHeap::mirror` (host.rs:3877).
  2. Draw route: `State::tables` (draw.rs:4423) / `table_entry` (4390). A table with a CBV or indexed
     entries gets a **fresh run of dynamic slots per draw**, each `make_at`/`make_listed` (host.rs:3448/3464).
  3. Byte-copy route: `sync_heaps` (descriptor.rs:428-636) asks the kernel which 4 KiB pages of the
     heap changed (`Tracked::take`, watch.rs:781 -> `GetWriteWatch`), reads them, compares each 32-byte
     record with the one last seen (`Copied::changes*`, host.rs:5279+), and mirrors the new ones.
     It runs on thread `victory-heap-sync` (descriptor.rs:265-327), a round per submission plus an
     "early" round every 4 ms.
- Slots a shader picks **by a computed index** (bindless) are served by two passes that run on a
  recording thread once per frame at the first list that needs them (draw.rs:2378 `heap_buffers`):
  `heap_buffer_pass` (draw.rs:1164) makes host slots for all buffer views and refreshes the host copy
  behind each one; `heap_textures` (draw.rs:3033) uploads textures whose slots wait in `TextureQueue`
  (host.rs:1179, a `BTreeMap` behind one `Mutex`).

## 2. Measured numbers (M, steady state, per frame)

| Item | Value | Source |
|---|---|---|
| Heap pages with changes / slots looked at / slots mirrored | 264-285 / 271-297k / 8.0-9.9k | "descriptor records..." line |
| Of the changed records: new table records left to draws / waited for a draw / waited, no draw came | 27-32k / 8.9-11.9k / 2.1-2.2k | same |
| Bytes read per frame | 16.5-17.8 MB (about 63 KB per changed range, i.e. ring pages are rewritten almost whole) | same |
| `sync_heaps` wall | 25-29 ms (asking 6.3-9.9, reading+comparing 2.9-4.7, making 4.7-8.2) | same |
| Submitting thread waiting for those rounds | 0.25-1.4 ms, plus 0.6-1.0 "heaps' records and what is swept" | "preparing per frame", heap_marks lines |
| Raw twins of buffer views made | 17.3k (590 of them off 16 bytes and null) | "raw twins" line |
| View-slot writes (note_buffer path) | 14.3-17.0k | "writes of view slots" line |
| Texture-view slot writes | about 640 placed + 1,030 object per frame | "texture slots written so far" deltas |
| Heap-picked buffer set | 139k views, 4,519 host copies, 2,448 MB | "made ready for shaders that pick them" line |
| Refreshes on the submitting thread | 16-18 ms in 37-41 flushes of about 6,400 copies, about 5,350 known to copy nothing | "preparing per frame" line |
| Texture queue lock | 32-37k takes / 5 s, 664 ms waited / 5 s (558 by non-pass threads), 857k takes avoided by marks | "texture queue's lock" line |
| Views table lock | about 410 ms waited / 5 s by non-pass threads | "its rest" line |
| Texture queue | 10.8k slots, 9.6-9.8k "uploaded whole, waiting to be looked at again" | "textures picked by index, the queue" |

Loading phase (09:54:10-09:56:40, about 150 s at 3-5 fps, 250-380 ms/frame):
- Queue 23-32k slots waiting, "37,715 held back for room" in one 5 s window; texture line 8.2 GB against
  a 10.3 GB budget with 9.6 GB in use. 700 -> 60k textures uploaded (3.1 GB) over 2.5 minutes.
- "new card memory waited for until it is mapped" 18,200 times, 7.15 s; the waits were 0.1 ms each at the
  start and 2-4 ms each at 09:55:34-09:56:06.
- `list::thin` 55k calls cost 961 ms (all threads) against 300-325 ms in steady state; the title waited
  229 ms/frame at presents while our submitting thread worked 65 ms; the card was 20% busy. The frame is
  limited by recording/translation threads, not by the card or the preparing thread.
- Lock waits of the two descriptor locks in loading: queue 664 ms + views 396 ms + pass 145 ms per 5 s
  (about 75 ms/frame of thread time at 3.2 fps). The texture pass itself: 777 ms per 5 s, 413 ms in
  the frame's passes, 40 stopped at their time (draw.rs:3173-3186: 250 ms, tripled to 750 ms by catch-up).
- The title writes about 535k placed and 193k object slots in the first 80 s (frame lines).

## 3. Mechanism by mechanism: why, requirement or artifact, why per frame, can it go

### 3.1 Mirror heap with identical slot order
- Why: translated shaders index the heap by the title's descriptor positions (host.rs:1-6). [S]
- Requirement for bindless picks (ResourceDescriptorHeap[i] on D3D12 needs a real descriptor at i).
  Artifact for table reads: those are bound by draw, and we already create fresh dynamic copies per draw.
- Per frame: the title rewrites its tables every frame (ring), and the records name new addresses.
- Can it go: not the mirror for bindless picks. The mirror for table-read slots can be skipped
  (it already largely is: "left to the draws").

### 3.2 `sync_heaps` (write watch, read, compare, mirror)
- Why: no API call sees the title's byte copies (descriptor.rs:418). Requirement: some route to
  find those writes. The write watch is the only non-invasive one. [S]
- Artifact: it scans 8.5 MB (271k slots) a frame to find 8-10k records, 3% hit rate. The pages
  it reads are the title's draw-table ring: 27-32k new records per frame are "of draws' tables, left to
  the draws", meaning the scan finds, compares and then discards them. [M]
- Per frame: rounds are asked per submission (17 a frame) and every 4 ms; asking costs 6-10 ms
  of sync-thread time per frame (about 0.4-0.5 ms a round, `Tracked::take` -> `GetWriteWatch` over the
  heap range plus a `BTreeMap` of last seen writes under a mutex, watch.rs:786-789). [M for total, H for per-round]
- Mostly off the critical path: the submitting thread loses 0.3-1.4 ms waiting plus about 1 ms. It does
  use the shared `land::each_view` workers and 25-29 ms of wall on fast cores that 8 translation
  threads and the title also want. [H: 2-5 ms/frame of contention]
- Can it be anticipated/persisted/eliminated: see opportunity C.

### 3.3 Fresh dynamic tables per draw (`State::tables`, draw.rs:4423)
- Why: CBVs must be 256-aligned on the host and constant bytes are copied to scratch per draw
  (draw.rs:4411-4416, 4641-4649); indexed ("descriptor array") tables need all n entries. Requirement of
  the alignment mismatch, artifact in that it is done by recreating descriptors.
- Per frame: constants live at new addresses each frame (the comment at descriptor.rs:1067 puts it at
  14,000 CBV writes a frame), so a per-list key hashes the n record words (4495-4503) and a per-recording
  `ListViews` (host.rs:3464) caches descriptors only until the recording changes (`views.made.clear()`).
- Costs: 17.3k twins and the views they twin are created per frame (`twin_window` counter, host.rs:4454;
  one buffer view = typed/structured view + raw twin = two `Create*View`). 222 lists a frame means
  about 78 twinned buffer views per list, many identical across lists. [M count, H cost]
- Can it go: the per-recording cache can be made global (opportunity B).

### 3.4 Interned view table (`VIEWS`)
- Why: records are 12 bytes and must survive byte copies; the table gives them identity and a
  generation so released resources go stale (descriptor.rs:1056-1078). Requirement.
- Artifact: `BTreeMap<View, u32>` keyed by a `View` that can be 27 u32 (sampler) or 30 u32 (placed);
  a write lock for every new view and for every `forget(resource)` (descriptor.rs:1478-1502); every
  `view_at` clones the `View` under the read lock, including in the `mirror` quick path (host.rs:3900-3906),
  which is meant to be the cheap one. Measured 410 ms/5 s waiting by non-pass threads (about 6 ms/frame
  of thread time). [S + M]
- Can it go: reads can be lock-free (opportunity E).

### 3.5 Texture queue (`TextureQueue`, host.rs:1179)
- Why: textures the title fills itself (placed memory) have no host texture until something reads them.
  Table reads upload them at draw time; bindless picks need a pass over every such slot because nothing
  says which one a shader will pick. Requirement given the bindless model, artifact in the lock design. [S]
- Per frame: 640 placed + 1,030 object slot writes a frame; each takes the queue's mutex unless the mark
  says nothing changes (QueueMarks, host.rs:1082): 32-37k takes and 857k avoided per 5 s. [M]
- The queue is single mutex, `BTreeMap<u32, Waiting>`, plus `held` map; the due scan walks it under lock
  (`due_and_census`, host.rs:1252: held 57-84 ms / 5 s). Writers wait 558-1,260 ms / 5 s. [M]
- Can it go: structurally yes (opportunity D) and the volume can be cut by demand (opportunity A).

### 3.6 Bindless buffer pass and refresh coverage (`heap_buffer_pass`, draw.rs:1164)
- Why: a shader may pick any buffer view of the heap, so every host copy a buffer view shows must be
  fresh when the list runs, "nothing tells us which of the heap's a computed index names" (draw.rs:2370).
  Requirement under the current model, but it is a **conservative over-approximation**: 139k views
  over 4,519 host copies, 2.4 GB, are declared read by every frame.
- Per frame: it is the set that is refreshed, per frame, because the title writes buffers every frame.
- Cost: part of the submitting thread's "refreshes 16-18 ms in 37-41 flushes of about 6,400 copies, 5,350
  known to copy nothing". The heap set is about 4,500 of those 6,400 copies. [M for counts; H that most of the 16 ms is
  attributable to them, no measurement separates it. Experiment in section 6.]
- Can it go: opportunity A (demand).

### 3.7 Raw twins (`RAW_TWINS`, host.rs:1864, `twin_window` 4454)
- Why: shaders that load a typed/structured buffer with raw loads read a byte-addressed alias at slot
  `raw + index` (victory-shader `emit::raw_views`). Requirement of the shader translation.
- Per frame: created along with each buffer view made for a draw table or the buffer pass (17.3k a frame).
  Persistable: the view is a pure function of (resource/shadow identity, range, format, stride).

### 3.8 `WAITING`/`make_waiting`, `show_after`/`show_new_later`/`kept views`
- Why: a texture view written before its texture has a host copy is left null until first use
  (descriptor.rs:1378-1464); an upload still in flight must not be visible (device loss every 16 min before
  SHOW_AFTER, findings.md). Requirement. Counters: 40,000 slots shown after upload, 39,123 on the
  neutral texture meanwhile, 64.8k "kept what they showed". [M]
- Per frame, small. Not a priority except that all of these go through `FLIPS`/`NEW_PENDING` mutexes
  (host.rs:3983-4005); fine while the volume stays at hundreds a frame.

### 3.9 `Made`/`kept`/`quick` per-slot arrays and the 512 locks
- Why: slots are made by several threads; a key per slot plus lock stripes (host.rs:93-122). Already
  tuned (profile thin-2 in the comment). `kept` is a `Mutex<Option<ID3D12Resource>>` per slot, so a host
  COM pointer is `AddRef`'d and replaced under a mutex for every make (host.rs:3564-3583). Minor.

## 4. Ranked opportunities

Estimates are for the submitting/critical path in ms/frame unless stated; thread-time is the sum
over all threads. All estimates are [H]; counts above are [M].

### A. Demand feedback for slots picked by index (GPU-written "picked" bitmap)
- Replaces: the "everything the heap names" assumption of `heap_buffer_pass` (draw.rs:1164) and
  the all-or-nothing upload queue of `heap_textures` (draw.rs:3033).
- Design: translated shaders already know when they pick a heap slot by a computed index (the
  `SLOT_STRIDES` table and raw twins prove the emitter distinguishes them). Add, at that site only, an
  `InterlockedOr` into a per-slot bit table in a UAV buffer (1 bit per slot: 8 KB per 64k slots;
  clear it per frame). Read it back with two frames of latency through a readback buffer (the relay
  thread that lands results already exists). Then:
  - buffers: the heap pass refreshes only host copies whose slots were picked in the last N
    frames, plus slots written since (fallback: refresh stays as today for slots of unknown state, one
    of every 16 passes already exists: `HEAP_ROUND`, draw.rs:702);
  - textures: queue priority by "picked last frame", upload only those; the rest stay neutral (the
    neutral stand-in and `show_after` already exist, host.rs:411-527, 337-357).
- Estimates: steady state 4-10 ms off the submitting thread if the heap coverage explains most of its
  refreshes (see experiment E1); the loading phase (2.5 min at 250-380 ms/frame, 60k textures,
  3.1 GB) probably shrinks several-fold if the number of textures actually sampled is a small share
  of the queue. Neither is measured.
- Risk: medium-high. Pop-in of 1-3 frames; shaders that read through waterfalls or descriptors written
  and read in the same frame; the ocean height/displacement read-back (texback) must stay on the
  non-demand path. Mitigation: A/B switch and keep the full pass available (`VICTORY_HEAP_DEMAND=0`).
- Files: victory-shader `emit` (instrumentation), `descriptor/host.rs` (bitmap + consumer),
  `draw.rs` `heap_buffer_pass`, `heap_textures`, `TextureQueue::due*`.

### B. A persistent descriptor cache across lists and frames
- Replaces: `ListViews` rebuilt per recording (`views.made.clear()`, host.rs:3464-3478) and the `Create*View`
  work per twinned buffer view.
- Design: a global CPU-only staging descriptor heap, keyed by the interned record `(i, generation)` plus
  the host copy identity (`Shadow::id`/gpu address, since the host copy can be replaced), holding the
  typed view and the raw twin in adjacent slots. A draw or the buffer pass obtains slots with
  `CopyDescriptorsSimple` (already used: `copy_from`, host.rs:3513) instead of `CreateShaderResourceView`.
  Invalidate on `forget(resource)` (descriptor.rs:1478, already bumps the generation, so a stale key
  cannot hit) and when the shadow's GPU address changes. Constant buffer views stay per draw
  (`View::Constant` is rejected by `make_listed`, host.rs:3477).
- Estimate: 17.3k twinned views = about 34k `Create*View` a frame. At 0.5-1 us each that is 17-34 ms of
  thread time, spread over the translation threads (8 on this machine), so 2-4 ms of wall. [H, not
  measured: the code does not time view creation. Add a counter first.]
- Risk: low-medium. Lifetime of the host resource (descriptors for released host copies must not be
  copied: `keep` and `retire_resource` already govern that).
- Files: `descriptor/host.rs` (`make_listed`, `make_at`, `twin_window`, `make_twinned`, `ListViews`).

### C. Stop scanning the title's table ring; gate rounds on need
- Replaces: `sync_heaps` reading/comparing pages that are tables (descriptor.rs:486-592) and the 4 ms/
  per-submission rounds (descriptor.rs:241-416).
- Design in three small steps:
  1. A page whose changed slots are, for several consecutive frames, >90% `copied_tabled` or "of draws'
     tables" is a **ring page**. Skip its read/compare/mirror (keep the `Tracked` bookkeeping). The draws
     read those records directly (`view(handle)` at draw.rs:4525, 4640), so nothing is lost for them.
     Slots in a ring page that a bindless shader picks are exactly what opportunity A's bitmap detects;
     until then keep a low-rate sweep of ring pages (1 of 16 rounds, like `HEAP_ROUND`).
  2. A list sets a flag when it records a pick-by-index draw (`heap_buffers`/`heap_strides`/`lent_heap`).
     `begin_sync_heaps` for a submission is asked only when one of its lists has it set. Dirty ranges accumulate in
     `Tracked`, so one round in a frame (the first submission with picks) is enough.
  3. Coalesce the kernel ask: one `GetWriteWatch` per round across both heaps, not per heap; keep
     `seen` as a flat per-page `Vec<u64>` instead of a `BTreeMap` behind a mutex (watch.rs:789).
- Estimate: sync-thread time 25-29 ms -> about 8-12 ms (asking is about 7.7 ms of it and falls with rounds;
  reading/making fall with the ring skipped); critical path 1-2.5 ms (waiting 0.3-1.4 + 0.6-1.0), plus
  relief on the shared worker pool and fast cores (H: 2-4 ms/frame wall). This is the cheapest of the ranked items.
- Risk: low for step 2, medium for step 1 (a bindless-picked record on a ring page would go stale; the
  sweep and the bitmap cover it, and a diagnostic can count "mirrored from a ring page later read by a draw").
- Files: `descriptor.rs` (`sync_heaps`, `begin_sync_heaps*`, `round_after_now`), `heap_marks.rs`,
  `descriptor/host.rs` (`copied_tabled`, `Copied`), `watch.rs` (`Tracked::take`), `draw.rs` (flag).

### D. Lock-free texture queue
- Replaces: `Mutex<TextureQueue>` + `BTreeMap` (host.rs:1179, 2206-2232) and its 7 lock sites
  (`queue_site`, descriptor.rs:42-60).
- Design: per slot an atomic word `{state, due_frame, write_seq}` in a `Vec<AtomicU64>`. Writers do a
  single CAS and push the slot into the existing lock-free `Slots` bitset (host.rs:29-83: dirty set).
  The pass (one consumer thread at a time, as `IN_TEXTURE_PASS` already assumes) takes the dirty bits, orders
  by `due`/`write_seq` and does everything else on its own non-shared structures (`held`, census).
  `QueueMarks` (host.rs:1082) is the first half of this idea; make it the whole thing.
- Estimate: 558 ms/5 s steady (about 8.5 ms of thread time per frame) and 664+145 ms/5 s in loading
  (about 50 ms of thread time per frame at 3.2 fps) of waits disappear. Wall effect 1-2 ms/frame
  steady; in loading it removes a share of the 3x inflation of `thin` (961 ms vs 300 ms), which
  the other loading causes (memory, staging) also drive. [M for waits, H for wall]
- Risk: medium (ordering semantics in `TextureQueue::written/later/hold/recheck`: unit tests exist, host.rs:6158+).
- Files: `descriptor/host.rs` (`TextureQueue`, `QueueMarks`, `queue_texture`, `tabled_in*`), `draw.rs` `heap_textures`.

### E. Lock-free `VIEWS`
- Replaces: `RwLock<Views>` with two `BTreeMap`s (descriptor.rs:1095), `view_at` clone per lookup.
- Design: entries in an append-only chunked arena (stable addresses), `generation` an `AtomicU32` per entry,
  readers take no lock: `view_at(i, g)` checks the atomic generation, reads the immutable `View`, and
  returns `&View` or `Arc`. Writers (new view, forget) use one writer mutex and a sharded hash index
  (e.g. 64 shards by hash of the `View`) in place of the global `BTreeMap` index.
- Estimate: 6 ms/frame of thread time in waits (410 ms / 5 s, steady); more in loading (396 ms / 5 s).
  Wall 0.5-1.5 ms. `forget()` stops blocking every reader when textures are released in bulk.
- Risk: low-medium. Files: `descriptor.rs` (`Views`, `view_at`, `words_for`, `forget`, `views_read/write`).

### F. Stop the per-slot COM traffic in `keep`
- `kept: Box<[Mutex<Option<ID3D12Resource>>]>` (host.rs:175) clones and swaps a COM pointer under a mutex
  per make. With B in place a draw's dynamic slots can take `Arc`-less "borrowed by the cache" references
  retired in bulk at list reset (`crate::host::retire_resource`). Small (about 1 ms of thread time). Skip unless B lands.

### G. Long-term: make the shader read the title's records and resolve through a persistent table
- Idea: have the shader read the 12-byte `{tag, i, generation}` record from a GPU-visible alias of the
  title heap (e.g. `ID3D12Device3::OpenExistingHeapFromAddress` over the title's heap memory; **unverified**
  whether the title's `MEM_WRITE_WATCH` placement allows it) and index a persistent global descriptor
  table by `i`. This removes the host mirror, the byte-copy scan (3.2) and the fresh per-draw tables
  for non-CBV tables. Cost: placed texture/buffer and constant views carry addresses, not entries
  (descriptor.rs:1067-1078: 2.45M entries in 4 minutes if interned), so they need either an
  address-keyed resolver on the GPU (raw loads, no descriptor) or a persistent LRU. A large rewrite of
  `victory-shader::emit`. High risk, one to two months. Do not start before A, B, C, D, E are measured.

## 5. What the data says about "late loading" (about 150 s in this run; user reports about 5 min)

- Loading frames are CPU-bound on recording/translation threads (card 20% busy, title waits 229 ms/frame at
  presents, `thin` 961 ms vs 300). The descriptor locks account for about 8% of thin's thread time. [M]
- The structural cause visible in my area: after world entry the title writes a large part of its texture views
  at once (535k placed slot writes in 80 s), and the pass uploads each as a host texture (3.1 GB, 60k) under a
  per-frame time/byte budget (draw.rs:3173-3186) that runs on the recording threads and holds back slots when
  the 8.2 GB line is full (37.7k held in one window). Most of those uploads are for textures no draw samples
  yet. [H: needs the picked bitmap of A to prove]
- Memory waits: 18.2k "new card memory waited for until it is mapped" = 7.15 s, concentrated when the line was full. [M]
- First experiments (no new code): `VICTORY_TEXTURE_STAGE_MB`, `VICTORY_TEXTURE_CATCHUP=0`,
  `VICTORY_HEAP_TEXTURES=0` (draw.rs:3041) to see the frame time and visual cost without the pass.

## 6. Measurements to add before building (cheap)

- E1: gate `heap_buffer_pass` coverage (a switch that uses only slots written in the last N frames plus the
  sweep) and read "refreshes" in the preparing line: tells how much of 16-18 ms is bindless coverage.
- E2: time and count `make_twinned`/`CreateShaderResourceView` calls on the translation threads (a per-thread
  counter flushed in batches, as `not_taken` does); gives B's real value.
- E3: count how many of the 4.5k heap copies and 10.8k queue slots a shader actually picks per frame (A's
  bitmap, even diagnostic-only, reading back every 30 frames).
- E4: split the "asking" 6-10 ms of `sync_heaps` into kernel call and journal work (`Tracked::take`, `writes`).
- E5: a one-line counter of ring pages (changed pages where tabled share >90%).

## 7. Priorities in one table

| Rank | Opportunity | Est. critical-path ms/frame | Thread-time relief | Risk | Effort |
|---|---|---|---|---|---|
| 1 | C. Gate and coalesce heap rounds, skip ring pages | 1-2.5 | 12-17 ms | low-med | days |
| 2 | A. Demand bitmap for picked slots | 4-10 (steady, unproven), loading time several-fold | large | med-high | 2-3 weeks |
| 3 | B. Persistent descriptor cache | 2-4 | 17-34 ms | low-med | 1 week |
| 4 | D. Lock-free texture queue | 1-2 (more in loading) | 8-50 ms | med | 1 week |
| 5 | E. Lock-free views table | 0.5-1.5 | 6 ms | low-med | 3-4 days |
| 6 | F. Drop COM traffic in `keep` | under 1 | 1 ms | low | days, after B |
| 7 | G. Shader reads records | unknown | very large | high | months |

Caveats: sum of the above is not additive; the heap-sync thread is already beside the preparing thread, so the
direct gain from C is small and the main gain from C, B, D, E is shared-core relief that helps translation
threads (`thin` 300 ms over 8 threads) and the title's own threads. The preparing thread's 70 ms is mostly
buffer refreshes and steps (outside this report) except where A and its 4,500 copies matter.
