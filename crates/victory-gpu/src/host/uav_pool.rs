//! Pooled device-local scratch buffers (`UAV_SPARE`) for [`HostList`].
//!
//! Creating a committed `D3D12_HEAP_TYPE_DEFAULT` UAV buffer costs a synchronous
//! `CreateCommittedResource` (64 KiB minimum alignment), `MakeResident`, and
//! [`mapped`] copy-queue fence wait (`0.30-1.60 ms` per call). `HostList`
//! recycles its per-recording `between` buffers at [`HostList::reset`] and its
//! `card_budget`, `indirect_args`, and `batch_args` buffers when dropped
//! (`IDLE_MOST` overflow or `VICTORY_ALLOCATOR_TRIM` idle-list trim) through
//! [`UAV_SPARE`], keyed by exact `Width` in power-of-two multiples of 64 KiB.
//!
//! Only buffers verified to be back in `D3D12_RESOURCE_STATE_COMMON` and past
//! every in-flight GPU submission ([`retire_uavs`]) enter [`UAV_SPARE`], and
//! only scratch consumers that overwrite their ranges before reading take from
//! it (`Host::uav_buffer` preserves zero-initialized semantics for `Shadow`).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use super::*;

/// Byte width of [`HostList::card_budget`]: 64 KiB matches D3D12 default-heap
/// placement alignment and shares the 64 KiB bucket with `ahead` and scratch.
pub(super) const CARD_BUDGET_BYTES: u64 = crate::ahead::SIZE;

/// Byte width of one [`HostList::batch_args`] buffer (`8 * 256 * 32 == 64 KiB`).
pub(super) const BATCH_ARGS_BYTES: u64 =
    (BATCHES as u64) * (crate::indirect::batch::JOBS as u64) * (crate::indirect::batch::SLOT as u64);

/// Maximum bytes kept in [`UAV_SPARE`] on >= 32+ GiB hosts.
const UAV_SPARE_MOST: u64 = 256 << 20;

/// Maximum bytes kept in [`UAV_SPARE`] when [`crate::residency::limited_host_ram`] is true.
const UAV_SPARE_LIMITED: u64 = 128 << 20;

/// Largest single scratch UAV buffer rounded to a power of two and cached.
const UAV_SPARE_MAX_ITEM: u64 = 16 << 20;

/// Warm floor retained under mild pressure or 10 s idle-watermark trims so
/// steady-state command list churn does not fall back to `mapped()` stalls.
const UAV_SPARE_WARM_FLOOR: u64 = 16 << 20;

static UAV_SPARE: Mutex<Spare<ID3D12Resource>> = Mutex::new(Spare::new());

type RetiredBatch = (u64, Vec<(ID3D12Resource, u64)>);
static RETIRED_UAVS: Mutex<VecDeque<RetiredBatch>> = Mutex::new(VecDeque::new());

/// `[hits, misses, kept_count, kept_bytes, trimmed_bytes]`.
pub(crate) static UAV_SPARE_STATS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

