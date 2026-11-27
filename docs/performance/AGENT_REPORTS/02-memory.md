# 02 - Memory architecture (shadow, watch, land, uploads, residency)

Scope: `crates/victory-gpu` shadow.rs, watch.rs, land.rs, titlewrite.rs, inplace.rs, copystat.rs,
memory.rs, residency.rs, safecopy.rs, upload.rs, texback.rs, tiles.rs, popwatch.rs, pool.rs,
plus `victory-hle/src/modules/xmem.rs`. Read-only investigation; no source was edited.

Evidence labels: **[S]** static reading of code, **[M]** measured in a log, **[H]** hypothesis that needs an experiment.

Logs used: `%LOCALAPPDATA%\victory\logs\play-20261127-094655-17644.log` (RTX 4070 12 GB, series-s profile, DLSS + frame gen 2x,
title started 09:47:00, world entered about 09:49:05) and `play-...093851-14116.log`. Line numbers below are in the 094655 log
unless stated. `runs\*\victory.log` were not needed.

## 0. Summary of where the memory machinery costs

Steady gameplay, 09:51-09:53 [M] (lines 21940-22010, 25978):

| Quantity per frame | Value |
|---|---|
| Frame | 48-51 ms (20 fps); `fps` line: 33.7 preparing (copies 10.6 ms / 30 MB, textures 0.6 ms), 6.8 GPU, 11.4 "copying back" |
| Submitting ("preparing") thread | 46-48 ms, of which: 9.6 title bytes put under flights' shader writes, 15.4 list steps (refreshes 9.8), 7.2 copies in card memory, 6.1 planning card steps, 4.1 asking the write watch, 1.1 sending lists |
| Landing (what shaders wrote -> title memory) | relay thread 17.3 ms busy, 27-30 ms waiting for the card; 280 pieces of 64 KiB fetched (about 18 MB), of which 872 of 2482 pages have a new byte (3.4 MB) and only **0.77 MB** actually go in as dwords |
| Card-side coherence passes | 150 hash passes over 260 pieces + 360 compare passes; 5.5-5.8 thousand "fills" for card steps |
| CPU -> card | 18 MB / 760 runs sent as scatters (2.7 ms to extract); 2.2 MB / 61,000 changed runs put under flights' writes |
| Descriptor-heap records | 200-240 changed 4 KiB heap pages, 210-250 k slots looked at, 8-9.5 k mirrored, 17 ms of thread time |
| Texture refreshes | 5-8 ms of thread time (read, hash, detile) |

The card is not the wall in steady state (6.8 ms of GPU in the `fps` line, though the "card's clock" line says 86 % busy, which
includes our own hash/compare/fill passes). The wall is CPU-side coherence work: **about 42 of 48 ms is "noticing and moving"**
(`docs/findings.md`: "the one idea behind most of it"). The fix is not a faster scan; it is to stop scanning, comparing and
landing data that is provably (or learned to be) in one direction only.

## 1. Mechanism inventory: why it exists, requirement vs artifact

### 1.1 Host copy ("shadow") of every title buffer a draw uses - `shadow.rs:28` `Shadow`
- Why: title buffers are CPU memory the host GPU cannot address. A GPU-visible twin is mandatory (the card cannot read an arbitrary
  `VirtualAlloc` address) unless the memory itself is opened as a heap (`inplace.rs`). **Real requirement: a card-visible
  representation.** Copying is the implementation choice.
- Why per frame: nobody tells us when the CPU wrote, so each submission asks (`defer_refresh` at `host.rs:7250`, `Shadow::update`
  `shadow.rs:3926`, `copy_stale` `:4083`).
- Large buffers (>= 64 KiB, `VICTORY_CARD_BUFFERS_MIN_KB=64`) have two sides: PC-memory staging (`Card::staging`, `shadow.rs:112`)
  and the card-memory buffer shaders use (`shadow.rs:170` `cards()`). Every CPU change is copied twice (title -> staging ->
  card, `card_before` `host.rs:4411`), and every GPU change twice (card -> fetch -> title, plus staging merge).
