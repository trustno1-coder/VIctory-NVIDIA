# Copy bridges between title memory and GPU-accessible backing

Status: Phase 1 audit and plan (committed on `phase/copy-bridge-1-audit`), with the Phase 2
implementation written up in section 12 (`phase/copy-bridge-2-invariants`).

This is the Phase 1 audit of the copy-bridge elimination work, and the plan that follows from it.
It covers the whole pipeline between the title's memory and whatever the host GPU actually reads
or writes: the host mirrors of title buffers and windows, their transfer, hash, compare and
fetch-back machinery, the texture path, the direct-access path that already exists off by
default, and the GPU resources that only look like copies. Every row states the source and
destination, why the copy exists, whether D3D12, the hardware, resource layout, synchronisation or
title semantics require it, whether direct access to the original backing is possible, the
guarantees a removal must preserve, the evidence needed, and a classification. Line numbers are of
the tree at `60e8281` (the accepted RAM/VRAM separation work on base `63b902f`) unless a source
comment names a run.

The companion documents are [memory-separation.md](memory-separation.md) (heap classes, ownership
rules and the transfer contract) and [memory-separation-window-budget.md](memory-separation-window-budget.md)
(the window/texture budget audit and the separation-exception matrix). This document does not
repeat them; it looks at the same pipeline from the copy side.

The environment has no NVIDIA GPU (a VMware SVGA adapter only). Everything here is source analysis
plus the repository's own software-adapter (WARP) probe and tests, cited as such. Nothing in this
document is a claim about physical residency, PCIe cost or target-driver behaviour.

## 1. The model in one page

The title is an Xbox Series title: its GPU memory is console unified memory, so a **title GPU
address is a CPU address**. The layer emulates the title's D3D12 object model in CPU RAM
(`resource.rs:1014-1183`) and materialises real host D3D12 memory only for data the host GPU must
read or write. Two consequences shape every bridge in this audit:

- The title's authority over its memory is absolute. Its CPU may write any title address at any
  time; the title's memory manager (`victory-hle/src/modules/xmem.rs`) may map, unmap, commit,
  decommit, free or reprotect pages at any time. A host copy is therefore a **mirror** the layer
  must keep current in both directions.
- A D3D12 resource cannot be created over arbitrary title pages by the ordinary committed or
  placed path. The only API route is `OpenExistingHeapFromAddress1` on a committed allocation or
  on a committed **view** of a section, plus `CreatePlacedResource` for a buffer in it
  (`inplace.rs:120-172`, `239-274`). That route exists in the tree as the **direct-access path**,
  off by default.

Bridges are therefore of three kinds, and the distinction decides everything below:

1. **Mirror bridges.** A host copy of bytes that also exist in title memory: title buffer and
   window copies (`shadow.rs`), title-filled textures (`texture.rs`). The copy is removable for the
   memory classes the GPU can reach directly.
2. **Materialisations.** A host resource whose contents exist only on the host: render targets,
   depth and UAV textures, twins, GPU-written textures (`resource.rs`). They are not copies of
   title bytes; direct access would require the GPU to render into title memory, which needs layout
   and format compatibility, not just an open heap.
3. **Infrastructure.** Resources D3D12 requires regardless: descriptor heaps, command lists,
   PSOs, swap chain, fences, indirect clamping, per-draw CPU-written scratch. Direct access never
   removes these, and the audit's job is to say so once, not per bridge.

## 2. Method and evidence classes

- Static call-graph and ownership analysis of `crates/victory-gpu/src` and
  `crates/victory-hle/src`, from every constructor of a host buffer or texture backwards to every
  caller, and from every copy/transfer site to the resource it serves.
- The repository's own WARP evidence: `inplace.rs`'s probe and tests, `port.rs`'s test, and the
  device tests in `host.rs`/`residency.rs` (run on CI `windows-latest`).
- The repository's measured numbers quoted in source comments, marked [run] below, and the
  launcher's product switch set (`apps/victory/src/play.rs:55-116`).
- No build-, run- or hardware-dependent claim is made. Where a claim needs a run it says so.

## 3. Title memory kinds (the backing side)

The placement decision depends on which kind of memory the title bytes have. This table is the
shared vocabulary for the rest of the document; none of these are bridges.