#[cfg(test)]
static TEST_POOL_MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Whether [`HostList`] shares device-local scratch UAV buffers through
/// [`UAV_SPARE`] (`VICTORY_UAV_POOL=0` disables pooling for A/B comparisons).
pub(super) fn uav_pool() -> bool {
    #[cfg(test)]
    match TEST_POOL_MODE.load(Ordering::Relaxed) {
        1 => return false,
        2 => return true,
        _ => {}
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_UAV_POOL").is_none_or(|v| v != "0"))
}

pub(super) fn uav_spare_limit() -> u64 {
    if crate::residency::limited_host_ram() {
        UAV_SPARE_LIMITED
    } else {
        UAV_SPARE_MOST
    }
}

/// Rounds a scratch UAV request to a power-of-two multiple of 64 KiB up to
/// [`UAV_SPARE_MAX_ITEM`] (64 KiB multiples above that), keeping bucket counts
/// small while every buffer keeps `GetDesc().Width == requested_bucket_size`.
pub(super) fn uav_bucket_size(size: u64) -> u64 {
    let min = crate::ahead::SIZE;
    let clamped = size.max(min);
    if clamped <= UAV_SPARE_MAX_ITEM {
        clamped
            .checked_next_power_of_two()
            .unwrap_or_else(|| clamped.next_multiple_of(min))
    } else {
        clamped.next_multiple_of(min)
    }
}

/// Takes a `COMMON`-state device-local UAV buffer of exact `size` bytes from
/// [`UAV_SPARE`], falling back to [`Host::uav_buffer`] (`ahead::card_if_ready`
/// for 64 KiB, then fresh creation).
pub(super) fn take_uav(host: &Host, size: u64) -> Option<ID3D12Resource> {
    if uav_pool()
        && size <= UAV_SPARE_MAX_ITEM
        && let Some(buffer) = UAV_SPARE.lock().unwrap_or_else(|e| e.into_inner()).take(size)
    {
        UAV_SPARE_STATS[0].fetch_add(1, Ordering::Relaxed);
        UAV_SPARE_STATS[2].fetch_sub(1, Ordering::Relaxed);
        UAV_SPARE_STATS[3].fetch_sub(size, Ordering::Relaxed);
        return Some(buffer);
    }
    UAV_SPARE_STATS[1].fetch_add(1, Ordering::Relaxed);
    host.uav_buffer(size)
}

/// Returns `COMMON`-state UAV buffers whose GPU work has completed to
/// [`UAV_SPARE`], freeing any overflow past [`uav_spare_limit`] outside the lock.
pub(super) fn give_uavs(buffers: Vec<(ID3D12Resource, u64)>) {
    if buffers.is_empty() {
        return;
    }
    let pool_on = uav_pool();
    let limit = uav_spare_limit();
    let mut freed = Vec::new();
    {
        let mut spare = UAV_SPARE.lock().unwrap_or_else(|e| e.into_inner());
        for (buffer, size) in buffers {
            let rejected = if pool_on && size <= UAV_SPARE_MAX_ITEM {
                spare.give(buffer, size, limit)
            } else {
                Some(buffer)
            };
            match rejected {
                None => {
                    UAV_SPARE_STATS[2].fetch_add(1, Ordering::Relaxed);
                    UAV_SPARE_STATS[3].fetch_add(size, Ordering::Relaxed);
                }
                Some(buffer) => freed.push(buffer),
            }
        }
    }
    maybe_window_trim();
    drop(freed);
}

/// Returns `buffers` to [`UAV_SPARE`] once all currently started or queued GPU
/// submissions have completed, or immediately when none are in flight.
pub(super) fn retire_uavs(buffers: Vec<(ID3D12Resource, u64)>) {
    let started = STARTED.load(Ordering::Acquire) + crate::queue::queued();
    let finished = FINISHED.load(Ordering::Acquire);
    retire_uavs_at(buffers, started, finished);
}

fn retire_uavs_at(buffers: Vec<(ID3D12Resource, u64)>, started: u64, finished: u64) {
    if buffers.is_empty() {
        return;
    }
    if !uav_pool() {
        drop(buffers);
        return;
    }
    if started <= finished {
        give_uavs(buffers);
        return;
    }
    RETIRED_UAVS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_back((started, buffers));
}

/// Drains UAV batches whose in-flight submission fence `finished` has completed.
pub(super) fn drain_retired_uavs(finished: u64) {
    let ready: Vec<Vec<(ID3D12Resource, u64)>> = {
        let mut retired = RETIRED_UAVS.lock().unwrap_or_else(|e| e.into_inner());
        let n = retired.iter().take_while(|(at, _)| *at <= finished).count();
        if n == 0 {
            return;
        }
        retired.drain(..n).map(|(_, list)| list).collect()
    };
    for list in ready {
        give_uavs(list);
    }
}

fn high_vram_usage() -> (bool, bool) {
    let Some([budget, used, ..]) = host().and_then(|h| h.video_memory()) else {
        return (false, false);
    };
    if budget == 0 {
        return (false, false);
    }
    let pct = u128::from(used) * 100 / u128::from(budget);
    (pct >= 88, pct >= 95)
}

fn maybe_window_trim() {
    if allocator_trim().is_none() {
        return;
    }
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = now_ms();
    let last = LAST.load(Ordering::Relaxed);
    if now >= last + 10_000
        && LAST
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        let (vram_high, _) = high_vram_usage();
        let floor = if vram_high { 0 } else { UAV_SPARE_WARM_FLOOR };
        window_trim_uav_spare(floor);
    }
}

fn window_trim_uav_spare(floor: u64) {
    let trimmed = UAV_SPARE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .trim_above(floor);
    account_trimmed(trimmed);
}

/// Trims [`UAV_SPARE`] down to `max_bytes` immediately.
pub(super) fn trim_uav_spare_to(max_bytes: u64) {
    let trimmed = UAV_SPARE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .trim_to(max_bytes);
    account_trimmed(trimmed);
}

