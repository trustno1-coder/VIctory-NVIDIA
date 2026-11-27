//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and
//! the title's first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! A first use queues the pipeline and waits for it within a budget per
//! frame, so the cheap ones (menus, one-off passes such as a colour-grading
//! bake) are there at once as before; past the budget a draw or dispatch
//! whose pipeline is not ready is dropped, as an untranslated one is.
//!
//! The default budget is half a second per title frame. A pass the title
//! runs only once and whose pipeline takes longer is lost. Passes over the
//! screen (no vertex input, no depth) have workers and a longer budget of
//! their own ([`screen_budget`]): the title fills its lighting tables with
//! such draws once, at the world's first frame, and behind a world's worth of
//! pipelines they were dropped; that run had no lamps and no diffuse light to
//! its end (run textures-d2, the first after a new translation; Belobug
//! 18e57db8 found the same).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    queue_state: AtomicU8,
    /// Taken by a worker from [`Queue::slow`].
    was_slow: AtomicBool,
    made: OnceLock<Option<HostPipeline>>,
    /// What identifies it across runs (shader containers and state), and
    /// whether a draw or dispatch has requested it yet.
    key: String,
    used: AtomicBool,
    /// A pass over the screen ([`GraphicsState::screen_pass`]) or a confirmed
    /// opening-scene / bake producer that must never sit behind cold RT shaders.
    screen: bool,
    /// Confirmed first-scene / bake producer prepared early even on a cold cache.
    scene_input: bool,
    /// Why this pipeline is intentionally skipped (e.g. RT resolve without BVH).
    left_out: Option<&'static str>,
    /// When a draw or dispatch first asked for it ([`stamp`]; 0: none has),
    /// and when it was put on [`Queue::requested`] (0: it was not). Both for
    /// [`skip_wait`].
    asked_ms: AtomicU64,
    queued_ms: AtomicU64,
}

/// Whether a compute shader of the pipeline carries `name`, a pipeline from
/// pieces by the book's record of its program (`Shader::has_name`).
fn named(shaders: &[Arc<Shader>], name: &[u8]) -> bool {
    shaders.iter().any(|s| s.stage == "compute" && s.has_name(name))
}

/// Shader hashes whose pipelines are left out in every build
/// (`VICTORY_LEAVE_OUT=<16 hex digits>[,..]`; `VICTORY_SKIP_PIPELINES` does
/// the same in diagnostics builds only). The decal heap's
/// `CS_DefragHeap_ComputeRelocations` (02384a60fda6f2d0), translatable for
/// 32-lane hosts through host waves that meet by spinning, took 11.85 ms of
/// the card a frame in the first gameplay area (timing run, 2026-11-26).
/// The hashes are those of the title's own containers, and a pipeline made
/// from pieces (`VICTORY_PIPELINE_PIECES=1`) is left out when the title's
/// container of the same program is named (`Shader::is`).
fn leave_out() -> &'static [String] {
    static HASHES: OnceLock<Vec<String>> = OnceLock::new();
    HASHES.get_or_init(|| {
        std::env::var("VICTORY_LEAVE_OUT")
            .unwrap_or_default()
            .split(',')
            .map(|h| h.trim().to_ascii_lowercase())
            .filter(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            .collect()
    })
}

/// The light in the water's volume, in tiles (`water_shading:CS_TransmissionVolumetrics_Tiled`
/// and `_Tiled_LocalLights`, GTA VI 1.0.2.1): programs of one 64-lane wave a group, which a
/// host of 32-lane waves gets in the wave-wide form only (`victory_shader::emit`, 4 and 7
/// branches taken wave-wide). In that form they leave whole tiles of open water dark, around
/// boats, along shores and under bridges, and take about 15 ms of the card a frame (RTX 5070,
/// runs r_water1 and r_water3 of 2026-11-26: 25 fps with them, 30 without, the blocks gone;
/// fxceless/victory-fork's README of 2026-11-27 names the same two programs and form). Left
/// out there, the water has no light of its own depth (darker, bluer) and no blocks;
/// `VICTORY_WATER_LIGHT=1` runs them as before.
const WATER_LIGHT: [&str; 2] = ["5dc3b25986622949", "6f225b641c5b0708"];

fn water_light_left_out(shaders: &[Arc<Shader>]) -> bool {
    static ASKED: OnceLock<bool> = OnceLock::new();
    !*ASKED.get_or_init(|| std::env::var_os("VICTORY_WATER_LIGHT").is_some_and(|v| v == "1"))
        && victory_shader::emit::host_settings()[0] == 32
        && shaders.iter().any(|s| WATER_LIGHT.iter().any(|h| s.is(h)))
}

/// Whether the title's own ray tracing is asked for: its structure builder
/// runs (`VICTORY_BVH_BUILDER=1`), which `VICTORY_RT_NATIVE=1` asks for as
/// well (the title's calls of the builder then also go through
/// `victory_hle::modules::bvh_builder`). With neither, nothing changes.
pub(super) fn builder_asked() -> bool {
    static ASKED: OnceLock<bool> = OnceLock::new();
    *ASKED.get_or_init(|| {
        ["VICTORY_BVH_BUILDER", "VICTORY_RT_NATIVE"]
            .iter()
            .any(|name| std::env::var_os(name).is_some_and(|v| v == "1"))
    })
}

