//! Two-level 64-bit bitmap allocator for a host descriptor heap's dynamic slots.
//!
//! Replaces linear `Vec<bool>` scanning with:
//! - Level 0 (`bits`): 1 bit per slot (`1` = taken, `0` = free). Padding bits
//!   at indices `>= len` in the final word are permanently set to `1`.
//! - Level 1 (`full`): 1 bit per 64-slot word in `bits`, set iff that word is
//!   completely taken (`u64::MAX`). Padding bits beyond `bits.len()` are
//!   permanently set to `1`.
//!
//! Preserves exact next-fit cursor, wrap-around (without wrapping a single
//! contiguous run past the physical end), `free`, `too_long`, and release
//! semantics.

/// Pure-Rust allocator for `0..len` contiguous dynamic descriptor slots.
pub(crate) struct DynamicSlots {
    /// Total number of allocatable slots (`0..len`).
    len: u32,
    /// Next-fit search cursor (`0..len`, or `0` when `len == 0`).
    cursor: u32,
    /// Number of currently free slots (`0..=len`).
    free: u32,
    /// Shortest request length that found no contiguous free run since slots
    /// were last given back (`u32::MAX`: none).
    too_long: u32,
    /// Level 0 bitmap: bit `i` is `1` iff slot `i` is taken (or `i >= len`).
    bits: Vec<u64>,
    /// Level 1 coarse summary bitmap: bit `w` is `1` iff `bits[w] == u64::MAX`
    /// (or `w >= bits.len()`).
    full: Vec<u64>,
}

impl DynamicSlots {
    /// Creates an allocator managing `len` initially free slots (`0..len`).
    pub(crate) fn new(len: u32) -> Self {
        if len == 0 {
            return Self {
                len: 0,
                cursor: 0,
                free: 0,
                too_long: u32::MAX,
                bits: Vec::new(),
                full: Vec::new(),
            };
        }
        let words = (len as usize).div_ceil(64);
        let mut bits = vec![0u64; words];
        let rem = len & 63;
        if rem != 0 {
            bits[words - 1] = u64::MAX << rem;
        }
        let summary_words = words.div_ceil(64);
        let mut full = vec![0u64; summary_words];
        let srem = (words & 63) as u32;
        if srem != 0 {
            full[summary_words - 1] = u64::MAX << srem;
        }
        Self {
            len,
            cursor: 0,
            free: len,
            too_long: u32::MAX,
            bits,
            full,
        }
    }

    /// Number of free slots remaining.
    #[inline]
    pub(crate) fn free(&self) -> u32 {
        self.free
    }

    /// Whether relative slot `index` is currently taken (`None` if out of range).
    #[cfg_attr(not(any(feature = "diagnostics", test)), allow(dead_code))]
    #[inline]
    pub(crate) fn is_taken(&self, index: u32) -> Option<bool> {
        (index < self.len).then(|| {
            let i = index as usize;
            (self.bits[i >> 6] >> (i & 63)) & 1 != 0
        })
    }

    /// Allocates `n` contiguous free slots starting from the next-fit cursor,
    /// wrapping to `0` (extending past the cursor by `n - 1` so runs crossing
    /// the cursor are found, while never wrapping a single run past `len`).
    pub(crate) fn alloc(&mut self, n: u32) -> Option<u32> {
        if n == 0 || n > self.len {
            return None;
        }
        if self.free < n || n >= self.too_long {
            return None;
        }
        let len = self.len as usize;
        let cursor = self.cursor as usize;
        let count = n as usize;
        let found = if count == 1 {
            self.find_one(cursor, len)
                .or_else(|| (cursor > 0).then(|| self.find_one(0, cursor)).flatten())
        } else {
            self.find_run(cursor, len, count).or_else(|| {
                if cursor > 0 {
                    let second_end = cursor.saturating_add(count - 1).min(len);
                    self.find_run(0, second_end, count)
                } else {
                    None
                }
            })
        };
        let Some(first) = found else {
            self.too_long = self.too_long.min(n);
            return None;
        };
        if count == 1 {
            self.set_one(first as usize);
        } else {
            self.set_range(first as usize, count);
        }
        self.free -= n;
        let next = first + n;
        self.cursor = if next == self.len { 0 } else { next };
        Some(first)
    }

