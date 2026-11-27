//! Shared state that the threads translating list calls touch on every draw,
//! spread over cache lines of their own.
//!
//! Why: eight threads of ours (`list::thin`) and the title's recording
//! threads each ran one atomic add on the same few process-wide counters per
//! draw (draws recorded, dropped, dispatched), and one read lock of the
//! table of views per descriptor a draw reads. An atomic add or a read lock
//! is a write: the line moves to the core that does it, and with eight cores
//! doing it on every draw it moves on every one (50 to 300 ns a time). A
//! list call cost 3.5 us alone and 11.5 us with the street chase's eight
//! threads busy (log play-20261127-104020, 70,418 calls in 813 ms of thread
//! time): what grows with the threads is what they share.
//!
//! So each thread takes one of [`SHARDS`] places, numbered on its first use,
//! and writes only its own: a [`Counter`] is summed when it is read (the
//! 5-second line, the frame line), and a [`ReadMostly`] value is read under
//! the thread's own lock and written under all of them.
#![cfg_attr(not(windows), allow(dead_code))]

use std::cell::{Cell, UnsafeCell};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};

/// Places: more than the threads that record or translate lists at once
/// (two threads that share one only share a line again).
pub(crate) const SHARDS: usize = 32;

/// A value alone on its cache lines (two: the processor fetches lines in
/// pairs).
#[repr(align(128))]
struct Padded<T>(T);

static NEXT: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static SHARD: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// This thread's place.
#[inline]
pub(crate) fn shard() -> usize {
    SHARD
        .try_with(|s| {
            let mut at = s.get();
            if at == usize::MAX {
                at = NEXT.fetch_add(1, Relaxed) % SHARDS;
                s.set(at);
            }
            at
        })
        .unwrap_or(0)
}

/// A count many threads add to and one reads now and then.
pub(crate) struct Counter([Padded<AtomicU64>; SHARDS]);

impl Counter {
    pub(crate) const fn new() -> Self {
        Counter([const { Padded(AtomicU64::new(0)) }; SHARDS])
    }

    #[inline]
    pub(crate) fn add(&self, n: u64) {
        self.0[shard()].0.fetch_add(n, Relaxed);
    }

    /// The count so far.
    pub(crate) fn load(&self) -> u64 {
        self.0.iter().fold(0u64, |sum, c| sum.wrapping_add(c.0.load(Relaxed)))
    }

    /// The count so far, set to 0 (nothing added meanwhile is lost: each
    /// place is taken whole).
    pub(crate) fn take(&self) -> u64 {
        self.0.iter().fold(0u64, |sum, c| sum.wrapping_add(c.0.swap(0, Relaxed)))
    }
}

/// A value read by many threads at once and written seldom: a reader takes
/// only its thread's lock, a writer takes all of them (in order, so writers
/// do not hold each other up halfway). As an `RwLock` for what it allows.
pub(crate) struct ReadMostly<T> {
    locks: [Padded<RwLock<()>>; SHARDS],
    value: UnsafeCell<T>,
}

// SAFETY: the value is reached only through the guards below: shared while a
// thread holds one of the locks for reading (no writer then holds all of
// them), alone while a thread holds all of them for writing.
unsafe impl<T: Send + Sync> Sync for ReadMostly<T> {}
// SAFETY: as for `RwLock<T>`.
unsafe impl<T: Send> Send for ReadMostly<T> {}

/// A thread's share of a [`ReadMostly`] value.
pub(crate) struct ReadGuard<'a, T> {
    _held: RwLockReadGuard<'a, ()>,
    value: &'a T,
}

/// A [`ReadMostly`] value held alone.
pub(crate) struct WriteGuard<'a, T> {
    _held: Vec<RwLockWriteGuard<'a, ()>>,
    value: &'a mut T,
}

impl<T> std::ops::Deref for ReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value
    }
}

impl<T> std::ops::Deref for WriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value
    }
}

impl<T> std::ops::DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value
    }
}

impl<T> ReadMostly<T> {
    pub(crate) const fn new(value: T) -> Self {
        ReadMostly {
            locks: [const { Padded(RwLock::new(())) }; SHARDS],
            value: UnsafeCell::new(value),
        }
    }

    fn reading<'a>(&'a self, held: RwLockReadGuard<'a, ()>) -> ReadGuard<'a, T> {
        // SAFETY: a lock is held for reading (see `Sync`).
        ReadGuard { _held: held, value: unsafe { &*self.value.get() } }
    }