fn left_out(shaders: &[Arc<Shader>]) -> Option<&'static str> {
    if !leave_out().is_empty() && shaders.iter().any(|s| leave_out().iter().any(|h| s.is(h))) {
        return Some("named in VICTORY_LEAVE_OUT, left out");
    }
    if water_light_left_out(shaders) {
        return Some("the water's tiled light on a host of 32-lane waves (dark blocks; VICTORY_WATER_LIGHT=1 runs it), left out");
    }
    static SKIP_RT_RESOLVE: OnceLock<bool> = OnceLock::new();
    let skip_rt = *SKIP_RT_RESOLVE.get_or_init(|| {
        if builder_asked() {
            return false;
        }
        // Without BVH, ~1,000 CS_RaytracingResolve shaders shade no hits while
        // clogging the cold compiler queue; skip unless VICTORY_RT_RESOLVE=1.
        std::env::var_os("VICTORY_RT_RESOLVE").is_none_or(|v| v == "0")
    });
    if skip_rt && named(shaders, b"CS_RaytracingResolve") {
        return Some("shades ray-tracing hits without BVH, left out");
    }
    static SKIP_ACTIVATE_RAYS: OnceLock<bool> = OnceLock::new();
    let skip_activate = *SKIP_ACTIVATE_RAYS.get_or_init(|| {
        if builder_asked() {
            return std::env::var_os("VICTORY_ACTIVATE_RAYS").is_some_and(|v| v == "0");
        }
        std::env::var_os("VICTORY_ACTIVATE_RAYS").is_none_or(|v| v == "0")
    });
    if skip_activate && named(shaders, b"CS_ScreenProbes_ActivateRays") {
        return Some("ray activation shader without BVH, left out");
    }
    None
}

/// The programs of the first scene's producers, by the hashes of the title's
/// own containers (`Shader::is` also matches them when made from pieces).
const SCENE_INPUT: [&str; 6] = [
    "970c4fbb3b017c0e",
    "068c90badc86f1fd",
    "e4a7e088c7780ce8",
    "ebe4e53f031f5249",
    "2cc4c197ee29e291",
    "369cbe906ea72764",
];

/// Confirmed first-scene producers. Prepare these at creation even on a cold
/// launch so cutscene lighting, tiling, and bakes never miss their first frame.
fn scene_input(shaders: &[Arc<Shader>]) -> bool {
    shaders.iter().any(|s| {
        SCENE_INPUT
            .iter()
            .chain(crate::draw::BAKE_PIXEL_SHADERS.iter())
            .any(|h| s.is(h))
    })
}

// SAFETY: D3D12 objects are free-threaded; the graphics state (whose input
// elements point into its own names) is only read after creation.
unsafe impl Send for Pending {}
// SAFETY: as above.
unsafe impl Sync for Pending {}

impl Pending {
    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let mut key = blake3::Hasher::new();
        for s in &shaders {
            key.update(s.stage.as_bytes());
            key.update(s.hash.as_bytes());
        }
        key.update(graphics.as_ref().map(|g| g.describe()).unwrap_or_default().as_bytes());
        let scene_input = scene_input(&shaders);
        let screen = (graphics.as_ref().is_some_and(|g| g.screen_pass())
            && shaders.iter().all(|s| s.stage != "mesh"))
            || scene_input;
        let left_out = left_out(&shaders);
        Pending {
            screen,
            scene_input,
            left_out,
            shaders,
            graphics,
            queue_state: AtomicU8::new(NOT_QUEUED),
            was_slow: AtomicBool::new(false),
            made: OnceLock::new(),
            key: key.finalize().to_hex()[..32].to_owned(),
            used: AtomicBool::new(false),
            asked_ms: AtomicU64::new(0),
            queued_ms: AtomicU64::new(0),
        }
    }

    pub(super) fn key(&self) -> &str {
        &self.key
    }

    fn make(&self) {
        self.made.get_or_init(|| {
            let began = Instant::now();
            let made = HostPipeline::new(&self.shaders, self.graphics.as_deref());
            super::library::note_time(&self.key, began.elapsed().as_millis() as u64);
            made
        });
        let (lock, done) = &DONE;
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        done.notify_all();
    }
}

/// Why a pipeline has no host pipeline (diagnostics): left out on purpose
/// ([`left_out`]), its shaders that do not translate, else "refused" or
/// "being made"; `None` if it has one. (A pipeline left out is never made:
/// it was counted as "being made" in every frame, some 630 indirect
/// ray-tracing resolves a frame in gameplay, while no pipeline was queued.)
#[cfg(feature = "diagnostics")]
pub(crate) fn missing(p: &Pending) -> Option<String> {
    if let Some(why) = p.left_out {
        return Some(format!("left out on purpose: {why}"));
    }
    match p.made.get() {
        Some(Some(_)) => None,
        None => Some("being made".into()),
        Some(None) => {
            let failed: Vec<String> = p
                .shaders
                .iter()
                .filter(|s| matches!(s.translated.get(), Some(None)))
                .map(|s| format!("{} {}", s.stage, s.hash))
                .collect();
            Some(if failed.is_empty() {
                format!("refused {:?}", p.shaders)
            } else {
                failed.join(" + ")
            })
        }
    }
}

/// The hashes of the shaders `p` is made of.
pub(crate) fn hashes(p: &Pending) -> String {
    let hashes: Vec<&str> = p.shaders.iter().map(|s| s.hash.as_str()).collect();
    hashes.join("+")
}

/// Signalled whenever a worker has made a pipeline.
static DONE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// Waiting allowed per title frame, over all threads.
fn wait_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let millis = std::env::var("VICTORY_PIPELINE_WAIT_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(500);
        tracing::info!(millis, "host pipeline wait budget per frame");
        Duration::from_millis(millis)
    })
}

/// As [`wait_budget`], for passes over the screen (`VICTORY_SCREEN_WAIT_MS`,
/// 10 s; 0: like the others). Below the title's 30 s watchdog.
fn screen_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let millis = std::env::var("VICTORY_SCREEN_WAIT_MS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(10_000);
        Duration::from_millis(millis)
    })
}

/// Whether passes over the screen go before the others.
fn screens_first() -> bool {
    !screen_budget().is_zero()
}

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);

/// Draws and dispatches (direct and indirect) asked of a pipeline left out on
/// purpose ([`left_out`]), since the last [`take_left_out`].
static LEFT_OUT: AtomicU64 = AtomicU64::new(0);

