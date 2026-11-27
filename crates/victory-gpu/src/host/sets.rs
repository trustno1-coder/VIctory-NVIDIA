//! Refresh sets (`VICTORY_REFRESH_SET=1`): one step for the title-memory
//! copies a list must have brought up to date, instead of one for each.
//!
//! `Memory::bring_up` (memory.rs) pushed one `Step::Refresh` for each of the
//! title-memory copies (some 245) to every list that touches by-address
//! memory, about 191 lists a frame, and every walk over a submission's steps
//! (`copy_spans`, `refresh_front`, `refresh_ahead`, `run_deferred` with
//! `Batch::add`, `does_of`, `blocked_by`, `stale_under_flights`,
//! `copied_read`, the upcheck) went over all of them. With the switch on a
//! list holds one `Step::RefreshSet` instead: an immutable [`CopySet`],
//! shared by every list while the number of copies stays the same.
//!
//! The old form is exactly the naive expansion: every occurrence of a set
//! stands for every member, in member order, each as
//! `Step::Refresh(shadow, at, len)`. The new form may skip a repeat where a
//! repeat cannot change anything. A refresh is idempotent: a range that is
//! already in a batch, or covered by what was refreshed ahead, adds nothing.
//! So a read-only walk takes a set once ([`Once`]), and a batch re-adds only
//! the members that something invalidated since it last took the set
//! ([`Seen`]). `VICTORY_REFRESH_SET_CHECK=1` runs the walks both ways at the
//! start of each submission ([`begin`]) and falls back to the naive form for
//! that submission if they disagree.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::shadow::Shadow;

/// Whether lists hold refresh sets (`VICTORY_REFRESH_SET=1`, or the check
/// below, which turns them on too so that there is something to check).
/// Read once; unset, every list holds the one step per copy it always did.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_REFRESH_SET") || check())
}

/// Whether each submission's walks are run both ways and compared
/// (`VICTORY_REFRESH_SET_CHECK=1`). Read once. It does the naive walk over
/// every occurrence of every set, so it is slow by design.
pub(crate) fn check() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_REFRESH_SET_CHECK"))
}

/// One range of a set: what `Step::Refresh(shadow, at, len)` would have been.
pub(crate) struct Member {
    pub(crate) shadow: Arc<Shadow>,
    pub(crate) at: usize,
    pub(crate) len: usize,
}

/// The copies of the memory module, as one immutable list of ranges, made
/// for a number of copies and shared by every list while that number holds.
pub(crate) struct CopySet {
    copies: usize,
    members: Vec<Member>,
    index: Index,
}

impl CopySet {
    /// A set of `members`, made when the memory module had `copies` copies.
    pub(crate) fn new(copies: usize, members: Vec<Member>) -> CopySet {
        // members that are empty or wrap get an empty span, which no range
        // overlaps, so a bad length cannot make the index lie.
        let spans: Vec<(usize, usize)> = members.iter().map(|m| (m.at, m.at.saturating_add(m.len))).collect();
        CopySet { copies, index: Index::new(&spans), members }
    }

    /// The number of copies the memory module had when this was made.
    pub(crate) fn copies(&self) -> usize {
        self.copies
    }

    /// The ranges, in the order the naive form has them.
    pub(crate) fn members(&self) -> &[Member] {
        &self.members
    }

    /// A number for the set that is its own while it lives: its address (it
    /// lives in an `Arc`, which does not move it).
    pub(crate) fn id(&self) -> usize {
        std::ptr::from_ref(self) as usize
    }

    /// Which members lie where, for [`Seen::due`].
    pub(super) fn index(&self) -> &Index {
        &self.index
    }
}

/// The members of a set by the title addresses they cover, so that the ones
/// a range touches are found without going over all of them.
pub(super) struct Index {
    /// (from, to) per member, in member order.
    spans: Vec<(usize, usize)>,
    /// Member numbers sorted by (from, number).
    order: Vec<usize>,
    /// For `order`, the largest `to` of it up to and including each place.
    reach: Vec<usize>,
}

impl Index {
    /// An index of `spans`, (from, to) per member in member order.
    pub(super) fn new(spans: &[(usize, usize)]) -> Index {
        let mut order: Vec<usize> = (0..spans.len()).collect();
        order.sort_unstable_by_key(|&n| (spans[n].0, n));
        let mut top = 0usize;
        let reach = order
            .iter()
            .map(|&n| {
                top = top.max(spans[n].1);
                top
            })
            .collect();
        Index { spans: spans.to_vec(), order, reach }
    }

