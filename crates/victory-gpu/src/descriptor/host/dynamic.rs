//! Ranges in the dynamic part of one shader-visible descriptor heap.
//! A recording owns its ranges until it is discarded or its submission fence
//! completes. Descriptor reuse does not depend on CPU readback or list reset.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

pub(super) struct Slots {
    state: Mutex<State>,
    retired: Mutex<Retired>,
    has_ready: AtomicBool,
}

#[derive(Default)]
struct Retired {
    by_fence: VecDeque<(u64, Vec<Range<u32>>)>,
    ready: Vec<Vec<Range<u32>>>,
}

impl Retired {
    fn complete(&mut self, completed: u64) {
        let count = self
            .by_fence
            .partition_point(|(fence, _)| *fence <= completed);
        self.ready
            .extend(self.by_fence.drain(..count).map(|(_, ranges)| ranges));
    }
}

struct State {
    by_start: BTreeMap<u32, u32>,
    by_size: BTreeSet<(u32, u32)>,
    free: u32,
}

pub(super) enum Take {
    Slot(u32),
    Short {
        free: u32,
        largest: u32,
        fence: Option<u64>,
    },
}

impl Slots {
    pub(super) fn new(capacity: u32) -> Self {
        let mut state = State {
            by_start: BTreeMap::new(),
            by_size: BTreeSet::new(),
            free: capacity,
        };
        if capacity != 0 {
            state.insert(0, capacity);
        }
        Self {
            state: Mutex::new(state),
            retired: Mutex::new(Retired::default()),
            has_ready: AtomicBool::new(false),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn retired(&self) -> MutexGuard<'_, Retired> {
        self.retired.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn take(&self, count: u32, completed: Option<u64>) -> Take {
        let mut state = self.state();
        if completed.is_none()
            && !self.has_ready.load(Ordering::Acquire)
            && let Some(first) = state.take(count)
        {
            return Take::Slot(first);
        }
        // Snapshot both queues together so a completed batch is visible
        // either as ready ranges or through the fence an exhausted caller polls.
        let (ready, fence) = {
            let mut retired = self.retired();
            if let Some(completed) = completed {
                retired.complete(completed);
            }
            self.has_ready.store(false, Ordering::Release);
            (
                std::mem::take(&mut retired.ready),
                retired.by_fence.front().map(|&(fence, _)| fence),
            )
        };
        state.give_batches(ready);
        match state.take(count) {
            Some(first) => Take::Slot(first),
            None => Take::Short {
                free: state.free,
                largest: state.by_size.last().map_or(0, |&(length, _)| length),
                fence,
            },
        }
    }

    /// Publish completed batches for the next allocation. The relay moves
    /// each batch as a whole and leaves the interval trees to the allocator.
    pub(super) fn reclaim(&self, completed: u64) {
        let mut retired = self.retired();
        retired.complete(completed);
        if !retired.ready.is_empty() {
            self.has_ready.store(true, Ordering::Release);
        }
    }

    #[cfg(feature = "diagnostics")]
    pub(super) fn taken(&self, index: u32) -> bool {
        self.state()
            .by_start
            .range(..=index)
            .next_back()
            .is_none_or(|(&first, &length)| index >= first + length)
    }
}

impl State {
    fn take(&mut self, count: u32) -> Option<u32> {
        let &(length, first) = self.by_size.range((count, 0)..).next()?;
        self.erase(first, length);
        if count < length {
            self.insert(first + count, length - count);
        }
        self.free -= count;
        Some(first)
    }

    fn insert(&mut self, first: u32, length: u32) {
        self.by_start.insert(first, length);
        self.by_size.insert((length, first));
    }

    fn erase(&mut self, first: u32, length: u32) {
        self.by_start.remove(&first);
        self.by_size.remove(&(length, first));
    }

    fn give(&mut self, mut range: Range<u32>) {
        self.free += range.end - range.start;
        if let Some((&first, &length)) = self.by_start.range(..range.start).next_back() {
            debug_assert!(first + length <= range.start);
            if first + length == range.start {
                self.erase(first, length);
                range.start = first;
            }
        }
        if let Some(length) = self.by_start.get(&range.end).copied() {
            self.erase(range.end, length);
            range.end += length;
        }
        self.insert(range.start, range.end - range.start);
    }

    /// Combine adjacent returns before updating the two interval indexes.
    fn give_batches(&mut self, batches: Vec<Vec<Range<u32>>>) {
        let mut ranges: Vec<_> = batches.into_iter().flatten().collect();
        ranges.sort_unstable_by_key(|range| range.start);
        let mut ranges = ranges.into_iter();
        if let Some(mut joined) = ranges.next() {
            for range in ranges {
                debug_assert!(joined.end <= range.start);
                if joined.end == range.start {
                    joined.end = range.end;
                } else {
                    self.give(joined);
                    joined = range;
                }
            }
            self.give(joined);
        }
    }
}

/// All ranges taken by consecutive allocations on one heap in a recording.
/// The owning pool stays distinct even if a new heap reuses the title address.
pub(crate) struct Tables {
    slots: Arc<Slots>,
    ranges: Vec<Range<u32>>,
}

impl Tables {
    pub(super) fn keep(all: &mut Vec<Self>, slots: &Arc<Slots>, range: Range<u32>) {
        if let Some(tables) = all.last_mut().filter(|t| Arc::ptr_eq(&t.slots, slots)) {
            if let Some(last) = tables
                .ranges
                .last_mut()
                .filter(|last| last.end == range.start)
            {
                last.end = range.end;
            } else {
                tables.ranges.push(range);
            }
        } else {
            all.push(Self {
                slots: slots.clone(),
                ranges: vec![range],
            });
        }
    }

    pub(crate) fn submitted(mut self, fence: u64) {
        let mut retired = self.slots.retired();
        // Submissions register their ranges under the host submission lock.
        debug_assert!(
            retired
                .by_fence
                .back()
                .is_none_or(|(last, _)| *last <= fence)
        );
        retired
            .by_fence
            .push_back((fence, std::mem::take(&mut self.ranges)));
    }
}

impl Drop for Tables {
    fn drop(&mut self) {
        if !self.ranges.is_empty() {
            let mut state = self.slots.state();
            for range in self.ranges.drain(..) {
                state.give(range);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishing_a_fence_does_not_lock_the_allocator() {
        let slots = Arc::new(Slots::new(1));
        assert!(matches!(slots.take(1, None), Take::Slot(0)));
        let mut recording = Vec::new();
        Tables::keep(&mut recording, &slots, 0..1);
        std::thread::scope(|scope| {
            let tree = slots.state();
            let (sent, wait) = std::sync::mpsc::channel();
            scope.spawn(move || {
                recording.pop().unwrap().submitted(3);
                sent.send(()).unwrap();
            });
            wait.recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            drop(tree);
        });
        assert!(matches!(slots.take(1, Some(3)), Take::Slot(0)));
    }

    #[test]
    fn reuses_only_completed_tables_without_waiting_for_newer_submissions() {
        let slots = Arc::new(Slots::new(12));
        let mut oldest = Vec::new();
        let mut newer = Vec::new();
        let mut recording = Vec::new();
        for (count, first, tables) in [
            (4, 0, &mut oldest),
            (5, 4, &mut newer),
            (3, 9, &mut recording),
        ] {
            assert!(matches!(slots.take(count, None), Take::Slot(slot) if slot == first));
            Tables::keep(tables, &slots, first..first + count);
        }
        oldest.pop().unwrap().submitted(10);
        newer.pop().unwrap().submitted(20);
        assert!(matches!(
            slots.take(4, Some(9)),
            Take::Short {
                free: 0,
                fence: Some(10),
                ..
            }
        ));
        assert!(matches!(slots.take(4, Some(10)), Take::Slot(0)));
        assert!(matches!(
            slots.take(1, Some(19)),
            Take::Short {
                free: 0,
                fence: Some(20),
                ..
            }
        ));
        drop(recording);
        assert!(matches!(slots.take(8, Some(20)), Take::Slot(4)));
    }

    #[test]
    fn a_concurrent_reclaim_keeps_completed_ranges_available() {
        for _ in 0..32 {
            let slots = Arc::new(Slots::new(12));
            let mut recording = Vec::new();
            for (first, fence) in [(0, 10), (4, 10), (8, 20)] {
                assert!(matches!(slots.take(4, None), Take::Slot(slot) if slot == first));
                Tables::keep(&mut recording, &slots, first..first + 4);
                recording.pop().unwrap().submitted(fence);
            }
            let start = std::sync::Barrier::new(2);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    start.wait();
                    slots.reclaim(10);
                });
                start.wait();
                assert!(matches!(slots.take(8, Some(10)), Take::Slot(0)));
            });
            assert!(matches!(
                slots.take(4, None),
                Take::Short {
                    fence: Some(20),
                    ..
                }
            ));
            slots.reclaim(20);
            assert!(matches!(slots.take(4, None), Take::Slot(8)));
        }
    }

    #[test]
    fn uses_smallest_fitting_range_and_coalesces_both_neighbors() {
        let slots = Arc::new(Slots::new(16));
        for (size, first) in [(6, 0), (3, 6), (4, 9), (3, 13)] {
            assert!(matches!(slots.take(size, None), Take::Slot(slot) if slot == first));
        }
        {
            let mut state = slots.state();
            state.give(0..6);
            state.give(9..13);
        }
        assert!(matches!(
            slots.take(7, None),
            Take::Short {
                free: 10,
                largest: 6,
                fence: None
            }
        ));
        assert!(matches!(slots.take(4, None), Take::Slot(9)));
        {
            let mut state = slots.state();
            state.give(9..13);
            state.give(6..9);
        }
        assert!(matches!(slots.take(13, None), Take::Slot(0)));
    }
}
