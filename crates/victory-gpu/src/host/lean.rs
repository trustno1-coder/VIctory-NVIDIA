//! The submitting thread's work in a submission, done less: two switches, both
//! off by default, and the counters that say where the thread's steps go.
//!
//! One thread prepares every submission (`queue::behind`), so what it does
//! between a list closing and the lists going to the card is the frame. Run 9
//! (foxhound-20261126-232504, ray tracing) split its 55-80 ms a frame like
//! this: asking the write watch 16 (`watch.rs`, not here), the lists' steps
//! 19-20 of which refreshes 10-11 in 38-40 flushes of 7,400 spans (6,300 of them
//! known to copy nothing), copies in the card's memory 5-6, planning steps for
//! the card 4-5, and the rest waiting for flights. This file is for the part
//! of the lists' steps that `host.rs` does itself.
//!
//! - [`spread`] (`VICTORY_REFRESH_PROBE_US`): a small flush is tried here first.
//! - [`Declared`] (`VICTORY_DECLARE_ACROSS_LISTS=1`): a range shaders write is
//!   declared once to a copy per submission, not once per list.
//! - [`ours`] and [`declared`] only count, and only while a switch of this file
//!   is on ([`counting`], [`declare_across`]): with all unset the steps are not
//!   timed and nothing is counted.
//!
//! The file also holds the counters of the walks the thread makes over a
//! submission's steps ([`walking`], [`polled`], [`flushed`], [`census`],
//! [`card_part`]), which are always on and are printed by [`walks_line`], and
//! the two switches of step 1 ([`title_write_range`], [`title_write_check`]):
//! a refresh done ahead of the steps stays valid across a title write of ours
//! that touches none of its pages.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// How long a small flush of refreshes is worked on this thread alone before
/// the rest goes to the kept threads (`VICTORY_REFRESH_PROBE_US=<microseconds>`,
/// 1 to 5000; unset, 0 or anything else: every flush of four or more goes to the
/// threads at once, as before).
///
/// Handing work to the kept threads costs the submitting thread a wake for each
/// (a condition variable and the lock behind it) and a wait at the end until
/// the last of them is done, whether or not they had anything to do. A frame
/// has 38-40 flushes (run 9), most of them a few dozen spans after the ones
/// known to copy nothing are left out, so a flush takes 0.27 ms of the thread
/// on average. Earlier notes found the refreshes' rate the same (2-3 GB/s over
/// all threads) with and without ray tracing, so the time per flush is not
/// memory speed; how much of it is the wake and the wait is a guess until a
/// run with the switch shows the line's "done on this thread alone". With the
/// switch a flush goes through its refreshes one after the other here until the
/// probe has run out and only then hands the rest over, so a flush that is
/// small is done without any wake and a flush that is not loses at most the
/// probe and the one refresh it was in.
///
/// Where the bytes are does not change: title memory is read into the copy by
/// the same refresh, once; only which thread calls it does. Flushes with
/// textures due (long, hashed whole) and the flush of the heap's thousands of
/// copies go to the threads at once, as before.
fn probe() -> Option<Duration> {
    static PROBE: OnceLock<Option<Duration>> = OnceLock::new();
    *PROBE.get_or_init(|| probe_of(std::env::var("VICTORY_REFRESH_PROBE_US").ok().as_deref()))
}

/// [`probe`] for a setting.
fn probe_of(setting: Option<&str>) -> Option<Duration> {
    let us: u64 = setting?.trim().parse().ok()?;
    // A probe past a few milliseconds is a flush done here whatever it is.
    (us > 0).then(|| Duration::from_micros(us.min(5_000)))
}

/// Since the last frame-rate line: flushes that were done on this thread
/// alone, those that went on to the threads after the probe, the nanoseconds
/// spent here before either, and the refreshes done here.
static PROBED: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Bytes of title memory a due texture may read and still count as light for
/// [`textures_small`].
const SMALL_TEXTURE: usize = 256 << 10;

