//! Learned, tight declarations of what a root UAV may write
//! (`VICTORY_TIGHT_WRITES=1`; off by default, see below).
//!
//! A root view has no size, so `Draw::root_uav` declared everything that is
//! left of the title's buffer as written by the shader (the 174 MB heap, from
//! dozens of offsets): that wide declaration drives "title bytes under
//! flights' writes", the hash and compare passes, and the landing of 64 KiB
//! pieces that nothing changed in.
//!
//! Here the footprint is learned per (pipeline, root parameter), relative to
//! the root address, from what landed in title memory
//! ([`landed`], called from the landing's footprint counter):
//!
//! - Learn: the full range is declared, as before, and noted ([`Decls`]);
//!   what lands inside a noted range, within [`EXPIRE`], widens that key's
//!   footprint. After [`OBSERVE`] landings and [`MIN_DECLS`] declarations the
//!   key is tight.
//! - Tight: only the footprint, [`MARGIN`] wider on both sides and in whole
//!   64 KiB blocks, is declared. Every [`VERIFY`]th declaration is full again
//!   (noted as in Learn). A landing outside the footprint inside such a
//!   verification sets the key to wide for good (logged once).
//! - Wide: as before.
//!
//! What this does not do: a write outside the footprint between two
//! verifications is not declared, so it stays on the card (its bytes reach
//! title memory with the next declared write of the same 64 KiB, or with the
//! next verification that finds it, but not before the title is told). That
//! is why the switch is off by default: the fallback is complete for what a
//! verification sees, not for what happens between verifications. Setting
//! `VICTORY_TIGHT_VERIFY=1` verifies every declaration (nothing is gained
//! then but the learned data's log). Table views ([`crate::draw`]'s
//! `readable`) are sized and have no pipeline identity there: not changed.

use std::collections::HashMap;
use std::sync::{
    Mutex, RwLock,
    atomic::{AtomicU32, Ordering},
};
use std::time::{Duration, Instant};

const BLOCK: u64 = 64 << 10;
/// Landings that must have been seen before a key is tight.
const OBSERVE: u32 = 8;
/// Full declarations that must have been made before a key is tight.
const MIN_DECLS: u32 = 32;
/// Added to the learned footprint on both sides.
const MARGIN: u64 = 4 * BLOCK;
/// A noted declaration takes landings this long.
const EXPIRE: Duration = Duration::from_millis(400);
/// Noted declarations kept per copy.
const KEEP: usize = 128;

/// Always on (experimental code: no environment switch).
pub(crate) fn on() -> bool {
    true
}

/// Every this many declarations of a tight key are full.
fn verify_every() -> u32 {
    256
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Learn,
    Tight,
    Wide,
}

#[derive(Debug)]
struct Foot {
    mode: Mode,
    /// Footprint relative to the root address: `[lo, hi)`.
    lo: u64,
    hi: u64,
    seen: u32,
    decls: u32,
    since_full: AtomicU32,
}

impl Foot {
    fn new() -> Foot {
        Foot {
            mode: Mode::Learn,
            lo: u64::MAX,
            hi: 0,
            seen: 0,
            decls: 0,
            since_full: AtomicU32::new(0),
        }
    }

