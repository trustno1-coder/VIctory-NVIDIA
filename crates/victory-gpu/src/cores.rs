//! Which processors our own threads run on, where a processor has two kinds
//! of cores, or its cores in more than one group around a cache of their own
//! (a Ryzen of two dies: 9900X, 9950X, 7900X, 5900X ...). `cpu_sets` and
//! `keep_on_fast_cores` are fxceless/victory-fork's (`framelog.rs`, commit
//! 0b0074e0, GPL-3.0-or-later, as this).

/// One of the system's processor sets of group 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Set {
    id: u32,
    /// Efficiency class: higher is faster.
    class: u8,
    /// The last-level cache it shares (a Ryzen's die: its L3).
    cache: u8,
    /// The system's ranking of it for single threads (CPPC's preferred
    /// cores; 0 where Windows does not say).
    rank: u8,
}

/// The system's processor sets of group 0.
fn cpu_sets() -> Vec<Set> {
    use windows_sys::Win32::System::SystemInformation::{GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut bytes = 0u32;
    // SAFETY: a size query, then the call into a buffer of that size.
    unsafe { GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &mut bytes, GetCurrentProcess(), 0) };
    let mut buffer = vec![0u64; (bytes as usize).div_ceil(8).max(1)];
    // SAFETY: as above; the buffer holds `bytes` bytes, aligned for the records.
    let ok = unsafe { GetSystemCpuSetInformation(buffer.as_mut_ptr().cast(), bytes, &mut bytes, GetCurrentProcess(), 0) };
    let mut sets = Vec::new();
    let mut at = 0usize;
    while ok != 0 && at + size_of::<SYSTEM_CPU_SET_INFORMATION>() <= bytes as usize {
        // SAFETY: a record inside the buffer; each says its own size.
        let info = unsafe { &*(buffer.as_ptr().cast::<u8>().add(at).cast::<SYSTEM_CPU_SET_INFORMATION>()) };
        if info.Size == 0 {
            break;
        }
        // SAFETY: the record's type is CpuSet (the only one there is).
        let set = unsafe { &info.Anonymous.CpuSet };
        if set.Group == 0 {
            // SAFETY: a byte of the union's four, zero where it is reserved.
            let rank = unsafe { set.Anonymous2.SchedulingClass };
            sets.push(Set { id: set.Id, class: set.EfficiencyClass, cache: set.LastLevelCacheIndex, rank });
        }
        at += info.Size as usize;
    }
    sets
}

/// The sets of the fastest class, if there are slower ones too.
fn fast_of(sets: &[Set]) -> Vec<u32> {
    let best = sets.iter().map(|c| c.class).max().unwrap_or(0);
    let fast: Vec<u32> = sets.iter().filter(|c| c.class == best).map(|c| c.id).collect();
    if fast.len() == sets.len() { Vec::new() } else { fast }
}

/// Where all cores are of one kind but share more than one last-level
/// cache (a Ryzen 9 9900X: two dies of six cores, 32 MB of L3 each): the
/// sets of one cache, that of the best-ranked core (the system's preferred
/// cores; a tie: the first cache, a Ryzen's first die). Otherwise none.
///
/// The threads a frame waits for hand their work to each other (the
/// submitting thread, the threads that spread its refreshes, those that
/// land copies, the one that makes the heaps' records): on one die what
/// one wrote is in the cache the next reads; across the two it goes over
/// the link between them, at several times the latency (on Zen 4 and 5
/// about 70-100 ns against 10-15). The title's own threads are not asked
/// for and keep both dies.
fn one_cache_of(sets: &[Set]) -> Vec<u32> {
    let Some(first) = sets.first() else { return Vec::new() };
    if sets.iter().any(|s| s.class != first.class) || sets.iter().all(|s| s.cache == first.cache) {
        return Vec::new();
    }
    let best = sets.iter().max_by(|a, b| a.rank.cmp(&b.rank).then(b.cache.cmp(&a.cache))).map_or(first.cache, |s| s.cache);
    sets.iter().filter(|s| s.cache == best).map(|s| s.id).collect()
}