/// Whether a flush with only light textures due still counts as small for
/// [`spread`] (`VICTORY_REFRESH_PROBE_TEXTURES=1`; off by default: a flush with
/// any texture due goes to the threads at once, as before). Only meaningful
/// with [`probe`] set, so it is off without it.
///
/// 30 of the 36.5 flushes a frame in run12-live went straight to the kept
/// threads, most because a texture was due or the flush was over 256 spans. A
/// wake and a barrier for work of a few dozen microseconds is what the probe
/// was written to avoid, and a texture that reads under 256 KiB, or was read
/// ahead already, costs about as little as a few copies. The probe still bounds
/// the time on this thread (its microseconds, plus the one refresh in progress,
/// which for these textures is small). A texture refresh that blocks on another
/// thread's hold of the same texture can run past the probe, since the probe is
/// only looked at between refreshes; the line's "waiting for a texture another
/// thread is refreshing" says whether that happens.
fn probe_textures() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| probe().is_some() && textures_of(std::env::var("VICTORY_REFRESH_PROBE_TEXTURES").ok().as_deref()))
}

/// [`probe_textures`] for a setting: only "1".
fn textures_of(setting: Option<&str>) -> bool {
    setting == Some("1")
}

/// Since the last frame-rate line: flushes with textures due that were taken
/// as small only because the textures were light.
static TEXTURES_PROBED: AtomicU64 = AtomicU64::new(0);

/// Whether every texture in `due` is light enough ([`SMALL_TEXTURE`]) for the
/// flush to be tried on this thread first. False with the switch off, so the
/// caller's own test stands as it was. Counts a yes for a non-empty `due` when
/// with the `spans` the flush has two or more pieces: with fewer it never went
/// to the threads, so the textures did not change where it ran.
pub(super) fn textures_small(due: &[std::sync::Arc<crate::texture::Staging>], spans: usize) -> bool {
    if !probe_textures() || !due.iter().all(|t| t.cheap(SMALL_TEXTURE)) {
        return false;
    }
    if !due.is_empty() && spans + due.len() >= 2 {
        TEXTURES_PROBED.fetch_add(1, Ordering::Relaxed);
    }
    true
}

/// Since the last frame-rate line (`VICTORY_REFRESH_SPLIT=1`): flushes with
/// nothing left to do after the checks, flushes handed straight to the threads
/// (one or more to do, or not small), the nanoseconds in those calls, and the
/// refreshes in them.
static SPLIT: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// What [`spread_with`] did.
#[derive(Debug, PartialEq, Eq)]
struct Spread {
    /// How many of the `n` were done on this thread before the rest left it.
    here: usize,
    /// Whether the rest went to the kept threads.
    went: bool,
}

/// Calls `work` with every number below `n`, on this thread while the clock
/// says less than `probe` has passed since the first, and with what is left on
/// the kept threads and this one (`crate::shadow::wide_each`).
///
/// With no `probe`, or `small` false, or fewer than two to do: all of it goes
/// to `wide_each` as it did.
fn spread_with(n: usize, small: bool, probe: Option<Duration>, work: impl Fn(usize) + Send + Sync + 'static) -> Spread {
    let Some(probe) = probe.filter(|_| small && n >= 2) else {
        crate::shadow::wide_each(n, work);
        return Spread { here: 0, went: true };
    };
    let started = std::time::Instant::now();
    let here = serial_first(n, probe, || started.elapsed(), &work);
    if here >= n {
        return Spread { here, went: false };
    }
    // What the thread did not get to: numbers from `here` on.
    crate::shadow::wide_each(n - here, move |i| work(here + i));
    Spread { here, went: true }
}

/// Calls `work` with 0, 1, ... while `elapsed` says the probe has not run out;
/// at least once. How many it called, `n` if it called all.
fn serial_first(n: usize, probe: Duration, mut elapsed: impl FnMut() -> Duration, work: &impl Fn(usize)) -> usize {
    let mut i = 0;
    while i < n {
        work(i);
        i += 1;
        if i < n && elapsed() >= probe {
            break;
        }
    }
    i
}