| ID | kind | allocated / reported by | openable as a D3D12 heap? |
|---|---|---|---|
| M01 | private committed write-watched allocation (the default `XMemVirtualAlloc` class) | `xmem.rs:87-199`; `title_memory_allocated` at `185` | yes, a committed part (`From::Part`, `inplace.rs:120-135`; probe `910-975`) if every page is committed read-write in one allocation |
| M02 | private committed allocation without write watch (caller-fixed base) | same call, no watch flag | yes, same predicate |
| M03 | placeholder reservations (the console 64 KiB-page kind) | `xmem.rs:87-199`; host `MEM_RESERVE_PLACEHOLDER` | only committed parts of it; the reservation itself is not memory |
| M04 | the title's 32-bit graphics heap: a placeholder whose views become one section stretch below 4 GiB | `xmem.rs:191-196`; port `port.rs:4-18` | yes through our own view of the stretch (`port.rs:170-226`) |
| M05 | the page pool: `SEC_RESERVE` pagefile-backed sections (6.1 GiB [run]) | `xmem.rs:723-799`, `740-753` | not through `OpenExistingHeapFromFileMapping` (reserved sections are refused, `inplace.rs:22-27`); a committed **view** is openable through `Address1` (probe section test; port WARP test) |
| M06 | title views of section pages (committed lazily, aliasable, possibly scattered) | `xmem.rs:1296-2203`; reported by `title_memory_view` at `1855, 2006, 2527` | only a range that is one contiguous run of one section: our own view of that run is openable; scattered pages are not (current `cover()`, `inplace.rs:506-540`) |
| M07 | memory shown in very many views (>64) | `watch.rs:2144-2145`; `inplace.rs:408, 443` | no placement decision may rely on the guard's view list for these; key by physical page |
| M08 | our own whole-section view used by `beside` (landings, `land.rs:25-30`) | `inplace.rs:576-614` | it is a view, so a part of it is openable; its lifetime is the section's |
| M09 | xmem's own whole-section view (`Section::window`) | `xmem.rs:424-459` | same |
| M10 | port's own view per region | `port.rs:195-219` | same; kept forever today (see Phase 5) |
| M11 | backing of title resources (`XMemAlloc` through the title's hooks) | `resource.rs:3271-3275`; `d3d12x.rs:20-22` | M01/M02 kinds in practice |
| M12 | title heap memory (`CreateHeap` → `Memory::new`) | `resource.rs:1150-1183` | M01/M02 kinds |
| M13 | `CreateCommittedResource` at a title address | `resource.rs:3447-3488` | M01/M02 kinds |
| M14 | memory flagged write-combined by the title | `xmem.rs:187-190`; `shadow.rs:2435` | a note only; placement is tried before the write-combined policy branch today |
| M15 | DirectStorage/engine writes into pool pages | `dstorage.rs`; pages committed by `Section::window` | commits pages; placement must see the commit as an event |
| M16 | the title's own CPU copies/fills (executed by the layer on the CPU) | `host.rs:8267-8341` (`Step::Copy`/`Step::Fill`) | not memory; they are the title's commands and must respect placed writes |

Openability is a measured API contract, not a policy (probe: `inplace.rs:13-36`; WARP assertions
`1447-1508`, `port.rs:338-400`): `Address1` takes any committed part of one allocation with every
page committed and one protection; the heap is custom write-back pool L0 (system RAM), shared and
cross-adapter shared, 64 KiB aligned; a placed resource must be a buffer with
`ALLOW_CROSS_ADAPTER` and `ALLOW_UNORDERED_ACCESS`; a texture would have to be row-major with one
level, and a tile mapping into such a heap removes the device. While an address-opened heap stands,
the title's own `VirtualFree`/`VirtualProtect` on those pages fail with error 87.

## 4. Inventory

Columns: **src → dst**; **where**; **why it exists**; **required by**; **avoidable?**; **direct
access possible?** (and the bounded prerequisite); **guarantees a removal must preserve**;
**evidence needed**; **classification**. Classifications are the mission's four:
REMOVABLE_NOW, REMOVABLE_AFTER_PREREQUISITE, REQUIRED, UNRESOLVED.

### 4.1 Title buffers and windows outside buffers

| ID | what (src → dst) | where | why / scale | required by | avoidable? | direct access possible? + prerequisite | guarantees to preserve | evidence needed | class |
|---|---|---|---|---|---|---|---|---|---|
| B01 | title buffer mirror: title RAM → CPU-mapped `CUSTOM/WRITE_BACK/L0` UAV buffer, refreshed by write watch, GPU writes found by snapshot compare and merged back | `shadow.rs:5525-5653` (lookup), `11014-11107` (buffer), `4264-4676` (refresh), `3569-3724` (declarations/snapshot), `3736-3982` (copy-back) | the GPU cannot use title pages, so the bytes are mirrored; a refused copy stays in system memory for the run. 5.7 GB of copies in PC memory at the default 512 MiB card cap on a 4070 SUPER [run, `shadow.rs:217-224`]; copies ≥1 GiB can never be carded (u32 piece counter, `shadow.rs:427-429, 482-484`) | nothing in D3D12 requires the mirror; it is this layer's way to expose title memory | yes, for the system-memory class | yes where the backing is openable: M01/M02 through `place_part` (stage 2 already exists, `inplace.rs:140-172`); M06 through a section-view placement (new; Phase 3). Prerequisite: an in-flight gate or placed-write declarations (G18), and a synchronous revoke (G13) | G1-G3, G4-G7 (copy-artefacts, not needed for placed), G8-G20 for everything that still copies the same bytes; see §6 | census on hardware (`VICTORY_INPLACE_PROBE`); WARP data-integrity test; a two-submission ordering test; hardware free-under-GPU run | REMOVABLE_AFTER_PREREQUISITE (Phase 4, gated; Phase 5 unbounded) |
| B02 | two-sided buffer copy: CPU staging → per-submission send memory → `DEFAULT` card side; hash twins + on-card compare + fetch + merge | `shadow.rs:5604-5650`; `host/send.rs:108-323`; `host.rs:4856-4887, 5222-5803` | a device-local copy cannot be CPU-written/read; the transfer is the D3D12-forced price of the VRAM class | D3D12 (device-local memory + CPU staging) | no, while the card side remains device-local | not a direct-access candidate: replacing a VRAM side with system-memory title pages is a memory-class downgrade | all copy guarantees (§6) | none for removal; kept | **REQUIRED** |
| B03 | window mirror: title memory outside buffer objects → 4 MiB-window copies in system memory (or card side), refreshed and copied back like B01 | `shadow.rs:5877-5986` (loose), `5418-5570` (coverage); 803 windows / 3.3 GB in run 60 [run, `shadow.rs:5664-5668`]; 861 / 3,552 MB run 104 | a title GPU address need not be inside a buffer object; the layer covers the window with a host buffer | as B01 | yes, for the refused/system-memory class | yes: same as B01; stage 1 already places private windows (`shadow.rs:5922-5929`), the missing classes are view memory (Phase 3) and the decision (place only when the VRAM side is refused, Phase 4). Shaders wrote no windows in run 91 ("0.0 MB compared in 0 ranges", `shadow.rs:548-549`) | as B01 | as B01, plus the run-91 evidence re-taken | REMOVABLE_AFTER_PREREQUISITE (Phase 3/4) |
| B04 | window card side: `DEFAULT` buffer with its staging/send, admitted by `window_card_most` (9% of budget, ≤1024 MiB, 0 below 8 GiB) | `shadow.rs:562-629, 687-718` | moves shader reads off the bus; the window's bytes stay in system memory for the CPU side | layer performance policy | no while used | not a direct-access candidate (memory-class downgrade) | B02's guarantees | none | **REQUIRED** |
| B05 | by-address shader memory: arenas + directory + faults, copies of title pages for raw-address shaders | `shadow.rs:5811-5860`; `memory.rs:1-67, 212, 299-437` | translated raw-address shaders need the bytes at a GPU address; a placed buffer's GPU VA is driver-chosen, so the title's CPU address cannot be recreated for them; arenas live for the process (`memory.rs` never removes copies, so the cap give-back at `shadow.rs:3031-3033` is unreachable) | shader address translation (layer architecture) | no | **no** by this mechanism; a redesign (device-local arena with staged CPU writes) is UNRESOLVED | arena contents/lifetime rules | none; keep | **REQUIRED** (design change out of scope) |
| B06 | `exact()` view-byte copies: a view whose bytes do not start on its buffer's element gets a private copy, refreshed and copied back | `shadow.rs:5988-6073` | a host view counts elements from its buffer's start; the title places views at ring positions | layer view model | yes (the view's own bytes are ≤64 MiB and system memory by design) | yes where openable, same as B01 | as B01 | WARP test with a placed `exact` range | REMOVABLE_AFTER_PREREQUISITE (Phase 4) |
| B07 | write-combined / mapped-VRAM experiments: `VICTORY_WC_BAR` (`GPU_UPLOAD` buffer the CPU writes, no copy-back), `VICTORY_WINDOWS_BAR_MB`, `VICTORY_SEND_BAR` | `shadow.rs:654-674, 5584-5603`; `host.rs:11113-11175`, `11090-11108` | CPU-mapped VRAM shortcuts for CPU-written data; opt-in | none (experiments) | yes (off by default) | a placed title buffer is the console-correct form of the same idea for openable memory; otherwise keep | the write-combined policy and stale-RAM contract | hardware A/B | REMOVABLE_AFTER_PREREQUISITE (policy decision; Phase 6) |
| B08 | second system-memory sides (`VICTORY_CARD_ALL`, `VICTORY_WINDOWS_OWN`): a "card side" that is RAM | `shadow.rs:386-399, 605-629` | keeps two-sided semantics when the card refused, at the cost of a second RAM buffer | layer compatibility experiment | yes (off by default) | direct placement can give the same two-sided behaviour for openable memory without the second buffer | two-sided semantics | decision test | REMOVABLE_AFTER_PREREQUISITE (Phase 6, with the in-flight model) |
| B09 | buffers made ahead: 4,096 × 64 KiB shared + card buffer pairs made by a background thread; 1,024 kept | `ahead.rs:22-160`; `host.rs:11014-11021, 11176` | hides allocation latency at scene start (2.8 s of a first 3D frame [run]) | layer performance only | yes (`VICTORY_BUFFERS_AHEAD=0`) | not applicable | none (pure cache) | hardware timing to decide | REMOVABLE_NOW (switch; keep by default) |
| B10 | per-submission send memory: changed ranges of a copy's CPU side → UPLOAD (or mapped VRAM with `VICTORY_SEND_BAR`) → card side | `host/send.rs:108-323`; 73-93 MB a frame [run, `send.rs:126-132`] | an in-flight submission must send the bytes as they were at prepare time | D3D12 + the layer's in-flight model, for a VRAM side | no while B02 exists | not applicable | B02's ordering | none | **REQUIRED** |
| B11 | hash twins, on-card compare and the 1 MiB results area: GPU compare of a declared range with the card side's bytes-before, read by the CPU | `host.rs:4856-4887, 4903-5183`; `shadow.rs:424-429, 4801` | finds GPU writes to a device-local copy without CPU snapshots | B02's design | no while B02 exists | not applicable | compare/fetch ordering behind sends | none | **REQUIRED** |
| B12 | fetched-piece buffer: differing pieces from the card side → shared system-memory buffer → CPU merge into title memory | `host.rs:5389-5395, 5671-5803`; `shadow.rs:4897-5001` | the CPU needs only the differing pieces, through the cheapest existing path | B02's design | no while B02 exists | not applicable | fetch behind sends; checked waits | none | **REQUIRED** |
| B13 | card steps: 4 MiB device-local buffer + a list performing copies/fills on the card in flight order | `host/card_steps.rs:216, 920-1040`; product `VICTORY_CARD_STEPS=2` | avoids a 25-30 ms wait of the submitting thread [run] | layer sync performance | yes (`CARD_STEPS=0` waits instead) | direct placement removes the copies it schedules, not the need to order some steps | ordering of copies relative to flights | hardware timing | REMOVABLE_AFTER_PREREQUISITE (Phase 6) |
| B14 | list upload memory: title/CPU bytes → UPLOAD (or `CUSTOM/WB` with card steps) buffers → copies into host resources | `host.rs:7855-7865, 2314-2351`; `list.rs:1720-1760` | D3D12 requires CPU-written data for copies to come from UPLOAD memory (or a CPU-visible heap) | D3D12 / CPU-authored data | yes where the data is title memory and the target is placed | not applicable | written-before-use, not rewritten while a copy may read it | existing tests | **REQUIRED** (where it moves title bytes, Phase 3 may remove it) |
| B15 | the title's own CPU copies/fills (`Step::Copy`/`Step::Fill`): executed by the layer on the CPU at preparation | `host.rs:8267-8341`; 13,000 a frame [run] | the title's console GPU copies are served on the CPU; interleaved with the layer's own steps | title semantics + layer design | no (this is the title's command) | not a copy bridge of host memory; but it is a **CPU reader of title memory** and must respect placed writes (G18) | ordering against flights | ordering test | **REQUIRED** (ordering work in Phase 5) |
| B16 | CPU-mapped VRAM send memory (`VICTORY_SEND_BAR=1`) | `host.rs:11090-11175`; `host/send.rs:138-160` | CPU writes send memory straight into VRAM instead of copying staging | experiment | yes | not applicable | as B10 | hardware A/B | REMOVABLE_AFTER_PREREQUISITE (policy) |
| B17 | window coverage variants: tails (`VICTORY_WINDOW_TAILS`), finer grids (`VICTORY_WINDOW_KB`) | `shadow.rs:5720-5749, 5877-5929` | less copied memory per ask | layer policy | yes (switches) | direct placement makes coverage granularity irrelevant for openable memory | unchanged coverage semantics | run with the switch; census | REMOVABLE_AFTER_PREREQUISITE (Phase 4) |

### 4.2 Textures

| ID | what | where | why / scale | required by | avoidable? | direct access possible? | evidence | class |
|---|---|---|---|---|---|---|---|---|
| T01 | title-filled texture mirror: title RAM (console tiled layout) → `DEFAULT` PC-layout texture, filled through staging and detiling | `resource.rs:1701-1829, 2969-3100`; `texture.rs:2119-2228`; 3,871 textures / 9.2 GB created before the first world frame, 7.3 GB title-written at it [run, `resource.rs:1282-1288, 1905-1907`] | the host cannot sample the console's tiled/swizzled layout; the mirror is a layout conversion | resource layout (console tiling) + D3D12 sampling | only for a layout-equal subclass | only for sample-only, linear (unswizzled), one-level textures with a PC-identical format and row pitch in one committed openable range; otherwise no (`inplace.rs:30-32`) | a WARP texture probe + a read-only layout census; no hardware-free proof of the eligible share | REMOVABLE_AFTER_PREREQUISITE (layout-equal subset only; mostly REQUIRED) |
| T02 | render-target / depth / UAV host copies | `resource.rs:1668-1676, 1896-1951, 2999-3100`; 2.8-3.1 GB two minutes in, 2.1-2.9 GB targets at 1440p [run] | the console renders into unified memory; the PC needs a host target/depth resource in a PC layout | D3D12 target/depth semantics + layout + depth+UAV restriction | only for layout/format-compatible subsets | would require the GPU to render into title memory directly: open heap + PC-compatible layout + a target/depth view over it; the depth+UAV restriction (T03) still applies | a target/depth layout census + WARP creation probe; hardware run | REMOVABLE_AFTER_PREREQUISITE (large but high-risk; not this project's first migration) |
| T03 | depth twins (colour twin for the stencil plane, R16/R32 second plane, per-twin through buffer, plane pass) | `resource.rs:342-592`; `host.rs:1651-1740, 7312-7450` | this layer's PC path refuses depth resources with unordered access, and no 24-bit colour format holds the depth plane | the layer's D3D12 path + copy-format families | no until a host depth+UAV path exists | not applicable (the twin is the workaround; direct depth would remove it only with T02) | a WARP/NVIDIA depth+UAV creation probe | **REQUIRED** |
| T04 | texture staging: title bytes (detiled) → `UPLOAD` buffer → `CopyTextureRegion` into the host copy; 64 MiB blocks, 8 kept (512 MiB), 2,246 buffers / 2.1 GB at end of run 24 [run] | `texture.rs:2119-2228`; `pool.rs:524-552` | D3D12 requires UPLOAD memory for CPU-written texture data; detiling is a CPU step | D3D12 + layout conversion | yes with T01/T02 direct placement | not applicable (disappears with the mirror) | WARP; run counters | REMOVABLE_AFTER_PREREQUISITE (with T01/T02) |
| T05 | texture read-back: GPU-written host texture → `READBACK` buffer → CPU tiling → title memory (`texback`) | `texback.rs:1-58, 144-249` | the title's CPU reads GPU-written texels; they are in title memory on the console | title semantics + layout | the over-copy by kind (~1 MB per such list) is removable after a read-watch-driven demand; the copy itself with T02 | not applicable | read-watch run | **REQUIRED** (over-copy REMOVABLE_AFTER_PREREQUISITE) |
| T06 | title texture→buffer read-back (two tiny targets per frame) | `list.rs:1318-1376`; 80 + 16 bytes [run] | same as T05 for buffer destinations | title semantics + D3D12 readback | with T02 | not applicable | existing counters | **REQUIRED** |
| T07 | texture pool blocks (64 MiB `DEFAULT` heaps, up to 4 GiB kept at 85% budget) | `pool.rs:28-77, 790-883` | 310 µs per allocation vs 6 µs from a block [run] | layer performance only | yes (`VICTORY_TEXTURE_POOL=0`) | not applicable | timing run | **REQUIRED** (perf; switch exists) |
| T08 | levels on demand: reserved texture + 64 MiB tile heaps + `UpdateTileMappings` | `tiles.rs:74-210, 510-592` | the title holds only some mip levels; 2.2-3.0 GB of 4.3-5.5 GB copies in levels whose title memory is not mapped [run] | memory saving | yes (`VICTORY_LEVELS_ON_DEMAND`, launcher off) | not applicable while host copies exist | hardware run with the switch | REMOVABLE_AFTER_PREREQUISITE (or keep as the no-direct-access saving) |
| T09 | format stand-ins (consoles' 16-bit depth-stencil, B5G5R1/B4G4R4/R9G9B9E5 targets) | `resource.rs:246-311` | no PC format for these | API/format | no | no (the bytes can never be the title's format on the host) | documented | **REQUIRED** |
| T10 | alias copies / neutral stand-ins / host-written textures | `texture.rs` (alias/THROUGH), `descriptor/host.rs:410-480` | descriptor safety and layout aliasing | descriptor model | no | not applicable | existing tests | **REQUIRED** |
| T11 | upload-ahead (`VICTORY_UPLOAD_AHEAD` copy queue) and prelude uploads | `texture/ahead.rs`; `host.rs:7855-7865` | moves texture uploads off the submission's critical path | scheduling | yes (switches) | not applicable | hardware timing | **REQUIRED** (perf) |

### 4.3 GPU resources that only look like copies

These are listed once so the plan does not chase them. Full rows are in the audit's resource report;
the counts come from there.

| ID | resource | where | why it exists | direct access? | class |
|---|---|---|---|---|---|
| D01 | host shader-visible descriptor heaps mirroring the title's CPU descriptor heaps (up to 2^21/2^22 descriptors, 64-128 MiB each) | `descriptor/host.rs:1875-2125` | D3D12 shaders index real heaps; the title's heaps are CPU RAM | impossible: D3D12 gives no way to back a descriptor heap with application memory | **REQUIRED** |
| D02 | descriptor-stride table (8 MiB `UPLOAD` per non-sampler heap) | `descriptor/host.rs:2056-2068, 3479-3491` | translated shaders need element stride/format kind per view slot | CPU-authored data read while recording; a double-buffered VRAM design is UNRESOLVED | **REQUIRED** (justified CPU-visible exception) |
| D03 | indirect clamp scratch, batch buffers, 256 B work budget | `indirect.rs:1-34, 235-551`; `host.rs:7946-8085` | untrusted title arguments must be clamped before the GPU reads them | not applicable (layer-owned derived data) | **REQUIRED** |
| D04 | command allocators/lists/preludes | `host.rs:4704-4772, 1491-1614` | D3D12 recording | not applicable | **REQUIRED** |
| D05 | PSOs, root signatures, command signatures | `pipeline/host.rs:596-1067`; `draw.rs:5491-5527` | D3D12 objects for translated shaders | not applicable | **REQUIRED** |
| D06 | swap chain and present copies/compose machinery | `host.rs:9784-10804`; `compose.rs:34-190` | DXGI owns the back buffers; the presented texture is ours | impossible (DXGI-owned) | **REQUIRED** |
| D07 | overlay/menu/notice buffers | `host/panel.rs`, `host/menu.rs` | the layer's own UI | not a title copy | **REQUIRED** (feature) |
| D08 | frame-generation hudless copies + present proxies | `streamline.rs:1476-1633` | the feature's input | not a title copy | **REQUIRED** (opt-in feature) |
| D09 | keep-awake buffer/dispatch | `host/awake.rs:123-178` | the card lowers clocks between submissions [run] | not applicable | **REQUIRED** (feature) |
| D10 | query heaps: title's are emulated CPU objects; diagnostics heaps only in diagnostics builds | `query.rs:1-51`; `list.rs:1604-1653` | the title's queries produce plausible values; diagnostics need timestamps | title: nothing on the GPU to remove | **REQUIRED** |
| D11 | per-draw scratch (`UPLOAD` chunks: vertex/rect expansion, constant views, root-view copies) | `host.rs:2314-2521, 7886-7915`; `draw.rs:1136, 4711-4717, 4979` | CPU-written per draw, read by the same submission; VRAM would need double buffering | UNRESOLVED (documented exception) | **REQUIRED** |
| D12 | resident ping-pong 256 B copy and the residency probe | `host.rs:259-331` | confirms committed resources are reachable (`MakeResident`) | small; `VICTORY_WAIT_RESIDENT` controls the wait | **REQUIRED** |
| D13 | list scratch spare (128 MiB), upload buffers, preludes | `host.rs:2314-2351` | recording intermediates | not applicable | **REQUIRED** |
| D14 | diagnostics: capture/readback/probe buffers, book/popwatch, replay | `capture.rs`, `book.rs`, `popwatch.rs`, `replay/*` | tooling | build off | **REQUIRED** (outside the product path) |
| D15 | title upload arena (CPU low memory registered as a title buffer) | `upload.rs:1-14, 147-263`; `shadow.rs:5418` | console memory the CPU and GPU both address | CPU memory reached by B05's arenas; not a host GPU resource | **REQUIRED** |

## 5. The direct-access path that exists today

Both mechanisms are structurally complete and off by default.

- `inplace.rs`: `place_for` (`140-172`) admits only when the range is **not** view memory
  (`cover() == Cover::None`, `144-148`), is within `VICTORY_INPLACE_MB` (4 GiB default,
  `95-102`), and is private committed whole (`private_whole`, `644-659`); it opens an
  `Address1` heap on the 64 KiB-aligned part (`120-135, 239-249`) and places one
  cross-adapter UAV buffer (`251-274`). `VICTORY_INPLACE=1` places windows, `=2` also title
  buffers but only while they are "not big" in the card sense (`shadow.rs:5566-5580`); the
  launcher's `VICTORY_CARD_BUFFERS_MIN_KB=64` makes every registered buffer big, so in the
  product path stage 2 places windows only (`inplace.rs:65-74`).
- `port.rs`: maps our own view of a stretch of the title's sub-4-GiB graphics-heap section, checks
  every page committed, opens the heap on the view and places buffers/windows in it (`105-141,
  143-226`). The `LOW` gate (`63-65`) exists because the constants heap is the only memory below
  4 GiB; non-32-bit reservations start at or above 4 GiB by design (`xmem.rs:71-85`), so the 6.1
  GiB page pool is out of reach today.
- `Shadow::placed` (`shadow.rs:3486-3497`) shapes a placed buffer as a copy so binding, descriptor
  views and lifetime work unchanged; refresh, snapshot, shader-write declaration, copy-back and
  `placed_copy` are all no-ops (`3569-3592, 3624-3627, 3736-3745, 3946-3949, 4267-4271,
  4308-4311, 4737-4741`).

Measured API rules, probe and WARP tests: see §3. What the placed path does **not** provide is the
subject of Phase 5: it records no shader writes (so the layer's own CPU reads of the same range do
not wait — the relay is on by default), it cannot see a placed write from any other host copy of
the same bytes (write watch is a CPU-write mechanism), and a heap opened on the title's private
pages makes the title's own `VirtualFree`/`VirtualProtect` fail with error 87 while it stands.
The last is wired only partially: `title_memory_freeing` is called from the mapping-change path
(`watch.rs:2908-2915`), not from `title_memory_freed` (`watch.rs:2850-2884`), not from
`XMemFreeDefault`'s raw `VirtualFree` (`xmem.rs:370-389`), not from `XMemFreePhysicalPages`
(`xmem.rs:806-819`), and `VirtualProtect` is only intercepted under `VICTORY_SOFT_VIEWS`
(`xmem.rs:2829-2842`). Even the wired path retires through `host::retire`, which defers drops by
16 or 64 submissions (`host.rs:472-531`), so the pages stay pinned past the let-go.

## 6. Synchronisation and lifetime contract a removal must preserve

The copy machinery's correctness rests on a declaration-based contract, and a placed copy bypasses
it. The full guarantee table is G1-G20 in the audit's sync report; the part that matters for
placement:

- **Copy-artefacts a placed path does not need**: refresh visibility (G1/G2), busy-range exclusion
  from refresh (G3), GPU→CPU copy-back before the title is told (G4), sends/compares/fetches
  ordering (G5/G6), snapshots (G7), merges (G20). Direct access is console-correct for these: the
  GPU writes the bytes the title reads.
- **Guarantees a placed path must still supply**: objects named by a submitted list outlive the GPU
  work (G8), a title list may be reset while its host submission is in the air (G9), readbacks are
  complete before the CPU reads them (G10), device removal is detected and fatal (G11), CPU reads
  tolerate remapping (G12), a placed heap is let go before the title reuses its pages (G13),
  descriptors are current (G14), refusal falls back to a correct copy (G15), the title's fences
  signal after the writes (G16), the layer's CPU writes into title memory are journalled (G17).
- **Missing today and required before placed ranges may be shader-written (Phase 5)**: a record of
  placed shader writes (G18) and visibility of a placed write to other host copies of the same
  bytes (G19). The relay/late model is on by default (`shadow.rs:1081-1089`,
  `host.rs:2924-2939`), so "no submissions in the air" is not a property of the product path.

## 7. Dependency graph

```
title memory (M01-M16) ── view registry events ──┐
   │                                            │
   │  copy path (default)                        │  direct path (off by default)
   v                                            v
lookup/loose/exact/by_address                inplace::place_part (private part)   port::place_in (own view of
   │  shared_buffer (CUSTOM/WB/L0 mirror)    inplace::place_for                     a sub-4-GiB section stretch)
   │  or uav_buffer (DEFAULT card side)            │                               │
   v                                            └────────────── Shadow::placed ────┘
Shadow{staging, card, hash twin}                 (buffer + heap + bias; no refresh,
   │  refresh (watch)   snapshot   declarations      no snapshot, no copy-back)
   │  send (per submission)  compare (on card)          │
   │  fetch  merge  readback/texback                    │
   v                                                    v
host queue / fences ───────────── retire/bury ─────> drops deferred 16-64 submissions
   │
   └─ device removal: fatal; landings/relay ordered behind flights

budget feedback: CARD_BYTES/WINDOW_CARD_BYTES/ADDRESS_CARD_USED/WINDOW_MAPPED_BYTES
  -> card_room_kept -> texture queue room (resource.rs:1883-1891) -> texture admission
  -> placement ledger (residency.rs:355-418) -> frames
```

The three consequences that drive the plan:

1. All host copies are created through five constructors (`host.rs:9673, 9712, 11014, 11025,
   11113, 11176`); the decision to place directly must be taken **before** the first of them, and
   the admission decision that decides the memory class must be separable from the allocation
   (today `card_for`/`window_card_for` test and allocate in one call, `shadow.rs:436-459,
   605-629`).
2. The direct path already provides bindings, lifetime and accounting; what it lacks is a
   backing-storage registry that knows which memory can be opened (and when that changes), a
   synchronous revoke, and a placed-write declaration model.
3. The bridges that must stay are exactly the ones that give the GPU a memory class the title's
   pages are not (VRAM card sides), the ones D3D12 requires (infrastructure), the ones needed by
   layout (textures, depth) and by address translation (by-address arenas).

## 8. Classification summary

- **REMOVABLE_NOW** (no new mechanism, no hardware evidence): the look-ahead buffer cache (B09) and
  the write-combined/mapped-VRAM experiments (B16) are switch-controlled caches/shortcuts; the
  second memory sides (B08) are experiments superseded by the direct path's semantics. None of
  these is a *necessary* copy; they are kept only as performance/behaviour switches.
- **REMOVABLE_AFTER_PREREQUISITE**: the system-memory mirrors of title buffers (B01), windows
  (B03), view bytes (B06) and the coverage variants (B17) — the largest copy class by bytes; the
  texture mirror and its staging (T01/T04) and the target/depth class (T02), for a layout- and
  format-compatible subset only; the texback over-copy (T05); the card-steps performance layer
  (B13); own-view lifecycle defects (M08-M10); the levels-on-demand policy (T08).
- **REQUIRED**: the card-copy machinery (B02, B10-B12), by-address shader memory (B05), the
  title's own CPU steps (B15), all texture bridges that exist for layout/format reasons
  (T01 mostly, T03, T09, T10), and all infrastructure (D01-D15).
- **UNRESOLVED**: overlapping placed resources and D3D12 aliasing barriers; the NVIDIA driver's
  coherency/pinning behaviour for opened heaps; multi-run (scattered/holes) compositing; a
  device-local by-address arena design; WDDM residency and PCIe cost. Each is stated with the
  evidence that would settle it in §10.

## 9. Ranked implementation plan

The plan is one project, not six unrelated optimisations. Each phase is a branch off the previous
phase branch and ends with a reviewable commit; the final deliverable is one commit against
`63b902f`.

**Phase 2 — backing-storage and ownership invariants** (no placement behaviour change)
- Split admission from allocation: `card_admission(size)`/`window_admission(size)` (same rules,
  counters and logs) from `card_allocate`/`window_allocate`, so a direct placement can be chosen
  on the same evidence as the system-memory fallback (`shadow.rs:436-459, 605-629, 5604-5650,
  5943-5983`).
- Add the placement/backing model: an openability predicate (`Cover::Run` + committed/protected
  page check) and a backing registry keyed by physical backing (section handle + offset for view
  memory; allocation base for private memory) with generations, one owning view per section, and
  refcounted teardown (buffer → heap → view). Replace port's unowned region view; add the
  section-close notification.
- Extend the placement ledger (`residency.rs`) with direct-backed live bytes by class and with
  direct-refusal reasons; keep the API-heap-class totals as they are.
- Fix `in_place_let_go`'s bounded search (`shadow.rs:5449-5475`, last 256 buffers / 4 MiB of
  windows) so a covering placed copy cannot be missed.
- Tests: unit tests for the decision/classification functions and counter balance; a WARP test
  that a refcounted own view is unmapped after its last heap and not before.
- Acceptance: no allocation, budget, policy or rendering change with the direct switches off;
  `cargo check`/clippy clean; the existing WARP tests unchanged.

**Phase 3 — minimal direct-access prototype: page-pool windows** (independently switchable)
- Generalise `port.rs`'s "own view of one section stretch" into the registry: `place_view`, which
  takes a title range that `cover()` reports as one run of one section at any address (the 4 GiB
  gate is gone), maps our own view of that run, verifies commit, opens the heap and places a
  buffer; scattered/holes/uncommitted ranges are refused with a reason.
- Prototype switch `VICTORY_INPLACE_VIEW=1`: used by the window path only, and only when the
  window would otherwise be a system-memory mirror; the mirror remains the fallback. While the
  relay/flights model is unchanged, the prototype is gated to runs without late submissions
  (documented, warned) — Phase 5 lifts the gate.
- WARP tests (extending `port.rs:338-400`): a placeholder above 4 GiB, two committed views of one
  reserved section, GPU reads/writes both ways across the views; a title-view protect/unmap/remap
  while the heap stands (the corrected P2 test); a scattered ask refused to a copy; switch off
  means no placement and no allocation.
- Acceptance: the prototype's data path is WARP-proven; no default behaviour change.

**Phase 4 — migrate eligible resource classes**
- Windows: when the window card side is refused (or windows have no card budget), place the window
  directly over its openable backing (private part or section view) instead of making the
  system-memory mirror; fall back to the mirror for every refusal and for allocation failure.
- Title buffers: the same decision for copies whose card side was refused and for copies ≥ 1 GiB
  that can never be carded, behind the same switch; keep the card path for admitted copies
  (memory-class rule).
- View-byte copies (`exact()`) participate when openable.
- Add the "would have been a mirror" accounting: direct placements per class and refusal reasons,
  so a run's log shows how much of the system-memory class the direct path replaced.
- Tests: WARP data-integrity tests per class (GPU reads the title bytes at the right offset; GPU
  writes land at the title address; a CPU write after placement is seen by the GPU; no mirror
  allocation is made); decision tests with a forced admission refusal; fallback tests for
  uncommitted/scattered/cap/host-refused.
- Acceptance: with the switch off, byte-identical allocations and behaviour; with it on, the
  mirror is absent for placed copies and present for every refused one.

**Phase 5 — coherence and lifecycle safety**
- Placed-write declarations: `Shadow::gpu_writes`/`writing` for a placed copy declare the range
  into the same flight/pending state as a copy, without snapshot or copy-back; `copy_back_done`
  releases it; `late_blocked`/settle (the machinery that already delays CPU reads of declared
  ranges) then covers placed writes too.
- Overlap policy: refuse placement over a range any host copy covers (and vice versa), or journal
  the placed writes to invalidate overlapping copies; D3D12 aliasing between two placed resources
  over the same bytes documented and tested as read-only coexistence or refused.
- Synchronous revoke: wire `inplace::title_memory_freeing` into `title_memory_freed`; hook
  `XMemFreeDefault`'s raw `VirtualFree`, `XMemFreePhysicalPages` and `VirtualProtect`
  unconditionally; drain the submissions that can name a placed buffer before returning to the
  title's kernel call (no `bury_lag` for placed objects); record any title-visible failure that the
  protocol could not avoid.
- Capability gate and pinned-byte budget: probe `ID3D12Device13`/`Address1` once, fall back for
  the run on failure, count pinned bytes and refuse over the cap; a refused placement is retried
  when the backing changes (commit/remap), so a transient uncommitted interval does not latch.
- Tests: WARP two-submission ordering test (a dispatch writes a placed buffer; the next
  preparation's CPU fill must see it only after the wait); title-view remap under a live heap;
  revoke-before-free; refusal and fallback fidelity; the existing device-removal wait tests stay.
- Acceptance: with the switch on, placed ranges are as ordered as mirrored ranges, the title's
  free/protect calls succeed after revoke, and every failure path falls back.

**Phase 6 — remove the superseded bridges and record the rest**
- Consolidate the duplicated own-view bookkeeping: `port.rs`'s region view becomes a user of the
  Phase 2 registry (one owner, refcounted), and the port's policy becomes a thin selection of
  sub-4-GiB view memory; no duplicate mapping lifetime remains.
- Remove the placed copies' refresh steps once the explicit keep replaces them (sync R5), and the
  write-combined/second-side experiments whose purpose the direct path now serves, with the
  evidence and the release note.
- Delete code that is provably unreachable after the migration; every remaining copy is documented
  in §10 with its requirement (D3D12 / VRAM class / layout / address translation / sync).
- Final evidence pass: check/clippy/tests, CI on the fork (WARP), the audit of the diff against
  `63b902f`.

## 10. Copies that remain, and why

These stay after the project, with the requirement that forces them:

1. **Carded copies of title buffers and windows** (B02/B04, B10-B12): device-local memory the title's
   pages cannot provide; the transfer machinery is the D3D12 price of that class. Replacing them
   with direct system-memory access would be a memory-class downgrade and is not done without
   hardware measurement.
2. **By-address shader memory** (B05): raw-address shaders need a GPU address that a title CPU
   address cannot be; a device-local-arena redesign is unresolved and out of scope.
3. **Textures that are not layout-equal** (T01/T03/T04/T09/T10): the console's tiled/swizzled
   layout, mip packing, depth+UAV restriction and missing PC formats require the conversion/staging
   or the twin workaround.
4. **Render targets and depth** (T02): rendering into title memory needs a PC-compatible layout,
   format and target/depth semantics over an opened heap; a bounded subset may follow with
   hardware evidence, not in this project.
5. **D3D12 infrastructure** (D01-D15): descriptor heaps, command memory, PSOs, swap chain,
   indirect clamping, per-draw CPU-written scratch, residency probes and the layer's own UI.
6. **The title's own CPU steps** (B15): the title's commands; only their ordering relative to
   placed writes is the layer's responsibility.
7. **Justified CPU-visible exceptions**: the GPU→CPU fault/compare channels, the descriptor-stride
   table, per-draw scratch and CPU-authored upload data — documented in
   `memory-separation.md` §3.4 and unchanged.

## 11. Evidence, tests and what this environment cannot establish

Run here: static analysis of the whole tree (buffers, windows, textures, resources, synchronisation
and call-graph completeness; the six audit reports under the workspace's `notes/copy-bridges/`
are the working papers). WARP evidence used: `inplace.rs`'s probe and tests, `port.rs`'s test,
the host/residency device tests; CI runs `cargo test --workspace` on `windows-latest` (WARP).

Planned per phase: `cargo check --workspace --all-targets [--all-features]`,
`cargo clippy --workspace --all-targets -- -D warnings` (baseline has pre-existing lints),
`cargo test` on CI, plus the WARP tests named in section 9.

Not establishable here, and explicitly not claimed:

- Physical residency of `DEFAULT` resources and WDDM paging behaviour (D3D12 has no per-resource
  residency query).
- The target NVIDIA driver's acceptance and coherency of `OpenExistingHeapFromAddress1` on private
  allocations and on committed section views; whether opening a view pins pages and at what
  working-set cost; whether the title's own views may be unmapped/reprotected while the heap
  stands (WARP says yes for the FileMapping form, untested here for the address form).
- The PCIe cost of GPU reads/writes of title memory in place versus a copy in system memory or
  VRAM; the ranking of the two needs an A/B hardware run.
- Device-removal/TDR behaviour of the direct paths.
- How much of the title's memory is openable (private-whole vs one-section-run vs scattered); the
  census exists (`inplace.rs:1125-1204`) but no run of it is recorded.

A hardware validation session should run `VICTORY_INPLACE_PROBE=1` with the census, then the
direct switch on a title save with `VICTORY_MEMORY_REPORT=1` and the placement ledger read
against DXGI usage, and finally a GPUView/ETW capture for residency and the PCIe question.

## 12. Phase 2 implemented: backing storage and admission invariants

Every change below is either a pure refactor or measurement; no allocation policy, admission
threshold, budget, switch default or rendering behaviour changed, and with the direct switches
off the same buffers are made in the same order as before.

Sections 12-16 are the per-phase implementation records, in order. Where a later phase
changes an earlier statement, the later section is authoritative; the cross-references in the
text mark those changes.

### 12.1 The admission decision split from the allocation

`shadow::card_admission` and `shadow::window_admission` now decide whether a copy is to have a
device-local side under exactly the old rules (`cards`, `card_limits` sizes and the 1 GiB
compare limit, `Host::card_ready`, the 80/97% budget line, and the `card_limits` /
`VICTORY_CARD_WINDOWS_MB` caps), including the old refusal counters and log lines; they allocate
nothing. `card_allocate`/`window_allocate` then take the cap and make the buffer, and
`card_side`/`window_side` compose the two with the `VICTORY_CARD_ALL`/`VICTORY_WINDOWS_OWN`
second-memory side. `lookup` and `loose` ask the admission first, then make the CPU staging
buffer and the side, so a direct placement (Phase 3/4) can be chosen on exactly the evidence the
system-memory fallback would be taken on, before any host buffer exists for a copy that will not
need one. The only observable differences are the order of two internal counters/log lines.

### 12.2 An openability predicate, not an address check

`inplace::openability(start, len)` classifies a title range without opening anything:

- `Openability::Private` - private memory, every page committed read-write in one allocation (the
  `Address1` part opening);
- `Openability::View { section, offset }` - title view memory whose bytes are one contiguous run
  of one section (the opening Phase 3 builds on; the port uses it today for the sub-4-GiB heap);
- `Openability::Refused(NotOpenable)` - `Unmapped` (reserved, decommitted, guarded, memory of
  another kind), `Scattered` (views of different places) or `Holes` (pages not mapped).

`inplace::place_for` uses it instead of its own `cover`/`private_whole` pair, so an opt-in
placement refuses the same ranges as before, with the same counters. A backing is not a virtual
address: callers are expected to re-ask after mapping changes.

### 12.3 The placed-copy registry, and an exact invalidation

The placed copies are now noted in a weak registry (`shadow::PLACED`) where a
`Shadow::placed` is made, and `shadow::in_place_let_go` takes its set from that registry instead
of bounded scans of the copies' tables (the last 256 buffers starting below the range; windows
starting within 4 MiB below it). A placed title buffer can start far below the changed range and
many copies can start in between, so the bounded scan could miss a placed copy that covers the
range and leave the GPU reading pages the title had reused. The registry is exact and short (only
placed copies are in it); the copies are removed from their tables by the key the ask that made
them used, or by identity. A test puts a placed copy below 300 other copies and asserts the
invalidation still finds it, and that the heap releases the title's pages.

### 12.4 Our views: one owner, refcounted, unmapped after the last user

A heap opened on a section view of ours must outlive the title's own views of the same pages but
must not outlive its last placed buffer. `inplace::SectionView` is that view now: an `Arc` owned
by every `Placed` made over it (`Placed::keep`) and dropped - unmapping the view - when the last
placed buffer and its heap are released (release order: buffer, heap, view). The port's regions use
it instead of a view that was never unmapped, and `inplace::title_memory_section_gone` tells the
port a section is gone (`port::section_gone`), so a forgotten or closed section leaves no heap or
view of ours behind. The port's WARP test now actually exercises the claim its comment used to
make: with the heap standing, the title's own view is made read-only and another is unmapped, the
placed buffer still reads the section's pages, and it survives the section being closed; only then
do the placed buffers go and the view is unmapped.

### 12.5 The placement ledger counts direct backing

`residency` now counts directly backed copies (title memory in place, no host copy) while they
live and since the start, windows and title buffers apart (`direct_add`/`direct_freed`), and the
asks that wanted direct backing and did not get it, by reason (`NoDirect::NotOpenable`, `Cap`,
`HostRefused`, `HostUnavailable`). The `memory placement` line prints both; it is now also
printed while a direct copy lives (not only while a system-memory fallback does), so an opt-in
direct run has its measurement without forcing `VICTORY_MEMORY_REPORT=1`.

### 12.6 Tests and checks

- Unit tests: `openability_names_private_and_view_memory_and_the_refusals`,
  `our_section_view_is_unmapped_with_its_last_holder`,
  `a_covering_placed_copy_is_let_go_without_a_table_scan`,
  `a_directly_backed_copy_is_counted_while_it_lives_and_after`,
  `direct_refusals_keep_their_reason_and_bytes`; the port and in-place WARP tests were extended as
  above.
- `cargo check --workspace --all-targets --locked`: PASS. `cargo clippy -p victory-gpu
  --all-targets --locked -- -D warnings`: the only failure is the pre-existing
  `clippy::manual_is_multiple_of` in `shadow.rs` (now line 1861), the same baseline condition
  documented in `memory-separation.md` section 8.1; the changed lines add no lint.
- `cargo test` cannot link on this host (GNU raw-dylib needs a working `dlltool`); the tests run
  on `windows-latest` (WARP) in CI.

## 13. Phase 3 implemented: the page-pool prototype

The first direct-access prototype for the memory the port could not reach: the page pool's view
memory, at any address. It is behind its own switch, off by default.

### 13.1 The mechanism

`inplace::place_view_part(device, start, len)` places a window (or buffer) over page-pool memory:

1. the 64 KiB-aligned range around `[start, start + len)` must be one contiguous run of one
   section in the title's view registry (`cover` gives `Run(section, offset)`); a scattered run
   or a hole is refused before anything is created;
2. `inplace::SectionView` maps **our own view** of that run of the section (the port's old
   mechanism, generalised and now owned by the Phase 2 registry);
3. every page of the view must be committed; a reserved section maps only committed pages;
4. `OpenExistingHeapFromAddress1` opens a heap on our view, and a placed cross-adapter UAV buffer
   covers the run;
5. `Shadow::placed` shapes it as a copy, so binding, descriptors and lifetime work unchanged,
   while refresh, snapshot, write declaration and copy-back stay no-ops for it.

`inplace::place_view_checked(start, len, window)` is the policy around it (written as
`place_view` at Phase 3): `VICTORY_INPLACE_VIEW=1` enables windows, `=2` also title buffers; the
in-place byte cap (`VICTORY_INPLACE_MB`, default 4 GiB, measured in whole view bytes because
opening pins them) applies. At Phase 3 a placement was only tried while no submission could be in
the air (`shadow::flights_air()`: the relay or the late wait). That gate was the Phase 5
prerequisite: a placed copy declared no shader writes yet, so the layer's own CPU reads of title
memory (constant fills, the title's buffer copies, texture staging) could not be ordered against a
flight's writes through it. Without the gate the prototype was unsafe in the product path, where
the launcher leaves the relay on; with it, the prototype ran with `VICTORY_RELAY=0` and no
`VICTORY_LATE_WAIT`. Phase 5 replaced the gate with the placed-write declarations (section 15.1);
the final code has no `shadow::flights_air` and no flight gate on either placement path.

### 13.2 Where it is used

The window path (`shadow::loose`) asks the direct placement after the device-local admission
refused the window, so a window that would have got a VRAM side is never replaced (the
memory-class rule of section 9). A placed window makes no staging buffer, no card side, no
refresh/write-watch state, no compare and no copy-back; the GPU reads and writes the title's own
pages over PCIe, which is the memory class the refused window would have stayed in anyway. Every
refusal (not one run, not committed, cap, host refused, and at Phase 3 also in flight) is counted
with its reason in the placement ledger and leaves the old copy path in place.

### 13.3 Tests and checks

- `warp_a_page_pool_window_is_placed_over_our_view_of_its_run`: a placeholder above 4 GiB, a
  reserved section, two title views that follow in memory and in the section; a 5 KiB window that
  crosses both is placed and the card reads the title's bytes at the right offset and writes them
  back into the title's own views; with the heap standing a title view is made read-only and then
  another unmapped, and the placed buffer still reads the section's pages; the last holder going
  unmaps our view and the address is free again.
- `place_view_part_refuses_scattered_and_private_memory`: adjacent views from two sections are
  refused as scattered, and private memory is refused (it belongs to the other opening).
- The existing port and in-place WARP tests still cover the older mechanisms.
- `cargo check --workspace --all-targets --locked`: PASS; clippy shows only the pre-existing
  baseline lint.

### 13.4 What it did not do

Scattered or holed ranges (multi-run composition is unresolved), memory shown in very many views,
title buffers until `VICTORY_INPLACE_VIEW=2` is wired and the flight gate is lifted (Phase 4 and
5), and any claim about the target driver's pinning, coherency or cost. Each placement opened its
own heap and view (no region cache yet); that was acceptable for a prototype and is where a
region registry matters for the buffer class (Phase 6 adds it, section 16.1).

## 14. Phase 4 implemented: the migration of the refused copies

The copy classes the audit ranked as removable now have a policy that replaces them where the
memory can be opened - and only there. It is one decision, in one place.

### 14.1 The policy

`inplace::place_fallback(start, len, window)` is asked by the copy paths exactly where the
device-local admission has already refused the copy, i.e. where it would have stayed a system
memory mirror. It tries, in order:

1. the page pool's view memory (`VICTORY_INPLACE_VIEW=1` for windows, `=2` for title buffers and
   view-byte copies) through `place_view_checked` - our own view of the section run, a heap on it,
   a placed buffer (Phase 3);
2. private title memory (`VICTORY_INPLACE=3` and more) through the first mechanism's
   `place_private` - a committed part of the allocation opened as a heap.

Both were refused while any submission could be in the air (`shadow::flights_air`), however the
switches were set: the placed-write declarations that make it safe under the relay are Phase 5,
which removed the gate and the refusal reason with it (section 15.1). The paths are a pure
function (`fallback_paths`) and unit-tested.

`VICTORY_INPLACE=1`/`=2` keep their old meaning (place wherever the memory can be opened, before
any admission decision) and are no longer taken for `=3` and more: from 3 on, only the refused
copies are placed, so no copy a VRAM side would have served is replaced by system-memory access.

### 14.2 Where it is wired

- `shadow::lookup` (title buffers): the card admission is asked, and when it refused, the
  fallback placement is tried before the CPU staging buffer exists. A placed buffer makes no
  staging side, no card side, no hash twin and no send/fetch/compare state; the GPU reads and
  writes the title's own pages, the memory class the refused copy would have been in.
- `shadow::loose` (windows outside buffers): same, after `window_admission`.
- `shadow::exact` (a view's own bytes, system memory by design): same policy for its copy; with
  `VICTORY_INPLACE_VIEW=2` or `VICTORY_INPLACE=3` those copies can be the title's memory too.

Every placement is counted in the in-place census and the placement ledger (Phase 2); every
refusal is counted with its reason. At Phase 4 the reasons were `NotOpenable`, `Cap`,
`HostRefused`, `HostUnavailable` and `InFlight` (the gate above); Phase 5 removed `InFlight`
with the gate. The startup lines name `VICTORY_INPLACE` and `VICTORY_INPLACE_VIEW`, warn for the
levels 1 and 2 combination with late submissions that the project keeps documented as
unsupported, and - for the view path - say that shader writes through a placed copy are declared,
so a log cannot show the switches set without saying what is and is not ordered.

### 14.3 What is not migrated, and why

- Copies the card admitted: replacing a device-local side with GPU access to system memory is a
  memory-class downgrade (section 9's rule) and is not done without hardware measurement.
- By-address shader memory: raw-address shaders need a GPU address a title CPU address cannot be
  (the audit's anti-candidate).
- Textures, render targets and depth: the console's tiled sizes and layouts, the depth+UAV
  restriction and the missing PC formats block a direct representation (sections 4.2, 10).
- Scattered or holed title memory: multi-run composition is unresolved; those copies keep the
  mirror.
- The layer's own CPU-authored buffers (per-draw scratch, descriptor-stride table, indirect
  clamp): they are not copies of title bytes.

### 14.4 Tests and checks

- `the_fallback_paths_follow_the_switches` (then named
  `the_fallback_paths_follow_the_switches_and_the_flight_gate`) covers the policy matrix (legacy
  levels, 3, view 1/2, windows/buffers); the flight-gate cases went with the gate in Phase 5.
- The Phase 3 WARP tests exercise both mechanisms' data paths; the Phase 2 tests exercise the
  invalidation, the ledger and the view ownership. A run of the title is needed to measure how
  much of the refused class is openable on a real save; no run is available here.
- `cargo check --workspace --all-targets --locked`: PASS; clippy shows only the baseline lint.

## 15. Phase 5 implemented: coherence and lifecycle safety

The placed copies now take part in the copy machinery's ordering, and the lifecycle holes the audit
found are wired. The prototype's flight gate is gone because the reason for it is fixed.

### 15.1 Placed shader writes are declared

`Shadow::gpu_writes` for a placed copy no longer returns in silence: it marks the copy written and
records the range through `HostList::defer_gpu_write`, exactly the declaration path a copy's
shaders use. The copy machinery builds each submission's written-range list from those declarations,
so the late waits and settles now make a CPU reader of those title bytes wait for the flight that
may write them - a refresh of an overlapping copy, a fill of upload memory, a title buffer copy or
texture staging. That was guarantee G18 in the audit and the reason `VICTORY_INPLACE=2` was called
unsupported with submissions in the air; the fallback placements (`VICTORY_INPLACE_VIEW`, and the
`VICTORY_INPLACE=3` private path) are therefore no longer gated on the relay being off.
The old, pre-existing shortcut is unchanged and documented: a host copy of the same bytes that a
shader writes through a *different* copy is not told, because the card's writes are not seen by
write watching. Placement does not widen it.

A test in the host's placement tests places a copy over private title memory, declares a shader
write to a host list, and asserts the range reached the list's deferred steps and the copy's
written flag: the declaration path is the one the machinery already uses for copies.

### 15.2 The title's free, decommit and protect let go of placed copies

- `watch::title_memory_freed` (called by the title's `VirtualFree` hook for free, decommit and
  release) now calls `inplace::title_memory_freeing` before the kernel call, so a placed copy is out
  of the tables and retired rather than silently blocking the title's call.
- `VirtualProtect` is now intercepted unconditionally (it used to be an export only with
  `VICTORY_SOFT_VIEWS`) and lets go of what is in place in the range before forwarding.
- `XMemFreeDefault`'s raw `VirtualFree` for the title's 32-bit allocations does the same.
- The device's part-opening call (`ID3D12Device13`) is probed once; a run without it refuses every
  placement as `HostUnavailable` (counted in the ledger) and makes copies as before, instead of
  failing a host call per ask.

### 15.3 Why the page-pool route is the safe one

A heap opened on the title's **private** pages pins them: the title's own `VirtualFree` and
`VirtualProtect` there fail with error 87 while it stands, and the heap is released through
`host::retire` (deferred by the bury lag), so a title call that races the retirement is the
documented private-placement exception. That is why the recommended migration is the page pool:
a heap opened on **our own view** of a section leaves the title's views, placeholders and kernel
calls alone.

The section's pages cannot be recycled under that heap either: the section object stays alive while
our view exists, so unmapping the title's view, freeing page numbers and closing the section handle
do not touch the bytes our heap reads. A WARP test writes a pattern through a title view, maps our
own `SectionView` of the same section, unmaps the title's view, closes the section handle, and
asserts the pattern is still readable through our view. The remaining invalidation - the title maps
other pages at an address, or the section ends - is already wired through
`title_memory_changed`/`title_memory_freeing`/`port::section_gone`.

### 15.4 Tests and checks

- `a_placed_copy_declares_its_shader_writes` (host placement tests, WARP): a placed copy's shader
  write reaches the copy machinery's declaration list.
- `our_view_keeps_the_sections_pages_after_the_title_lets_go` (WARP): the section's pages survive
  the title unmapping its view and closing the handle, through our view.
- The Phase 2-4 tests (openability, invalidation, view ownership, direct accounting, page-pool
  placement) are unchanged and still pass on CI.
- `cargo check --workspace --all-targets --locked`: PASS; clippy shows only the baseline lint.
- Not tested here: the ordering the declaration buys under a live relay needs a run with
  submissions actually in the air (the unit only proves the declaration is recorded); the target
  driver's behaviour remains unverified as stated in section 11.

## 16. Phase 6: the removal, the copies that remain, and the end state

### 16.1 Removed: the per-placement duplication of the page-pool view

The Phase 3 prototype opened one view of the section run and one heap for every placed copy. Its
replacement is a shared region: `inplace` keeps one `SectionView` and one heap per title run of a
section (`REGIONS`), and the placements whose addresses fall in the run place their buffers into
that heap at their offsets. A run is forgotten when the title shows other pages there
(`forget_regions`, called from `title_memory_freeing`), and the view stays mapped while any placed
buffer holds it - the same refcounted lifetime as before, now one per run instead of one per copy.
The WARP test places two copies in one run and asserts they share the region, then that the title
showing other pages forgets it. Opening a heap locks its pages, so a run larger than 512 MiB is
still cut to the placement that asked for it (`REGION_MOST`); the in-place byte cap is the same.

The port keeps its own older region policy (sub-4-GiB graphics heap, its own caps and statistics).
It has used the shared `SectionView` since Phase 2; unifying its region bookkeeping with the
general one is a follow-up, not a copy bridge.

### 16.2 The copy bridges that remain, and why

- **Carded copies of title buffers and windows** (CPU staging -> per-submission sends -> device-local
  side, hash twins, on-card compare, fetch and merge): required by the VRAM memory class; replacing
  them with GPU access to title system memory would be a memory-class downgrade. Unchanged, and
  still what the product launcher asks for (`VICTORY_CARD_BUFFERS_MIN_KB=64`).
- **System-memory mirrors of memory that cannot be opened**: scattered or holed view memory (no
  multi-run composition yet), reserved/guarded/decommitted private memory, runs over the in-place
  caps, runs without `ID3D12Device13`, and every copy the policy places while a placement is
  refused. These are the fallback of the direct path and the whole path with the switches off.
- **By-address shader memory** (arenas, directory, faults): raw-address shaders need a GPU address
  that a title CPU address can never be; a device-local arena redesign is unresolved and out of
  scope.
- **Textures**: the console's tiled/mip-packed layouts, format stand-ins, and the row-major
  single-level rule for a texture in an opened heap leave only a layout-equal linear subset as
  plausible direct access; that needs a layout census and a WARP/NVIDIA probe first. The mirror,
  detiling staging and texture read-backs stay for everything else.
- **Render targets, depth and UAV textures**: the host copy is the contents' authority and PC target
  and depth semantics plus the depth+UAV refusal require it; a layout- and format-compatible
  subset is future work, not this project.
- **Depth twins and the plane pass**: required by the PC's depth+UAV restriction and the missing
  24-bit colour format.
- **D3D12 infrastructure**: descriptor heaps (a mirror of the title's CPU descriptor bytes, with no
  API to back them with application memory), the descriptor-stride table, command allocators and
  lists, PSOs/root signatures/command signatures, the swap chain and present path, indirect
  clamping, per-draw CPU-written scratch, the residency probe, the layer's UI, frame generation and
  keep-awake. A direct-access migration cannot remove these; they are the documented required set.
- **The title's own CPU copies and fills**: the title's commands executed on its memory; the layer's
  job is their ordering, and placed writes are now declared and ordered like a copy's.
- **Off-by-default experiments now superseded for openable memory**: the second system-memory sides
  (`VICTORY_CARD_ALL`, `VICTORY_WINDOWS_OWN`), CPU-mapped VRAM (`VICTORY_SEND_BAR`,
  `VICTORY_WINDOWS_BAR_MB`, `VICTORY_WC_BAR`) and the legacy in-place levels 1 and 2. They are kept
  as switches because removing them is a policy decision that needs a hardware A/B; the fallback
  substitution gives the same or better behaviour wherever the memory is openable.

### 16.3 The end state

The project delivers, relative to the agreed base:

- a complete, sourced audit of the copy bridges and their classification (`docs/copy-bridges.md`);
- backing-storage and admission invariants, so a direct placement is chosen on the same evidence as
  the system-memory fallback it replaces, with the failure paths counted (`inplace::openability`,
  `card_admission`/`window_admission`, the placement ledger, the exact placed-copy invalidation);
- a working, switchable page-pool placement (`VICTORY_INPLACE_VIEW`) that opens our own view of a
  section run at any address, shares it across placements, and keeps the title's own views, free,
  decommit and protect calls untouched;
- the migration policy for the refused system-memory copies of title buffers, windows and view
  bytes (`VICTORY_INPLACE=3` for private memory, `VICTORY_INPLACE_VIEW=1/2` for page-pool
  memory), with the mirror as the fallback and every refusal counted;
- placed shader writes declared to the copy machinery, so the late waits order CPU readers behind
  them; and the title's free/decommit/protect and section-close paths wired to let placed copies go
  before the kernel call;
- tests on the software adapter for the placement mechanics, the ordering declaration, the view
  ownership and section lifetime, the invalidation and the policy matrix; and no default change:
  the product path behaves as at the base until a hardware-validated run enables the switches.

What is needed before any of it becomes a default is stated in section 11: target-driver behaviour,
pinning cost and coherency, the openable share of the real title memory (the census), the cost of
GPU access to title memory against the copies it replaces, and device-removal behaviour. The
switches and the startup warnings are the way to measure that without changing the product path.

## 17. Verification

The project has no NVIDIA hardware here. What follows is what the software adapter (WARP) and a
local Windows experiment establish, and what they cannot.

### 17.1 The section-view question, settled

The page-pool prototype rests on two properties of a heap opened on our own view of a section:
that the title's own views may change under it, and that the section's committed pages are not
lost when they do. Both were checked:

- A local Windows experiment (this development machine) maps two views of a `SEC_RESERVE`
  section, commits the pages through the first and queries the second: the commit is shared (both
  views report `MEM_COMMIT`), and after the committing view is unmapped the other still reports
  `MEM_COMMIT`. A section page's commit belongs to the section, not to a view.
- The port's WARP test now exercises it end to end: with a heap standing on our view, it makes one
  title view read-only, unmaps another, and the placed buffer still reads the section's pages
  through the heap; then it closes the section and the placed buffer still reads them. (The first
  version of the test died with an access violation - not in the placement code, but because its
  assertion compared the GPU's bytes against title memory it had just unmapped, which faults as it
  should. The bytes are saved before the unmap now. The fault was the test's; it is recorded here
  so the same "proof" is not attempted twice.)

### 17.2 What was run, and the results

GitHub Actions `windows-latest` (WARP), through temporary `ci/**` branches on the authorized
fork whose only extra file is the test workflow (the arrangement the earlier RAM/VRAM work used;
nothing is pushed to the NVIDIA target repository and no PR is opened):

| Run | Branch / commit | Result |
|---|---|---|
| Baseline (recorded by the earlier work) | `63b902f`, run 46122898345 | build check PASS; tests **452 passed, 1 failed, 36 ignored** |
| This branch, final | `ci/copy-bridges-final-fixed` (run 46139316939; source tip d04af6e) | build check PASS; tests **470 passed, 1 failed, 36 ignored** (the one failure is the baseline `depth_bounds_pipeline_loads_and_filters_draws`) |
| Intermediate runs | the same branch before the last two test fixes | the placement tests passed; two test-only defects were found and fixed: a startup-warning unit test still asserted the old `VICTORY_INPLACE` level semantics, and the new port test read unmapped title memory in an assertion (the access violation above) |

Local checks (Windows, Rust 1.98.1 x86_64-pc-windows-gnu, `CARGO_TARGET_DIR` outside the tree):

- `cargo check --workspace --all-targets --locked`: PASS.
- `cargo check --workspace --all-targets --all-features --locked`: PASS (the CI build check runs
  the same command).
- `cargo clippy -p victory-gpu -p victory-hle --all-targets --locked -- -D warnings`: the only
  failure is the pre-existing `clippy::manual_is_multiple_of` in `shadow.rs`, unchanged from the
  base; `cargo test` cannot link locally (GNU raw-dylib needs a working `dlltool`), so the tests
  run on CI as documented.

The source tip the run tested is `d04af6e`; the commits after it on this branch change
documentation and comments only (the phase records in sections 13-16 and the stale flight-gate
notes), so the run describes this tree's source.

New tests added by this project (all on the software adapter, all pass): the openability
predicate, the refcounted section view's last-holder unmap and section lifetime, the page-pool
placement (data both ways across two title views, a title view reprotected and unmapped under the
heap, the shared region and its forget, the section close), the placed-copy invalidation among
many copies, the placed-write declaration to a host list, the direct-backing accounting and
refusal reasons, and the fallback policy matrix.

### 17.3 What the results do not establish

Still requiring the target NVIDIA driver and hardware: opened-heap acceptance, physical residency
and pinning cost on that driver, coherency of GPU access to title system memory, PCIe cost against
the copies replaced, the openable share of the real title memory (the in-place census has never
been recorded on a run), WDDM paging, and device-removal/TDR behaviour. WARP is not equivalent
hardware, and `GetHeapProperties`/the WARP probe report the API's answer, not physical placement.
The switches remain off by default; sections 11 and 16.3 record what a hardware run must
establish.
