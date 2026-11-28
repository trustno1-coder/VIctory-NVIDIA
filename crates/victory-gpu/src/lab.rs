//! Lab (2026-11-27): switches read from a file while the title runs, counts
//! for the frame-rate block, and upload and shared buffers kept for use
//! again when their last holder lets them go.
//!
//! Run 32 (prison room) had about 85 allocations made and 85 destroyed per
//! frame: `MakeResident` was waited for 173,000 times in 200 seconds (77.6 s
//! in all), and the thread that drops what was let go stood in
//! `NtGdiDdDDIDestroyAllocation2` 80 % of its time, 42 ms a frame.

use std::collections::{BTreeMap, VecDeque};
use std::panic::Location;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard, Once};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::{D3D12_RESOURCE_DIMENSION_BUFFER, ID3D12Resource};
use windows::core::GUID;

/// Buffers nothing holds any more are used again (0: destroyed, as before).
pub(crate) const REUSE: usize = 0;
/// A buffer used again is zeroed first, as a new one is.
pub(crate) const ZERO: usize = 1;
/// Free for whatever a build tries.
pub(crate) const A: usize = 2;
pub(crate) const B: usize = 3;
pub(crate) const C: usize = 4;
pub(crate) const D: usize = 5;
pub(crate) const E: usize = 6;
pub(crate) const F: usize = 7;
/// The priority of the threads that translate lists, and of the one that
/// lands flights ([`priority`]).
pub(crate) const G: usize = 8;
pub(crate) const H: usize = 9;
/// The title's waits for what the card tells it are woken when it is told
/// (1), not polled as the title's code has it here ([`import`]).
pub(crate) const W: usize = 10;
/// How many presents may be in the line when a present of the title's
/// returns (0: as the environment says, one).
pub(crate) const FRAMES: usize = 11;
/// So many frames of what the line, the flights and the title's waits do
/// are written to the log, from the next present on ([`trace_frame`]).
pub(crate) const T: usize = 12;
/// A write fault of the title's on a block we made read-only opens it
/// without the guards' lock (1), as `VICTORY_FAULT_UNLOCKED=1` does
/// (`crate::watch`).
pub(crate) const U: usize = 13;
/// Milliseconds a block the title writes again and again is left open, as
/// `VICTORY_GUARD_HOT` gives them (0: as the environment says).
pub(crate) const HOT: usize = 14;
/// The submitting thread has a physical core to itself (1), or shares it
/// with the thread that lands flights and nothing else (2); every other
/// thread of the process runs on the other cores ([`own_core`]).
pub(crate) const OWN: usize = 15;
/// How many of the threads kept for refreshes on several threads are in use
/// (`crate::shadow::wide_threads`; 0: as `VICTORY_WIDE_THREADS` or the number
/// of processors says).
pub(crate) const WIDE: usize = 16;
/// How many of the threads that translate lists take work
/// (`crate::list::thin`; 0: as `VICTORY_THIN_THREADS` says, eight without it).
pub(crate) const LISTS: usize = 17;
/// The submitting thread's time on a processor is counted beside the clock's
/// time for each part of its work (1), for the line [`on_line`] writes: some
/// 1,200 questions to the system a frame ([`cycles`]).
pub(crate) const CYC: usize = 18;
/// The submitting thread runs at the priority "highest" (1), where the
/// title's own threads are, and the threads it shares its work with as well
/// (2: `crate::cores::Priority`); 0: as made, above normal
/// ([`submitter_priority`]).
pub(crate) const P: usize = 19;
/// Microseconds a thread that finds the guards' lock taken asks for it again
/// and again before it sleeps (`crate::watch::guards`; 0: as the lock is
/// made, which sleeps after a few microseconds).
pub(crate) const SPIN: usize = 20;
/// An asking that guards view memory keeps the guards' lock from its look at
/// the range to what it makes read-only (1), where it has nothing to ask
/// Windows between them; 0: it takes the lock for each (`crate::watch::guard`).
pub(crate) const ONCE: usize = 21;

/// The switches' names in the file and what they are without it.
const KNOBS: [(&str, i64); 22] = [
    ("reuse", 1),
    ("zero", 1),
    ("a", 0),
    ("b", 0),
    ("c", 0),
    ("d", 0),
    ("e", 0),
    ("f", 0),
    ("g", 0),
    ("h", 0),
    ("w", 0),
    ("frames", 0),
    ("t", 0),
    ("u", 0),
    ("hot", 0),
    ("own", 0),
    ("wide", 0),
    ("lists", 0),
    ("cyc", 0),
    ("p", 0),
    ("spin", 0),
    ("once", 0),
];
static VALUES: [AtomicI64; 22] = [const { AtomicI64::new(0) }; 22];

/// Switch `p`, asked by the submitting thread before each submission: its
/// priority is "highest" with 1 or 2, "above normal" (as made) with 0. (The
/// title asks "highest" for some twenty threads of its own and gets it; the
/// PC is 89 % busy in the room, and the submitting thread is off a processor
/// 15 ms of a 45 ms frame inside its work, 9.5 ms of it under no wait of ours
/// that has a name: run 52.)
pub(crate) fn submitter_priority() {
    thread_local! {
        static APPLIED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if !SUBMITS.get() {
        return;
    }
    let high = knob(P) >= 1;
    if APPLIED.get() == high {
        return;
    }
    APPLIED.set(high);
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_HIGHEST};
    // SAFETY: this thread's own priority.
    let set = unsafe { SetThreadPriority(GetCurrentThread(), if high { THREAD_PRIORITY_HIGHEST } else { THREAD_PRIORITY_ABOVE_NORMAL }) } != 0;
    tracing::info!("lab switch p: the submitting thread's priority is {}: {set}", if high { "highest" } else { "above normal" });
}

