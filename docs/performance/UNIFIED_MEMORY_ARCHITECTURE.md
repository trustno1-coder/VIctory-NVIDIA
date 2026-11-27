# Unified memory architecture: placement by access pattern

Branch `perf/unified-memory` (from `perf/radical-refactor`). Written by reading only; **not compiled, not run** (the
user compiles). File:line references are to this branch at the commit that adds this document unless a later commit
is named.

## 0. The error, stated once

The console has one memory. The layer keeps a second copy of every title buffer a draw touches (`shadow.rs:28`
`Shadow`; for buffers >= 64 KiB two copies: `Card::staging` in PC memory and the card-side buffer, `shadow.rs:121`,
`shadow.rs:179 cards()`), and then spends the frame keeping the copies coherent:

| Cost | Mechanism | Evidence |
|---|---|---|
| noticing CPU writes | write watch / read-only guards + vectored handler | `watch.rs:3160 on_write`; 02-memory 1.2: 4.1 ms/frame asking, 323-1,017 re-protects |
| noticing GPU writes | card hash/compare passes over declared ranges | `host.rs:4446 card_before`; 02-memory 1.5: 150 hash + 360 compare passes/frame |
| moving bytes | refresh (`shadow.rs:4236 update`, `:4405 copy_stale`), sends, landing (`land.rs`, `host.rs:5048 relay_land`, `:5193 card_back`) | 02-memory 0: 30 MB/frame copies, ~18 MB fetched to land 0.77 MB |
| merging under flights | `shadow.rs:4041 merge_under_writes` | 02-memory 1.4: 9.6 ms/frame, 61k runs of ~36 B (constants, counters) |
| waiting | settles (`host.rs:5487 settle`), read-wait (`host.rs:2907 late_blocked`, `:3093 blocked_by`) | EXPERIMENTS "read-wait": 18.6 ms/frame "a step that reads where a copy at its place writes" |

All of it exists because there are two homes for the same bytes and no one says which home is authoritative. The
structural fix is to give every *access* exactly one authoritative home, chosen by how the bytes are written and read,
so that for the common classes there is nothing to notice, move or wait for.

## 1. Hard constraints that shape the design (not choices)

