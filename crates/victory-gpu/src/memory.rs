//! Title memory by address, for translated shaders that read and write
//! through raw 40-bit addresses (`victory_shader::emit::SLOT_MEMORY`: the
//! console's ray-tracing structure builder and the programs that walk its
//! structures; the structures themselves lie in plain title memory, in no
//! buffer object).
//!
//! A 64 KiB page in a title buffer that holds all of it is in that buffer's
//! copy ([`crate::shadow`]), which every other access uses. A page outside
//! buffers has a page of its own in an **arena**: memory of ours with a copy
//! on the card ([`crate::shadow::by_address`]). The streamed structures lie
//! anywhere between other data (6.6 GB of title memory in 4 MiB windows, run
//! light-1; 8 GB in windows of 256 KiB, light-2), so a page is what is kept.
//!
//! This module keeps what a shader needs to find the pages:
//!
//! - the **directory**: per 64 KiB page of title address space the number
//!   of the copy that holds the page (0: none) and the page's byte offset
//!   in it; one for all lists, entries only ever go from empty to filled.
//!   The CPU writes it in memory of its own and shaders read a copy of that
//!   on the card, brought up to date as title memory is (over the bus, every
//!   step of every ray waited for it);
//! - per view heap a run of slots with a raw unordered access view of each
//!   numbered copy (slot 0 of the run: a null view, for pages without a
//!   copy);
//! - the **faults**: the page numbers shaders found empty, read back here
//!   once a frame, so the pages are there from the next frame on. A pointer a
//!   shader takes out of memory cannot be known before it runs.
//!
//! **A page that is not there when a shader touches it is a lost access**:
//! the read gives 0, the store is dropped, and the fault note only helps
//! the next frame. For the structure builder that is the same as a dropped
//! stage, so before one of its dispatches the host makes ready what it can
//! see the dispatch name:
//!
//! - 256 KiB from each root view outside buffers, and where title memory has a
//!   packed ("dehydrated") structure there, all of it: the title ships its
//!   structures packed, and the builder's rehydration stages unpack each in
//!   place once it is streamed in;
//! - 4 MiB from each unordered access view outside buffers (structures built);
//! - the page of each pair of root constants that is a title address and of
//!   each root constant that is one in 256-byte units;
//! - the first page of every structure a list's upload memory names
//!   (a top-level build's instances).
//!
//! **What comes back to title memory**: what the builder stores in title
//! buffers of at most [`BACK`] bytes it names (the sizes the title reads to
//! make a structure smaller). Structures and scratch memory stay on the card
//! (the first whole run of the builder wrote garbage over the title's
//! objects). So an arena's page is the only place a structure is whole, and
//! title memory must not go over it: of a page the title wrote only the
//! dwords that changed since they were last taken go up. A packed structure
//! goes up whole when the builder comes to it, whatever is known of it: the
//! same bytes streamed in again do not differ from those last taken, and the
//! card holds them unpacked (run light-3: a quarter of the structures
//! unpacked a second time, into garbage all over their neighbours).
//!
//! **Stores are bounded**: a structure the builder misreads makes it store
//! anywhere. Pages of title buffers take its stores only once one of its
//! dispatches was given that buffer as an unordered access view (its scratch
//! memory, its results); what else a store reaches is an arena's.
//!
//! A packed structure is gone when title memory has no header of it any more
//! (looked at over [`SWEEP`] frames); the pages it alone has are given back.
//!
//! shortcut: a page held for anything else is kept for the rest of the run.
//! shortcut: pages a list does not name are looked at again in turn, over
//! [`SWEEP`] frames.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use victory_shader::emit::{MEMORY_FAULTS, MEMORY_PAGES, MEMORY_READ_ONLY};
use windows::Win32::Graphics::Direct3D12::*;

use crate::descriptor::host::HostHeap;
use crate::host::sets::{CopySet, Member};
use crate::host::{HostList, host};
use crate::shadow::Shadow;

/// Copies that can be numbered (the run of heap slots is one longer).
const COPIES: u32 = (1 << 12) - 1;
/// A directory page.
const PAGE: u64 = 1 << 16;
/// An arena's bytes.
const ARENA: u64 = 64 << 20;
/// What is compared at a time.
const PIECE: usize = 4096;
/// Pages held, at most (3 GiB).
const MOST: usize = 3 << 14;
/// Pages of title memory with small title buffers in them, at most.
const WINDOWS: usize = 512;
/// The biggest packed structure taken for one.
const PACKED: u64 = 64 << 20;
/// For so many frames what a page holds may still be on its way.
const YOUNG: u64 = 4;
/// Free pages there are, at least, when a list is recorded.
const SPARE: usize = 1024;
/// A packed structure not named for so many frames is another one.
const AGAIN: u64 = 8;
/// From a root view; from one that is written (a structure being built).
const VIEW: u64 = 256 << 10;
const BUILT: u64 = 4 << 20;
/// The biggest title buffer whose builder results come back.
const BACK: u64 = 256 << 10;
/// Pages taken from the faults, of one frame and in all, at most: a ray in a
/// structure that is not right goes anywhere, and 2,048 a frame took all the
/// pages there are in the first seconds (run light-4).
const FAULTS: usize = 128;
const FAULTS_ALL: usize = 2048;
/// The budget of pages given a place from the faults in a run:
/// [`FAULTS_ALL`], or what `VICTORY_MEMORY_FAULT_PAGES` says (to learn
/// whether the pages shaders go on finding empty once it is spent are data
/// they need: run rt-8 had some 250 a frame of them at title addresses
/// outside every structure the builder's calls named).
fn faults_all() -> usize {
    static ALL: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *ALL.get_or_init(|| {
        std::env::var("VICTORY_MEMORY_FAULT_PAGES").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(FAULTS_ALL)
    })
}

/// `VICTORY_MEMORY_DIRTY=1` (off): a list's submission asks the write watch
/// about the arenas that were written since, not about all of them.
///
/// Where the bytes are: an arena is memory of ours (title address space the
/// title never touches) with a copy on the card; this module writes it, under
/// its mutex, from title memory (`bring`, `take`) and nobody else does. Every
/// list that holds by-address memory used to refresh every arena whole at its
/// submission, and the submission asks the kernel once per refreshed copy,
/// 23 arenas to a list and 16 lists to a frame: 7 to 8 ms of the submitting
/// thread a frame (the same for no written page and for 250, 42 to 45 us a
/// copy; verified over four runs, 13 arenas +4.3 ms, 25 +8). The hops are
/// the same as before (title -> arena -> staging -> card); only the asking
/// for arenas nobody wrote goes.
///
/// A list cannot know at its recording which arenas will have been written
/// by its submission: a page a later dispatch of the same list brings, a
/// page another list brings meanwhile and the packed structures `land`
/// unpacks at the submission itself all come after its `bring_up` (run
/// light-8 showed the cost of missing one). So the writers note the pages
/// they wrote ([`Dirty`]), and each list holds a step that, when the
/// submission reaches it, takes the notes and brings exactly those runs of
/// pages up to date ([`settle`]): one after every `land`, one for the rest
/// of the list. Copies that are not arenas (title buffers, windows of title
/// memory) are written by the title and are refreshed whole as before.
fn dirty_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_MEMORY_DIRTY").is_some_and(|v| v == "1"))
}

/// `VICTORY_MEMORY_LEAN=1` (off): [`ready`] asks the kernel less, and not
/// under the mutex. A root view's page that has a place already is not asked
/// about again (`possible` is a working-set query and a write-watch asking,
/// 130 to 430 of them a frame for pages held), and the pages a call names
/// that have none are asked about before the mutex is taken: a recording
/// thread that stands in `watch::writes` behind the submitting thread's
/// asking (about 11 ms a frame with ray tracing) no longer holds every other
/// recording thread behind it. Differs from before only for title memory
/// that was decommitted after it was given a place: it is no longer found
/// out here, its deferred landing finds out (`land` looks itself).
fn lean_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_MEMORY_LEAN").is_some_and(|v| v == "1"))
}

/// `VICTORY_MEMORY_BUFFER_FAULTS=1` (off): a page shaders found empty that
/// lies whole in a title buffer goes to [`Memory::map`] whatever the budgets
/// are, and comes out of neither. Mapping it numbers the buffer's copy and
/// gives every page of the buffer its entry, no arena page, so the budgets
/// that keep stray pointers from taking the arenas have nothing to guard;
/// run 5 had pages of buffers of 8 to 94 MB found empty for ever once the
/// run's 2,048 were spent.
fn buffer_faults_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_MEMORY_BUFFER_FAULTS").is_some_and(|v| v == "1"))
}

/// Why [`Memory::map`] refused a page, for the report: a page outside what
/// the directory covers, one turned down earlier in the frame, memory that
/// is not committed, memory that is not write-watched, a title buffer that
/// has no copy, a copy that cannot be numbered, a window of title memory
/// that could not be made, no page left to hold, no arena to be had.
const WHYS: usize = 9;
const WHY_RANGE: usize = 0;
const WHY_AGAIN: usize = 1;
const WHY_UNCOMMITTED: usize = 2;
const WHY_UNWATCHED: usize = 3;
const WHY_NO_COPY: usize = 4;
const WHY_NO_NUMBER: usize = 5;
const WHY_NO_WINDOW: usize = 6;
const WHY_FULL: usize = 7;
const WHY_NO_ARENA: usize = 8;

/// The most runs of pages one arena is brought up in by one [`settle`]: each
/// is a kernel asking of its own (about 43 us on the submitting thread, which
/// is what 130 KB of copying takes, two pages), and joined across the smallest
/// gaps they cost the gap's pages (that were never taken, and go up as zeros)
/// instead. A streaming frame settles hundreds of times, so an arena with many
/// scattered runs must not cost many askings each time: past four the next gap
/// is cheaper to copy than to ask across.
const RUNS: usize = 4;

/// Pages a call's pre-asking looks at at most ([`ask_ahead_of_the_mutex`]).
const PRE: usize = 64;
/// Refusals one thread keeps for a frame ([`REFUSED`]); past it they are let go.
const REFUSED_MOST: usize = 4096;

/// Pages given a place in one frame for structures the builder's calls
/// named that shaders found empty (64 MiB: several top levels of 12 MB).
const NAMED: u64 = 1024;
/// Frames over which every page is looked at once.
const SWEEP: usize = 128;
/// Frames after an arena could not be had before another is asked for.
const ARENA_AGAIN: u64 = 30;

/// Whether what [`Memory::land`] stores at a submission reaches that
/// submission's sends (`VICTORY_MEMORY_LAND_SENT=1`, off by default).
///
/// Where the bytes are: title memory -> the arena (our memory, the copy's
/// "title memory") -> the copy's CPU side -> the submission's send memory
/// -> the card. With `VICTORY_JOIN_PARTS` above 1 the submission refreshes
/// every copy in front of its steps (`refresh_front` in `crate::host`),
/// and a later refresh of the same span in that submission is dropped.
/// `land` runs as a step, after that refresh: what it stores into an arena
/// (a packed structure streamed in between recording and submission, a
/// directory entry of a page placed then) is in the arena, not in the CPU
/// side the sends are taken from, so the submission sends the old bytes
/// and the right ones follow with the next refresh, after the builder's
/// dispatch read the old ones. With the switch the pages `land` stored are
/// noted here ([`landed`]) and the submission refreshes them once more
/// after its steps and before its sends, as it did before refreshes ran in
/// front. No hop is added: the refresh the next submission would have done
/// moves into this one. Only with the title's ray tracing; it is a matter
/// of order, not of frame time. If "stored at submissions" on the line
/// stays 0 over a streaming run, the case does not happen.
fn land_sent() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_MEMORY_LAND_SENT").is_some_and(|v| v == "1"))
}

/// With [`land_sent`]: the copies and whole pages of them stored at
/// submissions, for the submission to refresh after its steps. Apart from
/// the mutex: the submitting thread drains it without taking that.
static LANDED: Mutex<Vec<(Arc<Shadow>, usize, usize)>> = Mutex::new(Vec::new());
static LANDED_ANY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Since the last report: bytes stored at submissions by `land`, `place`
/// and `gone`, and bytes of stale pages the submissions then read into the
/// CPU side for them (pages read, not bytes that differed).
static LANDED_BYTES: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// `[from, to)` rounded out to whole pages and cut to the copy's span
/// `[first, first + len)`: a page refreshed whole is noted as taken, one
/// refreshed in part is read again by the next refresh.
fn whole_pages(from: usize, to: usize, first: usize, len: usize) -> Option<(usize, usize)> {
    let page = PAGE as usize;
    let (a, b) = ((from / page * page).max(first), to.div_ceil(page).saturating_mul(page).min(first + len));
    (from < to && a < b).then_some((a, b))
}

/// Runs by copy (`key`), those of one copy that overlap or touch as one.
fn joined<T: Clone>(mut runs: Vec<(usize, T, usize, usize)>) -> Vec<(T, usize, usize)> {
    runs.sort_by_key(|r| (r.0, r.2));
    let mut out: Vec<(usize, T, usize, usize)> = Vec::with_capacity(runs.len());
    for run in runs {
        match out.last_mut() {
            Some(last) if last.0 == run.0 && run.2 <= last.3 => last.3 = last.3.max(run.3),
            _ => out.push(run),
        }
    }
    out.into_iter().map(|(_, t, a, b)| (t, a, b)).collect()
}

/// Notes `[from, to)` of `copy` as stored at a submission ([`land_sent`]).
fn note_landed(copy: &Arc<Shadow>, from: usize, to: usize) {
    let (first, len) = copy.span();
    let Some((a, b)) = whole_pages(from, to, first as usize, len as usize) else { return };
    LANDED.lock().unwrap_or_else(|e| e.into_inner()).push((copy.clone(), a, b));
    LANDED_ANY.store(true, std::sync::atomic::Ordering::Release);
}

/// On the submitting thread, after a submission's steps: the pages of
/// copies stored at it ([`land_sent`]), whole pages, joined by copy. Empty
/// without the switch; takes no lock while nothing was noted.
pub(crate) fn landed() -> Vec<(Arc<Shadow>, usize, usize)> {
    if !land_sent() || !LANDED_ANY.swap(false, std::sync::atomic::Ordering::Acquire) {
        return Vec::new();
    }
    let noted = std::mem::take(&mut *LANDED.lock().unwrap_or_else(|e| e.into_inner()));
    joined(noted.into_iter().map(|(copy, a, b)| (Arc::as_ptr(&copy) as usize, copy, a, b)).collect())
}

/// The bytes a submission read into a CPU side for [`landed`]'s pages.
pub(crate) fn landed_read(bytes: usize) {
    LANDED_BYTES[1].fetch_add(bytes as u64, std::sync::atomic::Ordering::Relaxed);
}

