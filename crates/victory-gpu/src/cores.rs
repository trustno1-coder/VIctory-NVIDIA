//! Which processors our own threads run on, where a processor has two kinds
//! of cores. `cpu_sets` and `keep_on_fast_cores` are fxceless/victory-fork's
//! (`framelog.rs`, commit 0b0074e0, GPL-3.0-or-later, as this).

/// The system's processor sets of group 0: each one's id and its efficiency
/// class (higher is faster).
fn cpu_sets() -> Vec<(u32, u8)> {
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
            sets.push((set.Id, set.EfficiencyClass));
        }
        at += info.Size as usize;
    }
    sets
}

/// The sets of the fastest class, if there are slower ones too.
fn fast_of(sets: &[(u32, u8)]) -> Vec<u32> {
    let best = sets.iter().map(|c| c.1).max().unwrap_or(0);
    let fast: Vec<u32> = sets.iter().filter(|c| c.1 == best).map(|c| c.0).collect();
    if fast.len() == sets.len() { Vec::new() } else { fast }
}

/// The last-level cache of each processor set of group 0, by set id: its
/// bytes, and the mask of the processors that share it (which tells one
/// core complex from another). None known: empty.
fn l3_of_sets() -> Vec<(u32, u32, u64)> {
    use windows_sys::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, GetSystemCpuSetInformation, RelationCache, SYSTEM_CPU_SET_INFORMATION,
        SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // The largest cache of each logical processor of group 0, by its index,
    // and the processors that share it.
    let mut by_processor = [(0u32, 0u64); 64];
    let mut bytes = 0u32;
    // SAFETY: a size query, then the call into a buffer of that size.
    unsafe { GetLogicalProcessorInformationEx(RelationCache, std::ptr::null_mut(), &mut bytes) };
    let mut buffer = vec![0u64; (bytes as usize).div_ceil(8).max(1)];
    // SAFETY: as above; the buffer holds `bytes` bytes, aligned for the records.
    let ok = unsafe { GetLogicalProcessorInformationEx(RelationCache, buffer.as_mut_ptr().cast(), &mut bytes) };
    let mut at = 0usize;
    while ok != 0 && at + size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>() <= bytes as usize {
        // SAFETY: a record inside the buffer; each says its own size.
        let info = unsafe { &*(buffer.as_ptr().cast::<u8>().add(at).cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>()) };
        if info.Size == 0 {
            break;
        }
        if info.Relationship == RelationCache {
            // SAFETY: a cache record (asked for), with at least its first group mask.
            let cache = unsafe { &info.Anonymous.Cache };
            let mask = unsafe { cache.Anonymous.GroupMask };
            if mask.Group == 0 {
                for (p, last) in by_processor.iter_mut().enumerate() {
                    if mask.Mask >> p & 1 != 0 && cache.CacheSize > last.0 {
                        *last = (cache.CacheSize, mask.Mask as u64);
                    }
                }
            }
        }
        at += info.Size as usize;
    }
    // Processor indices to set ids.
    let mut bytes = 0u32;
    // SAFETY: as for `cpu_sets`.
    unsafe { GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &mut bytes, GetCurrentProcess(), 0) };
    let mut buffer = vec![0u64; (bytes as usize).div_ceil(8).max(1)];
    // SAFETY: as above.
    let ok = unsafe { GetSystemCpuSetInformation(buffer.as_mut_ptr().cast(), bytes, &mut bytes, GetCurrentProcess(), 0) };
    let mut caches = Vec::new();
    let mut at = 0usize;
    while ok != 0 && at + size_of::<SYSTEM_CPU_SET_INFORMATION>() <= bytes as usize {
        // SAFETY: a record inside the buffer; each says its own size.
        let info = unsafe { &*(buffer.as_ptr().cast::<u8>().add(at).cast::<SYSTEM_CPU_SET_INFORMATION>()) };
        if info.Size == 0 {
            break;
        }
        // SAFETY: the record's type is CpuSet (the only one there is).
        let set = unsafe { &info.Anonymous.CpuSet };
        if set.Group == 0 && let Some(&(size, shared)) = by_processor.get(set.LogicalProcessorIndex as usize) {
            caches.push((set.Id, size, shared));
        }
        at += info.Size as usize;
    }
    caches
}

/// Whether our threads keep to one core complex where the processor has
/// several of one kind (`VICTORY_CCD_CORES=1`; off by default while it is
/// tested; `VICTORY_AB` can flip it while the title runs). On a Ryzen of two
/// complexes (7900X, 7950X, 9900X, 9950X, and the X3D ones) every core reports
/// the same efficiency class, so nothing was asked for: the submitting thread
/// and those it shares its work with ran on either, and every lock, claim and
/// handoff between two of them could cross from one complex to the other:
/// 180 ns a way on a 7900X, against 20 ns inside one.
fn ccd_cores() -> bool {
    crate::asked_for!("VICTORY_CCD_CORES")
}

