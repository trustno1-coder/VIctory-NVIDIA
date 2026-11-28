# System RAM and GPU VRAM separation: audit and ownership rules

This document maps which memory of the ported title lives in system RAM, which lives in the
host GPU's own memory, how the two domains are crossed, and which remaining paths still let
the GPU read or write system memory. It is the Phase 1 deliverable of the RAM/VRAM separation
work: an evidence-based audit made before any allocation behaviour was changed. Every claim
below was checked against the source at 63b902f; line numbers are of that tree.

The project's rule: **system RAM holds CPU-owned state; GPU VRAM holds GPU-owned resources;
crossing the boundary goes through an explicit transfer.** Lower memory use or higher frame
rate do not by themselves show separation and are not claimed here.

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
| Readback (GPU→CPU) | `D3D12_HEAP_TYPE_READBACK` | yes | texture readback and `texback` (`host.rs:9513-9550`, `texback.rs:144-249`), fetched card-copy pieces (`host.rs:5557-5560`), deferred query readbacks (`list.rs:1341-1358`) |
| CPU-mapped VRAM | `D3D12_HEAP_TYPE_GPU_UPLOAD` (resizable BAR) | yes | opt-in only: `VICTORY_SEND_BAR`, `VICTORY_WINDOWS_BAR_MB`, `VICTORY_WC_BAR`; `VICTORY_CARD_AWAKE` keep-awake buffer (`host.rs:10937-10992`, `host/awake.rs:144-164`) |
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
  `texback` (`texback.rs:144-249`). Deferred query values are copied into title memory after their
  list ran (`host.rs:4365-4399`).

### 3.4 Justified CPU-visible shared exceptions

- GPU→CPU channels the CPU polls once a frame, kept in small system-memory UAV buffers because a
  readback would add a submission and a fence wait each frame: the by-address faults buffer (8 KiB,
  `memory.rs:230`), the card-compare results area (1 MiB, `host.rs:4728-4732`), and the indirect
  work budgets (256-byte slots, `host.rs:2054-2112`).
- Small CPU-authored buffers the shaders read directly from UPLOAD memory: the shared
  descriptor-stride table (an 8 MiB root SRV, `descriptor/host.rs:2056-2068`, `descriptor/host.rs:3479-3491`),
  per-draw constant/rectangle data (`host.rs:7707-7755`), and list upload data (`host.rs:7695-7705`).
  These follow D3D12's standard CPU-written staging pattern; copying them into VRAM would add a
  submission and a copy per draw, so the transfer is the direct read of the upload heap instead.
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
  driver — only a feature check exists in source (`host.rs:10914-10932`, `inplace.rs:13-36`).

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

## 6. Remaining separation gaps (verified)

6.1 **Single-side copies.** A title buffer the checks above refuse, a window below the window
budget, and any copy at or above 1 GiB stay as one CUSTOM/WRITE_BACK/L0 buffer in system memory
that shaders read and write over PCIe (`shadow.rs:5482-5506`, `shadow.rs:5790-5843`). Refusals are
permanent: a copy is asked for once and a refused one is not retried when room appears
(`shadow.rs:5486-5506`; `CARD_REFUSED`, `shadow.rs:301-321`). This is the main remaining default
dependency on system-memory-backed GPU resources; it is bounded by the VRAM budget, so resolving
it fully is a budget/priority decision, not an allocation-type fix.

6.2 **"Two-sided" is not necessarily device-local.** With `VICTORY_CARD_ALL=1` a refused copy gets
a second system-memory buffer as its "card side", counted in `CARD_BYTES` as card memory
(`shadow.rs:386-399`). The switch is off by default and is not a separation policy.

6.3 **By-address arena accounting defect.** In `shadow::by_address` the lifetime cap counter is
incremented before the size check (`shadow.rs:5705`), so refusals grow it too, and it is never
decremented when a copy goes (`Shadow::drop`, `shadow.rs:2909-2938` decrements the window counter
instead). After enough raw-address memory has been streamed, new arenas silently fall back to
system memory for the rest of the run although VRAM is free. The fallback buffer is also counted
as card memory (`shadow.rs:5716-5717`).