/// See [`LEFT_OUT`] (for the frame line).
pub(crate) fn take_left_out() -> u64 {
    LEFT_OUT.swap(0, Ordering::Relaxed)
}

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

/// The host pipeline if it is made (or is made within the frame's budget);
/// queues it on first use.
pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
    if p.left_out.is_some() {
        LEFT_OUT.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    // A still-pending pipeline was requested by the title too. Prewarm it
    // on the next launch, even if this run never got to use it.
    if !p.used.load(Ordering::Relaxed) && !p.used.swap(true, Ordering::Relaxed) {
        super::library::note_used(&p.key);
    }
    if p.made.get().is_none() {
        // (Noted on every call until it is: beside `used` above, a second
        // thread could find `used` set and the time not yet.)
        let first = p.asked_ms.load(Ordering::Relaxed) == 0
            && p.asked_ms.compare_exchange(0, stamp(), Ordering::Relaxed, Ordering::Relaxed).is_ok();
        start(p, REQUESTED);
        wait(p, crate::frame::current(), first);
    }
    p.made.get()?.as_ref()
}

/// [`crate::host::now_ms`], never 0 (its first call returns 0): 0 stands for
/// "never" where these are kept.
fn stamp() -> u64 {
    crate::host::now_ms().max(1)
}

/// How long ago `then` ([`stamp`]) was; `None` for 0.
fn since(then: u64, now: u64) -> Option<Duration> {
    (then != 0).then(|| Duration::from_millis(now.saturating_sub(then)))
}

/// Why [`wait`] does not wait ([`skip_wait`]).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Skip {
    /// It was first asked for longer ago than a frame's budget.
    Old,
    /// No worker has it, and the oldest requested pipeline no worker has
    /// taken has stood for longer than a frame's budget.
    Behind,
}

/// Whether a wait for a pipeline can be given up before it begins, because it
/// cannot end within `budget`: `in_hand` is whether a worker is making it,
/// `front` how long the oldest entry of [`Queue::requested`] has stood there
/// (`None`: the list is empty), `asked` how long ago it was first asked for
/// (`None`: just now).
///
/// A pipeline that was asked for a whole budget ago was waited for then (or
/// was skipped for the reason below) and is still not made: a wait in every
/// later frame only holds the list. And while the front of the line has stood
/// a whole budget with no worker free for it, one behind it that no worker has
/// begun cannot be made in a budget either. A burst of new pipelines on a warm
/// cache has a young front (the workers take them within milliseconds), so its
/// first uses keep their waits; the length of the line says nothing of that
/// and is not asked.
fn skip_wait(in_hand: bool, front: Option<Duration>, asked: Option<Duration>, budget: Duration) -> Option<Skip> {
    if asked.is_some_and(|asked| asked > budget) {
        Some(Skip::Old)
    } else if !in_hand && front.is_some_and(|front| front > budget) {
        Some(Skip::Behind)
    } else {
        None
    }
}

/// When the oldest entry of [`Queue::requested`] was put there ([`stamp`]; 0:
/// the list is empty). Written under [`QUEUE`]'s lock, read by [`wait`]
/// without it.
static FRONT_MS: AtomicU64 = AtomicU64::new(0);

/// Whether [`wait`] counts what it does and says so every few seconds:
/// `VICTORY_PIPELINE_WAIT_ONCE=1` or `=count` (the latter changes nothing
/// else, for the run to compare with).
fn waits_counted() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_WAIT_ONCE").is_some_and(|v| v == "1" || v == "count"))
}

/// Since the last line of [`waits_said`]: waits begun, microseconds waited,
/// waits that ended with the budget and no pipeline, waits not begun for
/// [`Skip::Old`], for [`Skip::Behind`], and those of the latter that were the
/// pipeline's first use.
static WAITS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// One line of [`WAITS`] at most every five seconds, from the thread that
/// ends a wait then (a list thread of ours, or a recording thread).
fn waits_said() {
    static SAID_MS: AtomicU64 = AtomicU64::new(0);
    let (now, last) = (stamp(), SAID_MS.load(Ordering::Relaxed));
    if now.saturating_sub(last) < 5_000 || SAID_MS.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        return;
    }
    let n: Vec<u64> = WAITS.iter().map(|n| n.swap(0, Ordering::Relaxed)).collect();
    tracing::info!(
        "waits for host pipelines by draws and dispatches in the last {:.1} s: {} begun, {} ms waited in all, {} ended with \
         the budget and no pipeline; not waited for: {} first asked for longer ago than the budget, {} behind a line that \
         stood longer than the budget ({} of them a first use); {} being made (VICTORY_PIPELINE_WAIT_ONCE=1 leaves those \
         waits out, =count only counts)",
        now.saturating_sub(last) as f64 / 1e3,
        n[0],
        n[1] / 1000,
        n[2],
        n[3],
        n[4],
        n[5],
        waiting(),
    );
}

/// Makes `p` on this thread if it is not made yet (waiting for a worker
/// that is making it); whether it has a host pipeline.
pub(crate) fn now(p: &Arc<Pending>) -> bool {
    if p.left_out.is_some() {
        return false;
    }
    p.make();
    p.made.get().is_some_and(|made| made.is_some())
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it),
/// or if it is a confirmed opening-scene / bake producer.
pub(crate) fn early(p: &Arc<Pending>) {
    if p.left_out.is_some() {
        return;
    }
    if !p.shaders.is_empty() && (p.scene_input || super::library::wanted_early(&p.key)) {
        super::library::EARLY.fetch_add(1, Ordering::Relaxed);
        start(p, if p.scene_input { REQUESTED } else { EARLY });
    }
}

