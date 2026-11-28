//! BLAKE3 of a large buffer on several threads: the same hash as
//! `blake3::hash`, for the title's 115 MB (hashed twice at every start, the
//! file's bytes and the patched ones: 18 ms each on one thread).
//!
//! BLAKE3 is a tree of 1 KiB chunks. Its left subtree is always the largest
//! power of two of chunks smaller than the whole, so the input splits where
//! `left_subtree_len` says, each half is hashed as a subtree (on a thread of
//! its own, the left on this one) and the two are joined, down to pieces of
//! about a core's share.

use blake3::Hasher;
use blake3::hazmat::{
    ChainingValue, HasherExt, Mode, left_subtree_len, merge_subtrees_non_root, merge_subtrees_root,
};

/// No thread below this: their start costs more than the hash they share.
const MIN_PIECE: usize = 4 << 20;

/// At most this many threads: past it, the memory's speed is the limit.
const MAX_THREADS: usize = 8;

/// `blake3::hash(bytes)`, on as many threads as the machine and the size are
/// worth.
pub fn hash(bytes: &[u8]) -> blake3::Hash {
    let threads = std::thread::available_parallelism().map_or(1, usize::from);
    hash_with(bytes, threads.min(MAX_THREADS), MIN_PIECE)
}

/// [`hash`] on at most `threads` threads, none for less than `min_piece`
/// bytes.
fn hash_with(bytes: &[u8], threads: usize, min_piece: usize) -> blake3::Hash {
    // Halvings that leave pieces of a thread's share: 8 threads, 3.
    let depth = threads.next_power_of_two().trailing_zeros();
    if depth == 0 || !splits(bytes.len(), min_piece) {
        return blake3::hash(bytes);
    }
    let (left, right) = halves(bytes, 0, depth, min_piece);
    merge_subtrees_root(&left, &right, Mode::Hash)
}

/// Worth halving: two pieces of `min_piece` or more, and more than a chunk
/// (a tree of one chunk has no halves).
fn splits(len: usize, min_piece: usize) -> bool {
    len >= 2 * min_piece && len > blake3::CHUNK_LEN
}

/// The chaining values of the two halves of the subtree `bytes`, which
/// starts `offset` bytes into the input; the right half on another thread.
fn halves(bytes: &[u8], offset: u64, depth: u32, min_piece: usize) -> (ChainingValue, ChainingValue) {
    let left_len = left_subtree_len(bytes.len() as u64);
    let (left, right) = bytes.split_at(left_len as usize);
    let right_offset = offset + left_len;
    std::thread::scope(|scope| {
        let other = std::thread::Builder::new()
            .name("victory-hash".into())
            .spawn_scoped(scope, || subtree(right, right_offset, depth - 1, min_piece));
        let left = subtree(left, offset, depth - 1, min_piece);
        // No thread to be had: this one hashes the right half too.
        let right = match other {
            Ok(thread) => thread.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
            Err(_) => subtree(right, right_offset, depth - 1, min_piece),
        };
        (left, right)
    })
}

/// The chaining value of the subtree `bytes`, `offset` bytes into the input.
fn subtree(bytes: &[u8], offset: u64, depth: u32, min_piece: usize) -> ChainingValue {
    if depth == 0 || !splits(bytes.len(), min_piece) {
        return Hasher::new().set_input_offset(offset).update(bytes).finalize_non_root();
    }
    let (left, right) = halves(bytes, offset, depth, min_piece);
    merge_subtrees_non_root(&left, &right, Mode::Hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(len: usize) -> Vec<u8> {
        let mut x = 0x2545_f491_4f6c_dd1d_u64;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    /// The same hash as one thread's, at every length about the places the
    /// tree and the pieces change (chunks, powers of two, uneven halves), for
    /// every number of threads.
    #[test]
    fn the_same_hash_as_blake3s_own() {
        const CHUNK: usize = blake3::CHUNK_LEN;
        let input = bytes(64 * CHUNK + 1);
        let mut lens = vec![0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 2 * CHUNK, 3 * CHUNK + 5];
        for power in [2, 4, 8, 16, 32, 64] {
            lens.extend([power * CHUNK - 1, power * CHUNK, power * CHUNK + 1]);
        }
        lens.extend([5 * CHUNK + 7, 13 * CHUNK + 999, 37 * CHUNK + 3, 50 * CHUNK]);
        for len in lens {
            let input = &input[..len];
            let want = blake3::hash(input);
            for threads in 1..=9 {
                for min_piece in [1, CHUNK, 3 * CHUNK + 1, 8 * CHUNK] {
                    assert_eq!(hash_with(input, threads, min_piece), want, "{len} bytes, {threads} threads, pieces of {min_piece}");
                }
            }
        }
    }

    #[test]
    fn the_same_hash_at_the_size_it_is_for() {
        let input = bytes(3 * MIN_PIECE + 12_345);
        assert_eq!(hash(&input), blake3::hash(&input));
        assert_eq!(hash_with(&input, 3, MIN_PIECE), blake3::hash(&input));
    }

    /// One thread against several on 115 MB: `cargo test -p victory-loader
    /// --release -- --ignored --exact hash::tests::bench_hash --nocapture`
    #[test]
    #[ignore = "a benchmark"]
    fn bench_hash() {
        let input = bytes(115 << 20);
        for (name, f) in [("one thread", blake3::hash as fn(&[u8]) -> blake3::Hash), ("threads", hash)] {
            let mut times: Vec<f64> = (0..11)
                .map(|_| {
                    let at = std::time::Instant::now();
                    std::hint::black_box(f(std::hint::black_box(&input)));
                    at.elapsed().as_secs_f64() * 1e3
                })
                .collect();
            times.sort_by(f64::total_cmp);
            println!("{name}: median {:.1} ms, {times:.1?}", times[times.len() / 2]);
        }
    }
}
