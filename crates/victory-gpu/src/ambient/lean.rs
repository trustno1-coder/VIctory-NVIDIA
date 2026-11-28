//! Leaner forms of the stand-in's passes (`VICTORY_AMBIENT_LEAN=<bits>`;
//! off by default, the passes as before).
//!
//! The gather that sums the cells' light at each of the 160x90 points runs
//! three times a frame (after both reflection miss programs and the diffuse
//! resolve), each point looping over all 2,304 cells: the frame's largest
//! pass of ours. Bits:
//! - [`GATHER_WIDE`] (1): the gather in groups of 32 threads (16x2) instead
//!   of 16 (8x2). On a 32-lane card a group of 16 leaves half of each wave
//!   idle. Every point is worked out by the same code from the same inputs:
//!   the same bytes.
//! - [`MEAN_WIDE`] (2): the mean of the cells' light by 256 threads, summed
//!   in a fixed tree, instead of one thread adding 2,304 values in turn. The
//!   sum is rounded in another order: the mean may differ in its last bits.
//! - [`REUSE`] (4): a gather is left out when the one before it in the same
//!   list, since its reset, was of the same depth view and size, and no keep
//!   of the cells' light was recorded since: its points are what this one
//!   would write.
//!
//! shortcut: [`REUSE`] trusts that the depth view names the same depth with
//! the same contents between two calls in one list (the two miss programs
//! of a frame follow each other); a list's commands run in its order, so the
//! points it reuses were written before.

use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::host::HostList;

pub(super) const GATHER_WIDE: u32 = 1;
pub(super) const MEAN_WIDE: u32 = 2;
pub(super) const REUSE: u32 = 4;
const ALL: u32 = GATHER_WIDE | MEAN_WIDE | REUSE;

pub(super) fn parse(set: &str) -> u32 {
    set.trim().parse::<u32>().map_or(0, |bits| bits & ALL)
}

/// The bits asked for (read once).
pub(super) fn lean() -> u32 {
    static BITS: OnceLock<u32> = OnceLock::new();
    *BITS.get_or_init(|| parse(&std::env::var("VICTORY_AMBIENT_LEAN").unwrap_or_default()))
}

pub(super) fn has(bit: u32) -> bool {
    lean() & bit != 0
}

/// The mean of the cells' light in one group of 256 threads (compiled with
/// `KEEP` and `MEAN_WIDE`; `CELLS` is a multiple of 256).
pub(super) const SOURCE: &str = r#"
#if defined(KEEP) && defined(MEAN_WIDE)
groupshared float3 part[256];
[RootSignature(RS)]
[numthreads(256, 1, 1)]
void mean_wide(uint gi : SV_GroupIndex) {
    float3 sum = 0;
    for (uint i = gi; i < CELLS; i += 256) sum += cells[i].rgb;
    part[gi] = sum;
    GroupMemoryBarrierWithGroupSync();
    for (uint s = 128; s > 0; s >>= 1) {
        if (gi < s) part[gi] += part[gi + s];
        GroupMemoryBarrierWithGroupSync();
    }
    if (gi == 0) {
        float4 was = cells[CELLS];
        float3 now = part[0] / CELLS;
        cells[CELLS] = float4(was.a > 0.0 ? lerp(was.rgb, now, follow) : now, 1.0);
    }
}
#endif
"#;

/// What a gather's points are made of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Points {
    /// The host list it was recorded into.
    pub(super) list: usize,
    /// The depth view (its descriptor's place) and the depth's size.
    pub(super) depth: u64,
    pub(super) size: [u32; 2],
    /// Keeps of the cells' light recorded before it.
    pub(super) keeps: u64,
}

/// Whether a gather of `now` may be left out: the last one recorded was of
/// the same inputs, into the same list, and that list has not been reset
/// since (`in_list`).
pub(super) fn reuses(last: Option<Points>, now: Points, in_list: bool) -> bool {
    in_list && last == Some(now)
}

static LAST: Mutex<Option<Points>> = Mutex::new(None);
static KEEPS: AtomicU64 = AtomicU64::new(0);
/// Gathers left out and done, for the line.
pub(super) static COUNTS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Marks the list's own gather since its reset (`HostList::first_use`).
const GATHERED: usize = usize::MAX - 0xa3b;

/// A keep of the cells' light was recorded.
pub(super) fn kept() {
    KEEPS.fetch_add(1, Ordering::Relaxed);
}

/// Whether the gather about to be recorded into `list` must run (else the
/// points it would write are there). With [`REUSE`] off, always.
pub(super) fn gather_needed(list: &HostList, depth: u64, size: [u32; 2]) -> bool {
    if !has(REUSE) {
        return true;
    }
    let now = Points { list: std::ptr::from_ref(list) as usize, depth, size, keeps: KEEPS.load(Ordering::Relaxed) };
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if reuses(*last, now, list.used(GATHERED)) {
        COUNTS[0].fetch_add(1, Ordering::Relaxed);
        return false;
    }
    list.first_use(GATHERED);
    *last = Some(now);
    COUNTS[1].fetch_add(1, Ordering::Relaxed);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_are_read_as_asked() {
        assert_eq!(parse(""), 0);
        assert_eq!(parse("0"), 0);
        assert_eq!(parse("1"), GATHER_WIDE);
        assert_eq!(parse(" 7 "), GATHER_WIDE | MEAN_WIDE | REUSE);
        assert_eq!(parse("15"), ALL, "unknown bits dropped");
        assert_eq!(parse("on"), 0);
    }

    #[test]
    fn a_gather_is_left_out_only_after_the_same_one_in_the_same_list() {
        let a = Points { list: 1, depth: 0x100, size: [1280, 720], keeps: 4 };
        assert!(reuses(Some(a), a, true));
        assert!(!reuses(Some(a), a, false), "the list was reset since");
        assert!(!reuses(None, a, true));
        for other in [
            Points { list: 2, ..a },
            Points { depth: 0x140, ..a },
            Points { size: [640, 360], ..a },
            Points { keeps: 5, ..a },
        ] {
            assert!(!reuses(Some(a), other, true), "{other:?}");
        }
    }
}