/// `VICTORY_ONE_CCD`: on a processor whose cores are alike but in groups
/// around caches of their own, the threads `VICTORY_FAST_CORES` 1 and 2
/// name (those a frame waits for, and those they share their work with)
/// are kept on one group ([`one_cache_of`]); 0: not (as before). Those of
/// 3 (the many that translate lists) are left to every core either way:
/// with the helpers they would be more threads than one die runs at once.
fn one_ccd() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_ONE_CCD").map_or(true, |v| v.trim() != "0"))
}

/// `VICTORY_FAST_CORES`: 0: no thread asks; 1: the threads a frame waits for
/// one by one; 2 (the default): the threads they share their work with too;
/// 3: those that work beside the rest as well (lists translated, descriptors
/// made): more threads by then than the fast cores run at a time, while the
/// slow ones stood idle.
fn stage() -> u32 {
    static STAGE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *STAGE.get_or_init(|| std::env::var("VICTORY_FAST_CORES").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(2))
}

/// Asks the system to run the calling thread, which a frame waits for, on
/// the fast cores only, where the processor has two kinds. That fork's submit
/// thread was found on a slow one in 1,529 looks of 24,021. (A selection of
/// processors is a preference the scheduler keeps unless nothing of it is
/// free.) The title's threads are not asked for.
pub(crate) fn keep_on_fast_cores(what: &str) {
    ask(what, 1);
}

/// [`keep_on_fast_cores`] for one of the threads a submission's preparing or
/// a landing shares its work with.
pub(crate) fn keep_helper_on_fast_cores(what: &str) {
    ask(what, 2);
}

/// [`keep_on_fast_cores`] for one of many threads whose work is ahead of what
/// a frame waits for.
pub(crate) fn keep_many_on_fast_cores(what: &str) {
    ask(what, 3);
}

/// The priority our own threads that a frame waits for, and those they share
/// their work with, run at (`VICTORY_HELPER_PRIORITY=<n>`, a thread priority
/// from -2 to 2; not given: as made, normal, the submitting thread above
/// normal). A frame's timeline in the first gameplay area (run timeline,
/// 2026-11-26) had such threads woken and not running yet for 4-11 ms of its
/// chain: the listing threads up to 5.4 ms at a time, while the title's own
/// workers held every processor.
fn helper_priority() -> Option<i32> {
    static PRIORITY: std::sync::OnceLock<Option<i32>> = std::sync::OnceLock::new();
    *PRIORITY.get_or_init(|| {
        std::env::var("VICTORY_HELPER_PRIORITY").ok().and_then(|v| v.trim().parse::<i32>().ok()).map(|p| p.clamp(-2, 2))
    })
}

fn ask(what: &str, from: u32) {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, SetThreadSelectedCpuSets};
    if let Some(priority) = helper_priority() {
        // SAFETY: this thread's own priority.
        let set = unsafe { SetThreadPriority(GetCurrentThread(), priority) } != 0;
        tracing::debug!(what, priority, set, "the thread's priority (VICTORY_HELPER_PRIORITY)");
    }
    if stage() < from {
        return;
    }
    static FAST: std::sync::OnceLock<(Vec<u32>, Vec<u32>, usize)> = std::sync::OnceLock::new();
    let (fast, one_cache, of) = FAST.get_or_init(|| {
        let sets = cpu_sets();
        (fast_of(&sets), one_cache_of(&sets), sets.len())
    });
    if !fast.is_empty() {
        // SAFETY: our own thread and a list of the system's own set ids.
        let ok = unsafe { SetThreadSelectedCpuSets(GetCurrentThread(), fast.as_ptr(), fast.len() as u32) };
        if ok == 0 {
            tracing::warn!(what, error = %std::io::Error::last_os_error(), "the thread could not be kept on the fast cores");
        } else {
            tracing::info!(what, fast = fast.len(), of, "the thread is kept on the fast cores (VICTORY_FAST_CORES=0: not)");
        }
        return;
    }
    // All alike: on one die of two, if there are two, for the threads a frame waits for.
    if one_cache.is_empty() || from > 2 || !one_ccd() {
        return;
    }
    // SAFETY: our own thread and a list of the system's own set ids.
    let ok = unsafe { SetThreadSelectedCpuSets(GetCurrentThread(), one_cache.as_ptr(), one_cache.len() as u32) };
    if ok == 0 {
        tracing::warn!(what, error = %std::io::Error::last_os_error(), "the thread could not be kept on one die");
    } else {
        tracing::info!(what, sets = one_cache.len(), of, "the thread is kept on one die, beside its cache (VICTORY_ONE_CCD=0: not)");
    }
}

