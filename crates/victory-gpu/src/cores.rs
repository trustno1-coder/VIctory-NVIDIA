//! Which processors our own threads run on, where a processor has two kinds
//! of cores, and the whole process where it has two sizes of last-level cache
//! (`VICTORY_CACHE_CORES`). `cpu_sets` and `keep_on_fast_cores` are fxceless/victory-fork's
//! (`framelog.rs`, commit 0b0074e0, GPL-3.0-or-later, as this).

/// The system's processor sets of group 0: each one's id, its efficiency
/// class (higher is faster), its logical processor and its physical core index.
fn cpu_sets() -> Vec<(u32, u8, u8, u8)> {
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
            sets.push((set.Id, set.EfficiencyClass, set.LogicalProcessorIndex, set.CoreIndex));
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

/// Group 0's last-level caches: the logical processors under each (a mask)
/// and its bytes.
fn last_level_caches() -> Vec<(usize, u32)> {
    use windows_sys::Win32::System::SystemInformation::{
        CACHE_RELATIONSHIP, GetLogicalProcessorInformationEx, RelationCache, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };
    let mut bytes = 0u32;
    // SAFETY: a size query, then the call into a buffer of that size.
    unsafe { GetLogicalProcessorInformationEx(RelationCache, std::ptr::null_mut(), &mut bytes) };
    let mut buffer = vec![0u64; (bytes as usize).div_ceil(8).max(1)];
    // SAFETY: as above; the buffer holds `bytes` bytes, aligned for the records.
    let ok = unsafe { GetLogicalProcessorInformationEx(RelationCache, buffer.as_mut_ptr().cast(), &mut bytes) };
    let (start, bytes) = (buffer.as_ptr().cast::<u8>(), bytes as usize);
    // A record is shorter than its type (a union of the kinds): its fields are read one by one.
    let body = std::mem::offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Anonymous);
    let mut caches: Vec<(u8, usize, u32)> = Vec::new();
    let mut at = 0usize;
    while ok != 0 && at + body <= bytes {
        // SAFETY: a record's relationship and size, inside the buffer.
        let (relationship, size) = unsafe { (start.add(at).cast::<i32>().read_unaligned(), start.add(at + 4).cast::<u32>().read_unaligned() as usize) };
        if size == 0 || at + size > bytes {
            break;
        }
        if relationship == RelationCache && size >= body + size_of::<CACHE_RELATIONSHIP>() {
            // SAFETY: a cache's record, whole inside the buffer; its first mask.
            let (cache, mask) = unsafe {
                let cache = start.add(at + body).cast::<CACHE_RELATIONSHIP>().read_unaligned();
                (cache, cache.Anonymous.GroupMask)
            };
            if mask.Group == 0 {
                caches.push((cache.Level, mask.Mask, cache.CacheSize));
            }
        }
        at += size;
    }
    let last = caches.iter().map(|c| c.0).max().unwrap_or(0);
    caches.into_iter().filter(|c| c.0 == last).map(|c| (c.1, c.2)).collect()
}

/// The sets (id, logical processor) under the largest last-level cache, if
/// others have a smaller one: the half with 3D V-Cache of a processor that has it on one only.
fn big_cache_of(sets: &[(u32, u8)], caches: &[(usize, u32)]) -> Vec<u32> {
    let most = caches.iter().map(|c| c.1).max().unwrap_or(0);
    let under = caches.iter().filter(|c| c.1 == most).fold(0usize, |all, c| all | c.0);
    let big: Vec<u32> = sets.iter().filter(|s| u32::from(s.1) < usize::BITS && under >> s.1 & 1 != 0).map(|s| s.0).collect();
    if big.len() == sets.len() { Vec::new() } else { big }
}

