# System RAM and GPU VRAM separation: audit and ownership rules

This document maps which memory of the ported title lives in system RAM, which lives in the
host GPU's own memory, how the two domains are crossed, and which remaining paths still let
the GPU read or write system memory. It is the Phase 1 deliverable of the RAM/VRAM separation
work. Sections 1-6 are the evidence-based audit made before any allocation behaviour was
changed: every claim there was checked against the source at 63b902f and line numbers are of that
tree. Section 7 describes the changes made afterwards (references are to those commits) and
section 8 records the verification and its limits.

The project's rule: **system RAM holds CPU-owned state; GPU VRAM holds GPU-owned resources;
crossing the boundary goes through an explicit transfer.** Lower memory use or higher frame
rate do not by themselves show separation and are not claimed here.

The focused follow-up audit of the window/texture VRAM budget and of the separation exception
paths is [memory-separation-window-budget.md](memory-separation-window-budget.md).

## 1. The platform model this layer works in

The host is Windows with Direct3D 12 on a discrete GPU. The title is an Xbox Series title whose
GPU model is the console's: unified memory, so a title GPU address is a CPU address. The layer
emulates the title's D3D12 object model in CPU RAM (heaps, resources, descriptor and query heaps,
fences, pipeline states are host-side objects; see `crates/victory-gpu/src/resource.rs:1014-1183`,
`resource.rs:3334-3402`, `query.rs:35`, `fence.rs:47`). Only data the host GPU must actually read
or write is materialised as real D3D12 memory.

Two consequences shape everything below:

- Separation is a property of the **host copies** the layer makes of title memory, and of the
  **transfer buffers** between those copies and title RAM — not of the emulated guest objects.
- The title's authority over its memory is absolute: the CPU can write any title address at any
  time, and the title can map, unmap or reprotect its pages. Host copies are therefore mirrors
  that the layer keeps current (CPU→GPU) and commits back (GPU→CPU).

## 2. Heap classes actually used

| Class | D3D12 description | CPU maps | Used by |
|---|---|---|---|
| Device-local | `D3D12_HEAP_TYPE_DEFAULT` | no | host textures, render targets, depth and colour twins (`resource.rs:3052-3095`, `resource.rs:451-592`); pooled texture blocks (`pool.rs:826-850`); card side of two-sided title-buffer copies and their hash twins (`host.rs:10996-11041`, `shadow.rs:4664-4682`); indirect argument/scratch and clamp buffers; overlay/panel/compose/Streamline resources; reserved texture + tiles when `VICTORY_LEVELS_ON_DEMAND` is on (`resource.rs:3046-3060`, `tiles.rs:154-210`) |
| System memory, GPU-visible | `D3D12_HEAP_TYPE_CUSTOM`, CPU page WRITE_BACK, pool L0, UAV allowed, mapped | yes | **single-side** title-buffer and window copies the GPU reads/writes directly (`host.rs:10853-10909`, `shadow.rs:5482-5506`); the CPU staging side of two-sided copies; the GPU→CPU channels below; CPU snapshots are RAM-only |
| Staging (CPU→GPU) | `D3D12_HEAP_TYPE_UPLOAD` | yes (write-combine) | texture/list staging, per-submission send memory (`host.rs:9552-9605`, `host/send.rs:138-160`) |
| Readback (GPU→CPU) | `D3D12_HEAP_TYPE_READBACK` | yes | texture readback and `texback` (`host.rs:9513-9550`, `texback.rs:144-249`), deferred texture read-backs into title memory (`list.rs:1340-1374`); fetched card-copy pieces go to a shared system-memory buffer, not a READBACK heap (`host.rs:5579-5582`) |
| CPU-mapped VRAM | `D3D12_HEAP_TYPE_GPU_UPLOAD` (resizable BAR) | yes | opt-in only: `VICTORY_SEND_BAR`, `VICTORY_WINDOWS_BAR_MB`, `VICTORY_WC_BAR` (`host.rs:10937-10992`); the keep-awake buffer of `VICTORY_CARD_AWAKE` is a device-local 256-byte `DEFAULT` buffer, not CPU-mapped VRAM (`host/awake.rs:144-164`) |
| Opened title memory | `OpenExistingHeapFromAddress/Address1/FileMapping` | yes | opt-in only: `VICTORY_INPLACE` windows/buffers (`inplace.rs:116-168`) and the renderer port's constants heap (`port.rs:143-226`) |
| Driver-managed, opaque | descriptor heaps, command allocators/lists, root signatures, pipeline states, query heaps, swap chain | no API | placement is not stated to the application and cannot be classified from source |