    fn writing<'a>(&'a self, held: Vec<RwLockWriteGuard<'a, ()>>) -> WriteGuard<'a, T> {
        debug_assert_eq!(held.len(), SHARDS);
        // SAFETY: every lock is held for writing (see `Sync`).
        WriteGuard { _held: held, value: unsafe { &mut *self.value.get() } }
    }

    /// Shared, without waiting; `None` if a writer holds this thread's lock.
    pub(crate) fn try_read(&self) -> Option<ReadGuard<'_, T>> {
        match self.locks[shard()].0.try_read() {
            Ok(held) => Some(self.reading(held)),
            Err(TryLockError::Poisoned(e)) => Some(self.reading(e.into_inner())),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    /// Shared.
    pub(crate) fn read(&self) -> ReadGuard<'_, T> {
        let held = self.locks[shard()].0.read().unwrap_or_else(|e| e.into_inner());
        self.reading(held)
    }

    /// Alone, without waiting; `None` if any lock is held.
    pub(crate) fn try_write(&self) -> Option<WriteGuard<'_, T>> {
        let mut held = Vec::with_capacity(SHARDS);
        for lock in &self.locks {
            match lock.0.try_write() {
                Ok(g) => held.push(g),
                Err(TryLockError::Poisoned(e)) => held.push(e.into_inner()),
                Err(TryLockError::WouldBlock) => return None,
            }
        }
        Some(self.writing(held))
    }

    /// Alone.
    pub(crate) fn write(&self) -> WriteGuard<'_, T> {
        let held = self.locks.iter().map(|lock| lock.0.write().unwrap_or_else(|e| e.into_inner())).collect();
        self.writing(held)
    }
}

/// Unique numbers from a process-wide count, taken by each thread
/// [`BLOCK`] at a time: one add to the shared count every [`BLOCK`] numbers.
/// The numbers are unique, not in order across threads.
pub(crate) struct Numbers {
    next: AtomicU64,
}

/// Numbers a thread takes at a time ([`Numbers`]).
const BLOCK: u64 = 1024;

impl Numbers {
    /// Numbers from `first` on.
    pub(crate) const fn new(first: u64) -> Self {
        Numbers { next: AtomicU64::new(first) }
    }

    /// A number not given before; `own` is this thread's block of this
    /// count (`(next, end)`, `(0, 0)` at first).
    #[inline]
    pub(crate) fn next(&self, own: &Cell<(u64, u64)>) -> u64 {
        let (mut at, mut end) = own.get();
        if at == end {
            at = self.next.fetch_add(BLOCK, Relaxed);
            end = at + BLOCK;
        }
        own.set((at + 1, end));
        at
    }

    /// One number, without a block (a thread whose own is gone).
    pub(crate) fn one(&self) -> u64 {
        self.next.fetch_add(1, Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_counter_sums_what_every_thread_added() {
        static C: Counter = Counter::new();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..1000 {
                        C.add(1);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(C.load(), 8000);
        assert_eq!(C.take(), 8000);
        assert_eq!(C.load(), 0);
    }

    #[test]
    fn a_writer_excludes_readers_and_sees_their_world() {
        static V: ReadMostly<Vec<u32>> = ReadMostly::new(Vec::new());
        let threads: Vec<_> = (0..8)
            .map(|i| {
                std::thread::spawn(move || {
                    for k in 0..200 {
                        if k % 10 == 0 {
                            V.write().push(i);
                        } else {
                            let v = V.read();
                            let n = v.len();
                            assert_eq!(v.iter().count(), n);
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(V.read().len(), 8 * 20);
        assert!(V.try_write().is_some());
        let held = V.read();
        assert!(V.try_write().is_none());
        drop(held);
    }

    #[test]
    fn numbers_are_unique_across_threads() {
        static N: Numbers = Numbers::new(1);
        thread_local! {
            static OWN: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
        }
        let threads: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(|| (0..3000).map(|_| OWN.with(|own| N.next(own))).collect::<Vec<u64>>()))
            .collect();
        let mut all: Vec<u64> = threads.into_iter().flat_map(|t| t.join().unwrap()).collect();
        all.sort_unstable();
        let n = all.len();
        all.dedup();
        assert_eq!(all.len(), n);
        assert!(all[0] >= 1);
    }
}