/// The ids of `sets` (id, class, logical processor, core index) under the largest of `caches` (mask, bytes) and the others,
/// where all cores are of one class and that part has 16 or more and no fewer than the rest (3D V-Cache).
fn split(sets: &[(u32, u8, u8, u8)], caches: &[(usize, u32)]) -> Option<(Vec<u32>, Vec<u32>)> {
    let classes: Vec<(u32, u8)> = sets.iter().map(|s| (s.0, s.1)).collect();
    if !fast_of(&classes).is_empty() {
        return None;
    }
    let places: Vec<(u32, u8)> = sets.iter().map(|s| (s.0, s.2)).collect();
    let big = big_cache_of(&places, caches);
    let rest: Vec<u32> = places.iter().map(|p| p.0).filter(|id| !big.contains(id)).collect();
    (big.len() >= 16 && big.len() >= rest.len()).then_some((big, rest))
}

/// How the submitting thread is placed on a dual-CCD V-Cache processor
/// (`VICTORY_SUBMIT_CORE`: `1` (default) = own dedicated physical core on the V-Cache half,
/// `0` = share all V-Cache cores, `ccd1` = dedicated physical core on the frequency half).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SubmitCore {
    Off,
    VcacheCore,
    OtherCore,
}

fn submit_core_mode() -> SubmitCore {
    static MODE: std::sync::OnceLock<SubmitCore> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("VICTORY_SUBMIT_CORE").ok().as_deref().map(str::trim) {
        Some("0") => SubmitCore::Off,
        Some("ccd1") | Some("other") => SubmitCore::OtherCore,
        _ => SubmitCore::VcacheCore,
    })
}

/// Splits one dedicated physical core (its primary logical processor's CPU set id)
/// out of `group` (removing both SMT siblings of that physical core from the remainder).
fn carve_core(sets: &[(u32, u8, u8, u8)], group: &[u32]) -> Option<(u32, Vec<u32>)> {
    let &(primary_id, _, _, core_idx) = sets.iter().find(|s| group.contains(&s.0))?;
    let others: Vec<u32> = sets
        .iter()
        .filter(|s| group.contains(&s.0) && s.3 != core_idx)
        .map(|s| s.0)
        .collect();
    (!others.is_empty()).then_some((primary_id, others))
}

/// This PC's [`split`].
fn halves() -> Option<&'static (Vec<u32>, Vec<u32>)> {
    static HALVES: std::sync::OnceLock<Option<(Vec<u32>, Vec<u32>)>> = std::sync::OnceLock::new();
    HALVES.get_or_init(|| split(&cpu_sets(), &last_level_caches())).as_ref()
}

/// Whether [`place_process`] put the process on one half.
static PLACED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Reserved CPU set id for `victory-submit` when [`submit_core_mode`] carves a dedicated core.
static SUBMIT_SET: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Default CPU sets actually given to the process (for tests).
static PROCESS_SETS: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();

/// The whole process, the title's threads too, on the part with the larger cache ([`halves`]),
/// unless `VICTORY_CACHE_CORES=0` or the process was started with an affinity mask.
pub(crate) fn place_process() {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessAffinityMask, SetProcessDefaultCpuSets};
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Some((big, rest)) = halves() else {
            tracing::info!("no larger cache on a part of 16 logical processors or more: the process is not placed");
            return;
        };
        let sets = cpu_sets();
        let mode = submit_core_mode();
        let (proc_sets, carved) = match mode {
            SubmitCore::VcacheCore => match carve_core(&sets, big) {
                Some((id, others)) => (others, Some(id)),
                None => (big.clone(), None),
            },
            SubmitCore::OtherCore => (big.clone(), carve_core(&sets, rest).map(|(id, _)| id)),
            SubmitCore::Off => (big.clone(), None),
        };
        let lps = sets.iter().filter(|s| proc_sets.contains(&s.0)).fold(0usize, |m, s| m | 1 << s.2);
        let every = sets.iter().fold(0usize, |m, s| m | 1 << s.2);
        let on = format!("{lps:#x}");
        if std::env::var_os("VICTORY_CACHE_CORES").is_some_and(|v| v == "0") {
            tracing::info!(on, "VICTORY_CACHE_CORES=0: the process is not put on the logical processors under the larger cache");
            return;
        }
        let (mut ours, mut all) = (0usize, 0usize);
        // SAFETY: this process's own masks.
        if unsafe { GetProcessAffinityMask(GetCurrentProcess(), &mut ours, &mut all) } != 0 && ours & every != every {
            tracing::warn!(on, mask = %format!("{ours:#x}"), "the process was started with an affinity mask: it is not placed");
            return;
        }
        // SAFETY: our own process and a list of the system's own set ids.
        if unsafe { SetProcessDefaultCpuSets(GetCurrentProcess(), proc_sets.as_ptr(), proc_sets.len() as u32) } == 0 {
            tracing::warn!(error = %std::io::Error::last_os_error(), "the process could not be put on the larger cache's processors");
            return;
        }
        let _ = PROCESS_SETS.set(proc_sets);
        if let Some(id) = carved {
            SUBMIT_SET.store(id, std::sync::atomic::Ordering::Release);
        }
        PLACED.store(true, std::sync::atomic::Ordering::Release);
        tracing::info!(
            on,
            others = rest.len(),
            submit_core = ?mode,
            submit_set = carved.unwrap_or(0),
            "the process runs on the logical processors under the larger cache, pipeline makers, DirectStorage and the runtime's background compiles on the others (VICTORY_CACHE_CORES=0: not)"
        );
        let watch = std::thread::Builder::new().name("victory-cores".to_owned()).spawn(|| {
            keep_off_the_frames_cores("victory-cores");
            loop {
                keep_runtime_compiles_off_the_frames_cores();
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        });
        if let Err(error) = watch {
            tracing::warn!(%error, "the runtime's background compiles stay on the frames' cores");
        }
    });
}