/// What a builder or reading dispatch whose every key its list has used
/// already does ([`ready`]): `VICTORY_MEMORY_QUIET=1` returns it before the
/// mutex is taken; `=check` judges every call so before taking the mutex,
/// takes it all the same, and counts the calls judged quiet that did
/// something under it (that count must stay 0 over a streaming run before
/// `=1` is used); unset, every call takes the mutex as before.
///
/// Such a call only finds the heap's run under the lock. The saving is
/// thread time spent waiting, not time under the lock, and only with the
/// title's ray tracing: in the newest run with this line (224808) the
/// threads waited 55.5 ms a frame at dispatches in its first window and 2-8
/// ms after, and the time under the lock is the same either way; the
/// checking run's share of the waiting is the measure, not frame time.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Quiet {
    Off,
    Check,
    On,
}

fn quiet() -> Quiet {
    static ON: std::sync::OnceLock<Quiet> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("VICTORY_MEMORY_QUIET").as_deref() {
        Ok("1") => Quiet::On,
        Ok("check") => Quiet::Check,
        _ => Quiet::Off,
    })
}

/// Per host view heap (by address): the first slot of its run, and how many
/// copies have their view there. Written under the mutex after the views
/// are in the heap, read there and by a quiet call ([`quiet`]).
static HEAPS: std::sync::LazyLock<std::sync::RwLock<HashMap<usize, (u32, u32)>>> =
    std::sync::LazyLock::new(Default::default);
/// The frame the first look was done in ([`Memory::frame`]), for a quiet
/// call; set where that is.
static FRAME_LOOKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
/// How many copies are numbered, set at the push ([`Memory::number`]).
static COPIES_NOW: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// A builder dispatch would make an arena under the mutex (or ask the
/// arenas thread for one): no builder call is quiet. Set wherever the
/// mutex is let go by a call that may have used free pages.
static TOP_UP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
/// What a quiet call returns besides the run: fixed once memory by address
/// is made.
static READY_AT: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
/// Since the last report: quiet calls returned, reading and building; with
/// `=check` the calls judged quiet, of them those that did something, and
/// their microseconds waiting for the mutex.
static QUIET_STATS: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// The keys [`ready`] gives its list's `first_use`; the quiet test asks
/// the same ones (the two change together).
const READ: usize = usize::MAX - 0x6d;
/// A page named by a constant (kind 1) or a root view (kind 2).
fn page_key(kind: usize, at: u64) -> usize {
    usize::MAX - (kind << 26) - (at / PAGE) as usize
}
/// A root view the builder was given.
fn view_key(at: u64) -> usize {
    usize::MAX - (3 << 33) - (at >> 8) as usize
}
/// A root view the builder writes.
fn written_key(at: u64) -> usize {
    usize::MAX - (4 << 33) - (at >> 2 & 0x7FFF_FFFF) as usize
}
/// Every copy brought up, for `n` of them.
fn copies_key(n: usize) -> usize {
    usize::MAX - (6 << 33) - n
}

/// The title addresses `constants` name, in order: in two constants (low
/// dword, then bits 32 to 39), or in one in 256-byte units (the builder's
/// alone).
fn constant_addresses(constants: &[u32], builds: bool, mut each: impl FnMut(u64)) {
    for (i, &c) in constants.iter().enumerate() {
        if let Some(&high) = constants.get(i + 1)
            && (1..=0x1F).contains(&high)
            && c >= 0x1_0000
        {
            each(u64::from(c) | u64::from(high) << 32);
        }
        if builds && (0x0100_0000..0x2000_0000).contains(&c) {
            each(u64::from(c) << 8);
        }
    }
}

/// What a call with these arguments returns if its list has used every key
/// the long way of [`ready`] would give it, in the long way's order and
/// short cuts: the view key of every root view above 4 GiB when it builds
/// (asked before anything else of it), the written key only when it builds
/// and writes it, the page key of kind 2 for each (a packed structure is
/// only found inside a view key's first use), both constant forms, the copies
/// key and READ; and nothing to do besides: the frame's first look done,
/// no arena to make, the heap's run viewing every copy. None otherwise.
fn quiet_call(
    used: impl Fn(usize) -> bool,
    heap: usize,
    addresses: &[u64],
    written: &[u64],
    constants: &[u32],
    builds: bool,
) -> Option<Ready> {
    use std::sync::atomic::Ordering::Acquire;
    if FRAME_LOOKED.load(Acquire) != crate::frame::current() || (builds && TOP_UP.load(Acquire)) {
        return None;
    }
    let copies = COPIES_NOW.load(Acquire);
    let (base, made) = *HEAPS.read().unwrap_or_else(|e| e.into_inner()).get(&heap)?;
    if made as usize != copies || !keys_used(used, addresses, written, constants, builds, copies) {
        return None;
    }
    let &(directory, faults) = READY_AT.get()?;
    Some(Ready { directory, faults, base })
}

/// Whether `used` holds every key [`ready`]'s long way would give a call
/// with these arguments, `copies` copies numbered ([`quiet_call`]).
fn keys_used(used: impl Fn(usize) -> bool, addresses: &[u64], written: &[u64], constants: &[u32], builds: bool, copies: usize) -> bool {
    if !used(READ) || !used(copies_key(copies)) {
        return false;
    }
    let mut all = true;
    constant_addresses(constants, builds, |at| all = all && used(page_key(1, at)));
    all && addresses.iter().filter(|&&a| a >= 1 << 32).all(|&at| {
        (!builds || used(view_key(at)))
            && (!builds || !written.contains(&at) || used(written_key(at)))
            && used(page_key(2, at))
    })
}

/// Whether the arenas thread is making one now while pages are still free:
/// the pages left are used first. An arena made on a recording thread
/// meanwhile would wait for the same mapping lock with the mutex held, and
/// one of the two would be left over.
fn arena_coming(m: &Memory) -> bool {
    arenas_ahead() && AHEAD_BUSY.load(std::sync::atomic::Ordering::Relaxed) && !m.free.is_empty()
}

/// Whether a builder dispatch would make an arena (or ask for one ahead)
/// with memory as it is: [`TOP_UP`] from here.
fn note_top_up(m: &Memory) {
    if quiet() == Quiet::Off {
        return;
    }
    let frame = crate::frame::current();
    let make = !arena_coming(m) && m.free.len() < SPARE && m.pages.len() + m.free.len() < MOST && frame >= m.no_arena_before;
    let ahead = arenas_ahead()
        && m.ahead.is_none()
        && want_ahead(m.free.len(), m.pages.len(), m.copies.len())
        && frame >= AHEAD_PAUSED.load(std::sync::atomic::Ordering::Relaxed)
        && !AHEAD_BUSY.load(std::sync::atomic::Ordering::Relaxed);
    TOP_UP.store(make || ahead, std::sync::atomic::Ordering::Release);
}

/// Whether the builder's root view on a page held already is taken as the
/// title's without asking the kernel again (`VICTORY_MEMORY_ASK_RUNS=1`, off
/// by default), and the root views' time on the time line is split.
///
/// Asking is two kernel questions under the mutex (committed, and the write
/// watch for one page), each list's first naming of the view, though the
/// page has been held for minutes. Landing asks committed again at the
/// submission, and the next look at the page asks the write watch with a
/// later number, so nothing is missed; a held page the title gave back is
/// no longer counted as turned down. Expected: about 7-10 us per held root
/// view (QueryWorkingSetEx on 16 pages 5-7 us, a write watch asking about
/// 2 us, measured apart from the game), 1-2 ms a frame of the list threads'
/// time in the steady state; no measured share of the streaming frame until
/// a run with the split says otherwise. The other questions per run of
/// pages (one for the run, not one a page) are not made: through the
/// guards they could protect memory that is not the title's.
fn ask_runs() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_MEMORY_ASK_RUNS").is_some_and(|v| v == "1"))
}

/// Whether the next arena is made ahead, by a thread of its own
/// (`VICTORY_MEMORY_ARENAS_AHEAD=1`, off by default; with it unset no
/// thread is started and [`Memory::another`] makes every arena as before).
///
/// An arena is a search of the address space for 64 MiB and two 64 MiB
/// D3D12 buffers made resident and mapped, and it is made on a recording
/// thread under the one mutex: 13 arenas at about 19.4 ms each, 252 ms
/// once in the first ten seconds of run rt-b, about 40 ms in each later 10
/// s window, and single arenas of up to about 0.6 s in the newest run;
/// nothing without the title's ray tracing (`VICTORY_RT_NATIVE`), where a
/// run holds one arena. The thread only makes the memory and the copy, with
/// no lock held, and parks the pair in [`Memory::ahead`]; `another` alone
/// numbers an arena and hands out its pages, so lists learn of it when
/// they do today, and no arena more than today is numbered (each numbered
/// copy is refreshed and asked of the write watch at every submission that
/// reads by address). It is asked for only from a builder's dispatch, while
/// fewer than twice [`SPARE`] pages are free. It removes a hitch; it is not
/// the cause of the 300-450 ms streaming frames.
fn arenas_ahead() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_MEMORY_ARENAS_AHEAD").is_some_and(|v| v == "1"))
}

/// The arenas thread's call: set and told when an arena is wanted.
static AHEAD: (Mutex<bool>, std::sync::Condvar) = (Mutex::new(false), std::sync::Condvar::new());
/// An arena is asked of the thread or being made (or the thread could not
/// be started: then it stays set and nothing is asked again).
static AHEAD_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// After a failure the thread is not asked before this frame; the
/// recording threads keep their own pause ([`Memory::no_arena_before`]).
static AHEAD_PAUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Since the last report: arenas the thread made, its microseconds, and the
/// times it could not make one.
static AHEAD_STATS: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Whether another arena is wanted ahead with `free` pages free, `held`
/// held and `copies` numbered: fewer than twice [`SPARE`] free, and room
/// for all of its pages and for its number.
fn want_ahead(free: usize, held: usize, copies: usize) -> bool {
    free < 2 * SPARE && held + free + (ARENA / PAGE) as usize <= MOST && copies < COPIES as usize
}

/// Asks the arenas thread for an arena, starting it the first time. Called
/// with the mutex held: it only sets a flag and wakes the thread.
fn wake_ahead() {
    use std::sync::atomic::Ordering::Relaxed;
    if crate::frame::current() < AHEAD_PAUSED.load(Relaxed) || AHEAD_BUSY.swap(true, Relaxed) {
        return;
    }
    static STARTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let started = *STARTED.get_or_init(|| {
        let spawned = std::thread::Builder::new().name("victory-arenas".into()).spawn(make_ahead).is_ok();
        if !spawned {
            tracing::warn!("title memory by address: the arenas thread could not be started; arenas are made as before");
        }
        spawned
    });
    if !started {
        return;
    }
    *AHEAD.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
    AHEAD.1.notify_one();
}

/// The arenas thread: makes an arena's memory and copy with no lock held,
/// then takes the mutex alone to park them for [`Memory::another`].
fn make_ahead() {
    use std::sync::atomic::Ordering::Relaxed;
    loop {
        {
            let mut asked = AHEAD.0.lock().unwrap_or_else(|e| e.into_inner());
            while !*asked {
                asked = AHEAD.1.wait(asked).unwrap_or_else(|e| e.into_inner());
            }
            *asked = false;
        }
        let started = std::time::Instant::now();
        // (Fresh: nothing wrote it, and it starts as zeros on both sides.)
        let made = crate::upload::reserve_out_of_reach(ARENA as usize).and_then(|at| {
            let copy = crate::shadow::by_address(at as u64, ARENA, true);
            if copy.is_none() {
                crate::upload::release(at);
            }
            copy.map(|copy| (at, copy))
        });
        match made {
            None => {
                AHEAD_STATS[2].fetch_add(1, Relaxed);
                AHEAD_PAUSED.store(crate::frame::current() + ARENA_AGAIN, Relaxed);
            }
            Some((at, copy)) => {
                AHEAD_STATS[0].fetch_add(1, Relaxed);
                AHEAD_STATS[1].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
                let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
                match guard.as_mut() {
                    Some(m) if m.ahead.is_none() => m.ahead = Some((at, copy)),
                    // (Not wanted after all: its copy goes before its memory.)
                    _ => {
                        drop(guard);
                        drop(copy);
                        crate::upload::release(at);
                    }
                }
            }
        }
        AHEAD_BUSY.store(false, Relaxed);
    }
}

/// What a draw or dispatch does with title memory by address.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Use {
    Reads,
    /// Stores by address, as the structure builder; `back`: its results in
    /// small title buffers come back.
    Builds { back: bool },
}

/// A page of title memory in an arena.
struct Held {
    /// Where in the arena's memory, and the arena's place in `copies`.
    at: usize,
    copy: usize,
    /// The write-watch asking after which it was last taken (0: never).
    taken: u64,
    /// The frame it was last looked at in, and the one it came in.
    looked: u64,
    born: u64,
    /// Packed structures that have bytes in it, and whether it is held for
    /// them alone.
    owners: u32,
    theirs: bool,
}

/// A packed structure the builder came to.
struct Packed {
    /// The frame it last did.
    frame: u64,
    len: u64,
    /// Its first two dwords in title memory.
    header: u64,
}

/// The pages of arenas this module wrote since [`settle`] last took them up
/// (with `VICTORY_MEMORY_DIRTY`): per copy's number, the page numbers inside
/// the arena. Written and taken under the mutex.
#[derive(Default)]
struct Dirty {
    on: bool,
    pages: HashMap<usize, std::collections::BTreeSet<u32>>,
}

impl Dirty {
    /// `len` bytes at `offset` of copy `copy` were written (after the write).
    fn mark(&mut self, copy: usize, offset: u64, len: u64) {
        if !self.on || len == 0 {
            return;
        }
        let pages = self.pages.entry(copy).or_default();
        for page in offset / PAGE..=(offset + len - 1) / PAGE {
            pages.insert(page as u32);
        }
    }

    /// Everything noted, as runs of pages per copy; nothing is noted after.
    fn take(&mut self) -> Vec<(usize, Vec<(u32, u32)>)> {
        let mut all: Vec<_> = self.pages.drain().filter(|(_, pages)| !pages.is_empty()).map(|(copy, pages)| (copy, runs_of(&pages, RUNS))).collect();
        // (The copies in the order they were made: the same order each time.)
        all.sort_unstable_by_key(|run| run.0);
        all
    }
}

/// The pages as runs `[first, end)`, at most `most` of them: the smallest
/// gaps are closed first.
fn runs_of(pages: &std::collections::BTreeSet<u32>, most: usize) -> Vec<(u32, u32)> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &page in pages {
        match runs.last_mut() {
            Some(run) if run.1 == page => run.1 += 1,
            _ => runs.push((page, page + 1)),
        }
    }
    let most = most.max(1);
    while runs.len() > most {
        let Some(at) = (1..runs.len()).min_by_key(|&i| runs[i].0 - runs[i - 1].1) else { break };
        runs[at - 1].1 = runs[at].1;
        runs.remove(at);
    }
    runs
}