/// The sets of one last-level cache (`l3`: bytes and sharers by set id), if
/// there are several: the one shared by the most sets (of those alike, the
/// largest cache, which is the stacked one of an X3D; then the lowest id), at
/// least eight of them.
fn fast_by_complex(sets: &[(u32, u8)], l3: &[(u32, u32, u64)]) -> Vec<u32> {
    let cache = |id: u32| l3.iter().find(|s| s.0 == id).map_or((0, 0), |s| (s.1, s.2));
    let mut complexes: Vec<(u64, u32, Vec<u32>)> = Vec::new();
    for s in sets {
        let (size, mask) = cache(s.0);
        if mask == 0 {
            return Vec::new();
        }
        match complexes.iter_mut().find(|c| c.0 == mask) {
            Some(c) => c.2.push(s.0),
            None => complexes.push((mask, size, vec![s.0])),
        }
    }
    if complexes.len() < 2 {
        return Vec::new();
    }
    use std::cmp::Reverse;
    let best = complexes.into_iter().min_by_key(|c| (Reverse(c.2.len()), Reverse(c.1), c.2.iter().copied().min()));
    best.map(|c| c.2).filter(|f| f.len() >= 8).unwrap_or_default()
}

/// The sets our threads are asked to run on: those of the fastest class if
/// there are slower ones, else those of one core complex ([`ccd_cores`]);
/// none: any. `now`: as things are at this moment (else whether there could
/// be any, for threads to ask at all).
fn fast_sets(sets: &[(u32, u8)], now: bool) -> Vec<u32> {
    static L3: std::sync::OnceLock<Vec<(u32, u32, u64)>> = std::sync::OnceLock::new();
    fast_sets_with(sets, || L3.get_or_init(l3_of_sets), !now || ccd_cores())
}

/// [`fast_sets`] with the caches given, and whether one complex is asked for.
fn fast_sets_with<'a>(sets: &[(u32, u8)], l3: impl FnOnce() -> &'a [(u32, u32, u64)], complex: bool) -> Vec<u32> {
    let by_class = fast_of(sets);
    if !by_class.is_empty() || !complex {
        return by_class;
    }
    fast_by_complex(sets, l3())
}

/// Asks the system to run that thread on those sets (none: any).
fn place(thread: windows_sys::Win32::Foundation::HANDLE, fast: &[u32]) -> bool {
    use windows_sys::Win32::System::Threading::SetThreadSelectedCpuSets;
    let at = if fast.is_empty() { std::ptr::null() } else { fast.as_ptr() };
    // SAFETY: a thread handle that is open, and the system's own set ids.
    unsafe { SetThreadSelectedCpuSets(thread, at, fast.len() as u32) != 0 }
}

/// Our threads that asked, by a handle of each kept open for [`settle`].
static ASKED: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

