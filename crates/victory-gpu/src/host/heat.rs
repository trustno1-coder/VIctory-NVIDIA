//! Compares that read only the pieces likely to have changed
//! (`VICTORY_CARD_COMPARE_HOT=<D>`; off by default).
//!
//! After each submission the card compares every 64 KiB piece of every range
//! its shaders were declared to write (`CARD_COMPARE`). Walking in the prison
//! that is 7,600-13,400 pieces a frame (500-880 MB read) of which 90-95 % are
//! found unchanged, and the same few pieces are found changed frame after
//! frame. With the switch a submission's compare reads only the declared
//! pieces found changed lately ("hot": [`hot_span`] submissions); the others
//! are "owed": their stored hash is still the one from before this
//! submission's shaders ran, so a later compare of them still finds what the
//! shaders wrote, however many submissions wrote it meanwhile.
//!
//! What may not happen to an owed piece, and what this book does about it:
//!
//! - **its stored hash replaced before a compare has looked at it**: the hash
//!   passes after ranges are sent up (mode 0) would take the shaders' bytes
//!   into the hash, and no compare would ever flag them. A submission whose
//!   sends hash an owed piece again compares it first, in a list of its own
//!   ahead of the sends ("a cover"). The same for an owed piece that the
//!   submission's own compare would read (it stores its hash too: what it
//!   flags lands with this submission, not with the one that owes it).
//! - **the title told before what the shaders wrote is in title memory**:
//!   fences and bottom-of-pipe values are told when a flight lands, and the
//!   title may then read or rewrite those bytes. A flight that owes pieces
//!   does not land before a cover has compared all of them and that cover has
//!   run on the card ([`Book::hold`]); the cover's results land with it. Every
//!   point that waits for flights to land (a synchronous submission, a read of
//!   title memory a flight may write, a blocking fence wait, the lists in the
//!   air, the sweep) waits for that too: nothing changes for them.
//! - **owed for ever**: a piece owed `D` submissions is compared by the cover
//!   of the `D`-th one after; and the thread that lands flights compares every
//!   owed piece itself when the card has run everything in the line or a
//!   thread waits for landings ([`Book::take_all`]), so a title that stops
//!   submitting still has every flight land.
//!
//! The book is pure: the host keeps it under a lock, records the covers it
//! asks for and tells it what compares found.

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// `VICTORY_CARD_COMPARE_HOT=<D>`: the most submissions after its own that a
/// piece a submission skipped waits for its compare; unset or 0, every
/// declared piece is compared after every submission, as before. The flight
/// that owes it lands no earlier than that compare has run, so its fences are
/// told up to `D` submissions later (a frame is some 30).
pub(super) fn deadline() -> Option<u64> {
    static D: OnceLock<Option<u64>> = OnceLock::new();
    *D.get_or_init(|| {
        std::env::var("VICTORY_CARD_COMPARE_HOT").ok().and_then(|v| v.trim().parse::<u64>().ok()).filter(|&d| d > 0)
    })
}

/// `VICTORY_CARD_COMPARE_HOT_SPAN` (90, about three frames): for how many
/// submissions after a compare found a piece changed the piece is compared
/// after every submission that declares it.
pub(super) fn hot_span() -> u64 {
    static SPAN: OnceLock<u64> = OnceLock::new();
    *SPAN.get_or_init(|| {
        std::env::var("VICTORY_CARD_COMPARE_HOT_SPAN").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(90)
    })
}

/// `VICTORY_CARD_COMPARE_EVERY=<D>` (off by default; not with
/// `VICTORY_CARD_COMPARE_HOT`): a declared range of a copy is read whole by
/// a compare at most once in `D` submissions; the compares between read only
/// its pieces found changed in the last [`hot_span`] submissions ([`Lean`]).
/// Nothing is owed and no flight waits: a shader's write to a piece nobody
/// found changed lately reaches title memory with the range's next whole
/// compare, at most `D` submissions after its own flight landed.
pub(super) fn every() -> Option<u64> {
    static EVERY: OnceLock<Option<u64>> = OnceLock::new();
    *EVERY.get_or_init(|| {
        std::env::var("VICTORY_CARD_COMPARE_EVERY").ok().and_then(|v| v.trim().parse::<u64>().ok()).filter(|&d| d > 0)
    })
}

/// With [`every`]: whether the pieces a compare left that a send is about to
/// hash again are compared first ([`Lean::ahead`]; on by default,
/// `VICTORY_CARD_COMPARE_EVERY_AHEAD=0` leaves them out, to measure what they
/// cost: shaders' writes to such pieces may then never reach title memory).
pub(super) fn ahead_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CARD_COMPARE_EVERY_AHEAD").is_none_or(|v| v != "0"))
}

/// What [`every`] goes by: for each declared range the submission whose
/// compare last read all of it, and for each copy the pieces found changed
/// and when. A range is a copy, its first piece and its pieces: the title
/// declares the same ranges frame after frame.
///
/// And the ranges a compare left pieces of since their last whole read
/// (`cold`). A shader may have written such a piece, and its stored hash is
/// still the one from before: the range's next whole compare finds the
/// write. But the hash passes after title bytes are sent up (mode 0) store
/// the hash of what the piece holds then, the shader's bytes with it, and no
/// compare would flag them again: they would never reach title memory. So
/// before a submission's sends the pieces they hash again that lie in such a
/// range are compared first ([`Lean::ahead`]), each once until the range is
/// declared again: a few hundred pieces a frame, and nothing kept per piece
/// of a whole range.
#[derive(Default)]
pub(super) struct Lean {
    whole: HashMap<(u64, u32, u32), u64>,
    changed: HashMap<u64, HashMap<u32, u64>>,
    /// Per copy, per range left in part (first piece, pieces): its offsets
    /// in the copy and the pieces compared ahead of sends since it was last
    /// declared.
    cold: HashMap<u64, HashMap<(u32, u32), Cold>>,
    plans: u64,
}

/// A range a compare left pieces of ([`Lean`]).
struct Cold {
    from: u32,
    to: u32,
    read: std::collections::HashSet<u32>,
}

/// Since the last frame-rate line ([`lean_line`]): declared ranges read
/// whole, ranges read in their hot pieces only, the pieces read of those,
/// the pieces left; pieces compared ahead of sends that hash them again
/// ([`Lean::ahead`]), of those found changed, and fetched whole for want of
/// room in the compare's results.
pub(super) static LEAN_STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

pub(super) mod lean_stat {
    pub(crate) const AHEAD: usize = 4;
    pub(crate) const AHEAD_FOUND: usize = 5;
    pub(crate) const AHEAD_UNCOMPARED: usize = 6;
}

