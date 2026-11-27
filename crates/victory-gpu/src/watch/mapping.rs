//! A ticket admits readers together and writers one at a time in arrival order.
use std::sync::{Condvar, LazyLock, Mutex};

const MAX: usize = 64;
const CHUNK: usize = 2 << 20;

pub fn mapping_stripes() -> usize {
    static N: LazyLock<usize> = LazyLock::new(|| {
        std::env::var("VICTORY_MAPPING_STRIPES")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|v| (1..=MAX).contains(v))
            .unwrap_or(1)
    });
    *N
}

#[derive(Clone, Copy, Default)]
pub struct MappingRanges(u64);
impl MappingRanges {
    pub fn add(&mut self, at: usize, len: usize) {
        if len == 0 {
            return;
        }
        let first = at / CHUNK;
        let last = at.saturating_add(len - 1) / CHUNK;
        let n = mapping_stripes();
        for k in 0..(last - first + 1).min(n) {
            self.0 |= 1 << ((first + k) % n);
        }
    }
    pub(super) fn of(at: usize, len: usize) -> Self {
        let mut ranges = Self::default();
        ranges.add(at, len);
        ranges
    }
    pub(super) fn all() -> Self {
        Self(u64::MAX >> (64 - mapping_stripes()))
    }
    // An empty operation still advances the global epoch. Reserve one stripe
    // so an all-address writer cannot enter its epoch concurrently.
    pub(super) fn nonempty(self) -> Self {
        Self(self.0.max(1))
    }
    pub(super) fn include(&mut self, other: Self) {
        self.0 |= other.0;
    }
    pub(super) fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

#[derive(Default)]
struct State {
    next: u64,
    serving: u64,
    readers: usize,
    writing: bool,
}

#[derive(Default)]
struct Stripe {
    state: Mutex<State>,
    changed: Condvar,
}

static LOCKS: LazyLock<[Stripe; MAX]> =
    LazyLock::new(|| std::array::from_fn(|_| Stripe::default()));

pub(super) struct Permit {
    stripe: &'static Stripe,
    write: bool,
}
impl Drop for Permit {
    fn drop(&mut self) {
        let mut s = self.stripe.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.write {
            s.writing = false;
        } else {
            s.readers -= 1;
        }
        self.stripe.changed.notify_all();
    }
}
impl Stripe {
    fn enter(&'static self, write: bool, now: bool) -> Option<Permit> {
        let mut s = if now {
            match self.state.try_lock() {
                Ok(s) => s,
                Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => return None,
            }
        } else {
            self.state.lock().unwrap_or_else(|e| e.into_inner())
        };
        if now && (s.next != s.serving || s.writing || write && s.readers != 0) {
            return None;
        }
        let ticket = s.next;
        s.next = s.next.wrapping_add(1);
        while ticket != s.serving || s.writing || write && s.readers != 0 {
            s = self.changed.wait(s).unwrap_or_else(|e| e.into_inner());
        }
        if write {
            s.writing = true;
        } else {
            s.readers += 1;
        }
        s.serving = s.serving.wrapping_add(1);
        self.changed.notify_all();
        Some(Permit {
            stripe: self,
            write,
        })
    }
}

pub(super) fn take(ranges: MappingRanges, write: bool, now: bool) -> Option<Vec<Permit>> {
    let mut bits = ranges.0;
    let mut held = Vec::with_capacity(bits.count_ones() as usize);
    while bits != 0 {
        let index = bits.trailing_zeros() as usize;
        held.push(LOCKS[index].enter(write, now)?);
        bits &= bits - 1;
    }
    Some(held)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_readers_yield_to_a_queued_writer() {
        use std::sync::mpsc;
        let stripe: &'static Stripe = Box::leak(Box::new(Stripe::default()));
        let read = stripe.enter(false, false).unwrap();
        let (ready, started) = mpsc::sync_channel(0);
        std::thread::scope(|scope| {
            scope.spawn(move || {
                ready.send(()).unwrap();
                let _guard = stripe.enter(true, false).unwrap();
            });
            started.recv().unwrap();
            while stripe.state.lock().unwrap().next < 2 {
                std::thread::yield_now();
            }
            assert!(stripe.enter(false, true).is_none());
            drop(read);
        });
    }

    #[test]
    fn crossing_a_chunk_locks_both_endpoints() {
        let n = mapping_stripes();
        let cross = MappingRanges::of(CHUNK - 1, 2);
        let held = take(cross, true, false).unwrap();
        assert!(take(MappingRanges::of(0, 1), false, true).is_none());
        assert!(take(MappingRanges::of(CHUNK, 1), false, true).is_none());
        if n >= 3 {
            assert!(take(MappingRanges::of(2 * CHUNK, 1), false, true).is_some());
        }
        drop(held);
        assert!(take(cross, false, true).is_some());
    }
}