    /// Releases `n` slots starting at relative index `at`. Safe with
    /// overlapping, duplicate, zero-length, or out-of-range inputs. Returns
    /// the number of slots that were actually taken and freed.
    pub(crate) fn release(&mut self, at: u32, n: u32) -> u32 {
        if at >= self.len || n == 0 {
            return 0;
        }
        let end = at.saturating_add(n).min(self.len);
        let given = self.clear_bits(at as usize, end as usize);
        if given > 0 {
            self.free += given;
            self.too_long = u32::MAX;
        }
        given
    }

    /// Releases a batch of `(at, n)` relative slot ranges in one pass.
    /// Produces the exact same state as calling [`Self::release`] on each
    /// range in order. Returns total slots freed.
    pub(crate) fn release_batch(&mut self, ranges: impl IntoIterator<Item = (u32, u32)>) -> u32 {
        let mut total_given = 0u32;
        for (at, n) in ranges {
            if at >= self.len || n == 0 {
                continue;
            }
            let end = at.saturating_add(n).min(self.len);
            total_given += self.clear_bits(at as usize, end as usize);
        }
        if total_given > 0 {
            self.free += total_given;
            self.too_long = u32::MAX;
        }
        total_given
    }

    /// Finds the lowest non-full word index in `from_word..=end_word` using
    /// the Level 1 `full` summary bitmap (skipping 4,096 taken slots per word).
    #[inline]
    fn next_non_full(&self, from_word: usize, end_word: usize) -> Option<usize> {
        if from_word > end_word {
            return None;
        }
        let mut sw = from_word >> 6;
        let mut sb = from_word & 63;
        let last_sw = end_word >> 6;
        while sw <= last_sw {
            let mut s = self.full[sw] | ((1u64 << sb) - 1);
            if sw == last_sw {
                let s_end_bit = (end_word & 63) + 1;
                if s_end_bit < 64 {
                    s |= u64::MAX << s_end_bit;
                }
            }
            if s != u64::MAX {
                return Some((sw << 6) + (!s).trailing_zeros() as usize);
            }
            sw += 1;
            sb = 0;
        }
        None
    }

    /// Fast path for `n == 1`: finds the lowest free slot in `[lo, hi)`.
    #[inline]
    fn find_one(&self, lo: usize, hi: usize) -> Option<u32> {
        if lo >= hi {
            return None;
        }
        let start_word = lo >> 6;
        let start_bit = lo & 63;
        let end_word = (hi - 1) >> 6;
        let end_bit = ((hi - 1) & 63) + 1;
        let low_taken = (1u64 << start_bit) - 1;
        let high_taken = if end_bit == 64 {
            0
        } else {
            u64::MAX << end_bit
        };

        // Check `start_word` directly first (the hot path when slots near cursor are free).
        let first_word = self.bits[start_word]
            | low_taken
            | if start_word == end_word {
                high_taken
            } else {
                0
            };
        if first_word != u64::MAX {
            return Some(((start_word << 6) + (!first_word).trailing_zeros() as usize) as u32);
        }
        if start_word == end_word {
            return None;
        }

        let w = self.next_non_full(start_word + 1, end_word)?;
        let word = if w == end_word {
            self.bits[w] | high_taken
        } else {
            self.bits[w]
        };
        if word != u64::MAX {
            Some(((w << 6) + (!word).trailing_zeros() as usize) as u32)
        } else {
            None
        }
    }