/// Switch `own`, asked by the submitting thread (`role` 0) before each
/// submission and by the thread that lands flights (`role` 1) before each
/// landing. With 1 the process's default processor sets leave out the last
/// physical core and the submitting thread is kept on its first logical
/// processor, the second left idle; with 2 the landing thread is kept on the
/// second. 0: as made. (The 12 logical processors of this PC are nearly all
/// busy in play, the submitting thread is what the frame waits for, and a
/// thread whose core's other half is busy runs at some two thirds of its
/// speed. Run 29's try of this, from outside the process, was compared
/// between runs whose scenes differed.) On a processor with two kinds of
/// cores this drops the thread's selection of the fast ones when switched
/// off again.
pub(crate) fn own_core(role: usize) {
    thread_local! {
        static APPLIED: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
    }
    if role == 0 && !SUBMITS.get() {
        return;
    }
    let want = knob(OWN).clamp(0, 2);
    if APPLIED.get() == want {
        return;
    }
    APPLIED.set(want);
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, SetProcessDefaultCpuSets, SetThreadSelectedCpuSets};
    let sets = crate::cores::sets_by_core();
    let Some(core) = sets.iter().map(|s| s.1).max() else {
        return;
    };
    // The core's logical processors (their number, their set's id), in order.
    let mut mine: Vec<(u8, u32)> = sets.iter().filter(|s| s.1 == core).map(|s| (s.2, s.0)).collect();
    mine.sort_unstable();
    let rest: Vec<u32> = sets.iter().filter(|s| s.1 != core).map(|s| s.0).collect();
    if mine.is_empty() || rest.is_empty() {
        return;
    }
    let keep = |ids: &[u32]| {
        // SAFETY: this thread, and the system's own set ids (none: as made).
        unsafe { SetThreadSelectedCpuSets(GetCurrentThread(), if ids.is_empty() { std::ptr::null() } else { ids.as_ptr() }, ids.len() as u32) != 0 }
    };
    if role == 0 {
        let others: &[u32] = if want > 0 { &rest } else { &[] };
        // SAFETY: this process, and the system's own set ids (none: as made).
        let process = unsafe { SetProcessDefaultCpuSets(GetCurrentProcess(), if others.is_empty() { std::ptr::null() } else { others.as_ptr() }, others.len() as u32) } != 0;
        let first = [mine[0].1];
        let thread = keep(if want > 0 { &first } else { &[] });
        tracing::info!(
            "lab switch own={want}: core {core} (logical processors {:?}) {}; the process's other threads on {} sets: {process}, the submitting thread: {thread}",
            mine.iter().map(|m| m.0).collect::<Vec<_>>(),
            if want > 0 { "is the submitting thread's" } else { "is everybody's again" },
            rest.len(),
        );
    } else {
        let second = [mine[mine.len() - 1].1];
        let thread = keep(if want == 2 && mine.len() > 1 { &second } else { &[] });
        tracing::info!("lab switch own={want}: the landing thread's processors set: {thread}");
    }
}

/// A switch as the file was last read, with nothing started and no lock
/// taken: for the write-fault handler (`crate::watch`). 0 until [`knob`] has
/// been asked once.
pub(crate) fn peek(which: usize) -> i64 {
    VALUES[which].load(Relaxed)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// A switch as it is now. `VICTORY_LAB_FILE=<file>`: lines `name=value`,
/// read again ten times a second; a name the file does not give has its
/// default. Every change is logged, so the frame-rate block's lines can be
/// told apart by what was on.
pub(crate) fn knob(which: usize) -> i64 {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        for (value, (_, default)) in VALUES.iter().zip(KNOBS) {
            value.store(default, Relaxed);
        }
        let Some(path) = std::env::var_os("VICTORY_LAB_FILE") else {
            return;
        };
        let mut last = String::new();
        read(&path, &mut last);
        let _ = std::thread::Builder::new().name("victory-lab".into()).spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(100));
                read(&path, &mut last);
            }
        });
    });
    VALUES[which].load(Relaxed)
}

/// Keeps this thread busy for as many microseconds as switch `which` says
/// (0: none). A stage of the frame's work made slower by a known amount
/// shows, by how much the frame grows, whether the frame waits for it: `b`
/// each submission prepared, `c` each piece of a list translated, `d` each
/// landing, `e` each present of the title's.
pub(crate) fn burn(which: usize) {
    let us = knob(which);
    if us <= 0 {
        return;
    }
    let began = Instant::now();
    while (began.elapsed().as_micros() as i64) < us {
        std::hint::spin_loop();
    }
}

/// Gives this thread the priority switch `which` says: 0 normal (as it was
/// made), 1 above normal, 2 highest, -1 below normal. Asks the system only
/// when the switch has changed.
pub(crate) fn priority(which: usize) {
    thread_local! {
        static APPLIED: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
    }
    let want = knob(which).clamp(-2, 2);
    if APPLIED.get() == want {
        return;
    }
    APPLIED.set(want);
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority};
    // SAFETY: this thread's own priority.
    unsafe { SetThreadPriority(GetCurrentThread(), want as i32) };
}

fn read(path: &std::ffi::OsString, last: &mut String) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    // (A file caught half written has no end of line yet.)
    if text == *last || !text.ends_with('\n') {
        return;
    }
    for (i, (name, default)) in KNOBS.iter().enumerate() {
        let value = text
            .lines()
            .filter_map(|line| line.split_once('='))
            .filter(|(n, _)| n.trim() == *name)
            .filter_map(|(_, v)| v.trim().parse::<i64>().ok())
            .last()
            .unwrap_or(*default);
        if VALUES[i].swap(value, Relaxed) != value {
            tracing::info!("lab switch {name}={value}");
        }
    }
    *last = text;
}