impl Lean {
    /// What submission `number`'s compare reads of the declared range
    /// (`copy`, from piece `first`, `pieces` of them, offsets `bytes` of the
    /// copy): `None` for all of it, else each piece's yes or no. Called
    /// after [`Lean::ahead`] for the same submission's sends.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn plan(
        &mut self,
        number: u64,
        every: u64,
        span: u64,
        copy: u64,
        first: u32,
        pieces: u32,
        bytes: (u32, u32),
    ) -> Option<Vec<bool>> {
        self.plans += 1;
        if self.plans.is_multiple_of(1024) {
            self.prune(number, every);
        }
        let whole = self.whole.entry((copy, first, pieces)).or_insert(0);
        if *whole == 0 || *whole + every <= number {
            *whole = number;
            // (What changed long ago is forgotten at these.)
            if let Some(changed) = self.changed.get_mut(&copy) {
                changed.retain(|_, n| *n + 4 * span >= number);
            }
            self.settled(copy, first, pieces);
            LEAN_STATS[0].fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let mut chosen = vec![false; pieces as usize];
        let mut hot = 0u64;
        if let Some(changed) = self.changed.get(&copy) {
            for (&p, &n) in changed {
                if n + span >= number && p >= first && p - first < pieces {
                    chosen[(p - first) as usize] = true;
                    hot += 1;
                }
            }
        }
        if hot < u64::from(pieces) {
            // Left in part: what this submission's shaders write there is
            // compared ahead of any send that hashes it again before the
            // range is next read whole.
            let cold = self.cold.entry(copy).or_default();
            let c = cold.entry((first, pieces)).or_insert_with(|| Cold { from: bytes.0, to: bytes.1, read: Default::default() });
            (c.from, c.to) = bytes;
            c.read.clear();
        } else {
            // Read all of it after this submission's lists, as a whole read.
            self.settled(copy, first, pieces);
        }
        LEAN_STATS[1].fetch_add(1, Ordering::Relaxed);
        LEAN_STATS[2].fetch_add(hot, Ordering::Relaxed);
        LEAN_STATS[3].fetch_add(u64::from(pieces) - hot, Ordering::Relaxed);
        Some(chosen)
    }

    /// A compare reads all of the range after this submission's lists: what
    /// earlier ones left of it is found there.
    fn settled(&mut self, copy: u64, first: u32, pieces: u32) {
        if let Some(cold) = self.cold.get_mut(&copy) {
            cold.remove(&(first, pieces));
            if cold.is_empty() {
                self.cold.remove(&copy);
            }
        }
    }

    /// Ahead of a submission's sends to `copy`, which hash its pieces in
    /// `runs` again (`[a, b)` of pieces; `None`: all of them): those of them
    /// that lie in a range a compare left in part since its last whole read,
    /// and that no compare ahead of a send has read since the range was last
    /// declared, each with the offsets of the copy those ranges take (from,
    /// to: the piece's own are the caller's to cut). They are to be compared,
    /// storing their hashes, before the sends; taken as read here.
    pub(super) fn ahead(&mut self, copy: u64, runs: Option<&[(u32, u32)]>) -> Vec<(u32, u32, u32)> {
        let Some(cold) = self.cold.get_mut(&copy) else {
            return Vec::new();
        };
        let mut out: std::collections::BTreeMap<u32, (u32, u32)> = std::collections::BTreeMap::new();
        for (&(first, pieces), c) in cold.iter_mut() {
            let (lo, hi) = (first, first + pieces);
            let mut take = |p: u32| {
                if c.read.insert(p) {
                    let at = out.entry(p).or_insert((c.from, c.to));
                    *at = (at.0.min(c.from), at.1.max(c.to));
                }
            };
            match runs {
                None => (lo..hi).for_each(&mut take),
                Some(runs) => {
                    for &(a, b) in runs {
                        (a.max(lo)..b.min(hi)).for_each(&mut take);
                    }
                }
            }
        }
        let out: Vec<(u32, u32, u32)> = out.into_iter().map(|(p, (from, to))| (p, from, to)).collect();
        LEAN_STATS[lean_stat::AHEAD].fetch_add(out.len() as u64, Ordering::Relaxed);
        out
    }

    /// Ranges not declared for long go, and copies with nothing found lately
    /// (their numbers are never given again; the book would only grow). A
    /// range left in part stays until it is read whole: a write there may
    /// still be on the card only.
    fn prune(&mut self, number: u64, every: u64) {
        let cold = &self.cold;
        self.whole.retain(|&(copy, first, pieces), n| {
            *n + 64 * every >= number || cold.get(&copy).is_some_and(|c| c.contains_key(&(first, pieces)))
        });
        self.changed.retain(|_, c| !c.is_empty());
    }

    /// Submission `number`'s compare found `piece` of `copy` changed.
    pub(super) fn found(&mut self, copy: u64, piece: u32, number: u64) {
        let at = self.changed.entry(copy).or_default().entry(piece).or_insert(number);
        *at = (*at).max(number);
    }
}

/// The frame-rate line's part ([`LEAN_STATS`]), if the switch is on.
pub(super) fn lean_line(frames: f64) {
    let Some(every) = every() else {
        return;
    };
    let s: Vec<f64> = LEAN_STATS.iter().map(|n| n.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    tracing::info!(
        "declared ranges read whole once in {every} submissions (VICTORY_CARD_COMPARE_EVERY, hot for {} submissions) per frame: {:.1} read whole, \
         {:.1} read in their hot pieces only, {:.0} pieces read of those, {:.0} left; {:.1} pieces left so compared ahead of \
         sends that hash them again, {:.1} of them found changed, {:.1} fetched whole for want of room",
        hot_span(),
        s[0],
        s[1],
        s[2],
        s[3],
        s[lean_stat::AHEAD],
        s[lean_stat::AHEAD_FOUND],
        s[lean_stat::AHEAD_UNCOMPARED],
    );
}

/// Since the last frame-rate line ([`line`]): declared pieces compared as
/// hot, declared pieces left for later, owed pieces compared by covers (as
/// due, ahead of a send, ahead of the submission's own compare, by the thread
/// that lands flights), owed pieces found changed by covers, the most
/// submissions a changed one waited, the lists the landing thread made for
/// covers, the times it held a flight back for covers, microseconds it
/// waited for their compares.
pub(super) static STATS: [AtomicU64; 12] = [const { AtomicU64::new(0) }; 12];

pub(super) mod stat {
    pub(crate) const HOT: usize = 0;
    pub(crate) const LEFT: usize = 1;
    pub(crate) const DUE: usize = 2;
    pub(crate) const SENT: usize = 3;
    pub(crate) const AHEAD: usize = 4;
    pub(crate) const FORCED: usize = 5;
    pub(crate) const FOUND: usize = 6;
    /// A maximum, not a sum.
    pub(crate) const WAITED: usize = 7;
    pub(crate) const FORCED_LISTS: usize = 8;
    pub(crate) const HELD: usize = 9;
    pub(crate) const HELD_US: usize = 10;
    /// Owed pieces when the line was written (a level, not a sum).
    pub(crate) const OWED: usize = 11;
}

pub(super) fn count(what: usize, n: u64) {
    STATS[what].fetch_add(n, Ordering::Relaxed);
}

/// The frame-rate line's part ([`STATS`]), if the switch is on.
pub(super) fn line(frames: f64) {
    let Some(d) = deadline() else {
        return;
    };
    let s: Vec<u64> = STATS
        .iter()
        .enumerate()
        .map(|(i, v)| if i == stat::OWED { v.load(Ordering::Relaxed) } else { v.swap(0, Ordering::Relaxed) })
        .collect();
    let f = |i: usize| s[i] as f64 / frames;
    tracing::info!(
        "compares of hot pieces only (VICTORY_CARD_COMPARE_HOT={d}, hot for {} submissions) per frame: {:.0} declared pieces \
         compared as hot, {:.0} left for later; {:.0} owed pieces compared later ({:.0} due, {:.0} ahead of a send, {:.0} ahead of \
         their submission's own compare, {:.0} by the landing thread in {:.1} lists of its own), {:.1} of them found changed, \
         the longest a changed one waited {} submissions; the landing thread held a flight back for them {:.1} times, \
         {:.2} ms waiting for their compares; \
         {} pieces owed now",
        hot_span(),
        f(stat::HOT),
        f(stat::LEFT),
        f(stat::DUE) + f(stat::SENT) + f(stat::AHEAD) + f(stat::FORCED),
        f(stat::DUE),
        f(stat::SENT),
        f(stat::AHEAD),
        f(stat::FORCED),
        f(stat::FORCED_LISTS),
        f(stat::FOUND),
        s[stat::WAITED],
        f(stat::HELD),
        f(stat::HELD_US) / 1000.0,
        s[stat::OWED],
    );
}

/// A piece no compare has looked at since a submission that may have written
/// it: the first such submission (its flight may not land before the piece
/// is compared), and the offsets of the copy its declared ranges took of the
/// piece (what its landing merges).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Owed {
    pub(super) first: u64,
    pub(super) lo: usize,
    pub(super) hi: usize,
}