## 3. Ownership classification

### 3.1 CPU-owned, stays in system RAM

- Title memory itself (the emulated heaps/resources created by the title) and the guest object
  tables — `resource.rs:1014-1183`.
- Title upload arenas (`upload.rs:1-14`), CPU snapshots of one-sided copies (`shadow.rs:2945-2975`),
  write-watch journals and guards (`watch.rs:1-27`).
- The CPU staging side of a two-sided copy (`Card::staging`, `shadow.rs:112-137`): the CPU fills and
  compares it, the GPU only ever reads it through a copy.

### 3.2 GPU-owned, device-local VRAM

- Every texture the GPU samples or renders to, and every render target and depth resource
  (`resource.rs:3020-3095`, `pool.rs:826-850`); the depth twins are GPU-written UAVs
  (`resource.rs:451-592`).
- The card side of two-sided title-buffer copies, including the hash twin the compare pass writes
  (`host.rs:10996-11041`, `shadow.rs:416-474`, `shadow.rs:4664-4682`).
- By-address shader memory: the directory copy and the arena copies the translated raw-address
  shaders read (`shadow.rs:5689-5735`, `memory.rs:1-67`).
- Indirect execution's clamped arguments and scratch (`indirect.rs:1-50`), and the overlay, menu,
  panel, compose and Streamline resources.

### 3.3 Cross-domain transfers (explicit by construction)

- CPU→GPU: write-watch/guards find title writes, the staging side is refreshed, and each submission
  takes the changed ranges into per-submission send memory before its lists run
  (`shadow.rs:4145-4181`, `host/send.rs:193-323`, `host.rs:4774-4923`). Texture data takes the same
  shape through UPLOAD staging (`texture.rs:2119-2192`).
- GPU→CPU: declared write ranges are hashed and compared on the card (two-sided copies,
  `host.rs:2461-2544`) or against CPU snapshots (one-sided copies, `shadow.rs:3502-3685`); differing
  pieces are fetched into a readback or shared buffer, waited for, merged into title memory and the
  copy's CPU side (`host.rs:5521-5648`, `shadow.rs:4775-4817`). CPU-read textures go through
  `texback` (`texback.rs:144-249`). Deferred texture read-backs are copied into title memory after
  their list ran (`host.rs:4365-4399`, `host.rs:7686-7687`).

### 3.4 Justified CPU-visible shared exceptions

- GPU→CPU channels the CPU polls once a frame, kept in small system-memory UAV buffers because a
  readback would add a submission and a fence wait each frame: the by-address faults buffer (16 KiB:
  `MEMORY_FAULTS` is 4096 dwords, `memory.rs:230`) and the card-compare results area (1 MiB,
  `host.rs:4728-4732`). The indirect work budgets are the other way round by default: the
  clamp atomics live in a 256-byte device-local `DEFAULT` buffer (`host.rs:7946-7953`;
  `VICTORY_INDIRECT_CARD_BUDGET` on) and only 16 bytes of notes are copied into a CPU-mapped
  buffer at list close (`host.rs:7989-8007`). `VICTORY_INDIRECT_CARD_BUDGET=0` is the
  opt-in that keeps those atomics in system memory.
- CPU-authored buffers the shaders read directly from UPLOAD memory, following D3D12's standard
  CPU-written staging pattern: the descriptor-stride table (an 8 MiB root SRV,
  `descriptor/host.rs:2056-2068`, `descriptor/host.rs:3479-3491`) and per-draw
  constant/rectangle data (`host.rs:7707-7755`). Copying them into VRAM would add a submission and
  a copy per draw. List upload data is UPLOAD memory on the default path; with
  `VICTORY_CARD_STEPS=2` (set by the launcher) it is the shared write-back buffer instead
  (`host.rs:7731-7733`), still CPU-authored data read by the GPU.
