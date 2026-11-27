# 01 - GPU submission and scheduling (victory-gpu: queue, host, fence, frame, list, present)

Scope: `crates/victory-gpu` `queue.rs`, `queue/behind.rs`, `host.rs` (submit, settle, relay, present), `host/card_steps.rs`,
`host/send.rs`, `host/awake.rs`, `host/two_queues.rs`, `fence.rs`, `frame.rs`, `list.rs`, `list/thin.rs`, `shadow.rs` (`wide*` only).
Read-only review; no source edited. Tags: **[M]** measured from the logs, **[S]** static (code read), **[H]** hypothesis that needs an experiment.
Descriptor, memory, shader and streaming internals belong to reports 02-05; they are only cross-referenced.

## 0. Evidence base and caveats

- [M] Logs: `%LOCALAPPDATA%\victory\logs\play-20261127-*.log` (5 sessions, diagnostics build, one stats block per ~5 s window, "per frame" = per present).
  Machine in `runs\*\frametime.csv`: **Ryzen 7 5700X (8C/16T), RTX 4070 12 GB, 32 GB**, not the 12900K/7900 XTX of `docs/findings.md`.
  So wide pools are `16/4 = 4` threads (`shadow.rs:1549`), thin is 8 (`list/thin.rs:303`), and 8 physical cores are close to saturated (report 06).
- Numbers below are medians over the "in-world" windows (>2000 draws/frame) of each log unless stated. I wrote small scripts
  (scratchpad `b.js`, `c.js`) to aggregate them; the raw lines are quoted where it matters.
- Frame time in world: median **54 ms (log 094655), 62 ms (093037)**, up to 98-139 ms medians in the stutter-heavy sessions (093851, 094254, 095317).
- The `fps` line's "0.0 on the GPU" and "copying back" are for waited submissions only. They say nothing about the card's real time in relay mode.
  Real card time is **not measured anywhere in these logs** (`VICTORY_GPU_TIMING` and `victory replay --timing detailed` were not run).

## 1. How a submission travels today (the pipeline that is the wall)

```
title render threads --ExecuteCommandLists--> queue.rs:593 execute -> behind::submit (queue/behind.rs:841)
   (list calls only appended as words: list/thin.rs, translated by 8 threads, 43.6k calls = 233 ms CPU / frame)
            |  Job::Lists pushed in ONE line (LANE) in the title's call order
            v
victory-submit thread (behind.rs:267 serve -> step:315 -> Job::go:620)   <== ONE thread, ONE lock (host.submitted)
   Host::execute_parts (host.rs:5315) -> cut_for / split -> send_late (5339) -> Host::submit (5640):
     5675 card_steps         plan + record "steps done on the card" list   (4.6-8.6 ms)
     5681 late_blocked       if a read hits flights' declared writes: relay_wait/settle = FULL DRAIN   (mean 8-14 ms, p90 35-50)
     5702 heap records       begin_sync_heaps (other threads)
     5743 sweep              periodic forced settle (every 1.5 s)
     5784 watch::ask_once    write-watch asking                                (3.6-7 ms)
     5797 stale_under_flights / merge_under_writes  title bytes under flights' writes (5-7 ms)
     5813 refresh_front / run_deferred / batch.flush  refresh host copies, fills, copies (15-28 ms, 37 fork/joins)
     5840 take_snapshots
     5862 card_before        sync list + check list (hash 185 + compare 352 dispatches, 21 MB sent up)   (7-9 ms)
     5912 ExecuteCommandLists(prelude, list, check) + Signal; flight pushed in LINE; relay thread lands it later
   Present job: queue.rs:1032 -> behind::present (929) -> go -> present_planes (host.rs:9747) -> swap_chain.Present (10033)
relay thread (host.rs:3565 relay_worker): waits card fence, fetches pieces, merges into title memory, then tells the title (fences, values)
```

