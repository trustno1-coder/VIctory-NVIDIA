# 06 - CPU runtime and threading

Scope: crates/victory-gpu (cores.rs, queue/behind.rs, list/thin.rs, shadow.rs `wide`, host.rs submit/settle),
crates/victory-hle (runtime.rs, kernel.rs, xasync.rs, dstorage.rs), crates/victory-loader (console_insn.rs).
Read-only review; nothing was edited. Tags: [M] measured from logs, [S] static (code read), [H] hypothesis, needs an experiment.

## 0. Caveats about the data

- [M] The logs in `%LOCALAPPDATA%\victory\logs` and `runs\*\frametime.csv` come from an **AMD Ryzen 7 5700X (8C/16T, no hybrid cores) + RTX 4070**,
  not the i9-12900K. Evidence: the `frametime.csv` header (`# cpu: AMD Ryzen 7 5700X ... 16 logical`), `dstorage: workers=16`, `land ... 4 threads`
  (= logical/4), and no "kept on the fast cores" line anywhere (cores.rs stays silent when all cores are alike).
  So everything in `cores.rs` (P/E handling) is unexercised in these logs. Findings in section 4 about hybrid CPUs are static only.
- [M] Thread counts scale with logical CPUs: on the 12900K (24) dstorage runs 24 workers, land 6, `wide` 6 (clamp), pipeline workers more.
  The fixed ones (thin = 8) do not scale.
- The per-frame lines are diagnostics-build averages over 5 s; "per frame" means per present. Scenes differ between windows.

## 1. What the logs say (play-20261127-094655-17644.log, the most useful one)

Two regimes appear in the same session.

**Regime A, about 20 fps, 48-50 ms/frame (log lines 22554-22640).** [M]
- `victory-submit` thread "worked 47.3 / 48.0 / 47.9 ms" of a 49 ms frame: **98 % occupied**, with only 1.9-3.3 ms waiting for flights.
- Its 44 ms ("preparing per frame, the submitting thread"): write-watch asking 3.6, title bytes put under flights' writes 9.7,
  lists' steps 13.3 (refreshes 8.5), copies in the card's memory 6.7, planning steps done on the card 6.0, waiting for flights 1.9-3.3,
  heaps' records 1.6, sending 1.0.
- Title threads waited 14.8-19.6 ms per frame at presents (the line is full, `VICTORY_SUBMIT_FRAMES=1`); no wait for room in the line.
  So the title's recording is not the wall; the single submit thread is.
- The card is "86-90 % busy by this count", but the submit thread holds the whole preparing chain in series with the card (frame is about
  34 prep + 5 card + 11 copy back, from the frame line).
- thin: 38.9 k calls, 207 ms of thread time over 202 lists = 5.3 us per call. At 20.4 fps this is 4.2 logical CPUs permanently busy
  (of 16 logical, 8 physical). Total demand (thin 4.2 + submit 1.0 + land/wide/refresh helpers + title threads + spinners, see 3.5) is close to
  saturating 8 physical cores.

**Regime B, 12-15 fps, 65-84 ms/frame (lines 26060-26118, 25423).** [M]
- Submit thread worked 72-84 ms, of which **43.0 / 59.1 ms "waiting for flights"** and "settles" = 0.8-1.0 per frame, all of them for
  "title memory read ... a fill of upload memory" (0.8-1.0 of 0.8-1.0 settles).
  One settle per frame costs a whole drain of the card: 43-59 ms. The frame equals prep + GPU + copy back in series
  (26.6 + 34.7 + 6.0 = 67.3 of 69.7 ms).
