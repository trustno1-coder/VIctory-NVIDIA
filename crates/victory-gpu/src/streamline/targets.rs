//! A list, at chosen upscalings, of the textures the title's passes wrote
//! since the upscaling before, so the buffers Ray Reconstruction would need
//! can be named from a run. Off by default.
//!
//! Ray Reconstruction wants diffuse albedo, specular albedo, normals and
//! roughness (and optionally a hit distance or specular motion vectors).
//! The layer sees only what FidelityFX is given at the upscale call: colour,
//! depth, motion vectors and the output. The title's deferred passes write
//! the rest, and what those passes are called and what formats they write is
//! not known until a run lists them. `VICTORY_RR_TARGETS=1` does that: for
//! the 1st, 300th and 3000th upscaling (or the numbers of a comma list such
//! as `1200,5000`) it logs every pass recorded since the previous upscaling
//! with the textures it wrote (render targets, UAVs) or bound (depth), their sizes
//! and formats, and the program's own name where its container carries one.
//! Every line starts `RR targets:`.
//!
//! `1` alone names the 1st, 300th and 3000th upscaling, so it cannot mean
//! "only the first"; `1,2` does (a list is taken as written).
//!
//! The cost is a lock and a hash a draw or dispatch, so this is a
//! diagnostics switch and not a play setting; unset, every entry point
//! returns after one load. Nothing is read back, nothing is saved.
//!
//! The limits: lists are recorded ahead of the card and on several threads,
//! so entries are in recording order, not card order. UAV writes are only
//! seen where the title's tables are walked, so a repeat of an unchanged
//! table is not seen again, and a pass that writes through one is listed
//! with the textures of the last walk only.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};

use super::{COLOUR, DEPTH, MOTION, OUTPUT, Upscale};
use crate::object::This;

/// The upscaling numbers (counted from 1 here) that are logged; empty is off.
static SAMPLES: OnceLock<Vec<u64>> = OnceLock::new();
/// Recording order of passes, across threads.
static SEQ: AtomicU64 = AtomicU64::new(0);
/// Upscalings seen so far.
static UPSCALINGS: AtomicU64 = AtomicU64::new(0);
/// What each pipeline is called, by its address, so the name is built once.
static LABELS: Mutex<Option<HashMap<usize, Arc<str>>>> = Mutex::new(None);
/// The passes since the last upscaling.
static JOURNAL: std::sync::LazyLock<Mutex<Journal>> = std::sync::LazyLock::new(Mutex::default);

/// Passes kept between two upscalings; past it they are counted and dropped.
const CAP: usize = 4096;
/// Lines of passes in one report.
const LINES: usize = 200;

thread_local! {
    /// The pass this thread is recording: its writes are still arriving.
    static OPEN: RefCell<Option<Open>> = const { RefCell::new(None) };
}

/// How a pass wrote a texture.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum How {
    Target,
    /// a depth view was bound, which does not say the pass wrote to it
    Depth,
    Unordered,
}

impl How {
    fn name(self) -> &'static str {
        match self {
            How::Target => "target",
            How::Depth => "depth",
            How::Unordered => "uav",
        }
    }
}

/// A pass whose writes are still being collected on this thread.
struct Open {
    list: usize,
    pipeline: usize,
    label: Arc<str>,
    compute: bool,
    writes: Vec<(usize, How)>,
}

/// A texture a pass wrote, described once when the pass is first stored.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Written {
    resource: usize,
    how: How,
    size: [u32; 2],
    format: i32,
}

/// A distinct pass (same pipeline, same textures) and how often it ran.
#[derive(Debug)]
struct Entry {
    first: u64,
    last: u64,
    count: u32,
    thread: u32,
    list: usize,
    label: Arc<str>,
    compute: bool,
    writes: Vec<Written>,
}

/// The passes recorded since the last upscaling.
#[derive(Debug, Default)]
struct Journal {
    entries: Vec<Entry>,
    /// (pipeline, hash of the writes) to the entry.
    index: HashMap<(usize, u64), usize>,
    /// Passes dropped for want of room.
    overflow: u64,
}

/// The upscaling numbers to log. Unset, empty or `0` is none; `1` is the
/// 1st, 300th and 3000th; otherwise a comma list of positive numbers.
fn parse(raw: &str) -> Vec<u64> {
    let raw = raw.trim();
    if raw.is_empty() || raw == "0" {
        return Vec::new();
    }
    if raw == "1" {
        return vec![1, 300, 3000];
    }
    // anything that is not a positive number is skipped, not an error
    raw.split(',').filter_map(|s| s.trim().parse::<u64>().ok()).filter(|&n| n > 0).collect()
}