/// Memory made since the last frame-rate line, by the place that asked:
/// times, bytes, microseconds, and how many were buffers used again.
static SITES: Mutex<Vec<((&'static str, u32), [u64; 4])>> = Mutex::new(Vec::new());

/// Notes one piece of memory made (or a buffer used `again`) for `site`.
pub(crate) fn made(site: &'static Location<'static>, bytes: u64, began: Instant, again: bool) {
    let us = began.elapsed().as_micros() as u64;
    let key = (site.file(), site.line());
    let mut sites = lock(&SITES);
    let at = match sites.iter().position(|s| s.0 == key) {
        Some(at) => at,
        None => {
            sites.push((key, [0; 4]));
            sites.len() - 1
        }
    };
    let s = &mut sites[at].1;
    s[0] += 1;
    s[1] += bytes;
    s[2] += us;
    s[3] += u64::from(again);
}

/// What a buffer of ours says of itself: its kind (0 in the upload heap, 1
/// shared with the processor) and the size it was asked with.
const TAG: GUID = GUID::from_u128(0x2b7d5c1e_93a4_4f60_8d2b_51c0a7e4f9d3);

/// A buffer nothing holds, and since when.
struct Free(ID3D12Resource, Instant);
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Free {}

/// The buffers kept, by kind and size, the one let go last at the back.
static FREE: [Mutex<BTreeMap<u64, VecDeque<Free>>>; 2] = [const { Mutex::new(BTreeMap::new()) }; 2];
/// Kept now: how many, their bytes.
static HELD: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// Since the last frame-rate line: taken again, kept at their last holder's
/// letting go, destroyed after resting.
static POOL: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
/// Since the last frame-rate line, last references dropped (the object
/// destroyed) and their nanoseconds, by what it was: an upload buffer, a
/// shared one, another buffer, a texture.
static LAST: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
/// At most this many bytes are kept (they are the PC's memory).
const MOST: u64 = 1 << 30;

/// Marks a new buffer of `kind` as one that may be kept ([`let_go`]).
pub(crate) fn tag(buffer: &ID3D12Resource, kind: u64, size: u64) {
    let tag = [kind, size];
    // SAFETY: 16 bytes, which the object copies.
    let _ = unsafe { buffer.SetPrivateData(&TAG, 16, Some(tag.as_ptr().cast())) };
}

fn tagged(buffer: &ID3D12Resource) -> Option<(usize, u64)> {
    let (mut tag, mut len) = ([0u64; 2], 16u32);
    // SAFETY: room for the 16 bytes asked for.
    unsafe { buffer.GetPrivateData(&TAG, &mut len, Some(tag.as_mut_ptr().cast())) }.ok()?;
    (len == 16 && tag[0] < 2).then_some((tag[0] as usize, tag[1]))
}

/// Draws since the last frame-rate line of ours (the scene's weight, beside
/// the time a frame took).
pub(crate) static DRAWN: AtomicU64 = AtomicU64::new(0);
/// The submitting thread's line since then, in microseconds: waiting with
/// nothing in it, standing at something not met; the looks that found it
/// standing so; and the standing by what its oldest entry waited for: a
/// fence, a memory value, lists being translated, something else.
pub(crate) static SERVE: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

/// A buffer of `kind` and exactly `size` bytes that nothing holds, mapped
/// (and zeroed if `zero`, as a new one is): the one let go last.
pub(crate) fn take(kind: usize, size: u64, zero: bool) -> Option<(ID3D12Resource, *mut u8)> {
    if knob(REUSE) == 0 {
        return None;
    }
    let Free(buffer, _) = lock(&FREE[kind]).get_mut(&size)?.pop_back()?;
    HELD[0].fetch_sub(1, Relaxed);
    HELD[1].fetch_sub(size, Relaxed);
    let mut data = std::ptr::null_mut();
    // SAFETY: a live buffer of ours in a heap the processor maps; mapping it
    // again gives its address.
    if unsafe { buffer.Map(0, None, Some(&mut data)) }.is_err() || data.is_null() {
        return None;
    }
    if zero && knob(ZERO) != 0 {
        // SAFETY: the buffer's mapping, `size` bytes of it; nothing else
        // holds the buffer.
        unsafe { std::ptr::write_bytes(data.cast::<u8>(), 0, size as usize) };
    }
    POOL[0].fetch_add(1, Relaxed);
    Some((buffer, data.cast()))
}

/// Drops one reference to `resource`. If it is the last and the resource a
/// buffer of ours ([`tag`]), the buffer is kept for whoever next asks for
/// one of its kind and size ([`take`]) instead of being destroyed: whoever
/// held it last has let it go when it would have been destroyed, no sooner.
pub(crate) fn let_go(resource: ID3D12Resource) {
    if crate::host::references(&resource) != 1 {
        return;
    }
    let class = match tagged(&resource) {
        Some((kind, size)) => {
            if knob(REUSE) != 0 && HELD[1].load(Relaxed) + size <= MOST {
                lock(&FREE[kind]).entry(size).or_default().push_back(Free(resource, Instant::now()));
                HELD[0].fetch_add(1, Relaxed);
                HELD[1].fetch_add(size, Relaxed);
                POOL[1].fetch_add(1, Relaxed);
                return;
            }
            kind
        }
        // SAFETY: a live resource.
        None if unsafe { resource.GetDesc() }.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER => 2,
        None => 3,
    };
    let began = Instant::now();
    drop(resource);
    LAST[2 * class].fetch_add(1, Relaxed);
    LAST[2 * class + 1].fetch_add(began.elapsed().as_nanos() as u64, Relaxed);
}

/// Destroys the buffers kept that nothing asked for in three seconds (all
/// of them with the switch off). Looks four times a second at most.
pub(crate) fn trim() {
    const REST: Duration = Duration::from_secs(3);
    static LOOKED: Mutex<Option<Instant>> = Mutex::new(None);
    let now = Instant::now();
    {
        let mut looked = lock(&LOOKED);
        if looked.is_some_and(|t| now.saturating_duration_since(t) < Duration::from_millis(250)) {
            return;
        }
        *looked = Some(now);
    }
    let off = knob(REUSE) == 0;
    let mut gone = Vec::new();
    for (kind, free) in FREE.iter().enumerate() {
        lock(free).retain(|size, line| {
            while line.front().is_some_and(|f| off || now.saturating_duration_since(f.1) >= REST) {
                if let Some(f) = line.pop_front() {
                    gone.push((kind, *size, f.0));
                }
            }
            !line.is_empty()
        });
    }
    for (kind, size, buffer) in gone {
        HELD[0].fetch_sub(1, Relaxed);
        HELD[1].fetch_sub(size, Relaxed);
        POOL[2].fetch_add(1, Relaxed);
        let began = Instant::now();
        drop(buffer);
        LAST[2 * kind].fetch_add(1, Relaxed);
        LAST[2 * kind + 1].fetch_add(began.elapsed().as_nanos() as u64, Relaxed);
    }
}

/// The host's own `Sleep` and `SleepEx`, as the title imports them.
static SLEEPS: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];
/// Since the last frame-rate line, the title's sleeps (calls, microseconds):
/// those of its waits for the card ([`wait_site`]), its others of no time,
/// and its others of some.
static SLEPT: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
/// Counted up whenever the card tells the title something ([`told`]).
static TOLD: AtomicU32 = AtomicU32::new(0);

/// What the title's import `name`, which the host has at `address`, is bound
/// to: its sleeps come through here.
///
/// The title waits for a fence of ours by looking at the fence's value in
/// memory: `monitorx` on its address, `mwaitx`, and between two looks
/// `Sleep(0)` (or `Sleep(1)`, in the wait that does not hurry). The console's
/// processor sleeps in `mwaitx` until the value is written. A PC's Intel
/// processor has neither instruction, they are a pause here, and so the
/// title's render thread goes round that loop for as long as it waits, most
/// of a frame (profile of run 39: 89 % of its samples there, a third of a
/// processor), giving its time slice away at each turn and getting to look
/// again when the scheduler next has a processor for it. With switch `w` the
/// sleep of such a loop waits until a fence is set or a value written behind
/// a flight ([`told`]), a millisecond at most.
pub fn import(name: &str, address: usize) -> usize {
    let (which, ours) = match name {
        "Sleep" => (0, title_sleep as *const () as usize),
        "SleepEx" => (1, title_sleep_ex as *const () as usize),
        _ => return address,
    };
    SLEEPS[which].store(address, Relaxed);
    ours
}

/// The title's `Sleep`: on to [`sleep_from`], with the address it returns to.
#[unsafe(naked)]
unsafe extern "system" fn title_sleep(_ms: u32) {
    core::arch::naked_asm!("mov rdx, [rsp]", "jmp {inner}", inner = sym sleep_from)
}

/// The title's `SleepEx`: on to [`sleep_ex_from`], likewise.
#[unsafe(naked)]
unsafe extern "system" fn title_sleep_ex(_ms: u32, _alertable: i32) -> u32 {
    core::arch::naked_asm!("mov r8, [rsp]", "jmp {inner}", inner = sym sleep_ex_from)
}

extern "system" fn sleep_from(ms: u32, from: usize) {
    let real = SLEEPS[0].load(Relaxed);
    // SAFETY: the host's `Sleep`, which the title asked for.
    title_slept(ms, from, || unsafe { std::mem::transmute::<usize, unsafe extern "system" fn(u32)>(real)(ms) });
}

extern "system" fn sleep_ex_from(ms: u32, alertable: i32, from: usize) -> u32 {
    let real = SLEEPS[1].load(Relaxed);
    // SAFETY: the host's `SleepEx`, which the title asked for.
    let real = unsafe { std::mem::transmute::<usize, unsafe extern "system" fn(u32, i32) -> u32>(real) };
    if alertable != 0 {
        // (Its own business: what is queued for the thread runs in it.)
        // SAFETY: the caller's own call.
        return unsafe { real(ms, alertable) };
    }
    let mut result = 0;
    // SAFETY: the caller's own call.
    title_slept(ms, from, || result = unsafe { real(ms, alertable) });
    result
}

/// A sleep of the title's for `ms`, called from just before `from`.
fn title_slept(ms: u32, from: usize, real: impl FnOnce()) {
    let waits = ms <= 1 && wait_site(from);
    let began = crate::host::now_us();
    if waits && knob(W) != 0 {
        wait_told();
    } else {
        real();
    }
    let us = crate::host::now_us().saturating_sub(began);
    let which = if waits {
        0
    } else if ms == 0 {
        2
    } else {
        4
    };
    SLEPT[which].fetch_add(1, Relaxed);
    SLEPT[which + 1].fetch_add(us, Relaxed);
    if waits && traced() {
        waited(from, began, us);
    }
}

/// Whether the sleep called from just before `from` is one of a loop that
/// waits with `mwaitx` (as the title's code has it, or as the loader left
/// it: `pause` and a no-op): that instruction stands shortly before the call.
fn wait_site(from: usize) -> bool {
    thread_local! {
        static SITES: std::cell::Cell<[(usize, bool); 4]> = const { std::cell::Cell::new([(0, false); 4]) };
    }
    let mut sites = SITES.get();
    if let Some(site) = sites.iter().find(|s| s.0 == from) {
        return site.1;
    }
    const BACK: usize = 128;
    let waits = from > (1 << 16) && {
        // (Not past the start of the call's page: what is before may not be there.)
        let start = (from - BACK).max((from - 1) & !0xfff);
        // SAFETY: code of the caller's on the page its call is on, which is
        // being run.
        let code = unsafe { std::slice::from_raw_parts(start as *const u8, from - start) };
        code.windows(3).any(|w| w == [0x0f, 0x01, 0xfb] || w == [0xf3, 0x90, 0x90])
    };
    sites.rotate_right(1);
    sites[0] = (from, waits);
    SITES.set(sites);
    waits
}

/// The card told the title something: a fence was set, a value written
/// behind a flight. Whoever waits for that looks now ([`wait_told`]).
pub(crate) fn told() {
    TOLD.fetch_add(1, std::sync::atomic::Ordering::Release);
    // SAFETY: wakes those waiting on the counter, if any.
    unsafe { windows_sys::Win32::System::Threading::WakeByAddressAll((&raw const TOLD).cast()) };
}

/// Waits until the card tells the title something, a millisecond at most
/// (what was told between the title's last look and this call is waited out
/// so long, no longer).
fn wait_told() {
    let seen = TOLD.load(std::sync::atomic::Ordering::Acquire);
    // SAFETY: waits on the counter while it is what was just seen.
    unsafe { windows_sys::Win32::System::Threading::WaitOnAddress((&raw const TOLD).cast(), (&raw const seen).cast(), 4, 1) };
}

thread_local! {
    /// This is the thread that prepares and sends submissions.
    pub(crate) static SUBMITS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// This thread's wait of the title's being noted for the trace: the
    /// trace, the place, when it began and when it last ended, its sleeps
    /// and their microseconds.
    static WAIT: std::cell::Cell<(u64, usize, u64, u64, u64, u64)> = const { std::cell::Cell::new((0, 0, 0, 0, 0, 0)) };
}

/// Since the last frame-rate line, the kernel's write watch asked: by the
/// submitting thread and by the others, each the askings, the kilobytes
/// asked about, the 4 KiB pages found written and the nanoseconds.
static ASKS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

/// The submitting thread's askings by size (under 1 MB, to 8, to 64, to 256, more): each the
/// askings, the kilobytes and the nanoseconds since the last frame-rate line.
static ASK_SIZES: [AtomicU64; 15] = [const { AtomicU64::new(0) }; 15];

/// Its askings of 8 MB and more, by where they begin (MB) and their size (MB): how often, and
/// the nanoseconds.
static ASK_BIG: std::sync::Mutex<Vec<(usize, usize, u64, u64)>> = std::sync::Mutex::new(Vec::new());

/// One asking of the kernel's write watch, begun at `began`, about `kb`
/// kilobytes from `start`, which found `found` pages written.
pub(crate) fn asked(start: usize, kb: usize, found: usize, began: Instant) {
    let ns = began.elapsed().as_nanos() as u64;
    let at = if SUBMITS.get() { 0 } else { 4 };
    ASKS[at].fetch_add(1, Relaxed);
    ASKS[at + 1].fetch_add(kb as u64, Relaxed);
    ASKS[at + 2].fetch_add(found as u64, Relaxed);
    ASKS[at + 3].fetch_add(ns, Relaxed);
    if at == 0 {
        let size = match kb >> 10 {
            0 => 0,
            1..8 => 1,
            8..64 => 2,
            64..256 => 3,
            _ => 4,
        };
        ASK_SIZES[size * 3].fetch_add(1, Relaxed);
        ASK_SIZES[size * 3 + 1].fetch_add(kb as u64, Relaxed);
        ASK_SIZES[size * 3 + 2].fetch_add(ns, Relaxed);
        if size >= 2 {
            let (mb_at, mb) = (start >> 20, kb >> 10);
            let mut big = ASK_BIG.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(at) = big.iter().position(|b| b.0 == mb_at && b.1 == mb) {
                big[at].2 += 1;
                big[at].3 += ns;
            } else if big.len() < 4096 {
                big.push((mb_at, mb, 1, ns));
            }
        }
    }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn QueryThreadCycleTime(thread: *mut std::ffi::c_void, cycles: *mut u64) -> i32;
}

/// The submitting thread, for its time on a processor ([`on_line`]): a handle
/// kept for the process's life, 0 until that thread has started.
static SUBMITTER: AtomicUsize = AtomicUsize::new(0);

/// The calling thread is the one that prepares and sends submissions.
pub(crate) fn submits() {
    use windows_sys::Win32::System::Threading::{GetCurrentThreadId, OpenThread, THREAD_QUERY_LIMITED_INFORMATION};
    SUBMITS.set(true);
    // SAFETY: a handle to this thread, never closed.
    let handle = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, GetCurrentThreadId()) };
    SUBMITTER.store(handle as usize, Relaxed);
}