/// [`spread_with`] under [`probe`], counted. `small`: the caller's flush is one
/// the probe is for (few spans, and no textures due, or with
/// `VICTORY_REFRESH_PROBE_TEXTURES=1` only light ones: [`textures_small`]).
pub(super) fn spread(n: usize, small: bool, work: impl Fn(usize) + Send + Sync + 'static) {
    let Some(probe) = probe().filter(|_| small && n >= 2) else {
        // (with the split switch off no clock is read and no counter touched.)
        if !crate::shadow::refresh_split() {
            crate::shadow::wide_each(n, work);
            return;
        }
        if n == 0 {
            SPLIT[0].fetch_add(1, Ordering::Relaxed);
            crate::shadow::wide_each(n, work);
            return;
        }
        let started = std::time::Instant::now();
        crate::shadow::wide_each(n, work);
        SPLIT[1].fetch_add(1, Ordering::Relaxed);
        SPLIT[2].fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        SPLIT[3].fetch_add(n as u64, Ordering::Relaxed);
        return;
    };
    let started = std::time::Instant::now();
    let done = spread_with(n, small, Some(probe), work);
    // (The time is the whole call: the part on this thread is what the probe
    // bounds, and when the rest went out the call is longer. The line says so.)
    let k = usize::from(done.went);
    PROBED[k].fetch_add(1, Ordering::Relaxed);
    PROBED[2].fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    PROBED[3].fetch_add(done.here as u64, Ordering::Relaxed);
}

/// Whether a range shaders write is declared to a host copy once for a whole
/// submission (`VICTORY_DECLARE_ACROSS_LISTS=1`; off by default: once for each
/// list, as before).
///
/// A list declares a range once until it next writes title memory
/// (`HostList::defer_gpu_write`), but a submission has ten to sixteen lists and
/// each declares the ranges it reaches again: the world's lists name the
/// views of the title's one big heap dozens of times over, and
/// [`super::declared_writes`] already counts each once for the copy-back. At
/// the submission each repeat is a call of `Shadow::writing`, which takes the
/// copy's lock, looks through the ranges already declared (a list, one by one)
/// and, with the relay, takes a second lock to count the range for this
/// submission; for a range declared already it then changes nothing. The
/// repeat is left out here, behind the copy's pending refresh, which is still
/// done where it was (`Batch::flush_one`): a submission's later refreshes of a
/// declared range are made around it either way.
///
/// What ends the counting is what ends it for a list: a step that writes
/// title memory ([`super::Batch::title_written`]). Not measured in a run: the
/// line says how many of the declarations were repeats.
pub(super) fn declare_across() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DECLARE_ACROSS_LISTS").is_some_and(|v| v == "1"))
}

/// The ranges (copy by address, address, bytes) declared to host copies since a
/// submission last wrote title memory.
#[derive(Default)]
pub(super) struct Declared(std::collections::HashSet<(usize, usize, usize), std::hash::BuildHasherDefault<super::AddressHash>>);

impl Declared {
    /// Notes the range; whether it is the first time since the last [`Declared::clear`].
    pub(super) fn first(&mut self, copy: usize, address: usize, len: usize) -> bool {
        self.0.insert((copy, address, len))
    }

    /// Title memory was written: what was declared before has to be again.
    pub(super) fn clear(&mut self) {
        self.0.clear();
    }
}

/// Since the last frame-rate line: ranges shaders write, as the submissions'
/// lists declared them, and those of them that were repeats left out.
static DECLARED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Counts a declaration (`repeat`: left out). Called only with [`declare_across`]
/// on, so with the switch unset a declaration costs no counting at all.
pub(super) fn declared(repeat: bool) {
    DECLARED[0].fetch_add(1, Ordering::Relaxed);
    if repeat {
        DECLARED[1].fetch_add(1, Ordering::Relaxed);
    }
}

/// Since the last frame-rate line: steps of ours the lists run on this thread
/// (`Step::Run` and `Step::Host`: what the memory's landing, the heaps' records
/// and the like are), and their nanoseconds.
static OURS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Whether the steps of ours are timed: only while one of this file's switches is
/// on, so that with all of them unset the submitting thread reads no clock and
/// writes no counter for them. One load of a cell read once.
pub(super) fn counting() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| declare_across() || probe().is_some())
}

/// Counts a step of ours that took `took` (the caller timed it under [`counting`]).
pub(super) fn ours(took: Duration) {
    OURS[0].fetch_add(1, Ordering::Relaxed);
    OURS[1].fetch_add(took.as_nanos() as u64, Ordering::Relaxed);
}