- The big one: the 174 MB heap (`start=0x1a1f0000 len=182452224`, line 278), 14 refreshes a frame of ~4 MB each (comment
  `shadow.rs:4150`), declared as written "from dozens of offsets" (`shadow.rs:3774` comment).
- [M] 3,935 card copies, 1,937 MB (line 25977). Pool has `texture room: 8.1 GB in use`.

### 1.2 Write watch + guards (CPU writes noticed) - `watch.rs`
- Why: need to know which pages the title wrote since the last copy. Private memory uses kernel dirty bits (`GetWriteWatch`
  `watch.rs:262,275`); view memory (the page pool, SEC_RESERVE sections) cannot be write-watched, so a vectored handler
  (`watch.rs:3058` `on_write`) makes blocks read-only and notes the first write.
- Real requirement: yes, for any buffer the title writes after first use. Implementation artifacts: (a) protection changes at
  ~5 us each, 590 write faults/frame measured earlier (`titlewrite.rs` header, `watch.rs:1631`: 321-1,017 blocks re-protected
  and 323-331 faults per frame); (b) 64 KiB block granularity for views; (c) one syscall-level ask per refresh range
  (4.1 ms/frame [M], "0.6 finding what to ask").
- Per-frame because reset is part of the ask (asking also clears). Could be anticipated: **yes** for memory the title never
  writes after init (never faults/never dirty), which the code only handles implicitly (`ranged`/`nothing_new` fast path,
  `shadow.rs:3959`).
- `VICTORY_STORE_BESIDE` / `VICTORY_COPY_BESIDE` (titlewrite.rs) are off by default [M: no log line, play.rs does not set them].

### 1.3 Landing: what shaders wrote -> title memory - `land.rs`, `shadow.rs` `card_merge`, `host.rs:5013 relay_land`, `:5158 card_back`
- Why: on the console the CPU sees GPU writes for free. Here it must be copied back, in order, before the title is told
  anything (land.rs header: "a landing is done, whole, before the title is told anything").
- **Real requirement only for ranges the title's CPU reads.** Known cases: ocean height/displacement textures (`texback.rs`),
  the culling/occlusion data the title reads to decide "camera in a room" (`host.rs:3984` walls trace), BVH/structure builder
  results (`memory.rs`). Everything else is an artifact of "declare the whole UAV view".
- Declaration is the whole bound range: `readable()` -> `shadow.gpu_writes(list, address, len)` (`draw.rs:5589`), root UAV
  "cover all that is left of the title's buffer" (`draw.rs:2167` shortcut comment). [M] 109,228 MB/frame "declared written" in
  WC memory left on the card (line 22003 area): the declared extent is ~600x the actual heap, because the same 174 MB heap is
  declared dozens of times per frame.
- WC shortcut exists: memory the title mapped write-combined is not landed (`shadow.rs:3237`, `VICTORY_WC_NO_BACK`, min 1 MB).
  64 regions found in the log (lines 9168..). Non-WC (cached) memory, notably the 174 MB heap, is all landed.
- [M] fetched ~18 MB vs 0.77 MB dword-changed per frame: 96 % of the fetch/hash/compare traffic is false positive at
  piece granularity. 17.3 ms thread time landing (relay) and 7-11 ms visible as "copying back".
- Could be persisted: yes. `picked.rs` already learns "a buffer written so once is written so again" within a session
  (`picked.rs` header), proving the premise, but only for the picked-view case and not persisted.

### 1.4 Title bytes put under flights' shader writes - `shadow.rs:3729 merge_under_writes`, `host.rs:2488 late()`, `:2529 late_exact()`
- Why: submissions are sent ahead without waiting (`VICTORY_LATE_WAIT=2`). A range a flight may write is "pending"; if the
  title meanwhile wrote bytes into the same 64 KiB the refresh must merge only the changed dwords instead of overwriting
  what shaders wrote.
