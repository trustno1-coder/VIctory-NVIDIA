//! The journal's pages ([`super::Journal`]): page number → the asking that
//! last found it written, in the shape of a processor's page table, not a
//! sorted tree.
//!
//! The pages are grouped in chunks of [`CHUNK`] (256 MiB of title memory),
//! made when a page of theirs is first noted. A chunk holds every page's
//! asking (0: never written) and a bit per page that is not 0. A question
//! about a range goes through it 64 pages (one word of bits) at a time: a
//! word of no written pages is passed over at once, one with a few written
//! pages visits them by their bits, and a word of many is compared four
//! pages an instruction (AVX2), with no visit of single pages. A tree
//! answered the same questions a node at a time: 1.4 to 29 times as long
//! (measured; most for short ranges and journals of many written pages).

use std::collections::HashMap;
use std::hash::BuildHasherDefault;

/// Pages a chunk.
const CHUNK: usize = 4096;
/// Words of bits a chunk.
const WORDS: usize = CHUNK / 64;
/// A word with more written pages than this is compared whole.
const MANY: u32 = 12;

struct Chunk {
    asked: [u64; CHUNK],
    bits: [u64; WORDS],
}

/// See the file.
pub(super) struct Pages {
    chunks: HashMap<usize, Box<Chunk>, BuildHasherDefault<super::PageHash>>,
    count: usize,
}

impl Pages {
    pub(super) const fn new() -> Pages {
        Pages { chunks: HashMap::with_hasher(BuildHasherDefault::new()), count: 0 }
    }

    /// Notes `page` as last found written by `asked` (not 0); the asking
    /// before, if there was one.
    pub(super) fn insert(&mut self, page: usize, asked: u64) -> Option<u64> {
        debug_assert!(asked != 0);
        let chunk = self.chunks.entry(page / CHUNK).or_insert_with(|| {
            // made on the heap at once (32 KiB): no copy through the stack
            let zeroed = vec![0u64; CHUNK + WORDS].into_boxed_slice();
            // SAFETY: `Chunk` is CHUNK + WORDS u64s, of u64 alignment, and
            // all zeros is a valid one.
            unsafe { Box::from_raw(Box::into_raw(zeroed).cast::<Chunk>()) }
        });
        let at = page % CHUNK;
        let before = std::mem::replace(&mut chunk.asked[at], asked);
        if before == 0 {
            chunk.bits[at / 64] |= 1 << (at % 64);
            self.count += 1;
            None
        } else {
            Some(before)
        }
    }

    #[cfg(test)]
    pub(super) fn get(&self, page: usize) -> Option<u64> {
        let v = self.chunks.get(&(page / CHUNK))?.asked[page % CHUNK];
        (v != 0).then_some(v)
    }

    /// Pages written in all (for tests and the diagnostics).
    #[allow(dead_code)]
    pub(super) fn len(&self) -> usize {
        self.count
    }

    /// The chunks of `[first, last)`, each with the part of the range in it
    /// (as positions inside the chunk) and the page number of its start.
    fn spans(&self, first: usize, last: usize) -> impl Iterator<Item = (&Chunk, usize, usize, usize)> + '_ {
        let ends = if first < last { first / CHUNK..(last - 1) / CHUNK + 1 } else { 0..0 };
        ends.filter_map(move |c| {
            let chunk = self.chunks.get(&c)?;
            let base = c * CHUNK;
            Some((&**chunk, first.max(base) - base, last.min(base + CHUNK) - base, base))
        })
    }

    /// `(page, asking)` of the written pages of `[first, last)`, in page order.
    pub(super) fn range(&self, first: usize, last: usize) -> Vec<(usize, u64)> {
        // counted first (a popcount a word), so the list is made once
        let count: usize = self
            .spans(first, last)
            .map(|(chunk, from, to, _)| (from / 64..to.div_ceil(64)).map(|w| word(chunk, w, from, to).count_ones() as usize).sum::<usize>())
            .sum();
        let mut out = Vec::with_capacity(count);
        if count == 0 {
            return out;
        }
        for (chunk, from, to, base) in self.spans(first, last) {
            for w in from / 64..to.div_ceil(64) {
                let mut m = word(chunk, w, from, to);
                while m != 0 {
                    let at = w * 64 + m.trailing_zeros() as usize;
                    out.push((base + at, chunk.asked[at]));
                    m &= m - 1;
                }
            }
        }
        out
    }

    /// Whether every written page of `[first, last)` was last found written
    /// no later than `taken[page - first]` or `ranged`, whichever is later
    /// (`taken` holds `last - first` askings).
    pub(super) fn none_newer(&self, first: usize, last: usize, taken: &[u64], ranged: u64) -> bool {
        assert!(taken.len() >= last.saturating_sub(first));
        for (chunk, from, to, base) in self.spans(first, last) {
            for w in from / 64..to.div_ceil(64) {
                let m = word(chunk, w, from, to);
                if m == 0 {
                    continue;
                }
                let lo = w * 64;
                let whole = m == u64::MAX || m.count_ones() > MANY;
                #[cfg(target_arch = "x86_64")]
                if whole && lo >= from && lo + 64 <= to && std::arch::is_x86_feature_detected!("avx2") {
                    let t = &taken[base + lo - first..][..64];
                    // SAFETY: AVX2 is there; 64 askings of both.
                    if unsafe { newer_avx2(&chunk.asked[lo..lo + 64], t, ranged) } {
                        return false;
                    }
                    continue;
                }
                let mut m = m;
                while m != 0 {
                    let at = lo + m.trailing_zeros() as usize;
                    if chunk.asked[at] > taken[base + at - first].max(ranged) {
                        return false;
                    }
                    m &= m - 1;
                }
            }
        }
        true
    }
}