- `VICTORY_WC_NO_BACK` (set by the launcher, `apps/victory/src/play.rs:78-80`) deliberately leaves
  shader writes in large write-combined copies on the card instead of copying them back; RAM is
  stale for those ranges by design (`shadow.rs:2366-2410`).
- Opt-in shared paths, off by default and to be treated as experiments, not architecture:
  `VICTORY_INPLACE` and `VICTORY_PORT_CONSTANTS` (GPU reads/writes title memory directly),
  `VICTORY_SEND_BAR`, `VICTORY_WINDOWS_BAR_MB`, `VICTORY_WC_BAR` (CPU-mapped VRAM), and
  `VICTORY_CARD_ALL` (see 6.2).

### 3.5 Placement that cannot be established from source

- Driver-managed memory: descriptor heaps (up to 2^22 descriptors, `descriptor/host.rs:1918-2044`),
  command allocators/lists, root signatures, pipelines, query heaps, swap-chain buffers.
- Whether a `DEFAULT` resource is physically resident at a given moment: WDDM may page or demote
  it under pressure and D3D12 exposes no per-resource residency query. The source requests
  device-local memory; the API reports only budget and usage totals.
- The real behaviour of `GPU_UPLOAD` and of heaps opened on title memory on the target NVIDIA
  driver: GPU_UPLOAD is only feature-checked in source (`host.rs:10914-10932`) and the
  OpenExistingHeap rules were measured only against the software adapter
  (`inplace.rs:9-36`); no target-driver behaviour is established here.

## 4. How two-sided copies are admitted today

`shadow::lookup` makes a host copy on first use (`shadow.rs:5403-5530`). A copy becomes two-sided
when `card_for` admits it (`shadow.rs:386-474`):

- size at least `card_limits.0` (library default 16 MiB on local budgets up to 12 GiB, 4 MiB on
  16 GiB, 1 MiB on 20 GiB and more; `shadow.rs:193-207`), and below 1 GiB (the on-card compare
  counts pieces in 32 bits);
- the process's local budget use plus the copy stays under 80% (97% once the texture queue has kept
  the room), and the total of such copies stays under `card_limits.1` (18% of budget clamped
  512-2048 MiB, 2048 MiB, or a quarter of budget up to 8192 MiB);
- the host compare shader can be created (`Host::card_ready`, `host.rs:4688-4703`).

Refusals keep the copy single-sided in system memory. Windows outside buffers get a VRAM side
only at or above an 8 GiB local budget (9% of it, at most 1 GiB; `shadow.rs:496-518`) and fall back
the same way. By-address arena memory gets a VRAM side while under `VICTORY_MEMORY_CARD_MB`
(default 4096 MiB) and 97% of budget (`shadow.rs:5689-5713`).

The shipped launcher passes `VICTORY_CARD_BUFFERS_MIN_KB=64` (`apps/victory/src/play.rs:66`), so in
the product path every title-buffer copy asks for a VRAM side first and falls back to system
memory only when the checks above refuse it. The library default stays conservative because tests
and tools run without the launcher's environment, and the effect of the wider policy has not been
measured on target hardware.

## 5. Transfer and synchronization contract (as implemented)

- Refresh of a copy happens only during preparation of a submission, on the submitting thread;
  ranges a shader may write are excluded until every declaring flight has been copied back
  (`shadow.rs:4145-4181`, `shadow.rs:1022-1052`, `shadow.rs:3824-3860`).
- Sends: changed ranges are snapshotted out of staging into per-submission memory
  (`VICTORY_SEND_OWN`, on by default), recorded in a sync list executed immediately before that
  submission's lists, and numbered so a fetch can tell which sends the card had received
  (`host/send.rs:193-323`, `host.rs:4787-4926`, `shadow.rs:648-664`).
- Fetches run only after the flight's fence is reached, behind the sends they must see, and are
  merged under the copy's merging lock and the landing order (`host.rs:2976-2988`, `host.rs:5376-5511`,
  `shadow.rs:4945-4981`).
