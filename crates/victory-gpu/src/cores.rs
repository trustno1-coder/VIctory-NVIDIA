//! Which processors our own threads run on, where a processor has two kinds
//! of cores. `cpu_sets` and `keep_on_fast_cores` are fxceless/victory-fork's
//! (`framelog.rs`, commit 0b0074e0, GPL-3.0-or-later, as this). The title's
//! own threads, where it places them itself: [`title_group_affinity`].

use std::ffi::c_void;

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
    static FAST: std::sync::OnceLock<(Vec<u32>, usize)> = std::sync::OnceLock::new();
    let (fast, of) = FAST.get_or_init(|| {
        let sets = cpu_sets();
        (fast_of(&sets), sets.len())
    });
    // All alike (or nothing known): nothing to ask for.
    if fast.is_empty() {
        return;
    }
    // SAFETY: our own thread and a list of the system's own set ids.
    let ok = unsafe { SetThreadSelectedCpuSets(GetCurrentThread(), fast.as_ptr(), fast.len() as u32) };
    if ok == 0 {
        tracing::warn!(what, error = %std::io::Error::last_os_error(), "the thread could not be kept on the fast cores");
    } else {
        tracing::info!(what, fast = fast.len(), of, "the thread is kept on the fast cores (VICTORY_FAST_CORES=0: not)");
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

/// A logical processor of group 0: its number in the group's masks, the
/// core it is on and that core's efficiency class (higher is faster).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Logical {
    index: u8,
    core: u8,
    class: u8,
}

/// Group 0's logical processors ([`cpu_sets`], with where each one is).
fn logical_processors() -> Vec<Logical> {
    use windows_sys::Win32::System::SystemInformation::{GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut bytes = 0u32;
    // SAFETY: a size query, then the call into a buffer of that size.
    unsafe { GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &mut bytes, GetCurrentProcess(), 0) };
    let mut buffer = vec![0u64; (bytes as usize).div_ceil(8).max(1)];
    // SAFETY: as above; the buffer holds `bytes` bytes, aligned for the records.
    let ok = unsafe { GetSystemCpuSetInformation(buffer.as_mut_ptr().cast(), bytes, &mut bytes, GetCurrentProcess(), 0) };
    let mut all = Vec::new();
    let mut at = 0usize;
    while ok != 0 && at + size_of::<SYSTEM_CPU_SET_INFORMATION>() <= bytes as usize {
        // SAFETY: a record inside the buffer; each says its own size.
        let info = unsafe { &*(buffer.as_ptr().cast::<u8>().add(at).cast::<SYSTEM_CPU_SET_INFORMATION>()) };
        if info.Size == 0 {
            break;
        }
        // SAFETY: the record's type is CpuSet (the only one there is).
        let set = unsafe { &info.Anonymous.CpuSet };
        if set.Group == 0 && set.LogicalProcessorIndex < 64 {
            all.push(Logical { index: set.LogicalProcessorIndex, core: set.CoreIndex, class: set.EfficiencyClass });
        }
        at += info.Size as usize;
    }
    all
}

/// The mask of group 0's logical processors on the fastest cores, if some
/// are slower (0: all alike, or nothing known).
fn fast_mask(all: &[Logical]) -> u64 {
    let best = all.iter().map(|l| l.class).max().unwrap_or(0);
    if all.iter().all(|l| l.class == best) {
        return 0;
    }
    all.iter().filter(|l| l.class == best).fold(0, |m, l| m | 1u64 << l.index)
}

/// What a group 0 mask holds, for the log: how many logical processors, on
/// how many cores, how many of them on fast cores, and how many cores hold
/// two of them.
fn describe(mask: u64, all: &[Logical]) -> String {
    let best = all.iter().map(|l| l.class).max().unwrap_or(0);
    let held: Vec<&Logical> = all.iter().filter(|l| mask >> l.index & 1 != 0).collect();
    let mut cores: Vec<u8> = held.iter().map(|l| l.core).collect();
    cores.sort_unstable();
    let shared = cores.windows(2).filter(|w| w[0] == w[1]).count();
    cores.dedup();
    let fast = held.iter().filter(|l| l.class == best).count();
    format!(
        "{} logical on {} cores ({} on fast cores, {} on slow; {} cores with two of them)",
        held.len(),
        cores.len(),
        fast,
        held.len() - fast,
        shared
    )
}

/// A thread the title placed itself ([`title_group_affinity`]): its id, our
/// own handle of it, and the group and mask it asked for.
struct Placed {
    thread: u32,
    handle: usize,
    group: u16,
    mask: usize,
}