Frame pacing facts [S]: the title may be at most one present ahead (`frames_most` = 1, `behind.rs:736`; `wait_presents` `behind.rs:250`),
up to 64 submissions in the line (`runs_most`, `behind.rs:728`), up to 256 lists in the air (`late_most`, `host.rs:2722`),
swap chain has 2 buffers and `Present(0,0)` (`host.rs:9222`, `10033`). `VICTORY_FRAME_INTERVAL_US=1000` means we do not pace the title.

### 1.1 Measured split of a world frame (submit thread = "the wall")

| per frame, medians [M] | log 094655 (54 ms frame) | log 093037 (62 ms frame) |
|---|---|---|
| victory-submit thread worked (`behind.rs` BUSY) | 49.9 ms (92 % of the frame) | 58.4 ms (94 %) |
| of it: write-watch asking | 3.6 | 4.9 |
| title bytes put under flights' shader writes | 6.9 | 5.2 |
| lists' steps (refreshes 10.0 / 13.0 of it, in 36-38 flushes) | 15.4 | 19.4 |
| copies in the card's memory (card_before) | 6.8 | 7.6 |
| planning steps done on the card (5.4k fills) | 4.9 | 4.6 |
| sending lists | 1.1 | 1.2 |
| waiting for flights (median / **mean**) | 4.7 / **14.4** | 0.1 / **12.0** |
| title threads waiting at presents | 19.7 | 23.6 |
| card "nothing of ours queued" (idle while submit thread works) | 7.3 (7.2) | 8.3 (8.0) |
| card clock "busy" | 85 % | 86 % |
| relay thread: waiting for the card / landing | 29 / 15 ms | 38 / 13.6 ms |

Reading [M]: the title's render thread sits ~20 ms per frame at the present because the submit thread is the pipeline's slowest stage.
The card is queued-busy 85-90 % of the time (its own fence clock, `host.rs:648-735`), idle only 7-9 ms per frame, all of it while our thread is preparing.
So **two walls are close to each other**: the submit thread (~50 ms) and the card (up to ~45-49 ms of queued work, composition unknown).
Reports that say "the card is not the wall (6.8 ms of GPU)" read a counter that only counts waited submissions. The honest status is: **unknown, must be measured** (section 5, item 0).
Whatever the card's real time is, speeding up the CPU side alone can at best lower the frame to about the card's time.

Regime split [M] (settle reasons, `c.js`): "title memory read" settles average **8.4-11.8 ms per frame in every log**, p90 of 5 s windows 34-50 ms, and
the `fps` windows with mean frame >100 ms in logs 093851/094254/095317 are the ones with the large `waitfl`. This is the biggest *stutter* mechanism in my area.

## 2. Mechanism inventory: why it exists, requirement or artifact