- Objects a title releases are retired behind in-flight submissions (`host.rs:474-531`); lists the
  GPU still uses are recycled only after landing.
- Device removal is detected at submission, late-wait and relay points and ends the run with a
  crash report (`host.rs:797-834`); it is not recovered from.

## 6. Separation gaps found by the Phase 1 audit, and their status at HEAD

6.1 **Single-side copies.** A title buffer the checks above refuse, a window whose card side would
go past the window budget (on a card below the 8 GiB local-budget threshold that is every window),
and any copy at or above 1 GiB stay as one CUSTOM/WRITE_BACK/L0 buffer in system memory
that shaders read and write over PCIe (`shadow.rs:5482-5506`, `shadow.rs:5790-5843`). Refusals are
permanent: a copy is asked for once and a refused one is not retried when room appears
(`shadow.rs:5486-5506`; `CARD_REFUSED`, `shadow.rs:301-321`). This is the main remaining default
dependency on system-memory-backed GPU resources; it is bounded by the VRAM budget, so resolving
it fully is a budget/priority decision, not an allocation-type fix.

6.2 **"Two-sided" is not necessarily device-local.** With `VICTORY_CARD_ALL=1` a refused copy gets
a second system-memory buffer as its "card side", counted in `CARD_BYTES` as card memory
(`shadow.rs:386-399`). The switch is off by default and is not a separation policy.

6.3 **By-address arena accounting defect. Fixed in 7.2 and 7.4.** In `shadow::by_address` the
lifetime cap counter was incremented before the size check (`shadow.rs:5705` in the audit), so
refusals grew it too, and it was never decremented when a copy went (`Shadow::drop`,
`shadow.rs:2909-2938` decremented the window counter instead). After enough raw-address memory had
streamed, new arenas silently fell back to system memory for the rest of the run although VRAM was
free. The fallback buffer was also counted as window card memory (`shadow.rs:5716-5717`), so it
consumed window VRAM budget; 7.4 keeps it out of that counter.

6.4 **Fallbacks are not classified or reported. Addressed in 7.1 for the copy machinery's
buffers.** Before 7.1 every refusal returned `None` or a system-memory copy with at most a warn
log; nothing counted bytes by heap class or by fallback reason, and
`GetHeapProperties`/`GetHeapDesc` were never called (`host.rs:10753-11042` reported only DXGI
totals). The ledger now reports the API heap class of the buffers it makes and every live fallback
copy by reason. Still open: textures, render targets, depth resources, descriptor heaps and the
swap chain are not in that ledger, and `MADE[4]/[5]` still conflates CPU-mapped VRAM with
device-local buffers (`host.rs:10941-10942`/`host.rs:10997-10998` in the audit; line numbers have
shifted).

6.5 **GPU→CPU channels in system memory. Partly fixed by 7.1's placement ledger; the indirect
budget's default was corrected in the Phase 2 follow-up.** The faults buffer and the card-compare
results of 3.4 are GPU-written system memory (16 KiB and 1 MiB). They are small and one-way, and
their design trade is explicit: moving them to VRAM would add a submission and a fence wait per
frame. The indirect budgets are device-local by default (3.4); only
`VICTORY_INDIRECT_CARD_BUDGET=0` moves their clamp atomics to system memory, and the startup
line of the final Phase 2 commit names that switch.

6.6 **Device-removal waits. Addressed in 7.3 for the three named sites.** `card_back`,
`relay_card_back` and `settle` waited with `WaitForSingleObject(INFINITE)` without re-checking
the completed value the way `wait_host_fence` does (`host.rs:5576-5582`, `host.rs:5267-5291`,
`host.rs:5856-5862`, `host.rs:3765-3782` in the audit), so a removed device could produce a stale
merge before the run was failed. They now re-check the fence after the wait (7.3). Whether a
removed device signals the event at all remains driver behaviour and needs hardware removal/TDR
evidence.