    /// Calls `each` with the number of every member that has a nonempty span
    /// overlapping `[from, to)`, once each, in start order. Spans touching
    /// at an end do not overlap.
    pub(super) fn overlapping(&self, from: usize, to: usize, mut each: impl FnMut(usize)) {
        if from >= to {
            return;
        }
        // members may lie inside one another (buffers within buffers), so
        // the ends are not sorted: the running maximum says where the first
        // span that can still reach `from` is, and from there every span
        // is tested itself until one starts at or past `to`.
        let first = self.reach.partition_point(|&r| r <= from);
        for &n in &self.order[first..] {
            let (a, b) = self.spans[n];
            if a >= to {
                break;
            }
            if a < b && from < b {
                each(n);
            }
        }
    }
}

thread_local! {
    /// The old form for the rest of the submission this thread is preparing.
    static NAIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the submission this thread is preparing takes every occurrence of
/// a set as every member (the old form). Only [`begin`] changes it.
pub(super) fn naive() -> bool {
    NAIVE.with(|n| n.get())
}

/// The sets a walk over a submission's steps has taken.
#[derive(Default)]
pub(super) struct Once {
    ids: Vec<usize>,
}

impl Once {
    /// Whether this is the first time the walk meets `set`: its members are
    /// taken then and not again. Always true in the old form.
    pub(super) fn first(&mut self, set: &CopySet) -> bool {
        if naive() {
            return true;
        }
        let id = set.id();
        if self.ids.contains(&id) {
            return false;
        }
        self.ids.push(id);
        true
    }

    /// Forgets the sets taken: a step that makes a repeat matter has come.
    pub(super) fn clear(&mut self) {
        self.ids.clear();
    }
}

/// What a step refreshes, as (copy, address, bytes): the one range of a
/// `Step::Refresh`, the members of a `Step::RefreshSet` the first time the
/// walk (`once`) meets it, nothing for a repeat or any other step.
pub(super) fn refreshes<'a>(step: &'a Step, once: &mut Once) -> impl Iterator<Item = (&'a Arc<Shadow>, usize, usize)> + use<'a> {
    // decided here, not while iterating, so that the iterator does not hold
    // `once` and a caller can go on using it.
    let one = match step {
        Step::Refresh(shadow, at, len) => Some((shadow, *at, *len)),
        _ => None,
    };
    let members: &'a [Member] = match step {
        Step::RefreshSet(set) if once.first(set) => set.members(),
        _ => &[],
    };
    one.into_iter().chain(members.iter().map(|m| (&m.shadow, m.at, m.len)))
}

/// What a batch knows of the sets it took: which members of each it must add
/// again because something since made their refresh stale.
#[derive(Default)]
pub(super) struct Seen {
    /// Per set (id), how many entries of `dropped` there were when its
    /// members were last added.
    at: Vec<(usize, usize)>,
    /// Ranges of title addresses, in time order, over which a member's
    /// refresh no longer holds.
    dropped: Vec<(usize, usize)>,
}

/// A range that stands for all of title memory.
const EVERYTHING: (usize, usize) = (0, usize::MAX);

impl Seen {
    /// Refreshes of `[from, to)` no longer hold (a title write, a copy given
    /// to shaders).
    pub(super) fn dropped(&mut self, from: usize, to: usize) {
        if from < to {
            self.dropped.push((from, to));
        }
    }

    /// No refresh holds any more. Always recorded, even right after another
    /// one: a set added between the two took its place after the first, and
    /// only the second tells it that what was written since is stale again.
    pub(super) fn dropped_all(&mut self) {
        self.dropped.push(EVERYTHING);
    }

    /// Forgets everything: the batch starts a submission.
    pub(super) fn clear(&mut self) {
        self.at.clear();
        self.dropped.clear();
    }

    /// The numbers (places in `set.members()`) the batch must add now, and
    /// the count for the frame-rate line.
    pub(super) fn due(&mut self, set: &CopySet) -> Vec<usize> {
        let count = set.members().len();
        let due = self.due_in(set.id(), count, set.index());
        COUNTS[0].fetch_add(1, Ordering::Relaxed);
        COUNTS[1].fetch_add(due.len() as u64, Ordering::Relaxed);
        COUNTS[2].fetch_add(count.saturating_sub(due.len()) as u64, Ordering::Relaxed);
        due
    }