/// Queues `p` for the workers at once, ahead of any draw or dispatch: for
/// pipelines that are all needed in the same instant (the structure
/// builder's, which run all or none). Nothing waits here; [`now`] waits
/// for one a worker has begun and makes one no worker has taken yet.
///
/// The builder's were made by [`now`] alone at first: one after the other
/// on the title's recording thread at the first builder dispatch, some
/// fifty programs at two to four a second on a cache that had none of
/// them. The frame stood for 32 s and the title's own watchdog (30 s)
/// ended the run.
pub(super) fn ahead(p: &Arc<Pending>) {
    // (One left out on purpose is never made; one without shaders has
    // nothing to make.)
    if p.left_out.is_some() || p.shaders.is_empty() {
        return;
    }
    start(p, REQUESTED);
}

/// Whether the frame's budget is one for all threads together: a thread
/// that waits takes what is left of it and gives back what it did not
/// use. Counted only when a wait had ended, the budget was one per thread:
/// each of the title's five recording threads began its first wait of a
/// frame with nothing counted yet and waited the whole half second, 2.5 s
/// of waiting in every frame in which pipelines were being made (runs
/// 34-49: "2504 ms waiting for pipelines" in each such frame).
/// On by default; `VICTORY_PIPELINE_WAIT_SHARED=0` counts as before.
fn shared_budget() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_WAIT_SHARED").is_none_or(|v| v != "0"))
}

/// `VICTORY_PIPELINE_WAIT_BACKLOG=n` (read once, unset by default): past
/// `n` pipelines queued, a draw's pipeline is not waited for at all and the
/// draw is dropped at once, as it is when the budget runs out. Unset, every
/// draw waits as before.
fn backlog() -> Option<u64> {
    static MOST: OnceLock<Option<u64>> = OnceLock::new();
    *MOST.get_or_init(|| {
        let most = std::env::var("VICTORY_PIPELINE_WAIT_BACKLOG").ok().and_then(|v| v.trim().parse::<u64>().ok());
        if let Some(most) = most {
            tracing::info!(most, "host pipelines are not waited for while more than this many are queued");
        }
        most
    })
}

/// Waits for `p` while frame `frame` has budget left; `first` is whether this
/// is the first time a draw or dispatch asks for it.
fn wait(p: &Pending, frame: u64, first: bool) {
    let screen = p.screen && screens_first();
    let budget = if screen { screen_budget() } else { wait_budget() };
    if budget.is_zero() {
        return;
    }
    // with more queued than the workers can make within the budget, the
    // pipeline asked for here will not arrive in time, and waiting only
    // costs the frame: run 5 spent the whole 500 ms in every frame for two
    // minutes while 470 were queued (about 290 ms a frame, two frames in
    // the line), run 10 sat at 3 fps with 600 queued. passes over the
    // screen keep their own budget and workers.
    if !screen && backlog().is_some_and(|most| WAITING.load(Ordering::Relaxed) > most) {
        return;
    }
    // `VICTORY_PIPELINE_WAIT_ONCE=1` (off by default): a wait that cannot end
    // within the budget is not begun ([`skip_wait`]). Without it a thread
    // takes what is left of the frame's budget in every frame in which a
    // pipeline it draws with is unmade, whether or not a worker has it: the
    // list stands, the line behind it does, and the title waits at its
    // present. Up to about 215 ms a frame while pipelines asked for long ago
    // are unmade (run of 2026-11-26 with the title's ray tracing, the same
    // place before and after its queue ran empty: list threads 780 to 260 ms a
    // frame, the title at its presents 258 to 41 ms). A cold pipeline first
    // asked for while the workers keep up still costs one wait of up to the
    // budget, as before: of the two rules only the one about the line applies
    // to a first use, and a pass the title runs once is never asked for a
    // second time, so the rule about age cannot lose it. Passes over the
    // screen keep their own budget. Compiled and tested, not yet seen in a
    // run; the line of [`waits_said`] is what a run shows it by.
    let counted = !screen && waits_counted();
    if !screen && p.made.get().is_none() && crate::asked_for!("VICTORY_PIPELINE_WAIT_ONCE") {
        let now = stamp();
        let skip = skip_wait(
            p.queue_state.load(Ordering::Relaxed) == MAKING,
            since(FRONT_MS.load(Ordering::Relaxed), now),
            since(p.asked_ms.load(Ordering::Relaxed), now),
            budget,
        );
        if let Some(skip) = skip {
            if counted {
                WAITS[if skip == Skip::Old { 3 } else { 4 }].fetch_add(1, Ordering::Relaxed);
                if skip == Skip::Behind && first {
                    WAITS[5].fetch_add(1, Ordering::Relaxed);
                }
                waits_said();
            }
            return;
        }
    }
    // The frame the budget belongs to, and what was waited of it (or is
    // taken by a thread waiting now); of the passes over the screen too.
    static SPENT: [Mutex<(u64, Duration)>; 2] = [const { Mutex::new((u64::MAX, Duration::ZERO)) }; 2];
    let spent_of = &SPENT[usize::from(screen)];
    let shared = shared_budget();
    let left = {
        let mut spent = spent_of.lock().unwrap_or_else(|e| e.into_inner());
        if spent.0 != frame {
            *spent = (frame, Duration::ZERO);
        }
        let left = budget.saturating_sub(spent.1);
        if shared {
            spent.1 += left;
        }
        left
    };
    if left.is_zero() {
        return;
    }
    let started = Instant::now();
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done.wait_timeout(held, rest).unwrap_or_else(|e| e.into_inner()).0;
    }
    drop(held);
    let waited = started.elapsed().min(left);
    if counted {
        WAITS[0].fetch_add(1, Ordering::Relaxed);
        WAITS[1].fetch_add(waited.as_micros() as u64, Ordering::Relaxed);
        if p.made.get().is_none() {
            WAITS[2].fetch_add(1, Ordering::Relaxed);
        }
        waits_said();
    }
    if screen && waited > Duration::from_millis(500) {
        tracing::info!(
            ms = waited.as_millis() as u64,
            made = p.made.get().is_some(),
            shaders = ?p.shaders,
            "a pass over the screen was waited for (VICTORY_SCREEN_WAIT_MS=0: dropped past half a second a frame)"
        );
    }
    {
        let mut spent = spent_of.lock().unwrap_or_else(|e| e.into_inner());
        // A frame that began meanwhile has its own budget.
        if spent.0 == frame {
            if shared {
                spent.1 = spent.1.saturating_sub(left - waited);
            } else {
                spent.1 += waited;
            }
        }
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::PipelineWaitUs,
        started.elapsed().as_micros() as u64,
    );
}