/// The runtime's "D3D Background Thread"s (shaders compiled again, for minutes after pipelines are
/// made or loaded) on the other half too: as threads of the process they take its default sets.
fn keep_runtime_compiles_off_the_frames_cores() {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, GetThreadDescription, OpenThread, SetThreadSelectedCpuSets, THREAD_QUERY_LIMITED_INFORMATION,
        THREAD_SET_LIMITED_INFORMATION,
    };
    static MOVED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let Some((_, rest)) = placed() else { return };
    let mut moved = 0;
    // SAFETY: a snapshot of our own, walked with a sized entry; handles opened here are closed, names freed as asked.
    unsafe {
        let own = GetCurrentProcessId();
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return;
        }
        let mut entry = THREADENTRY32 { dwSize: size_of::<THREADENTRY32>() as u32, ..Default::default() };
        let mut more = Thread32First(snapshot, &mut entry) != 0;
        while more {
            let thread = if entry.th32OwnerProcessID == own {
                OpenThread(THREAD_QUERY_LIMITED_INFORMATION | THREAD_SET_LIMITED_INFORMATION, 0, entry.th32ThreadID)
            } else {
                std::ptr::null_mut()
            };
            if !thread.is_null() {
                let mut name = std::ptr::null_mut();
                if GetThreadDescription(thread, &mut name) >= 0 && !name.is_null() {
                    let len = (0..).take_while(|&i| *name.add(i) != 0).count();
                    let text = String::from_utf16_lossy(std::slice::from_raw_parts(name, len));
                    LocalFree(name.cast());
                    if text.starts_with("D3D") && text.to_lowercase().contains("background") {
                        moved += usize::from(SetThreadSelectedCpuSets(thread, rest.as_ptr(), rest.len() as u32) != 0);
                    }
                }
                CloseHandle(thread);
            }
            more = Thread32Next(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    if moved > MOVED.swap(moved, std::sync::atomic::Ordering::Relaxed) {
        tracing::info!(threads = moved, "the runtime's background compiles run on the half the frames' are not on");
    }
}

/// The half the process was put on and the other one; `None` where it was not put on one.
fn placed() -> Option<&'static (Vec<u32>, Vec<u32>)> {
    halves().filter(|_| PLACED.load(std::sync::atomic::Ordering::Relaxed))
}

/// Asks the system to run the calling thread, one below the frame's in priority (a pipeline maker, DirectStorage's), on
/// the half the process was not put on ([`place_process`]); nothing where it was not. Parking goes before it.
pub(crate) fn keep_off_the_frames_cores(what: &str) {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadSelectedCpuSets};
    let Some((_, rest)) = placed() else { return };
    // SAFETY: our own thread and a list of the system's own set ids.
    if unsafe { SetThreadSelectedCpuSets(GetCurrentThread(), rest.as_ptr(), rest.len() as u32) } == 0 {
        tracing::warn!(what, error = %std::io::Error::last_os_error(), "the thread could not be kept off the frames' cores");
    }
}