    /// [`Seen::due`] for a set of `count` members that has `index`, by `id`.
    fn due_in(&mut self, id: usize, count: usize, index: &Index) -> Vec<usize> {
        // the old form adds every member every time and keeps no memory.
        if naive() {
            return (0..count).collect();
        }
        let now = self.dropped.len();
        let since = match self.at.iter_mut().find(|e| e.0 == id) {
            Some(e) => std::mem::replace(&mut e.1, now),
            None => {
                self.at.push((id, now));
                return (0..count).collect();
            }
        };
        let gone = &self.dropped[since.min(now)..];
        if gone.is_empty() {
            return Vec::new();
        }
        if gone.contains(&EVERYTHING) {
            return (0..count).collect();
        }
        let mut due = Vec::new();
        for &(from, to) in gone {
            index.overlapping(from, to, |n| due.push(n));
        }
        due.sort_unstable();
        due.dedup();
        due
    }
}

/// Since the last frame-rate line: set steps met by a batch, members added,
/// members left out as still done, mismatches the check found.
static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Counts a difference the check found between the two forms, and says so
/// for the first eight and then at powers of two.
pub(crate) fn mismatch(what: &str) {
    let n = COUNTS[3].fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 8 || n.is_power_of_two() {
        tracing::warn!(n, what, "refresh sets: the walks over a submission differ from the naive expansion (VICTORY_REFRESH_SET_CHECK=1), this submission takes every member every time");
    }
}

/// Called by `Host::submit` once per submission, right after it takes the
/// submission lock. Without the check it only makes sure the submission
/// takes the new form. With it, runs the walks that the sets change both
/// ways over `runs` (`parts`: the lists a part of the submission begins
/// at) and takes the new form only if the results are equal.
pub(super) fn begin(runs: &[&HostList], parts: &[usize]) {
    NAIVE.with(|n| n.set(false));
    if !check() {
        return;
    }
    // what each form gives: the span per copy, the span the front refreshes
    // per copy, and the lists' refreshes the card must be told of.
    let mut results = Vec::with_capacity(2);
    for naive_form in [false, true] {
        NAIVE.with(|n| n.set(naive_form));
        // (a) the span of all refreshes of each copy.
        let mut batch = Batch::default();
        batch.copy_spans(runs);
        let spans: Vec<(usize, usize, usize)> = batch.spans.iter().map(|s| (Arc::as_ptr(&s.0) as usize, s.1, s.2)).collect();
        // (b) the walk of the front, over refreshes, writes and parts.
        let mut once = Once::default();
        let mut touches: Vec<Touch> = Vec::new();
        for (n, list) in runs.iter().enumerate() {
            if parts.contains(&n) {
                touches.push(Touch::Part);
                once.clear();
            }
            for step in &list.deferred().copies {
                if let Step::GpuWrite(shadow, ..) = step {
                    touches.push(Touch::Write(Arc::as_ptr(shadow) as usize));
                    continue;
                }
                for (shadow, at, len) in refreshes(step, &mut once) {
                    touches.push(Touch::Refresh(Arc::as_ptr(shadow) as usize, at, at + len));
                }
            }
        }
        let front = front_spans(touches.into_iter());
        // (c) the refreshes the card's steps are told of.
        results.push((spans, front, card_steps::refresh_does(runs)));
    }
    let (sets, plain) = (&results[0], &results[1]);
    let mut differs = false;
    if sets.0 != plain.0 {
        mismatch("copy_spans");
        differs = true;
    }
    if sets.1 != plain.1 {
        mismatch("front_spans");
        differs = true;
    }
    if sets.2 != plain.2 {
        mismatch("refresh_does");
        differs = true;
    }
    NAIVE.with(|n| n.set(differs));
}

