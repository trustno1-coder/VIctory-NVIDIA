//! Fewer compares of big copies in the card's memory (`VICTORY_LAZY_COMPARE_MB=<MiB>`,
//! off by default).
//!
//! A copy whose shader writes are declared is compared on the card after
//! every submission, every declared piece. For the constants heap
//! (174 MB) that is 2.2 GB of the card's reads a frame to find about
//! 2.6 MB that shaders wrote. With this switch, of a copy at least that
//! large, a submission compares only:
//!
//! - the pieces found written lately ("hot", for `VICTORY_LAZY_HOT`
//!   compares, 64 by default, after the last time one was found written);
//! - the pieces about to be sent up that were left out of a compare since
//!   their last one ("forced": they are not hashed again when sent, so the
//!   compare still sees what shaders wrote there before);
//! - the next `VICTORY_LAZY_SWEEP` pieces (128 by default) of those left
//!   out, in turn, so a piece written for the first time is found within a
//!   sweep of the copy.
//!
//! The other declared pieces are left out and marked pending; their hashes
//! stay as they were, so what shaders wrote there is found when the sweep
//! or a send reaches them. These compares store the hashes they make
//! (mode 2), so a piece the CPU sent up and nothing wrote is not found
//! again. What is found is fetched and merged as before: a merge takes only
//! the bytes that differ from the CPU's side, so a piece found that nothing
//! wrote costs a fetch and nothing else. Left out: shader bytes in a 4 KiB
//! page that the CPU sends up before the piece's compare are lost.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::shadow::{CardSpan, Shadow};

/// The smallest copy (bytes) taken this way, if on.
pub(super) fn min() -> Option<u64> {
    static MIN: OnceLock<Option<u64>> = OnceLock::new();
    *MIN.get_or_init(|| {
        let mb = std::env::var("VICTORY_LAZY_COMPARE_MB").ok()?.trim().parse::<u64>().ok().filter(|&mb| mb > 0)?;
        tracing::info!(
            mb,
            hot = hot_life(),
            sweep = sweep_len(),
            "copies in the card's memory this large are compared only where written lately, before a send, and in a sweep \
             (VICTORY_LAZY_COMPARE_MB)"
        );
        Some(mb << 20)
    })
}

fn hot_life() -> u8 {
    static N: OnceLock<u8> = OnceLock::new();
    *N.get_or_init(|| std::env::var("VICTORY_LAZY_HOT").ok().and_then(|v| v.trim().parse().ok()).filter(|&n| n > 0).unwrap_or(64))
}

fn sweep_len() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| std::env::var("VICTORY_LAZY_SWEEP").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(128))
}

/// Whether what flights may write of such a copy counts, for the steps that
/// wait for flights or are done on the card in their order, as only its
/// pieces written lately (`VICTORY_LAZY_NARROW=1`, with
/// `VICTORY_LAZY_COMPARE_MB`). The skinning passes declare the whole
/// constants heap written; every constant read of it then waited or was
/// done on the card (5.5 ms a frame of planning on the submitting thread).
/// A piece written for the first time is read as the title has it until the
/// sweep finds it, as its write-back is late until then anyway.
pub(super) fn narrow() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = min().is_some() && std::env::var_os("VICTORY_LAZY_NARROW").is_some_and(|v| v == "1");
        if on {
            tracing::info!("flights' writes to big card copies count only where written lately (VICTORY_LAZY_NARROW)");
        }
        on
    })
}

/// Of the title range `[from, to)` of `shadow`: the parts in pieces written
/// lately, as title ranges.
pub(super) fn hot_within(shadow: &Shadow, from: usize, to: usize, out: &mut Vec<(usize, usize)>) {
    let start = shadow.span().0 as usize;
    let len = shadow.span().1 as usize;
    let (lo, hi) = (from.max(start) - start, to.min(start + len).saturating_sub(start));
    if lo >= hi {
        return;
    }
    let (first, last) = (shadow.card_piece_of(lo), shadow.card_piece_of(hi - 1));
    let states = STATES.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = states.as_ref().and_then(|s| s.get(&shadow.id())) else {
        return;
    };
    for piece in first..=last {
        if state.hot.get(piece as usize).is_some_and(|&h| h > 0) {
            let (a, b) = piece_offsets(shadow, piece);
            let (a, b) = (a.max(lo) + start, b.min(hi) + start);
            match out.last_mut() {
                // (`out` may hold other ranges already, above this one: the
                // callers sort and join after.)
                Some(last) if last.0 <= a && last.1 >= a => last.1 = last.1.max(b),
                _ => out.push((a, b)),
            }
        }
    }
}

/// Whether `shadow` is compared this way.
pub(super) fn on(shadow: &Shadow) -> bool {
    min().is_some_and(|min| shadow.span().1 >= min)
}

#[derive(Default)]
struct State {
    /// Per piece: compares left while it counts as written lately.
    hot: Vec<u8>,
    /// Per piece: left out of a compare since it was last compared.
    pending: Vec<bool>,
    /// Per piece: pending and sent up without being hashed again.
    forced: Vec<bool>,
    cursor: usize,
}

impl State {
    fn sized(&mut self, pieces: usize) {
        if self.hot.len() < pieces {
            self.hot.resize(pieces, 0);
            self.pending.resize(pieces, false);
            self.forced.resize(pieces, false);
        }
    }
}

static STATES: Mutex<Option<HashMap<u64, State>>> = Mutex::new(None);

/// Since the last line: pieces compared (hot, forced, swept), left out, found.
static STATS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