6.4 **Fallbacks are not classified or reported.** Every refusal returns `None` or a system-memory
copy with at most a warn log; nothing counts bytes by heap class or by fallback reason, and
`GetHeapProperties`/`GetHeapDesc` are never called (`host.rs:10753-11042` reports only DXGI
totals). `MADE[4]/[5]` even conflates CPU-mapped VRAM with device-local buffers
(`host.rs:10941-10942`, `host.rs:10997-10998`).

6.5 **GPU→CPU channels in system memory.** The faults buffer, compare results and indirect budgets
of 3.4 are GPU-written system memory. They are small and one-way and their design trade is
explicit, but they are unreported and were kept as they are: moving them to VRAM would have to
add a submission and a fence wait per frame, and the audit did not establish that as safe without
hardware.

6.6 **Device-removal waits.** `card_back`, `relay_card_back` and `settle` wait with
`WaitForSingleObject(INFINITE)` without re-checking the completed value the way `wait_host_fence`
does (`host.rs:5576-5582`, `host.rs:5267-5291`, `host.rs:5856-5862`, `host.rs:3765-3782`), so a
removed device can produce a stale merge or a hang before the run is failed.

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

6.10 **Documentation drift.** Several comments state the opposite of the code: the window-budget
default ("0, the default, none" vs budget-derived, `shadow.rs:476-477` against `shadow.rs:495-518`),
the relay default ("off by default" vs on outside tests, `shadow.rs:966-969` against
`shadow.rs:982-990`), and `Card::before` described as a byte twin where the code allocates 16
bytes of hashes per 64 KiB piece (`shadow.rs:110-121` against `shadow.rs:4664-4682`).

## 7. Minimum safe changes made on this branch

Phase 2 keeps the existing allocation behaviour and adds only what the audit shows can be changed
safely:

1. Placement accounting and a placement report (no behaviour change): every buffer the layer
   creates is classified by the heap the API reports (`GetHeapProperties`), the system-memory
   fallbacks of the copy machinery are counted by reason, and one periodic line reports
   device-local vs fallback bytes against the DXGI budget/usage groups.
2. The by-address lifetime-cap defect (6.3): the reservation is taken only when it fits, given
   back when the copy goes, and a copy whose VRAM side was refused is no longer counted as card
   memory.
3. The device-removal fence waits (6.6) are re-checked like `wait_host_fence`.

Deliberately not changed, with reasons: the size thresholds and maxima of two-sided copies
(6.1/6.2) are a VRAM budget policy and the product path already asks for VRAM on every copy
(launcher sets `VICTORY_CARD_BUFFERS_MIN_KB=64`); their effect on target hardware is unmeasured
here, and `VICTORY_CARD_ALL` remains off. The window default below an 8 GiB budget stays 0
because window VRAM is not reserved in the texture queue's accounting, so changing it without
hardware would risk texture starvation. The GPU→CPU channels of 6.5 stay in system memory (small,
one-way, polled once a frame) and are documented as the justified exception. No room test is added
for render targets, depth and twins (6.7): refusing them changes what the title renders and needs
hardware measurement; the placement report's tracked-vs-DXGI reconciliation is what will show
whether the process overshoots its budget. No Evict,
SetResidencyPriority or reservation API is added: there is no evidence of OS paging to act on,
and evicting resources a submission may name is a known device-loss hazard.

## 8. What cannot be established in this environment

This work was audited on a VM without a discrete NVIDIA GPU. WARP can run the unit/device tests
but is not equivalent hardware. Therefore: physical residency of any DEFAULT resource, WDDM
paging under pressure, the real cost of the PCIe fallback, and the correctness/perf effect of
changing the copy budget are not established here. `GetHeapProperties` proves the heap class the
API reports for a resource, not its current physical location.

Phase 3 verification plan: local `cargo check` (default and `--all-features`), `clippy` compared
with the baseline, the workspace tests through the fork's GitHub Actions workflow on
windows-latest, and — on a real NVIDIA machine — the placement report over a full session with
`VICTORY_MEMORY_REPORT=1`, compared with DXGI local/non-local usage and with GPUView/ETW or
NSight Graphics residency events.