- [M] 9.6 ms/frame of the submitting thread: 186 pages looked at, 61,334 changed runs, 2.24 MB (lines 21990-22000).
  36 bytes per run: small CPU stores (constants, counters, free-list links) in blocks that are only "pending" because the
  declared extent covers them.
- Real requirement: only where the same bytes can be written by both sides in flight. Artifact: false sharing from the
  over-wide declaration (see 1.3). This is the single most expensive item on the submitting thread.

### 1.5 Card-side hash/compare passes - `host.rs:4411 card_before`, `shadow.rs:4459+ card_span / card_hashed`
- Why: with the card holding the copy, shader writes are found by hashing declared 64 KiB pieces before/after and fetching only
  differing pieces (`Card::before`, `shadow.rs:112+`). [M] 150 hash passes over 260 pieces, 360 compare passes per frame
  (line 21946).
- Real requirement: finding shader writes. Artifact: the number of dispatches (per range, per submission) rather than one
  pass per buffer per frame; and the pieces compared that are never going to be landed (WC) are skipped already.

### 1.6 Descriptor heap mirror (records the title copies as bytes) - `descriptor/host.rs`, `heap_marks.rs`
- Why: the console GPU reads descriptors from memory; the host needs real views. The runtime notices heap bytes the title wrote
  and translates them.
- [M] 17 ms/frame thread time, 210-250 k slots looked at for 8-9.5 k new records, 124 of 200 changed pages are guarded views
  (line 22000 area). 80 % of slots looked at are unchanged or empty.
- Not my main area, but it is the same pattern: **scan memory to find the 4 % that changed**.

### 1.7 Texture refresh (CPU-written texture memory -> detile -> host texture) - `texture.rs:1410 refresh`, `:1621 ...`
- Why: textures are written by CPU/DirectStorage into tiled memory; the host copy is detiled linear. 5-8 ms/frame thread time [M].
- Already has learned state: `once` flag (`texture.rs:1448`), `last.asked`, `exact`; 30-frame partial re-read (`:1560`).
- Memory of textures in view sections is guarded; hash whole texture otherwise (`texture.rs:1570-1580`, `digest(tiled)`):
  an artifact for textures that are never rewritten.

### 1.8 Uploads/staging/pool - `upload.rs`, `pool.rs`, `texture.rs`, `residency.rs`
- Why: GPU textures are separate objects from the title's memory; each texture needs a card allocation + staging. The pool
  (`pool.rs:20-60`) cuts blocks of 64 MiB to avoid 310 us allocations.
- [M] "83,816 textures uploaded (5,057 MB)" over the first 5 minutes; "83,393 reads of the GPU side served from the pages (4,017 MB)"
  per 10 s window (line 26086).

### 1.9 In-place / zero-copy path - `inplace.rs`
- Exists: `VICTORY_INPLACE=0/1/2` (`inplace.rs:89`), default **off** (unset in the play settings; `grep VICTORY_INPLACE` in
  the log header: absent). Reasons it is off (static): view memory (page pool) is a SEC_RESERVE section, the host refuses it
  (`inplace.rs` header, `xmem.rs:706`); level 2 cannot combine with `VICTORY_LATE_WAIT` (`inplace.rs:86`); opening locks all
  pages (256 MiB: 70 ms); `VICTORY_PORT_CONSTANTS` doc says "constants heap in place: the GPU then waits (slower overall) on a
  12 GB card" (`docs/README-foxhound.md:142`).
- Conclusion: in place is not a free lunch on a PCIe card because the card then reads PC memory over the bus.
  It is the right answer only for read-once, write-combined, upload-style memory (constants, streamed instance data).

## 2. Ranked structural opportunities

Estimates are for the submitting/landing critical path on this PC (RTX 4070). Frame is ~48 ms; items overlap, do not sum.