/// An owed piece taken for a cover: the copy's number, its handle, the
/// piece, and what was owed.
pub(super) type Taken<S> = (u64, S, u32, Owed);

struct Copy<S> {
    handle: S,
    /// Piece, and the submission whose compare last found it changed.
    changed: HashMap<u32, u64>,
    owed: BTreeMap<u32, Owed>,
}

struct Cover<C> {
    /// Done when the cover fence has reached it.
    value: u64,
    /// The first submission that owed one of its pieces.
    oldest: u64,
    payload: C,
}

/// One declared range of a copy in a submission, piece by piece: the piece
/// and the offsets of the copy the range takes of it.
pub(super) struct Span<S> {
    pub(super) copy: u64,
    pub(super) handle: S,
    pub(super) pieces: Vec<(u32, usize, usize)>,
}

/// What [`Book::plan`] decided for a submission.
pub(super) struct Planned<S> {
    /// The owed pieces to compare ahead of its sends (a cover).
    pub(super) covers: Vec<Taken<S>>,
    /// Per span given, per piece: whether its own compare reads it.
    pub(super) chosen: Vec<Vec<bool>>,
}

/// Why a flight may not land yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Hold {
    Free,
    /// It, or one ahead of it, owes a piece no cover has taken.
    Uncovered,
    /// A cover of a piece it owes has not run: the cover fence's value.
    Cover(u64),
}

pub(super) struct Book<S, C> {
    copies: HashMap<u64, Copy<S>>,
    /// By the first submission that owes them: pieces owed and not taken.
    uncovered: BTreeMap<u64, u32>,
    covers: Vec<Cover<C>>,
    plans: u64,
}

impl<S: Clone, C> Default for Book<S, C> {
    fn default() -> Self {
        Book { copies: HashMap::new(), uncovered: BTreeMap::new(), covers: Vec::new(), plans: 0 }
    }
}

impl<S: Clone, C> Book<S, C> {
    /// Submission `number`'s compares. `spans`: its declared ranges of copies
    /// it compares (`flown`: sent ahead, its compare stores the hashes it
    /// makes; else it reads every piece, as it always did). `rehashed`: the
    /// pieces (copy, piece) its sends hash again.
    ///
    /// In this order, because each step changes what the next may do: which
    /// declared pieces are hot; which owed pieces a cover takes now (due, or
    /// to be hashed again by the sends, or read by the submission's own
    /// compare, which stores its hash); and the declared pieces not read
    /// become owed (by this submission, if no earlier one owes them still).
    pub(super) fn plan(
        &mut self,
        number: u64,
        deadline: u64,
        span: u64,
        spans: &[Span<S>],
        rehashed: &dyn Fn(u64, u32) -> bool,
        flown: bool,
    ) -> Planned<S> {
        self.plans += 1;
        if self.plans.is_multiple_of(1024) {
            self.prune(number, span);
        }
        let chosen: Vec<Vec<bool>> = spans
            .iter()
            .map(|s| {
                let copy = self.copies.get(&s.copy);
                s.pieces
                    .iter()
                    .map(|&(p, ..)| !flown || copy.and_then(|c| c.changed.get(&p)).is_some_and(|&n| n + span >= number))
                    .collect()
            })
            .collect();
        let read: std::collections::HashSet<(u64, u32)> = spans
            .iter()
            .zip(&chosen)
            .flat_map(|(s, c)| s.pieces.iter().zip(c).filter(|(_, c)| **c).map(|(p, _)| (s.copy, p.0)))
            .collect();
        let mut covers = Vec::new();
        for (&copy, state) in &mut self.copies {
            let taken: Vec<u32> = state
                .owed
                .iter()
                .filter(|(p, o)| {
                    let why = if o.first + deadline <= number {
                        Some(stat::DUE)
                    } else if rehashed(copy, **p) {
                        Some(stat::SENT)
                    } else if read.contains(&(copy, **p)) {
                        Some(stat::AHEAD)
                    } else {
                        None
                    };
                    why.inspect(|&w| count(w, 1)).is_some()
                })
                .map(|(p, _)| *p)
                .collect();
            for p in taken {
                let o = state.owed.remove(&p).expect("taken from the map");
                uncount(&mut self.uncovered, o.first);
                covers.push((copy, state.handle.clone(), p, o));
            }
        }
        for (s, c) in spans.iter().zip(&chosen) {
            let state = self.copies.entry(s.copy).or_insert_with(|| Copy {
                handle: s.handle.clone(),
                changed: HashMap::new(),
                owed: BTreeMap::new(),
            });
            state.handle = s.handle.clone();
            for (&(p, lo, hi), &chosen) in s.pieces.iter().zip(c) {
                if chosen {
                    count(stat::HOT, 1);
                    continue;
                }
                count(stat::LEFT, 1);
                match state.owed.get_mut(&p) {
                    Some(o) => (o.lo, o.hi) = (o.lo.min(lo), o.hi.max(hi)),
                    None => {
                        state.owed.insert(p, Owed { first: number, lo, hi });
                        *self.uncovered.entry(number).or_insert(0) += 1;
                    }
                }
            }
        }
        STATS[stat::OWED].store(self.owed() as u64, Ordering::Relaxed);
        Planned { covers, chosen }
    }