/// Whether the threads the submitting thread waits for run at its priority
/// (`VICTORY_HELPER_PRIORITY`, on unless `0`; `VICTORY_AB` can flip it while
/// the title runs): above normal, ahead of the title's threads and of the
/// threads that translate lists. They are the threads that spread its work
/// (`shadow::wide`, the pools of `land`), the one that makes the heaps'
/// records and the one that passes over the heap's views beside the lists.
///
/// On a processor with six cores (i5-12400F, 12 threads) the title's threads
/// and eight translating lists kept every core busy, and the submitting
/// thread (above normal since `queue::behind`) stood waiting for helpers at
/// normal priority that were not running. Measured with `VICTORY_AB` in one
/// run (run10, Series S at 540p, medians of 5-second windows in the world,
/// off / on): 8.6 / 9.6 fps, refreshes 23.1 / 17.6 ms a frame, waiting for
/// the heaps' records 6.2 / 2.5 ms, the card idle behind the submitting
/// thread 24.7 / 13.2 ms. Lowering the translating threads below normal as
/// well made each list call about 5 % slower, with no clear gain of its own.
fn helpers_raised() -> bool {
    crate::toggled!("VICTORY_HELPER_PRIORITY")
}

/// A helper's priority ([`helpers_raised`]). [`Self::keep`] is called before
/// each piece of work; it asks the system only when the switch has changed.
pub(crate) struct Priority {
    applied: Option<bool>,
}

impl Priority {
    pub(crate) fn new() -> Self {
        let mut priority = Self { applied: None };
        priority.keep();
        priority
    }

    pub(crate) fn keep(&mut self) {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_NORMAL,
        };
        let on = helpers_raised();
        if self.applied == Some(on) {
            return;
        }
        let priority = if on { THREAD_PRIORITY_ABOVE_NORMAL } else { THREAD_PRIORITY_NORMAL };
        // SAFETY: our own thread.
        unsafe { SetThreadPriority(GetCurrentThread(), priority) };
        self.applied = Some(on);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(id: u32, class: u8, cache: u8, rank: u8) -> Set {
        Set { id, class, cache, rank }
    }

    #[test]
    fn the_fast_cores_are_those_of_the_best_class_if_there_is_a_worse_one() {
        assert_eq!(fast_of(&[set(256, 1, 0, 0), set(257, 1, 0, 0), set(258, 0, 0, 0)]), [256, 257]);
        assert!(fast_of(&[set(256, 0, 0, 0), set(257, 0, 0, 0)]).is_empty());
        assert!(fast_of(&[]).is_empty());
        assert!(!cpu_sets().is_empty());
        std::thread::spawn(|| keep_on_fast_cores("test")).join().unwrap();
    }

    #[test]
    fn two_dies_of_alike_cores_keep_one_die() {
        // A 9900X: 24 sets, 12 on each die's cache; no ranking given: the first die.
        let two: Vec<Set> = (0..24).map(|i| set(256 + i, 0, (i / 12) as u8 * 12, 0)).collect();
        assert_eq!(one_cache_of(&two), (256..268).collect::<Vec<u32>>());
        // The system's preferred cores on the second die: that one.
        let mut ranked = two.clone();
        ranked[15].rank = 9;
        assert_eq!(one_cache_of(&ranked), (268..280).collect::<Vec<u32>>());
        // One die (a 9800X3D), or two kinds of cores (left to `fast_of`): none.
        let one: Vec<Set> = (0..16).map(|i| set(256 + i, 0, 0, 0)).collect();
        assert!(one_cache_of(&one).is_empty());
        let hybrid = [set(256, 1, 0, 0), set(257, 0, 1, 0)];
        assert!(one_cache_of(&hybrid).is_empty());
        assert!(one_cache_of(&[]).is_empty());
    }
}