/// The logical processors [`keep_off_the_frames_cores`] puts a thread on: the other half where
/// the process was placed, else all of them.
pub(crate) fn off_the_frames_cores() -> usize {
    placed().map_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()), |h| h.1.len())
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

/// Places the single submitting thread (`victory-submit`): on a dual-CCD V-Cache processor
/// with [`SubmitCore::VcacheCore`] / [`SubmitCore::OtherCore`], gives it its own dedicated
/// physical core (no SMT sibling contention) and `THREAD_PRIORITY_HIGHEST` (unless
/// `VICTORY_SUBMIT_PRIORITY` overrides it).
pub(crate) fn keep_submit_core(what: &str) {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, SetThreadSelectedCpuSets, THREAD_PRIORITY_ABOVE_NORMAL,
        THREAD_PRIORITY_HIGHEST,
    };
    keep_on_fast_cores(what);
    let prio = match std::env::var("VICTORY_SUBMIT_PRIORITY").ok().and_then(|v| v.trim().parse::<i32>().ok()) {
        Some(p) => p.clamp(-2, 2),
        None => if PLACED.load(std::sync::atomic::Ordering::Acquire) { THREAD_PRIORITY_HIGHEST } else { THREAD_PRIORITY_ABOVE_NORMAL },
    };
    // SAFETY: our own thread.
    unsafe { SetThreadPriority(GetCurrentThread(), prio) };
    let id = SUBMIT_SET.load(std::sync::atomic::Ordering::Acquire);
    if id != 0 {
        let ids = [id];
        // SAFETY: our own thread and a valid system CPU set id.
        if unsafe { SetThreadSelectedCpuSets(GetCurrentThread(), ids.as_ptr(), 1) } != 0 {
            tracing::info!(what, set = id, priority = prio, "submitting thread placed on dedicated core (VICTORY_SUBMIT_CORE)");
        } else {
            tracing::warn!(what, error = %std::io::Error::last_os_error(), "could not pin submitting thread to dedicated core");
        }
    }
}