static PLACED: std::sync::Mutex<Vec<Placed>> = std::sync::Mutex::new(Vec::new());

/// Whether the title's placed threads go on every fast core instead
/// (0: not read yet, 1: no, 2: yes): `VICTORY_TITLE_FAST_CORES=1`, which
/// `VICTORY_AB` can flip while the title runs ([`title_reapply`]).
static TITLE_FAST: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn title_fast() -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    match TITLE_FAST.load(Relaxed) {
        0 => {
            let on = crate::asked_for!("VICTORY_TITLE_FAST_CORES");
            // (A flip by `title_reapply` meanwhile wins.)
            let _ = TITLE_FAST.compare_exchange(0, if on { 2 } else { 1 }, Relaxed, Relaxed);
            TITLE_FAST.load(Relaxed) == 2
        }
        v => v == 2,
    }
}

/// [`fast_mask`] of this processor, found once.
fn fast_now() -> u64 {
    static FAST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *FAST.get_or_init(|| fast_mask(&logical_processors()))
}

/// The group and mask a thread that asked for `(group, mask)` is given:
/// with `fast`, group 0's fast cores in place of any group 0 mask.
fn chosen(group: u16, mask: usize, fast: bool, fast_mask: u64) -> (u16, usize) {
    if fast && group == 0 && fast_mask != 0 { (0, fast_mask as usize) } else { (group, mask) }
}

/// The title's `SetThreadGroupAffinity`. The title places its threads on
/// processors it picks from what `GetLogicalProcessorInformation` tells it,
/// with a console's like cores in mind; on a processor with fast and slow
/// cores (i9-13900KF: 8 fast ones with two logical processors each, 16 slow
/// ones) a thread a frame waits for can be kept on a slow core, or two of
/// them on one core. The first 128 placements are logged; with
/// `VICTORY_TITLE_FAST_CORES=1` a thread is given all of group 0's fast cores
/// in place of the mask it asked for, and the scheduler spreads them there.
///
/// What the log showed on an i9-13900KF (2026-11-27): every thread the title
/// places stays inside logical processors 0 to 13, seven fast cores with
/// their two logical processors each, as on the console (masks 0x3fff; 0xff,
/// eight threads on four cores; 0x1f00, some eleven threads on five logical
/// processors; 0x3f00; 0x2000); some calls ask for mask 0, which Windows
/// refuses. The eighth fast core and the sixteen slow ones are never asked
/// for. All fast cores in place of those masks measured no gain with
/// `VICTORY_AB` on the traffic road (+3.4 % time per draw, better in 14 of 34
/// paired phases): our own submitting thread and its helpers are kept on the
/// same cores. Off by default; the log of the masks is what it is for.
///
/// # Safety
///
/// As the host's `SetThreadGroupAffinity`: `thread` a thread handle with
/// `THREAD_SET_INFORMATION` and `THREAD_QUERY_INFORMATION`, `asked` null or a
/// readable `GROUP_AFFINITY`, `previous` null or a writable one.
pub unsafe fn title_group_affinity(thread: *mut c_void, asked: *const c_void, previous: *mut c_void) -> i32 {
    use windows_sys::Win32::Foundation::{CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle};
    use windows_sys::Win32::System::SystemInformation::GROUP_AFFINITY;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetThreadId, SetThreadGroupAffinity};
    let asked = asked.cast::<GROUP_AFFINITY>();
    if asked.is_null() {
        // SAFETY: the title's own arguments, passed on (the host refuses them).
        return unsafe { SetThreadGroupAffinity(thread, asked, previous.cast()) };
    }
    // SAFETY: the title passes a GROUP_AFFINITY to read.
    let a = unsafe { *asked };
    // SAFETY: the title's thread handle.
    let id = unsafe { GetThreadId(thread) };
    let fast = title_fast();
    static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 128 {
        let held = if a.Group == 0 { describe(a.Mask as u64, &logical_processors()) } else { String::new() };
        tracing::info!(
            thread = id,
            group = a.Group,
            mask = format_args!("{:#x}", a.Mask),
            held = %held,
            on_fast_cores = fast,
            "the title places a thread (SetThreadGroupAffinity; VICTORY_TITLE_FAST_CORES=1: on all fast cores instead)"
        );
    }
    let mut own: *mut c_void = std::ptr::null_mut();
    // SAFETY: the title's thread handle, duplicated into our own process
    // with its access (which let the title place it).
    let copied = unsafe {
        DuplicateHandle(GetCurrentProcess(), thread, GetCurrentProcess(), &mut own, 0, 0, DUPLICATE_SAME_ACCESS)
    } != 0;
    if copied {
        let mut placed = PLACED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = placed.iter_mut().find(|p| p.thread == id) {
            // SAFETY: our own handle, replaced.
            unsafe { CloseHandle(p.handle as *mut c_void) };
            (p.handle, p.group, p.mask) = (own as usize, a.Group, a.Mask);
        } else {
            placed.push(Placed { thread: id, handle: own as usize, group: a.Group, mask: a.Mask });
        }
    }
    let (group, mask) = chosen(a.Group, a.Mask, fast, fast_now());
    let given = GROUP_AFFINITY { Mask: mask, Group: group, Reserved: [0; 3] };
    // SAFETY: the title's thread handle and out pointer; our own mask.
    unsafe { SetThreadGroupAffinity(thread, &given, previous.cast()) }
}