/// The bits of word `w` of `chunk` that lie in `[from, to)`.
fn word(chunk: &Chunk, w: usize, from: usize, to: usize) -> u64 {
    let lo = w * 64;
    let mut m = chunk.bits[w];
    if from > lo {
        m &= u64::MAX << (from - lo);
    }
    if to < lo + 64 {
        m &= (1u64 << (to - lo)) - 1;
    }
    m
}

/// Whether any of 64 askings is later than its `taken` and `ranged` both.
/// Askings stay below 2^63, so signed compares do.
///
/// # Safety
/// AVX2; both hold 64.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn newer_avx2(asked: &[u64], taken: &[u64], ranged: u64) -> bool {
    use std::arch::x86_64::*;
    let r = _mm256_set1_epi64x(ranged as i64);
    let mut bad = _mm256_setzero_si256();
    for k in 0..16 {
        // SAFETY: inside both (64 each).
        let (a, t) = unsafe { (_mm256_loadu_si256(asked.as_ptr().add(4 * k).cast()), _mm256_loadu_si256(taken.as_ptr().add(4 * k).cast())) };
        let t = _mm256_blendv_epi8(t, r, _mm256_cmpgt_epi64(r, t));
        bad = _mm256_or_si256(bad, _mm256_cmpgt_epi64(a, t));
    }
    _mm256_testz_si256(bad, bad) == 0
}

#[cfg(test)]
mod tests {
    use super::Pages;
    use std::collections::BTreeMap;

    /// The same answers as the tree it replaces, over random notes and
    /// ranges across chunks, sparse and dense.
    #[test]
    fn answers_as_the_tree_did() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for density in [3u64, 30, 95] {
            let (mut pages, mut tree) = (Pages::new(), BTreeMap::new());
            let span = 3 * super::CHUNK as u64 + 1000;
            let base = 7 * super::CHUNK as u64 - 500;
            for asked in 1..span * density / 100 {
                let page = (base + next(span)) as usize;
                assert_eq!(pages.insert(page, asked), tree.insert(page, asked));
            }
            assert_eq!(pages.len(), tree.len());
            for _ in 0..3000 {
                let first = (base - 100 + next(span + 200)) as usize;
                let short = next(2) == 0;
                let last = first + next(if short { 70 } else { 5000 }) as usize;
                let want: Vec<(usize, u64)> = tree.range(first..last).map(|(&p, &w)| (p, w)).collect();
                assert_eq!(pages.range(first, last), want);
                let limit = next(span);
                let taken: Vec<u64> = (first..last).map(|_| limit + next(3) * next(span / 4 + 1)).collect();
                let ranged = if next(4) == 0 { next(span) } else { 0 };
                let want = tree.range(first..last).all(|(&page, &write)| write <= taken[page - first].max(ranged));
                assert_eq!(pages.none_newer(first, last, &taken, ranged), want, "[{first}, {last}) ranged {ranged}");
                let page = (base + next(span)) as usize;
                assert_eq!(pages.get(page), tree.get(&page).copied());
            }
        }
    }
}