| Mechanism | Why it exists | Real Xbox requirement? | Why per frame | Can it go |
|---|---|---|---|---|
| One serial prepare+send thread under `host.submitted` (`host.rs:5647`) | host copies, snapshots, numbered sends and the compare area are per host copy, not per submission (comment `host.rs:5644`) | No. Artifact of shared mutable per-copy state | every submission, 28 a frame, in title order | Split into stages (R2) |
| Cut at memory-value waits, 29 parts a frame (`host.rs:3200-3207`, `queue.rs` `cut_list`) | the title's lists wait on values the GPU writes; the host can only wait on fences | Partly: a real ordering dependence, but 18.7 of them per frame are met by a flight in the same stream ("met by a flight's bottom-of-pipe write", log) and need no cut | each wait | Join more (R4), elide waits met by an earlier part of the same queue |
| Refresh host copies at submit (`Batch`, `host.rs:8121`; `defer_refresh` `host.rs:7250`) | card cannot read title memory | **Yes**, a card-visible copy is required (or in-place heap, `inplace.rs`) | nobody tells us when the CPU wrote | Make it incremental/push (R5) |
| Heap-buffer pass: ~10k buffer views refreshed once per frame (`draw.rs:2382-2413`, `heap_buffer_pass`) | bindless SM6.6 shaders index any view | Artifact of "no idea which view a computed index names" | per frame | descriptor report 03 |
| Write-watch asking + guards (`watch.rs`) | detect CPU writes | Required in some form | per submission (`ask_once`) | report 02 |
| Title bytes put under flights' writes (`merge_under_writes`, `host.rs:3199`) | the title's 480-byte constants ring lives in the same 174 MB heap that 11 skinning dispatches write as a whole (`card_steps.rs:12-24`) | **Artifact** of declaring a whole-heap root view as written | per frame, 58k changed runs / 2.3 MB | R1 (narrow declared writes) |
| Card steps: copy/fill done on the card (`card_steps.rs`, `stage()=2`) | avoid waiting for flights before reading what they write | Artifact (same root cause) | 5.4k fills / frame | R1, then R3 (batch) |
| Settle/drain for "title memory read" (`late_blocked` `host.rs:2891`, call site `5681`) | a CPU step reads memory flights may still write | Artifact of the same over-declaration | 0.17-0.2 per frame, each 40-50 ms | R1 |
| Card hash+compare (`card_before` `host.rs:4411`) | find what shaders wrote | Requirement is "know what shaders wrote" (audit I3), not "hash" | 185 hash + 352 compare passes | R4/R6 |
| Landing thread (relay, `host.rs:3565`) | R1 invariant: CPU must see GPU results before a fence/present/value | **Yes** (ocean maps, culling, skinning lists) | each flight | keep, but do less |
| Present on the submit thread (`queue/behind.rs:677-689`, `host.rs:9747`) | order with lists | No | per frame, 1.2-1.9 ms | R7 |
| Forced settle at the sweep every 1500 ms (`host.rs:5745-5753`) | free retired objects when nothing is in the air | No: could retire by fence value | every 1.5 s | R8 |
| `CARD_AWAKE` spin kernel (`host/awake.rs`) | keep NVIDIA/AMD clocks up across 30 gaps a frame | Artifact of the gaps | continuous | disappears with R2 (fewer gaps) |

## 3. Ranked structural opportunities

Estimates are for the submit thread's ms/frame unless said otherwise; they do not add up (overlap). Risk is risk of wrong pixels/lost writes.

### R1. Stop over-declaring the title's heap as shader-written: removes the full drains, most "under flights" merging and part of the card steps
- **Evidence** [M] waiting for flights mean 12-17 ms/frame (median 0-5) in 4 of 5 logs; "title memory read" 8.4-11.8 ms/frame mean; `submissions sent ahead ... 0.2 settles, 6.3 ms waiting` and
  `0.17 submissions refused and waited` (card_steps line). Per occurrence ~30-50 ms (a whole drain of the flights). Title bytes under flights' writes 5-7 ms. [S] root cause in
  `card_steps.rs:12-24`: eleven skinning dispatches write "through one view of the title's whole 174 MB heap, at places they read out of job records", so `Line.written` (`host.rs:2782`) covers the heap and
  every fill/copy of the title's constants ring (also in that heap) "reads what flights write" (`blocked_by` `host.rs:3072`, `read_blocked` `3059`).
- **Why a drain** [S]: when `card_steps` refuses (`card_steps.rs:661-679`; refusal strings at `:261-409` and `:723-785`: "a step of ours runs" (`Does::Run`), "a texture reads it", "flights write it through two host copies",
  "no host copy holds all of it", "too much at once", ...), `submit` falls to `relay_wait`/`settle` (`host.rs:5681-5693`) which waits for the whole line.
  A blame line exists but only in diagnostics when enabled (`log_blame`, `host.rs:3024`): "waited for flights, per frame: ..." did **not** appear in any of the five logs, so the refusing step kind is not known from the data.