struct Memory {
    /// The CPU's entries (number, offset), and the copy shaders read.
    directory: (*mut u32, Arc<Shadow>),
    faults: (ID3D12Resource, *mut u32, u64),
    /// Copy `n` is `copies[n - 1]`.
    copies: Vec<Arc<Shadow>>,
    /// The set of copies a list brings up to date, made for the number of
    /// copies it holds. Copies only ever grow in number, and a copy's span and
    /// whether it is an arena do not change after it is numbered, so a set
    /// stays right while the count is the same.
    set: Option<Arc<CopySet>>,
    /// Places of title buffers' copies among `copies`.
    numbers: HashMap<usize, usize>,
    /// Arenas: place in `copies`, address.
    arenas: Vec<(usize, usize)>,
    /// By title page.
    pages: HashMap<u64, Held>,
    /// Their numbers in the order they came, and where the sweep is.
    order: Vec<u64>,
    sweep: usize,
    /// Windows of title memory itself ([`Memory::map`]).
    windows: usize,
    /// Packed structures the builder came to: the frame it last did.
    packed: HashMap<u64, Packed>,
    /// Their addresses in the order they came, and where the sweep is.
    packed_order: Vec<u64>,
    packed_sweep: usize,
    /// Places of pages that are free: the copy, the address in the arena.
    free: Vec<(usize, usize)>,
    /// Copies of title buffers the builder may store in.
    open: std::collections::HashSet<usize>,
    /// Pages taken from faults.
    faulted: usize,
    /// The frame the faults were last read in.
    frame: u64,
    /// Pages asked for this frame that are not the title's memory.
    refused: std::collections::HashSet<u64>,
    /// No arena is asked for before this frame ([`Memory::another`]).
    no_arena_before: u64,
    /// With [`arenas_ahead`]: an arena the thread made, its memory and its
    /// copy, not numbered yet; [`Memory::another`] takes it.
    ahead: Option<(usize, Arc<Shadow>)>,
    /// Per copy's number: whether it is an arena (memory only this module
    /// writes).
    arena_flags: Vec<bool>,
    /// Arena pages written and not yet brought up ([`settle`]).
    dirty: Dirty,
    /// Why [`Memory::map`] last said no (one of the `WHY_` numbers).
    why: usize,
    stats: Stats,
    #[cfg(feature = "diagnostics")]
    dump: dump::Dump,
    #[cfg(feature = "diagnostics")]
    trace: trace::Trace,
}

/// Since the last report.
#[derive(Default)]
struct Stats {
    from: Option<(std::time::Instant, u64)>,
    uses: [u64; 2],
    faults: u64,
    unknown: u64,
    back: u64,
    /// Pages looked at, bytes that changed, packed structures sent.
    looked: u64,
    changed: u64,
    packed: u64,
    packed_bytes: u64,
    gone: u64,
    /// Pages that came for an address in constants, a written view, another
    /// view, a structure named in upload memory.
    came: [u64; 4],
    micros: u64,
    /// Where the recording threads' time goes around the one mutex, in
    /// microseconds over all threads: waiting for it at a dispatch; holding
    /// it for the frame's first look (faults, the sweep), for new arenas,
    /// for what the constants and the root views name, for the views and
    /// the bringing up; and at a submission, waiting for it and landing.
    /// (Run rt-2: 36 s of the recording threads a frame once 19 arenas were
    /// held, against 95 ms without the builder, and no number said where.)
    waited: u64,
    held: [u64; 5],
    landing: [u64; 2],
    /// Times every copy was asked to be brought up to date by a list.
    brought_up: u64,
    /// Root views outside buffers made ready with a length the builder's
    /// call gave ([`structure_extent`]), and with a guessed one.
    sized: [u64; 2],
    /// Times an arena could not be had.
    no_arena: u64,
    /// With [`arenas_ahead`]: arenas taken from the thread's slot, and
    /// arenas made on a recording thread under the mutex.
    arenas_made: [u64; 2],
    /// With [`ask_runs`]: microseconds in `possible` and its calls, of them
    /// the microseconds in the root views' section; microseconds there in
    /// `named_in_upload` and in bringing up; root views whose page was held
    /// and not asked about.
    asks: [u64; 6],
    /// Of the pages shaders found empty: those below 4 GiB (not memory of
    /// the title's the directory covers: an offset on a base that read 0),
    /// those inside a structure the builder's calls named, and those of the
    /// rest that were given a place from the budgets; then those left
    /// without one because the budget for the run was spent, because the
    /// frame's was, and because the memory is not the title's.
    fault_kinds: [u64; 6],
    /// Pages shaders found empty that [`Memory::map`] refused, by the
    /// reason (the `WHY_` numbers).
    refusals: [u64; WHYS],
    /// Pages whole in a title buffer that went to `map` outside the budgets
    /// (`VICTORY_MEMORY_BUFFER_FAULTS`): given a place, refused.
    buffered: [u64; 2],
}

// SAFETY: the mappings are of memory the struct owns; all access is under
// the mutex.
unsafe impl Send for Memory {}

static MEMORY: Mutex<Option<Memory>> = Mutex::new(None);

/// What a draw binds.
pub(crate) struct Ready {
    pub(crate) directory: u64,
    pub(crate) faults: u64,
    /// The view heap slot of the null view; copy `n`'s view is `n` further.
    pub(crate) base: u32,
}

impl Memory {
    fn new() -> Option<Memory> {
        let host = host()?;
        let bytes = u64::from(MEMORY_PAGES) * 8;
        // Zeroed, and watched for writes as the title's memory is.
        // The CPU and the card's shadow copy read it, never a title address,
        // so a wide output (little room below 2^37) keeps it out of reach.
        let entries = if crate::upload::wide() {
            crate::upload::reserve_out_of_reach(bytes as usize)?
        } else {
            crate::upload::reserve(bytes as usize)?
        };
        // (Fresh: nothing wrote it, and it starts as zeros on both sides.)
        let directory = crate::shadow::by_address(entries as u64, bytes, true)?;
        crate::host::name(&directory.buffer, || "title memory by address: directory".into());
        let (faults, data) = host.shared_buffer(u64::from(MEMORY_FAULTS) * 4)?;
        crate::host::name(&faults, || "title memory by address: faults".into());
        // SAFETY: a fresh mapping of at least that many bytes.
        unsafe { std::ptr::write_bytes(data, 0, MEMORY_FAULTS as usize * 4) };
        // SAFETY: a live buffer.
        let gpu = unsafe { faults.GetGPUVirtualAddress() };
        tracing::info!("title memory by address: a shader reads or writes through raw addresses");
        DIRECTORY.store(entries, std::sync::atomic::Ordering::Release);
        Some(Memory {
            directory: (entries as *mut u32, directory),
            faults: (faults, data.cast::<u32>(), gpu),
            copies: Vec::new(),
            set: None,
            numbers: HashMap::new(),
            arenas: Vec::new(),
            pages: HashMap::new(),
            order: Vec::new(),
            sweep: 0,
            windows: 0,
            packed: HashMap::new(),
            packed_order: Vec::new(),
            packed_sweep: 0,
            free: Vec::new(),
            open: Default::default(),
            faulted: 0,
            frame: u64::MAX,
            refused: Default::default(),
            no_arena_before: 0,
            ahead: None,
            arena_flags: Vec::new(),
            dirty: Dirty { on: dirty_on(), ..Default::default() },
            why: 0,
            stats: Stats::default(),
            #[cfg(feature = "diagnostics")]
            dump: Default::default(),
            #[cfg(feature = "diagnostics")]
            trace: Default::default(),
        })
    }

    /// The directory's entry of `page`.
    fn entry(&self, page: u64) -> *mut u32 {
        debug_assert!(page < u64::from(MEMORY_PAGES));
        // SAFETY: inside the directory.
        unsafe { self.directory.0.add(page as usize * 2) }
    }

    /// The place in `copies` of the copy that holds `page`, if one does.
    fn copy_of(&self, page: u64) -> Option<usize> {
        if page >= u64::from(MEMORY_PAGES) {
            return None;
        }
        // SAFETY: an entry of the directory.
        ((unsafe { self.entry(page).read() } & !MEMORY_READ_ONLY) as usize).checked_sub(1)
    }

    /// Whether `page` can be given a place: in the directory, not turned down
    /// this frame, and the title's GPU memory. (A shader's pointer, or a
    /// constant taken for an address, may be anything.)
    fn possible(&mut self, page: u64) -> bool {
        if !ask_runs() {
            return self.possible_now(page);
        }
        let started = std::time::Instant::now();
        let ours = self.possible_now(page);
        self.stats.asks[0] += started.elapsed().as_micros() as u64;
        self.stats.asks[1] += 1;
        ours
    }

