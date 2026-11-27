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

/// The switches' names in the file and what they are without it.
const KNOBS: [(&str, i64); 13] = [
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
];
static VALUES: [AtomicI64; 13] = [const { AtomicI64::new(0) }; 13];

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

/// One asking of the kernel's write watch, begun at `began`, about `kb`
/// kilobytes, which found `found` pages written.
pub(crate) fn asked(kb: usize, found: usize, began: Instant) {
    let at = if SUBMITS.get() { 0 } else { 4 };
    ASKS[at].fetch_add(1, Relaxed);
    ASKS[at + 1].fetch_add(kb as u64, Relaxed);
    ASKS[at + 2].fetch_add(found as u64, Relaxed);
    ASKS[at + 3].fetch_add(began.elapsed().as_nanos() as u64, Relaxed);
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