    /// Every owed piece, taken for a cover the thread that lands flights
    /// makes.
    pub(super) fn take_all(&mut self) -> Vec<Taken<S>> {
        let mut out = Vec::new();
        for (&copy, state) in &mut self.copies {
            for (p, o) in std::mem::take(&mut state.owed) {
                out.push((copy, state.handle.clone(), p, o));
            }
        }
        self.uncovered.clear();
        count(stat::FORCED, out.len() as u64);
        STATS[stat::OWED].store(0, Ordering::Relaxed);
        out
    }

    /// A cover of `taken` is on the queue, done when the cover fence reaches
    /// `value`; `payload` lands with the first flight that owed one of them.
    pub(super) fn add_cover(&mut self, value: u64, taken: &[Taken<S>], payload: C) {
        let oldest = taken.iter().map(|t| t.3.first).min().unwrap_or(0);
        self.covers.push(Cover { value, oldest, payload });
    }

    /// A compare of submission `number` found the piece changed.
    pub(super) fn found(&mut self, copy: u64, piece: u32, number: u64) {
        if let Some(state) = self.copies.get_mut(&copy) {
            let at = state.changed.entry(piece).or_insert(number);
            *at = (*at).max(number);
        }
    }

    /// Whether flight `flight` may land, the cover fence having reached
    /// `completed`. (Flights land in order: one ahead of it that owes is
    /// held, and so it is.)
    pub(super) fn hold(&self, flight: u64, completed: u64) -> Hold {
        if self.uncovered.range(..=flight).next().is_some() {
            return Hold::Uncovered;
        }
        match self.covers.iter().filter(|c| c.oldest <= flight && c.value > completed).map(|c| c.value).max() {
            Some(value) => Hold::Cover(value),
            None => Hold::Free,
        }
    }

    /// The covers that land with flights up to `last` (their first owing
    /// flight is one of them), done by `completed`.
    pub(super) fn take_landed(&mut self, last: u64, completed: u64) -> Vec<C> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.covers.len() {
            if self.covers[i].oldest <= last && self.covers[i].value <= completed {
                out.push(self.covers.swap_remove(i).payload);
            } else {
                i += 1;
            }
        }
        out
    }

    /// Pieces owed and not taken.
    pub(super) fn owed(&self) -> usize {
        self.uncovered.values().map(|&n| n as usize).sum()
    }

    /// Covers not landed yet, and the first submission that owed a piece of
    /// the oldest of them.
    pub(super) fn waiting(&self) -> (usize, Option<u64>) {
        (self.covers.len(), self.covers.iter().map(|c| c.oldest).min())
    }

    /// Copies with nothing owed and nothing found lately go (their numbers
    /// are never given again; the book would only grow).
    fn prune(&mut self, number: u64, span: u64) {
        self.copies.retain(|_, c| {
            c.changed.retain(|_, n| *n + span >= number);
            !c.changed.is_empty() || !c.owed.is_empty()
        });
    }
}