const NOT_QUEUED: u8 = 0;
const EARLY: u8 = 1;
const REQUESTED: u8 = 2;
const MAKING: u8 = 3;

/// How many workers may make pipelines at once that are known to take long
/// and that no draw has asked for: a quarter of them. On a fresh cache (a
/// change of the translator or the driver) the early list begins with
/// programs that take tens of seconds each: all 16 workers were in them
/// through the whole of the title's loading, 97 pipelines made in 20 s, and
/// the first minute and a half of the world ran at 0.5 to 1.6 s a frame,
/// dropping up to 2,900 draws a frame (run shaders-1).
/// `VICTORY_PIPELINE_SLOW=0`: in the order the title creates them, as before.
fn slow_limit() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        if std::env::var_os("VICTORY_PIPELINE_SLOW").is_some_and(|v| v == "0") {
            usize::MAX
        } else {
            (worker_count(None, std::thread::available_parallelism().map_or(2, |n| n.get())) / 4).max(1)
        }
    })
}

struct Queue {
    /// Requested passes over the screen: before all others, and the only ones
    /// [`SCREEN_WORKERS`] take.
    screen: VecDeque<Arc<Pending>>,
    early: VecDeque<Arc<Pending>>,
    /// Those of the early ones known to take long, and how many of them are
    /// being made ([`slow_limit`]).
    slow: VecDeque<Arc<Pending>>,
    slow_now: usize,
    requested: VecDeque<Arc<Pending>>,
}

impl Queue {
    const fn new() -> Self {
        Self {
            screen: VecDeque::new(),
            early: VecDeque::new(),
            slow: VecDeque::new(),
            slow_now: 0,
            requested: VecDeque::new(),
        }
    }

    /// Queues an unrequested pipeline or promotes one queued at creation.
    /// Returns whether it is new to the queue, or `None` if nothing changed.
    fn push(&mut self, p: &Arc<Pending>, priority: u8) -> Option<bool> {
        self.push_as(p, priority, slow_limit() != usize::MAX && super::library::slow(&p.key))
    }

    fn push_as(&mut self, p: &Arc<Pending>, priority: u8, slow: bool) -> Option<bool> {
        let prev = p.queue_state.load(Ordering::Relaxed);
        if prev >= priority {
            return None;
        }
        p.queue_state.store(priority, Ordering::Relaxed);
        if priority == REQUESTED && p.screen && screens_first() {
            self.screen.push_back(p.clone());
        } else if priority == REQUESTED {
            p.queued_ms.store(stamp(), Ordering::Relaxed);
            self.requested.push_back(p.clone());
        } else if slow {
            self.slow.push_back(p.clone());
        } else {
            self.early.push_back(p.clone());
        }
        Some(prev == NOT_QUEUED)
    }

    fn pop(&mut self, screens_only: bool, slow_limit: usize) -> Option<Arc<Pending>> {
        // An early entry can have been promoted and claimed already.
        let unclaimed = |list: &mut VecDeque<Arc<Pending>>| {
            std::iter::from_fn(|| list.pop_front()).find(|p| p.queue_state.load(Ordering::Relaxed) == EARLY)
        };
        let mut p = self.screen.pop_front();
        if p.is_none() && !screens_only {
            p = self.requested.pop_front().or_else(|| unclaimed(&mut self.early));
            if p.is_none() && self.slow_now < slow_limit {
                p = unclaimed(&mut self.slow);
                if let Some(p) = &p {
                    p.was_slow.store(true, Ordering::Relaxed);
                    self.slow_now += 1;
                }
            }
        }
        let p = p?;
        p.queue_state.store(MAKING, Ordering::Relaxed);
        Some(p)
    }

    /// For a thread of [`slow_extras`]: the first slow pipeline still
    /// unclaimed, only while nothing is requested and no pass over the
    /// screen waits. It does not count in `slow_now` and is not marked
    /// `was_slow`: the workers' own limit and places stay as they are.
    fn take_slow(&mut self) -> Option<Arc<Pending>> {
        if !self.requested.is_empty() || !self.screen.is_empty() {
            return None;
        }
        // (One promoted since it was queued is skipped, as in `pop`.)
        let p = std::iter::from_fn(|| self.slow.pop_front()).find(|p| p.queue_state.load(Ordering::Relaxed) == EARLY)?;
        p.queue_state.store(MAKING, Ordering::Relaxed);
        Some(p)
    }

    /// When the oldest requested pipeline no worker has taken was queued
    /// ([`FRONT_MS`]).
    fn front_ms(&self) -> u64 {
        self.requested.front().map_or(0, |p| p.queued_ms.load(Ordering::Relaxed))
    }
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue::new());
static AVAILABLE: Condvar = Condvar::new();