/// `VICTORY_TITLE_FAST_CORES` switched to `fast` while the title runs
/// (`VICTORY_AB`): every thread the title placed is placed again.
#[cfg(feature = "diagnostics")]
pub(crate) fn title_reapply(fast: bool) {
    use windows_sys::Win32::System::SystemInformation::GROUP_AFFINITY;
    use windows_sys::Win32::System::Threading::SetThreadGroupAffinity;
    let placed = PLACED.lock().unwrap_or_else(|e| e.into_inner());
    TITLE_FAST.store(if fast { 2 } else { 1 }, std::sync::atomic::Ordering::Relaxed);
    let fast_mask = fast_now();
    let mut moved = 0u32;
    for p in placed.iter() {
        let (group, mask) = chosen(p.group, p.mask, fast, fast_mask);
        let given = GROUP_AFFINITY { Mask: mask, Group: group, Reserved: [0; 3] };
        // SAFETY: our own handle of the thread (one that has ended refuses).
        moved += u32::from(unsafe { SetThreadGroupAffinity(p.handle as *mut c_void, &given, std::ptr::null_mut()) } != 0);
    }
    tracing::debug!(fast, moved, of = placed.len(), "the title's placed threads placed again (VICTORY_TITLE_FAST_CORES)");
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

    fn l(index: u8, core: u8, class: u8) -> Logical {
        Logical { index, core, class }
    }

    #[test]
    fn a_mask_is_told_by_its_cores_and_the_fast_ones_replace_it_when_asked() {
        // Two fast cores with two logical processors each, two slow ones.
        let all = [l(0, 0, 1), l(1, 0, 1), l(2, 1, 1), l(3, 1, 1), l(4, 2, 0), l(5, 3, 0)];
        assert_eq!(fast_mask(&all), 0b1111);
        assert_eq!(fast_mask(&all[..4]), 0, "all alike: nothing to prefer");
        assert_eq!(describe(0b11, &all), "2 logical on 1 cores (2 on fast cores, 0 on slow; 1 cores with two of them)");
        assert_eq!(describe(0b110100, &all), "3 logical on 3 cores (1 on fast cores, 2 on slow; 0 cores with two of them)");
        assert_eq!(chosen(0, 0b10000, true, 0b1111), (0, 0b1111));
        assert_eq!(chosen(0, 0b10000, false, 0b1111), (0, 0b10000));
        assert_eq!(chosen(1, 0b1, true, 0b1111), (1, 0b1), "another group stays as asked");
        assert_eq!(chosen(0, 0b10000, true, 0), (0, 0b10000), "no fast cores known");
        assert!(!logical_processors().is_empty());
    }

    #[test]
    fn the_titles_placement_is_passed_on() {
        use windows_sys::Win32::System::SystemInformation::GROUP_AFFINITY;
        use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadGroupAffinity};
        std::thread::spawn(|| {
            // SAFETY: this thread's own affinity, read and set again as it was.
            unsafe {
                let mut was: GROUP_AFFINITY = std::mem::zeroed();
                assert_ne!(GetThreadGroupAffinity(GetCurrentThread(), &mut was), 0);
                let mut before: GROUP_AFFINITY = std::mem::zeroed();
                let ok = title_group_affinity(GetCurrentThread(), (&raw const was).cast(), (&raw mut before).cast());
                assert_ne!(ok, 0);
                assert_eq!(before.Mask, was.Mask);
            }
        })
        .join()
        .unwrap();
        assert!(!PLACED.lock().unwrap().is_empty());
    }
}