/// Called when host memory is pressured (`trim_spare_chunks`).
pub(super) fn trim_uav_spare_pressure() {
    let floor = if crate::residency::critical_commit() {
        0
    } else {
        UAV_SPARE_WARM_FLOOR
    };
    trim_uav_spare_to(floor);
}

/// Once-per-second sweep hook (`Host::sweep`): trims on VRAM or host pressure
/// and advances the 10-second idle-watermark window when `allocator_trim` is on.
pub(super) fn sweep_uav_spare() {
    let (vram_high, vram_critical) = high_vram_usage();
    if crate::residency::critical_commit() || vram_critical {
        trim_uav_spare_to(0);
    } else if crate::residency::pressured() || vram_high {
        trim_uav_spare_to(UAV_SPARE_WARM_FLOOR);
    } else {
        maybe_window_trim();
    }
}

fn account_trimmed(trimmed: Vec<(ID3D12Resource, u64)>) {
    if trimmed.is_empty() {
        return;
    }
    let count = trimmed.len() as u64;
    let bytes: u64 = trimmed.iter().map(|(_, s)| *s).sum();
    UAV_SPARE_STATS[2].fetch_sub(count, Ordering::Relaxed);
    UAV_SPARE_STATS[3].fetch_sub(bytes, Ordering::Relaxed);
    UAV_SPARE_STATS[4].fetch_add(bytes, Ordering::Relaxed);
    drop(trimmed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    static SERIAL: Mutex<()> = Mutex::new(());

    struct PoolModeGuard(u8);

    impl PoolModeGuard {
        fn set(mode: u8) -> Self {
            let prev = TEST_POOL_MODE.swap(mode, Ordering::SeqCst);
            Self(prev)
        }
    }

    impl Drop for PoolModeGuard {
        fn drop(&mut self) {
            TEST_POOL_MODE.store(self.0, Ordering::SeqCst);
        }
    }

    fn clear_pool_for_test() {
        trim_uav_spare_to(0);
        for v in &UAV_SPARE_STATS {
            v.store(0, Ordering::SeqCst);
        }
    }

    #[test]
    fn bucket_sizes_are_exact_power_of_two_multiples_up_to_cap() {
        assert_eq!(uav_bucket_size(0), 64 << 10);
        assert_eq!(uav_bucket_size(256), 64 << 10);
        assert_eq!(uav_bucket_size(64 << 10), 64 << 10);
        assert_eq!(uav_bucket_size((64 << 10) + 1), 128 << 10);
        assert_eq!(uav_bucket_size(192 << 10), 256 << 10);
        assert_eq!(uav_bucket_size(16 << 20), 16 << 20);
        assert_eq!(uav_bucket_size((16 << 20) + 1), (16 << 20) + (64 << 10));
        assert_eq!(CARD_BUDGET_BYTES, 64 << 10);
        assert_eq!(BATCH_ARGS_BYTES, 64 << 10);
    }

    #[test]
    fn trim_above_and_trim_to_preserve_warm_floor_and_evict_oldest() {
        let mut spare: Spare<u32> = Spare::new();
        for id in 1..=6 {
            assert_eq!(spare.give(id, 8 << 20, 256 << 20), None);
        }
        assert_eq!(spare.bytes, 48 << 20);
        // First window primes `low` without evicting.
        assert!(spare.trim_above(16 << 20).is_empty());
        assert_eq!(spare.low, 48 << 20);

        // Second idle window trims down to the 16 MiB warm floor, oldest first.
        let evicted = spare.trim_above(16 << 20);
        assert_eq!(
            evicted.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(spare.bytes, 16 << 20);

        // Critical pressure trims the remaining floor to 0.
        let rest = spare.trim_to(0);
        assert_eq!(rest.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![5, 6]);
        assert_eq!(spare.bytes, 0);
    }

    fn spare_has(raw: usize, size: u64) -> bool {
        UAV_SPARE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .kept
            .iter()
            .any(|(b, s)| b.as_raw() as usize == raw && *s == size)
    }

    fn wait_submissions_idle() {
        let start = Instant::now();
        while STARTED.load(Ordering::Acquire) + crate::queue::queued()
            > FINISHED.load(Ordering::Acquire)
            && start.elapsed().as_millis() < 500
        {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn host_list_uav_pool_lifecycle_retire_trim_and_info_queue() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let Some(host) = host() else {
            return;
        };
        let _mode = PoolModeGuard::set(2);
        let _ = host.log_messages();

        let fresh64 = host.uav_buffer(crate::ahead::SIZE).expect("64 KiB uav_buffer");
        assert_eq!(unsafe { fresh64.GetDesc() }.Width, crate::ahead::SIZE);
        drop(fresh64);

        // Source and destination buffers for `between` + `card_budget` + `indirect_scratch` + `batch_place`.
        // Use 1 MiB / 2 MiB buckets alongside 64 KiB `card_budget` & `batch_place` so parallel tests
        // sharing the 64 KiB bucket cannot steal the pointer-tracked buffers mid-assertion.
        let src = host.uav_buffer_now(128 << 10).expect("src uav");
        let dst = host.uav_buffer_now(128 << 10).expect("dst uav");
        assert_eq!(unsafe { src.GetDesc() }.Width, 128 << 10);

        let [ind1m_raw, ind2m_raw, bet4m_raw] = {
            let list = host.new_list().expect("new_list");
            let _budget_gpu = list.budget().expect("budget with card_budget");
            let card_ref = list.card_budget.get().and_then(|c| c.as_ref()).expect("card_budget");
            assert_eq!(unsafe { card_ref.GetDesc() }.Width, 64 << 10);

            let ind1m = list.indirect_scratch(700 << 10).expect("indirect 1 MiB");
            assert_eq!(unsafe { ind1m.GetDesc() }.Width, 1 << 20);
            // Grow mid-list to 2 MiB: 1 MiB buffer moves to `retired_uavs`.
            let ind2m = list.indirect_scratch((1 << 20) + 4096).expect("indirect 2 MiB");
            assert_eq!(unsafe { ind2m.GetDesc() }.Width, 2 << 20);

            let (batch_buf, batch_off, batch_state) = list.batch_place().expect("batch_place");
            assert_eq!(batch_off, 0);
            assert_eq!(batch_state, D3D12_RESOURCE_STATE_COMMON);
            assert_eq!(unsafe { batch_buf.GetDesc() }.Width, 64 << 10);
            unsafe {
                list.list.ResourceBarrier(&[transition(
                    &batch_buf,
                    D3D12_RESOURCE_STATE_COMMON,
                    D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
                )]);
            }

            let bet4m = list.between(host, (2 << 20) + 4096).expect("between 4 MiB");
            assert_eq!(unsafe { bet4m.GetDesc() }.Width, 4 << 20);
            unsafe {
                list.list.CopyBufferRegion(&bet4m, 0, &src, 0, 4096);
                list.list.ResourceBarrier(&[transition(
                    &bet4m,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                list.list.CopyBufferRegion(&dst, 0, &bet4m, 0, 4096);
            }

            list.close();
            host.execute(&[&list]);
            assert_eq!(host.log_messages(), None, "D3D12 debug layer clean on first run");

            let ptrs = [
                ind1m.as_raw() as usize,
                ind2m.as_raw() as usize,
                bet4m.as_raw() as usize,
            ];
            drop((ind1m, ind2m, batch_buf, bet4m));

            // `reset()` recycles `between` (4 MiB) and outgrown `ind1m` (1 MiB), keeping `ind2m` on `list`.
            wait_submissions_idle();
            list.reset();
            assert!(spare_has(ptrs[0], 1 << 20));
            assert!(spare_has(ptrs[2], 4 << 20));
            assert!(!spare_has(ptrs[1], 2 << 20));

            // Dropping `list` recycles `card_budget`, `ind2m` (2 MiB), and `batch_buf` into `UAV_SPARE`.
            wait_submissions_idle();
            drop(list);
            assert!(spare_has(ptrs[1], 2 << 20));
            ptrs
        };

        // Verify in-flight retirement queuing without touching global `STARTED`/`FINISHED`.
        let inflight_buf = host.uav_buffer_now(8 << 20).expect("inflight 8 MiB");
        let inflight_raw = inflight_buf.as_raw() as usize;
        let sentinel = u64::MAX - 8;
        retire_uavs_at(vec![(inflight_buf, 8 << 20)], sentinel, sentinel - 1);
        assert!(
            !spare_has(inflight_raw, 8 << 20),
            "in-flight buffer must stay in RETIRED_UAVS until its submission completes"
        );
        drain_retired_uavs(sentinel);
        assert!(
            spare_has(inflight_raw, 8 << 20),
            "completed submission moves retired UAV buffer into UAV_SPARE"
        );

        // Second `HostList` reuses the pooled buffers with exact Width and 0 D3D12 errors.
        let hits_before = UAV_SPARE_STATS[0].load(Ordering::Relaxed);
        {
            let list2 = host.new_list().expect("new_list 2");
            let _ = list2.budget().expect("budget 2");
            let ind2 = list2.indirect_scratch((1 << 20) + 4096).expect("indirect 2 MiB reuse");
            assert_eq!(unsafe { ind2.GetDesc() }.Width, 2 << 20);
            assert_eq!(ind2.as_raw() as usize, ind2m_raw);

            let (batch2, _, _) = list2.batch_place().expect("batch_place 2");
            assert_eq!(unsafe { batch2.GetDesc() }.Width, 64 << 10);
            unsafe {
                list2.list.ResourceBarrier(&[transition(
                    &batch2,
                    D3D12_RESOURCE_STATE_COMMON,
                    D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
                )]);
            }

            let bet2 = list2.between(host, (2 << 20) + 4096).expect("between 4 MiB reuse");
            assert_eq!(unsafe { bet2.GetDesc() }.Width, 4 << 20);
            assert_eq!(bet2.as_raw() as usize, bet4m_raw);
            unsafe {
                list2.list.CopyBufferRegion(&bet2, 0, &src, 0, 4096);
                list2.list.ResourceBarrier(&[transition(
                    &bet2,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                list2.list.CopyBufferRegion(&dst, 0, &bet2, 0, 4096);
            }

            list2.close();
            host.execute(&[&list2]);
            assert_eq!(host.log_messages(), None, "D3D12 debug layer clean on pooled reuse");
            assert!(UAV_SPARE_STATS[0].load(Ordering::Relaxed) >= hits_before + 3);
            wait_submissions_idle();
        }

        // Pressure trim evicts all pooled buffers cleanly.
        let trimmed_before = UAV_SPARE_STATS[4].load(Ordering::Relaxed);
        trim_uav_spare_to(0);
        assert!(!spare_has(ind1m_raw, 1 << 20));
        assert!(!spare_has(ind2m_raw, 2 << 20));
        assert!(!spare_has(bet4m_raw, 4 << 20));
        assert!(!spare_has(inflight_raw, 8 << 20));
        assert!(UAV_SPARE_STATS[4].load(Ordering::Relaxed) >= trimmed_before + (15 << 20));
        assert_eq!(host.log_messages(), None);
    }

    /// RTX hardware benchmark comparing `VICTORY_UAV_POOL=0` vs `VICTORY_UAV_POOL=1`
    /// across 64 `HostList`s allocating `card_budget` + `batch_place` + `between`,
    /// resetting, and dropping (the exact `IDLE_MOST` / `allocator_trim` churn pattern).
    #[test]
    #[ignore = "hardware timing benchmark; run with --ignored --nocapture"]
    fn bench_host_list_uav_pool_ab() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let Some(host) = host() else {
            return;
        };
        const LISTS_N: usize = 64;
        const ROUNDS: usize = 7;

        let run_mode = |pool_enabled: bool| -> (Vec<f64>, u64, u64) {
            let _mode = PoolModeGuard::set(if pool_enabled { 2 } else { 1 });
            clear_pool_for_test();
            // Pre-create `HostList` shells once if testing reset+drop, or create+drop full `HostList`s!
            // In gameplay (`IDLE_MOST` overflow & `allocator_trim`), `HostList`s are created, record
            // `card_budget` + `batch_place` + `between`, reset once, and drop.
            // Warm the pool with one wave when enabled so steady-state reuse is measured fairly,
            // and also report cold-to-warm behavior.
            let record_wave = |l: &HostList| {
                let _ = l.budget();
                if let Some((batch, _, state)) = l.batch_place() {
                    unsafe {
                        l.list.ResourceBarrier(&[transition(
                            &batch,
                            state,
                            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
                        )]);
                    }
                }
                if let Some(bet) = l.between(host, 4096) {
                    unsafe {
                        l.list.ResourceBarrier(&[transition(
                            &bet,
                            D3D12_RESOURCE_STATE_COMMON,
                            D3D12_RESOURCE_STATE_COPY_SOURCE,
                        )]);
                    }
                }
                l.close();
            };

            if pool_enabled {
                let mut warm = Vec::with_capacity(LISTS_N);
                for _ in 0..LISTS_N {
                    let l = host.new_list().expect("warm list");
                    record_wave(&l);
                    warm.push(l);
                }
                drop(warm);
            }
            UAV_SPARE_STATS[0].store(0, Ordering::SeqCst);
            UAV_SPARE_STATS[1].store(0, Ordering::SeqCst);
            let made_start = MADE[4].load(Ordering::Relaxed);
            let mut samples_ms = Vec::with_capacity(ROUNDS);
            for _ in 0..ROUNDS {
                let t0 = Instant::now();
                let mut lists = Vec::with_capacity(LISTS_N);
                for _ in 0..LISTS_N {
                    let l = host.new_list().expect("bench list");
                    record_wave(&l);
                    l.reset();
                    if let Some(bet) = l.between(host, 4096) {
                        unsafe {
                            l.list.ResourceBarrier(&[transition(
                                &bet,
                                D3D12_RESOURCE_STATE_COMMON,
                                D3D12_RESOURCE_STATE_COPY_SOURCE,
                            )]);
                        }
                    }
                    l.close();
                    lists.push(l);
                }
                drop(lists);
                samples_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
            }
            samples_ms.sort_by(f64::total_cmp);
            let made_delta = MADE[4].load(Ordering::Relaxed) - made_start;
            let hits = UAV_SPARE_STATS[0].load(Ordering::Relaxed);
            (samples_ms, made_delta, hits)
        };

        let (off_ms, off_made, off_hits) = run_mode(false);
        let (on_ms, on_made, on_hits) = run_mode(true);
        clear_pool_for_test();

        let off_med = off_ms[ROUNDS / 2];
        let on_med = on_ms[ROUNDS / 2];
        let speedup = off_med / on_med.max(1e-6);
        println!(
            "UAV_POOL=0 (64 new+dropped HostLists x 4 UAV ops): median {off_med:.2} ms (samples {off_ms:.2?}, D3D12 buffers made={off_made}, hits={off_hits})"
        );
        println!(
            "UAV_POOL=1 (64 new+dropped HostLists x 4 UAV ops): median {on_med:.2} ms (samples {on_ms:.2?}, D3D12 buffers made={on_made}, hits={on_hits})"
        );
        println!("Full HostList churn speedup: {speedup:.2}x ({:.2} ms saved per 64-list wave)", off_med - on_med);

        // Also measure steady-state `HostList::reset()` + `between()` (64 live HostLists x 4 between copies).
        let run_reset_between = |pool_enabled: bool| -> Vec<f64> {
            let _mode = PoolModeGuard::set(if pool_enabled { 2 } else { 1 });
            clear_pool_for_test();
            let lists: Vec<HostList> = (0..LISTS_N).map(|_| host.new_list().expect("live list")).collect();
            if pool_enabled {
                for l in &lists {
                    for _ in 0..4 {
                        if let Some(bet) = l.between(host, 4096) {
                            unsafe {
                                l.list.ResourceBarrier(&[transition(
                                    &bet,
                                    D3D12_RESOURCE_STATE_COMMON,
                                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                                )]);
                            }
                        }
                    }
                    l.close();
                    l.reset();
                }
            } else {
                for l in &lists {
                    l.close();
                    l.reset();
                }
            }
            let mut samples = Vec::with_capacity(ROUNDS);
            for _ in 0..ROUNDS {
                let t0 = Instant::now();
                for l in &lists {
                    for _ in 0..4 {
                        if let Some(bet) = l.between(host, 4096) {
                            unsafe {
                                l.list.ResourceBarrier(&[transition(
                                    &bet,
                                    D3D12_RESOURCE_STATE_COMMON,
                                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                                )]);
                            }
                        }
                    }
                    l.close();
                    l.reset();
                }
                samples.push(t0.elapsed().as_secs_f64() * 1000.0);
            }
            samples.sort_by(f64::total_cmp);
            drop(lists);
            samples
        };
        let reset_off = run_reset_between(false);
        let reset_on = run_reset_between(true);
        clear_pool_for_test();
        let roff = reset_off[ROUNDS / 2];
        let ron = reset_on[ROUNDS / 2];
        println!(
            "UAV_POOL=0 (64 HostLists reset + 4x between): median {roff:.2} ms (samples {reset_off:.2?})"
        );
        println!(
            "UAV_POOL=1 (64 HostLists reset + 4x between): median {ron:.2} ms (samples {reset_on:.2?})"
        );
        println!("Reset+between speedup: {:.2}x ({:.2} ms saved per 256 between copies)", roff / ron.max(1e-6), roff - ron);
    }
}
