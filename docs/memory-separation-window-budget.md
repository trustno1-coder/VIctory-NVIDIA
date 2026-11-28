# Window buffers and the VRAM budget, and the separation exceptions

This is the supplementary report of the RAM/VRAM separation work: a focused audit of how the
windows of title memory outside buffers share the VRAM budget with textures, render targets,
depth resources and the other GPU-local allocations, and of the paths and switches that
deliberately weaken separation. It records what was verified, the minimal fixes made, what
stayed unchanged, and what only NVIDIA hardware can establish. It is evidence, not a claim
that physical separation has been proven: see section 6.

The main document is [memory-separation.md](memory-separation.md); the Phase 1 audits behind
both are the reports listed there.

## 1. Executive summary

- **Windows cannot starve textures, render targets or depth resources by themselves.** A
  window's device-local side is hard-capped (9% of a local budget of 8 GiB and more, at most
  1024 MiB; nothing below 8 GiB), each allocation is checked against the live DXGI local usage
  (80% line), and the texture queue's own line sits higher (about 90-92.5% of the budget, with
  a floor of min(2 GiB, budget/4) that is always admitted). Windows are in fact the consumer
  most often refused: once refused, a window keeps its system-memory copy for the run.
- **The real budget gaps are elsewhere**, and are documented rather than fixed: render targets,
  depth resources and depth twins have no admission test (the layer relies on WDDM to keep the
  process in budget); title-buffer card copies are admitted up to 97% once their room has been
  kept and a refusal is permanent; by-address arenas may hold up to 4 GiB at 97%; a host
  texture whose creation or residency failed once is refused for the rest of the run.
- **Three accounting defects were confirmed and fixed.** A window whose `VICTORY_WINDOWS_OWN`
  second side is system memory was still charged to the windows' device-local cap and never
  released; by-address arenas were charged to that same cap although they are admitted by
  their own lifetime cap; and a by-address fallback/refusal could consume the window budget.
  No threshold, cap or default changed.
- **Observability was added**: windows turned down are counted and logged, the texture queue's
  room line prints the alive device-local bytes of windows and by-address arenas beside its own
  weighing (not in it), live VRAM copies and system-memory fallback copies are reported
  separately under host-memory pressure, and one startup line names the separation-relevant
  switches this run has set and warns for the documented unsafe/weakening combinations.
- **Unresolved risks**: whether WDDM pages DEFAULT memory under pressure, the real cost of the
  PCIe fallback, the target driver's behaviour of GPU_UPLOAD (resizable BAR) and of heaps opened
  on title memory, broken pieces from the copy-queue fetch if its check is off, and the
  in-place/port paths under submissions in the air — all need NVIDIA hardware (section 6).

## 2. Budget coordination findings

### 2.1 How a window is admitted and accounted

`shadow::window_card_for` → `window_room_for` (crates/victory-gpu/src/shadow.rs):

1. `cards()` must be on and the window budget (`window_card_most`, from
   `VICTORY_CARD_WINDOWS_MB` or `default_window_card_mb`) must be non-zero;
2. the host compare machinery must be ready (`Host::card_ready`);
3. live DXGI local usage plus the window must stay under 80% of the reported local budget;
4. the windows' device-local counter plus the window must stay under the window cap;
5. the allocation is a device-local `DEFAULT`-heap buffer (`Host::uav_buffer`), rolled back
   from the counter on failure.

A refusal is final for that window: it becomes a single CARD/WRITE_BACK/L0 buffer in system
memory that shaders read and write over PCIe (the separation fallback), or, with
`VICTORY_WINDOWS_OWN=1`, a second system-memory side. The lookup that made the window is not
asked again. Windows are created during the world-load burst and on demand.

### 2.2 Budget sources, and which are live