### R1. Learn and persist the GPU-write footprint; stop declaring whole UAV views - est. 10-18 ms/frame, risk medium
**Problem [S+M]:** declared writes = the whole bound range (`draw.rs:2167`, `:5589`). The 174 MB heap is declared from dozens
of offsets, so (a) a large part of the heap is "pending" in every flight, forcing `merge_under_writes` (9.6 ms), gap-splitting in
`update` (`shadow.rs:3926`, comment "copying the gaps whole cost ~1 s per frame"), snapshots and compare passes; (b) the card
hashes and fetches 260 pieces/frame to land 0.77 MB.

**Design:**
1. Per Shadow keep `gpu_blocks: BitVec` (64 KiB blocks of the copy ever found changed by the card compare) plus a probation
   cursor. Feed it from the existing compare result (`card_back` `host.rs:5158` / `walls_merged` shows the same data: which
   pieces differed).
2. `Shadow::gpu_writes` (`shadow.rs:3220`) clamps `[address, address+len)` to `gpu_blocks` plus the block(s) probation selects
   this frame (e.g. 1/16 of the unlearned blocks of that copy, rotating, as `picked.rs` `slice` already does at 32 MB/frame).
   Everything downstream (pending ranges, snapshots, merge-under-writes, hash/compare passes, fetches) then only sees the
   learned footprint. The hash passes for non-landed blocks can keep running on the card (cheap, no CPU work) if exactness is
   wanted.
3. Anything discovered on probation joins the set, lands, and is logged.
4. Persist: `cache_dir/memory-profile/<build>.json` with key = (copy size, allocation sequence number or xmem allocation
   site hash) -> block bitmap. Addresses vary between runs (xmem address-range logic, `xmem.rs:80`), so key by allocation
   identity + offset, not address. Validate on load with the first N frames (any differing block promotes).
5. Metrics: log `gpu_blocks` size, probation discoveries, and `0.77 MB real vs fetched` ratio.

**Risk:** a first-ever GPU write to an unlearned block while the title also writes the same page in flight loses the GPU
bytes (the page goes full copy). Mitigation: probation + the existing `merge_under_writes` path is kept as fallback for unlearned
blocks; persistence removes the cold start; kill switch like `VICTORY_PICKED_WRITES`.
**Files:** `shadow.rs` (`gpu_writes`, `Entry`, `update`), `host.rs` (`declared_writes` :3960, `card_back` :5158),
`draw.rs:2167,:5589`, new `memory_profile.rs`.
**Experiment first [H]:** count distinct blocks of the 174 MB heap ever differing across 60 s of gameplay. If it is
under ~20 % of 2,784 blocks the saving is as estimated.

### R2. Learned CPU-read set for landings ("demand landing") - est. 5-11 ms/frame (the fps line's "copying back"), also shortens frame latency, risk high
**Problem [M]:** landing is on the critical path because the title is told nothing before it is done; relay thread waits
27-30 ms for the card and works 17 ms; only 0.77 MB of 18 MB is real.
**Requirement [S]:** only memory the title CPU reads after a GPU write needs landing (ocean textures `texback.rs`, room
culling readbacks `host.rs:3984`, BVH structures `memory.rs`). There is no read detection today (`VICTORY_READ_WATCH` is
mentioned only in a comment, `texback.rs:7`; nothing in the tree implements it).
**Design:** add read observation on the same vectored handler (`watch.rs:3058`): in a probe mode, make landed pieces
`PAGE_NOACCESS` for K frames; a read fault records (copy id, block) in `cpu_read_blocks`, unprotects, lands that piece
synchronously and resumes. Kernel-side reads (ReadFile/DirectStorage into the page) bypass faults: `safecopy.rs` already
covers our own copies; title I/O into GPU-written pages must be excluded (treat as read-set if ever seen). Then landing is
restricted to `cpu_read_blocks`, all other pieces never fetched. Persist the read set with R1's profile.
**Risk:** high (unobserved reader means stale data, e.g. frame-delayed culling flicker, "outdoor flicker" in
`docs/findings.md` item 4). Start in measure-only mode and ship R1 first; keep a once-per-N-frames full landing as a safety.
**Files:** `watch.rs` (handler, protection), `land.rs`, `shadow.rs` `card_merge`, `host.rs:5013`.