/// The processor cycles the submitting thread has had so far, asked by
/// itself: 0 on any other thread and with switch `cyc` off. (The system
/// counts them at every change of thread on a processor, by the processor's
/// own clock: time waiting, for a lock or for a processor, is not in them.)
pub(crate) fn cycles() -> u64 {
    if !SUBMITS.get() || peek(CYC) == 0 {
        return 0;
    }
    let mut n = 0;
    // SAFETY: this thread's own count.
    unsafe { QueryThreadCycleTime(windows_sys::Win32::System::Threading::GetCurrentThread(), &mut n) };
    n
}

/// Since the last frame-rate line, the submitting thread's processor cycles
/// by part of its work: the parts of `crate::host`'s split under their numbers
/// there (0 to 15), after a sending (16), and the pieces of [`PIECES`] (17 on).
static ON: [AtomicU64; 20] = [const { AtomicU64::new(0) }; 20];

/// The submitting thread's cycles since `since` ([`cycles`] then) are part
/// `part`'s. Its cycles now (0: not counted).
pub(crate) fn on(part: usize, since: u64) -> u64 {
    if since == 0 {
        return 0;
    }
    let now = cycles();
    if now != 0 {
        ON[part].fetch_add(now.saturating_sub(since), Relaxed);
    }
    now
}