/// Start or promote work for a draw without waiting behind the entire early
/// list. Queue changes and worker claims happen under the same lock.
fn start(p: &Arc<Pending>, priority: u8) {
    if p.queue_state.load(Ordering::Relaxed) >= priority {
        return;
    }
    if !workers() {
        if p.queue_state.swap(MAKING, Ordering::Relaxed) == NOT_QUEUED {
            p.make();
        }
        return;
    }
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(new) = queue.push(p, priority) {
        if new {
            PEAK.fetch_max(WAITING.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
        }
        // (One woken could be a worker that takes passes over the screen
        // alone: it would leave the pipeline to nobody.)
        AVAILABLE.notify_all();
        FRONT_MS.store(queue.front_ms(), Ordering::Relaxed);
    }
}

/// Workers that make passes over the screen alone, besides the others.
const SCREEN_WORKERS: usize = 2;

fn next(screens_only: bool) -> Arc<Pending> {
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(p) = queue.pop(screens_only, slow_limit()) {
            FRONT_MS.store(queue.front_ms(), Ordering::Relaxed);
            // Nothing asked for is left: the extra threads may take slow
            // ones now ([`slow_extras`]).
            if EXTRAS.load(Ordering::Relaxed) > 0
                && queue.requested.is_empty()
                && queue.screen.is_empty()
                && !queue.slow.is_empty()
            {
                AVAILABLE.notify_all();
            }
            return p;
        }
        queue = AVAILABLE.wait(queue).unwrap_or_else(|e| e.into_inner());
    }
}

/// For a thread of [`slow_extras`]: the next slow pipeline, waited for.
fn next_slow() -> Arc<Pending> {
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(p) = queue.take_slow() {
            return p;
        }
        queue = AVAILABLE.wait(queue).unwrap_or_else(|e| e.into_inner());
    }
}

/// The threads of [`slow_extras`] that were started.
static EXTRAS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// More threads that make early pipelines known to take long, only while
/// nothing is requested (`VICTORY_PIPELINE_SLOW_WORKERS=<n>`, 0 by
/// default): at most `processors - workers - 4`, none where the PC's memory
/// is short or [`slow_limit`] puts nothing on the slow list. They take
/// nothing a draw asks for and do not count in the workers' slow limit, so
/// the workers keep their places for requested work.
///
/// It helps when the slow list is known but its pipelines are not cheap in
/// this run yet: after the shaders were translated again, a driver change,
/// or a run that ended before they were made. Runs that showed the queue
/// at the world's first frame (2026-11-26): -221426, -222450, -223904 and
/// -224808, 485 and 526 queued in the last two, where four workers took
/// slow ones while twelve stood idle through menu and loading; runs with
/// settled caches showed none (-220729, -225818, -230328, -231606), and
/// there the extra threads stay idle. Expected from the measured 3.5-4 a
/// second with sixteen threads: the queue at the world's first frame falls
/// to between 0 and about 150; not verified in a run. The risk to measure
/// in a pair of runs on an unsettled cache: in the world up to `n` slow
/// compiles run beside the workers at the same priority; if requested
/// pipelines come later with the switch ("dropped N without a host
/// pipeline", the time to "host pipelines all made"), the extras need a
/// further gate before this is recommended.
fn slow_extras(workers: usize, processors: usize) -> usize {
    if crate::residency::limited_host_ram() || slow_limit() == usize::MAX {
        return 0;
    }
    let asked = std::env::var("VICTORY_PIPELINE_SLOW_WORKERS").ok().and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
    extras_for(asked, workers, processors)
}

/// `asked` extra threads, capped at the processors left after `workers`
/// and four more.
fn extras_for(asked: usize, workers: usize, processors: usize) -> usize {
    asked.min(processors.saturating_sub(workers + 4))
}

/// How many workers make pipelines: `asked` (`VICTORY_PIPELINE_WORKERS`, 1 to
/// 64), else two thirds of the logical processors, 2 to 16. They run below the
/// title's own threads, and a cold cache queues hundreds at once.
fn worker_count(asked: Option<usize>, processors: usize) -> usize {
    worker_count_for(asked, processors, false)
}

fn worker_count_for(asked: Option<usize>, processors: usize, limited_ram: bool) -> usize {
    match asked {
        Some(n) => n.clamp(1, 64),
        None if limited_ram => (processors * 3 / 4).clamp(2, 8),
        None => (processors * 2 / 3).clamp(2, 16),
    }
}

/// Starts the workers ([`worker_count`]) on first use.
fn workers() -> bool {
    static RUNNING: OnceLock<bool> = OnceLock::new();
    *RUNNING.get_or_init(|| {
        let mut running = false;
        let workers = worker_count_for(
            std::env::var("VICTORY_PIPELINE_WORKERS")
                .ok()
                .and_then(|v| v.trim().parse().ok()),
            std::thread::available_parallelism().map_or(2, |n| n.get()),
            crate::residency::limited_host_ram(),
        );
        tracing::info!(workers, "pipeline workers");
        let screens = if screens_first() { SCREEN_WORKERS } else { 0 };
        for i in 0..workers + screens {
            let screens_only = i >= workers;
            let spawned = std::thread::Builder::new()
                .name(format!("victory-pipelines-{i}"))
                .spawn(move || {
                    // Below the title's own threads (loading, recording),
                    // and the streaming one, which is below normal itself
                    // (`VICTORY_PIPELINE_LOWEST=0`: level with it, as before).
                    let lowest = !screens_only && std::env::var_os("VICTORY_PIPELINE_LOWEST").is_none_or(|v| v != "0");
                    // SAFETY: a plain call on our own thread.
                    unsafe {
                        use windows::Win32::System::Threading::*;
                        let priority = if lowest { THREAD_PRIORITY_LOWEST } else { THREAD_PRIORITY_BELOW_NORMAL };
                        let _ = SetThreadPriority(GetCurrentThread(), priority);
                    }
                    loop {
                        let p = next(screens_only);
                        p.make();
                        if p.was_slow.load(Ordering::Relaxed) {
                            let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
                            queue.slow_now -= 1;
                            AVAILABLE.notify_all();
                        }
                        if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                            drained();
                        } else {
                            super::library::save(false);
                        }
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "no pipeline worker");
            } else {
                running = true;
            }
        }
        // Read once, here ([`slow_extras`]); none with it unset. the screen
        // workers count among the threads already making pipelines.
        let extras = if running { slow_extras(workers + screens, std::thread::available_parallelism().map_or(2, |n| n.get())) } else { 0 };
        for i in 0..extras {
            let spawned = std::thread::Builder::new().name(format!("victory-pipelines-slow-{i}")).spawn(|| {
                let lowest = std::env::var_os("VICTORY_PIPELINE_LOWEST").is_none_or(|v| v != "0");
                // SAFETY: a plain call on our own thread.
                unsafe {
                    use windows::Win32::System::Threading::*;
                    let priority = if lowest { THREAD_PRIORITY_LOWEST } else { THREAD_PRIORITY_BELOW_NORMAL };
                    let _ = SetThreadPriority(GetCurrentThread(), priority);
                }
                loop {
                    let p = next_slow();
                    p.make();
                    // As the workers do: "(N being made)" comes back to zero
                    // and the cache is saved.
                    if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                        drained();
                    } else {
                        super::library::save(false);
                    }
                }
            });
            match spawned {
                Ok(_) => {
                    EXTRAS.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => tracing::warn!(error = %e, "no extra pipeline thread for slow ones"),
            }
        }
        if extras > 0 {
            tracing::info!(
                extras = EXTRAS.load(Ordering::Relaxed),
                "threads that make slow pipelines while nothing is requested (VICTORY_PIPELINE_SLOW_WORKERS)"
            );
        }
        running
    })
}