| Source | Used by | Live? |
|---|---|---|
| DXGI local `Budget`/`CurrentUsage` (`Host::video_memory`) | the 80% window line, the 80/97% card-copy line, the texture queue's room line | queried per check (budget capped once by `VICTORY_VRAM_BUDGET_MB`) |
| DXGI non-local budget/usage | reported only; never used for admission | reported per query |
| `WINDOW_CARD_BYTES` (windows' device-local bytes) | the window cap | live atomic |
| `WINDOW_MAPPED_BYTES` (CPU-mapped windows) | `VICTORY_WINDOWS_BAR_MB` cap | live atomic |
| `CARD_BYTES[0]` and `CARD_BYTES[1]` (title-buffer card sides, hash twins) | `card_limits` cap; hash twins count into the statistics | live atomics |
| `ADDRESS_CARD_USED` (by-address device-local bytes) | `VICTORY_MEMORY_CARD_MB` lifetime cap | live atomic |
| `window_card_most`, `card_limits`, `VICTORY_MEMORY_CARD_MB` | hard caps, computed once from the budget at first use | **OnceLock: stale if the budget moves** (the host logs moves of 256 MiB and more) |
| texture queue `room_line`, `texture_floor_bytes` | texture admission | line from live budget |

The DXGI budget moves in practice (the source records a fall from 11,242 MB to about
10,300 MB on the first world frame). The hard caps keep their initial value for the run; no
code re-evaluates them. That is a known limitation, not changed here (re-partitioning a moved
budget is a policy decision that needs measurement).

### 2.3 Do windows compete with textures?

Yes, for bytes, but inside an admission line: a window allocation raises DXGI `CurrentUsage`,
which the texture queue weighs with its own reserve terms (free texture blocks, room kept for
card copies of title buffers, room kept for render targets). Windows are not among the reserves
(`resource::room` does not add `WINDOW_CARD_BYTES` or `WINDOW_MAPPED_BYTES`), so a window made
after the queue has filled the card to its line can push usage past that line and make new
textures wait or be refused. The reverse is bounded: windows have a hard cap and their
per-allocation 80% line.

The mechanism by which window growth could degrade rendering is therefore "textures wait for
room", not "windows evict live textures": the layer never evicts, and a refused host texture is
permanent for the run. That is why this work did not raise or lower the window budget: any
change there trades window VRAM for texture VRAM and needs hardware measurement.

### 2.4 Failure, budget change, residency and device removal

- **Allocation failure**: the window falls back to system memory (counted in the placement
  ledger), no state is corrupted; the window counter is rolled back.
- **Budget change**: the window's own admission uses the live usage but the cap is from the
  first reading; a budget fall does not re-size it.
- **Residency failure** (MakeResident/mapped in `Host::uav_buffer`): the allocation returns
  `None`, same fallback.
- **Device removal**: fatal by design (`host::fail`), with the crash report; there is no
  recovery path.
- **Recovery from a refusal**: not supported; only letting the window go and asking again (the
  runtime lets idle windows go) can try again.

### 2.5 Counter defects found and fixed

1. `VICTORY_WINDOWS_OWN`'s second system-memory side was added to `WINDOW_CARD_BYTES` while
   the copy's release path did not balance that add: a one-sided add (fixed: the counter now
   holds only device-local bytes and each copy remembers what it added).
2. `shadow::by_address` added every arena to `WINDOW_CARD_BYTES` (a by-address fallback
   consumed the window budget; fixed in b212d4a; now removed entirely in e221cad).
3. By-address device-local arenas were charged to the windows' cap although admitted by
   `ADDRESS_CARD_USED`: enough of them (about 1 GiB) could `Cap`-refuse every later window.
   Fixed: by-address bytes count only against their own cap, and `live_span_bytes`/the card
   statistics include them so the reported total stays complete.
4. Windows refused were silent; they are now counted and logged, like `CARD_REFUSED`.
5. `MADE[4]/[5]` still conflates CPU-mapped VRAM with device-local buffers and counts failed
   allocations; it is not used for admission, so it is left and documented (the placement
   ledger separates the classes from the API's answer).
6. `live_span_bytes` mixed VRAM and system memory in the host-pressure warning; the warning now
   reports VRAM copies and live system-memory fallbacks separately. Still inside the "VRAM" figure
   under two opt-ins that are off by default: `VICTORY_CARD_ALL`'s system-memory second side (it is
   counted in `CARD_BYTES[0]`) and `VICTORY_WC_BAR`'s CPU-mapped bytes; the placement line is the
   authoritative heap-class split in those runs.

## 3. Memory-separation exception matrix

Every path below was verified in source at HEAD; "class" follows the Phase 1 taxonomy.