/// Since the last frame-rate line, pieces of the submitting thread's work
/// inside the parts above, each how many and their nanoseconds by the clock:
/// work spread over threads (`crate::shadow::wide`), the kernel's write watch
/// asked, blocks of title memory made read-only (`crate::watch`). Their
/// cycles are in [`ON`] from 17 on.
static PIECES: [[AtomicU64; 2]; 3] = [const { [const { AtomicU64::new(0) }; 2] }; 3];

/// One of [`PIECES`], counted when dropped.
pub(crate) struct Piece(usize, Instant, u64);

/// Piece `which` (17 on) of the submitting thread's work begins; nothing on
/// another thread.
pub(crate) fn piece(which: usize) -> Option<Piece> {
    SUBMITS.get().then(|| Piece(which, Instant::now(), cycles()))
}

impl Drop for Piece {
    fn drop(&mut self) {
        PIECES[self.0 - 17][0].fetch_add(1, Relaxed);
        PIECES[self.0 - 17][1].fetch_add(self.1.elapsed().as_nanos() as u64, Relaxed);
        on(self.0, self.2);
    }
}

/// Since the last frame-rate line, waits of the submitting thread inside its
/// work, each how many and their nanoseconds: for ranges another thread asks
/// the kernel about (0), for the journal to read it (1) and to note in it
/// (2), for the guards' lock (3) (`crate::watch`; only when the lock was not
/// free); for the helpers at the end of work spread over threads (4); for the
/// lock of the queue of textures picked by index (5) and of the views' table
/// (6) (`crate::descriptor`).
static WAITS: [[AtomicU64; 2]; 7] = [const { [const { AtomicU64::new(0) }; 2] }; 7];