### R3. Persisted per-resource write classes at allocation time - est. 3-6 ms/frame, risk low
**Idea [S]:** XMem flags tell the class for free: write-combined = CPU write-only, GPU read-only or GPU-private (no landing,
already used by `VICTORY_WC_NO_BACK`, min 1 MB); cached = bidirectional. Extend to:
- WC & GPU-read-only (never in a UAV): "upload class": no snapshot, no pending logic, no landing, refresh only
  (`wc_no_back_min` 1024 KB limits this today; use class for every size).
- Never dirty after N frames ("frozen"): skip `defer_refresh` entirely until the guard handler or the kernel dirty bit says
  otherwise. Today every draw still enqueues a refresh (`Shadow::refresh` `shadow.rs:3155`): [M] 9,551 flushes/frame, 8,477 (89 %)
  "known to copy nothing" and still 9.8 ms. A per-copy `epoch` (set by guard fault / one batched `GetWriteWatch` per region
  per frame) lets `defer_refresh` return in one atomic compare, recorded once per list instead of per draw.
- Classes persisted with R1's profile so frame 1 already starts with the right state.
**Files:** `shadow.rs` (`refresh`, `nothing_new`, `watched`), `host.rs:7250 defer_refresh`, `watch.rs` (epoch),
`xmem.rs:189` (flag capture).

### R4. One batched write-watch ask per region per frame - est. 2-3 ms/frame, risk low
`watch::asked/writes` (`watch.rs:660,674`) are asked per range; 4.1 ms/frame, 3.5 ms of it kernel asking. Ask the 3-5
large regions (174 MB heap, page pool views) once at frame start, publish into the journal, and let all refreshes read the
journal (they already can: `asked()`).

### R5. Texture residency: late loading is VRAM-room bound, not CPU-bound - est. late load 5 min -> 1-2 min [H], risk medium
**[M] evidence (lines 5430-20000 region, texture-room lines):**
- `no room, new textures wait` from 09:49:47 until the end: "8,092 MB in use - 1,500 MB free in texture blocks + 115 MB card
  copies + 1,846 MB render targets = 8,553 MB against a line of 7,932 MB (80 % of a budget of 9,915 MB)".
- Queue: 25,875 slots (09:49:47), 520 due with oldest waiting **533 frames** (09:50:08), 17,991 slots at 50:23.
- Uploads stall at 55,000 textures / 3.1 GB for ~60 s (09:50:02-09:50:23), then burst +12,000 at 09:50:29.
- Plus "textures picked by index: 15,195 passed over unmapped" - title had not mapped them.
- Settings cause part of it: `play.rs:213` sets `VICTORY_TEXTURE_ROOM_PERCENT=80` with frame generation (86 without), vs the
  computed `line_for` (95 %); `VICTORY_LEVELS_ON_DEMAND=0` (`play.rs:58`) although `docs/README-fxceless.md:69`
  measured 3.1 GB of card memory not allocated for levels the title does not hold ("no device loss"). tiles.rs:20-50
  quotes run 189: 2.2-3.0 GB of 4.3-5.5 GB copies were unmapped levels.
**Design:**
1. Turn levels-on-demand on by default on tier-2 tiled cards (RTX 4070 qualifies) and re-measure: frees ~2-3 GB, i.e. the
   whole "no room" deficit (621 MB) several times over.
2. Add usage feedback instead of uploading by written order: the pass uploads textures picked by index in the order the title
   wrote the slots (`draw.rs:3140+`, `HEAP_TEXTURES_QUEUE`), with no knowledge of which are sampled. `memory.rs` already has
   the pattern for pages ("faults the shaders found empty, read back once a frame"). Do the same for descriptor-heap indices:
   shaders OR the index into a visibility bitmap, read back once a frame; upload used slots first, evict slots unseen for N
   frames. (Nothing like it exists: grep for slot-use feedback is empty.)