/// The counters, per frame, for the frame-rate line (called from
/// `submit_split_line`): what the lists' steps that are not refreshes, copies
/// or fills come to, and what the two switches did.
pub(super) fn line(frames: f64) {
    let frames = frames.max(1.0);
    let o: [f64; 2] = std::array::from_fn(|i| OURS[i].swap(0, Ordering::Relaxed) as f64 / frames);
    let d: [f64; 2] = std::array::from_fn(|i| DECLARED[i].swap(0, Ordering::Relaxed) as f64 / frames);
    let p: [f64; 4] = std::array::from_fn(|i| PROBED[i].swap(0, Ordering::Relaxed) as f64 / frames);
    let sp: [f64; 4] = std::array::from_fn(|i| SPLIT[i].swap(0, Ordering::Relaxed) as f64 / frames);
    let textures = TEXTURES_PROBED.swap(0, Ordering::Relaxed) as f64 / frames;
    if o[0] + d[0] + p[0] + p[1] + sp[0] + sp[1] <= 0.0 {
        return;
    }
    let across = if declare_across() {
        format!("{:.0} of them repeats within a submission left out (VICTORY_DECLARE_ACROSS_LISTS=1)", d[1])
    } else {
        "VICTORY_DECLARE_ACROSS_LISTS is off".to_string()
    };
    let mut probed = match probe() {
        Some(probe) => format!(
            "{:.1} small flushes done on this thread alone and {:.1} that went on to the threads, {:.2} ms in all these calls, \
             {:.0} refreshes made here (VICTORY_REFRESH_PROBE_US={})",
            p[0],
            p[1],
            p[2] / 1e6,
            p[3],
            probe.as_micros()
        ),
        None => "VICTORY_REFRESH_PROBE_US is off".to_string(),
    };
    if probe_textures() {
        probed.push_str(&format!(
            ", {textures:.1} flushes taken as small only because their due textures were light (VICTORY_REFRESH_PROBE_TEXTURES=1)"
        ));
    }
    tracing::info!(
        "the lists' other steps, per frame, the submitting thread: {:.1} steps of ours in {:.1} ms; {:.0} ranges shaders write \
         declared to host copies, {across}; {probed}",
        o[0],
        o[1] / 1e6,
        d[0]
    );
    if crate::shadow::refresh_split() {
        tracing::info!(
            "refresh flushes, per frame (VICTORY_REFRESH_SPLIT=1): {:.1} had nothing left after the checks, {:.1} went straight to the threads ({:.2} ms in the calls, {:.0} refreshes among them)",
            sp[0],
            sp[1],
            sp[2] / 1e6,
            sp[3]
        );
    }
}

/// Whether a refresh done ahead of a submission's steps stays valid across a
/// title write of ours that touches none of its pages (`VICTORY_TITLE_WRITE_RANGE=1`,
/// or `VICTORY_TITLE_WRITE_RANGE_CHECK=1`, which turns it on too; off by default:
/// a title write clears every refresh done ahead, as before). Read once.
///
/// The refreshes ahead copy title memory into our copies before the steps run.
/// A title write of ours (`Batch::title_written` in host.rs) changes only the
/// pages it names, so a refresh whose pages it does not overlap has nothing to
/// redo; clearing them all makes the steps after the write read the same bytes a
/// second time. With the switch the write names its pages first and only the
/// refreshes overlapping them are dropped.
pub(super) fn title_write_range() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_TITLE_WRITE_RANGE") || title_write_check())
}

/// The check of [`title_write_range`] (`VICTORY_TITLE_WRITE_RANGE_CHECK=1`): a
/// refresh that stood across a title write is tested against the write's pages,
/// and a standing one the write turns out to have touched is counted by
/// [`retained_violation`]; the caller then falls back to the old clear for the
/// rest of the submission. Read once.
pub(super) fn title_write_check() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_TITLE_WRITE_RANGE_CHECK"))
}

/// The walks the submitting thread makes over a submission's steps, each with
/// its own counter in [`WALKED`].
#[derive(Clone, Copy)]
pub(super) enum Walk {
    /// Collecting the spans a copy has to be given.
    CopySpans,
    /// Collecting the refreshes due in front of the steps.
    RefreshFront,
    /// Refreshing ahead of the steps (`refresh_ahead`).
    RefreshAhead,
    /// Running the deferred steps, which includes the steps' own work.
    RunDeferred,
    /// Listing what a step does to the card.
    DoesOf,
    /// Finding what a step is blocked by.
    BlockedBy,
    /// Finding copies that are stale under flights in the air.
    StaleUnderFlights,
    /// Reading what a step copied.
    CopiedRead,
    /// Deciding which steps can be joined.
    Joinable,
}