- **Design (replacement)**: replace "declared range" by "learned write zones" for the purpose of *blocking*.
  1. Keep the conservative declaration for the *compare/landing* (correctness: R1 invariant), but compute `Line.written` for blocking from `watch.rs` `LANDED` journal
     (`landed_lately`, `watch.rs:1944`, 4 KiB pages, last-landed frame): a read is blocked only if its pages were landed in the last K frames or are in a range whose shader writes are bounded by the pipeline's observed extent.
     This is what `VICTORY_FILL_TITLE=1` already does for small fills (`host.rs:2921`, 600 frames, **off by default**). Extend it to copies/textures/steps and make the whole-heap compare run each frame on pieces to verify.
  2. Verification: the compare already hashes declared pieces; hash the *whole* heap's pieces (174 MB / 64 KiB = 2.7k pieces, one pass) every frame in the check list. A write into a "never written" piece marks it learned and
     raises a counter `learned_violations` (log); the affected frame can be one frame stale for that constant, which matches the existing `FILL_TITLE` risk note (`host.rs:2915-2919`).
  3. Optional split: allocate the constants ring in its own host copy (a second `Shadow` over the ring's address range) so it is not part of any whole-heap declaration at all.
- **Quick experiment before any code**: A/B `VICTORY_FILL_TITLE=1` (exists, off) on the same save; watch `title memory read` ms and `waitfl`. Also run once with diagnostics blame to name the refusing step.
- **Estimated gain**: -8 to -12 ms mean on the submit thread (more important: removes 40-50 ms spikes, the main stutter source here), plus -3 to -5 ms of "under" merging, plus part of `plan` if fewer steps are needed.
- **Risk**: medium (one-frame-stale constants if wrong). **Files**: `host.rs` `blocked_by` (3072), `late_blocked` (2891), `read_blocked` (3059), `fill_is_titles` (2921), `stale_of`/`merge_under_writes` (3199);
  `host/card_steps.rs` `card_steps` (660), `plan` (259), `take_steps` (682); `watch.rs` `landed_lately` (1944); `host.rs` `card_before` (4411, whole-heap verify pass).

### R2. Pipeline the submit thread into two stages (prepare / send) - the audit's own "next" item
- **Evidence** [M]+[S]: thread 92-94 % occupied; stage P (watch 3.6 + under 6.9 + steps 15.4 = ~26 ms) and stage S (card_before 6.8 + plan 4.9 + send 1.1 + after-send ~1 = ~14 ms) are
  independent enough to overlap *different submissions*. `findings.md` "What is next" item 1 asks for this; report 06 ranks it #2 (high risk).
- **Why possible** [S]: what P of submission N+1 needs from N is only N's *declared writes* (to know what is "in the air": `let air = line().written.clone()`, `host.rs:5810`) and numbering of sends (`QUEUE_ORDER`, `card_before` `host.rs:4424`).
  Both are computable from the lists before N is sent (`declared_writes(runs)`, `host.rs:3960`).
- **Design**: split `Host::submit` (`host.rs:5640`) at `host.rs:5862`. Stage P runs on a second thread `victory-prep` that pulls the next ready job from `Lane` (`behind.rs:315 step` currently pops one job at a time and runs it inline in `Job::go`),
  holds a *prep lock* (not `submitted`), and appends the declared ranges of the job to a `pending_air` list under `Line` *before* it starts; it produces a `Prepared` (batch.placed, picked, declared, `own`, lists vec).
  Stage S (existing thread) holds `submitted`, runs `card_before`, `ExecuteCommandLists`, `Signal`, pushes the flight. Order of S = order of jobs (a bounded queue of `Prepared`, depth 2).
  Keep `take_along` (joins, `behind.rs:557`) before P so joined parts are prepared together.
  The present job and signals pass through S only.
- **Estimated gain**: submit-thread critical path 50 -> ~30-35 ms if P and S overlap fully; realistic -10 to -15 ms. Also halves the card-idle gaps (7-9 ms) because S sends sooner.
  Bound by the card (section 1.1) and by core count: P adds a busy thread on an 8-core machine already carrying ~8 logical CPUs of work.
- **Risk**: high (ordering, R2 "never overwrite newer title bytes", numbered sends). Test with replay `--compare` and the lost-write stress (audit C4: 600 runs; `SUBMIT_DEPTH=1/64`).
- **Files**: `queue/behind.rs` `Lane::step` (315), `Job::go` (620), `Job::take_along` (557); `host.rs` `submit` (5640), `send_late` (5339), `card_before` (4411), `Line` (2782); `host/card_steps.rs` `card_steps` (660).

### R3. One gather dispatch instead of ~5,400 small copy commands per frame (card steps' fills)
- **Evidence** [M]: `steps done on the card ... 5409.44 fills, 0.00 buffer copies, 2296.9 KB` (log 093037, line 18711 area); fills of ~430-480 B. `planning steps done on the card` 4.6-8.6 ms per frame.
  [S] `card_steps::record` (`card_steps.rs:546-600`) emits one `CopyBufferRegion` per read and per write plus barriers per level; `places` dedupes by (copy, src, len, level) but every distinct 480-byte constant is still its own command.
  The code already has the right tool: `send.rs:277-314` sends runs "as pairs of offset and value, stored by one dispatch per copy".
- **Design**: add a tiny compute kernel "gather": input = a table `{src_off, dst_off, len}` (built on the CPU in one upload, 5.4k x 16 B = 86 KB), source = the host copy of the heap, destination = `between` (`BETWEEN` 4 MiB, `card_steps.rs:111`).
  One dispatch per level. Coalesce adjacent src/dst runs on the CPU (the ring is written sequentially). CPU: build table instead of 5.4k D3D12 calls; GPU: one dispatch instead of 5.4k copies (each a tiny serialized copy).
- **Estimated gain**: -3 to -6 ms CPU on the submit thread (recording 5.4k commands at ~1 us), plus unmeasured GPU time (5.4k tiny copies; likely several ms of card time, [H]).
- **Risk**: low-medium (self-contained, replay `--compare` verifies bytes). **Files**: `host/card_steps.rs` `record` (546), `take_steps` (682-800), `Op`/`To` (520-536); new kernel next to `send.rs:300-320`.

### R4. Fewer, bigger submissions and one coherence pass set per frame segment
- **Evidence** [M]: 28 submissions/frame, only 13 prepared together with the previous (`JOIN_PARTS=8`, `JOIN_LISTS=48` default `host.rs:3215-3226`), 16.1 "submissions sent ahead", 18.7 waits met by in-air flights.
  Each prepared unit pays a fixed cost: lock, `begin_sync_heaps`, `ask_once`, `refresh_front` fork/join, `card_before` (a sync list *and* a check list with a full UAV barrier each, `host.rs:4635-4643`, 4553), `Signal`, `awake::sent`, `land()`.
  `card_before` costs 6.8-9.9 ms/frame; 185 hash + 352 compare passes are recorded per frame (log line "our passes over copies").
- **Design**: (a) cheap: raise `JOIN_PARTS` to 16-32 and `JOIN_LISTS` to 96-128 (existing env vars) and A/B; (b) in `Job::take_along` (`behind.rs:557`) treat a `Job::Value` wait that a flight *before it in the same queue's joined part* will meet
  (`will_meet`, `host.rs:3236`) as satisfied without cutting (it already does for `hosts()`, but `late_met_known` waits split); (c) structural: record all hash passes of a prepared unit into one list with one barrier and
  collapse per-copy `SetComputeRootShaderResourceView/UnorderedAccessView` into one bindless table kernel (SM6.6 `ResourceDescriptorHeap`, same mechanism as the heap strides table).
- **Estimated gain**: (a) -1 to -3 ms, (c) -3 to -5 ms CPU and fewer GPU pipeline drains (2 global UAV barriers per submission x ~15 = 30 drains/frame, [H] 1-3 ms of card time).
- **Risk**: (a) low; (b) medium (ordering); (c) medium-high (hash exactness; `--skip-hash-passes` replay oracle exists). **Files**: `queue/behind.rs` `take_along` (557); `host.rs` `joins`/`joinable` (3211-3245), `card_before` (4411-4670), `card_pass` (2232).

### R5. Make refresh incremental: one fork/join per submission (not 37 per frame) and no per-span lock chatter
- **Evidence** [M]: refreshes 10-13 ms/frame in `36-38 flushes of 9,939` spans; 8,849 known to copy nothing; `wide`'s helper wake costs "50 us and more, 95 times a frame" (`shadow.rs:1529-1533`, `VICTORY_WIDE_SPIN` default 0); copies 11-20 ms thread time / 30 MB.
  `nothing_new` takes three mutexes per span (`shadow.rs:3959-3984`: `pending`, `copied`, `ranged`) on ~10k spans (the ~4,100 heap-buffer copies come with `heap_buffer_pass`, once per frame, but re-flushed per submission).
- **Design**: (1) per frame, compute the set of dirty copies once from the `watch` journal (`ask_ahead`) into a `Vec<Arc<Shadow>>` and give each `Batch` only those (O(dirty), not O(10k)); (2) one persistent job queue (`Workers`, `shadow.rs:1520`) that stays awake
  between flushes inside a submission (spin 20-50 us only while a submission is being prepared, never otherwise); (3) merge flushes of joined parts (`refresh_front` already does for `JOIN_PARTS`; raise parts, see R4).
- **Estimated gain**: -3 to -6 ms [H] (fork/join latency 37 x ~0.1-0.2 ms + lock chatter). Cheap probe: `VICTORY_WIDE_SPIN=100` A/B (expect little on a CPU-saturated 8-core, positive on the 12900K).
- **Risk**: low-medium. **Files**: `host.rs` `Batch::refresh_front` (8520), `flush_all` (8701), `copy_spans` (8410); `shadow.rs` `wide` (1593), `wide_each` (1655), `Shadow::nothing_new` (3959).

### R6. GPU-side cost of the coherence machinery: measure first, then cut
- **Evidence** [M]: card queued 85-90 % of every frame; [S] per frame the card runs ~540 hash/compare dispatches, ~5.4k tiny copies, 16 sync lists + 16 check lists, 30 global UAV drains, 21 MB of sends, ~18 MB fetched for 0.77 MB landed (report 02).
  None of it is the title's own work. If the title's own GPU time is 30-35 ms (Series X 30 fps target), the emulation overhead is a double-digit percentage of the card's frame. **Unknown without the replay tool** (`docs/replay.md`: `--skip-hash-passes`, `--no-barriers`, `--merge-barriers`, `--timing detailed`, `--one-submit`, `--pace`).
- **Design**: run `victory replay <capture> --compare --skip-hash-passes` and `--one-submit` on the 4070; attribute card ms to (a) title commands, (b) coherence passes, (c) fills/copies, (d) gaps. Only then choose between
  R3/R4(c) (fewer commands), declared writes replacing compare (audit C3, `PICKED_WRITES`), and async compute (`two_queues.rs` is a plan only: every host list is a DIRECT list, `two_queues.rs:11-28`).
- **Estimated gain**: unknown, potentially the dominant term once the CPU is below ~45 ms. **Risk**: n/a (measurement).

### R7. Move Present (and the panel/menu/meter) off the submit thread
- **Evidence** [M]: "1.47 ms in [Present] (the longest 4.9 ms), 0.39 ms recording the upscaling" per frame on the presenting thread (log line 18711); median 0.9-1.9 ms over the logs. [S] it runs inline in `Job::go` (`behind.rs:677-689`) -> `present_planes` (`host.rs:9747-10050`):
  swap-chain access, composite list, `Present(0,0)` (blocks when both of the 2 buffers are queued), `p.panel.frame`, `p.menu.frame`, `p.meter.frame`.
- **Design**: after recording the copy list and sending it (`execute_own`, `host.rs:5402`), hand `Present` + frame meter to a presenter thread that waits for the flight's fence. Raise `BUFFERS` to 3 only if it measurably stops `Present` blocking.
  Streamline's per-present tagging (`streamline::present_begins/called/returned`) must stay in order on one thread; the presenter thread owns it.
- **Estimated gain**: -1.5 to -2 ms/frame, and less jitter (`longest 4.9 ms`). **Risk**: low-medium (Streamline thread rules). **Files**: `queue/behind.rs` `Job::go` (620), `host.rs` `present_planes` (9747), `streamline.rs` present hooks.

### R8. Retire resources by fence value instead of the periodic forced drain
- **Evidence** [S]: `host.rs:5743-5753`: when `late_free_present()` and 1.5 s passed since `SWEPT`, `submit` calls `settle(.., WHY_SYNC)` = full drain of the flights, then `sweep()`; also `present_planes` resize path `settle_now(WHY_SYNC)` (`host.rs:9800`).
  [M] `a synchronous submission or present` averages 0.2-0.4 ms/frame (= a drain every ~1.5 s, each ~10-30 ms): a recurring hitch every 1.5 s, invisible in averages.
- **Design**: give every retired object (texture/view memory, tables, scratch) the fence value of the last flight that may use it (`Flight.fence`, `host.rs:2760`) and free when `fence.completed() >= value` on the relay thread; keep the drain only for resize/shutdown.
- **Estimated gain**: removes a periodic 10-30 ms hitch; mean ~0.3 ms. **Risk**: low-medium (lifetime bugs = device loss; run `SLOT_CHECK`). **Files**: `host.rs` `sweep` (435), `submit` (5738-5754), `retire`/`bury` (`host.rs`, uses `FINISHED`).

### R9. Things I checked and do NOT recommend now
- Raising `VICTORY_SUBMIT_FRAMES` (queue depth): only moves the wait from the title's present to the line; the submit thread is the bottleneck (title waits 20 ms at presents anyway). It also holds more list allocators (findings: 9 GB).
- A second drawing queue / `ASYNC_SUBMIT` off: already measured negative (`findings.md`). `two_queues` async compute stays a plan until R6 says the card is the wall.
- Barrier removal: "nothing to 0.1 ms each" (`findings.md`); only `--merge-barriers` in R6 for the UAV drains.
- `VICTORY_SUBMIT_DEPTH` (64 = two frames): not a limiter (`threads of the title waited 0.0 ms for room in the line`).

## 4. Order of work (what I would do, cheapest and most certain first)

0. **Measure the card** (replay `--compare --skip-hash-passes`, `--timing detailed`) and capture one diagnostics run with blame lines for `waited for flights`; decide R1 vs R6 priority. (No code.)
1. R1 step A: A/B `VICTORY_FILL_TITLE=1`, `VICTORY_JOIN_PARTS=16 VICTORY_JOIN_LISTS=96`, `VICTORY_WIDE_SPIN=100` - three env-only experiments, 15 intervals each (audit method: median of >=15 intervals with >=1000 draws/frame). Expected: -3 to -12 ms and far fewer spikes.
2. R7 and R8 (small, low risk, remove 1.5 ms and the 1.5 s hitch).
3. R3 (self-contained, replay-verifiable), then R1 step B (learned write zones with whole-heap verification).
4. R2 (two-stage pipeline) once the stages are smaller (after R1/R3 the P stage is ~15 ms and S ~8 ms, so the split no longer needs to be perfect).
5. R6 follow-ups chosen from the replay breakdown.

Projection [H]: R1+R3+R4(a)+R5+R7 take the thread from ~50 to ~28-32 ms; R2 would take it under 25. The frame then equals the card's time, so the achievable fps on this 4070 is
set by item 0: if the card's real time is ~45 ms the CPU work buys ~20 -> 22-25 fps and the GPU-side cut (R6) is the only way past it.

## 5. What I could not establish (and how)

- Real card time and its split (title vs coherence): replay tool, `VICTORY_GPU_TIMING`, or timeline (`VICTORY_TIMELINE`) with `card_wait` spans.
- Which step kind causes the 0.17-0.2 refused submissions per frame: diagnostics `log_blame` (`host.rs:3024`) was not enabled in these logs.
- Whether the thin-translator's 5.3 us/call (43.6k calls, 233 ms CPU/frame, `list::thin` line) is dominated by the ~4.7k draws (233/4700 = ~50 us per draw incl. bindings) or by the cheap state calls: needs a per-op timer; out of my scope (report 06, 03).
- Residency waits: "new card memory waited for until it is mapped ... 18600 times, 2986 ms, the longest 63.6 ms" (log 093037, line ~18690) is a submit-path stall source; belongs to report 02.