    fn possible_now(&mut self, page: u64) -> bool {
        // Not the last entry: addresses past the directory read it, and it
        // stays empty.
        if page < 0x1_0000 || page >= u64::from(MEMORY_PAGES) - 1 {
            self.why = WHY_RANGE;
            return false;
        }
        if self.refused.contains(&page) {
            self.why = WHY_AGAIN;
            return false;
        }
        // With `VICTORY_MEMORY_LEAN` the call may have asked already, before
        // it took the mutex ([`ask_ahead_of_the_mutex`]).
        let refusal = match asked_before(page) {
            Some(answer) => {
                LEAN[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                answer
            }
            None => not_ours(page),
        };
        let ours = refusal.is_none();
        if let Some(why) = refusal {
            self.why = why;
        }
        if !ours {
            self.refused.insert(page);
            self.stats.unknown += 1;
        }
        ours
    }

    /// Gives a copy a number, if one is left.
    fn number(&mut self, shadow: Arc<Shadow>) -> Option<usize> {
        let key = Arc::as_ptr(&shadow) as usize;
        if let Some(&n) = self.numbers.get(&key) {
            return Some(n);
        }
        if self.copies.len() >= COPIES as usize {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!(COPIES, "title memory by address: no number left for another copy; its pages read zeros"));
            return None;
        }
        // SAFETY: a live buffer.
        let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
        if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
            return None;
        }
        self.copies.push(shadow);
        // (At the push: a quiet call never sees more copies than there are.)
        COPIES_NOW.store(self.copies.len(), std::sync::atomic::Ordering::Release);
        let n = self.copies.len() - 1;
        self.numbers.insert(key, n);
        Some(n)
    }

    /// Gives `page` a place, if it has none and the memory is the title's;
    /// whether it has one then.
    fn map(&mut self, page: u64) -> bool {
        if self.copy_of(page).is_some() {
            return true;
        }
        if !self.possible(page) {
            return false;
        }
        let start = page * PAGE;
        if crate::shadow::in_buffer(start, PAGE).is_some() {
            let Some(shadow) = crate::shadow::find(start, PAGE) else {
                return self.refuse(WHY_NO_COPY);
            };
            let fresh = !self.numbers.contains_key(&(Arc::as_ptr(&shadow) as usize));
            let Some(n) = self.number(shadow.clone()) else {
                return self.refuse(WHY_NO_NUMBER);
            };
            // Every page the copy holds whole, not only the one asked for.
            // shortcut: a page of a title buffer inside this one that has a
            // copy of its own is given this copy too.
            if fresh {
                let (first, len) = shadow.span();
                let mut at = first.next_multiple_of(PAGE);
                while at + PAGE <= first + len && at / PAGE < u64::from(MEMORY_PAGES) - 1 {
                    let entry = self.entry(at / PAGE);
                    // SAFETY: an entry of the directory.
                    if unsafe { entry.read() } == 0 {
                        close(entry, n, at - first);
                    }
                    at += PAGE;
                }
            }
            // SAFETY: as above.
            if unsafe { self.entry(page).read() } == 0 {
                close(self.entry(page), n, start - shadow.span().0);
            }
            return true;
        }
        // Small title buffers in it: what the builder stores in them comes
        // back, so title memory itself.
        if self.windows < WINDOWS && crate::shadow::small_buffer_in(start, PAGE, BACK) {
            // (Title memory: its bytes are the title's, never fresh.)
            if let Some(shadow) = crate::shadow::by_address(start, PAGE, false)
                && let Some(n) = self.number(shadow)
            {
                self.windows += 1;
                fill(self.entry(page), n, 0);
                return true;
            }
            return self.refuse(WHY_NO_WINDOW);
        }
        if self.pages.len() >= MOST {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!(MOST, "title memory by address: no more pages are taken; others read zeros"));
            return self.refuse(WHY_FULL);
        }
        if let Some((copy, at)) = self.free.pop() {
            self.hold(page, at, copy);
            return true;
        }
        if !self.another() {
            return self.refuse(WHY_NO_ARENA);
        }
        let Some((copy, at)) = self.free.pop() else {
            return self.refuse(WHY_NO_ARENA);
        };
        self.hold(page, at, copy);
        true
    }

    /// `false`, with the reason [`Memory::map`] gave it (for the report).
    fn refuse(&mut self, why: usize) -> bool {
        self.why = why;
        false
    }

    /// Whether copy `n` is an arena: memory only this module writes.
    fn is_arena(&self, n: usize) -> bool {
        self.arena_flags.get(n).copied().unwrap_or(false)
    }

    /// Another arena's pages.
    ///
    /// After one that could not be made, no other is tried before
    /// the next frame. `ready` asks for one at every dispatch of the builder
    /// while the spares are low (thousands a frame); once the reservation
    /// failed every ask searched the address range again under this lock,
    /// and frames took 30 s until the title's watchdog ended the run (RTX
    /// 4090, 11-27: 19 arenas for minutes, the recording threads 70 % in
    /// the kernel under `upload::reserve`). A reservation whose copy could
    /// not be made is given back, not left behind.
    fn another(&mut self) -> bool {
        // One that could not be had is not asked for again at once: every
        // builder dispatch comes through here, a thousand and more a frame,
        // and a search of the title's whole address space that finds no
        // room takes milliseconds (runs rt-2 and rt-4: 5 to 18 s of every
        // frame went here, under the mutex, and the title's watchdog ended
        // both runs).
        if self.frame < self.no_arena_before {
            return false;
        }
        let failed = |m: &mut Memory, what: &'static str, error: u32| {
            m.no_arena_before = m.frame + ARENA_AGAIN;
            m.stats.no_arena += 1;
            static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                tracing::warn!(
                    what,
                    error,
                    arenas = m.arenas.len(),
                    pages = m.pages.len(),
                    "title memory by address: another arena could not be had; pages without a place read zeros until one can"
                );
            }
            false
        };
        // One the thread made ahead ([`arenas_ahead`]) is numbered here, as
        // one made now would be: every list learns of it at the same moment.
        let (at, copy) = match self.ahead.take() {
            Some((at, copy)) => {
                self.stats.arenas_made[0] += 1;
                (at, Some(copy))
            }
            None => {
                // (Where shaders do not look for it: they get its copy's number.)
                let Some(at) = crate::upload::reserve_out_of_reach(ARENA as usize) else {
                    return failed(self, "no address space for it", crate::upload::last_error());
                };
                if arenas_ahead() {
                    self.stats.arenas_made[1] += 1;
                }
                // (Fresh: nothing wrote it, and it starts as zeros on both sides.)
                (at, crate::shadow::by_address(at as u64, ARENA, true))
            }
        };
        let Some(n) = copy.and_then(|s| self.number(s)) else {
            // (Its memory is of no use without its copy.)
            crate::upload::release(at);
            return failed(self, "no copy of it on the host", 0);
        };
        self.arenas.push((n, at));
        if self.arena_flags.len() <= n {
            self.arena_flags.resize(n + 1, false);
        }
        self.arena_flags[n] = true;
        // The lowest first.
        self.free.extend((0..(ARENA / PAGE) as usize).rev().map(|i| (n, at + i * PAGE as usize)));
        true
    }

    /// `page` is held at `at` in the arena that is `copy`.
    fn hold(&mut self, page: u64, at: usize, copy: usize) {
        self.pages.insert(page, Held { at, copy, taken: 0, looked: u64::MAX, born: self.frame, owners: 0, theirs: false });
        self.order.push(page);
        fill(self.entry(page), copy, at as u64 - self.copies[copy].span().0);
    }

    /// The packed structure at `address` is gone: the pages it alone had are
    /// given back.
    fn gone(&mut self, address: u64) {
        let Some(packed) = self.packed.remove(&address) else { return };
        for page in address / PAGE..=(address + packed.len - 1) / PAGE {
            let Some(held) = self.pages.get_mut(&page) else { continue };
            held.owners = held.owners.saturating_sub(1);
            if held.owners == 0 && held.theirs {
                self.free.push((held.copy, held.at));
                self.pages.remove(&page);
                // SAFETY: an entry of the directory, under the mutex.
                unsafe { self.entry(page).write_volatile(0) };
                // At a submission the place may be given again by `place`
                // in the same `land`: the card's directory learns of the
                // zero with this submission ([`land_sent`]). (Gone between
                // submissions, the note costs the next one a page's look.)
                if land_sent() {
                    let entry = self.entry(page) as usize;
                    crate::watch::wrote(entry, 8);
                    note_landed(&self.directory.1, entry, entry + 8);
                }
            }
        }
        self.stats.gone += 1;
    }

    /// Whether the packed structure at `address` is still in title memory.
    fn there(&self, address: u64) -> bool {
        let mut header = [0u8; 8];
        crate::texture::committed(address as *const u8, 8) && {
            crate::host::copy_mapped(header.as_mut_ptr(), address as *const u8, 8, false);
            self.packed.get(&address).is_some_and(|p| p.header == u64::from_le_bytes(header))
        }
    }

    /// Looks at some of the pages and packed structures again.
    fn look_again(&mut self) {
        for _ in 0..self.order.len().div_ceil(SWEEP) {
            self.sweep = (self.sweep + 1) % self.order.len();
            let page = self.order[self.sweep];
            if self.pages.contains_key(&page) {
                self.bring(page, page + 1);
            } else {
                self.order.swap_remove(self.sweep);
                if self.order.is_empty() {
                    break;
                }
            }
        }
        for _ in 0..self.packed_order.len().div_ceil(SWEEP) {
            self.packed_sweep = (self.packed_sweep + 1) % self.packed_order.len();
            let address = self.packed_order[self.packed_sweep];
            if !self.packed.contains_key(&address) {
                self.packed_order.swap_remove(self.packed_sweep);
                if self.packed_order.is_empty() {
                    break;
                }
            } else if !self.there(address) {
                self.gone(address);
            }
        }
    }

    /// A dispatch of the builder was given `address` to write: if it is in a
    /// title buffer's copy, stores by address reach that from here on.
    fn opens(&mut self, address: u64) {
        let Some(n) = self.map(address / PAGE).then(|| self.copy_of(address / PAGE)).flatten() else {
            return;
        };
        if self.pages.contains_key(&(address / PAGE)) || !self.open.insert(n) {
            return;
        }
        // (Its pages hold what the CPU writes beside the builder's scratch.)
        self.copies[n].keep_stores();
        let (first, len) = self.copies[n].span();
        tracing::info!(
            start = format_args!("{first:#x}"),
            len,
            "title memory by address: the structure builder may store in a title buffer"
        );
        for page in first.div_ceil(PAGE)..(first + len) / PAGE {
            if self.copy_of(page) == Some(n) {
                fill(self.entry(page), n, page * PAGE - first);
            }
        }
    }

    /// [`Memory::map`] of the pages of `[address, address + len)`, up to the
    /// first that is not the title's, and what the title wrote there since
    /// they were taken goes into the arenas.
    fn ready(&mut self, address: u64, len: u64) {
        let first = address / PAGE;
        let mut end = first;
        while end * PAGE < address + len && self.map(end) {
            end += 1;
        }
        self.bring(first, end);
    }

    /// Takes the pages `[first, end)` that are in arenas from title memory
    /// where it wrote them.
    fn bring(&mut self, first: u64, end: u64) {
        // Once a frame: the builder names the same pages list after list.
        let frame = self.frame;
        if !(first..end).any(|page| self.pages.get(&page).is_some_and(|h| h.looked != frame)) {
            return;
        }
        let started = std::time::Instant::now();
        let answer = crate::watch::writes((first * PAGE) as usize, (end * PAGE) as usize, true);
        for page in first..end {
            let Some(held) = self.pages.get_mut(&page) else {
                continue;
            };
            if held.looked == frame {
                continue;
            }
            held.looked = frame;
            self.stats.looked += 1;
            let wrote = answer
                .as_ref()
                .map(|(_, w)| w.binary_search_by_key(&(page as usize), |w| w.0).map_or(0, |i| w[i].1));
            let now = answer.as_ref().map_or(1, |a| a.0);
            let stale = held.taken == 0 || wrote.is_none_or(|w| w > held.taken);
            if !stale {
                continue;
            }
            let never = std::mem::replace(&mut held.taken, now.max(1)) == 0;
            let shadow = &self.copies[held.copy];
            let base = held.at - shadow.span().0 as usize;
            let was = self.stats.changed;
            let mut title = [0u8; PIECE];
            for piece in (0..PAGE as usize).step_by(PIECE) {
                let from = (page * PAGE) as usize + piece;
                // (Pages that are not there read as zeros.)
                crate::host::copy_mapped(title.as_mut_ptr(), from as *const u8, PIECE, false);
                // SAFETY: a page of an arena of ours, under the mutex.
                let ours = unsafe { std::slice::from_raw_parts_mut((held.at + piece) as *mut u8, PIECE) };
                self.stats.changed += merge(&title, ours, never) as u64 * 4;
            }
            // Written now (a page taken for the first time whole, or one that
            // had dwords differ): the submission brings it up
            // ([`settle`]), after the bytes are in.
            if never || self.stats.changed != was {
                self.dirty.mark(held.copy, base as u64, PAGE);
            }
            if never {
                shadow.resend(base, base + PAGE as usize);
            }
        }
        self.stats.micros += started.elapsed().as_micros() as u64;
    }

    /// The bytes of a packed structure title memory has at `address` by now,
    /// and its first two dwords.
    fn packed_at(&self, address: u64) -> Option<(u64, u64)> {
        if address & 0xFF != 0 {
            return None;
        }
        let mut header = [0u8; 8];
        crate::host::copy_mapped(header.as_mut_ptr(), address as *const u8, 8, false);
        let header = u64::from_le_bytes(header);
        Some((packed_len(header)?, header))
    }

    /// At the submission of a list whose builder dispatch was given `address`
    /// outside buffers, in the order of its copies: what title memory has
    /// there now goes up. When the dispatch is recorded it may not be there
    /// yet: the title's queue waits for its streaming. (Nothing is asked of
    /// the write watch here: the submission's asking stands.)
    fn land(&mut self, address: u64) {
        let page = address / PAGE;
        if !crate::texture::committed((page * PAGE) as *const u8, PAGE as usize) {
            return;
        }
        let Some((len, header)) = self.packed_at(address) else {
            // Anything else: what came since its page was first taken.
            for page in page..=(address + VIEW - 1) / PAGE {
                if self.pages.get(&page).is_some_and(|h| h.born + YOUNG >= self.frame) {
                    self.take(page, 0, PAGE as usize, false);
                }
            }
            return;
        };
        if let Some(known) = self.packed.get_mut(&address) {
            let soon = known.frame + AGAIN >= self.frame;
            known.frame = self.frame;
            if known.header == header && soon {
                return;
            }
            if known.header != header {
                self.gone(address);
            }
        }
        let last = (address + len - 1) / PAGE;
        if !self.packed.contains_key(&address) {
            self.packed.insert(address, Packed { frame: self.frame, len, header });
            self.packed_order.push(address);
            for page in page..=last {
                let new = self.copy_of(page).is_none();
                if new && !self.place(page) {
                    continue;
                }
                if let Some(held) = self.pages.get_mut(&page) {
                    held.owners += 1;
                    held.theirs |= new;
                }
            }
            self.stats.packed_bytes += len;
        }
        for page in page..=last {
            let from = address.max(page * PAGE) - page * PAGE;
            let to = (address + len).min((page + 1) * PAGE) - page * PAGE;
            self.take(page, from as usize, to as usize, true);
        }
        self.stats.packed += 1;
    }

    /// A place in an arena for a page of committed title memory, at a
    /// submission: only one there is already (the list brings up the arenas
    /// it knew when it was recorded).
    fn place(&mut self, page: u64) -> bool {
        if page < 0x1_0000
            || page >= u64::from(MEMORY_PAGES) - 1
            || !crate::texture::committed((page * PAGE) as *const u8, PAGE as usize)
            || crate::shadow::in_buffer(page * PAGE, PAGE).is_some()
        {
            return false;
        }
        let Some((copy, at)) = self.free.pop() else {
            return false;
        };
        self.hold(page, at, copy);
        crate::watch::wrote(self.entry(page) as usize, 8);
        // The entry goes up with this submission, with the page's bytes
        // ([`land_sent`]).
        if land_sent() {
            let entry = self.entry(page) as usize;
            note_landed(&self.directory.1, entry, entry + 8);
            LANDED_BYTES[0].fetch_add(8, std::sync::atomic::Ordering::Relaxed);
        }
        self.take(page, 0, PAGE as usize, true);
        true
    }

    /// Bytes `from..to` of `page` from title memory into its arena's page, at
    /// a submission; `whole`: sent whether they differ from those last taken
    /// or not.
    fn take(&mut self, page: u64, from: usize, to: usize, whole: bool) {
        let Some(held) = self.pages.get(&page) else { return };
        let shadow = &self.copies[held.copy];
        let base = held.at - shadow.span().0 as usize;
        let mut title = [0u8; PIECE];
        let mut at = from;
        while at < to {
            let n = (PIECE - at % PIECE).min(to - at);
            crate::host::copy_mapped(title.as_mut_ptr(), (page as usize * PAGE as usize + at) as *const u8, n, false);
            // SAFETY: a page of an arena of ours, under the mutex.
            let ours = unsafe { std::slice::from_raw_parts_mut((held.at + at) as *mut u8, n) };
            if *ours != title[..n] {
                ours.copy_from_slice(&title[..n]);
                crate::watch::wrote(held.at + at, n);
                if land_sent() {
                    note_landed(shadow, held.at + at, held.at + at + n);
                    LANDED_BYTES[0].fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                }
                // (After the write, under the mutex: [`settle`] takes the
                // notes under it too.)
                self.dirty.mark(held.copy, (base + at) as u64, n as u64);
            }
            at += n;
        }
        if whole {
            // Told and noted whether the bytes differed or not: a list
            // thread's `bring` may have stored them after the refresh in
            // front of the steps, and then nothing above tells of them while
            // the CPU side still holds the old ones ([`land_sent`]).
            // Only where the CPU side lacks what the arena holds: that is
            // the bring-after-refresh case. Bytes the refresh in front took
            // already are not read a second time.
            if land_sent() && from < to && {
                let side = shadow.card_data();
                // SAFETY: the arena page holds `[from, to)` of the page and the
                // CPU side holds the same offsets of the copy; nothing
                // refreshes this copy while the submission is landing.
                side.is_null()
                    || unsafe {
                        std::slice::from_raw_parts((held.at + from) as *const u8, to - from)
                            != std::slice::from_raw_parts(side.add(base + from), to - from)
                    }
            } {
                crate::watch::wrote(held.at + from, to - from);
                note_landed(shadow, held.at + from, held.at + to);
            }
            shadow.resend(base + from, base + to);
        }
    }

    /// The pages shaders found empty since the last frame.
    fn take_faults(&mut self) {
        let mut pages = Vec::new();
        for i in 1..MEMORY_FAULTS as usize {
            // SAFETY: inside the faults' mapping; the GPU may be writing,
            // a number missed now is written again next frame.
            let at = unsafe { self.faults.1.add(i) };
            // SAFETY: as above.
            let page = unsafe { at.read_volatile() };
            if page != 0 {
                pages.push(u64::from(page));
                // SAFETY: as above.
                unsafe { at.write_volatile(0) };
            }
        }
        if pages.is_empty() {
            return;
        }
        pages.sort_unstable();
        let mut made = 0;
        // Pages given a place this frame because a structure the builder's
        // calls named lies on them, and the end of the last such structure
        // (the pages come sorted: a structure's other pages need no second
        // look).
        let (mut named, mut named_until) = (0u64, 0u64);
        for &page in &pages {
            if page < 0x1_0000 {
                self.stats.fault_kinds[0] += 1;
                continue;
            }
            if self.copy_of(page).is_some() {
                continue;
            }
            // A structure the builder was told to build or copy there: the
            // title's own word that the memory is a structure's, so it is
            // not a pointer gone astray and comes out of neither budget.
            // (Run rt-7: the budget for the run, 2,048 pages, was spent in
            // the first minute of the world and no page was given a place
            // after it, `made=0` in every line, while shaders went on
            // finding 300 to 1,400 pages a frame empty.) All of it, so that
            // its other pages do not each cost a frame of zeros.
            if page * PAGE < named_until {
                continue;
            }
            if named < NAMED
                && let Some((start, len)) = structure_extent_over(page)
            {
                let before = self.pages.len();
                self.ready(start, len);
                named += (self.pages.len() - before) as u64;
                named_until = start + len;
                self.stats.fault_kinds[1] += 1;
                continue;
            }
            // Whole in a title buffer (`VICTORY_MEMORY_BUFFER_FAULTS`): it
            // takes no arena page, so neither budget has anything to guard.
            // One page numbers the buffer's copy and gives all its pages
            // their entries.
            if buffer_faults_on() && crate::shadow::in_buffer(page * PAGE, PAGE).is_some() {
                if self.map(page) {
                    self.stats.buffered[0] += 1;
                } else {
                    self.stats.buffered[1] += 1;
                    self.stats.refusals[self.why.min(WHYS - 1)] += 1;
                }
                continue;
            }
            if self.faulted >= faults_all() {
                self.stats.fault_kinds[3] += 1;
            } else if made >= FAULTS {
                self.stats.fault_kinds[4] += 1;
            } else if self.map(page) {
                made += 1;
                self.faulted += 1;
                self.stats.fault_kinds[2] += 1;
                self.bring(page, page + 1);
            } else {
                self.stats.fault_kinds[5] += 1;
                self.stats.refusals[self.why.min(WHYS - 1)] += 1;
            }
        }
        self.stats.faults += pages.len() as u64;
        // Each is an access that read 0 or was dropped.
        static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let said = SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if said < 40 || said.is_multiple_of(100) {
            let some: Vec<String> = pages
                .iter()
                .step_by(pages.len().div_ceil(12))
                .map(|&p| format!("{:#x}", p * PAGE))
                .collect();
            tracing::warn!(
                frame = self.frame,
                pages = pages.len(),
                made,
                held = self.pages.len(),
                ?some,
                "title memory by address: shaders touched pages that had no copy (those accesses read 0 or were dropped); those that are the title's are there now"
            );
        }
    }

    /// The structures named in a list's upload memory around `address`
    /// (instances of a top-level build: a structure's address is their last
    /// 8 bytes): the first page of each.
    fn named_in_upload(&mut self, address: u64) {
        let Some((from, len)) = crate::upload::block(address as usize) else {
            return;
        };
        // SAFETY: the block is handed out and stays mapped; the title wrote
        // it before it recorded the dispatch.
        let words = unsafe { std::slice::from_raw_parts(from as *const u64, len / 8) };
        let mut seen = std::collections::HashSet::new();
        for &word in words {
            if word & 0xFF == 0 && (1 << 32..1 << 37).contains(&word) && seen.insert(word / PAGE) && self.map(word / PAGE) {
                let page = word / PAGE;
                self.bring(page, page + 1);
                // Where the builder's calls said how long the structure is:
                // its other pages that have no place yet (those that have
                // one are kept up to date where they are named, and are not
                // looked at again here: a top level names thousands).
                if let Some(len) = structure_extent_from(word) {
                    for other in page + 1..(word + len).div_ceil(PAGE) {
                        if self.copy_of(other).is_none() && self.map(other) {
                            self.bring(other, other + 1);
                        }
                    }
                }
            }
        }
    }

    /// A small title buffer whose results come back, if its pages are
    /// windows of title memory itself (or a copy of the buffer's own).
    fn small(&mut self, list: &HostList, start: u64, len: u64) {
        let mut at = start;
        while at < start + len {
            let page = at / PAGE;
            let n = ((page + 1) * PAGE).min(start + len) - at;
            self.map(page);
            if let Some(k) = self.copy_of(page)
                && !self.pages.contains_key(&page)
            {
                self.copies[k].refresh(list, at, n);
                self.copies[k].gpu_writes(list, at, n);
                self.stats.back += 1;
            }
            at += n;
        }
    }

    /// The members `bring_up` refreshes, in copy-number order: every copy
    /// except an arena when the dirty journal brings those up itself.
    fn set_members(&self) -> Vec<Member> {
        let tracked = self.dirty.on;
        let mut members = Vec::with_capacity(self.copies.len());
        for (n, shadow) in self.copies.iter().enumerate() {
            if tracked && self.is_arena(n) {
                continue;
            }
            // refresh clamps the length to the span and the address is its
            // start, so the span's length is the member's length
            let (start, len) = shadow.span();
            members.push(Member { shadow: shadow.clone(), at: start as usize, len: len as usize });
        }
        members
    }

    /// The set of copies a list brings up, kept while the number of copies
    /// stays the same. Its contract is checked at runtime by
    /// `VICTORY_REFRESH_SET_CHECK=1` and the walks are tested in host/sets.rs;
    /// a test here would need a `Shadow`, which needs a buffer.
    fn copy_set(&mut self) -> Arc<CopySet> {
        if let Some(set) = &self.set
            && set.copies() == self.copies.len()
        {
            if !crate::host::sets::check() {
                return set.clone();
            }
            // check mode: rebuild and compare, same copies, spans and order
            let fresh = self.set_members();
            let old = set.members();
            let same = fresh.len() == old.len()
                && fresh.iter().zip(old).all(|(a, b)| Arc::ptr_eq(&a.shadow, &b.shadow) && a.at == b.at && a.len == b.len);
            if same {
                return set.clone();
            }
            crate::host::sets::mismatch("a refresh set was out of date for the copies it was made for");
        }
        let set = Arc::new(CopySet::new(self.copies.len(), self.set_members()));
        self.set = Some(set.clone());
        set
    }

    /// Every copy is brought up to date when `list` is submitted, after what
    /// it deferred so far (once until a copy of the title's intervenes).
    /// `landed`: a landing was just deferred, whose writes come at the
    /// submission, after the recording.
    ///
    /// With `VICTORY_MEMORY_DIRTY` the arenas are not refreshed whole: a step
    /// brings up the pages written by then ([`settle`]), after every landing
    /// and once for the rest of the list. The directory and the copies of the
    /// title's memory (buffers, windows) are the title's to write, and stay
    /// as they were.
    ///
    /// With `VICTORY_REFRESH_SET` every copy is one step, `Step::RefreshSet`,
    /// which stands for the same refreshes in the same order.
    fn bring_up(&mut self, list: &HostList, landed: bool) {
        let (start, len) = self.directory.1.span();
        self.directory.1.refresh(list, start, len);
        let tracked = self.dirty.on;
        if crate::host::sets::on() {
            // one step for all the copies, which the walks expand to the
            // same refreshes the loop below would have pushed
            let set = self.copy_set();
            list.defer_refresh_set(&set);
        } else {
            for (n, shadow) in self.copies.iter().enumerate() {
                if tracked && self.is_arena(n) {
                    continue;
                }
                let (start, len) = shadow.span();
                shadow.refresh(list, start, len);
            }
        }
        if tracked && !self.arenas.is_empty() && (landed || list.first_use(usize::MAX - (7 << 33))) {
            list.defer_host(settle);
        }
    }

    /// Every ten seconds or so, what was used: the lines to write, which the
    /// caller does once it has let go of the mutex (none until it is time).
    fn report(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        let now = std::time::Instant::now();
        let (since, frame) = *self.stats.from.get_or_insert((now, self.frame));
        if now.duration_since(since).as_secs_f64() < 10.0 {
            return lines;
        }
        let frames = self.frame.wrapping_sub(frame).max(1) as f64;
        let s = std::mem::take(&mut self.stats);
        lines.push(format!(
            "title memory by address: {} pages ({} MB) in {} arenas, {} copies in all; per frame {:.0} dispatches that read, {:.0} that build, \
             {:.1} pages that had no copy, {:.1} pages turned down, {:.1} results brought back; {:.0} pages looked at, {:.1} KB changed, \
             {:.1} packed structures ({} there), {:.2} ms; {} windows; since the last line {} KB of packed structures came and {} of them went, \
             and pages for constants {}, views written {}, others {}, upload memory {}, faults in all {}; of the pages that had no copy, \
             per frame: {:.1} below 4 GiB (an offset on a base that read 0), {:.1} in structures the builder's calls named (given a place, all of each), \
             {:.1} others given a place from the budgets, {:.1} left without one with the run's budget spent (VICTORY_MEMORY_FAULT_PAGES), \
             {:.1} with the frame's spent, {:.1} that are not the title's memory",
            self.pages.len(),
            (self.pages.len() as u64 * PAGE) >> 20,
            self.arenas.len(),
            self.copies.len(),
            s.uses[0] as f64 / frames,
            s.uses[1] as f64 / frames,
            s.faults as f64 / frames,
            s.unknown as f64 / frames,
            s.back as f64 / frames,
            s.looked as f64 / frames,
            s.changed as f64 / 1024.0 / frames,
            s.packed as f64 / frames,
            self.packed.len(),
            s.micros as f64 / 1000.0 / frames,
            self.windows,
            s.packed_bytes >> 10,
            s.gone,
            s.came[0],
            s.came[1],
            s.came[2],
            s.came[3],
            self.faulted,
            s.fault_kinds[0] as f64 / frames,
            s.fault_kinds[1] as f64 / frames,
            s.fault_kinds[2] as f64 / frames,
            s.fault_kinds[3] as f64 / frames,
            s.fault_kinds[4] as f64 / frames,
            s.fault_kinds[5] as f64 / frames,
        ));
        // Where the threads' time went (all threads' together, a frame).
        let ms = |micros: u64| micros as f64 / 1000.0 / frames;
        lines.push(format!(
            "title memory by address, time per frame over all threads: {:.1} ms waiting for the mutex at dispatches; under it {:.1} ms the frame's first look, \
             {:.1} ms new arenas, {:.1} ms constants, {:.1} ms root views, {:.1} ms views and bringing up ({:.1} times every copy was brought up); \
             at submissions {:.1} ms waiting for it and {:.1} ms landing; a builder dispatch's recording in all: {:.1} ms the all-or-none check, \
             {:.1} ms binding, {:.1} ms the dispatch and what it writes; root views outside buffers sized by the builder's calls {:.1}, by a guess {:.1}; \
             arenas that could not be had since the last line: {}",
            ms(s.waited),
            ms(s.held[0]),
            ms(s.held[1]),
            ms(s.held[2]),
            ms(s.held[3]),
            ms(s.held[4]),
            s.brought_up as f64 / frames,
            ms(s.landing[0]),
            ms(s.landing[1]),
            ms(BUILDER_TIME[0].swap(0, std::sync::atomic::Ordering::Relaxed)),
            ms(BUILDER_TIME[1].swap(0, std::sync::atomic::Ordering::Relaxed)),
            ms(BUILDER_TIME[2].swap(0, std::sync::atomic::Ordering::Relaxed)),
            s.sized[0] as f64 / frames,
            s.sized[1] as f64 / frames,
            s.no_arena,
        ));
        if ask_runs() {
            let [all, calls, views, upload, up, held] = s.asks;
            lines.push(format!(
                "title memory by address, root views split per frame (VICTORY_MEMORY_ASK_RUNS=1): {:.2} ms asking whether pages are \
                 the title's ({:.1} askings), {:.2} ms of it in the root views; there {:.2} ms naming memory in upload buffers, \
                 {:.2} ms bringing up, {:.2} ms the rest; {:.1} held pages of the builder's views not asked about",
                ms(all),
                calls as f64 / frames,
                ms(views),
                ms(upload),
                ms(up),
                ms(s.held[3].saturating_sub(views + upload + up)),
                held as f64 / frames
            ));
        }
        if quiet() != Quiet::Off {
            use std::sync::atomic::Ordering::Relaxed;
            let [reads, builds, judged, did, waited] = [0, 1, 2, 3, 4].map(|i| QUIET_STATS[i].swap(0, Relaxed));
            lines.push(format!(
                "title memory by address, quiet calls (VICTORY_MEMORY_QUIET): per frame {:.1} reading and {:.1} building returned \
                 without the mutex (not in the counts above); checked: {:.1} a frame judged quiet, {:.1} ms a frame of the waiting \
                 for the mutex at dispatches theirs ({:.0}%), and {} judged quiet that did something since the last line",
                reads as f64 / frames,
                builds as f64 / frames,
                judged as f64 / frames,
                ms(waited),
                100.0 * waited as f64 / s.waited.max(1) as f64,
                did
            ));
        }
        if land_sent() {
            use std::sync::atomic::Ordering::Relaxed;
            let [stored, read] = [0, 1].map(|i| LANDED_BYTES[i].swap(0, Relaxed) as f64 / 1e6 / frames);
            lines.push(format!(
                "title memory by address, per frame (VICTORY_MEMORY_LAND_SENT=1): {stored:.2} MB stored at submissions by \
                 landing, {read:.2} MB of stale pages read into the CPU side behind it for the same submission's sends"
            ));
        }
        if arenas_ahead() {
            use std::sync::atomic::Ordering::Relaxed;
            let [made, micros, failed] = [0, 1, 2].map(|i| AHEAD_STATS[i].swap(0, Relaxed));
            lines.push(format!(
                "title memory by address, arenas since the last line (VICTORY_MEMORY_ARENAS_AHEAD=1): {made} made ahead in \
                 {:.1} ms on their thread ({failed} could not be had there), {} taken from its slot, {} made on a recording \
                 thread under the mutex; one {} parked",
                micros as f64 / 1000.0,
                s.arenas_made[0],
                s.arenas_made[1],
                if self.ahead.is_some() { "is" } else { "is not" }
            ));
        }
        // Why `map` refused the pages shaders found empty, and what the
        // three switches did (all counts over the frames since the last
        // line; a switch that is off counts nothing).
        use std::sync::atomic::Ordering::Relaxed;
        let per = |n: u64| n as f64 / frames;
        let lean = [0, 1, 2, 3].map(|i| LEAN[i].swap(0, Relaxed));
        let settled = [0, 1, 2, 3, 4, 5, 6].map(|i| SETTLED[i].swap(0, Relaxed));
        lines.push(format!(
            "title memory by address, per frame: pages that shaders found empty and `map` refused: {:.1} outside the directory, {:.1} turned down earlier              in the frame, {:.1} not committed, {:.1} not write-watched, {:.1} in a title buffer with no copy, {:.1} whose copy has no number,              {:.1} windows of title memory not made, {:.1} with no page left, {:.1} with no arena;              VICTORY_MEMORY_BUFFER_FAULTS ({}): {:.1} pages whole in title buffers given a place outside the budgets, {:.1} refused;              VICTORY_MEMORY_LEAN ({}): {:.1} pages asked about before the mutex, {:.1} answers used under it, {:.1} root views held and not asked about,              {:.1} named pages already placed; VICTORY_MEMORY_DIRTY ({}): {:.1} settling steps, {:.1} of them with nothing written, {:.1} arenas brought up,              {:.1} runs of pages, {:.1} KB copied, {:.2} ms on the submitting thread, {:.1} arena askings spared",
            per(s.refusals[WHY_RANGE]),
            per(s.refusals[WHY_AGAIN]),
            per(s.refusals[WHY_UNCOMMITTED]),
            per(s.refusals[WHY_UNWATCHED]),
            per(s.refusals[WHY_NO_COPY]),
            per(s.refusals[WHY_NO_NUMBER]),
            per(s.refusals[WHY_NO_WINDOW]),
            per(s.refusals[WHY_FULL]),
            per(s.refusals[WHY_NO_ARENA]),
            if buffer_faults_on() { "on" } else { "off" },
            per(s.buffered[0]),
            per(s.buffered[1]),
            if lean_on() { "on" } else { "off" },
            per(lean[0]),
            per(lean[1]),
            per(lean[2]),
            per(lean[3]),
            if dirty_on() { "on" } else { "off" },
            per(settled[0]),
            per(settled[1]),
            per(settled[2]),
            per(settled[3]),
            settled[4] as f64 / 1024.0 / frames,
            settled[5] as f64 / 1000.0 / frames,
            per(settled[6]),
        ));
        self.stats.from = Some((now, self.frame));
        lines
    }
}