/// How many [`Walk`]s there are.
const WALKS: usize = 9;

/// Since the last frame-rate line: for each [`Walk`], the walks made and their
/// nanoseconds.
static WALKED: [[AtomicU64; 2]; WALKS] = [const { [const { AtomicU64::new(0) }; 2] }; WALKS];

/// A walk in progress: dropping it counts the walk and the time it took.
pub(super) struct Walking {
    /// Which walk this is.
    walk: Walk,
    /// When it began.
    began: std::time::Instant,
}

/// Begins timing `walk`; the walk ends when the guard is dropped. Two clock
/// reads a walk is the whole cost, and a frame makes some hundreds of walks, so
/// the counters stay on without a switch. A step is not timed on its own: the
/// time of one walk covers all of its steps.
pub(super) fn walking(walk: Walk) -> Walking {
    Walking { walk, began: std::time::Instant::now() }
}

impl Drop for Walking {
    fn drop(&mut self) {
        let cell = &WALKED[self.walk as usize];
        cell[0].fetch_add(1, Ordering::Relaxed);
        cell[1].fetch_add(self.began.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
}

/// Who asked the shadow whether nothing is new in a range (`Shadow::nothing_new`).
#[derive(Clone, Copy)]
pub(super) enum Poll {
    /// The flush in front of the steps.
    Front,
    /// A flush at a title write or at the end of the steps.
    Barrier,
    /// The flush of one copy before shaders are given it.
    One,
    /// `refresh_ahead`.
    Ahead,
    /// The memory module's landed copies, after the steps.
    Landed,
}

/// How many [`Poll`] callers there are.
const POLLS: usize = 5;

/// Since the last frame-rate line: for each [`Poll`], the calls and, only with
/// `VICTORY_REFRESH_SPLIT=1`, their nanoseconds.
static POLLED: [[AtomicU64; 2]; POLLS] = [const { [const { AtomicU64::new(0) }; 2] }; POLLS];

/// `shadow.nothing_new(address, len)`, counted under `by`. The call is always
/// counted (one atomic add); the nanoseconds only with the split switch on, so
/// that without it no clock is read here.
pub(super) fn polled(by: Poll, shadow: &crate::shadow::Shadow, address: usize, len: usize) -> bool {
    let cell = &POLLED[by as usize];
    cell[0].fetch_add(1, Ordering::Relaxed);
    if !crate::shadow::refresh_split() {
        return shadow.nothing_new(address, len);
    }
    let began = std::time::Instant::now();
    let nothing = shadow.nothing_new(address, len);
    cell[1].fetch_add(began.elapsed().as_nanos() as u64, Ordering::Relaxed);
    nothing
}

/// What made a flush of refreshes happen.
#[derive(Clone, Copy)]
pub(super) enum Cause {
    /// The flush in front of the steps.
    Front,
    /// The flush of one copy.
    Copy,
    /// A flush for a placed range.
    Placed,
    /// A flush before a run step.
    Run,
    /// The flush at the end of the steps.
    End,
}

/// How many [`Cause`]s there are.
const CAUSES: usize = 5;

/// Since the last frame-rate line: for each [`Cause`], the flushes and the
/// copies in them.
static FLUSHED: [[AtomicU64; 2]; CAUSES] = [const { [const { AtomicU64::new(0) }; 2] }; CAUSES];

/// Counts one flush with `copies` copies in it.
pub(super) fn flushed(by: Cause, copies: usize) {
    let cell = &FLUSHED[by as usize];
    cell[0].fetch_add(1, Ordering::Relaxed);
    cell[1].fetch_add(copies as u64, Ordering::Relaxed);
}

/// The kinds of step a walk over a submission meets.
#[derive(Clone, Copy)]
pub(super) enum Kind {
    Copy,
    Fill,
    Refresh,
    RefreshSet,
    Run,
    Host,
    GpuWrite,
    Texture,
    Plane,
    Placed,
}

/// How many [`Kind`]s there are.
pub(super) const KINDS: usize = 10;

/// What the line calls each [`Kind`], in the enum's order.
const KIND_NAMES: [&str; KINDS] =
    ["copies", "fills", "refreshes", "refresh sets", "runs", "host steps", "gpu writes", "textures", "planes", "placed"];

/// Since the last frame-rate line: the steps met, by [`Kind`].
static CENSUS: [AtomicU64; KINDS] = [const { AtomicU64::new(0) }; KINDS];

/// Adds a count of steps per kind. The caller sums over one call to
/// `run_deferred` and adds once, so the atomics are touched ten times a call.
pub(super) fn census(kinds: &[u64; KINDS]) {
    for (cell, n) in CENSUS.iter().zip(kinds) {
        if *n != 0 {
            cell.fetch_add(*n, Ordering::Relaxed);
        }
    }
}

/// The parts of the sync list that goes ahead of a submission (`card_before`).
#[derive(Clone, Copy)]
pub(super) enum CardPart {
    /// Finding what changed on the CPU side.
    Dirty,
    /// Taking it out of the CPU sides.
    Take,
    /// Recording the sync list.
    RecordSync,
    /// Executing it.
    ExecuteSync,
    /// Recording the compare.
    RecordCompare,
}

/// How many [`CardPart`]s there are.
const CARD_PARTS: usize = 5;

/// Since the last frame-rate line: for each [`CardPart`], the calls and their
/// nanoseconds.
static CARD: [[AtomicU64; 2]; CARD_PARTS] = [const { [const { AtomicU64::new(0) }; 2] }; CARD_PARTS];

/// Counts a part of `card_before` that began at `began` and has just ended.
pub(super) fn card_part(part: CardPart, began: std::time::Instant) {
    let cell = &CARD[part as usize];
    cell[0].fetch_add(1, Ordering::Relaxed);
    cell[1].fetch_add(began.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

/// Since the last frame-rate line (step 1): refreshes done ahead that a title
/// write left standing, those it dropped, and the violations the check found.
static RETAINED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Counts the refreshes done ahead that a title write left standing (`kept`)
/// and those it dropped (`dropped`).
pub(super) fn retained(kept: usize, dropped: usize) {
    RETAINED[0].fetch_add(kept as u64, Ordering::Relaxed);
    RETAINED[1].fetch_add(dropped as u64, Ordering::Relaxed);
}

/// Counts a refresh that stood across a title write and that the write then
/// showed to be affected (the check of step 1).
pub(super) fn retained_violation() {
    RETAINED[2].fetch_add(1, Ordering::Relaxed);
}

/// Swaps a table of calls and nanoseconds to zero, per frame: calls and ms.
fn take_pairs<const N: usize>(table: &[[AtomicU64; 2]; N], frames: f64) -> [[f64; 2]; N] {
    std::array::from_fn(|i| std::array::from_fn(|j| table[i][j].swap(0, Ordering::Relaxed) as f64 / frames))
}

/// The counters of this file other than [`line`]'s, per frame, for the
/// frame-rate line: one line for each family with something in it. The counters
/// are taken (swapped to zero) whether or not they are printed.
pub(super) fn walks_line(frames: f64) {
    let frames = frames.max(1.0);
    let walked = take_pairs(&WALKED, frames);
    let polls = take_pairs(&POLLED, frames);
    let flushes = take_pairs(&FLUSHED, frames);
    let card = take_pairs(&CARD, frames);
    let kinds: [f64; KINDS] = std::array::from_fn(|i| CENSUS[i].swap(0, Ordering::Relaxed) as f64 / frames);
    let kept: [f64; 3] = std::array::from_fn(|i| RETAINED[i].swap(0, Ordering::Relaxed) as f64 / frames);

    if walked.iter().any(|w| w[0] > 0.0) {
        let names = [
            "copy spans",
            "front refreshes",
            "refreshes ahead",
            "run_deferred",
            "does of",
            "blocked by",
            "stale under flights",
            "copied read",
            "joinable",
        ];
        let parts: Vec<String> =
            names.iter().zip(&walked).map(|(name, w)| format!("{name} {:.1} calls {:.2} ms", w[0], w[1] / 1e6)).collect();
        tracing::info!(
            "the walks over a submission's steps, per frame, the submitting thread (run_deferred includes the steps' own work): {}",
            parts.join("; ")
        );
    }
    if kinds.iter().any(|k| *k > 0.0) {
        let parts: Vec<String> = KIND_NAMES.iter().zip(&kinds).map(|(name, n)| format!("{n:.0} {name}")).collect();
        tracing::info!("steps run by run_deferred, per frame, by kind: {}", parts.join(", "));
    }
    if polls.iter().any(|p| p[0] > 0.0) {
        let names = ["in front of the steps", "at a barrier", "of one copy", "ahead", "of landed copies"];
        let parts: Vec<String> = names.iter().zip(&polls).map(|(name, p)| format!("{name} {:.0} calls", p[0])).collect();
        let mut text = parts.join(", ");
        if crate::shadow::refresh_split() {
            let ms: Vec<String> = names.iter().zip(&polls).map(|(name, p)| format!("{name} {:.2} ms", p[1] / 1e6)).collect();
            text.push_str(&format!("; time in them: {}", ms.join(", ")));
        } else {
            text.push_str("; the milliseconds exist only with VICTORY_REFRESH_SPLIT=1");
        }
        tracing::info!("nothing-new polls, per frame, by caller: {text}");
    }
    if flushes.iter().any(|f| f[0] > 0.0) {
        let names = ["in front of the steps", "of one copy", "of placed ranges", "before runs", "at the end"];
        let parts: Vec<String> =
            names.iter().zip(&flushes).map(|(name, f)| format!("{name} {:.1} flushes with {:.0} copies", f[0], f[1])).collect();
        tracing::info!("refresh flushes, per frame, by cause: {}", parts.join("; "));
    }
    if card.iter().any(|c| c[0] > 0.0) {
        let names = ["what changed on the CPU side", "the take out of the CPU sides", "recording the sync list", "executing it", "recording the compare"];
        let parts: Vec<String> = names.iter().zip(&card).map(|(name, c)| format!("{name} {:.1} calls {:.2} ms", c[0], c[1] / 1e6)).collect();
        tracing::info!("the sync list ahead of a submission (card_before), per frame: {}", parts.join("; "));
    }
    if title_write_range() {
        tracing::info!(
            "refreshes done ahead, per frame, at title writes (VICTORY_TITLE_WRITE_RANGE=1): {:.1} left standing, {:.1} dropped, {:.1} violations found by the check",
            kept[0],
            kept[1],
            kept[2]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    /// The counters of the walks are shared by every test in the process, and
    /// `walks_line` swaps them to zero, so the tests that read them take turns.
    static COUNTERS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Takes the turn, going on with what a panicked test left.
    fn counters() -> std::sync::MutexGuard<'static, ()> {
        COUNTERS.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A table of how many times each number was called.
    fn tally(n: usize) -> Arc<Vec<AtomicUsize>> {
        Arc::new((0..n).map(|_| AtomicUsize::new(0)).collect())
    }

    #[test]
    fn the_texture_switch_is_one_and_nothing_else() {
        assert!(textures_of(Some("1")));
        assert!(!textures_of(None));
        assert!(!textures_of(Some("")));
        assert!(!textures_of(Some("0")));
        assert!(!textures_of(Some("true")));
    }

    #[test]
    fn the_probe_is_a_number_of_microseconds_or_nothing() {
        assert_eq!(probe_of(None), None);
        assert_eq!(probe_of(Some("")), None);
        assert_eq!(probe_of(Some("0")), None);
        assert_eq!(probe_of(Some("fast")), None);
        assert_eq!(probe_of(Some("-5")), None);
        assert_eq!(probe_of(Some(" 150 ")), Some(Duration::from_micros(150)));
        // A probe is bounded.
        assert_eq!(probe_of(Some("90000000")), Some(Duration::from_micros(5_000)));
    }

    #[test]
    fn the_thread_works_until_the_probe_has_run_out_and_at_least_once() {
        let called = tally(10);
        let work = {
            let called = called.clone();
            move |i: usize| {
                called[i].fetch_add(1, Ordering::Relaxed);
            }
        };
        // A clock that has run out at once: the first is still done.
        let here = serial_first(10, Duration::from_micros(1), || Duration::from_secs(1), &work);
        assert_eq!(here, 1);
        // A clock that never does.
        let here = serial_first(10, Duration::from_secs(1), || Duration::ZERO, &work);
        assert_eq!(here, 10);
        // One that runs out after three looks.
        let mut looks = 0;
        let here = serial_first(
            10,
            Duration::from_micros(30),
            || {
                looks += 1;
                Duration::from_micros(10 * looks)
            },
            &work,
        );
        assert_eq!(here, 3);
        assert_eq!(called[0].load(Ordering::Relaxed), 3);
        assert_eq!(called[3].load(Ordering::Relaxed), 1);
        assert_eq!(called[9].load(Ordering::Relaxed), 1);
    }

    #[test]
    fn every_number_is_called_once_whoever_does_it() {
        for (n, small, probe) in [
            (0, true, Some(Duration::from_secs(1))),
            (1, true, Some(Duration::from_secs(1))),
            (2, true, None),
            (37, false, Some(Duration::from_secs(1))),
            // All here: the probe is long.
            (37, true, Some(Duration::from_secs(60))),
            // Next to none here: the probe has run out after the first.
            (37, true, Some(Duration::from_nanos(1))),
            (500, true, Some(Duration::from_nanos(1))),
        ] {
            let called = tally(n);
            let work = {
                let called = called.clone();
                move |i: usize| {
                    called[i].fetch_add(1, Ordering::Relaxed);
                }
            };
            let done = spread_with(n, small, probe, work);
            assert!(called.iter().all(|c| c.load(Ordering::Relaxed) == 1), "n {n}, small {small}, probe {probe:?}");
            // Where it went.
            match (n, small, probe) {
                (37, true, Some(p)) if p >= Duration::from_secs(60) => assert_eq!(done, Spread { here: 37, went: false }),
                (37 | 500, true, Some(_)) => assert!(done.went && done.here >= 1),
                _ => assert!(done.went && done.here == 0),
            }
        }
    }

    #[test]
    fn a_range_is_declared_again_after_title_memory_is_written() {
        let mut seen = Declared::default();
        assert!(seen.first(0x1000, 0, 64));
        assert!(!seen.first(0x1000, 0, 64));
        // Another copy, another place, another length: not the same range.
        assert!(seen.first(0x2000, 0, 64));
        assert!(seen.first(0x1000, 64, 64));
        assert!(seen.first(0x1000, 0, 128));
        seen.clear();
        assert!(seen.first(0x1000, 0, 64));
    }

    #[test]
    fn a_walk_is_counted_when_its_guard_is_dropped() {
        let _turn = counters();
        let before = WALKED[Walk::Joinable as usize][0].load(Ordering::Relaxed);
        drop(walking(Walk::Joinable));
        assert!(WALKED[Walk::Joinable as usize][0].load(Ordering::Relaxed) >= before + 1);
    }

    #[test]
    fn the_census_adds_each_kind() {
        let _turn = counters();
        let before = CENSUS[Kind::Plane as usize].load(Ordering::Relaxed);
        let mut kinds = [0u64; KINDS];
        kinds[Kind::Plane as usize] = 3;
        census(&kinds);
        assert!(CENSUS[Kind::Plane as usize].load(Ordering::Relaxed) >= before + 3);
    }

    #[test]
    fn flushes_and_card_parts_add() {
        let _turn = counters();
        let flushes = FLUSHED[Cause::End as usize][0].load(Ordering::Relaxed);
        let copies = FLUSHED[Cause::End as usize][1].load(Ordering::Relaxed);
        flushed(Cause::End, 7);
        assert!(FLUSHED[Cause::End as usize][0].load(Ordering::Relaxed) >= flushes + 1);
        assert!(FLUSHED[Cause::End as usize][1].load(Ordering::Relaxed) >= copies + 7);
        let calls = CARD[CardPart::Take as usize][0].load(Ordering::Relaxed);
        card_part(CardPart::Take, std::time::Instant::now());
        assert!(CARD[CardPart::Take as usize][0].load(Ordering::Relaxed) >= calls + 1);
    }

    #[test]
    fn the_line_runs_on_zeroed_and_filled_counters() {
        let _turn = counters();
        walks_line(1.0);
        drop(walking(Walk::DoesOf));
        flushed(Cause::Front, 2);
        retained(3, 1);
        retained_violation();
        // Frames below one are clamped, as `line` does.
        walks_line(0.0);
    }
}