/// Tells each of our threads that asked where to run now ([`ccd_cores`] may
/// have changed).
#[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
pub(crate) fn settle() {
    let sets = cpu_sets();
    let asked = ASKED.lock().unwrap_or_else(|e| e.into_inner());
    let fast = fast_sets(&sets, true);
    for &thread in asked.iter() {
        place(thread as _, &fast);
    }
    tracing::info!(fast = fast.len(), of = sets.len(), threads = asked.len(), "our threads' processors (VICTORY_CCD_CORES)");
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
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, GetCurrentThreadId, OpenThread, SetThreadPriority, THREAD_SET_LIMITED_INFORMATION,
    };
    if let Some(priority) = helper_priority() {
        // SAFETY: this thread's own priority.
        let set = unsafe { SetThreadPriority(GetCurrentThread(), priority) } != 0;
        tracing::debug!(what, priority, set, "the thread's priority (VICTORY_HELPER_PRIORITY)");
    }
    if stage() < from {
        return;
    }
    static COULD: std::sync::OnceLock<(bool, usize)> = std::sync::OnceLock::new();
    let &(could, of) = COULD.get_or_init(|| {
        let sets = cpu_sets();
        // (whether there can be fast sets at all: `VICTORY_CCD_CORES` may
        // change while the title runs, and `settle` follows it)
        (!fast_sets(&sets, false).is_empty(), sets.len())
    });
    // All alike (or nothing known): nothing to ask for.
    if !could {
        return;
    }
    // SAFETY: our own thread; the handle stays open for `settle`.
    let thread = unsafe { OpenThread(THREAD_SET_LIMITED_INFORMATION, 0, GetCurrentThreadId()) };
    if thread.is_null() {
        tracing::warn!(what, error = %std::io::Error::last_os_error(), "the thread could not be kept on the fast cores");
        return;
    }
    let mut asked = ASKED.lock().unwrap_or_else(|e| e.into_inner());
    let fast = fast_sets(&cpu_sets(), true);
    if place(thread, &fast) {
        asked.push(thread as usize);
        tracing::info!(what, fast = fast.len(), of, "the thread is kept on the fast cores (VICTORY_FAST_CORES=0: not; VICTORY_CCD_CORES)");
    } else {
        tracing::warn!(what, error = %std::io::Error::last_os_error(), "the thread could not be kept on the fast cores");
        // SAFETY: the handle opened above, not kept.
        unsafe { CloseHandle(thread) };
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

    #[test]
    fn the_fast_cores_are_those_of_the_best_class_if_there_is_a_worse_one() {
        assert_eq!(fast_of(&[(256, 1), (257, 1), (258, 0)]), [256, 257]);
        assert!(fast_of(&[(256, 0), (257, 0)]).is_empty());
        assert!(fast_of(&[]).is_empty());
        assert!(!cpu_sets().is_empty());
        std::thread::spawn(|| keep_on_fast_cores("test")).join().unwrap();
    }

    const MB: u32 = 1 << 20;

    /// `n` sets of one class from id 256.
    fn alike(n: u32) -> Vec<(u32, u8)> {
        (0..n).map(|i| (256 + i, 0)).collect()
    }

    /// The caches of `n` sets from id 256, `per` of them to a complex, with
    /// the bytes `size` gives each set.
    fn caches(n: u32, per: u32, size: impl Fn(u32) -> u32) -> Vec<(u32, u32, u64)> {
        (0..n).map(|i| (256 + i, size(i), ((1u64 << per) - 1) << (i / per * per))).collect()
    }

    #[test]
    fn one_complex_is_kept_to_where_there_are_several() {
        // 7900X: two complexes of 12 sets, caches alike: the first.
        let l3 = caches(24, 12, |_| 32 * MB);
        assert_eq!(fast_by_complex(&alike(24), &l3), (256..268).collect::<Vec<_>>());
        // 9950X3D with the stacked cache on the second complex: that one.
        let x3d = caches(32, 16, |i| if i < 16 { 32 * MB } else { 96 * MB });
        assert_eq!(fast_by_complex(&alike(32), &x3d), (272..288).collect::<Vec<_>>());
        // Of complexes of unlike size, the larger (12 and 8 sets).
        let uneven: Vec<_> = (0..20).map(|i| (256 + i, 32 * MB, if i < 8 { 0xff } else { 0xfff00 })).collect();
        assert_eq!(fast_by_complex(&alike(20), &uneven), (264..276).collect::<Vec<_>>());
        // One complex (7800X3D, 9700X): nothing.
        assert!(fast_by_complex(&alike(16), &caches(16, 16, |_| 96 * MB)).is_empty());
        // Caches not known, or not for every set: nothing.
        assert!(fast_by_complex(&alike(24), &[]).is_empty());
        assert!(fast_by_complex(&alike(24), &l3[..12]).is_empty());
        // Complexes of under eight sets (a 5600X-like part of two): nothing.
        assert!(fast_by_complex(&alike(12), &caches(12, 6, |_| 16 * MB)).is_empty());
    }

    #[test]
    fn the_switch_decides_only_where_the_cores_are_of_one_class() {
        let l3 = caches(24, 12, |_| 32 * MB);
        assert_eq!(fast_sets_with(&alike(24), || &l3, true), (256..268).collect::<Vec<_>>());
        assert!(fast_sets_with(&alike(24), || panic!("caches read with the switch off"), false).is_empty());
        // Eight fast cores (two sets each) and eight slow ones, the caches told
        // to be of other complexes too: the class decides.
        let hybrid: Vec<(u32, u8)> = (0..16).map(|n| (n, 1)).chain((16..24).map(|n| (n, 0))).collect();
        let l3: Vec<(u32, u32, u64)> = (0..24).map(|n| (n, 30 * MB, 1 << (n / 4))).collect();
        for complex in [true, false] {
            assert_eq!(fast_sets_with(&hybrid, || &l3, complex), (0..16).collect::<Vec<_>>());
        }
    }

    #[test]
    fn the_caches_here_are_known() {
        let sets = cpu_sets();
        let l3 = l3_of_sets();
        let mut shared: Vec<u64> = l3.iter().map(|c| c.2).collect();
        shared.sort_unstable();
        shared.dedup();
        println!("{} sets, {} last-level caches, one complex: {} sets", sets.len(), shared.len(), fast_by_complex(&sets, &l3).len());
        assert_eq!(l3.len(), sets.len());
        assert!(!shared.is_empty() && !shared.contains(&0));
        std::thread::spawn(|| {
            keep_helper_on_fast_cores("test");
            settle();
        })
        .join()
        .unwrap();
    }
}