/// The most pipelines waiting at once since the workers last ran out.
static PEAK: AtomicU64 = AtomicU64::new(0);

/// The workers ran out of work: logged after a batch of 10 or more, and
/// the cache saved (at most every 20 s).
fn drained() {
    use super::library::{EARLY, LOADED, MADE};
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipelines all made"
        );
    }
    super::library::save(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_count_follows_the_processors_and_the_switch() {
        assert_eq!(worker_count(None, 1), 2);
        assert_eq!(worker_count(None, 24), 16);
        assert_eq!(worker_count(None, 64), 16);
        assert_eq!(worker_count(Some(0), 24), 1);
        assert_eq!(worker_count(Some(20), 24), 20);
        assert_eq!(worker_count_for(None, 8, true), 6);
        assert_eq!(worker_count_for(None, 16, true), 8);
        assert_eq!(worker_count_for(None, 24, true), 8);
        assert_eq!(worker_count_for(Some(12), 8, true), 12);
    }

    /// A first use queues the pipeline and, within the budget, gets the
    /// worker's answer (here: no host pipeline for no shaders).
    #[test]
    fn first_use_is_made_by_a_worker() {
        let p = Arc::new(Pending::new(Vec::new(), None));
        assert!(ready(&p).is_none());
        assert_ne!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    /// A pipeline left out on purpose (a ray-tracing resolve without the structure builder)
    /// is never queued, so never made: it is counted as left out, and named so, not as
    /// "being made" (some 630 indirect dispatches a frame were, in every gameplay frame).
    #[test]
    fn pipelines_left_out_are_counted_and_never_queued() {
        let mut p = Pending::new(Vec::new(), None);
        p.left_out = Some("shades ray-tracing hits without BVH, left out");
        let p = Arc::new(p);
        let before = LEFT_OUT.load(Ordering::Relaxed);
        assert!(ready(&p).is_none());
        assert!(ready(&p).is_none());
        assert!(!now(&p));
        assert_eq!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        assert!(p.made.get().is_none());
        assert!(LEFT_OUT.load(Ordering::Relaxed) >= before + 2);
        #[cfg(feature = "diagnostics")]
        assert_eq!(
            missing(&p).as_deref(),
            Some("left out on purpose: shades ray-tracing hits without BVH, left out")
        );
    }

    #[test]
    fn pending_pipeline_is_recorded_for_the_next_launch() {
        let mut p = Pending::new(Vec::new(), None);
        p.key = "pending-pipeline-test".to_owned();
        // Simulate a worker that has taken the job but cannot finish it yet.
        p.queue_state.store(MAKING, Ordering::Relaxed);
        let p = Arc::new(p);

        assert!(!super::super::library::wanted_early(&p.key));
        assert!(ready(&p).is_none());
        assert!(p.made.get().is_none());
        assert!(super::super::library::wanted_early(&p.key));
    }

    #[test]
    fn draws_promote_early_pipelines_without_claiming_them_twice() {
        let earlier = Arc::new(Pending::new(Vec::new(), None));
        let promoted = Arc::new(Pending::new(Vec::new(), None));
        let requested = Arc::new(Pending::new(Vec::new(), None));
        let mut queue = Queue::new();

        assert_eq!(queue.push(&earlier, EARLY), Some(true));
        assert_eq!(queue.push(&promoted, EARLY), Some(true));
        assert_eq!(queue.push(&requested, REQUESTED), Some(true));
        assert_eq!(queue.push(&promoted, REQUESTED), Some(false));
        assert_eq!(queue.push(&promoted, REQUESTED), None);

        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &requested));
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &promoted));
        assert_eq!(queue.push(&promoted, REQUESTED), None);
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &earlier));
        assert!(queue.pop(false, 1).is_none());
    }

    #[test]
    fn pipelines_known_to_take_long_leave_workers_for_the_others() {
        let new = || Arc::new(Pending::new(Vec::new(), None));
        let (long, longer, asked, quick) = (new(), new(), new(), new());
        let mut queue = Queue::new();
        for p in [&long, &longer, &asked] {
            assert_eq!(queue.push_as(p, EARLY, true), Some(true));
        }
        assert_eq!(queue.push_as(&quick, EARLY, false), Some(true));
        // The quick one first, then one long one at a time; one a draw asks
        // for waits for nobody.
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &quick));
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &long));
        assert!(queue.pop(false, 1).is_none());
        assert_eq!(queue.push_as(&asked, REQUESTED, true), Some(false));
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &asked));
        assert!(!asked.was_slow.load(Ordering::Relaxed));
        assert!(queue.pop(false, 1).is_none());
        queue.slow_now -= 1;
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &longer));
        assert!(queue.pop(false, 1).is_none());
    }

    /// An extra thread takes a slow pipeline only while nothing is requested
    /// and no pass over the screen waits, skips one promoted since, and
    /// leaves the workers' slow count alone; the extras are capped at the
    /// processors left.
    #[test]
    fn extra_threads_take_slow_ones_only_when_nothing_is_asked_for() {
        let new = || Arc::new(Pending::new(Vec::new(), None));
        let (long, promoted, longer, asked) = (new(), new(), new(), new());
        let mut queue = Queue::new();
        for p in [&long, &promoted, &longer] {
            assert_eq!(queue.push_as(p, EARLY, true), Some(true));
        }
        assert_eq!(queue.push_as(&asked, REQUESTED, false), Some(true));
        assert!(queue.take_slow().is_none());
        assert!(Arc::ptr_eq(&queue.pop(false, 0).unwrap(), &asked));
        let screen = {
            let mut p = Pending::new(Vec::new(), None);
            p.screen = true;
            Arc::new(p)
        };
        if screens_first() {
            assert_eq!(queue.push(&screen, REQUESTED), Some(true));
            assert!(queue.take_slow().is_none());
            assert!(Arc::ptr_eq(&queue.pop(true, 0).unwrap(), &screen));
        }
        assert!(Arc::ptr_eq(&queue.take_slow().unwrap(), &long));
        assert_eq!(long.queue_state.load(Ordering::Relaxed), MAKING);
        assert!(!long.was_slow.load(Ordering::Relaxed));
        assert_eq!(queue.slow_now, 0);
        // A draw asked for this one meanwhile, and a worker has it.
        assert_eq!(queue.push_as(&promoted, REQUESTED, true), Some(false));
        assert!(Arc::ptr_eq(&queue.pop(false, 0).unwrap(), &promoted));
        assert!(Arc::ptr_eq(&queue.take_slow().unwrap(), &longer));
        assert!(queue.take_slow().is_none());
        assert_eq!(queue.slow_now, 0);
        assert_eq!(extras_for(12, 16, 32), 12);
        assert_eq!(extras_for(20, 16, 32), 12);
        assert_eq!(extras_for(4, 16, 8), 0);
        assert_eq!(extras_for(0, 2, 32), 0);
    }

    /// A wait is given up before it begins only when it cannot end within
    /// the budget: the pipeline was first asked for longer ago than that, or
    /// no worker has it and the front of the line has stood that long.
    #[test]
    fn a_wait_that_cannot_end_in_the_budget_is_not_begun() {
        let budget = Duration::from_millis(500);
        let (young, old) = (Some(Duration::from_millis(20)), Some(Duration::from_millis(501)));
        // A first use on a warm cache: the front is young, the wait is kept.
        assert_eq!(skip_wait(false, young, Some(Duration::ZERO), budget), None);
        assert_eq!(skip_wait(false, None, None, budget), None);
        // In a worker's hands and asked for within the budget: kept, whatever the line.
        assert_eq!(skip_wait(true, old, young, budget), None);
        assert_eq!(skip_wait(true, old, Some(budget), budget), None);
        // Asked for longer ago than the budget: not begun, in hand or not.
        assert_eq!(skip_wait(true, young, old, budget), Some(Skip::Old));
        assert_eq!(skip_wait(false, None, old, budget), Some(Skip::Old));
        // No worker has it and the front has stood a budget: not begun, a first use too.
        assert_eq!(skip_wait(false, old, Some(Duration::ZERO), budget), Some(Skip::Behind));
        assert_eq!(skip_wait(false, old, None, budget), Some(Skip::Behind));
        // A time never noted is "just now", not "long ago".
        assert_eq!(since(0, 10_000), None);
        assert_eq!(since(1, 601), Some(Duration::from_millis(600)));
        assert_eq!(since(700, 601), Some(Duration::ZERO));
        assert_eq!(skip_wait(true, None, since(0, 10_000), budget), None);
    }

    /// The front of the requested line is the oldest entry no worker took.
    #[test]
    fn the_front_of_the_line_is_the_oldest_requested_one() {
        let new = || Arc::new(Pending::new(Vec::new(), None));
        let (early, first, second) = (new(), new(), new());
        let mut queue = Queue::new();
        assert_eq!(queue.front_ms(), 0);
        assert_eq!(queue.push_as(&early, EARLY, false), Some(true));
        assert_eq!(queue.front_ms(), 0);
        assert_eq!(queue.push_as(&first, REQUESTED, false), Some(true));
        first.queued_ms.store(7, Ordering::Relaxed);
        assert_eq!(queue.push_as(&second, REQUESTED, false), Some(true));
        second.queued_ms.store(9, Ordering::Relaxed);
        assert_eq!(queue.front_ms(), 7);
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &first));
        assert_eq!(queue.front_ms(), 9);
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &second));
        assert_eq!(queue.front_ms(), 0);
        assert_eq!(early.queued_ms.load(Ordering::Relaxed), 0);
    }

    /// Passes over the screen go before all others, and workers of their own
    /// take nothing else.
    #[test]
    fn passes_over_the_screen_go_first() {
        let of = |screen| {
            let mut p = Pending::new(Vec::new(), None);
            p.screen = screen;
            Arc::new(p)
        };
        let (world, screen) = (of(false), of(true));
        let mut queue = Queue::new();
        assert_eq!(queue.push(&world, REQUESTED), Some(true));
        assert_eq!(queue.push(&screen, REQUESTED), Some(true));
        assert!(Arc::ptr_eq(&queue.pop(true, 1).unwrap(), &screen));
        assert!(queue.pop(true, 1).is_none());
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &world));
    }
}