6.7 **No VRAM admission for render targets, depth and twins.** Only the index-picked texture
pass has a room test (`draw.rs:3085-3094`); render targets, depth planes, colour twins and a
texture's first host copy are made without one (`resource.rs:1896-1979` admits targets with no
room test). The layer therefore relies on WDDM to keep within budget; if the process overshoots,
the system pages device-local resources out and the GPU silently reads them from system memory.
`GetHeapProperties` cannot show that: the requested heap stays DEFAULT.

6.8 **Refusals latch for the run.** A host texture whose creation or residency step failed once is
marked refused and never retried (`resource.rs:1708-1773`, `resource.rs:3082-3089`), so one
transient `E_OUTOFMEMORY` can leave a texture black for the whole session.

6.9 **Unverifiable placement.** See 3.5: driver-managed memory, WDDM residency of DEFAULT
resources, and driver behaviour of the opt-in shared paths can only be bounded, not proven, from
source.

6.10 **Documentation drift. Fixed in the Phase 2 follow-up commit.** The window-budget default
("0, the default, none" vs budget-derived), the relay default ("off by default" vs on outside
tests) and `Card::before` (described as a byte twin where the code keeps 16 bytes of hashes per
64 KiB piece) were corrected with the code, together with the in-place and window accounting
comments.

## 7. Minimum safe changes made on this branch

Phase 2 keeps every admission rule, size threshold, budget and render path as it was. It adds
measurement, corrects two accounting defects, and re-checks three copy-back waits:

- the placement ledger (7.1) only counts and prints; it changes no allocation;
- the by-address lifetime cap (7.2) and the window VRAM counter (7.4) are accounting fixes: 7.2
  does change by-address placement after releases (the intended repair), and 7.4 stops a
  system-memory fallback from consuming window VRAM budget;
- the checked waits (7.3) are a drive-by correctness fix in the copy-back path, not part of the
  separation design; they are kept because they are small and strictly narrow the failure window.
- no rendering path changed.

### 7.1 Placement ledger (no behaviour change)

`crates/victory-gpu/src/residency.rs` gained a read-only ledger:

- host buffers made by the five constructors of the copy and transfer machinery
  (`upload_buffer`, `read_back_buffer`, `shared_buffer_now`, `mapped_card_buffer`,
  `uav_buffer` in `host.rs`) are classified from the heap
  `ID3D12Resource::GetHeapProperties` reports — the API's creation-time heap class, not a
  parallel table of call-site assumptions and not physical residency — and counted with their
  bytes (`HeapClass`: device-local, CPU-visible, upload, readback, CPU-mapped VRAM, other).
  Buffers created directly are not in this ledger (overlay panel/menu, keep-awake, the
  upload-mapping helper's two 256-byte buffers), and neither are textures, render targets, depth
  resources, descriptor heaps or the swap chain;
- every copy the copy machinery builds over a host copy that has no device-local side is counted
  while it lives and when it goes, with the reason: budget line, byte cap, allocation failure,
  host compare unavailable, not eligible (size thresholds / 1 GiB), switch off, or a view's own
  bytes (`ViewCopy`). Opt-in placed copies (`VICTORY_INPLACE`, `VICTORY_PORT_CONSTANTS`) are
  the title's memory itself rather than host copies and are not in this count;
- one line every five seconds reports the host buffers the copy and transfer machinery made
  since the start by API-reported class, the copies currently living in system memory with their
  reasons, and the DXGI LOCAL and NON_LOCAL budget/usage groups. The class figures are cumulative
  creation totals, not live bytes, and cover buffers only: textures, render targets, depth
  resources, descriptor heaps and the swap chain are not in this ledger. `VICTORY_MEMORY_REPORT=1`
  forces the line when no fallback is alive; `=0` silences it. Only the live fallback bytes and the
  existing live card-side counters (`shadow::card_stats`, `live_span_bytes`) can be read against
  DXGI usage; the class totals cannot. Nothing here is a per-resource residency query: the heap
  class and DXGI usage are the API's report, not proof of physical placement.

This is the measurement base for Phase 3; it is why the code now says which side a copy has.

### 7.2 By-address lifetime cap (verified defect fixed)

`shadow::by_address` used to add the requested bytes to its lifetime cap before testing the cap
(`shadow.rs:5705` in the audit), so refusals grew the counter, and the counter was never given
back when a copy went: after enough raw-address memory had streamed, new arenas fell back to
system memory for the rest of the run although VRAM was free. Now the cap is reserved atomically
only when the whole size fits and given back by `Shadow::drop`, and the copy's fallback reason is
counted. This is the one place where a real separation loss is repaired rather than measured. The
failure also no longer consumes the window VRAM budget: the window counter is charged only for a
copy whose side is device-local (7.4).

### 7.3 Device-removal waits (correctness)

`Host::card_back`, `Host::relay_card_back` and the settle path waited with
`WaitForSingleObject(INFINITE)` and read the fetched bytes without re-checking the fence's
completed value. They now use a checked wait like `wait_host_fence`: a wait that did not complete,
or a fence that did not reach its target (a removed device answers `u64::MAX`), fails the run
instead of merging stale bytes. The wait itself is still `WaitForSingleObject(INFINITE)`:
whether a removed device signals the event (rather than leaving the wait blocked) is driver
behaviour, so this narrows the failure window but is not proof that a removal cannot hang. That
needs a device-removal/TDR run on hardware.

### 7.4 Window VRAM budget accounting (accounting fix)

`WINDOW_CARD_BYTES` is the counter that limits the card sides of windows outside buffers
(`VICTORY_CARD_WINDOWS_MB`, or 9% of a local budget of 8 GiB and more) and that
`shadow::card_stats`/`live_span_bytes` report as window VRAM. `shadow::by_address` used to add
every arena to it, including arenas whose GPU-visible side is the system-memory fallback (6.3),
so a fallback consumed window VRAM budget and could refuse later windows the VRAM side they would
otherwise get. The counter is now charged only when the copy's side is device-local and is given
back by `Shadow::drop` through a per-copy `window_card` field; a window whose
`VICTORY_WINDOWS_OWN` second side is system memory no longer counts either. No threshold, cap
or default was changed; the effect is that a refused or fallback arena no longer holds window
budget it never used, so a later window can get the VRAM side it was previously refused. That is
an accounting correction, and it should be measured on a real card before any budget is tuned
from it.

### 7.5 Deliberately not changed

The size thresholds and maxima of two-sided copies (6.1/6.2) are a VRAM budget policy and the
product path already asks for VRAM on every copy (the launcher sets
`VICTORY_CARD_BUFFERS_MIN_KB=64`); their effect on target hardware is unmeasured here, and
`VICTORY_CARD_ALL` remains off. The window default below an 8 GiB budget stays 0 because window
VRAM is not reserved in the texture queue's accounting, so changing it without hardware would risk
texture starvation. The GPU→CPU channels of 6.5 (the faults buffer and the compare results) stay in system memory
(small, one-way, polled once a frame) and are documented as the justified exception; the audit did
not establish that moving them to VRAM plus a readback is safe without hardware. The indirect
budget is device-local by default and its weakening switch is named at startup. No room test is added for render targets,
depth and twins (6.7): refusing them changes what the title renders; whether the whole process (textures, render
targets and driver memory included) stays inside its budget is observable through DXGI usage and,
on hardware, GPUView/ETW, not from this ledger's class totals. No Evict,
SetResidencyPriority or reservation API is added: there is no evidence of OS paging to act on, and
evicting resources a submission may name is a known device-loss hazard (6.9 stays in place).

## 8. What cannot be established in this environment

This work was audited on a VM without a discrete NVIDIA GPU. WARP can run the unit/device tests
but is not equivalent hardware. Therefore: physical residency of any DEFAULT resource, WDDM
paging under pressure, the real cost of the PCIe fallback, and the correctness/perf effect of
changing the copy budget are not established here. `GetHeapProperties` proves the heap class the
API reports for a resource, not its current physical location.

### 8.1 Checks run

Environment: a VMware VM without a discrete NVIDIA GPU. Local checks used Rust 1.98.1
(`x86_64-pc-windows-gnu`); the tests ran on GitHub Actions `windows-latest`, whose adapter is
WARP. The run is of the branch's source tip (the Phase 2 commit; the temporary CI branch adds
only the test workflow file).