    /// Finds the lowest starting slot `first` in `[lo, hi)` such that all `n`
    /// slots `first..first + n` are free (`n >= 2`).
    fn find_run(&self, lo: usize, hi: usize, n: usize) -> Option<u32> {
        debug_assert!(n >= 2);
        if hi.saturating_sub(lo) < n {
            return None;
        }
        let start_word = lo >> 6;
        let start_bit = lo & 63;

        // O(1) fast path for the common next-fit bump case: the `n` slots
        // immediately starting at `lo` are already free.
        if n <= 64 {
            if start_bit + n <= 64 {
                let mask = (u64::MAX >> (64 - n)) << start_bit;
                if self.bits[start_word] & mask == 0 {
                    return Some(lo as u32);
                }
            } else {
                let overflow = start_bit + n - 64;
                if (self.bits[start_word] >> start_bit) == 0
                    && (self.bits[start_word + 1] & ((1u64 << overflow) - 1)) == 0
                {
                    return Some(lo as u32);
                }
            }
        }

        let end_word = (hi - 1) >> 6;
        let end_bit = ((hi - 1) & 63) + 1;
        let low_taken = (1u64 << start_bit) - 1;
        let high_taken = if end_bit == 64 {
            0
        } else {
            u64::MAX << end_bit
        };

        let mut w = if self.bits[start_word] != u64::MAX {
            start_word
        } else {
            self.next_non_full(start_word + 1, end_word)?
        };
        let mut run = 0usize;

        loop {
            if ((w << 6) - run).saturating_add(n) > hi {
                return None;
            }
            let mut word = self.bits[w];
            if w == start_word {
                word |= low_taken;
            }
            if w == end_word {
                word |= high_taken;
            }

            if word == 0 {
                run += 64;
                if run >= n {
                    return Some(((w << 6) + 64 - run) as u32);
                }
                if w == end_word {
                    return None;
                }
                w += 1;
                continue;
            }

            if word != u64::MAX {
                let low_zeros = word.trailing_zeros() as usize;
                if run + low_zeros >= n {
                    return Some(((w << 6) - run) as u32);
                }
                if n < 64
                    && word.count_zeros() as usize >= n
                    && let Some(bit) = find_zero_run_in_word(word, n as u32)
                {
                    return Some(((w << 6) + bit as usize) as u32);
                }
                run = word.leading_zeros() as usize;
            } else {
                run = 0;
            }

            if w == end_word {
                return None;
            }
            if run == 0 {
                w = self.next_non_full(w + 1, end_word)?;
            } else {
                w += 1;
            }
        }
    }

    #[inline]
    fn set_full(&mut self, w: usize) {
        self.full[w >> 6] |= 1u64 << (w & 63);
    }

    #[inline]
    fn clear_full(&mut self, w: usize) {
        self.full[w >> 6] &= !(1u64 << (w & 63));
    }

    #[inline]
    fn set_one(&mut self, index: usize) {
        let w = index >> 6;
        self.bits[w] |= 1u64 << (index & 63);
        if self.bits[w] == u64::MAX {
            self.set_full(w);
        }
    }

    fn set_range(&mut self, first: usize, n: usize) {
        debug_assert!(n > 0 && first + n <= self.len as usize);
        let end = first + n;
        let mut w = first >> 6;
        let start_bit = first & 63;
        let end_word = (end - 1) >> 6;
        let end_bit = ((end - 1) & 63) + 1;

        if w == end_word {
            let mask = (u64::MAX << start_bit) & (u64::MAX >> (64 - end_bit));
            self.bits[w] |= mask;
            if self.bits[w] == u64::MAX {
                self.set_full(w);
            }
            return;
        }

        let first_mask = u64::MAX << start_bit;
        self.bits[w] |= first_mask;
        if self.bits[w] == u64::MAX {
            self.set_full(w);
        }
        w += 1;

        while w < end_word {
            self.bits[w] = u64::MAX;
            self.set_full(w);
            w += 1;
        }

        let last_mask = u64::MAX >> (64 - end_bit);
        self.bits[end_word] |= last_mask;
        if self.bits[end_word] == u64::MAX {
            self.set_full(end_word);
        }
    }

    fn clear_bits(&mut self, start: usize, end: usize) -> u32 {
        debug_assert!(start < end && end <= self.len as usize);
        let mut w = start >> 6;
        let start_bit = start & 63;
        let end_word = (end - 1) >> 6;
        let end_bit = ((end - 1) & 63) + 1;

        if w == end_word {
            let mask = (u64::MAX << start_bit) & (u64::MAX >> (64 - end_bit));
            let old = self.bits[w];
            let hit = old & mask;
            if hit != 0 {
                self.bits[w] = old & !mask;
                if old == u64::MAX {
                    self.clear_full(w);
                }
                return hit.count_ones();
            }
            return 0;
        }

        let mut given = 0u32;

        {
            let mask = u64::MAX << start_bit;
            let old = self.bits[w];
            let hit = old & mask;
            if hit != 0 {
                self.bits[w] = old & !mask;
                if old == u64::MAX {
                    self.clear_full(w);
                }
                given += hit.count_ones();
            }
            w += 1;
        }

        while w < end_word {
            let old = self.bits[w];
            if old != 0 {
                self.bits[w] = 0;
                if old == u64::MAX {
                    self.clear_full(w);
                }
                given += old.count_ones();
            }
            w += 1;
        }

        {
            let mask = u64::MAX >> (64 - end_bit);
            let old = self.bits[end_word];
            let hit = old & mask;
            if hit != 0 {
                self.bits[end_word] = old & !mask;
                if old == u64::MAX {
                    self.clear_full(end_word);
                }
                given += hit.count_ones();
            }
        }

        given
    }
}