1. **A heap opened on title memory cannot take tile mappings** (`inplace.rs:30-31`: "Tile mappings into such a heap
   are refused and REMOVE THE DEVICE"). Therefore one GPU virtual range cannot be backed partly by title memory and
   partly by VRAM through a reserved resource. *Per-64 KiB mixed backing under one view is impossible.* The split has to
   be made per **access** (per bound view/root address), not per page under a single view.
2. **The 174 MB heap is mostly heavily-read static data.** Run 69: 190-330 MB of distinct ranges of it read per list
   (`shadow.rs` `cards()` doc, line ~160). In PC memory those reads cross PCIe; this is exactly why port #1 with the
   whole heap in place made "the GPU wait (slower overall)" (`docs/README-foxhound.md`, switch table), and why
   `CARD_BUFFERS` took the BVH skinning list from 50.5 to 4.5 ms (`shadow.rs:170-176`). The heap as a whole must stay
   VRAM-resident.
3. **Skinning writes through one UAV of the whole heap** (`port.rs` header; `draw.rs:2216-2235` declares "all that is
   left of the title's buffer"). A UAV view needs one contiguous GPU range; with constraint 1 it must be VRAM.
4. **CBVs need 256-byte aligned addresses** on the host (`draw.rs:1145-1165` root CBVs, `descriptor/host.rs:4889-4899`);
   the title aligns constants to 16. *Resolved on `perf/raw-constants` (section 10):* translated shaders no longer
   read constants through CBVs but as raw bytes (raw root SRVs need 4 bytes, raw views in slots 16), so no constant
   needs a copy because of its alignment.
5. **Title writes are visible to the GPU in place without any action**: the opened heap is a custom write-back L0 heap
   (`inplace.rs:27-30`); the GPU snoops. This is the one place the console's model holds for free.

## 2. Classification (per access, with a per-4 KiB-page oracle)

| Class | What | Home | Bound how | Coherence work |
|---|---|---|---|---|
| **C1 CPU->GPU** | constants (the ring in the 174 MB heap, per-draw constant buffers) read through CBVs | **T: title memory in place** (port region, one buffer over the whole stretch) | root CBV / table CBV address = region buffer + offset | **none**: no refresh, no fill, no send, no wait, no landing |
| **C2 GPU-written** | skinning outputs, compute UAV results, title GPU copies (copies at their place) | **V: VRAM** (existing card copy) | views as today | landing of what the card compare finds (existing); C2 pages never resolve to T |
| **C3 static/heavy read** | geometry, index data, structured buffers, textures | **V: VRAM** (existing card copy, uploaded once) | views as today | write watch only to notice the rare rewrite (refresh finds "nothing new") |

The oracle that separates C1 from C2 is per 4 KiB page and has three inputs, all facts the layer already has:

- **Landed lately** (`watch.rs:1931 LANDED`, `watch.rs:2046 landed_lately`, window 600 frames): every page a landing
  wrote into, fed by `watch::landing` (`land.rs:616`, `shadow.rs:2398`) and `watch::landed_beside` (`land.rs:528`),
  aliases included. The card compare finds real writes inside declared ranges. 600 frames is the window the shipped
  `fill_is_titles` default already reads title memory under (`host.rs:2944`); skinning outputs land every frame and so
  never leave C2, while a ring page written once by a title copy returns to C1.
- **Copy-bound at record time** (new, `port.rs` `PAGE_MARKS` stamp, 8 frames): destinations of every title buffer copy
  as it is recorded (`host.rs` `defer_copy`, `host/place.rs:61 note_placed`). Such a copy may be done on the card
  (`host/card_steps.rs` stage 2, or at its place in the list) and land after the flight; the stamp covers "written on
  the card, not landed yet" (the relay keeps one or two frames in the air; `placed_lately` trusts two).
- **GPU-bound for good** (new, `PAGE_MARKS` = `u32::MAX`): destinations of read-backs into title memory
  (`host.rs:7338 defer_read_back`).

An access resolves to T only if **no page it can read is marked by any of them**, the address is in an opened port
region, the CBV's rounded size fits in the region, and the host address is aligned for the view that reads it (since
`perf/raw-constants`: 4 bytes for a root constant buffer, 16 for one in a table; 256 only on a host without raw
twins). Anything else takes the old path, unchanged.

### The mixed 174 MB buffer

It is not split under one view (constraint 1). It has **two homes at once**: V (the card copy that every
SRV/UAV/root-SRV/vertex/index/indirect access uses, as today) and T (the port region buffer, which CBV accesses to C1
pages use). The same title address resolves differently by access kind:

- `draw.rs` `root_view` (root constant buffer, `srv == false`, a raw root SRV since `perf/raw-constants`):
  `port::constants(address, 64 KiB, Use::Root)` first; `Shadow::find`/`gpu` and the 64 KiB refresh only on
  fallback (any 4-byte address; scratch only off 4 bytes).
- `draw.rs` table CBV (`View::Constant` in `tables`): `port::constants(address, size, Use::Table)` first; the slot
  is made a structured view of dwords over the region buffer (`descriptor/host.rs` `constant_at` /
  `constant_views`, which holds the buffer); scratch + `defer_constants` only on fallback.
- Title slots holding a CBV (descriptor mirror, `make_twinned`): `port::constants(address, size, Use::Mirror)` first,
  else the host copy (V); a structured view either way.
- Everything else keeps `shadow::find` / `Shadow::gpu` (V).

Why that is coherent: C1 pages are by definition pages no shader writes, so V's bytes for them can only be *older*
than T's, never newer; a CBV reading T reads the newest bytes there are. V keeps being refreshed for the views that
read C1 pages (a skinning pass that reads bone matrices from the ring through its UAV), exactly as today.

## 3. Data flow after the change

```
title CPU writes constants ──► title memory (= T) ──► GPU reads via CBV at execution (PCIe snoop, L2-cached)
                                    │ (nothing noticed, sent, filled, merged or waited for)
title CPU writes geometry ───► title memory ──write watch──► refresh ──► V (once; "nothing new" afterwards)
shader writes (UAV) ─────────► V ──card compare──► land ──► title memory ──► LANDED pages (= C2 for 600 frames)
title buffer copy (any kind) ► CPU at submission, or V (card step / at its place) ──► land; stamped at record (C2 for 8 frames)
```

Per frame this removes, for C1 accesses: the 64 KiB `Shadow::refresh` per aligned root CBV (`draw.rs:1159`), the
`Step::Fill` + scratch per aligned table CBV (`draw.rs:4702-4709`, `host.rs:7281`), with them the card-step fills
(`host/card_steps.rs`), the write-watch asks those refreshes made, the sends of ring pages they caused into V, and the
read-wait/settle those fills triggered when a flight's declared writes covered the ring (`host.rs:3079 read_blocked`).

## 4. What is removed, what stays, and why

Removed in this branch:

| Item | Where | Why it is dead |
|---|---|---|
| `VICTORY_PORT_CONSTANTS` switch and the whole-buffer port path (`port::place_for`, `wanted`, `on`) | `port.rs`, `shadow.rs` lookup and windows, `inplace::counted`, menu item, `xmem.rs:1148 port_on` | whole buffers in place is the measured-slower model (constraint 2); the port region is now T, always opened |
| `VICTORY_FILL_TITLE` switch | `host.rs:2944` | the predicate is kept as the code default (it was on by default) |
| `VICTORY_WC_NO_BACK`, `_MIN_KB` switches | `shadow.rs` `wc_no_back`, `wc_no_back_min`, `play.rs:48-49`, launcher, menu | C2 rule for write-combined memory: the title maps WC what its CPU does not read; >= 1 MiB (the launcher's measured values: smaller ones such as exposure are read back) |

Kept, and why (honest scope):

- **Write watch / guards (`watch.rs`)**: still needed for C3 rewrites and for every V view that reads C1 pages. Off the
  hot path for C1 CBV accesses only.
- **Landing, relay, card hash/compare, `merge_under_writes`**: still the C2 path. Their volume comes from over-wide
  UAV declarations (`draw.rs:2216-2235`, `draw.rs:5643-5650`), which this branch does not narrow.
- **`tight.rs` (learned footprint)**: stays OFF. Narrowing a declaration without an exact fallback lets a write
  outside the footprint stay on the card past a fence (EXPERIMENTS "tight-writes", violates invariant R1).
- **read-wait step 2 (`host/place.rs:361 placed_constants`)**: still needed for constants that are destinations of
  copies at their place; those are C2 pages by the oracle and are resolved before T is asked (fills keep their order).
- **Descriptor mirror CBVs (`descriptor/host.rs:4889`)**: heaps the title writes and shaders pick by index keep V
  addresses; moving them to T needs the mirror to re-make slots on class change (next step, section 6).
- **`VICTORY_INPLACE` (whole private buffers in place)**: an unrelated opt-in experiment, off; left as is.

## 5. Invariants and how each holds

- **R1 (shader writes the CPU reads reach title memory before any fence/present/wait that observes them)**: unchanged.
  C2 lives in V and lands through the existing relay; nothing about landing order was touched.
- **R2 (stale results never overwrite newer title bytes)**: T is title memory; no copy is ever written into it by this
  path. V for C1 pages may be older than T, and no landing writes C1 pages (by definition no shader writes them).
- **Title writes reach the GPU before the submission that reads them**: trivially true in T (the GPU reads at
  execution, which is later than the fill used to snapshot them; the console also reads at execution).
- **Freed/remapped memory**: `inplace::title_memory_freeing` -> `port::forget` drops regions; bound lists keep the
  region buffer alive (`HostList::keep`, slot holds), which still maps our own view of the old pages (never unmapped),
  so no GPU fault; new binds fall back.

## 6. Risks and concrete mitigations

| Risk | Mitigation |
|---|---|
| First-ever shader write to a page that a CBV in the same or a later in-flight list reads (not landed yet, not declared at record) | Same accepted risk, same window, as the shipped `fill_is_titles` default (`host.rs:2925-2943`, `!landed_lately(600)`), plus the record-time copy stamps. The page becomes C2 at its first landing. If it shows (a constant one frame old in cutscenes), `LANDED_FRAMES` in `port.rs` set to `u64::MAX` makes landed pages the shaders' for good. Log: "renderer port 1 ... N on pages shaders write". |
| A title copy whose flight lands more than 8 frames after it was recorded | `COPY_FRAMES` (`port.rs`); the relay keeps 1-2 frames in the air. Raise it if `VICTORY_SUBMIT_FRAMES` is raised. |
| A draw recorded *before* a title copy in the same list reads the copy's new bytes (the CPU copy runs at submission, ahead of the list) | Same write-after-read ordering the ring discipline excludes (the ring advances to a new place each time); the copy stamp is set at record and any CBV recorded after it falls back. |
| The title rewrites a ring slot while the GPU still reads it (ring discipline assumed from the console) | That is the console's own contract (its GPU reads at execution too); our fences are signalled no earlier than the GPU's completion plus landing. Port #1's header already assumed it. If constants flicker: check "constants read in place" against the frame line, then disable by making `port::constants` return `None` (one line). |
| Region cannot be opened (lazy/soft views not committed, not one stretch, host refusal) | Falls back to the old path; retried every 600 frames for uncommitted/not-a-stretch; a host refusal disables T for the run. The log line counts each reason. |
| 185 MB of PC memory locked while the region stands | Bounded by `MOST` (512 MiB); the title keeps the heap for the process lifetime. |
| GPU reads constants over PCIe instead of VRAM (aligned root CBVs used V before) | Constants are tiny and L2-cached; table CBVs were already in upload (PC) memory (`host.rs:7369 scratch_held`). Watch "the card's clock" busy % in the frame line. |
| Pages mapped lazily (`VICTORY_SOFT_VIEWS=copies` in the launcher maps pool views when first touched, `xmem.rs` `map_pages(.., lazy)`): the stretch may be uncommitted or unknown to `inplace` when first asked | `NOT_COMMITTED`/`NOT_STRETCH` are retried every 600 frames; the log line counts them. If they never clear, the region is never opened and nothing changes (old path). |
| The region is opened on whichever thread records the first CBV in it (a THIN worker): a one-off map + open + place, pages locked (tens of ms once) | Under `REGIONS`' write lock, which no other lock is held around (`watch.rs:3015-3017` calls `views_gone` then `title_memory_freeing` sequentially). |
| `xmem::arrange` now always runs for the 32-bit reservation | It only exchanges never-mapped, never-written slices (`xmem.rs:1160-1172`); it ran in all port #1 runs. Log: "pages given their section's slices in address order". |

## 7. Next steps (designed, not in this branch)

1. **C2 demand landing (report 02 R2).** Land only pages the CPU reads. Mechanism: after a page lands, protect it
   `PAGE_NOACCESS` for a probe window; `watch.rs` `on_write`'s handler records read faults (copy id, page) into a
   lock-free bitmap and unprotects; the learned read set then gates `card_back` (pieces outside it are compared but not
   fetched). Exactness requires that an unlanded, unread page is protected and that a fault on it blocks the faulting
   title thread until the relay lands that piece; this is a fault-handler-to-relay handshake (no locks or allocation
   in the handler, `ARCHITECTURE_AUDIT.md` 4.1) and must be soaked. Ocean maps are textures (`texback.rs`), already by
   kind, unaffected.
2. **Exact narrow declarations** for C2 (tight footprint with a card-side whole-range compare-and-land as the
   fallback), which removes most of `merge_under_writes` and the 96 % landing waste.
3. **Mirror CBVs to T**: `descriptor/host.rs:4889` resolves through `port::constants` too, re-made on class change.
4. **C1 for small CPU-written vertex/index windows** (UI vertices outside buffers): same oracle, bound in T.

## 8. Log lines to check (see also section 10)

- `renderer port 1: a stretch of the title's graphics memory opened as a host heap through a view of ours` (once).
- Frame-rate block: `renderer port 1: N regions ... (M MB); per frame: X constants read in place (Y KB), fell back: a misaligned, b written by shaders, c outside the regions; ...`.
- `renderer port 1: the title's 32-bit graphics pages given their section's slices in address order` (xmem).
- Must fall: frame line "copies ms/MB", card-step fills, "title memory read" settles, "preparing per frame ... refreshes".
- Must not rise: `settled for: title memory changed under shader writes`; no constants flicker in cutscenes; boats float.

## 9. Commits on this branch

1. this document;
2. `port`: title-memory home (one region buffer over the 32-bit heap's stretch, `port::constants`, the per-page
   oracle, `gpu_bound`), always on; whole-buffer port path and `VICTORY_PORT_CONSTANTS` removed (gpu, xmem, menu);
3. `draw`: root and table CBVs resolve to the title-memory home first; `note_placed` and `defer_read_back` mark
   their pages; `VICTORY_FILL_TITLE` removed (its default kept);
4. `shadow`: write-combined copies >= 1 MiB keep shader writes on the card by default (`VICTORY_WC_NO_BACK`,
   `_MIN_KB` removed from code, launcher, `play.rs` and menu);
5. `port`: the page oracle per 4 KiB page (`PAGE_MARKS`): read-backs for good, every title buffer copy for 8 frames
   from its recording (`defer_copy`, `note_placed`: copies done on the card by `card_steps` or at their place land
   after their flight), landings for 600 frames.

## 10. `perf/raw-constants`: constants read as raw bytes (removes constraint 4)

Measured on this branch's port (log play-20261127-111731-14036): the region opened (185 MB), yet per frame only 371
constant views were read in place and 6,185 were "not 256-aligned"; 6,149 fills still ran on the CPU every frame
(lists' steps 29.8 ms, card copies 10.9 ms, submitting thread 71 of 71.7 ms, ~14 fps). The 256-byte rule is
D3D12's for CBVs; the console has none, and its constants lie on 16.

Structure after the change (no switch; constants in code):

| Access | Shader reads (victory-shader `emit/lower.rs` `constant_view`) | Host binds | Needs |
|---|---|---|---|
| root constant buffer | `rawBufferLoad` through root SRV `t<slot>` (`ROOT_CBV_SPACE`), zeros past 64 KiB by the code | `Set*RootShaderResourceView` at T (`port::constants(.., Use::Root)`) or the host copy | 4 bytes |
| constant buffer in a table | structured load (props 12, stride 4) of element `byte / 4` through its own slot | structured view (`DXGI_FORMAT_UNKNOWN`, stride 4, sized like the CBV: zeros past it) over T or the scratch copy | 4 bytes |
| title slot (mirror) holding a CBV | as in a table | structured view over T (`Use::Mirror`) or V | 4 bytes |
| constant buffer picked from the heap by a computed index | with raw twins: `rawBufferLoad` through the twin (null raw view if off 16); without: as in a table | twin: raw view of the same bytes | 16 / 4 bytes |

Revision (same day, log play-20261127-114321-28084): the first version read tables' constants through the slots' raw
twins and used `port::constants(.., 16 or 256)`. On NVIDIA the view heap is capped at 1,000,000 descriptors and the
twins (2 x 2^21) do not fit: `host view heaps hold 1000000 views ... raw_twins=false`, so every table constant asked
for 256 bytes: ~108 views a frame in place, 2,841-4,075 "not aligned", 2,574-3,669 fills, 13.8 fps. Cause (b), not
(a). Fix: a constant buffer's slot itself is a structured view of dwords, which begins on any 4 bytes and needs no
twin and no residual offset (a residual table would cost a root SRV per shader and a dependent load per read; a
structured view's first element is the title's own dword). Counts are split by binder: off 4 bytes as root, in
tables' slots, in title slots; read in place in tables' slots and in title slots.

Coherence is the port's (section 2, 5): only the view kind changed; which bytes are T is still decided per page by
the oracle. Fallbacks (not in a region, pages shaders write, off alignment) keep the old copy paths, so correctness
does not depend on the port opening. Pages shaders write keep V exactly as before.

Not done: a mirror slot made in place keeps the region buffer until the title rewrites the slot; a page that becomes
the shaders' later is still read in place through it (the same window as the port's oracle, section 6).

Log lines: `renderer port 1: ... per frame: N constant views read in place (...; T in tables' slots, M in title
slots, the rest root views)`: N in the thousands; `not in place: off 4 bytes R root, T' in tables' slots, M' in title
slots` all ~0; `fills read out of them` ~0. Must not appear: `host root signature refused`, `host pipeline refused`,
`constant buffer not on 4 bytes on the host`.