- The relay (landing) thread "waited 62-77 ms for the card with nothing to land".
- Card steps: 4,300-5,000 small fills per frame, 1.9-2.3 MB total (about 450 bytes each, the title's 480-byte constants ring).

**Per-call thin cost has regressed.** [M]+[S] list/thin.rs docs say 3.5 us per call (45-60 k calls, 160-190 ms). Now 4.5-5.3 us in normal windows and
7.3 us in a stall window (58 k calls in 424 ms, log 095317). Either per-draw work was added (descriptor tables, early textures, indirect merge),
or the 8 threads contend (shared atomics, SMT siblings, oversubscription). Not decidable from logs; needs per-op timing (section 5).

**Stalls.** [M] frame lines of 100-500 ms: 5 of 46 gameplay windows (log 094655), 19 of 35 (093851), 24 of 78 (093037). findings.md attributes
270-660 s per session of >200 ms frames to pipeline creation. Not CPU-thread caused as far as the logs show, but see 3.4 (dstorage).

## 2. Ranked opportunities

| # | Change | Gain (est.) | Risk | Evidence |
|---|---|---|---|---|
| 1 | Remove the one-per-frame full drain for fills of upload memory (declare writes tighter / `VICTORY_FILL_TITLE=1` experiment) | regime B: up to 40-55 ms of a 70-84 ms frame; none in regime A | medium (stale constants) | [M] |
| 2 | Split `victory-submit` work into two pipelined stages (prep vs send) | regime A: 48 -> about 30-35 ms (20 -> 28-30 fps) if half the 44 ms moves | high | [M]+[S] |
| 3 | Cut total CPU of thin translation (per-call 5.3 us) and spinning waits so the critical threads get cores | 5-15 % of frame through less contention [H] | low-medium | [M]+[H] |
| 4 | `wide()` fork-join wake latency: try `VICTORY_WIDE_SPIN` or a gang that stays awake during a submission | up to about 4-5 ms of the 48 [H] | low | [S] |
| 5 | Title wait loops (`mwaitx` patched to `pause`) busy-spin whole logical CPUs | [H] frees up to N logical CPUs | medium | [S] |
| 6 | dstorage workers: priority/QoS and count | stutter during streaming [H] | low-medium | [S] |
| 7 | cores.rs: SMT-sibling and E-core awareness on the 12900K | [H], unmeasurable on logs here | low | [S] |

### 2.1 (1) The fill-of-upload-memory settle (regime B)  [M]/[S]

What happens: `Host::submit` (host.rs about 5640) calls `late_blocked(runs)` -> `blocked_by(&line.written, ...)`
(host.rs about 3060-3110). A `Step::Fill(_, src, len, _)` of upload memory (the title's 480-byte constant fill) that touches any range a flight's
shaders may write returns `WHY_READ`; the submit thread then `relay_wait`s / `settle`s until the flights that may write it have landed.
Cause stated in the code comment at `fill_is_titles` (host.rs about 2902): the title's 174 MB heap is declared written as a whole (a root view has no size),
and the constants ring lives inside it, so each frame's fill collides with the declared range.
The mitigation `VICTORY_FILL_TITLE=1` exists and is **off by default and not in the switches of the analysed run** (header of log 094655 lists all switches;
FILL_TITLE is absent). Its author measured "30 ms of a 130 ms frame" for this wait (run blame1).

Design, in order of preference:
1. Run one A/B with `VICTORY_FILL_TITLE=1` (and `VICTORY_AB` if the diagnostics build is used). Record: "waiting for flights" ms, settles, fps, and watch for
   constant corruption (flicker, wrong lighting) because the heuristic can read a constant a flight really writes.
2. Real fix, no heuristic: give root views a size. At the place that declares writes for a root UAV/SRV with unknown size
   (`shadow::declare`/`wide_write` path in draw.rs `bind`, see `writes`/`stores` handling near draw.rs 1990), clamp the declared range to what the shader's stores
   can reach (translator knows buffer bounds only for descriptor tables; for root views use the pipeline's recorded store extents, or the resource's width
   from the title's own `CreateCommittedResource` record if the address is inside a registered buffer: `shadow::register` keeps (start, len)).
   Then the 480-byte ring at a fresh offset no longer touches a declared-written range.
3. Fallback structural change: perform the constants fill on the card ("steps done on the card", `card_steps`) when it is blocked; `VICTORY_CARD_STEPS=2` already does
   this for 4,300-5,000 fills but "0.8-1.0 submissions refused and waited" per frame remain (log line `steps done on the card ... 0.79 submissions refused and waited`).
   Find what makes that one submission refused (`card_steps::copied_read`, host.rs `late_blocked` second condition) and let it through.

Files: crates/victory-gpu/src/host.rs (`late_blocked`, `blocked_by`, `read_blocked`, `fill_is_titles`, `settle`, `relay_wait`),
crates/victory-gpu/src/host/card_steps.rs, crates/victory-gpu/src/draw.rs (declaration of writes in `bind`).

### 2.2 (2) The submit thread is a serial wall (regime A)  [M]/[S]

`queue/behind.rs` `Lane::serve` -> `step` -> `Job::go` runs one submission at a time on `victory-submit`, and `Host::submit` holds `self.submitted`
(the submission lock) "from preparation to copy-back". Inside, everything is sequential: ask watch, plan card steps, `late_blocked`, refresh (wide-spread
internally), put title bytes under flights, send. Parallelism exists only inside steps (`shadow::wide`), at 95 fork/joins per frame.

Already known (findings.md "What is next" 1). Concrete design with the least coupling:

- Stage A (new thread `victory-prep`, per submission k+1 while stage B sends k): everything that depends only on title memory and the write watch, not on
  the landing of flights: `watch::ask_once` ranges (3.6 ms), planning of card steps (`card_steps(runs)`, 6.0 ms), the heaps' record sync
  (`begin_sync_heaps`, already begun early with `VICTORY_HEAP_SYNC_FIRST`), pre-computing refresh plans (`defer_refresh` lists) and the "known to copy
  nothing" skip (8,887 of 10,094 flush entries are no-ops; make them cost zero by having the watch say "no dirty page in range" before the list is queued).
- Stage B (existing `victory-submit`): `late_blocked`/`stale_under_flights` (need the live `line.written`), refresh copies that touch ranges flights may write,
  `ExecuteCommandLists`, landing bookkeeping.
- Hazard: stage A for k+1 reads title memory that flights of k may still write back (landing). Only ranges not in `line.written` of flights up to k
  can be prepared early; the rest is deferred to B. This is exactly the existing `blocked_by` classification, so A can reuse it with a "will be in written after k"
  conservative set (`written` plus k's declared writes).
- Expected: moves the 3.6 + 6.0 + part of 8.5 (refresh scan) = roughly 15-18 ms off the critical thread; frame about 48 -> 32-35 ms, i.e. about 20 -> 28-30 fps in regime A.
- Cost/risk: high (ordering, lost writes were the hardest bugs in bug-journal). Prototype behind a switch and use the existing stress test for lost writes.

Smaller first step (low risk): "title bytes put under flights' writes" 9.7 ms for **29,806 changed runs per frame with 0.76 MB** (about 26 bytes per run;
host.rs line about 3830 stat, from `shadow::merged_stats`). Per-run overhead dominates. Coalesce runs with a gap tolerance (merge runs closer than 32-64 bytes
and copy the gap too, or compare in 64-byte blocks and copy whole blocks): fewer, larger copies. Needs the code in shadow.rs `merge`/`put_in_under` (search `merged_stats`).
Expected 9.7 -> 3-5 ms. Safe only if the gap bytes are the same in both copies or are not being written by a flight (they are checked against `written`).

### 2.3 (3) Total CPU: thin translation  [M]/[H]

- 37-43 k calls/frame, about 9 calls per draw (4.1 k indirect executions, 2.9-4.5 k draws). State calls are already lazy (`with_binding`), so the cost sits in
  `State::draw/draw_indexed/dispatch/execute_indirect` -> `prepare_for` -> `bind` -> `tables`, `shadow::find_any` per vertex slot, `shadow.refresh` ->
  `list.defer_refresh`, `ensure_state` per target, `crate::pipeline::host_pipeline` up to 4 times per draw (each goes through `make::ready`, two relaxed atomics on `p.used`),
  and the D3D12 calls themselves.
- [S] Cache-line ping-pong from process-wide atomics touched by all 8 translators on every draw: `DRAWN`, `DROPPED` (draw.rs 4760-4800), `DRAWS` (`count_draw` in list.rs),
  `STATS[..]` in thin.rs, `make::ready` `used` flag, `shadow` `used`, `BUFFERS_CHANGED` SeqCst load per find. Each is individually cheap (about 50-150 ns when contended),
  together plausibly 5-10 ms of the 200 ms. Fix: thread-local counters flushed at the 5 s line (the `FOUND` thread-local in shadow.rs shows the codebase already does this elsewhere).
- [H] Per-op timing is missing. The `diagnostics` feature has `copystat::recorded` per `with_state_on`, but nothing aggregates per op type. Add (diagnostics only)
  per-op-code `Instant` accumulation in `thin::translate` (the `match op` at thin.rs 699-800), emitted in the thin line. That decides between "D3D12 runtime cost" (inherent,
  needs fewer calls: dedupe identical `SetGraphicsRootDescriptorTable`/constants between draws) and "our bookkeeping" (fix per above).
- Load balance: parallelism is per recording (one thread runs a recording's chunks in order, `scheduled` flag). 202 lists but 319 pieces; if a few lists carry most calls,
  8 threads idle. Log `pieces`/`lists` shows 1.6 chunks per list on average; per-list sizes are not logged. Add a histogram.
- Do not raise `VICTORY_THIN_THREADS` above 8 on a machine at saturation (section 1 demand estimate).

### 2.4 (4) `wide()` fork-join latency  [S]

shadow.rs `wide` (about 1500-1650): Mutex+Condvar queue; helpers sleep in `Condvar::wait`; code comment at `spin` says "asleep, a thread is woken in 50 us and more, 95 times a frame".
95 x 50 us = 4.75 ms of the submit thread's 44 ms in pure wake latency, in the critical path. `VICTORY_WIDE_SPIN` (default 0) is a bounded spin before sleeping; it is not in any switch file, doc, or
log header, so it has probably never been measured. Experiment: `VICTORY_WIDE_SPIN=100`, `=300`; measure submit "worked" ms and thin us/call (spinning helpers use logical CPUs which regime-A demand
suggests are scarce, so check both). If it helps: spin only while the submission lock is held (arm a global flag in `Host::submit`, clear at the end) so helpers sleep otherwise.
Also `notify_one` x N vs `notify_all`: the code already limits; fine. Alternative: a `wide_each`-style persistent job where the submit thread publishes a counter (atomic next index) and helpers claim indices without a Mutex (replace `workers.jobs` Mutex hand-off with an atomic ticket).

### 2.5 (5) Title wait loops spin  [S]/[H]

victory-loader/src/console_insn.rs `wait_patch` replaces the title's `monitorx` with a 3-byte NOP and `mwaitx` with `pause; nop`, "no lighter a wait for it" (their comment). On the console `mwaitx`
sleeps the core until the monitored line is written or a timeout; here each such wait loop becomes a hot `pause` spin that never yields the logical processor. Every idle title worker therefore
holds a logical CPU at 100 %. On the 5700X 16 logical, on the 12900K it also holds an SMT sibling of a core our critical threads want (P-core placement is the scheduler's choice for unpinned threads).
Nothing in logs counts them. Measurement (cheap): sample `GetThreadTimes`/`QueryThreadCycleTime` per title thread over 5 s while at the title screen or paused; threads with about 100 % CPU and no frame work are spinners.
Design if confirmed: patch `monitorx; mwaitx` (adjacent, 6 bytes) as `call stub; nop` with a stub in the existing near-code arena (`console_insn::near`, like `clzero_stub`): stub saves the address from `rax`, spins about 20-50 us
with `pause` checking the dword, then `SwitchToThread()` (not Sleep: no added latency when nothing else wants the CPU, but yields to our critical threads), and returns. Do not use a timed sleep (job pick-up latency would raise frame time).
Risk: medium (patching title code; the `clzero` patch precedent exists).

### 2.6 (6) dstorage and background workers  [S]

dstorage.rs 326 and 632: `workers = available_parallelism()` (16 here, 24 on the 12900K), threads named `dstorage N`, **no priority, no QoS, no affinity**. BCPack decode and decompression run on them (`bcpack_workers` is logged
only at debug level). The title's streaming thread is deliberately lowered (kernel.rs `title_priority`), yet our own dstorage pool at normal priority is as wide as the machine and competes with `victory-submit` (above normal, 1 thread), thin (normal, 8),
title render threads. Pipeline workers (make.rs 571) set a priority; check which. Suggestions: cap dstorage at `n/2`, `THREAD_PRIORITY_BELOW_NORMAL` during gameplay (keep normal while loading, signalled by `crate::frame`/loading state),
and on hybrid CPUs `SetThreadInformation(ThreadPowerThrottling)` (EcoQoS) so Windows 11 places them on E-cores. Stutter symptom is frames of 100-500 ms in streaming areas; verify with a run at `workers=4`.
Also `Sleep(1)` retry loops in dstorage 1582/1672/1728/1876 are fine now (1 ms timer is on).

### 2.7 (7) cores.rs on hybrid hardware  [S]

`cpu_sets()` uses only `EfficiencyClass`. `SYSTEM_CPU_SET_INFORMATION_CPU_SET` also gives `CoreIndex`/`LogicalProcessorIndex`; nothing uses SMT-sibling information.
On the 12900K: the submit thread is pinned to the 16 P logical CPUs (stage 1) and shares physical cores with 4-6 land/wide helpers (stage 2) and unpinned thin/title threads. Experiments that need no code: `VICTORY_FAST_CORES=1/2/3`,
`VICTORY_HELPER_PRIORITY`, and record "submit worked ms" + thin us/call. Possible code change: choose one logical CPU of one P core for `victory-submit` and keep the sibling free from thin workers (set their CPU sets to the other P cores' logical CPUs + E cores),
so the critical thread never shares a core with 4 busy list translators. Stage 3 (thin pinned to P) currently only on `VICTORY_FAST_CORES=3`. No logs support any ranking here; do not ship without an A/B on the 12900K.
Note the 12900K default of `keep_helper_on_fast_cores` is correct for the helpers; the translating threads default to unpinned, which on Windows 11 means Thread Director may place them on E-cores (IPC about 0.6 of P).

## 3. Reviewed and found fine (static)

- Timer resolution: `runtime.rs use_console_timers` does `timeBeginPeriod(1)` + `PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION` + execution-speed throttling off + above-normal process class. Sleep(1) granularity is correct; `NtSetTimerResolution(5000)` (0.5 ms) would only help the title's audio mixer 4 ms sleeps marginally.
- Sync API emulation: WaitForSingleObject, SRW, condvars, critical sections, Sleep are the host's own (kernel32 pass-through, HostPolicy). No per-call emulation overhead. Import thunks are direct bound addresses; the generated stub path is only for unimplemented imports (thunks.rs). The HLE modules that matter (xasync, dstorage, audio) are not on the 20 fps critical path.
- xasync.rs: test-only `yield_now`/`sleep`; real work is on pool threads. audio capture thread `Sleep(1)` poll is small.
- `queue/behind.rs Lane::serve`: 1 ms `wait_timeout` poll only while items stand at a CPU-writable wait; `step` scans <= 38 items under the lock per step, allocating a `heads` Vec per step (minor).
- No `spin_loop` outside rare paths: `watch.rs change_done/change_over` (bounded 2 ms / 1 s with Sleep(0/1) fallback), `shadow::spin` (off), test code. No global spin locks found in the hot path.
- Frame limiter (`frame.rs` sleeps) is off (`VICTORY_FRAME_INTERVAL_US=1000`).
- Global locks on the hot path: `BUFFERS` RwLock is bypassed by the thread-local `FOUND` cache (shadow.rs 5014); `READY`/`WORK` in thin.rs is touched about 320 times per frame; `List.state` Mutex is per list; `Host.submitted` is the only process-wide serialization and it is the submit chain by design.

## 4. Instrumentation to add first (cheap, makes the rest decidable)

1. Per-thread CPU time (`QueryThreadCycleTime`) for every named thread group, in the 5 s frame line: `victory-submit`, `victory-list-*`, `victory-wide-*`, `land*`, relay thread, `dstorage *`, pipeline workers, plus the title's main/render/job threads. Shows total CPU demand vs 8 physical cores, spinners, and who competes.
2. Per-op microseconds in `thin::translate` (diagnostics) and a histogram of calls per recording.
3. Count of `monitorx`/`mwaitx` patches and a periodic "percent CPU of title threads in pause loops".
4. Log which submissions are refused card steps and why (`card_steps::copied_read`), per frame.
5. Print the CPU brand and logical count in the main log (the frametime.csv has it, the play log does not), so logs from other machines are not mistaken for the 12900K.

## 5. Experiment order

1. `VICTORY_FILL_TITLE=1` A/B (log regime B: expect "waiting for flights" 43-59 -> under 10 ms).
2. `VICTORY_WIDE_SPIN=100/300` A/B.
3. Add instrumentation (section 4), rerun, rank thin costs.
4. dstorage cap/priority; spinner patch (2.5) if the per-thread CPU shows spinners.
5. Coalesce changed runs under flights (2.2 small step).
6. Only then the two-stage submit pipeline (2.2 big), the largest and riskiest item.