/// Since the last frame-rate line, every thread's waits for the guards' lock
/// (the title's in a write fault among them): how many and their nanoseconds.
static GUARD_WAITS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// A wait of kind `what` ([`WAITS`]) that began at `began` is over.
pub(crate) fn waited_for(what: usize, began: Instant) {
    let ns = began.elapsed().as_nanos() as u64;
    if what == 3 {
        GUARD_WAITS[0].fetch_add(1, Relaxed);
        GUARD_WAITS[1].fetch_add(ns, Relaxed);
    }
    if SUBMITS.get() {
        WAITS[what][0].fetch_add(1, Relaxed);
        WAITS[what][1].fetch_add(ns, Relaxed);
    }
}

/// For the frame-rate line, from `crate::host`'s split of the submitting
/// thread's work: `wall` is its parts' milliseconds a frame by the clock (0
/// to 15 as there, 16 after a sending). Two lines: what that thread waited for
/// inside its work, with its time on a processor (in every run), and, with
/// switch `cyc`, each part's time on a processor beside its time by the clock.
pub(crate) fn on_line(frames: f64, wall: &[f64; 17]) {
    // When the last line was, the processor's clock then, and the thread's cycles.
    static BEFORE: Mutex<Option<(Instant, u64, u64)>> = Mutex::new(None);
    let mut whole = 0;
    let handle = SUBMITTER.load(Relaxed);
    if handle != 0 {
        // SAFETY: the handle made in `submits`.
        unsafe { QueryThreadCycleTime(handle as *mut std::ffi::c_void, &mut whole) };
    }
    // SAFETY: reads the processor's clock.
    let now = (Instant::now(), unsafe { std::arch::x86_64::_rdtsc() }, whole);
    let before = lock(&BEFORE).replace(now);
    let on: Vec<u64> = ON.iter().map(|n| n.swap(0, Relaxed)).collect();
    let pieces: Vec<[f64; 2]> = PIECES.iter().map(|p| [p[0].swap(0, Relaxed) as f64 / frames, p[1].swap(0, Relaxed) as f64 / 1e6 / frames]).collect();
    let waits: Vec<[f64; 2]> = WAITS.iter().map(|w| [w[0].swap(0, Relaxed) as f64 / frames, w[1].swap(0, Relaxed) as f64 / 1e6 / frames]).collect();
    let guards = [GUARD_WAITS[0].swap(0, Relaxed) as f64 / frames, GUARD_WAITS[1].swap(0, Relaxed) as f64 / 1e6 / frames];
    let Some(before) = before else {
        return;
    };
    let ms = now.0.saturating_duration_since(before.0).as_secs_f64() * 1000.0;
    // Cycles of the processor's clock in a millisecond.
    let rate = now.1.wrapping_sub(before.1) as f64 / ms.max(1.0);
    if handle == 0 || rate <= 0.0 || wall.iter().sum::<f64>() <= 0.0 {
        return;
    }
    let whole = now.2.saturating_sub(before.2) as f64 / rate / frames;
    tracing::info!(
        "the submitting thread's waits inside its work per frame (lab): it was on a processor {whole:.1} ms of the frame's {:.1}; \
         it waited {:.1} times {:.2} ms for the helpers at the end of work spread over threads, {:.1} times {:.2} ms for the guards' \
         lock, {:.1} times {:.2} ms for the journal to read it, {:.1} times {:.2} ms to note in it, {:.1} times {:.2} ms for ranges \
         another thread asked the kernel about, {:.1} times {:.2} ms for the lock of the queue of textures picked by index, {:.1} \
         times {:.2} ms for the views table's; by the clock: work spread over threads {:.1} times {:.2} ms, the kernel's write \
         watch {:.1} calls {:.2} ms, blocks made read-only {:.1} times {:.2} ms; every thread's waits for the guards' lock \
         {:.1} times {:.2} ms",
        ms / frames,
        waits[4][0],
        waits[4][1],
        waits[3][0],
        waits[3][1],
        waits[1][0],
        waits[1][1],
        waits[2][0],
        waits[2][1],
        waits[0][0],
        waits[0][1],
        waits[5][0],
        waits[5][1],
        waits[6][0],
        waits[6][1],
        pieces[0][0],
        pieces[0][1],
        pieces[1][0],
        pieces[1][1],
        pieces[2][0],
        pieces[2][1],
        guards[0],
        guards[1],
    );
    if on.iter().sum::<u64>() == 0 {
        return;
    }
    let on: Vec<f64> = on.iter().map(|&n| n as f64 / rate / frames).collect();
    tracing::info!(
        "the submitting thread on a processor per frame (lab, switch cyc): {whole:.1} ms of the frame's {:.1}; the parts of its \
         submissions, ms by the clock / ms on a processor: asking the write watch {:.2} / {:.2}, the title's bytes under flights' \
         writes {:.2} / {:.2}, the lists' steps {:.2} / {:.2} (their flushes of refreshes {:.2} / {:.2}), snapshots {:.2} / {:.2}, \
         copies in the card's memory {:.2} / {:.2}, sending {:.2} / {:.2}, planning steps done on the card {:.2} / {:.2}, waiting \
         for flights {:.2} / {:.2}, the heaps' records and what is swept {:.2} / {:.2}, waiting for the heaps' records {:.2} / \
         {:.2}, after a sending {:.2} / {:.2}; inside them: work spread over threads {:.2} / {:.2}, the kernel's write watch \
         {:.2} / {:.2}, blocks made read-only {:.2} / {:.2}",
        ms / frames,
        wall[0],
        on[0],
        wall[1],
        on[1],
        wall[2],
        on[2],
        wall[6] - wall[9],
        on[6],
        wall[3],
        on[3],
        wall[4],
        on[4],
        wall[5],
        on[5],
        wall[12],
        on[12],
        wall[13],
        on[13],
        wall[14],
        on[14],
        wall[15],
        on[15],
        wall[16],
        on[16],
        pieces[0][1],
        on[17],
        pieces[1][1],
        on[18],
        pieces[2][1],
        on[19],
    );
}