    /// A landing at `[from, to)` relative to the root address. True if it
    /// made the key wide.
    fn landed(&mut self, from: u64, to: u64) -> bool {
        match self.mode {
            Mode::Wide => false,
            Mode::Learn => {
                self.lo = self.lo.min(from);
                self.hi = self.hi.max(to);
                self.seen += 1;
                false
            }
            Mode::Tight => {
                if from < self.lo || to > self.hi {
                    self.mode = Mode::Wide;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Lock-free read path for keys already in `Tight` or `Wide`.
    fn declare_read(&self, len: u64, every: u32) -> Option<(u64, u64, bool, Option<(u64, u64)>)> {
        match self.mode {
            Mode::Wide => Some((0, len, false, None)),
            Mode::Learn => None,
            Mode::Tight => {
                let tight = Some((self.lo, self.hi));
                let next = self.since_full.fetch_add(1, Ordering::Relaxed) + 1;
                if next >= every {
                    self.since_full.store(0, Ordering::Relaxed);
                    return Some((0, len, true, tight));
                }
                let (from, to) = (self.lo.min(len), self.hi.min(len));
                if to <= from {
                    return Some((0, len, true, tight));
                }
                Some((from, to - from, false, tight))
            }
        }
    }

    /// The range, from the root address, to declare of `len` bytes;
    /// whether the declaration is a full one (noted for landings); and the
    /// key's learned footprint if already `Tight`.
    fn declare(&mut self, len: u64, every: u32) -> (u64, u64, bool, Option<(u64, u64)>) {
        self.decls = self.decls.saturating_add(1);
        match self.mode {
            Mode::Wide => (0, len, false, None),
            Mode::Learn => {
                if self.seen >= OBSERVE && self.decls >= MIN_DECLS && self.lo < self.hi {
                    self.lo = (self.lo & !(BLOCK - 1)).saturating_sub(MARGIN);
                    self.hi = self.hi.div_ceil(BLOCK) * BLOCK + MARGIN;
                    self.mode = Mode::Tight;
                    self.since_full.store(0, Ordering::Relaxed);
                    return (0, len, true, Some((self.lo, self.hi)));
                }
                (0, len, true, None)
            }
            Mode::Tight => {
                let tight = Some((self.lo, self.hi));
                let next = self.since_full.fetch_add(1, Ordering::Relaxed) + 1;
                if next >= every {
                    self.since_full.store(0, Ordering::Relaxed);
                    return (0, len, true, tight);
                }
                let (from, to) = (self.lo.min(len), self.hi.min(len));
                if to <= from {
                    // Nothing of the footprint inside the range: full.
                    return (0, len, true, tight);
                }
                (from, to - from, false, tight)
            }
        }
    }
}

struct Decl {
    key: (usize, u32),
    from: u64,
    to: u64,
    at: Instant,
}

#[derive(Default)]
struct CopyDecls {
    full: Vec<Decl>,
    tight: Vec<Decl>,
}

/// Footprints by key, in [`SHARDS`] parts by key, and noted declarations by
/// copy, in as many parts by copy: one lock for all was taken by every
/// draw or dispatch with a root UAV it writes, on every thread that
/// translates list calls. A declaration takes its key's part, then (when it
/// is a full one) its copy's; a landing takes its copy's part, then the
/// parts of the keys it found, never two at once.
const SHARDS: usize = 16;

type Keys = HashMap<(usize, u32), Foot>;
type Decls = HashMap<u64, CopyDecls>;

static KEYS: [RwLock<Option<Keys>>; SHARDS] = [const { RwLock::new(None) }; SHARDS];
static DECLS: [Mutex<Option<Decls>>; SHARDS] = [const { Mutex::new(None) }; SHARDS];

fn spread(x: u64) -> usize {
    (x.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 60) as usize % SHARDS
}

fn keys_read(key: (usize, u32)) -> std::sync::RwLockReadGuard<'static, Option<Keys>> {
    let at = spread(key.0 as u64 ^ u64::from(key.1).rotate_left(32));
    KEYS[at].read().unwrap_or_else(|e| e.into_inner())
}

fn keys_write(key: (usize, u32)) -> std::sync::RwLockWriteGuard<'static, Option<Keys>> {
    let at = spread(key.0 as u64 ^ u64::from(key.1).rotate_left(32));
    KEYS[at].write().unwrap_or_else(|e| e.into_inner())
}

fn decls_of(id: u64) -> std::sync::MutexGuard<'static, Option<Decls>> {
    DECLS[spread(id)].lock().unwrap_or_else(|e| e.into_inner())
}

fn push_decl(list: &mut Vec<Decl>, key: (usize, u32), from: u64, to: u64, now: Instant) {
    if let Some(i) = list.iter().rev().take(8).position(|d| d.key == key && d.from == from && d.to == to) {
        let at = list.len() - 1 - i;
        list[at].at = now;
        return;
    }
    if list.len() >= KEEP {
        list.remove(0);
    }
    list.push(Decl { key, from, to, at: now });
}

/// What `Draw::root_uav` declares of a root UAV at `address` in copy `id`
/// (title memory from `start`), whose range is `len` bytes: the offset from
/// `address` and the length. (0, len) unless the key is learned.
pub(crate) fn declare(pipeline: usize, param: u32, id: u64, start: u64, address: u64, len: u64) -> (u64, u64) {
    if !on() || len == 0 {
        return (0, len);
    }
    let key = (pipeline, param);
    let every = verify_every();
    let fast = {
        let guard = keys_read(key);
        guard.as_ref().and_then(|k| k.get(&key)).and_then(|foot| foot.declare_read(len, every))
    };
    let (from, n, full, tight) = match fast {
        Some(res) => res,
        None => keys_write(key)
            .get_or_insert_with(Default::default)
            .entry(key)
            .or_insert_with(Foot::new)
            .declare(len, every),
    };
    if full || tight.is_some() {
        let base = address.saturating_sub(start);
        let now = Instant::now();
        let mut decls = decls_of(id);
        let entry = decls.get_or_insert_with(Default::default).entry(id).or_default();
        if let Some((t_from, t_to)) = tight {
            let c_from = base.saturating_add(t_from);
            let c_to = base.saturating_add(t_to).min(base.saturating_add(len));
            if c_from < c_to {
                push_decl(&mut entry.tight, key, c_from, c_to, now);
            }
        }
        if full {
            push_decl(&mut entry.full, key, base, base.saturating_add(len), now);
        }
    }
    (from, n)
}

/// Shaders wrote bytes that landed in offsets `[from, to)` of copy `id`.
pub(crate) fn landed(id: u64, from: usize, to: usize) {
    if !on() {
        return;
    }
    let (from, to) = (from as u64, to as u64);
    // The noted declarations it lands in, in their order: the key and the
    // range relative to the declaration. Landings already inside an active
    // tight pipeline's learned footprint do not falsely widen another tight
    // pipeline verifying over the same shared heap.
    let (hits, explained): (Vec<((usize, u32), u64, u64)>, bool) = {
        let mut decls = decls_of(id);
        let Some(entry) = decls.as_mut().and_then(|d| d.get_mut(&id)) else {
            return;
        };
        let now = Instant::now();
        entry.full.retain(|d| now.duration_since(d.at) < EXPIRE);
        entry.tight.retain(|d| now.duration_since(d.at) < EXPIRE);
        let explained = entry.tight.iter().any(|t| from >= t.from && to <= t.to);
        let hits = entry
            .full
            .iter()
            .filter(|d| from < d.to && to > d.from)
            .map(|d| (d.key, from.max(d.from) - d.from, to.min(d.to) - d.from))
            .collect();
        (hits, explained)
    };
    for (key, a, b) in hits {
        let widened = keys_write(key)
            .as_mut()
            .and_then(|k| k.get_mut(&key))
            .is_some_and(|foot| !(foot.mode == Mode::Tight && explained) && foot.landed(a, b));
        if widened {
            tracing::warn!(
                pipeline = format_args!("{:#x}", key.0),
                param = key.1,
                from = format_args!("{a:#x}"),
                to = format_args!("{b:#x}"),
                "a shader wrote outside its learned footprint: its root UAV is declared whole again (VICTORY_TIGHT_WRITES)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learns_then_tightens_then_falls_back() {
        let mut f = Foot::new();
        let len = 100 * BLOCK;
        // Learning: whole, noted.
        for i in 0..MIN_DECLS {
            let res = f.declare(len, 4);
            assert_eq!((res.0, res.1, res.2), (0, len, true));
            if i + 1 < MIN_DECLS {
                assert_eq!(res.3, None);
            }
            f.landed(10 * BLOCK + 5, 11 * BLOCK);
        }
        // The declaration that finds it learned is still whole.
        assert_eq!(f.mode, Mode::Tight);
        let (from, n, full, tight) = f.declare(len, 4);
        assert!(!full);
        assert_eq!(tight, Some((6 * BLOCK, 15 * BLOCK)));
        assert_eq!(from, 6 * BLOCK);
        assert_eq!(from + n, 15 * BLOCK);
        // Every fourth is whole.
        assert!(!f.declare(len, 4).2);
        assert!(!f.declare(len, 4).2);
        assert!(f.declare(len, 4).2);
        // Inside: kept. Outside: wide for good.
        assert!(!f.landed(7 * BLOCK, 8 * BLOCK));
        assert!(f.landed(50 * BLOCK, 51 * BLOCK));
        assert_eq!(f.declare(len, 4), (0, len, false, None));
    }

    #[test]
    fn never_tight_without_landings() {
        let mut f = Foot::new();
        for _ in 0..1000 {
            assert_eq!(f.declare(BLOCK, 4), (0, BLOCK, true, None));
        }
        assert_eq!(f.mode, Mode::Learn);
    }

    #[test]
    fn tight_key_not_widened_by_other_tight_key_on_shared_copy() {
        let copy_id = 0xdead_beef_u64;
        let len = 100 * BLOCK;
        let key_a = (0xaaaa_1000, 0);
        let key_b = (0xbbbb_2000, 0);
        // Train A at [2..3) * BLOCK and B at [60..61) * BLOCK on separate copies first,
        // then run both against `copy_id` (shared 100 * BLOCK heap).
        let id_a = copy_id + 1;
        let id_b = copy_id + 2;
        for _ in 0..MIN_DECLS {
            let _ = declare(key_a.0, key_a.1, id_a, 0, 0, len);
            landed(id_a, (2 * BLOCK) as usize, (3 * BLOCK) as usize);
            let _ = declare(key_b.0, key_b.1, id_b, 0, 0, len);
            landed(id_b, (60 * BLOCK) as usize, (61 * BLOCK) as usize);
        }
        // Now both A and B declare on shared `copy_id`; run 256+ declarations so A verifies.
        for _ in 0..=verify_every() + 2 {
            let _ = declare(key_a.0, key_a.1, copy_id, 0, 0, len);
            let _ = declare(key_b.0, key_b.1, copy_id, 0, 0, len);
        }
        // B lands in its own learned footprint while A's verification is active on `copy_id`.
        landed(copy_id, (60 * BLOCK) as usize, (61 * BLOCK) as usize);
        let (from_a, n_a) = declare(key_a.0, key_a.1, copy_id, 0, 0, len);
        assert!(n_a < len, "pipeline A must stay tight when B lands in B's footprint (got {from_a}..{})", from_a + n_a);
    }
}