/// The directory's entries, for reading without the mutex
/// ([`placed_in`]); 0 until it is made, and it is never made again.
static DIRECTORY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// What `VICTORY_MEMORY_LEAN` did since the last report: pages asked about
/// before the mutex was taken, answers that spared a kernel asking under
/// it, root views whose page has a place and were not asked about, pages
/// that had one already and were not asked about before the mutex.
static LEAN: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// What `VICTORY_MEMORY_DIRTY` did since the last report, over all
/// submissions: steps run, steps that found nothing written, arenas brought
/// up (one asking each at least), runs of pages, bytes they copied,
/// microseconds they took, and arenas not asked about that the whole
/// refreshes would have asked.
static SETTLED: [std::sync::atomic::AtomicU64; 7] = [const { std::sync::atomic::AtomicU64::new(0) }; 7];

/// Whether `page` has a copy in the directory's entries `entries`, read
/// without the mutex: an aligned dword that is written whole under it, and
/// an answer that is a moment old is as good as one a moment earlier (it
/// only decides that a question is not asked).
fn placed_in(entries: usize, page: u64) -> bool {
    if entries == 0 || page >= u64::from(MEMORY_PAGES) {
        return false;
    }
    // SAFETY: an entry of the directory, which lives as long as the process;
    // a volatile read of a dword the writers store whole.
    unsafe { ((entries as *const u32).add(page as usize * 2).read_volatile() & !MEMORY_READ_ONLY) != 0 }
}