fn uncount(uncovered: &mut BTreeMap<u64, u32>, first: u64) {
    if let Some(n) = uncovered.get_mut(&first) {
        *n -= 1;
        if *n == 0 {
            uncovered.remove(&first);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{Book, Hold, Span};

    fn span(copy: u64, pieces: std::ops::Range<u32>) -> Span<()> {
        Span { copy, handle: (), pieces: pieces.map(|p| (p, p as usize * 65536, (p as usize + 1) * 65536)).collect() }
    }

    /// `Lean::plan`: a declared range is read whole the first time and once
    /// in so many submissions after; between, only its pieces found changed
    /// within the span.
    #[test]
    fn a_range_is_read_whole_once_in_so_many_submissions_and_in_its_hot_pieces_between() {
        let mut lean = super::Lean::default();
        // (The offsets of the range: only `Lean::ahead` gives them back.)
        const B: (u32, u32) = (0, 0);
        // The first time, and again when the count has passed: whole.
        assert_eq!(lean.plan(10, 30, 90, 7, 100, 8, B), None);
        assert_eq!(lean.plan(11, 30, 90, 7, 100, 8, B), Some(vec![false; 8]), "nothing found changed yet");
        // Another range of the same copy has its own count.
        assert_eq!(lean.plan(11, 30, 90, 7, 0, 4, B), None);
        // A piece found changed is read by every compare for the span, in the ranges that hold it.
        lean.found(7, 103, 11);
        lean.found(7, 2, 11);
        let mut hot = vec![false; 8];
        hot[3] = true;
        assert_eq!(lean.plan(12, 30, 90, 7, 100, 8, B), Some(hot.clone()));
        assert_eq!(lean.plan(39, 30, 90, 7, 100, 8, B), Some(hot.clone()));
        assert_eq!(lean.plan(40, 30, 90, 7, 100, 8, B), None, "thirty submissions after the last whole one");
        assert_eq!(lean.plan(41, 30, 90, 7, 100, 8, B), Some(hot));
        // Past the span it is cold again.
        assert_eq!(lean.plan(41 + 61, 30, 90, 7, 100, 8, B), None);
        assert_eq!(lean.plan(41 + 62, 30, 90, 7, 100, 8, B), Some(vec![false; 8]));
        // Another copy knows nothing of it.
        assert_eq!(lean.plan(12, 30, 90, 8, 100, 8, B), None);
    }

    /// A piece found changed is read after every submission that declares
    /// it while it is hot, the others are owed and their flight is held
    /// until a cover took them and ran.
    #[test]
    fn hot_pieces_are_read_and_cold_ones_owed_until_covered() {
        let mut book: Book<(), &str> = Book::default();
        let none = |_: u64, _: u32| false;
        // Submission 1: nothing known, everything owed.
        let p = book.plan(1, 4, 10, &[span(7, 0..4)], &none, true);
        assert!(p.covers.is_empty());
        assert_eq!(p.chosen, vec![vec![false; 4]]);
        assert_eq!(book.owed(), 4);
        assert_eq!(book.hold(1, 0), Hold::Uncovered);
        // Submission 2 declares nothing; piece 2 of submission 1's range is
        // hashed again by its sends: taken first.
        let p = book.plan(2, 4, 10, &[], &|c, p| c == 7 && p == 2, true);
        assert_eq!(p.covers.iter().map(|t| t.2).collect::<Vec<_>>(), vec![2]);
        book.add_cover(1, &p.covers, "two");
        assert_eq!(book.hold(1, 0), Hold::Uncovered, "three pieces still owed");
        // Submission 5 is submission 1's deadline: the rest are taken.
        let p = book.plan(5, 4, 10, &[], &none, true);
        let mut taken: Vec<u32> = p.covers.iter().map(|t| t.2).collect();
        taken.sort_unstable();
        assert_eq!(taken, vec![0, 1, 3]);
        book.add_cover(2, &p.covers, "five");
        assert_eq!(book.owed(), 0);
        assert_eq!(book.hold(1, 0), Hold::Cover(2));
        assert_eq!(book.hold(1, 1), Hold::Cover(2));
        assert_eq!(book.hold(1, 2), Hold::Free);
        assert!(book.take_landed(1, 1).contains(&"two"));
        assert_eq!(book.take_landed(1, 2), vec!["five"]);
        // The cover found piece 3 changed: hot for ten submissions.
        book.found(7, 3, 5);
        let p = book.plan(6, 4, 10, &[span(7, 2..5)], &none, true);
        assert_eq!(p.chosen, vec![vec![false, true, false]]);
        let p = book.plan(15, 4, 10, &[span(7, 3..4)], &none, true);
        assert!(p.chosen[0][0], "still hot ten submissions after");
        // Pieces 2 and 4, owed by 6, were due at 10 = 6 + 4.
        let mut taken: Vec<(u32, u64)> = p.covers.iter().map(|t| (t.2, t.3.first)).collect();
        taken.sort_unstable();
        assert_eq!(taken, vec![(2, 6), (4, 6)]);
        let p = book.plan(16, 4, 10, &[span(7, 3..4)], &none, true);
        assert!(!p.chosen[0][0], "cold eleven after");
    }

    /// An owed piece that turns hot (a landing found it changed after it was
    /// skipped) is taken by a cover before the submission's own compare
    /// stores its hash: else that compare would flag what the owing flight
    /// wrote, for its own landing.
    #[test]
    fn an_owed_piece_read_by_a_later_compare_is_covered_first() {
        let mut book: Book<(), ()> = Book::default();
        let none = |_: u64, _: u32| false;
        book.plan(1, 100, 10, &[span(1, 0..2)], &none, true);
        book.found(1, 1, 0);
        let p = book.plan(2, 100, 10, &[span(1, 0..2)], &none, true);
        assert_eq!(p.chosen, vec![vec![false, true]]);
        assert_eq!(p.covers.iter().map(|t| (t.2, t.3.first)).collect::<Vec<_>>(), vec![(1, 1)]);
        // Piece 0 stays owed by submission 1 (not 2), so 1 is held.
        assert_eq!(book.hold(1, u64::MAX), Hold::Uncovered);
        assert_eq!(book.take_all().iter().map(|t| (t.2, t.3.first)).collect::<Vec<_>>(), vec![(0, 1)]);
        assert_eq!(book.hold(2, 0), Hold::Free);
    }

    /// A submission that is waited for reads every declared piece.
    #[test]
    fn a_submission_waited_for_reads_everything() {
        let mut book: Book<(), ()> = Book::default();
        let p = book.plan(1, 4, 10, &[span(1, 0..3)], &|_, _| false, false);
        assert_eq!(p.chosen, vec![vec![true; 3]]);
        assert_eq!(book.owed(), 0);
    }

    /// A model of the card, the queue and the landings, with the book doing
    /// what `Host::card_before` and the landing thread do: per piece the card
    /// holds a version (a shader write makes a new one), the stored hash is
    /// the version a compare or a hash pass last saw, and title memory holds
    /// the newest version a landing fetched. Submissions declare and write
    /// pieces, their sends hash pieces again, the card runs behind them and
    /// the landing thread lands what the book lets it, covering everything
    /// when the card stands idle. Whenever a flight lands (the title is told
    /// it is done), title memory must hold every write of it and of the
    /// flights before it.
    #[test]
    fn no_write_reaches_the_title_late_in_a_model_of_the_relay() {
        const PIECES: u32 = 12;
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rand = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for deadline in [1u64, 2, 5, 30] {
            let mut book: Book<(), Vec<u32>> = Book::default();
            let mut card = [0u64; PIECES as usize];
            let mut stored = [0u64; PIECES as usize];
            let mut title = [0u64; PIECES as usize];
            let mut version = 0u64;
            // Flights in the line: number, own flagged pieces, writes.
            let mut line: std::collections::VecDeque<(u64, Vec<u32>, Vec<(u32, u64)>)> = Default::default();
            let mut cover_value = 0u64;
            // The card runs in order; `ran` is the last submission run and
            // covers are done as soon as they are put on the queue (they run
            // ahead of their submission's lists, behind earlier ones: the
            // model applies every effect in queue order at once).
            let mut landings = 0;
            let compare = |pieces: &[u32], card: &[u64], stored: &mut [u64]| -> Vec<u32> {
                let mut flagged = Vec::new();
                for &p in pieces {
                    if card[p as usize] != stored[p as usize] {
                        flagged.push(p);
                    }
                    stored[p as usize] = card[p as usize];
                }
                flagged
            };
            for number in 1..=600u64 {
                // Declared: one or two runs of pieces.
                let mut declared: Vec<u32> = Vec::new();
                for _ in 0..1 + rand(2) {
                    let from = rand(u64::from(PIECES)) as u32;
                    let to = (from + 1 + rand(5) as u32).min(PIECES);
                    declared.extend(from..to);
                }
                declared.sort_unstable();
                declared.dedup();
                let sent: Vec<u32> = (0..rand(3)).map(|_| rand(u64::from(PIECES)) as u32).collect();
                let spans = [Span { copy: 1, handle: (), pieces: declared.iter().map(|&p| (p, 0, 1)).collect() }];
                let planned = book.plan(number, deadline, 6, &spans, &|_, p| sent.contains(&p), true);
                // The cover, ahead of the sends.
                let covered: Vec<u32> = planned.covers.iter().map(|t| t.2).collect();
                let flagged = compare(&covered, &card, &mut stored);
                cover_value += 1;
                book.add_cover(cover_value, &planned.covers, flagged);
                // The sends hash pieces again.
                for &p in &sent {
                    stored[p as usize] = card[p as usize];
                }
                // The lists write some declared pieces.
                let mut writes = Vec::new();
                for &p in &declared {
                    if rand(3) == 0 {
                        version += 1;
                        card[p as usize] = version;
                        writes.push((p, version));
                    }
                }
                // The submission's own compare reads the chosen pieces.
                let chosen: Vec<u32> = declared.iter().zip(&planned.chosen[0]).filter(|(_, c)| **c).map(|(p, _)| *p).collect();
                let own = compare(&chosen, &card, &mut stored);
                line.push_back((number, own, writes));
                // The landing thread, now and then; with the card idle (a
                // third of the time) it covers what is still owed.
                if rand(2) == 0 {
                    continue;
                }
                let idle = rand(3) == 0;
                while let Some(front) = line.front() {
                    match book.hold(front.0, cover_value) {
                        Hold::Free => {}
                        Hold::Cover(_) => unreachable!("covers are done at once here"),
                        Hold::Uncovered if idle => {
                            let taken = book.take_all();
                            let covered: Vec<u32> = taken.iter().map(|t| t.2).collect();
                            let flagged = compare(&covered, &card, &mut stored);
                            cover_value += 1;
                            book.add_cover(cover_value, &taken, flagged);
                            continue;
                        }
                        Hold::Uncovered => break,
                    }
                    let (flight, own, writes) = line.pop_front().unwrap();
                    // Fetched now: whatever the card holds.
                    for p in own.iter().chain(book.take_landed(flight, cover_value).iter().flatten()) {
                        title[*p as usize] = card[*p as usize];
                        book.found(1, *p, flight);
                    }
                    for (p, v) in writes {
                        assert!(title[p as usize] >= v, "D {deadline}: flight {flight}'s write {v} of piece {p} is not in title memory");
                    }
                    landings += 1;
                }
            }
            assert!(landings > 100, "D {deadline}: {landings} landings");
        }
    }

    /// What the second model does otherwise on purpose.
    #[derive(Clone, Copy, Default)]
    struct Flaws {
        /// A cover is said to run behind its submission's sends (it runs
        /// ahead of them): wrong.
        cover_behind_its_sends: bool,
        /// A landing on the copy queue takes only its plans' sends as known,
        /// not what earlier landings knew, as `relay_card_back` does: a
        /// landing with covers may know more than the next one. Harmless:
        /// the notes let go of are of sends the card is known to have run.
        forgets_what_was_known: bool,
    }

    /// One piece in the second model: the shader's bytes and the title's
    /// bytes in it, each a version.
    type Cells = (u64, u64);

    enum Op {
        Cover(u64, Vec<u32>),
        /// Piece, the title's version it takes up.
        Send(u32, u64),
        Write(u32, u64),
        /// A flight's lists are done and its compare reads these pieces.
        Own(u64, Vec<u32>),
    }

    /// The relay with the queue in it, and the title's own bytes beside the
    /// shaders' in each piece: what `Host::card_before`, the card, the
    /// landing thread (`relay_worker`, `relay_card_back`) and the merge
    /// (`Shadow::card_merge_relay`) do, step by step. The card runs the queue
    /// behind the submitting side, as far as it got; covers, sends, writes
    /// and compares take effect when it reaches them. A landing waits for
    /// its flight and for what `Book::hold` names; its fetch reads the card
    /// as it is then: on the copy queue (`copy_queue`) at once, beside
    /// whatever the card is still at, else behind everything sent. The merge
    /// leaves out the title's bytes still to go up and those of sends after
    /// the ones the fetch is known to run behind. The title writes its own
    /// bytes now and then (they go up with the next send, which hashes the
    /// piece again).
    ///
    /// Checked at every landing: title memory holds every write of the
    /// flight's shaders, and no title byte is older than the title last
    /// wrote it. Returns the landings, or what went wrong.
    fn relay_model(seed: u64, deadline: u64, copy_queue: bool, flaws: Flaws) -> Result<u32, String> {
        const PIECES: usize = 10;
        let mut seed = seed | 1;
        let mut rand = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let mut book: Book<(), (u64, u64)> = Book::default();
        let (mut card, mut stored, mut held, mut title) = ([(0, 0); PIECES], [(0, 0); PIECES], [(0, 0); PIECES], [(0, 0); PIECES]);
        let mut newest_title = [0u64; PIECES];
        let (mut queue, mut ran): (Vec<Op>, usize) = (Vec::new(), 0);
        let (mut covers_done, mut flights_done) = (0u64, 0u64);
        let mut flags: HashMap<(bool, u64), Vec<u32>> = HashMap::new();
        let (mut dirty, mut sent): (Vec<usize>, Vec<(u64, usize)>) = (Vec::new(), Vec::new());
        let (mut order, mut known, mut covers, mut version) = (0u64, 0u64, 0u64, 0u64);
        // Flights in the line: number, sends it ran behind, its writes.
        let mut line: std::collections::VecDeque<(u64, u64, Vec<(usize, u64)>)> = Default::default();
        let mut landings = 0;
        let compare = |pieces: &[u32], card: &[Cells; PIECES], stored: &mut [Cells; PIECES]| -> Vec<u32> {
            let flagged = pieces.iter().copied().filter(|&p| card[p as usize] != stored[p as usize]).collect();
            for &p in pieces {
                stored[p as usize] = card[p as usize];
            }
            flagged
        };
        // The card goes on to `to`.
        let run = |to: usize, queue: &[Op], ran: &mut usize, card: &mut [Cells; PIECES], stored: &mut [Cells; PIECES],
                       flags: &mut HashMap<(bool, u64), Vec<u32>>, covers_done: &mut u64, flights_done: &mut u64| {
            while *ran < to {
                match &queue[*ran] {
                    Op::Cover(id, pieces) => {
                        flags.insert((true, *id), compare(pieces, card, stored));
                        *covers_done = *id;
                    }
                    &Op::Send(p, v) => {
                        card[p as usize].1 = v;
                        stored[p as usize] = card[p as usize];
                    }
                    &Op::Write(p, v) => card[p as usize].0 = v,
                    Op::Own(flight, pieces) => {
                        flags.insert((false, *flight), compare(pieces, card, stored));
                        *flights_done = *flight;
                    }
                }
                *ran += 1;
            }
        };
        for number in 1..=500u64 {
            // The title writes its bytes of a piece: into title memory and
            // the CPU's side, to go up with the next send.
            if rand(2) == 0 {
                let p = rand(PIECES as u64) as usize;
                version += 1;
                (title[p].1, held[p].1, newest_title[p]) = (version, version, version);
                if !dirty.contains(&p) {
                    dirty.push(p);
                }
            }
            let mut declared: Vec<u32> = Vec::new();
            for _ in 0..1 + rand(2) {
                let from = rand(PIECES as u64) as u32;
                declared.extend(from..(from + 1 + rand(4) as u32).min(PIECES as u32));
            }
            declared.sort_unstable();
            declared.dedup();
            let spans = [Span { copy: 1, handle: (), pieces: declared.iter().map(|&p| (p, 0, 1)).collect() }];
            let rehashed = dirty.clone();
            let planned = book.plan(number, deadline, 6, &spans, &|_, p| rehashed.contains(&(p as usize)), true);
            // `card_dirty` lets go of notes no merge needs; the taking is
            // numbered.
            sent.retain(|s| s.0 > known);
            order += 1;
            if !planned.covers.is_empty() {
                covers += 1;
                queue.push(Op::Cover(covers, planned.covers.iter().map(|t| t.2).collect()));
                let behind = if flaws.cover_behind_its_sends { order } else { order - 1 };
                book.add_cover(covers, &planned.covers, (covers, behind));
            }
            for p in dirty.drain(..) {
                queue.push(Op::Send(p as u32, held[p].1));
                sent.push((order, p));
            }
            let mut writes = Vec::new();
            for &p in &declared {
                if rand(3) == 0 {
                    version += 1;
                    queue.push(Op::Write(p, version));
                    writes.push((p as usize, version));
                }
            }
            let chosen = declared.iter().zip(&planned.chosen[0]).filter(|(_, c)| **c).map(|(p, _)| *p).collect();
            queue.push(Op::Own(number, chosen));
            line.push_back((number, order, writes));
            // The card gets on some way.
            let to = (ran + rand(12) as usize).min(queue.len());
            run(to, &queue, &mut ran, &mut card, &mut stored, &mut flags, &mut covers_done, &mut flights_done);
            if rand(2) == 0 {
                continue;
            }
            // The landing thread.
            while let Some(&(flight, behind, _)) = line.front() {
                if flights_done < flight {
                    break;
                }
                match book.hold(flight, covers_done) {
                    Hold::Free => {}
                    Hold::Cover(_) => break,
                    // The card has run the line: the landing thread covers.
                    Hold::Uncovered if ran == queue.len() => {
                        let taken = book.take_all();
                        covers += 1;
                        queue.push(Op::Cover(covers, taken.iter().map(|t| t.2).collect()));
                        book.add_cover(covers, &taken, (covers, order));
                        break;
                    }
                    Hold::Uncovered => break,
                }
                let (_, _, writes) = line.pop_front().expect("the front");
                let landed = book.take_landed(flight, covers_done);
                let mut fetched: Vec<u32> = flags.remove(&(false, flight)).unwrap_or_default();
                let mut after = landed.iter().map(|c| c.1).fold(behind, u64::max);
                for (id, _) in &landed {
                    fetched.extend(flags.remove(&(true, *id)).unwrap_or_default());
                }
                if copy_queue {
                    if !flaws.forgets_what_was_known {
                        after = after.max(known);
                    }
                } else {
                    // Behind everything sent.
                    run(queue.len(), &queue, &mut ran, &mut card, &mut stored, &mut flags, &mut covers_done, &mut flights_done);
                    after = order;
                }
                for p in fetched.iter().map(|&p| p as usize) {
                    if card[p].0 != held[p].0 {
                        (title[p].0, held[p].0) = (card[p].0, card[p].0);
                    }
                    let later = dirty.contains(&p) || sent.iter().any(|s| s.1 == p && s.0 > after);
                    if !later && card[p].1 != held[p].1 {
                        (title[p].1, held[p].1) = (card[p].1, card[p].1);
                    }
                    book.found(1, p as u32, flight);
                }
                known = known.max(after);
                for (p, v) in writes {
                    if title[p].0 < v {
                        return Err(format!("flight {flight}'s write {v} of piece {p} is not in title memory"));
                    }
                }
                if let Some(p) = (0..PIECES).find(|&p| title[p].1 != newest_title[p]) {
                    return Err(format!("landing flight {flight}: the title's byte of piece {p} went back"));
                }
                landings += 1;
            }
        }
        Ok(landings)
    }

    /// The lean compare ([`super::Lean`], `VICTORY_CARD_COMPARE_EVERY`) with
    /// the relay, as `Host::card_before` and the landing thread go with it:
    /// fixed declared ranges of one copy, one or two a submission; the title
    /// writes its own bytes of a piece now and then, and the next sends take
    /// them up and hash the piece again; the lists write declared pieces; the
    /// submission's compare reads what `Lean::plan` chose, storing hashes;
    /// flights land some submissions later and fetch what their compares
    /// flagged, the shaders' bytes going into title memory. The card runs
    /// everything at once in queue order: what matters here is the stored
    /// hashes, not the timing. `fix`: the pieces the sends hash again are
    /// compared first as `Lean::ahead` says, their flags landing with the
    /// submission.
    ///
    /// Checked at every landing of a flight whose compare read a range
    /// whole: title memory holds every write shaders made to that range's
    /// pieces in that flight and before (a write lands at most with the
    /// range's next whole read). At the end, with everything read whole once
    /// more and nothing sent, title memory holds every write. Returns the
    /// landings, or what went wrong.
    fn lean_model(seed: u64, every: u64, fix: bool) -> Result<u32, String> {
        const PIECES: usize = 10;
        const RANGES: [(u32, u32); 4] = [(0, 10), (2, 3), (5, 4), (8, 2)];
        let mut seed = seed | 1;
        let mut rand = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let mut lean = super::Lean::default();
        let (mut card, mut stored, mut held, mut title) = ([(0u64, 0u64); PIECES], [(0, 0); PIECES], [(0, 0); PIECES], [(0, 0); PIECES]);
        let compare = |pieces: &[u32], card: &[Cells; PIECES], stored: &mut [Cells; PIECES]| -> Vec<u32> {
            let flagged = pieces.iter().copied().filter(|&p| card[p as usize] != stored[p as usize]).collect();
            for &p in pieces {
                stored[p as usize] = card[p as usize];
            }
            flagged
        };
        let mut version = 0u64;
        let mut dirty: Vec<u32> = Vec::new();
        // Every shader write: piece, version, flight.
        let mut writes: Vec<(u32, u64, u64)> = Vec::new();
        // Flights in the line: number, flagged pieces, ranges read whole.
        let mut line: std::collections::VecDeque<(u64, Vec<u32>, Vec<(u32, u32)>)> = Default::default();
        let mut landings = 0;
        let mut land = |line: &mut std::collections::VecDeque<(u64, Vec<u32>, Vec<(u32, u32)>)>,
                        card: &[Cells; PIECES],
                        held: &mut [Cells; PIECES],
                        title: &mut [Cells; PIECES],
                        lean: &mut super::Lean,
                        writes: &[(u32, u64, u64)]|
         -> Result<(), String> {
            while let Some((flight, flagged, wholes)) = line.pop_front() {
                for &p in &flagged {
                    let p = p as usize;
                    if card[p].0 != held[p].0 {
                        (title[p].0, held[p].0) = (card[p].0, card[p].0);
                    }
                    lean.found(1, p as u32, flight);
                }
                for &(first, pieces) in &wholes {
                    let late = writes
                        .iter()
                        .find(|&&(p, v, by)| by <= flight && first <= p && p < first + pieces && title[p as usize].0 < v);
                    if let Some((p, v, by)) = late {
                        return Err(format!(
                            "flight {flight} read piece {first}+{pieces} whole and landed, flight {by}'s write {v} of piece {p} is not in title memory"
                        ));
                    }
                }
                landings += 1;
            }
            Ok(())
        };
        let mut number = 0u64;
        for _ in 0..600 {
            number += 1;
            // The title writes its bytes of a piece: into title memory and
            // the CPU's side, to go up with the next send.
            if rand(3) != 0 {
                let p = rand(PIECES as u64) as u32;
                version += 1;
                (title[p as usize].1, held[p as usize].1) = (version, version);
                if !dirty.contains(&p) {
                    dirty.push(p);
                }
            }
            let mut flagged = Vec::new();
            // Ahead of the sends, the pieces they hash again that a compare left.
            if fix && !dirty.is_empty() {
                let runs: Vec<(u32, u32)> = dirty.iter().map(|&p| (p, p + 1)).collect();
                let ahead: Vec<u32> = lean.ahead(1, Some(&runs)).into_iter().map(|a| a.0).collect();
                flagged.extend(compare(&ahead, &card, &mut stored));
            }
            // The sends hash the pieces again.
            for p in dirty.drain(..).map(|p| p as usize) {
                card[p].1 = held[p].1;
                stored[p] = card[p];
            }
            let mut ranges: Vec<(u32, u32)> = (0..1 + rand(2)).map(|_| RANGES[rand(RANGES.len() as u64) as usize]).collect();
            ranges.dedup();
            let mut read: Vec<u32> = Vec::new();
            let mut wholes = Vec::new();
            for &(first, pieces) in &ranges {
                let bytes = (first * 65536, (first + pieces) * 65536);
                match lean.plan(number, every, 6, 1, first, pieces, bytes) {
                    None => {
                        read.extend(first..first + pieces);
                        wholes.push((first, pieces));
                    }
                    Some(chosen) => read.extend((first..first + pieces).zip(chosen).filter(|(_, c)| *c).map(|(p, _)| p)),
                }
            }
            read.sort_unstable();
            read.dedup();
            // The lists write some declared pieces.
            for &(first, pieces) in &ranges {
                for p in first..first + pieces {
                    if rand(4) == 0 {
                        version += 1;
                        card[p as usize].0 = version;
                        writes.push((p, version, number));
                    }
                }
            }
            flagged.extend(compare(&read, &card, &mut stored));
            line.push_back((number, flagged, wholes));
            if rand(3) == 0 {
                land(&mut line, &card, &mut held, &mut title, &mut lean, &writes)?;
            }
        }
        // Every range read whole once more, nothing sent.
        for _ in 0..2 {
            number += every;
            let mut read = Vec::new();
            let mut wholes = Vec::new();
            for &(first, pieces) in &RANGES {
                if lean.plan(number, every, 6, 1, first, pieces, (first * 65536, (first + pieces) * 65536)).is_none() {
                    read.extend(first..first + pieces);
                    wholes.push((first, pieces));
                }
            }
            read.sort_unstable();
            read.dedup();
            let flagged = compare(&read, &card, &mut stored);
            line.push_back((number, flagged, wholes));
            land(&mut line, &card, &mut held, &mut title, &mut lean, &writes)?;
        }
        if let Some(p) = (0..PIECES).find(|&p| title[p].0 != card[p].0) {
            return Err(format!("at the end piece {p} holds the shaders' {} on the card and {} in title memory", card[p].0, title[p].0));
        }
        Ok(landings)
    }

    /// With the compares ahead of the sends no write is lost and none lands
    /// later than its range's next whole read; without them the model sees
    /// writes the sends' hashes took in, never to land.
    #[test]
    fn no_write_is_lost_to_a_send_with_ranges_read_whole_once_in_so_many() {
        let seeds = [0x9e37_79b9_7f4a_7c15u64, 0x2545_f491_4f6c_dd1d, 0xd1b5_4a32_d192_ed03, 7, 12345, 99, 31337];
        for every in [1u64, 2, 3, 5, 8, 30] {
            for &seed in &seeds {
                let landed = lean_model(seed, every, true);
                assert!(landed.as_ref().is_ok_and(|&n| n > 100), "every {every}, seed {seed}: {landed:?}");
            }
        }
        let caught = seeds.iter().any(|&seed| [2u64, 3, 5, 8, 30].iter().any(|&every| lean_model(seed, every, false).is_err()));
        assert!(caught, "the model sees a write lost to a send without the compares ahead of it");
    }

    /// `Lean::ahead`: of a range left in part, the pieces the sends hash
    /// again, each once until the range is declared again; none once the
    /// range was read whole, or all of it was hot.
    #[test]
    fn pieces_left_by_a_compare_are_compared_ahead_of_a_send_once() {
        let mut lean = super::Lean::default();
        assert_eq!(lean.plan(1, 30, 90, 7, 10, 8, (100, 900)), None);
        assert!(lean.ahead(7, Some(&[(0, 100)])).is_empty(), "read whole after its lists");
        assert_eq!(lean.plan(2, 30, 90, 7, 10, 8, (100, 900)), Some(vec![false; 8]));
        assert!(lean.ahead(8, None).is_empty(), "another copy");
        assert_eq!(lean.ahead(7, Some(&[(0, 11), (12, 14), (17, 40)])), vec![(10, 100, 900), (12, 100, 900), (13, 100, 900), (17, 100, 900)]);
        assert!(lean.ahead(7, Some(&[(10, 11)])).is_empty(), "compared already");
        // Declared again: its pieces may be written again.
        assert_eq!(lean.plan(3, 30, 90, 7, 10, 8, (100, 900)), Some(vec![false; 8]));
        assert_eq!(lean.ahead(7, Some(&[(10, 11)])), vec![(10, 100, 900)]);
        // Two ranges hold a piece: the offsets of both.
        assert_eq!(lean.plan(3, 30, 90, 7, 12, 2, (2000, 3000)), None);
        assert_eq!(lean.plan(4, 30, 90, 7, 12, 2, (2000, 3000)), Some(vec![false; 2]));
        assert_eq!(lean.ahead(7, Some(&[(12, 13)])), vec![(12, 100, 3000)]);
        // All of it hot: read after its lists, nothing left.
        lean.found(7, 12, 4);
        lean.found(7, 13, 4);
        assert_eq!(lean.plan(5, 30, 90, 7, 12, 2, (2000, 3000)), Some(vec![true; 2]));
        assert_eq!(lean.ahead(7, Some(&[(13, 14)])), vec![(13, 100, 900)]);
        // Read whole: nothing left of it.
        assert_eq!(lean.plan(32, 30, 90, 7, 10, 8, (100, 900)), None);
        assert!(lean.ahead(7, None).is_empty());
    }

    /// The second model holds on either queue for every deadline; it sees a
    /// cover said to run behind its own sends (the title's bytes put back by
    /// the merge of a fetch on the copy queue), and a landing that knows less
    /// than an earlier one does no harm.
    #[test]
    fn no_write_reaches_the_title_late_with_the_fetch_beside_later_flights() {
        let seeds = [0x9e37_79b9_7f4a_7c15u64, 0x2545_f491_4f6c_dd1d, 0xd1b5_4a32_d192_ed03, 7, 12345];
        let harmless = Flaws { forgets_what_was_known: true, ..Flaws::default() };
        for deadline in [1u64, 2, 4, 8, 30] {
            for copy_queue in [false, true] {
                for &seed in &seeds {
                    for flaws in [Flaws::default(), harmless] {
                        let landed = relay_model(seed, deadline, copy_queue, flaws);
                        assert!(landed.as_ref().is_ok_and(|&n| n > 100), "D {deadline}, copy queue {copy_queue}, seed {seed}: {landed:?}");
                    }
                }
            }
        }
        let wrong = Flaws { cover_behind_its_sends: true, ..Flaws::default() };
        let caught = seeds.iter().any(|&seed| [1u64, 2, 4, 8].iter().any(|&d| relay_model(seed, d, true, wrong).is_err()));
        assert!(caught, "the model sees a cover said to run behind its sends");
    }
}