fn with<R>(shadow: &Shadow, f: impl FnOnce(&mut State) -> R) -> R {
    let pieces = pieces_of(shadow);
    let mut states = STATES.lock().unwrap_or_else(|e| e.into_inner());
    let state = states.get_or_insert_with(HashMap::new).entry(shadow.id()).or_default();
    state.sized(pieces);
    f(state)
}

/// How many pieces the copy has (piece 0 holds offset 0).
fn pieces_of(shadow: &Shadow) -> usize {
    let len = shadow.span().1 as usize;
    if len == 0 { 0 } else { shadow.card_piece_of(len - 1) as usize + 1 }
}

/// The offsets of the copy's piece `piece`.
fn piece_offsets(shadow: &Shadow, piece: u32) -> (usize, usize) {
    let full = shadow.card_run(0, usize::MAX);
    shadow.card_piece(&full, piece - full.first)
}

/// Contiguous runs of the sorted, unique `pieces`, as spans of the copy.
fn runs_of(shadow: &Shadow, pieces: &[u32]) -> Vec<CardSpan> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < pieces.len() {
        let mut j = i + 1;
        while j < pieces.len() && pieces[j] == pieces[j - 1] + 1 {
            j += 1;
        }
        let lo = piece_offsets(shadow, pieces[i]).0;
        let hi = piece_offsets(shadow, pieces[j - 1]).1;
        if lo < hi {
            out.push(shadow.card_run(lo, hi));
        }
        i = j;
    }
    out
}

/// The hash passes after a send: of `runs`, the pending pieces are left
/// out (and forced into this submission's compare).
pub(super) fn hold_back(shadow: &Shadow, runs: Vec<CardSpan>) -> Vec<CardSpan> {
    let mut kept: Vec<u32> = with(shadow, |state| {
        let mut kept = Vec::new();
        for run in &runs {
            for piece in run.first..run.first + run.pieces {
                let p = piece as usize;
                if state.pending.get(p).copied().unwrap_or(false) {
                    state.forced[p] = true;
                } else {
                    kept.push(piece);
                }
            }
        }
        kept
    });
    kept.sort_unstable();
    kept.dedup();
    runs_of(shadow, &kept)
}

/// Of a declared `span` of the copy: the runs `[i0, i1)` of its pieces (by
/// index in the span) to compare now; the others are marked pending.
pub(super) fn select(shadow: &Shadow, span: &CardSpan) -> Vec<(u32, u32)> {
    with(shadow, |state| {
        let mut out: Vec<(u32, u32)> = Vec::new();
        let (mut now, mut left) = (0u64, 0u64);
        for i in 0..span.pieces {
            let p = (span.first + i) as usize;
            let Some(&hot) = state.hot.get(p) else {
                continue;
            };
            if hot > 0 || state.forced[p] {
                state.pending[p] = false;
                state.forced[p] = false;
                now += 1;
                match out.last_mut() {
                    Some(last) if last.1 == i => last.1 = i + 1,
                    _ => out.push((i, i + 1)),
                }
            } else {
                state.pending[p] = true;
                left += 1;
            }
        }
        STATS[0].fetch_add(now, Ordering::Relaxed);
        STATS[3].fetch_add(left, Ordering::Relaxed);
        out
    })
}

/// After the declared spans: the copy's forced pieces still pending, and the
/// next pending pieces of the sweep, as spans to compare now.
pub(super) fn sweep(shadow: &Shadow) -> Vec<CardSpan> {
    let pieces: Vec<u32> = with(shadow, |state| {
        let n = state.pending.len();
        let mut out = Vec::new();
        for p in 0..n {
            if state.forced[p] {
                state.forced[p] = false;
                state.pending[p] = false;
                out.push(p as u32);
            }
        }
        STATS[1].fetch_add(out.len() as u64, Ordering::Relaxed);
        let (mut taken, mut looked) = (0, 0);
        while taken < sweep_len() && looked < n {
            let p = state.cursor % n.max(1);
            state.cursor = (p + 1) % n.max(1);
            looked += 1;
            if state.pending[p] {
                state.pending[p] = false;
                out.push(p as u32);
                taken += 1;
            }
        }
        STATS[2].fetch_add(taken as u64, Ordering::Relaxed);
        out
    });
    let mut pieces = pieces;
    pieces.sort_unstable();
    pieces.dedup();
    runs_of(shadow, &pieces)
}

/// A compared piece of the copy was found written (`flagged`) or not.
pub(super) fn found(shadow: &Shadow, span: &CardSpan, flagged: impl Fn(u32) -> bool) {
    let life = hot_life();
    let mut hits = 0;
    with(shadow, |state| {
        for i in 0..span.pieces {
            let p = (span.first + i) as usize;
            let Some(hot) = state.hot.get_mut(p) else {
                continue;
            };
            if flagged(i) {
                *hot = life;
                hits += 1;
            } else {
                *hot = hot.saturating_sub(1);
            }
        }
    });
    STATS[4].fetch_add(hits, Ordering::Relaxed);
    log();
}

fn log() {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    if last.is_some_and(|t| now.duration_since(t).as_secs() < 5) {
        return;
    }
    let first = last.is_none();
    *last = Some(now);
    if first {
        return;
    }
    let [hot, forced, swept, left, found] = std::array::from_fn(|i| STATS[i].swap(0, Ordering::Relaxed));
    let hot_now: usize = STATES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or(0, |s| s.values().map(|s| s.hot.iter().filter(|&&h| h > 0).count()).sum());
    tracing::info!(
        hot,
        forced,
        swept,
        left,
        found,
        hot_now,
        "lazy compares in the last 5 s (VICTORY_LAZY_COMPARE_MB): pieces compared as written lately, before a send, in the \
         sweep; left out; found written; written lately now"
    );
}