/// Why the kernel says `page` is not title memory we can give a place:
/// `None` if it is.
fn not_ours(page: u64) -> Option<usize> {
    let at = (page * PAGE) as usize;
    if !crate::texture::committed(at as *const u8, PAGE as usize) {
        return Some(WHY_UNCOMMITTED);
    }
    crate::watch::writes(at, at + PAGE as usize, true).is_none().then_some(WHY_UNWATCHED)
}

thread_local! {
    /// Pages this thread's pre-asking was told are not the title's memory, with
    /// the reason, for the frame in the first field. The mutex's `refused`
    /// keeps a refused page from being asked about twice in a frame; a builder
    /// frame has a thousand dispatches naming the same constants that only look
    /// like addresses, so the pre-asking, which runs before that set can be
    /// read, keeps its own and asks the kernel once a page a frame, not once a
    /// dispatch.
    static REFUSED: std::cell::RefCell<(u64, HashMap<u64, usize>)> = std::cell::RefCell::new((0, HashMap::new()));
}

thread_local! {
    /// The kernel's answers for the pages the call on this thread names
    /// (page, [`not_ours`]), made before it took the mutex.
    static ASKED: std::cell::RefCell<Vec<(u64, Option<usize>)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// What [`ask_ahead_of_the_mutex`] found out of `page`: `None` if it did not
/// ask about it.
fn asked_before(page: u64) -> Option<Option<usize>> {
    ASKED.with(|asked| asked.borrow().iter().find(|a| a.0 == page).map(|a| a.1))
}

/// Drops the answers when the call is through.
struct Answers;

impl Drop for Answers {
    fn drop(&mut self) {
        ASKED.with(|asked| asked.borrow_mut().clear());
    }
}

/// The pages a call of [`ready`] may ask [`Memory::possible`] about, the
/// first [`PRE`] of them: the pages of each root view's address as far as
/// the builder's calls gave it length (else as far as the call will make
/// ready), and the page of each address the constants hold. `extent`: the
/// bytes from an address to the end of the structure the builder's calls
/// named there.
fn pages_named(addresses: &[u64], constants: &[u32], builds: bool, extent: impl Fn(u64) -> Option<u64>) -> Vec<u64> {
    let mut pages: Vec<u64> = Vec::new();
    let mut add = |page: u64| {
        if pages.len() < PRE && !pages.contains(&page) {
            pages.push(page);
        }
    };
    for &at in addresses.iter().filter(|&&a| a >= 1 << 32) {
        let len = extent(at).unwrap_or(if builds { VIEW } else { PAGE }).max(1);
        // (At most [`PRE`] are kept, whatever the length.)
        for page in (at / PAGE..=at.saturating_add(len - 1) / PAGE).take(PRE) {
            add(page);
        }
    }
    for (i, &c) in constants.iter().enumerate() {
        if let Some(&high) = constants.get(i + 1)
            && (1..=0x1F).contains(&high)
            && c >= 0x1_0000
        {
            add((u64::from(c) | u64::from(high) << 32) / PAGE);
        }
        if builds && (0x0100_0000..0x2000_0000).contains(&c) {
            add((u64::from(c) << 8) / PAGE);
        }
    }
    pages
}

/// With `VICTORY_MEMORY_LEAN`, before [`ready`] takes the mutex: asks the
/// kernel (committed, write-watched: a working-set query and a watch asking,
/// ms of waiting behind the submitting thread's own asking) about the pages
/// the call names that have no place yet, so that [`Memory::possible`] finds
/// the answers made. A page that is not asked about here is asked about
/// under the mutex, as before. The answers are those of a moment ago; the
/// memory may change in between, as it may between the asking and the use
/// under the mutex.
fn ask_ahead_of_the_mutex(addresses: &[u64], constants: &[u32], builds: bool) -> Answers {
    let entries = DIRECTORY.load(std::sync::atomic::Ordering::Acquire);
    let frame = crate::frame::current();
    let mut found = Vec::new();
    for page in pages_named(addresses, constants, builds, structure_extent_from) {
        if !(0x1_0000..u64::from(MEMORY_PAGES) - 1).contains(&page) {
            continue;
        }
        if placed_in(entries, page) {
            LEAN[3].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            continue;
        }
        // A page this thread was told is not the title's earlier in the frame
        // is answered the same without the kernel, as the mutex's own set does.
        let again = REFUSED.with(|refused| {
            let mut refused = refused.borrow_mut();
            if refused.0 != frame {
                *refused = (frame, HashMap::new());
            }
            refused.1.get(&page).copied()
        });
        if let Some(why) = again {
            found.push((page, Some(why)));
            continue;
        }
        let answer = not_ours(page);
        if let Some(why) = answer {
            REFUSED.with(|refused| {
                let mut refused = refused.borrow_mut();
                if refused.1.len() >= REFUSED_MOST {
                    refused.1.clear();
                }
                refused.1.insert(page, why);
            });
        }
        found.push((page, answer));
        LEAN[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    ASKED.with(|asked| *asked.borrow_mut() = found);
    Answers
}

/// A step of a list that holds by-address memory (`VICTORY_MEMORY_DIRTY`),
/// run when the list is submitted, on the submitting thread: the pages of
/// arenas written since the last one are brought up to date, each arena once
/// over the runs of its pages that were written, and an arena nobody wrote
/// is not asked about. The notes are taken under the mutex and the asking is
/// done outside it.
fn settle() {
    use std::sync::atomic::Ordering::Relaxed;
    let started = std::time::Instant::now();
    let (work, arenas) = {
        let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
        let Some(m) = guard.as_mut() else { return };
        let work: Vec<(Arc<Shadow>, Vec<(u32, u32)>)> = m
            .dirty
            .take()
            .into_iter()
            .filter_map(|(copy, runs)| Some((m.copies.get(copy)?.clone(), runs)))
            .collect();
        (work, m.arenas.len())
    };
    SETTLED[0].fetch_add(1, Relaxed);
    SETTLED[6].fetch_add(arenas.saturating_sub(work.len()) as u64, Relaxed);
    if work.is_empty() {
        SETTLED[1].fetch_add(1, Relaxed);
        return;
    }
    let mut bytes = 0;
    for (shadow, runs) in &work {
        let (start, len) = shadow.span();
        for &(first, end) in runs {
            let from = start as usize + first as usize * PAGE as usize;
            let to = (start as usize + end as usize * PAGE as usize).min((start + len) as usize);
            if from < to {
                bytes += shadow.update(from, to - from) as u64;
            }
        }
        SETTLED[3].fetch_add(runs.len() as u64, Relaxed);
    }
    SETTLED[2].fetch_add(work.len() as u64, Relaxed);
    SETTLED[4].fetch_add(bytes, Relaxed);
    SETTLED[5].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
}

/// What the recording threads spend in the structure builder's dispatches
/// since the last report, in microseconds over all threads: the
/// all-or-none check, the binding (which makes the memory ready, above),
/// and the dispatch itself with the noting of what it writes.
static BUILDER_TIME: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Adds the time since `began` to part `part` of [`BUILDER_TIME`]; the
/// moment the next part begins.
pub(crate) fn builder_time(part: usize, began: std::time::Instant) -> std::time::Instant {
    let now = std::time::Instant::now();
    if let Some(sum) = BUILDER_TIME.get(part) {
        sum.fetch_add(now.duration_since(began).as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
    }
    now
}

/// What the title's calls of its structure builder say lies where
/// (`victory_hle::modules::bvh_builder`, with `VICTORY_RT_NATIVE=1`): the
/// start of a structure being built or copied to, or of scratch memory, and
/// its length in bytes as the builder's own size calculation gives it.
/// Without that switch nothing is ever put in, and every dispatch is sized
/// by the guesses ([`VIEW`], [`BUILT`]) as before.
///
/// A dispatch names its memory by root views, which have a start and no
/// length. The guesses hold copies of 4 MiB of title memory for a structure
/// of a few KiB, and stop short of one that is longer: a top level of
/// 32,768 instances is 12 MB, and in run rt-3 some 800 pages a frame were
/// still found without a copy, in one stretch of addresses, long after the
/// faults' budget for the run was spent.
static EXTENTS: std::sync::RwLock<std::collections::BTreeMap<u64, u64>> = std::sync::RwLock::new(std::collections::BTreeMap::new());

/// Extents kept at most; past it all are let go (the builder's calls name
/// the live ones again as they come) and it is said once.
const EXTENTS_MOST: usize = 1 << 16;
/// The longest extent taken at its word (a size past it is a misread call).
const EXTENT_MOST: u64 = 4 * ARENA;

/// The builder was told to use `len` bytes of title memory from `start`
/// (see [`EXTENTS`]). An extent that begins inside it is replaced, and one
/// that reaches into it from below is cut short: memory is reused for
/// structures of other sizes.
pub fn structure_extent(start: u64, len: u64) {
    // Title memory the directory covers, and a length worth believing.
    if start < 1 << 32 || len == 0 || len > EXTENT_MOST || start + len > u64::from(MEMORY_PAGES) * PAGE {
        return;
    }
    let end = start + len;
    let mut extents = EXTENTS.write().unwrap_or_else(|e| e.into_inner());
    // (The same structure named again, frame after frame: nothing to do.)
    if extents.get(&start) == Some(&len) {
        return;
    }
    let inside: Vec<u64> = extents.range(start..end).map(|(&at, _)| at).collect();
    for at in inside {
        extents.remove(&at);
    }
    if let Some((&before, held)) = extents.range_mut(..start).next_back()
        && before + *held > start
    {
        *held = start - before;
    }
    if extents.len() >= EXTENTS_MOST {
        extents.clear();
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!(EXTENTS_MOST, "title memory by address: more extents named by the builder's calls than are kept; all were let go"));
    }
    extents.insert(start, len);
}

/// The bytes from `at` to the end of the extent that holds it
/// ([`structure_extent`]), if one does.
pub fn structure_extent_from(at: u64) -> Option<u64> {
    let extents = EXTENTS.read().unwrap_or_else(|e| e.into_inner());
    let (&start, &len) = extents.range(..=at).next_back()?;
    (at < start + len).then(|| start + len - at)
}

/// The extent that has bytes on `page` ([`structure_extent`]), as its start
/// and length, if one does. (Extents do not overlap; one that begins on the
/// page is taken before one that reaches into it from below.)
fn structure_extent_over(page: u64) -> Option<(u64, u64)> {
    let (from, end) = (page * PAGE, (page + 1) * PAGE);
    let extents = EXTENTS.read().unwrap_or_else(|e| e.into_inner());
    let (&start, &len) = extents.range(..end).next_back()?;
    (start + len > from).then_some((start, len))
}

/// The builder was told to copy the structure at `source` to `dest`
/// (compacting it). The title took the memory at `dest` by the compacted
/// size it read at `size_at`, the title address the build was told to
/// store that size at (0: not known): the copy's extent is that size, if
/// what is there now reads as one (a multiple of 64 bytes, no longer than
/// the structure it is a copy of). Otherwise nothing is noted, and the
/// copy's dispatches are sized by the guess as before; the source's own
/// length would reach over whatever lies behind the shorter copy.
///
/// Whether the size was plausible: said for the first calls
/// (`VICTORY_RT_TRACE` logs them), since a size that is not is the title
/// having read its slot before our submission landed there
/// ([`sizes_wait`]).
pub fn structure_copied(dest: u64, source: u64, size_at: u64) -> Option<u64> {
    let whole = structure_extent_from(source)?;
    if size_at < 1 << 32 {
        return None;
    }
    let mut bytes = [0u8; 8];
    // (Through the kernel: the slot is the title's, and may be gone.)
    if !crate::host::read_into(size_at as *const u8, &mut bytes) {
        return None;
    }
    let size = u64::from_le_bytes(bytes);
    if size == 0 || size > whole || size % 64 != 0 {
        return None;
    }
    structure_extent(dest, size);
    Some(size)
}

/// A list with this key ([`HostList::used`]) asks the builder for sizes.
const SIZES: usize = usize::MAX - (9 << 33);

/// Whether a submission that asks for the sizes of structures is waited for,
/// and landed, before the title goes on (`VICTORY_BVH_SIZES_WAIT=1`; off).
///
/// Off: it does not help. In run light-15, with it, 33 of 154 copies had the
/// slot's old size still (all of them recorded one frame after the size was
/// asked for) against 53 of 230 in light-11, 48 of 275 dynamic structures
/// were good, and the title asks in every frame, so every frame waits (109 ms
/// against 90). The title reads the slot before our submission of the asking
/// list is through, or reads it for another purpose; not known.
///
/// The title asks for a new structure's compacted size (a program of one
/// thread stores it into a small buffer), reads it a frame or two later
/// without looking whether it is there, takes memory of that size and copies
/// the structure into it. With submissions that land up to two frames late it
/// read what the slot held before, some other structure's size, and the copy
/// wrote over its neighbours: of 230 such copies of run light-11, 99 were
/// asked for two frames earlier and landed in that frame, none of those with
/// the right size.
fn sizes_wait() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BVH_SIZES_WAIT").is_some_and(|v| v == "1"))
}

/// Whether a dispatch of the builder is its program of a structure's size:
/// one thread, the structure's address and what to store (1: the compacted
/// size) as constants, a small title buffer to store into.
fn asks_size(written: &[u64], constants: &[u32], threads: Option<[u32; 3]>) -> bool {
    threads == Some([1, 1, 1])
        && matches!(*constants, [_, hi, 1..=3, ..] if (1..0x20).contains(&hi))
        && written.first().is_some_and(|&at| crate::shadow::in_buffer(at, 1).is_some_and(|b| b.1 <= BACK))
}

/// Whether one of `lists` asks for sizes.
pub(crate) fn asked_sizes<'a>(lists: impl Iterator<Item = &'a HostList>) -> bool {
    let mut lists = lists;
    let asked = sizes_wait() && lists.any(|l| l.used(SIZES));
    if asked {
        static TIMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = TIMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if n.is_power_of_two() || n.is_multiple_of(100) {
            tracing::info!(n, frame = crate::frame::current(), "submissions that ask for structures' sizes, waited for");
        }
    }
    asked
}

/// [`fill`] of a page stores by address do not reach.
fn close(entry: *mut u32, n: usize, offset: u64) {
    fill(entry, n | MEMORY_READ_ONLY as usize, offset);
}

/// The offset first: a shader that sees the number sees it too.
fn fill(entry: *mut u32, n: usize, offset: u64) {
    // SAFETY: an entry of the directory, under the mutex.
    unsafe {
        entry.add(1).write_volatile(offset as u32);
        entry.write_volatile(n as u32 + 1);
    }
}

/// The bytes of a packed structure whose first two dwords these are: the
/// version (6) with the marks of a bottom level and of one that is packed,
/// and its size unpacked in 64-byte nodes.
fn packed_len(header: u64) -> Option<u64> {
    let (version, nodes) = (header as u32, header >> 32);
    let marks = version >> 16;
    // The header counts its nodes of 64 bytes without the last one: the
    // builder's own program of a structure's size stores one more.
    let len = (nodes + 1) * 64;
    (version & 0xFFFF == 6 && marks & 0x8100 == 0x8100 && nodes > 0 && len <= PACKED).then_some(len)
}

/// Puts what `title` holds into `ours` (what was last taken): the dwords that
/// differ. How many.
fn merge(title: &[u8; PIECE], ours: &mut [u8], never: bool) -> usize {
    if never {
        ours.copy_from_slice(title);
        return PIECE / 4;
    }
    if ours == title {
        return 0;
    }
    let mut changed = 0;
    for (t, o) in title.chunks_exact(4).zip(ours.chunks_exact_mut(4)) {
        if t != o {
            o.copy_from_slice(t);
            changed += 1;
        }
    }
    changed
}

/// The lines of [`Memory::report`], written when this goes out of scope.
struct Said(Vec<String>);

impl Drop for Said {
    fn drop(&mut self) {
        for line in &self.0 {
            tracing::info!("{line}");
        }
    }
}

/// Before a draw whose shader reads title memory by address: what its
/// root arguments name is made ready (`addresses`: its root views, of them
/// `written` those it writes; `constants`: its root constants, see the
/// module), every numbered copy
/// has its view in `heap`'s run and is brought up to date when `list` is
/// submitted. None if the host cannot hold it; the draw is then dropped.
pub(crate) fn ready(
    list: &HostList,
    heap: &HostHeap,
    addresses: &[u64],
    written: &[u64],
    constants: &[u32],
    use_: Use,
    threads: Option<[u32; 3]>,
) -> Option<Ready> {
    let builds = use_ != Use::Reads;
    // A call that names nothing new needs nothing under the lock
    // ([`quiet`]). The size key is given first: it is the list's alone.
    // (Not while the diagnostics trace or dump look at every call.)
    #[cfg(feature = "diagnostics")]
    let looked_at = trace::on() || dump::folder().is_some();
    #[cfg(not(feature = "diagnostics"))]
    let looked_at = false;
    let mut judged_quiet = false;
    if quiet() != Quiet::Off && !looked_at {
        if builds && sizes_wait() && asks_size(written, constants, threads) {
            list.first_use(SIZES);
        }
        let heap_key = std::ptr::from_ref(heap) as usize;
        if let Some(ready) = quiet_call(|key| list.used(key), heap_key, addresses, written, constants, builds) {
            if quiet() == Quiet::On {
                QUIET_STATS[usize::from(builds)].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Some(ready);
            }
            judged_quiet = true;
        }
    }
    // With `VICTORY_MEMORY_QUIET=check`: whether this call did anything
    // under the lock (a key's first use, the frame's first look, an arena,
    // a heap's run or a view).
    let did = std::cell::Cell::new(false);
    let first_use = |key: usize| {
        let first = list.first_use(key);
        did.set(did.get() | first);
        first
    };
    // With `VICTORY_MEMORY_LEAN`: what the kernel is asked about the pages
    // this call names, it is asked before the mutex is taken, and the
    // answers are dropped when the call is through. (After the quiet
    // return above: a call that needs nothing under the lock asks nothing.)
    let lean = lean_on();
    let _answers = lean.then(|| ask_ahead_of_the_mutex(addresses, constants, builds));
    // (Timed in sections for the report's line: see `Stats::held`.)
    let asked = std::time::Instant::now();
    // (Declared before the guard, so it is dropped after it: the report's
    // lines are written with the mutex let go, whichever way this returns.)
    let mut said = Said(Vec::new());
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Memory::new();
    }
    let m = guard.as_mut()?;
    let mut mark = std::time::Instant::now();
    m.stats.waited += mark.duration_since(asked).as_micros() as u64;
    if quiet() != Quiet::Off {
        let directory = &m.directory.1;
        READY_AT.get_or_init(|| (directory.gpu(directory.span().0), m.faults.2));
        if judged_quiet {
            QUIET_STATS[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            QUIET_STATS[4].fetch_add(mark.duration_since(asked).as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
        }
    }
    // The time since the last mark goes to section `i`.
    let mut section = |m: &mut Memory, i: usize| {
        let now = std::time::Instant::now();
        m.stats.held[i] += now.duration_since(mark).as_micros() as u64;
        mark = now;
    };
    let frame = crate::frame::current();
    if m.frame != frame {
        m.frame = frame;
        m.refused.clear();
        m.take_faults();
        m.look_again();
        said.0 = m.report();
        #[cfg(feature = "diagnostics")]
        m.trace.look(frame);
        // (After the look: a quiet call finds it done.)
        FRAME_LOOKED.store(frame, std::sync::atomic::Ordering::Release);
        did.set(true);
    }
    section(m, 0);
    #[cfg(feature = "diagnostics")]
    if use_ != Use::Reads {
        m.trace.dispatch(frame, list, addresses, written, constants, threads);
    }
    // (Given before the lock with a quiet switch, unless the trace or the
    // dump sent every call the long way.)
    if (quiet() == Quiet::Off || looked_at) && builds && sizes_wait() && asks_size(written, constants, threads) {
        list.first_use(SIZES);
    }
    let coming = arena_coming(m);
    while builds && !coming && m.free.len() < SPARE && m.pages.len() + m.free.len() < MOST && m.another() {
        did.set(true);
    }
    // The next arena made ahead while there is still room ([`arenas_ahead`]).
    if builds && arenas_ahead() && m.ahead.is_none() && want_ahead(m.free.len(), m.pages.len(), m.copies.len()) {
        wake_ahead();
    }
    section(m, 1);
    m.stats.uses[usize::from(builds)] += 1;
    // Once per list and page named: the builder dispatches hundreds of times
    // a frame with the same arguments. (The keys change together with
    // `quiet_call`'s.)
    let named = |kind: usize, at: u64| first_use(page_key(kind, at));
    // A title address in two constants (low dword, then bits 32 to 39), or
    // in one in 256-byte units (the builder's alone). A constant that only
    // looks like one names memory nothing reads; one that is not the title's
    // memory is refused.
    let held = |m: &Memory| m.pages.len() as u64;
    let before = held(m);
    constant_addresses(constants, builds, |at| {
        if named(1, at) {
            m.ready(at, 1);
        }
    });
    let mut came = [held(m) - before, 0, 0, 0];
    section(m, 2);
    // (`possible`'s time in this section, for [`ask_runs`]'s line.)
    let asked_before = m.stats.asks[0];
    // With [`ask_runs`] a page held already is not asked about again.
    let held_page = |m: &mut Memory, page: u64| {
        let held = ask_runs() && m.pages.contains_key(&page);
        m.stats.asks[5] += u64::from(held);
        held
    };
    for &at in addresses.iter().filter(|&&a| a >= 1 << 32) {
        // (Several structures to a page.)
        let mut packed = false;
        if builds
            && first_use(view_key(at))
            && crate::shadow::in_buffer(at, 1).is_none()
            && (held_page(m, at / PAGE)
                || if lean && m.copy_of(at / PAGE).is_some() {
                    // It has a place: the kernel need not be asked again.
                    LEAN[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    true
                } else {
                    m.possible(at / PAGE)
                })
        {
            // What it needs, as far as it is known now; the rest when the
            // list is submitted ([`Memory::land`]).
            if let Some((len, _)) = m.packed_at(at) {
                packed = true;
                for page in at / PAGE..=(at + len - 1) / PAGE {
                    let new = m.copy_of(page).is_none();
                    if m.map(page)
                        && new
                        && let Some(held) = m.pages.get_mut(&page)
                    {
                        held.theirs = true;
                    }
                }
                m.bring(at / PAGE, (at + len - 1) / PAGE + 1);
            }
            list.defer_host(move || {
                let asked = std::time::Instant::now();
                if let Some(m) = MEMORY.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                    let got = std::time::Instant::now();
                    m.land(at);
                    m.stats.landing[0] += got.duration_since(asked).as_micros() as u64;
                    m.stats.landing[1] += got.elapsed().as_micros() as u64;
                    // (Landing may have placed pages from the free ones.)
                    note_top_up(m);
                }
            });
            let started = ask_runs().then(std::time::Instant::now);
            m.bring_up(list, true);
            if let Some(started) = started {
                m.stats.asks[4] += started.elapsed().as_micros() as u64;
            }
            m.stats.brought_up += 1;
        }
        if builds && written.contains(&at) && first_use(written_key(at)) {
            if let Some((start, len)) = crate::shadow::in_buffer(at, 1)
                && len <= BACK
                && use_ == (Use::Builds { back: true })
                && first_use(usize::MAX - (5 << 33) - (start >> 2 & 0x7FFF_FFFF) as usize)
            {
                m.small(list, start, len);
            }
            m.opens(at);
            if !packed && crate::shadow::in_buffer(at, 1).is_none() {
                #[cfg(feature = "diagnostics")]
                {
                    let built = m.dump.built.entry(at).or_insert((frame, frame, 0));
                    (built.1, built.2) = (frame, built.2 + 1);
                }
                let before = held(m);
                // As long as the builder's call said it is, where it did.
                let known = structure_extent_from(at);
                m.stats.sized[usize::from(known.is_none())] += 1;
                m.ready(at, known.unwrap_or(BUILT));
                came[1] += held(m).saturating_sub(before);
            }
        }
        if packed || !named(2, at) {
            continue;
        }
        let before = held(m);
        match crate::shadow::in_buffer(at, 1) {
            Some(_) => {
                m.ready(at, 1);
                let started = ask_runs().then(std::time::Instant::now);
                let asked_in = m.stats.asks[0];
                m.named_in_upload(at);
                if let Some(started) = started {
                    // (Less its own askings, which the views' share counts.)
                    let own = m.stats.asks[0] - asked_in;
                    m.stats.asks[3] += (started.elapsed().as_micros() as u64).saturating_sub(own);
                }
                came[3] += held(m).saturating_sub(before);
            }
            None => {
                // A structure a program walks, or memory a stage reads: all
                // of it where the builder's call gave its length.
                let known = structure_extent_from(at);
                m.stats.sized[usize::from(known.is_none())] += 1;
                m.ready(at, known.unwrap_or(if builds { VIEW } else { PAGE }));
                came[2] += held(m).saturating_sub(before);
            }
        }
    }
    for (sum, n) in m.stats.came.iter_mut().zip(came) {
        *sum += n;
    }
    m.stats.asks[2] += m.stats.asks[0] - asked_before;
    section(m, 3);
    let key = std::ptr::from_ref(heap) as usize;
    let run = HEAPS.read().unwrap_or_else(|e| e.into_inner()).get(&key).copied();
    let (base, made) = match run {
        Some(run) => run,
        None => {
            // Held for the heap's life: not given back with a list's slots.
            let Some(base) = heap.dynamic(COPIES + 1) else {
                note_top_up(m);
                return None;
            };
            // Every slot of the run: a number shows no view of another kind.
            for n in 0..=COPIES {
                heap.null_raw_uav_at(base + n);
            }
            did.set(true);
            (base, 0)
        }
    };
    for (n, shadow) in m.copies.iter().enumerate().skip(made as usize) {
        let (_, len) = shadow.span();
        heap.raw_uav_at(base + 1 + n as u32, &shadow.buffer, 0, len);
        did.set(true);
    }
    // (After the views are in the heap: a quiet call may use them.)
    // (Written only when it changes: quiet calls read it without the mutex.)
    let now = (base, m.copies.len() as u32);
    if run != Some(now) {
        HEAPS.write().unwrap_or_else(|e| e.into_inner()).insert(key, now);
    }
    // Every copy by every list that has come to by-address memory, whoever
    // numbered it: an arena another list made while this one was being
    // recorded was brought up to date by that list's submission alone, which
    // may run after this one's (structures unpacked from zeros, and then
    // written over by the arena's first upload: in frames that stream much in
    // half of them, run light-8).
    let first_read = first_use(READ);
    if first_use(copies_key(m.copies.len())) {
        m.bring_up(list, false);
    }
    if first_read {
        list.keep(&m.faults.0);
        #[cfg(feature = "diagnostics")]
        m.dump(list, addresses);
    }
    #[cfg(feature = "diagnostics")]
    if use_ == Use::Reads {
        m.dump.walked(addresses);
    }
    section(m, 4);
    note_top_up(m);
    if judged_quiet && did.get() {
        QUIET_STATS[3].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let directory = &m.directory.1;
    Some(Ready {
        directory: directory.gpu(directory.span().0),
        faults: m.faults.2,
        base,
    })
}

/// `VICTORY_BVH_TRACE=1`: the sizes the builder is asked for (its program of
/// one thread that stores a structure's compacted size) and when title memory
/// has them, and the copies to a place of that size.
#[cfg(feature = "diagnostics")]
mod trace {
    use super::*;

    #[derive(Default)]
    pub(super) struct Trace {
        /// Frame, where the size goes, the structure, what title memory had.
        asked: Vec<(u64, u64, u64, u64)>,
        /// By structure, what its size was when last seen to change.
        sizes: HashMap<u64, (u64, u64)>,
        said: u32,
    }

    pub(super) fn on() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("VICTORY_BVH_TRACE").is_some_and(|v| v == "1"))
    }

    fn value(at: u64) -> u64 {
        let mut b = [0u8; 8];
        crate::host::copy_mapped(b.as_mut_ptr(), at as *const u8, 8, false);
        u64::from_le_bytes(b)
    }

    impl Trace {
        fn say(&mut self, text: String) {
            self.said += 1;
            if self.said < 6000 {
                tracing::info!("bvh trace: {text}");
            }
        }

        pub(super) fn look(&mut self, frame: u64) {
            if !on() {
                return;
            }
            let mut gone = Vec::new();
            for i in 0..self.asked.len() {
                let (then, at, structure, was) = self.asked[i];
                let now = value(at);
                if now != was {
                    self.sizes.insert(structure, (now, frame));
                    gone.push(i);
                    self.say(format!("frame {frame}: size of {structure:#x} at {at:#x} asked in {then}: {was:#x} -> {now:#x}"));
                } else if frame > then + 60 {
                    gone.push(i);
                    self.say(format!("frame {frame}: size of {structure:#x} at {at:#x} asked in {then}: still {was:#x}"));
                }
            }
            for i in gone.into_iter().rev() {
                self.asked.swap_remove(i);
            }
        }

        pub(super) fn dispatch(
            &mut self,
            frame: u64,
            list: &HostList,
            addresses: &[u64],
            written: &[u64],
            constants: &[u32],
            threads: Option<[u32; 3]>,
        ) {
            if !on() {
                return;
            }
            let _ = addresses;
            let list = std::ptr::from_ref(list) as usize & 0xFFFF_FFFF;
            if threads == Some([1, 1, 1])
                && let [lo, hi, mode, ..] = *constants
                && (1..=3).contains(&mode)
                && hi < 0x20
                && let Some(&at) = written.first()
                && crate::shadow::in_buffer(at, 1).is_some_and(|b| b.1 <= BACK)
            {
                let structure = u64::from(lo) | u64::from(hi) << 32;
                let was = value(at);
                self.say(format!("frame {frame} list {list:x}: asks mode {mode} of {structure:#x} to {at:#x}, there {was:#x}"));
                if mode == 1 {
                    self.asked.push((frame, at, structure, was));
                }
            } else if threads == Some([32, 1, 1])
                && let [from, to] = *written
                && [from, to].iter().all(|&a| a >= 1 << 32 && crate::shadow::in_buffer(a, 1).is_none())
            {
                let known = self.sizes.get(&from).copied();
                self.say(format!("frame {frame} list {list:x}: copies {from:#x} to {to:#x} constants {:x?}, its size was {known:x?}", &constants[..constants.len().min(4)]));
            }
        }
    }
}

/// `VICTORY_MEMORY_DUMP=<folder>`: when a file `go` appears there, the arenas
/// as the card holds them are copied out and written (`arena-<n>.bin`), with
/// `pages.txt` (title page, arena, offset) and the structures the walking
/// programs were given: to look at the structures offline.
#[cfg(feature = "diagnostics")]
mod dump {
    use super::*;

    #[derive(Default)]
    pub(super) struct Dump {
        walked: std::collections::BTreeSet<u64>,
        /// Views the builder was given to write outside buffers: the first
        /// and the last frame, how often.
        pub(super) built: std::collections::BTreeMap<u64, (u64, u64, u64)>,
        /// The frame it is written at, and the copies.
        due: Option<(u64, Vec<(ID3D12Resource, usize)>)>,
        done: bool,
    }

    impl Dump {
        pub(super) fn walked(&mut self, addresses: &[u64]) {
            if !self.done && self.walked.len() < 64 {
                self.walked.extend(addresses);
            }
        }
    }

    pub(super) fn folder() -> Option<&'static std::path::PathBuf> {
        static FOLDER: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
        FOLDER.get_or_init(|| std::env::var_os("VICTORY_MEMORY_DUMP").map(Into::into)).as_ref()
    }

    fn turn(buffer: &ID3D12Resource, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) -> D3D12_RESOURCE_BARRIER {
        D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    // SAFETY: the same pointer, not counted; the barrier is
                    // used while the buffer lives.
                    pResource: unsafe { std::mem::transmute_copy(buffer) },
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: before,
                    StateAfter: after,
                }),
            },
        }
    }

    impl Memory {
        /// At a list's first use: the copies are recorded into it.
        pub(super) fn dump(&mut self, list: &HostList, _: &[u64]) {
            let Some(folder) = folder() else { return };
            if let Some((due, _)) = &self.dump.due {
                if self.frame < *due {
                    return;
                }
                let copies = self.dump.due.take().map(|d| d.1).unwrap_or_default();
                let mut text = String::new();
                for a in &self.dump.walked {
                    text += &format!("walked {a:#x}\n");
                }
                for (a, (first, last, n)) in &self.dump.built {
                    text += &format!("built {a:#x} {first} {last} {n}\n");
                }
                for (a, p) in &self.packed {
                    text += &format!("packed {a:#x} {} {}\n", p.len, p.frame);
                }
                text += &format!("frame {}\n", self.frame);
                for (page, held) in &self.pages {
                    let arena = self.arenas.iter().position(|a| a.0 == held.copy).unwrap_or(usize::MAX);
                    let at = held.at - self.arenas[arena].1;
                    text += &format!("{:#x} {arena} {at:#x}\n", page * PAGE);
                }
                let _ = std::fs::write(folder.join("pages.txt"), text);
                for (n, (_, data)) in copies.iter().enumerate() {
                    // SAFETY: a mapping of an arena's bytes; the copies ran
                    // frames ago.
                    let bytes = unsafe { std::slice::from_raw_parts(*data as *const u8, ARENA as usize) };
                    let _ = std::fs::write(folder.join(format!("arena-{n}.bin")), bytes);
                }
                tracing::info!(arenas = copies.len(), "title memory by address: the arenas are written out");
                return;
            }
            if self.dump.done || !folder.join("go").exists() {
                return;
            }
            self.dump.done = true;
            let Some(host) = host() else { return };
            let mut copies = Vec::new();
            for &(n, _) in &self.arenas {
                let buffer = &self.copies[n].buffer;
                let Some((to, data)) = host.shared_buffer(ARENA) else { break };
                // SAFETY: an open list; live buffers of those sizes. The list
                // has not used the arena yet.
                unsafe {
                    list.list.ResourceBarrier(&[turn(buffer, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                    list.list.CopyBufferRegion(&to, 0, buffer, 0, ARENA);
                    list.list.ResourceBarrier(&[turn(buffer, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON)]);
                }
                list.keep(&to);
                copies.push((to, data as usize));
            }
            self.dump.due = Some((self.frame + 12, copies));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call is quiet only when its list used every key the long way gives
    /// it: both constant forms (the second only when it builds), the view,
    /// written and page keys of root views above 4 GiB, the copies key for
    /// the present count and READ.
    #[test]
    fn a_call_is_quiet_only_with_every_key_used() {
        let (low, high, units) = (0x2234_0000u32, 0x3u32, 0x0200_0000u32);
        let constants = [low, high, units];
        let (view, other, small) = (0x5_0000_0100u64, 0x6_0000_0000u64, 0x8000u64);
        let mut seen = Vec::new();
        constant_addresses(&constants, true, |at| seen.push(at));
        assert_eq!(seen, vec![0x3_2234_0000, u64::from(units) << 8]);
        seen.clear();
        constant_addresses(&constants, false, |at| seen.push(at));
        assert_eq!(seen, vec![0x3_2234_0000]);
        let every = |builds: bool| {
            let mut keys = std::collections::HashSet::from([READ, copies_key(2), page_key(1, 0x3_2234_0000)]);
            if builds {
                keys.extend([page_key(1, u64::from(units) << 8), view_key(view), written_key(view), view_key(other)]);
            }
            keys.extend([page_key(2, view), page_key(2, other)]);
            keys
        };
        let (addresses, written) = ([view, other, small], [view]);
        for builds in [false, true] {
            let keys = every(builds);
            assert!(keys_used(|k| keys.contains(&k), &addresses, &written, &constants, builds, 2));
            // A new copy numbered since: the long way brings every copy up.
            assert!(!keys_used(|k| keys.contains(&k), &addresses, &written, &constants, builds, 3));
            // Any key missing sends the call the long way.
            for missing in &keys {
                assert!(!keys_used(|k| k != *missing && keys.contains(&k), &addresses, &written, &constants, builds, 2));
            }
        }
        // A reading call is not asked the builder's keys.
        let reads = every(false);
        assert!(!reads.contains(&view_key(view)));
    }

    /// What is noted for a submission is whole pages of the copy, cut to its
    /// span, and runs of one copy that meet are joined.
    #[test]
    fn landed_pages_are_whole_and_joined_by_copy() {
        let page = PAGE as usize;
        let first = 16 * page;
        assert_eq!(whole_pages(first + 10, first + 20, first, 4 * page), Some((first, first + page)));
        assert_eq!(whole_pages(first - 10, first + page + 1, first, 4 * page), Some((first, first + 2 * page)));
        assert_eq!(whole_pages(first + 3 * page + 1, first + 9 * page, first, 4 * page), Some((first + 3 * page, first + 4 * page)));
        assert_eq!(whole_pages(first + 8, first + 8, first, 4 * page), None);
        assert_eq!(whole_pages(first + 5 * page, first + 6 * page, first, 4 * page), None);
        let runs = vec![(2, 'b', 0, 10), (1, 'a', 20, 30), (1, 'a', 0, 10), (1, 'a', 10, 15), (2, 'b', 5, 40)];
        assert_eq!(joined(runs), vec![('a', 0, 15), ('a', 20, 30), ('b', 0, 40)]);
    }

    /// An arena is wanted ahead while fewer than twice the spare pages are
    /// free, and only with room for all of its pages and for its number.
    #[test]
    fn an_arena_is_wanted_ahead_only_with_room_for_it() {
        let arena = (ARENA / PAGE) as usize;
        assert!(want_ahead(SPARE, 0, 1));
        assert!(!want_ahead(2 * SPARE, 0, 1));
        assert!(want_ahead(0, MOST - arena, 1));
        assert!(!want_ahead(0, MOST - arena + 1, 1));
        assert!(!want_ahead(0, 0, COPIES as usize));
    }

    #[test]
    fn a_piece_takes_what_changed() {
        let mut title = [7u8; PIECE];
        let mut ours = vec![0u8; PIECE];
        assert_eq!(merge(&title, &mut ours, true), PIECE / 4);
        assert_eq!(ours, title);
        assert_eq!(merge(&title, &mut ours, false), 0);
        // A dword of it.
        title[41] = 8;
        assert_eq!(merge(&title, &mut ours, false), 1);
        assert_eq!(ours, title);
    }

    fn pages(of: &[u32]) -> std::collections::BTreeSet<u32> {
        of.iter().copied().collect()
    }

    #[test]
    fn pages_written_come_out_as_runs() {
        assert_eq!(runs_of(&pages(&[]), 4), vec![]);
        assert_eq!(runs_of(&pages(&[7]), 4), vec![(7, 8)]);
        // Pages that touch are one run; a gap of any size is two.
        assert_eq!(runs_of(&pages(&[3, 4, 5, 9, 1023]), 4), vec![(3, 6), (9, 10), (1023, 1024)]);
    }

    #[test]
    fn runs_past_the_most_are_joined_across_the_smallest_gaps() {
        // Gaps of 1, 5 and 90 pages: with two runs left, the first two join.
        let wide = pages(&[0, 2, 8, 99]);
        assert_eq!(runs_of(&wide, 4), vec![(0, 1), (2, 3), (8, 9), (99, 100)]);
        assert_eq!(runs_of(&wide, 3), vec![(0, 3), (8, 9), (99, 100)]);
        assert_eq!(runs_of(&wide, 2), vec![(0, 9), (99, 100)]);
        assert_eq!(runs_of(&wide, 1), vec![(0, 100)]);
        // A most of nothing is one run, not a loop that cannot end.
        assert_eq!(runs_of(&wide, 0), vec![(0, 100)]);
    }

    #[test]
    fn a_dirty_note_covers_every_page_a_write_touches() {
        let mut dirty = Dirty { on: true, ..Default::default() };
        // 16 bytes that straddle two pages, and a write of nothing.
        dirty.mark(3, PAGE - 8, 16);
        dirty.mark(3, 5 * PAGE, 0);
        // A whole page of another copy.
        dirty.mark(1, 2 * PAGE, PAGE);
        // The copies come in the order of their numbers; notes are taken once.
        assert_eq!(dirty.take(), vec![(1, vec![(2, 3)]), (3, vec![(0, 2)])]);
        assert!(dirty.take().is_empty());
        // A write after a take is noted again.
        dirty.mark(3, 0, 1);
        assert_eq!(dirty.take(), vec![(3, vec![(0, 1)])]);
    }

    #[test]
    fn nothing_is_noted_with_the_switch_off() {
        let mut dirty = Dirty::default();
        dirty.mark(0, 0, PAGE);
        assert!(dirty.take().is_empty());
        assert!(dirty.pages.is_empty());
    }

    #[test]
    fn the_directory_is_read_without_the_mutex() {
        // Entries of (number, offset); a closed page (read only) is placed too.
        let mut entries = vec![0u32; 8];
        entries[2] = 5;
        entries[4] = 2 | MEMORY_READ_ONLY;
        let at = entries.as_ptr() as usize;
        assert!(!placed_in(at, 0));
        assert!(placed_in(at, 1));
        assert!(placed_in(at, 2));
        assert!(!placed_in(at, 3));
        // No directory yet, and a page past it.
        assert!(!placed_in(0, 1));
        assert!(!placed_in(at, u64::from(MEMORY_PAGES)));
    }

    #[test]
    fn the_pages_a_call_names_are_those_of_its_views_and_constants() {
        let high = 3u64 << 32;
        let none = |_: u64| None;
        // Below 4 GiB is no title memory; a view reads a page, a builder's
        // view the guess, and a known length reaches as far as it says.
        assert_eq!(pages_named(&[0x1000], &[], false, none), Vec::<u64>::new());
        assert_eq!(pages_named(&[high], &[], false, none), vec![high / PAGE]);
        assert_eq!(pages_named(&[high], &[], true, none).len() as u64, VIEW / PAGE);
        let known = |at: u64| (at == high).then_some(3 * PAGE + 1);
        assert_eq!(pages_named(&[high], &[], false, known), (0..4).map(|i| high / PAGE + i).collect::<Vec<_>>());
        // An address in two constants, and one in 256-byte units (builder only).
        let constants = [0x0002_0000, 1, 0x0100_0000];
        assert_eq!(pages_named(&[], &constants, false, none), vec![(0x0002_0000 | 1 << 32) / PAGE]);
        assert_eq!(pages_named(&[], &constants, true, none), vec![(0x0002_0000 | 1 << 32) / PAGE, (0x0100_0000u64 << 8) / PAGE]);
        // A page named twice is asked once, and no call asks about more than PRE.
        assert_eq!(pages_named(&[high, high], &[], false, none).len(), 1);
        assert_eq!(pages_named(&[high], &[], false, |_| Some(1 << 40)).len(), PRE);
    }

    #[test]
    fn answers_made_before_the_mutex_are_found_and_then_dropped() {
        assert_eq!(asked_before(0x1_0007), None);
        ASKED.with(|asked| *asked.borrow_mut() = vec![(0x1_0007, None), (0x1_0008, Some(WHY_UNCOMMITTED))]);
        let answers = Answers;
        assert_eq!(asked_before(0x1_0007), Some(None));
        assert_eq!(asked_before(0x1_0008), Some(Some(WHY_UNCOMMITTED)));
        assert_eq!(asked_before(0x1_0009), None);
        drop(answers);
        assert_eq!(asked_before(0x1_0007), None);
    }

    #[test]
    fn a_packed_structure_is_known_by_its_header() {
        let header = |version: u32, nodes: u32| u64::from(version) | u64::from(nodes) << 32;
        assert_eq!(packed_len(header(0x8102_0006, 0x9a1a)), Some(0x9a1b * 64));
        // Unpacked, a top level, another version, no size, too big.
        assert_eq!(packed_len(header(0x8002_0006, 0x9a1a)), None);
        assert_eq!(packed_len(header(0x0000_0006, 5)), None);
        assert_eq!(packed_len(header(0x8102_0007, 5)), None);
        assert_eq!(packed_len(header(0x8102_0006, 0)), None);
        assert_eq!(packed_len(header(0x8102_0006, 1 << 21)), None);
    }
}