/// Since the last frame-rate line, the microseconds the submitting thread's
/// line stood, by what the first entries of its queues were: a bit each for
/// a wait for a fence (1), for a memory value (2), lists being translated
/// (4), another wait (8), a present (16), anything else (32).
pub(crate) static STOOD: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];

/// One thing noted for the trace: when (microseconds of the host's clock),
/// for how long, by which thread, what, and two numbers that say which.
struct Noted {
    at: u64,
    took: u64,
    thread: u32,
    what: &'static str,
    a: u64,
    b: u64,
}

/// What a trace has noted, and the threads that noted it with their names.
static TRACE: Mutex<(Vec<Noted>, Vec<(u32, String)>)> = Mutex::new((Vec::new(), Vec::new()));
static TRACING: AtomicBool = AtomicBool::new(false);
/// When the trace began (the host's clock), and which trace it is.
static TRACE_BEGAN: AtomicU64 = AtomicU64::new(0);
static TRACE_NUMBER: AtomicU64 = AtomicU64::new(0);

/// Whether a trace is being taken.
pub(crate) fn traced() -> bool {
    TRACING.load(Relaxed)
}

/// Notes for the trace that `what` happens now.
pub(crate) fn note(what: &'static str, a: u64, b: u64) {
    if traced() {
        noted(crate::host::now_us(), 0, what, a, b);
    }
}

/// Notes for the trace `what`, which began at `began` and ends now.
pub(crate) fn note_since(what: &'static str, began: Instant, a: u64, b: u64) {
    if traced() {
        let took = began.elapsed().as_micros() as u64;
        noted(crate::host::now_us().saturating_sub(took), took, what, a, b);
    }
}

fn noted(at: u64, took: u64, what: &'static str, a: u64, b: u64) {
    let thread = crate::host::thread_id();
    let mut trace = lock(&TRACE);
    if trace.0.len() >= 60_000 {
        return;
    }
    if !trace.1.iter().any(|t| t.0 == thread) {
        let name = std::thread::current().name().unwrap_or("a thread of the title's").to_string();
        trace.1.push((thread, name));
    }
    trace.0.push(Noted { at, took, thread, what, a, b });
}

/// A sleep of a wait of the title's ([`wait_site`]) for the trace: the
/// sleeps of one wait are noted as one thing, cut every two milliseconds.
fn waited(from: usize, began: u64, us: u64) {
    let number = TRACE_NUMBER.load(Relaxed);
    let mut w = WAIT.get();
    if w.4 > 0 && (w.0 != number || w.1 != from || began > w.3 + 200 || began + us > w.2 + 2000) {
        if w.0 == number {
            noted(w.2, w.3 - w.2, "the title waited for the card (sleeps, their microseconds)", w.4, w.5);
        }
        w.4 = 0;
    }
    if w.4 == 0 {
        w = (number, from, began, began, 0, 0);
    }
    w.3 = began + us;
    w.4 += 1;
    w.5 += us;
    WAIT.set(w);
}

/// At each present of the title's: a trace begins when switch `t` has a new
/// number (of frames), and ends, written to the log, that many presents
/// later.
pub(crate) fn trace_frame() {
    static ASKED: AtomicI64 = AtomicI64::new(0);
    static LEFT: AtomicI64 = AtomicI64::new(0);
    let want = knob(T);
    if traced() {
        if LEFT.fetch_sub(1, Relaxed) <= 1 {
            TRACING.store(false, Relaxed);
            trace_out();
        }
        return;
    }
    if want <= 0 {
        ASKED.store(0, Relaxed);
    } else if ASKED.swap(want, Relaxed) != want {
        {
            let mut trace = lock(&TRACE);
            trace.0.clear();
            trace.1.clear();
        }
        TRACE_NUMBER.fetch_add(1, Relaxed);
        TRACE_BEGAN.store(crate::host::now_us(), Relaxed);
        LEFT.store(want.min(30), Relaxed);
        TRACING.store(true, Relaxed);
    }
}

fn trace_out() {
    let (mut noted, threads) = std::mem::take(&mut *lock(&TRACE));
    let (number, began) = (TRACE_NUMBER.load(Relaxed), TRACE_BEGAN.load(Relaxed));
    noted.sort_by_key(|n| n.at);
    for (thread, name) in &threads {
        tracing::info!("lab trace {number}: thread {thread} is {name}");
    }
    for n in &noted {
        tracing::info!(
            "lab trace {number}: +{} us, {} us, thread {}, {}, {:#x}, {}",
            n.at as i64 - began as i64,
            n.took,
            n.thread,
            n.what,
            n.a,
            n.b
        );
    }
    tracing::info!("lab trace {number}: end, {} things noted", noted.len());
}