/// Asks the system to run the calling thread, which a frame waits for, on
/// the fast cores only, where the processor has two kinds. That fork's submit
/// thread was found on a slow one in 1,529 looks of 24,021. (A selection holds
/// unless its processors are parked.) The title's threads are not asked for.
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
        let classes: Vec<(u32, u8)> = sets.iter().map(|s| (s.0, s.1)).collect();
        (fast_of(&classes), sets.len())
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

    #[test]
    fn the_cores_under_the_larger_cache_are_told_apart() {
        let sets = [(256, 0), (257, 1), (258, 2), (259, 3)];
        assert_eq!(big_cache_of(&sets, &[(0b0011, 96 << 20), (0b1100, 32 << 20)]), [256, 257]);
        assert!(big_cache_of(&sets[..2], &[(0b01, 32 << 20), (0b10, 32 << 20)]).is_empty());
        assert!(big_cache_of(&sets[..2], &[(0b11, 96 << 20)]).is_empty());
        assert!(big_cache_of(&sets, &[]).is_empty());
        assert!(big_cache_of(&[], &[(0b11, 96 << 20)]).is_empty());
        // 16 of 32 like logical processors under the larger cache: two halves; 12 of 24, or 8 of 32: none.
        let like = |n: u8| (0..n).map(|i| (256 + u32::from(i), 0u8, i, i / 2)).collect::<Vec<_>>();
        let low = |n: u8| (1usize << n) - 1;
        let two = |big: u8, all: u8| [(low(big), 96 << 20), (low(all) & !low(big), 32 << 20)];
        assert_eq!(split(&like(32), &two(16, 32)).map(|h| (h.0.len(), h.1.len())), Some((16, 16)));
        assert_eq!(split(&like(24), &two(12, 24)), None);
        assert_eq!(split(&like(32), &two(8, 32)), None);
        assert_eq!(split(&like(32), &[(low(32), 32 << 20)]), None);
        let mut hybrid = like(32);
        hybrid[31].1 = 1;
        assert_eq!(split(&hybrid, &two(16, 32)), None);
        let all32 = like(32);
        let (big32, _) = split(&all32, &two(16, 32)).unwrap();
        let (primary, others) = carve_core(&all32, &big32).unwrap();
        assert_eq!(primary, 256);
        assert_eq!(others.len(), 14);
        assert!(!others.contains(&256) && !others.contains(&257));
        // This PC's own, whatever it has: the part found is some of its sets, not all.
        let sets: Vec<(u32, u8)> = cpu_sets().iter().map(|s| (s.0, s.2)).collect();
        let big = big_cache_of(&sets, &last_level_caches());
        assert!(big.len() < sets.len().max(1) && big.iter().all(|id| sets.iter().any(|s| s.0 == *id)));
        println!("{} logical processors, {} under the larger cache, halves: {}", sets.len(), big.len(), halves().is_some());
    }

    /// With `VICTORY_TEST_PLACE=1`, for the cost tests after it in one process (`--include-ignored
    /// --test-threads=1 --exact`, by name): the process placed as in a run of the title.
    #[test]
    #[ignore]
    fn the_process_is_placed_for_later_tests() {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessDefaultCpuSets};
        if std::env::var_os("VICTORY_TEST_PLACE").is_none_or(|v| v != "1") {
            return;
        }
        place_process();
        let (mut ids, mut count) = ([0u32; 256], 0u32);
        // SAFETY: a query into a buffer of that many ids.
        assert!(unsafe { GetProcessDefaultCpuSets(GetCurrentProcess(), ids.as_mut_ptr(), ids.len() as u32, &mut count) } != 0);
        assert_eq!(ids[..count as usize], *PROCESS_SETS.get().map_or(&[][..], Vec::as_slice));
        println!("placed on {count} of {} logical processors", cpu_sets().len());
    }

    /// Stand-ins for the title's threads beside the cost tests after this one: `VICTORY_TEST_LOAD=<n>` (none
    /// without), each through 4 MB of its own; prints their passes a second and one more thread's time for 256.
    #[test]
    #[ignore]
    fn the_tests_after_this_have_busy_threads_beside_them() {
        use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
        use std::time::{Duration, Instant};
        use windows_sys::Win32::System::Threading::GetCurrentProcessorNumber;
        static RAN_ON: AtomicUsize = AtomicUsize::new(0);
        static PASSES: AtomicU64 = AtomicU64::new(0);
        fn pass(words: &mut [u64], sum: &mut u64) {
            for word in words {
                *word = word.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(*sum);
                *sum ^= *word >> 7;
            }
            std::hint::black_box(*sum);
        }
        let Some(threads) = std::env::var("VICTORY_TEST_LOAD").ok().and_then(|v| v.trim().parse::<usize>().ok()) else { return };
        for t in 0..threads {
            std::thread::spawn(move || {
                let (mut words, mut sum) = (vec![t as u64; 4 << 17], 0u64);
                loop {
                    pass(&mut words, &mut sum);
                    PASSES.fetch_add(1, Ordering::Relaxed);
                    // SAFETY: a plain query.
                    RAN_ON.fetch_or(1 << unsafe { GetCurrentProcessorNumber() }, Ordering::Relaxed);
                }
            });
        }
        std::thread::sleep(Duration::from_millis(250));
        let (began, before) = (Instant::now(), PASSES.load(Ordering::Relaxed));
        std::thread::sleep(Duration::from_secs(1));
        let rate = (PASSES.load(Ordering::Relaxed) - before) as f64 / began.elapsed().as_secs_f64();
        let (mut words, mut sum, began) = (vec![1u64; 4 << 17], 0u64, Instant::now());
        for _ in 0..256 {
            pass(&mut words, &mut sum);
        }
        let one = began.elapsed().as_secs_f64() * 1e3;
        let on = RAN_ON.load(Ordering::Relaxed);
        println!("{threads} busy threads, on logical processors {on:#x}: {rate:.0} passes a second; one more thread: 256 passes in {one:.1} ms");
    }
}