fn samples() -> &'static [u64] {
    SAMPLES.get_or_init(|| {
        let raw = std::env::var("VICTORY_RR_TARGETS").unwrap_or_default();
        let list = parse(&raw);
        if list.is_empty() && !matches!(raw.trim(), "" | "0") {
            tracing::warn!("VICTORY_RR_TARGETS={raw:?} names no upscaling; the target log stays off");
        }
        list
    })
}

/// Whether the target log is on (read once).
pub(crate) fn on() -> bool {
    !samples().is_empty()
}

/// Locks `mutex`, going on with what a panicked holder left.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// A hash of a pass's writes, so a repeat of the same pass is found without
/// describing its textures again.
fn fnv1a(writes: &[(usize, How)]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (resource, how) in writes {
        for b in resource.to_le_bytes().into_iter().chain([*how as u8]) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// Size and DXGI format of a title texture's host copy, if it has one (a
/// buffer, or a texture not made yet, has none).
fn describe(resource: usize) -> Option<([u32; 2], i32)> {
    let made = crate::resource::made_copy(resource as This)?;
    // SAFETY: the host copy is held by the `Arc` for the call.
    let desc = unsafe { made.texture.GetDesc() };
    Some(([desc.Width as u32, desc.Height], desc.Format.0))
}

/// A draw or dispatch of pipeline `p` is being recorded on `list`: opens its
/// entry with the render targets and depth it writes (`targets`, `depth`;
/// 0 is none; both are ignored for a compute pass, since the state still
/// holds an earlier draw's). Unordered writes arrive by [`unordered`] until
/// the thread's next pass or the upscaling closes it. Any recording thread.
pub(crate) fn pass(
    list: &crate::host::HostList,
    p: &crate::pipeline::host::HostPipeline,
    compute: bool,
    depth: usize,
    targets: &[usize],
) {
    if !on() {
        return;
    }
    commit();
    let key = std::ptr::from_ref(p) as usize;
    let cached = lock(&LABELS).as_ref().and_then(|m| m.get(&key).cloned());
    let label = match cached {
        Some(label) => label,
        None => {
            // built outside the lock: reading a program's name walks its container
            let names: Vec<String> = p.stages.iter().filter_map(|s| s.shader.program_name()).collect();
            let names = if names.is_empty() { "-".to_string() } else { names.join("/") };
            let label: Arc<str> = format!("{names} {}", p.name).into();
            lock(&LABELS).get_or_insert_with(HashMap::new).insert(key, label.clone());
            label
        }
    };
    let mut writes: Vec<(usize, How)> = Vec::new();
    if !compute {
        for &t in targets.iter().filter(|&&t| t != 0) {
            if !writes.contains(&(t, How::Target)) {
                writes.push((t, How::Target));
            }
        }
        // a bound depth view, not proof of a write: the pass's depth-write
        // mask is not known here, so forward and sky passes that only test
        // against it are listed too
        if depth != 0 {
            writes.push((depth, How::Depth));
        }
    }
    let open = Open { list: std::ptr::from_ref(list) as usize, pipeline: key, label, compute, writes };
    OPEN.with(|o| *o.borrow_mut() = Some(open));
}

/// A resource the pass being recorded on this thread writes through an
/// unordered view (buffers arrive too and are dropped when the pass is
/// stored, as they have no host texture).
pub(crate) fn unordered(resource: This) {
    if !on() || resource.is_null() {
        return;
    }
    OPEN.with(|o| {
        if let Some(open) = o.borrow_mut().as_mut() {
            let write = (resource as usize, How::Unordered);
            if !open.writes.contains(&write) {
                open.writes.push(write);
            }
        }
    });
}

/// Stores this thread's open pass, if it has one.
fn commit() {
    let Some(open) = OPEN.with(|o| o.borrow_mut().take()) else { return };
    commit_open(&JOURNAL, &SEQ, crate::host::thread_id(), open, describe);
}

/// Stores `open` in `journal`: a repeat of a pass already there only counts;
/// a new one has its textures described (outside the lock, as describing
/// asks the host) and is dropped if none of them is a texture.
fn commit_open(
    journal: &Mutex<Journal>,
    seq: &AtomicU64,
    thread: u32,
    open: Open,
    describe: impl Fn(usize) -> Option<([u32; 2], i32)>,
) {
    let key = (open.pipeline, fnv1a(&open.writes));
    {
        let mut j = lock(journal);
        if let Some(&at) = j.index.get(&key) {
            let e = &mut j.entries[at];
            e.count = e.count.saturating_add(1);
            e.last = seq.fetch_add(1, Relaxed);
            return;
        }
        if j.entries.len() >= CAP {
            j.overflow += 1;
            return;
        }
    }
    let writes: Vec<Written> = open
        .writes
        .iter()
        .filter_map(|&(resource, how)| describe(resource).map(|(size, format)| Written { resource, how, size, format }))
        .collect();
    if writes.is_empty() {
        return;
    }
    let mut j = lock(journal);
    // another thread may have stored the same pass while this one described it
    if let Some(&at) = j.index.get(&key) {
        let e = &mut j.entries[at];
        e.count = e.count.saturating_add(1);
        e.last = seq.fetch_add(1, Relaxed);
        return;
    }
    if j.entries.len() >= CAP {
        j.overflow += 1;
        return;
    }
    let first = seq.fetch_add(1, Relaxed);
    let at = j.entries.len();
    j.entries.push(Entry {
        first,
        last: first,
        count: 1,
        thread,
        list: open.list,
        label: open.label,
        compute: open.compute,
        writes,
    });
    j.index.insert(key, at);
}

/// Whether two sizes are the same give or take a pixel a side.
fn near(a: [u32; 2], b: [u32; 2]) -> bool {
    a[0].abs_diff(b[0]) <= 1 && a[1].abs_diff(b[1]) <= 1
}

/// Whether `size` is the render size, the output size or half of either.
fn interesting(size: [u32; 2], render: [u32; 2], output: [u32; 2]) -> bool {
    let half = |s: [u32; 2]| [s[0] / 2, s[1] / 2];
    near(size, render) || near(size, output) || near(size, half(render)) || near(size, half(output))
}

/// The upscaling `id` on `host` is being recorded: closes this thread's
/// pass, counts the upscaling, empties the journal (each report is the
/// recording since the last upscaling) and, at a chosen number, logs it.
/// The title's recording thread that makes the upscale call.
pub(crate) fn upscaled(host: &crate::host::HostList, up: &Upscale, id: u64) {
    if !on() {
        return;
    }
    commit();
    let n = UPSCALINGS.fetch_add(1, Relaxed) + 1;
    let window = std::mem::take(&mut *lock(&JOURNAL));
    if !samples().contains(&n) {
        return;
    }
    report(host, up, id, n, window);
}

fn report(host: &crate::host::HostList, up: &Upscale, id: u64, n: u64, mut window: Journal) {
    let known: [(&str, usize); 4] = [
        ("colour", up.resources[COLOUR].0 as usize),
        ("depth", up.resources[DEPTH].0 as usize),
        ("motion", up.resources[MOTION].0 as usize),
        ("output", up.resources[OUTPUT].0 as usize),
    ];
    let host_at = std::ptr::from_ref(host) as usize;
    let (render, output) = (up.camera.render, up.camera.output);

    // the header: the upscaling's own textures as the host has them
    let mut head = format!(
        "RR targets: upscaling {n} (id {id}) render {}x{} output {}x{} list {host_at:#x}, {} passes, {} dropped for room",
        render[0], render[1], output[0], output[1], window.entries.len(), window.overflow
    );
    let mut own: Vec<(&str, *const crate::object::Object)> =
        vec![("colour", up.resources[COLOUR].0), ("depth", up.resources[DEPTH].0), ("motion", up.resources[MOTION].0), ("output", up.resources[OUTPUT].0)];
    for (name, i) in [("exposure", super::EXPOSURE), ("reactive", super::REACTIVE)] {
        if !up.resources[i].0.is_null() {
            own.push((name, up.resources[i].0));
        }
    }
    for (name, r) in own {
        match describe(r as usize) {
            Some((s, f)) => {
                let _ = write!(head, "; {name} {:#x} {}x{} f{f}", r as usize, s[0], s[1]);
            }
            None => {
                let _ = write!(head, "; {name} {:#x} gone", r as usize);
            }
        }
    }
    tracing::info!("{head}");

    // the passes that wrote something of a size the guides would have
    window.entries.sort_by_key(|e| e.first);
    let is_known = |r: usize| known.iter().any(|&(_, k)| k != 0 && k == r);
    let shown: Vec<&Entry> = window
        .entries
        .iter()
        .filter(|e| e.writes.iter().any(|w| interesting(w.size, render, output) || is_known(w.resource)))
        .collect();
    for e in shown.iter().take(LINES) {
        let binds_depth = e
            .writes
            .iter()
            .any(|w| w.how == How::Depth && w.resource == known[1].1 && known[1].1 != 0);
        let mut line = format!(
            "RR targets: [{}] thread {} list {:#x}{} {} x{}: {}{} -> ",
            e.first,
            e.thread,
            e.list,
            if e.list == host_at { " (the upscaling's list)" } else { "" },
            if e.compute { "dispatch" } else { "draw" },
            e.count,
            if binds_depth { "[binds the upscaling's depth] " } else { "" },
            e.label,
        );
        for (i, w) in e.writes.iter().enumerate() {
            if i > 0 {
                line.push_str(", ");
            }
            let _ = write!(line, "{:#x} {}x{} f{} {}", w.resource, w.size[0], w.size[1], w.format, w.how.name());
            for &(tag, k) in &known {
                if k != 0 && k == w.resource {
                    let _ = write!(line, " [{tag}]");
                }
            }
        }
        tracing::info!("{line}");
    }
    tracing::info!(
        "RR targets: {} passes listed of {} that wrote at those sizes ({} more not listed), {} of {} passes not shown",
        shown.len().min(LINES),
        shown.len(),
        shown.len().saturating_sub(LINES),
        window.entries.len() - shown.len(),
        window.entries.len(),
    );

    // the candidate guide buffers, one line a size: distinct textures in
    // first-write order with the pass that wrote each first
    for (what, size) in [("render", render), ("output", output)] {
        let mut order: Vec<(usize, i32, Arc<str>, u32)> = Vec::new();
        for e in &window.entries {
            for w in e.writes.iter().filter(|w| near(w.size, size)) {
                match order.iter_mut().find(|o| o.0 == w.resource) {
                    Some(o) => o.3 += 1,
                    None => order.push((w.resource, w.format, e.label.clone(), 1)),
                }
            }
        }
        let list: Vec<String> =
            order.iter().map(|(r, f, label, c)| format!("{r:#x} f{f} first by {label}, {c} writers")).collect();
        tracing::info!(
            "RR targets: written at {what} size {}x{}: {}",
            size[0],
            size[1],
            if list.is_empty() { "none".to_string() } else { list.join("; ") }
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_parsing() {
        assert_eq!(parse("1"), vec![1, 300, 3000]);
        assert_eq!(parse("5,x,9"), vec![5, 9]);
        assert_eq!(parse(" 1200 , 5000 "), vec![1200, 5000]);
        assert!(parse("0").is_empty());
        assert!(parse("").is_empty());
        assert!(parse("x,0,-3").is_empty());
    }

    fn open(pipeline: usize, writes: &[(usize, How)]) -> Open {
        Open { list: 1, pipeline, label: "p".into(), compute: false, writes: writes.to_vec() }
    }

    /// only addresses below 0x100 are textures
    fn made_up(r: usize) -> Option<([u32; 2], i32)> {
        (r < 0x100).then_some(([1706, 960], 10))
    }

    #[test]
    fn repeats_count_and_new_passes_describe() {
        let journal = Mutex::new(Journal::default());
        let seq = AtomicU64::new(0);
        let w = [(0x10, How::Target), (0x20, How::Depth)];
        commit_open(&journal, &seq, 7, open(1, &w), made_up);
        commit_open(&journal, &seq, 7, open(1, &w), made_up);
        // same pipeline, other textures: a second entry
        commit_open(&journal, &seq, 7, open(1, &[(0x30, How::Target)]), made_up);
        let j = journal.lock().unwrap();
        assert_eq!(j.entries.len(), 2);
        assert_eq!(j.entries[0].count, 2);
        assert!(j.entries[0].last > j.entries[0].first);
        assert_eq!(j.entries[0].writes[0].size, [1706, 960]);
        assert_eq!(j.entries[1].count, 1);
    }

    #[test]
    fn buffers_are_dropped_and_empty_passes_not_stored() {
        let journal = Mutex::new(Journal::default());
        let seq = AtomicU64::new(0);
        commit_open(&journal, &seq, 1, open(2, &[(0x1000, How::Unordered), (0x10, How::Unordered)]), made_up);
        commit_open(&journal, &seq, 1, open(3, &[(0x1000, How::Unordered)]), made_up);
        let j = journal.lock().unwrap();
        assert_eq!(j.entries.len(), 1);
        assert_eq!(j.entries[0].writes.len(), 1);
    }

    #[test]
    fn size_filter() {
        assert!(interesting([853, 480], [1706, 960], [2560, 1440]));
        assert!(interesting([1706, 961], [1706, 960], [2560, 1440]));
        assert!(!interesting([512, 512], [1706, 960], [2560, 1440]));
    }
}