/// For the frame-rate line.
pub(crate) fn log(frames: f64) {
    static BEFORE: Mutex<Option<Instant>> = Mutex::new(None);
    let now = Instant::now();
    let ms = lock(&BEFORE).replace(now).map_or(0.0, |t| now.saturating_duration_since(t).as_secs_f64() * 1000.0);
    let frames = frames.max(1.0);
    let switches: Vec<String> = KNOBS.iter().zip(&VALUES).map(|((name, _), v)| format!("{name}={}", v.load(Relaxed))).collect();
    let serve: Vec<f64> = SERVE.iter().map(|s| s.swap(0, Relaxed) as f64 / frames).collect();
    tracing::info!(
        "lab: {frames:.0} frames in {ms:.0} ms, {:.2} ms a frame, {:.0} draws a frame; line: empty {:.2} ms, standing {:.2} ms \
         (fence {:.2}, value {:.2}, lists {:.2}, other {:.2}; {:.1} looks); switches: {}",
        ms / frames,
        DRAWN.swap(0, Relaxed) as f64 / frames,
        serve[0] / 1000.0,
        serve[1] / 1000.0,
        serve[3] / 1000.0,
        serve[4] / 1000.0,
        serve[5] / 1000.0,
        serve[6] / 1000.0,
        serve[2],
        switches.join(" ")
    );
    let s: Vec<f64> = SLEPT.iter().map(|s| s.swap(0, Relaxed) as f64 / frames).collect();
    if s[0] + s[2] + s[4] > 0.0 {
        tracing::info!(
            "the title's sleeps per frame (lab): {:.1} in its waits for the card {:.2} ms (switch w=1: woken when the card tells it, \
             now {}), {:.1} others of no time {:.2} ms, {:.1} others of some time {:.2} ms",
            s[0],
            s[1] / 1000.0,
            if knob(W) != 0 { "on" } else { "off" },
            s[2],
            s[3] / 1000.0,
            s[4],
            s[5] / 1000.0
        );
    }
    let a: Vec<f64> = ASKS.iter().map(|a| a.swap(0, Relaxed) as f64 / frames).collect();
    if a[0] + a[4] > 0.0 {
        tracing::info!(
            "the write watch asked per frame (lab): by the submitting thread {:.1} askings over {:.1} MB, {:.0} pages of 4 KiB found \
             written, {:.2} ms; by the other threads {:.1} askings over {:.1} MB, {:.0} pages found written, {:.2} ms",
            a[0],
            a[1] / 1024.0,
            a[2],
            a[3] / 1e6,
            a[4],
            a[5] / 1024.0,
            a[6],
            a[7] / 1e6
        );
    }
    let sizes: Vec<f64> = ASK_SIZES.iter().map(|a| a.swap(0, Relaxed) as f64 / frames).collect();
    let mut big = std::mem::take(&mut *ASK_BIG.lock().unwrap_or_else(|e| e.into_inner()));
    if sizes.iter().step_by(3).sum::<f64>() > 0.0 {
        big.sort_by_key(|b| std::cmp::Reverse(b.3));
        let kinds = big.len();
        let top: Vec<String> = big
            .iter()
            .take(8)
            .map(|b| format!("{:#x} MB + {} MB: {:.1} times {:.2} ms", b.0, b.1, b.2 as f64 / frames, b.3 as f64 / frames / 1e6))
            .collect();
        let names = ["under 1 MB", "1 to 8 MB", "8 to 64 MB", "64 to 256 MB", "256 MB and more"];
        let by: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(i, n)| format!("{n}: {:.1} askings over {:.0} MB {:.2} ms", sizes[i * 3], sizes[i * 3 + 1] / 1024.0, sizes[i * 3 + 2] / 1e6))
            .collect();
        tracing::info!(
            "the submitting thread's askings of the write watch per frame, by size (lab): {}; of 8 MB and more, {} kinds, the costliest by \
             where they begin and their size: {}",
            by.join("; "),
            kinds,
            top.join("; ")
        );
    }
    let stood: Vec<String> = STOOD
        .iter()
        .enumerate()
        .filter_map(|(heads, us)| {
            let ms = us.swap(0, Relaxed) as f64 / 1000.0 / frames;
            let names = ["a fence", "a value", "lists", "another wait", "a present", "something free"];
            let what: Vec<&str> = names.iter().enumerate().filter(|(bit, _)| heads >> bit & 1 != 0).map(|(_, n)| *n).collect();
            (ms >= 0.005).then(|| format!("{} {ms:.2} ms", what.join(" and ")))
        })
        .collect();
    if !stood.is_empty() {
        tracing::info!("the line stood per frame (lab), by what its queues' first entries were: {}", stood.join("; "));
    }
    let mut sites = std::mem::take(&mut *lock(&SITES));
    if !sites.is_empty() {
        sites.sort_by_key(|s| std::cmp::Reverse(s.1[2]));
        let (times, us) = sites.iter().fold((0, 0), |(n, us), s| (n + s.1[0], us + s.1[2]));
        let text: Vec<String> = sites
            .iter()
            .take(12)
            .map(|&((file, line), s)| {
                format!(
                    "{}:{line} {:.1} ({:.1} used again) {:.0} KB {:.2} ms",
                    file.rsplit(['/', '\\']).next().unwrap_or(file),
                    s[0] as f64 / frames,
                    s[3] as f64 / frames,
                    s[1] as f64 / 1024.0 / frames,
                    s[2] as f64 / 1000.0 / frames
                )
            })
            .collect();
        tracing::info!(
            "memory made per frame, by the place that asked (lab): {:.1} times, {:.2} ms in all; {}",
            times as f64 / frames,
            us as f64 / 1000.0 / frames,
            text.join("; ")
        );
    }
    let p: Vec<f64> = POOL.iter().map(|p| p.swap(0, Relaxed) as f64 / frames).collect();
    let l: Vec<f64> = LAST.iter().map(|l| l.swap(0, Relaxed) as f64 / frames).collect();
    tracing::info!(
        "buffers kept for use again per frame (lab, reuse): {:.1} taken again, {:.1} kept when let go, {:.1} destroyed after resting; \
         kept now {} ({} MB); last references dropped per frame: {:.1} upload buffers {:.2} ms, {:.1} shared buffers {:.2} ms, \
         {:.1} other buffers {:.2} ms, {:.1} textures {:.2} ms",
        p[0],
        p[1],
        p[2],
        HELD[0].load(Relaxed),
        HELD[1].load(Relaxed) >> 20,
        l[0],
        l[1] / 1e6,
        l[2],
        l[3] / 1e6,
        l[4],
        l[5] / 1e6,
        l[6],
        l[7] / 1e6,
    );
}