| Check | Baseline 63b902f | This branch |
|---|---|---|
| `cargo check --workspace --all-targets --locked` | PASS | PASS |
| `cargo check --workspace --all-targets --all-features --locked` | PASS | PASS |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | FAIL: pre-existing `clippy::manual_is_multiple_of` in `shadow.rs` (`turns % 16 == 0`) | FAIL: the same pre-existing lint only; the changed lines add no warning |
| `cargo test` locally | not runnable: GNU raw-dylib linking needs a working `dlltool`, absent on this host | same |
| GitHub Actions: `cargo check --workspace --all-targets --all-features` | PASS | PASS |
| GitHub Actions: `cargo test --workspace` | **452 passed, 1 failed, 36 ignored** (run 46122898345 on the temporary `ci/probe-memory` branch) | **460 passed, 1 failed, 36 ignored** (the run of this branch's source tip; its run number is recorded in the pull request) |

The one failure is identical at baseline and at HEAD:
`pipeline::host::tests::depth_bounds_pipeline_loads_and_filters_draws` fails with
`depth 0.5 outside [0.6, 1.0] must be rejected` because WARP does not implement depth bounds.
The eight tests added on this branch all pass on WARP (452 + 8 = 460):
`host::placement_tests::host_buffers_land_in_the_heap_the_api_reports`,
`host::separation_switch_tests::inplace_warnings_use_the_parsed_level`,
`residency::tests::heap_classes_come_from_the_heap_the_api_reports`,
`residency::tests::fallback_bytes_are_printed_in_mib`,
`residency::tests::a_fallback_copy_is_counted_while_it_lives_and_after`,
`shadow::window_tests::the_by_address_cap_takes_only_what_fits`,
`shadow::window_tests::the_by_address_cap_is_given_back_when_a_copy_goes`,
`shadow::window_tests::a_window_side_is_given_back_when_the_copy_goes`.

The CI runs used temporary `ci/**` branches in the authorized fork whose only difference from
the feature branch is the test workflow file; that workflow is not part of this branch or of the
pull request. Independent reviews of the complete diff (code, claims, window budget, exception
paths and a final pass over the six review dimensions) found no blockers; their confirmed majors
- the startup warning comparing INPLACE spellings instead of the parsed level, and the indirect
budget heap class in sections 3.4/6.5 - are fixed in the final Phase 2 commit.

### 8.2 What the checks prove, and what they do not

Proven here: the source's allocation and transfer paths compile in both feature configurations;
the API reports the intended creation heap class for every buffer made through the five
instrumented constructors (asserted through `GetHeapProperties` on a real D3D12 device, WARP;
buffers created directly are listed in 7.1 and not tracked); the accounting
units and release paths behave as their tests say; the workspace test suite has no new failure
against the recorded baseline; and no default allocation policy (thresholds, caps, window
defaults, `VICTORY_CARD_ALL`) changed.

Not proven here, and requiring real NVIDIA hardware: whether a `DEFAULT` resource is
physically resident at a given moment (WDDM may page it); the actual PCIe cost of the copies
that remain in system memory; how the DXGI budget's movement affects the once-computed caps;
the target driver's GPU_UPLOAD (resizable BAR) and opened-heap behaviour; the torn-fetch rate
when the copy-queue fetch runs without its check; and device-removal wait behaviour. WARP is
not equivalent hardware and `GetHeapProperties` reports the heap a resource was created in,
not where its pages currently are.

A hardware validation session should run with
`victory play --set VICTORY_MEMORY_REPORT=1 --set VICTORY_LOG=info`, read the
`memory placement` line against DXGI local/non-local usage over the world-load burst and a long
gameplay segment, and capture GPUView/ETW (`DxgKrnl`) or NSight Graphics residency events; the
in-place driver questions are answered by `VICTORY_INPLACE_PROBE`/the in-place census. The
concrete runs and the falsification table are in
[memory-separation-window-budget.md](memory-separation-window-budget.md) sections 5-7.