| Path / switch | Default | Affected resources | Mechanism / sync | Risk | Class | Disposition |
|---|---|---|---|---|---|---|
| `VICTORY_SEND_BAR` | off | per-submission send memory | CPU-mapped VRAM, CPU writes at preparation; per-submission owned | CPU write cost; in-flight CPU/GPU write rules | optional optimization | keep off; documented |
| `VICTORY_WINDOWS_BAR_MB` | 0 | windows | CPU-mapped VRAM, no second side, no fence of ours | same | optional experiment | keep off; documented |
| `VICTORY_WC_BAR` | off | write-combined title buffers | CPU-mapped VRAM, shader writes stay on card | same, plus stale CPU reads | optional experiment | keep off; documented |
| `VICTORY_CARD_AWAKE` | off (launcher: 1) | 256-byte DEFAULT buffer + keep-alive dispatch | own queue/fence; re-checks removal | driver keep-alive only | optional optimization | unchanged |
| `VICTORY_INPLACE` 1 | off | windows of title memory | heap opened on title pages, no copies, no tracking | title cannot free/protect those pages | compatibility exception | keep off; startup warning |
| `VICTORY_INPLACE` 2 | off | title buffers (but "big" ones stay in the card path) | as above; unsupported with submissions in the air | stale/lost data when late | compatibility exception, **unsafe combination** | keep off; startup warning for the combination; with the launcher's MIN_KB=64 it places windows only |
| `VICTORY_PORT_CONSTANTS` | off | title 32-bit graphics heap | our section view opened as a heap | same as in-place; no tracking by design | compatibility exception | keep off; startup warning |
| `VICTORY_CARD_ALL` | off | every title-buffer copy | two sides, the refused side in system memory | not device-local; duplicates RAM; competes for the card budget | legacy experiment | keep off; startup warning |
| `VICTORY_WINDOWS_OWN` | off | windows | second system-memory side | not device-local | compatibility exception | keep off; startup warning |
| `VICTORY_CARD_BUFFERS` | on | title buffers | two-sided copies (the main separation path) | budget-limited refusals are permanent | architecture | unchanged |
| `VICTORY_CARD_BUFFERS_MIN_KB` | unset (launcher: 64) | all title buffers | eligibility floor only | first-come budget, no priority | policy | unchanged |
| `VICTORY_CARD_WINDOWS_MB` | budget-derived | windows | the window cap | see 2.3 | policy | unchanged |
| `VICTORY_MEMORY_CARD_MB` | 4096 | by-address arenas | lifetime cap, live | pressure fallback | policy | unchanged; accounting fixed |
| `VICTORY_RELAY_COPY` + `VICTORY_FETCH_CHECK` | copy=launcher on, check off | fetched pieces | copy-queue fetch | torn pieces possible without the check | correctness combination | startup warning; unchanged |
| `VICTORY_SEND_OWN` | on | send memory | per-submission snapshot of staging | off: staging rewrite under in-flight copies | correctness | unchanged; startup/warn |
| `VICTORY_LEVELS_ON_DEMAND` | launcher off | textures | reserved + tile mappings | tiled levels empty when refused | optional optimization | unchanged |
| memory stack: `VICTORY_SLOT_EVICT`, `VICTORY_ALLOCATOR_TRIM`, `VICTORY_BUDGET_PACK` | launcher on | texture slots, allocators, budgets | memory-use optimization | not separation; changes residency pressure | secondary optimization | unchanged; not counted as separation |

The launcher (`apps/victory/src/play.rs`) sets several of these at start: notably
`VICTORY_CARD_BUFFERS_MIN_KB=64` (every title buffer asks for a VRAM side), `VICTORY_RELAY_COPY=1`
(without its torn-piece check), `VICTORY_WC_NO_BACK=1`, `VICTORY_CARD_AWAKE=1` and the memory
stack. The startup line of e221cad names them, so a log says which are in force.

## 4. Changes made

| Phase | Change |
|---|---|
| Phase 1 commit | The audit and ownership rules (`docs/memory-separation.md`) |
| Phase 2 commit | Implementation: placement ledger (API heap class, fallback reasons, one line), by-address lifetime cap fix (atomic reserve/release, released on drop), checked device-removal waits, by-address and window VRAM budget separation, window refusal counters, room-line and pressure-warning accounting, startup switch line and unsafe-combination warnings, comment corrections, and the fixes from the independent reviews (MB units, `VICTORY_MEMORY_REPORT=0`, fallback reasons, parsed INPLACE levels, device-required tests, docs claims); six unit/device tests |
| Phase 3 commit | The Phase 3 verification record in `docs/memory-separation.md` section 8, this report, and the doc cross-links |

## 5. Tests and validation