/// Returns the lowest bit index `b` such that bits `b..b + n` of `word` are
/// all `0` (with `b + n <= 64`), for `2 <= n <= 63`.
#[inline]
fn find_zero_run_in_word(word: u64, n: u32) -> Option<u32> {
    debug_assert!((2..64).contains(&n));
    let mut rem = n;
    let mut span = word;
    let mut step = 1u32;
    let mut acc = 0u64;
    let mut acc_len = 0u32;
    loop {
        if rem & 1 != 0 {
            if acc_len == 0 {
                acc = span;
            } else {
                acc |= (span >> acc_len) | (u64::MAX << (64 - acc_len));
            }
            acc_len += step;
        }
        rem >>= 1;
        if rem == 0 {
            break;
        }
        span |= (span >> step) | (u64::MAX << (64 - step));
        step <<= 1;
    }
    (acc != u64::MAX).then(|| (!acc).trailing_zeros())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exact `Vec<bool>` reference oracle matching the original `HostHeap`
    /// dynamic slot allocator line-for-line.
    struct ReferenceSlots {
        next: u32,
        taken: Vec<bool>,
        free: u32,
        too_long: u32,
    }

    impl ReferenceSlots {
        fn new(len: u32) -> Self {
            Self {
                next: 0,
                taken: vec![false; len as usize],
                free: len,
                too_long: u32::MAX,
            }
        }

        fn alloc(&mut self, n: u32) -> Option<u32> {
            let dynamic = self.taken.len() as u32;
            if n == 0 || n > dynamic {
                return None;
            }
            let enough = self.free >= n && n < self.too_long;
            let n_usize = n as usize;
            let cursor = self.next as usize;
            let end = self.taken.len();
            let second_end = cursor.saturating_add(n_usize.saturating_sub(1)).min(end);
            let starts = (cursor..end).chain(0..second_end);
            let mut run = 0;
            let mut found = None;
            let mut last = usize::MAX;
            for at in starts.take(if enough { usize::MAX } else { 0 }) {
                if at != last.wrapping_add(1) {
                    run = 0;
                }
                last = at;
                run = if self.taken[at] { 0 } else { run + 1 };
                if run == n_usize {
                    found = Some(at + 1 - n_usize);
                    break;
                }
            }
            let Some(first) = found else {
                if enough {
                    self.too_long = self.too_long.min(n);
                }
                return None;
            };
            self.taken[first..first + n_usize].fill(true);
            self.free -= n;
            self.next = ((first + n_usize) % end) as u32;
            Some(first as u32)
        }

        fn release(&mut self, at: u32, n: u32) -> u32 {
            let end = at.saturating_add(n).min(self.taken.len() as u32) as usize;
            if let Some(slots) = self.taken.get_mut(at as usize..end) {
                let given = slots.iter().filter(|taken| **taken).count() as u32;
                if given > 0 {
                    self.free += given;
                    self.too_long = u32::MAX;
                    slots.fill(false);
                }
                given
            } else {
                0
            }
        }
    }

    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn range(&mut self, hi: u32) -> u32 {
            if hi == 0 {
                0
            } else {
                (self.next_u64() % u64::from(hi)) as u32
            }
        }
    }

    fn assert_invariants_and_occupancy(
        slots: &DynamicSlots,
        oracle: &ReferenceSlots,
        full_check: bool,
    ) {
        assert_eq!(slots.free, oracle.free, "free mismatch");
        assert_eq!(slots.too_long, oracle.too_long, "too_long mismatch");
        assert_eq!(slots.cursor, oracle.next, "cursor mismatch");
        assert_eq!(slots.is_taken(slots.len), None);
        if !full_check {
            return;
        }
        // Check Level 0 occupancy bit-for-bit against oracle.
        for (i, &expected) in oracle.taken.iter().enumerate() {
            assert_eq!(
                slots.is_taken(i as u32),
                Some(expected),
                "occupancy mismatch at slot {i} (len={})",
                slots.len
            );
        }
        // Check padding bits in final Level 0 word.
        if slots.len > 0 {
            let rem = slots.len & 63;
            if rem != 0 {
                let pad_mask = u64::MAX << rem;
                assert_eq!(
                    *slots.bits.last().unwrap() & pad_mask,
                    pad_mask,
                    "Level 0 padding bits corrupted"
                );
            }
        }
        // Check Level 1 summary matches every Level 0 word + padding bits.
        for (w, &word) in slots.bits.iter().enumerate() {
            let summary_bit = (slots.full[w >> 6] >> (w & 63)) & 1 != 0;
            assert_eq!(
                summary_bit,
                word == u64::MAX,
                "Level 1 summary mismatch at word {w} (word={word:#x})"
            );
        }
        if !slots.bits.is_empty() {
            let srem = (slots.bits.len() & 63) as u32;
            if srem != 0 {
                let pad_mask = u64::MAX << srem;
                assert_eq!(
                    *slots.full.last().unwrap() & pad_mask,
                    pad_mask,
                    "Level 1 padding bits corrupted"
                );
            }
        }
    }

    #[test]
    fn find_zero_run_in_word_matches_naive_exhaustive_patterns() {
        let naive = |word: u64, n: u32| -> Option<u32> {
            let mut run = 0u32;
            for b in 0..64u32 {
                if (word >> b) & 1 == 0 {
                    run += 1;
                    if run == n {
                        return Some(b + 1 - n);
                    }
                } else {
                    run = 0;
                }
            }
            None
        };
        let mut rng = Rng::new(0x1234_5678_9abc_def0);
        let edge_words = [
            0u64,
            u64::MAX,
            1,
            1 << 63,
            0x5555_5555_5555_5555,
            0xAAAA_AAAA_AAAA_AAAA,
            0x0000_FFFF_0000_FFFF,
            0xFFFF_0000_FFFF_0000,
            0x8000_0000_0000_0001,
            0x7FFF_FFFF_FFFF_FFFE,
        ];
        for n in 2..64u32 {
            for &w in &edge_words {
                assert_eq!(find_zero_run_in_word(w, n), naive(w, n), "n={n} w={w:#x}");
            }
            for _ in 0..256 {
                let w = rng.next_u64() & rng.next_u64();
                assert_eq!(find_zero_run_in_word(w, n), naive(w, n), "n={n} w={w:#x}");
            }
        }
    }

    #[test]
    fn oracle_parity_randomized_all_capacities_and_edge_cases() {
        const SIZES: &[u32] = &[0, 1, 2, 3, 7, 16, 32, 63, 64, 65, 127, 128, 129, 512, 4096];
        // Capacities covering 0, 1, non-64-aligned, exact word/summary boundaries, and 262,144.
        let configs: &[(u32, usize, bool)] = &[
            (0, 500, true),
            (1, 2_000, true),
            (2, 2_000, true),
            (63, 8_000, true),
            (64, 8_000, true),
            (65, 8_000, true),
            (127, 8_000, true),
            (128, 8_000, true),
            (129, 8_000, true),
            (1_000, 15_000, true),
            (4_096, 15_000, true),
            (4_097, 15_000, true),
            (262_144, 30_000, false),
        ];

        let mut total_ops = 0usize;
        let mut rng = Rng::new(0xDEAD_BEEF_CAFE_BABE);

        for &(cap, ops, check_every_slot) in configs {
            let mut slots = DynamicSlots::new(cap);
            let mut oracle = ReferenceSlots::new(cap);
            let mut live: Vec<(u32, u32)> = Vec::new();
            assert_invariants_and_occupancy(&slots, &oracle, true);

            for step in 0..ops {
                total_ops += 1;
                let op = rng.range(100);
                if op < 58 {
                    // Allocate: pick from SIZES, or cap, cap+1, or random within cap.
                    let n = match rng.range(20) {
                        0 => cap,
                        1 => cap.saturating_add(1),
                        2 => rng.range(cap.min(256).saturating_add(2)),
                        _ => SIZES[rng.range(SIZES.len() as u32) as usize],
                    };
                    let got = slots.alloc(n);
                    let exp = oracle.alloc(n);
                    assert_eq!(got, exp, "alloc({n}) mismatch at cap={cap} step={step}");
                    if let Some(first) = got {
                        live.push((first, n));
                    }
                } else if op < 78 {
                    // Single release: live range, partial/overlapping, duplicate, zero-len, or out-of-range.
                    let (at, n) = match rng.range(10) {
                        0 if !live.is_empty() => {
                            // Duplicate release (keep in `live` so it can be released again).
                            live[rng.range(live.len() as u32) as usize]
                        }
                        1 if !live.is_empty() => {
                            // Partial or extended/overlapping release.
                            let (first, len) =
                                live.swap_remove(rng.range(live.len() as u32) as usize);
                            let shift = rng.range(len.saturating_add(3));
                            (first.saturating_sub(shift / 2), len.saturating_add(shift))
                        }
                        2 => (rng.range(cap.saturating_add(5)), 0), // zero-length (including at==cap and at>cap)
                        3 => (cap, rng.range(10)),                  // at == cap
                        4 => (cap.saturating_add(1 + rng.range(20)), rng.range(16)), // at > cap
                        5 => (u32::MAX - 2, 10),                    // u32 overflow guard
                        _ if !live.is_empty() => {
                            live.swap_remove(rng.range(live.len() as u32) as usize)
                        }
                        _ => (rng.range(cap.saturating_add(4)), rng.range(16)),
                    };
                    let got = slots.release(at, n);
                    let exp = oracle.release(at, n);
                    assert_eq!(
                        got, exp,
                        "release({at}, {n}) mismatch at cap={cap} step={step}"
                    );
                } else if op < 95 {
                    // Batched release of 0..=16 ranges (mix of live, duplicate, overlapping, zero-len, OOB).
                    let batch_len = rng.range(17) as usize;
                    let mut batch = Vec::with_capacity(batch_len);
                    for _ in 0..batch_len {
                        let entry = if !live.is_empty() && rng.range(4) != 0 {
                            live.swap_remove(rng.range(live.len() as u32) as usize)
                        } else {
                            match rng.range(5) {
                                0 => (cap.saturating_add(1), 4),
                                1 => (cap, 0),
                                2 => (rng.range(cap.saturating_add(2)), 0),
                                _ => (rng.range(cap.saturating_add(3)), rng.range(65)),
                            }
                        };
                        batch.push(entry);
                    }
                    let got = slots.release_batch(batch.iter().copied());
                    let mut exp = 0u32;
                    for &(at, n) in &batch {
                        exp += oracle.release(at, n);
                    }
                    assert_eq!(got, exp, "release_batch mismatch at cap={cap} step={step}");
                } else {
                    // Fragmentation burst: fill with small chunks then free alternating ranges,
                    // then probe with larger sizes to exercise `too_long` refusal and wrap-around.
                    let chunk = if cap >= 64 && rng.range(2) == 0 { 2 } else { 1 };
                    let mut burst = Vec::new();
                    for _ in 0..64 {
                        let got = slots.alloc(chunk);
                        let exp = oracle.alloc(chunk);
                        assert_eq!(got, exp);
                        if let Some(first) = got {
                            burst.push((first, chunk));
                        } else {
                            break;
                        }
                    }
                    for (idx, &(at, n)) in burst.iter().enumerate() {
                        if idx.is_multiple_of(2) {
                            assert_eq!(slots.release(at, n), oracle.release(at, n));
                        } else {
                            live.push((at, n));
                        }
                    }
                    for probe in [chunk + 1, 4, 64, 65] {
                        assert_eq!(slots.alloc(probe), oracle.alloc(probe));
                    }
                }

                let do_full = check_every_slot || step.is_multiple_of(500);
                assert_invariants_and_occupancy(&slots, &oracle, do_full);
            }

            // Final exhaustive check for every capacity (including 262,144).
            assert_invariants_and_occupancy(&slots, &oracle, true);
            // Release everything and verify full recovery.
            assert_eq!(slots.release(0, cap), oracle.release(0, cap));
            assert_invariants_and_occupancy(&slots, &oracle, true);
            assert_eq!(slots.free(), cap);
        }

        assert!(total_ops >= 100_000, "ran {total_ops} ops");
    }

    #[test]
    fn wrap_across_cursor_and_no_physical_wrap_parity() {
        for cap in [63u32, 64, 65, 130, 4097] {
            let mut slots = DynamicSlots::new(cap);
            let mut oracle = ReferenceSlots::new(cap);

            // Place cursor in the middle, leave a free gap straddling the cursor.
            let a = slots.alloc(1).unwrap();
            assert_eq!(Some(a), oracle.alloc(1));
            let b = slots.alloc(10).unwrap();
            assert_eq!(Some(b), oracle.alloc(10));
            let c = slots.alloc(10).unwrap();
            assert_eq!(Some(c), oracle.alloc(10));
            let rest = slots.alloc(cap - 21).unwrap();
            assert_eq!(Some(rest), oracle.alloc(cap - 21));

            // Re-allocating `b` sets cursor to `c` (between `b` and `c`).
            assert_eq!(slots.release(b, 10), oracle.release(b, 10));
            assert_eq!(slots.alloc(10), oracle.alloc(10));
            // Now free both `b` and `c`: cursor sits at `c`, run `b..c+10` crosses cursor!
            assert_eq!(slots.release(b, 10), oracle.release(b, 10));
            assert_eq!(slots.release(c, 10), oracle.release(c, 10));
            assert_eq!(slots.alloc(20), oracle.alloc(20));
            assert_invariants_and_occupancy(&slots, &oracle, true);

            // Free first and last slot: must NOT wrap physically into a 2-slot run.
            assert_eq!(slots.release(0, 1), oracle.release(0, 1));
            assert_eq!(slots.release(cap - 1, 1), oracle.release(cap - 1, 1));
            assert_eq!(slots.alloc(2), None);
            assert_eq!(oracle.alloc(2), None);
            assert_invariants_and_occupancy(&slots, &oracle, true);
        }
    }

    /// Opt-in release microbenchmark comparing `DynamicSlots` + `release_batch`
    /// against `Vec<bool>` (`ReferenceSlots`) + per-range `release`.
    ///
    /// Run with:
    /// `cargo test --release --locked -p victory-gpu --lib bench_dynamic_slots_vs_vec_bool -- --ignored --nocapture`
    #[test]
    #[ignore = "microbenchmark: run with --release -- --ignored --nocapture"]
    fn bench_dynamic_slots_vs_vec_bool() {
        use std::hint::black_box;
        use std::sync::Mutex;
        use std::time::Instant;

        const CYCLES: usize = 30_000;
        const BATCH: usize = 500; // 60 command lists/frame * 500 tables = 30,000 ranges

        #[derive(Clone, Copy)]
        enum Regime {
            Normal,
            Fragmented,
            HighOccupancy,
        }

        impl Regime {
            fn label(self) -> &'static str {
                match self {
                    Self::Normal => "normal        ",
                    Self::Fragmented => "fragmented    ",
                    Self::HighOccupancy => "high-occupancy",
                }
            }
        }

        for &cap in &[262_144u32, 1_048_576u32] {
            for regime in [Regime::Normal, Regime::Fragmented, Regime::HighOccupancy] {
                let mut rng = Rng::new(
                    0x9E37_79B9_7F4A_7C15
                        ^ u64::from(cap)
                        ^ (regime as u64).wrapping_mul(0x517c_c1b7),
                );
                let reqs: Vec<u32> = (0..CYCLES)
                    .map(|i| {
                        if i.is_multiple_of(16) {
                            1
                        } else if i % 64 == 1 {
                            64 + rng.range(65)
                        } else {
                            1 + rng.range(24)
                        }
                    })
                    .collect();

                let mut new_alloc = DynamicSlots::new(cap);
                let mut old_alloc = ReferenceSlots::new(cap);

                match regime {
                    Regime::Normal => {}
                    Regime::Fragmented => {
                        // ~85% occupied with small 2-slot holes scattered across the whole ring,
                        // leaving free runs only in sparse windows so scans traverse long taken spans.
                        let chunk = 4u32;
                        let mut i = 0u32;
                        while i + chunk <= cap {
                            let a = new_alloc.alloc(chunk).unwrap();
                            let b = old_alloc.alloc(chunk).unwrap();
                            debug_assert_eq!(a, b);
                            i += chunk;
                        }
                        // Free 2-slot holes every 16 slots (cannot satisfy requests >= 3)
                        // plus 128-slot windows every 4096 slots so requests still succeed after scanning.
                        let mut pos = 0u32;
                        while pos + 2 <= cap {
                            if pos.is_multiple_of(4096) {
                                let w = 128.min(cap - pos);
                                new_alloc.release(pos, w);
                                old_alloc.release(pos, w);
                                pos += 4096;
                            } else {
                                new_alloc.release(pos, 2);
                                old_alloc.release(pos, 2);
                                pos += 16;
                            }
                        }
                    }
                    Regime::HighOccupancy => {
                        // Simulate prior in-flight frame holding 75% of the pool in large blocks
                        // interspersed with 16 KiB free bands, matching multi-frame ring pressure.
                        let band = cap / 4;
                        for seg in 0..4u32 {
                            let take = (band * 3) / 4;
                            let leave = band - take;
                            let a = new_alloc.alloc(take).unwrap();
                            let b = old_alloc.alloc(take).unwrap();
                            debug_assert_eq!(a, b);
                            if seg < 3 && leave > 0 {
                                let a2 = new_alloc.alloc(leave).unwrap();
                                let b2 = old_alloc.alloc(leave).unwrap();
                                debug_assert_eq!(a2, b2);
                                new_alloc.release(a2, leave);
                                old_alloc.release(b2, leave);
                            }
                        }
                    }
                }

                // Wrap in Mutex to mirror real `HostHeap::next` lock acquisition per alloc
                // and per-range vs batched lock acquisition on release.
                let old_mtx = Mutex::new(old_alloc);
                let new_mtx = Mutex::new(new_alloc);

                let mut old_batch = Vec::with_capacity(BATCH);
                let t0 = Instant::now();
                let mut old_sum = 0u64;
                for &n in &reqs {
                    if let Some(first) = old_mtx.lock().unwrap().alloc(n) {
                        old_sum = old_sum.wrapping_add(u64::from(first));
                        old_batch.push((first, n));
                    }
                    if old_batch.len() == BATCH {
                        for (at, len) in old_batch.drain(..) {
                            old_mtx.lock().unwrap().release(at, len);
                        }
                    }
                }
                for (at, len) in old_batch.drain(..) {
                    old_mtx.lock().unwrap().release(at, len);
                }
                let old_us = t0.elapsed().as_micros();
                black_box(old_sum);

                let mut new_batch = Vec::with_capacity(BATCH);
                let t1 = Instant::now();
                let mut new_sum = 0u64;
                for &n in &reqs {
                    if let Some(first) = new_mtx.lock().unwrap().alloc(n) {
                        new_sum = new_sum.wrapping_add(u64::from(first));
                        new_batch.push((first, n));
                    }
                    if new_batch.len() == BATCH {
                        new_mtx.lock().unwrap().release_batch(new_batch.drain(..));
                    }
                }
                new_mtx.lock().unwrap().release_batch(new_batch.drain(..));
                let new_us = t1.elapsed().as_micros();
                black_box(new_sum);

                assert_eq!(new_sum, old_sum, "benchmark parity check failed");
                let speedup = old_us as f64 / new_us.max(1) as f64;
                eprintln!(
                    "cap={cap:>7} ({}) {CYCLES} alloc+release: Vec<bool>+per-range = {old_us:>7} us | DynamicSlots+batch = {new_us:>5} us | speedup = {speedup:>6.1}x",
                    regime.label()
                );
            }
        }
    }

    #[test]
    fn noop_releases_preserve_too_long_watermark() {
        let mut s = DynamicSlots::new(64);
        // Fragment into alternating taken/free runs so a 16-slot run cannot fit.
        for i in (0..64).step_by(8) {
            assert_eq!(s.alloc(4), Some(i));
            assert_eq!(s.alloc(4), Some(i + 4));
        }
        for i in (4..64).step_by(8) {
            assert_eq!(s.release(i, 4), 4);
        }
        assert_eq!(s.free(), 32);
        assert_eq!(s.alloc(16), None);
        assert_eq!(s.too_long, 16);

        // Zero-length, out-of-range, and already-free releases must not clear `too_long`.
        assert_eq!(s.release(0, 0), 0);
        assert_eq!(s.release(64, 4), 0);
        assert_eq!(s.release(4, 4), 0);
        assert_eq!(s.release_batch([(0, 0), (4, 4), (64, 8)]), 0);
        assert_eq!(s.too_long, 16);

        // Releasing a genuinely taken slot resets `too_long`.
        assert_eq!(s.release(8, 4), 4);
        assert_eq!(s.too_long, u32::MAX);
    }
}