3. Persist "textures used in this area" (key: texture fingerprint, `texture::fingerprint(at, len)` exists) so the next session
   prefetches the first-minute set as soon as the title writes the slots.
4. Do not hold 1.4-1.5 GB "free in texture blocks" as unusable: it is counted in "in use" and then subtracted; check whether
   `pool.rs` blocks of one size class can serve another (`Pool::take` `pool.rs:211`).
**Files:** `resource.rs` (`room`, `line_for`, :1963-2000), `draw.rs` (`heap_textures` ~3140), `pool.rs`, `tiles.rs`,
`apps/victory/src/play.rs:58,213`, shader emit for the visibility bitmap (victory-shader).

### R6. Descriptor records: dirty-slot list from guards instead of scanning - est. 5-8 ms thread / 2-4 ms critical, risk medium
[M] 210 k slots looked at for 9 k new; 124 of 200 changed pages are guarded views, so the fault handler can note the
4 KiB heap page (and the slot range from the fault address) at the instant the title writes; then compare only those
slots. Also cache record bytes -> host view (content-addressed), since 13 k of the mirrored are "no views" (empty).
Files: `descriptor/host.rs`, `heap_marks.rs`, `watch.rs`. (Probably owned by the descriptor agent; listed because it is the
same noticing problem.)

### R7. Guarded-store handling by default (`VICTORY_STORE_BESIDE` / `COPY_BESIDE`) - est. 1-3 ms title-thread time, risk medium
`titlewrite.rs`: 590 write faults/frame, a protection change is ~5 us and serialised. Both options exist and are off.
Evaluate on the current build (store beside + memcpy interposition) against the run `faults-1` numbers; the prior measurement
was 69 ms of threads' time before fixes -> ~6 ms after (`docs/findings.md` table), so most of this may already be gone.

### R8. Textures: skip hash for textures learned as write-once - est. 1-3 ms thread time, risk low
`texture.rs:1570-1620`: view-memory textures are hashed whole whenever not write-watchable. The `once` flag already exists
(`texture.rs:1448`); extend it with the persisted classes of R3.

## 3. Things that were checked and are not worth it (or are blocked)

- Full in-place heaps (`VICTORY_INPLACE=2`) for hot GPU-read data: the card would read system memory over PCIe (docs: the
  constants heap in place made the card wait). Blocked for view memory (SEC_RESERVE). Only upload-class memory (R3) is a candidate.
- Bigger landing thread pools: the 4 threads are already waiting 8.9-10.4 ms "for the title's bytes going in"
  (line 22003); the problem is volume, not parallelism.
- Replacing the title's `memcpy` (`COPY_BESIDE`) with more hooks: marginal compared to R1.

## 4. Suggested order and measurement

1. **Measurement patch (no behaviour change):** per-copy counters of distinct 64 KiB blocks changed by the card, of landed
   blocks read by the CPU (probe mode), of blocks the title CPU wrote while pending (the 61 k runs), keyed by copy id.
   Decides R1/R2 gains before any risky change. Log once per 5 s next to the `shadow:` lines.
2. R3 + R4 (low risk, a few ms).
3. R1 (largest) with persistence, probation, kill switch.
4. R5.1 (turn on levels-on-demand, a settings change) immediately: cheapest test of the late-loading hypothesis.
5. R2 only after R1 data shows how much remains.

Open questions that need data rather than code reading: (a) fraction of the 174 MB heap's blocks ever GPU-written;
(b) which non-WC GPU-written ranges the title CPU actually reads; (c) why uploads pause for ~60 s while "no room" holds
(eviction policy vs. title streaming), i.e. whether `no room` alone explains the 5 minutes.