| Check | Result |
|---|---|
| `cargo check --workspace --all-targets --locked` (Rust 1.98.1 GNU) | PASS (HEAD) |
| `cargo check --workspace --all-targets --all-features --locked` | PASS (HEAD) |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | FAIL only on the pre-existing `clippy::manual_is_multiple_of` in `shadow.rs` (unchanged line; fails the same on 63b902f) |
| `cargo fmt` | not a CI gate; the tree is not rustfmt-clean at baseline and the pinned toolchain ships no rustfmt |
| `cargo test` locally | not runnable: GNU raw-dylib linking needs a working `dlltool`, absent here (documented limitation) |
| GitHub Actions (fork, `ci/vram-probe` @ e1b1d59+workflow, run 46125105833) | build check PASS; workspace tests 456 passed, 1 failed, 36 ignored |
| Baseline (fork, `ci/probe-memory` @ 63b902f+workflow, run 46122898345) | build check PASS; workspace tests 452 passed, 1 failed, 36 ignored |
| The one failure | `pipeline::host::tests::depth_bounds_pipeline_loads_and_filters_draws`: `depth 0.5 outside [0.6, 1.0] must be rejected` — identical at baseline; WARP does not implement depth bounds |
| New tests | all pass on WARP/CI: heap-class classification (pure), fallback live/total counts (pure), by-address cap fit/refusal (pure), heap classes the API reports for the host buffers (device), by-address cap given back on drop (device), window side count given back on drop (device) |
| Runs on this branch's source tip (+ test workflow) | build check PASS; workspace tests **460 passed, 1 failed, 36 ignored**; the failure is the same pre-existing WARP depth-bounds one as baseline; all 8 tests added on this branch pass (see the pull request for the exact run of the final commits) |

## 6. Hardware limitations

WARP and `GetHeapProperties` prove the creation heap class and the API's answer; they do not
prove physical residency. On real NVIDIA hardware, none of the following was established here:

- whether a `DEFAULT` resource is physically resident at a given moment (WDDM may page it);
- the real cost of the PCIe fallback for uncarded copies/windows/arenas, and whether the
  window/by-address budget split behaves as intended under load;
- whether the 80/97% lines keep the process inside the budget, and how the DXGI budget's
  movement affects the once-computed caps;
- GPU_UPLOAD (resizable BAR) limits and the OpenExistingHeap rules on the target driver;
- the torn-fetch rate when the copy-queue fetch runs without its check;
- device-removal wait behaviour (whether the event is signalled) and in-place/port hazards with
  submissions in the air.

The baseline GitHub Actions run compared above is 46122898345 (452 passed, 1 failed, 36
ignored); the run of this branch's source tip is recorded in the pull request. The eight added
tests are listed in [memory-separation.md](memory-separation.md) section 8.1.

Note on the CI gates: the standard repository CI gate (`clippy -D warnings`) is red at the
baseline commit as well (a pre-existing `manual_is_multiple_of` lint in `shadow.rs`), so the pull
request's inherited CI failure is a baseline condition, not caused by this branch; the temporary
test workflow above proves the build and the test suite.

A validation run on such hardware should use `victory play --set VICTORY_MEMORY_REPORT=1
--set VICTORY_LOG=info`, read the placement line against DXGI local/non-local usage, and
capture GPUView/ETW (DxgKrnl) or NSight Graphics residency events over a full session (world
load burst and a long gameplay segment), plus a TDR/device-removal experiment. The
`VICTORY_INPLACE_PROBE`/census tools answer the in-place driver questions.

## 7. Deferred recommendations

- **Re-evaluate the once-computed caps when the DXGI budget moves** (card_limits,
  window_card_most, VICTORY_MEMORY_CARD_MB): needs a measurement of how often and how far the
  budget moves on target hardware; re-partitioning could itself change rendering.
- **A retry policy for refused windows/copies** when room frees: needs evidence of how often a
  refusal is transient on hardware, and a design for swapping a system-memory copy for a
  device-local one safely.
- **Window VRAM reservation in the texture queue's accounting**: would make the window/texture
  interaction explicit; it lowers texture room, so it needs A/B hardware measurement before any
  default change.
- **Admission for render targets, depth and twins**: the layer currently relies on WDDM;
  refusing them changes rendering, so it needs a hardware experiment and a design decision.
- **Recoverable host-texture refusal**: one transient failure currently leaves a texture missing
  for the run; a bounded retry needs the failure counters on hardware.
- **A per-kind, live (not creation-total) placement ledger** including textures and render
  targets, if a tracked-vs-DXGI reconciliation is wanted; the current ledger intentionally
  reports API classes and live fallbacks only.