/// The frame-rate line for the sets, per frame, and the counters back to
/// zero. Nothing is said when nothing was counted.
pub(super) fn line(frames: f64) {
    let frames = frames.max(1.0);
    let c: [f64; 4] = std::array::from_fn(|i| COUNTS[i].swap(0, Ordering::Relaxed) as f64 / frames);
    if c[0] + c[3] <= 0.0 {
        return;
    }
    tracing::info!(
        "refresh sets per frame (VICTORY_REFRESH_SET=1): {:.0} set steps, {:.0} members added, {:.0} members left out as still done, {:.0} mismatches (VICTORY_REFRESH_SET_CHECK=1)",
        c[0],
        c[1],
        c[2],
        c[3]
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The numbers `index` finds for `[from, to)`.
    fn found(index: &Index, from: usize, to: usize) -> Vec<usize> {
        let mut out = Vec::new();
        index.overlapping(from, to, |n| out.push(n));
        out
    }

    #[test]
    fn index_finds_overlapping_and_nested() {
        // 0 holds 1 and 2; 3 stands apart.
        let index = Index::new(&[(0, 100), (10, 20), (30, 40), (200, 300)]);
        assert_eq!(found(&index, 15, 16), vec![0, 1]);
        assert_eq!(found(&index, 35, 36), vec![0, 2]);
        assert_eq!(found(&index, 50, 60), vec![0]);
        assert_eq!(found(&index, 0, 1000), vec![0, 1, 2, 3]);
        assert_eq!(found(&index, 100, 200), Vec::<usize>::new());
    }

    #[test]
    fn index_partial_overlap_and_adjacent_ends() {
        let index = Index::new(&[(10, 20), (20, 30), (15, 25)]);
        // touching ends do not overlap.
        assert_eq!(found(&index, 20, 21), vec![2, 1]);
        assert_eq!(found(&index, 0, 10), Vec::<usize>::new());
        assert_eq!(found(&index, 30, 40), Vec::<usize>::new());
        assert_eq!(found(&index, 19, 20), vec![0, 2]);
    }

    #[test]
    fn index_ignores_empty_spans_and_empty_asks() {
        let index = Index::new(&[(10, 10), (5, 5), (10, 20)]);
        assert_eq!(found(&index, 0, 100), vec![2]);
        assert_eq!(found(&index, 10, 10), Vec::<usize>::new());
        assert_eq!(found(&Index::new(&[]), 0, 100), Vec::<usize>::new());
    }

    #[test]
    fn index_first_and_last_member() {
        let index = Index::new(&[(0, 4), (4, 8), (8, 12)]);
        assert_eq!(found(&index, 0, 1), vec![0]);
        assert_eq!(found(&index, 11, 12), vec![2]);
        assert_eq!(found(&index, 3, 9), vec![0, 1, 2]);
    }

    /// A set of three members of ten bytes each.
    fn three() -> Index {
        Index::new(&[(0, 10), (10, 20), (20, 30)])
    }

    #[test]
    fn seen_first_sight_is_all_then_nothing() {
        let (mut seen, index) = (Seen::default(), three());
        assert_eq!(seen.due_in(1, 3, &index), vec![0, 1, 2]);
        assert_eq!(seen.due_in(1, 3, &index), Vec::<usize>::new());
    }

    #[test]
    fn seen_dropped_range_gives_only_its_members_once() {
        let (mut seen, index) = (Seen::default(), three());
        seen.due_in(1, 3, &index);
        seen.dropped(5, 12);
        seen.dropped(8, 11);
        seen.dropped(7, 7);
        assert_eq!(seen.due_in(1, 3, &index), vec![0, 1]);
        assert_eq!(seen.due_in(1, 3, &index), Vec::<usize>::new());
        seen.dropped(25, 26);
        assert_eq!(seen.due_in(1, 3, &index), vec![2]);
    }

    #[test]
    fn seen_dropped_all_gives_all_each_time_it_is_recorded() {
        let (mut seen, index) = (Seen::default(), three());
        seen.due_in(1, 3, &index);
        seen.dropped_all();
        assert_eq!(seen.due_in(1, 3, &index), vec![0, 1, 2]);
        assert_eq!(seen.due_in(1, 3, &index), Vec::<usize>::new());
        // a second drop after the set was added again must make it due again.
        seen.dropped_all();
        assert_eq!(seen.due_in(1, 3, &index), vec![0, 1, 2]);
        assert_eq!(seen.due_in(1, 3, &index), Vec::<usize>::new());
    }

    #[test]
    fn seen_sets_are_independent_and_clear_forgets() {
        let (mut seen, index) = (Seen::default(), three());
        seen.due_in(1, 3, &index);
        seen.dropped(0, 5);
        assert_eq!(seen.due_in(2, 3, &index), vec![0, 1, 2]);
        assert_eq!(seen.due_in(1, 3, &index), vec![0]);
        seen.clear();
        assert_eq!(seen.due_in(1, 3, &index), vec![0, 1, 2]);
        assert!(seen.dropped.is_empty());
    }

    #[test]
    fn once_takes_a_set_once() {
        let (a, b) = (CopySet::new(0, Vec::new()), CopySet::new(0, Vec::new()));
        let mut once = Once::default();
        assert!(once.first(&a));
        assert!(!once.first(&a));
        assert!(once.first(&b));
        once.clear();
        assert!(once.first(&a));
    }
}
