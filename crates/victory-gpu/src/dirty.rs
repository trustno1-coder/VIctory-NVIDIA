//! GPU-side 64 KiB dirty-page bitmap (`SLOT_DIRTY = 251`) and sparse per-piece
//! baseline for buffer copies in PC memory (`Shadow::card.is_none()`).
//!
//! Instead of snapshotting and diffing whole declared ranges (up to 174 MiB per
//! submission) in `Shadow::before`, translated shaders set bits `(addr >> 16)`
//! in a pooled 256 KiB bitmap (`u251`, covering all `2^21` 64 KiB pages below
//! 128 GiB). At copy-back only the 64 KiB pieces marked in `SLOT_DIRTY` are
//! inspected against a sparse per-piece baseline (`DirtyBefore`), preserving
//! dword-exact merging when the title's CPU writes adjacent dwords in the same
//! 64 KiB page while avoiding both full-range snapshots and single-lock
//! contention across flights.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use victory_shader::emit::{DIRTY_WORDS, MEMORY_PAGES};
use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

use crate::shadow::{
    Shadow, back_piece, changed_runs, merge_piece, piece_at, pieces_of, put_changed,
};
use crate::watch::PAGE;

/// Size of one dirty-tracked title page (`64 KiB`), matching `Shadow`'s piece size.
pub(crate) const PIECE: usize = 64 << 10;

/// Byte size of one `SLOT_DIRTY` bitmap (`65,536` dwords = `256 KiB`).
pub(crate) const DIRTY_BYTES: usize = DIRTY_WORDS as usize * 4;

static ZERO_PIECE: [u8; PIECE] = [0; PIECE];

/// Tracked ranges checked under `VICTORY_DIRTY_CHECK=1`, and any missed 64 KiB pieces.
pub(crate) static CHECKED_RANGES: AtomicU64 = AtomicU64::new(0);
pub(crate) static MISSED_PIECES: AtomicU64 = AtomicU64::new(0);
/// Declared bytes whose CPU `find_all` scan was skipped via `SLOT_DIRTY`, and pieces merged.
pub(crate) static TRACKED_SKIPPED_BYTES: AtomicU64 = AtomicU64::new(0);
pub(crate) static DIRTY_PIECES_MERGED: AtomicU64 = AtomicU64::new(0);

static TEST_CHECK_OVERRIDE: AtomicU8 = AtomicU8::new(0);

/// Whether `SLOT_DIRTY` dirty-bitmap tracking is active (`VICTORY_DIRTY_BITMAP`, on by default).
pub(crate) fn on() -> bool {
    victory_shader::emit::dirty_bitmap()
}

/// Whether `VICTORY_DIRTY_CHECK=1` cross-checks `SLOT_DIRTY` against CPU `Shadow::before` diffs.
pub(crate) fn check() -> bool {
    match TEST_CHECK_OVERRIDE.load(Ordering::Relaxed) {
        1 => return false,
        2 => return true,
        _ => {}
    }
    static CHECK: OnceLock<bool> = OnceLock::new();
    *CHECK.get_or_init(|| std::env::var_os("VICTORY_DIRTY_CHECK").is_some_and(|v| v == "1"))
}

#[cfg(test)]
fn set_check_for_test(on: Option<bool>) {
    let code = match on {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    };
    TEST_CHECK_OVERRIDE.store(code, Ordering::Relaxed);
}

/// A 256 KiB CPU-mapped UAV buffer (`u251`) bound per `HostList`.
pub(crate) struct DirtySlot {
    _buffer: ID3D12Resource,
    data: *mut u32,
    gpu: u64,
}

// SAFETY: D3D12 resource is free-threaded; `data` points to its persistent mapping,
// written by the GPU during execution and read/cleared on the CPU only after fences complete.
unsafe impl Send for DirtySlot {}
// SAFETY: Read-only word loads after fence completion; zeroing runs on unique `Arc` ownership.
unsafe impl Sync for DirtySlot {}

impl DirtySlot {
    pub(crate) fn gpu(&self) -> u64 {
        self.gpu
    }

    #[inline]
    pub(crate) fn word(&self, idx: usize) -> u32 {
        debug_assert!(idx < DIRTY_WORDS as usize);
        // SAFETY: `data` maps `DIRTY_WORDS` u32s (`256 KiB`).
        unsafe { *self.data.add(idx) }
    }

    fn clear_nonzero(&mut self) {
        // SAFETY: unique ownership after GPU execution finished; 256 KiB is 8-byte aligned.
        let words = unsafe { std::slice::from_raw_parts_mut(self.data.cast::<u64>(), DIRTY_BYTES / 8) };
        for w in words {
            if *w != 0 {
                *w = 0;
            }
        }
    }
}

static POOL: Mutex<Vec<DirtySlot>> = Mutex::new(Vec::new());
const POOL_MAX: usize = 128;

/// Takes a zeroed 256 KiB `DirtySlot` from the pool, or creates one.
pub(crate) fn acquire() -> Option<Arc<DirtySlot>> {
    if let Some(slot) = POOL.lock().unwrap_or_else(|e| e.into_inner()).pop() {
        return Some(Arc::new(slot));
    }
    let host = crate::host::host()?;
    let (buffer, data) = host.shared_buffer_now(DIRTY_BYTES as u64)?;
    // SAFETY: `shared_buffer_now` maps `DIRTY_BYTES` bytes; zero once at creation.
    let gpu = unsafe {
        std::ptr::write_bytes(data, 0, DIRTY_BYTES);
        buffer.GetGPUVirtualAddress()
    };
    Some(Arc::new(DirtySlot {
        _buffer: buffer,
        data: data.cast::<u32>(),
        gpu,
    }))
}

/// Zeroes and recycles `slot` once the last holder (`HostList` or `Flight`) drops it.
pub(crate) fn release(slot: Arc<DirtySlot>) {
    let Ok(mut slot) = Arc::try_unwrap(slot) else {
        return;
    };
    slot.clear_nonzero();
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    if pool.len() < POOL_MAX {
        pool.push(slot);
    }
}

/// Extracts `(offset, bytes)` 64 KiB pieces within `[from, to)` of a `Shadow` starting
/// at `start` whose title 64 KiB page bit is set in any of `slots`.
pub(crate) fn dirty_pieces(
    slots: &[Arc<DirtySlot>],
    start: usize,
    from: usize,
    to: usize,
) -> Vec<(usize, usize)> {
    if from >= to || slots.is_empty() {
        return Vec::new();
    }
    extract_pieces(start, from, to, |word_idx| {
        let mut bits = 0u32;
        for slot in slots {
            bits |= slot.word(word_idx);
        }
        bits
    })
}

fn extract_pieces(
    start: usize,
    from: usize,
    to: usize,
    mut word_at: impl FnMut(usize) -> u32,
) -> Vec<(usize, usize)> {
    if from >= to {
        return Vec::new();
    }
    let first_page = (start + from) >> 16;
    let last_page = (start + to - 1) >> 16;
    let mut out = Vec::new();
    let mut page = first_page;
    while page <= last_page {
        let idx = page & (MEMORY_PAGES as usize - 1);
        let word_idx = idx >> 5;
        let bit_lo = idx & 31;
        let max_in_word = (32 - bit_lo).min(last_page - page + 1);
        let mask_span = if max_in_word == 32 {
            u32::MAX
        } else {
            ((1u32 << max_in_word) - 1) << bit_lo
        };
        let mut bits = word_at(word_idx) & mask_span;
        while bits != 0 {
            let b = bits.trailing_zeros() as usize;
            let p = page + (b - bit_lo);
            let lo = (p << 16).saturating_sub(start).max(from);
            let hi = ((p + 1) << 16).saturating_sub(start).min(to);
            if lo < hi {
                out.push((lo, hi - lo));
            }
            bits &= bits - 1;
        }
        page += max_in_word;
    }
    out
}

/// Cross-checks CPU-diffed pieces (`Shadow::before`) against `SLOT_DIRTY` pieces
/// under `VICTORY_DIRTY_CHECK=1`. Every piece that actually changed on the GPU
/// must be present in `gpu_pieces`.
pub(crate) fn verify_check(start: u64, cpu_pieces: &[(usize, usize)], gpu_pieces: &[(usize, usize)]) {
    CHECKED_RANGES.fetch_add(1, Ordering::Relaxed);
    let mut j = 0;
    for &(at, n) in cpu_pieces {
        while j < gpu_pieces.len() && gpu_pieces[j].0 < at {
            j += 1;
        }
        if j >= gpu_pieces.len() || gpu_pieces[j] != (at, n) {
            let missed = MISSED_PIECES.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::error!(
                missed,
                copy_start = format_args!("{start:#x}"),
                piece_offset = format_args!("{at:#x}"),
                piece_bytes = n,
                "VICTORY_DIRTY_CHECK: GPU dirty-page bitmap missed a shader-written 64 KiB piece"
            );
        }
    }
}

fn zero_piece_box() -> Box<[u8; PIECE]> {
    vec![0u8; PIECE]
        .into_boxed_slice()
        .try_into()
        .expect("PIECE bytes")
}

/// Sparse per-64KiB-piece baseline for a non-carded `Shadow`.
///
/// Unpopulated pieces remain `None` (implicitly `[0u8; 65536]`) until written,
/// and each piece has its own `Mutex` so parallel landing threads (`land::each`)
/// and overlapping relay flights never contend on a whole-copy snapshot lock.
pub(crate) struct DirtyBefore {
    pieces: OnceLock<Box<[Mutex<Option<Box<[u8; PIECE]>>>]>>,
}

impl DirtyBefore {
    pub(crate) const fn new() -> Self {
        Self {
            pieces: OnceLock::new(),
        }
    }

    #[inline]
    pub(crate) fn is_init(&self) -> bool {
        self.pieces.get().is_some()
    }

    /// Initializes the per-piece table on the first tracked `writing_with` call,
    /// snapshotting only pieces that had non-zero bytes populated beforehand.
    pub(crate) fn ensure(
        &self,
        start: usize,
        len: usize,
        data: *mut u8,
        populated: &Mutex<Vec<u64>>,
    ) {
        if self.pieces.get().is_some() {
            return;
        }
        let count = pieces_of(start, 0, len.max(1)).end;
        let pop = populated.lock().unwrap_or_else(|e| e.into_inner());
        let _ = self.pieces.get_or_init(|| {
            (0..count)
                .map(|p| {
                    let has_data = pop
                        .get(p / 64)
                        .is_some_and(|word| (word >> (p % 64)) & 1 != 0);
                    let init = if has_data && len > 0 {
                        let (lo, hi) = piece_at(start, len, p);
                        let mut buf = zero_piece_box();
                        // SAFETY: `[lo, hi)` lies inside `data` (`len` bytes).
                        unsafe {
                            std::ptr::copy_nonoverlapping(data.add(lo), buf.as_mut_ptr(), hi - lo);
                        }
                        Some(buf)
                    } else {
                        None
                    };
                    Mutex::new(init)
                })
                .collect::<Vec<_>>()
                .into_boxed_slice()
        });
    }

    /// Updates baseline bytes from `data[from..to]` after a CPU refresh or untracked write.
    pub(crate) fn sync_from_data(
        &self,
        start: usize,
        len: usize,
        data: *mut u8,
        from: usize,
        to: usize,
    ) {
        let Some(table) = self.pieces.get() else {
            return;
        };
        if from >= to {
            return;
        }
        for p in pieces_of(start, from, to) {
            let (lo, hi) = piece_at(start, len, p);
            let (a, b) = (from.max(lo), to.min(hi));
            if a >= b {
                continue;
            }
            let mut guard = table[p].lock().unwrap_or_else(|e| e.into_inner());
            let buf = guard.get_or_insert_with(zero_piece_box);
            // SAFETY: `[a, b)` lies inside `data` and within `[lo, hi)` of piece `p`.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    data.add(a),
                    buf.as_mut_ptr().add(a - lo),
                    b - a,
                );
            }
        }
    }

    /// Copies `slice` (at copy offset `at`) into the baseline.
    pub(crate) fn put_slice(&self, start: usize, len: usize, at: usize, slice: &[u8]) {
        let Some(table) = self.pieces.get() else {
            return;
        };
        let to = (at + slice.len()).min(len);
        if at >= to {
            return;
        }
        for p in pieces_of(start, at, to) {
            let (lo, hi) = piece_at(start, len, p);
            let (a, b) = (at.max(lo), to.min(hi));
            if a >= b {
                continue;
            }
            let mut guard = table[p].lock().unwrap_or_else(|e| e.into_inner());
            let buf = guard.get_or_insert_with(zero_piece_box);
            buf[a - lo..b - lo].copy_from_slice(&slice[a - at..b - at]);
        }
    }

    /// Waited-for copy-back of dirty pieces `(at, n)`: merges GPU-changed dwords
    /// into title memory and updates both `data` and the piece baseline.
    pub(crate) fn copy_back(
        &self,
        start: usize,
        len: usize,
        data: *mut u8,
        pieces: &[(usize, usize)],
    ) -> usize {
        let Some(table) = self.pieces.get() else {
            return 0;
        };
        let (mut title, mut mask, mut runs) = (Vec::new(), Vec::new(), Vec::new());
        let mut written = 0;
        for &(at, n) in pieces {
            if n == 0 || at + n > len {
                continue;
            }
            let p = (start + at) / PIECE - start / PIECE;
            let (lo, _) = piece_at(start, len, p);
            let mut guard = table[p].lock().unwrap_or_else(|e| e.into_inner());
            // SAFETY: `[at, at + n)` lies inside `data` and no GPU work runs during waited copy-back.
            let host = unsafe { std::slice::from_raw_parts_mut(data.add(at), n) };
            let old_ref: &[u8] = match guard.as_deref() {
                Some(buf) => &buf[at - lo..at - lo + n],
                None => &ZERO_PIECE[..n],
            };
            let read = |first: usize, title: &mut [u8]| {
                crate::host::read_into((start + at + first) as *const u8, title)
            };
            let Some((first, stretch)) =
                back_piece(host, old_ref, &mut title, &mut mask, &mut runs, read)
            else {
                // Even if title memory matched `host` (`differs == 0`), sync baseline if `host != old_ref`.
                if host != old_ref {
                    let buf = guard.get_or_insert_with(zero_piece_box);
                    buf[at - lo..at - lo + n].copy_from_slice(host);
                }
                continue;
            };
            let buf = guard.get_or_insert_with(zero_piece_box);
            buf[at - lo + first..at - lo + first + stretch]
                .copy_from_slice(&host[first..first + stretch]);
            drop(guard);
            DIRTY_PIECES_MERGED.fetch_add(1, Ordering::Relaxed);
            let was: Vec<u8> = title
                .iter()
                .zip(&mask)
                .map(|(now, changed)| now ^ changed)
                .collect();
            if !runs.is_empty() {
                written += put_changed(start + at + first, &was[..stretch], &title[..stretch]);
            }
        }
        written
    }

    /// Relay merge of a dirty piece `[at, at + n)` without writing `data`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn relay_merge(
        &self,
        start: usize,
        len: usize,
        data: *mut u8,
        at: usize,
        n: usize,
        now: &mut Vec<u8>,
        title: &mut Vec<u8>,
    ) -> usize {
        let Some(table) = self.pieces.get() else {
            return 0;
        };
        let p = (start + at) / PIECE - start / PIECE;
        let (lo, _) = piece_at(start, len, p);
        let mut guard = table[p].lock().unwrap_or_else(|e| e.into_inner());
        now.clear();
        now.reserve(n);
        // SAFETY: `[at, at + n)` lies inside `data`; copied once into `now`.
        unsafe {
            std::ptr::copy_nonoverlapping(data.add(at), now.as_mut_ptr(), n);
            now.set_len(n);
        }
        if guard.is_none() && now.as_slice() == &ZERO_PIECE[..n] {
            return 0;
        }
        let buf = guard.get_or_insert_with(zero_piece_box);
        let held = &mut buf[at - lo..at - lo + n];
        let Some(first) = first_diff(now, held) else {
            return 0;
        };
        let last = last_diff(now, held).unwrap_or(first);
        let (at_byte, stretch) = (at + first, last + 1 - first);
        let (now_s, held_s) = (&now[first..=last], &mut held[first..=last]);
        title.clear();
        title.resize(stretch, 0);
        if !crate::host::read_into((start + at_byte) as *const u8, title) {
            return 0;
        }
        let was = title.clone();
        if !merge_piece(now_s, held_s, title) {
            return 0;
        }
        drop(guard);
        DIRTY_PIECES_MERGED.fetch_add(1, Ordering::Relaxed);
        put_changed(start + at_byte, &was, title)
    }

    /// Parallel `crate::land::piece` merge of `[at, at + n)` under piece `p`'s own lock.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn land_piece(
        &self,
        start: usize,
        len: usize,
        data: *mut u8,
        at: usize,
        n: usize,
        notes: &mut crate::land::Notes,
        shadow: &Shadow,
    ) -> u64 {
        let Some(table) = self.pieces.get() else {
            return 0;
        };
        let p = (start + at) / PIECE - start / PIECE;
        let (lo, _) = piece_at(start, len, p);
        let mut guard = table[p].lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            // SAFETY: `[at, at + n)` lies inside `data`.
            let same_zero = unsafe { !crate::safecopy::differ(data.add(at), ZERO_PIECE.as_ptr(), n) };
            if same_zero {
                return 0;
            }
        }
        let buf = guard.get_or_insert_with(zero_piece_box);
        // SAFETY: `data.add(at)` and `buf` at `at - lo` both cover `n` bytes; piece lock serializes `held`.
        let out = unsafe {
            crate::land::piece(
                start + at,
                data.add(at),
                buf.as_mut_ptr().add(at - lo),
                &[(0, n)],
                false,
                notes,
                shadow,
            )
        };
        if out > 0 {
            DIRTY_PIECES_MERGED.fetch_add(1, Ordering::Relaxed);
        }
        out
    }

    /// Merges CPU writes in 4 KiB title page `[lo, lo + n)` into `data` and the
    /// piece baseline while flights are in the air (`Shadow::merge_under_writes`).
    pub(crate) fn merge_page_under_writes(
        &self,
        start: usize,
        len: usize,
        data: *mut u8,
        lo: usize,
        n: usize,
    ) -> Option<Vec<(usize, usize)>> {
        let table = self.pieces.get()?;
        let mut title = vec![0u8; PAGE.max(n)];
        let title = &mut title[..n];
        if !crate::host::read_into(lo as *const u8, title) {
            return None;
        }
        let at = lo - start;
        let p = lo / PIECE - start / PIECE;
        let (piece_lo, _) = piece_at(start, len, p);
        let mut guard = table[p].lock().unwrap_or_else(|e| e.into_inner());
        let held_slice: &[u8] = match guard.as_deref() {
            Some(buf) => &buf[at - piece_lo..at - piece_lo + n],
            None => &ZERO_PIECE[..n],
        };
        let runs = changed_runs(title, held_slice);
        if !runs.is_empty() {
            let buf = guard.get_or_insert_with(zero_piece_box);
            let held = &mut buf[at - piece_lo..at - piece_lo + n];
            for &(r, m) in &runs {
                // SAFETY: `[at + r, at + r + m)` lies inside `data`.
                unsafe {
                    std::ptr::copy_nonoverlapping(title.as_ptr().add(r), data.add(at + r), m);
                }
                held[r..r + m].copy_from_slice(&title[r..r + m]);
            }
        }
        Some(runs)
    }
}

fn first_diff(a: &[u8], b: &[u8]) -> Option<usize> {
    a.iter().zip(b).position(|(x, y)| x != y)
}

fn last_diff(a: &[u8], b: &[u8]) -> Option<usize> {
    a.iter().zip(b).rposition(|(x, y)| x != y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    static TEST_SERIAL: Mutex<()> = Mutex::new(());
    use victory_shader::Container;
    use victory_shader::emit::{
        HostShader, SLOT_DIRTY, USER_VIEW_ADDRESS, emit, set_dirty_bitmap_for_test,
    };
    use victory_shader::native::{
        ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
    };
    use victory_shader::{io, ir, resource, structure};
    use windows::Win32::Graphics::Direct3D12::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };

    const S_ENDPGM: u32 = 0xBF81_0000;

    const fn s_mov_b32(sdst: u8, src0: u8) -> u32 {
        0xBE80_0000 | (sdst as u32) << 16 | 0x03 << 8 | src0 as u32
    }
    const fn v_mov_sgpr(vdst: u8, sgpr: u8) -> u32 {
        0x7E00_0200 | (vdst as u32) << 17 | sgpr as u32
    }
    const fn vop2(op: u32, vdst: u32, src0: u32, vsrc1: u32) -> u32 {
        (op << 25) | (vdst << 17) | (vsrc1 << 9) | src0
    }
    const fn s_load_dwordx4_off(dst: u8, base: u8, byte_off: u32) -> [u32; 2] {
        [
            0xF408_0000 | (dst as u32) << 6 | (base as u32 >> 1),
            0xFA00_0000 | (byte_off & 0x1F_FFFF),
        ]
    }
    const fn mubuf_offen(
        op: u8,
        vdata: u8,
        vaddr: u8,
        srsrc: u8,
        inst_off: u16,
        glc: bool,
    ) -> [u32; 2] {
        [
            0x38 << 26 | (op as u32) << 18 | ((glc as u32) << 14) | 1 << 12 | (inst_off as u32 & 0xFFF),
            0x80 << 24 | ((srsrc as u32 >> 2) & 0x1F) << 16 | (vdata as u32) << 8 | vaddr as u32,
        ]
    }

    fn alloc_title(len: usize) -> *mut u8 {
        // SAFETY: commits `len` bytes of read-write virtual memory for test buffers.
        let p = unsafe { VirtualAlloc(std::ptr::null(), len, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) }
            .cast::<u8>();
        assert!(!p.is_null());
        p
    }

    unsafe fn free_title(ptr: *mut u8) {
        // SAFETY: `ptr` was allocated by `alloc_title`.
        unsafe {
            VirtualFree(ptr.cast(), 0, MEM_RELEASE);
        }
    }

    fn make_non_carded_shadow(host: &crate::host::Host, base: *mut u8, len: usize) -> Arc<Shadow> {
        let (buffer, data) = host.shared_buffer(len as u64).expect("shared_buffer");
        // SAFETY: live D3D12 buffer; zero-initialized mapping.
        let gpu = unsafe {
            std::ptr::write_bytes(data, 0, len);
            buffer.GetGPUVirtualAddress()
        };
        let s = Arc::new(Shadow::new(buffer, data, gpu, base as u64, len as u64));
        assert!(!s.carded());
        s
    }

    /// Verifies `extract_pieces` boundary cutting across unaligned copy starts,
    /// multi-word 2 MiB boundaries, and > 128 GiB host pointers.
    #[test]
    fn extract_pieces_cuts_unaligned_and_cross_word_boundaries_exact() {
        let mut bitmap = vec![0u32; DIRTY_WORDS as usize];
        let mark = |bm: &mut [u32], addr: usize| {
            let idx = (addr >> 16) & (MEMORY_PAGES as usize - 1);
            bm[idx >> 5] |= 1 << (idx & 31);
        };
        // Unaligned start `0x02BF_0000_1000` (> 128 GiB!), span `0x20_0000` (32 pages + 2 ends).
        let start = 0x02BF_0000_1000usize;
        let len = 0x22_0000usize;
        mark(&mut bitmap, start + 0x100); // piece 0: `[0, 0xF000)`
        mark(&mut bitmap, start + 0x1F_0000); // piece across word boundary
        mark(&mut bitmap, start + len - 4); // final partial piece `[0x21F000, 0x220000)`

        let got = extract_pieces(start, 0, len, |w| bitmap[w]);
        assert_eq!(
            got,
            vec![
                (0, 0xF000),
                (0x1E_F000, 0x1_0000),
                (0x21_F000, 0x1000),
            ]
        );

        // Sub-range `[0x5000, 0x21_F800)` trims both first and last pieces cleanly.
        let sub = extract_pieces(start, 0x5000, 0x21_F800, |w| bitmap[w]);
        assert_eq!(
            sub,
            vec![
                (0x5000, 0xA000),
                (0x1E_F000, 0x1_0000),
                (0x21_F000, 0x800),
            ]
        );
    }

    /// Verifies `DirtyBefore` preserves concurrent CPU writes in the same 64 KiB
    /// page while landing GPU-changed dwords across multiple submissions.
    #[test]
    fn dirty_before_preserves_concurrent_cpu_writes_in_same_page() {
        let Some(host) = crate::host::host() else {
            return;
        };
        let len = 3 * PIECE;
        let title_ptr = alloc_title(len);
        // SAFETY: `title_ptr` owns `len` committed bytes.
        let title = unsafe { std::slice::from_raw_parts_mut(title_ptr, len) };
        title[0..4].copy_from_slice(&0x1111_1111u32.to_le_bytes());
        title[64..68].copy_from_slice(&0x2222_2222u32.to_le_bytes());

        let shadow = make_non_carded_shadow(host, title_ptr, len);
        shadow.update(title_ptr as usize, len);
        shadow.writing_with(title_ptr as usize, len, true);

        // Simulate GPU writing dword at offset 16, while CPU concurrently writes dword at offset 64!
        // SAFETY: test owns `shadow.buffer` mapping (`len` bytes).
        unsafe {
            let gpu_map = std::slice::from_raw_parts_mut(shadow.card_data().cast_mut(), len);
            gpu_map[16..20].copy_from_slice(&0xAAAA_5555u32.to_le_bytes());
        }
        title[64..68].copy_from_slice(&0x9999_8888u32.to_le_bytes());

        let written = shadow.copy_back_dirty(title_ptr as usize, len, vec![(0, PIECE)]);
        shadow.copy_back_done(&[(title_ptr as usize, title_ptr as usize + len)]);
        assert_eq!(written, 4);
        assert_eq!(u32::from_le_bytes(title[0..4].try_into().unwrap()), 0x1111_1111);
        assert_eq!(u32::from_le_bytes(title[16..20].try_into().unwrap()), 0xAAAA_5555);
        assert_eq!(
            u32::from_le_bytes(title[64..68].try_into().unwrap()),
            0x9999_8888,
            "CPU concurrent write in the same 64 KiB page must never be clobbered"
        );

        // Second submission on the same page writes offset 128; offset 64 must still stay `0x9999_8888`.
        shadow.writing_with(title_ptr as usize, len, true);
        // SAFETY: as above.
        unsafe {
            let gpu_map = std::slice::from_raw_parts_mut(shadow.card_data().cast_mut(), len);
            gpu_map[128..132].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        }
        let written2 = shadow.copy_back_dirty(title_ptr as usize, len, vec![(0, PIECE)]);
        shadow.copy_back_done(&[(title_ptr as usize, title_ptr as usize + len)]);
        assert_eq!(written2, 4);
        assert_eq!(u32::from_le_bytes(title[64..68].try_into().unwrap()), 0x9999_8888);
        assert_eq!(u32::from_le_bytes(title[128..132].try_into().unwrap()), 0xDEAD_BEEF);

        drop(shadow);
        // SAFETY: `title_ptr` is no longer referenced.
        unsafe { free_title(title_ptr) };
    }

    struct BenchCs {
        shader: HostShader,
        root: ID3D12RootSignature,
        pso: ID3D12PipelineState,
        user_dwords: u32,
        dirty_param: Option<u32>,
        dirty_tracked: bool,
    }

    impl BenchCs {
        /// Translates a GFX10 compute shader (`[numthreads(64, 1, 1)]`) that writes
        /// `tag + tid.x` at byte offset `byte_base + tgid.x * 3 MiB + tid.x * 4`
        /// through a static descriptor-table raw buffer UAV.
        fn compile(host: &crate::host::Host, enable_dirty: bool) -> Self {
            set_dirty_bitmap_for_test(Some(enable_dirty));
            let load_uav = s_load_dwordx4_off(8, 4, 0);
            let store = mubuf_offen(0x1C, 3, 2, 8, 0, false);
            let prog_words = [
                v_mov_sgpr(2, 5),           // v2 = s5 (tgid.x before s5 becomes 0x80)
                s_mov_b32(5, 0x80),         // s[4:5] = 64-bit table address
                load_uav[0],
                load_uav[1],
                vop2(0x1A, 1, 128 + 2, 0),  // v1 = tid.x * 4
                vop2(0x1A, 4, 128 + 1, 2),  // v4 = tgid.x * 2
                vop2(0x25, 2, 256 + 4, 2),  // v2 = tgid.x * 3
                vop2(0x1A, 2, 128 + 20, 2), // v2 = tgid.x * 3 MiB
                vop2(0x25, 2, 0, 2),        // v2 = s0 (byte_base) + v2
                vop2(0x25, 2, 256 + 1, 2),  // v2 = v1 + v2
                vop2(0x25, 3, 1, 0),        // v3 = s1 (tag) + v0
                store[0],
                store[1],
                S_ENDPGM,
            ];
            let native = NativeProgram {
                stage: HwStage::Cs,
                wave64: false,
                vgprs: 8,
                layout: vec![
                    RootParam {
                        root: RootType::Constants,
                        table_offset_dwords: 0,
                        sgprs: 4,
                        base_register: 0,
                        user_data: 0,
                        slot: 0,
                    },
                    RootParam {
                        root: RootType::Table,
                        table_offset_dwords: 0,
                        sgprs: 1,
                        base_register: 0,
                        user_data: 4,
                        slot: 1,
                    },
                ],
                used: vec![UsedResource {
                    kind: ResourceType::Uav,
                    root: RootType::Table,
                    table_offset_dwords: 0,
                    lower_bound: 0,
                    stride: 0,
                    dwords: 4,
                    user_data: 4,
                    slot: 1,
                }],
                io: StageIo::Cs(ComputeIo {
                    rsrc2: (5 << 1) | (1 << 7),
                    threads: [64, 1, 1],
                }),
                code: prog_words.iter().flat_map(|w| w.to_le_bytes()).collect(),
            };
            let container = Container {
                digest: [0; 16],
                version: (1, 0),
                parts: Vec::new(),
            };
            let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
            let st = structure::structure(&p).expect("structure");
            let res = resource::track(&p, &native.layout, native.stage, Some(&st))
                .expect("resource::track");
            let io_s = io::raise(&p, &native).expect("io::raise");
            let sh = emit(&container, &native, &p, &st, &io_s, &res).expect("emit CS");
            set_dirty_bitmap_for_test(None);
            assert_eq!(sh.dirty_tracked, enable_dirty);

            let user_dwords = sh.user_data_dwords().max(1);
            let mut params = vec![D3D12_ROOT_PARAMETER1 {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                Anonymous: D3D12_ROOT_PARAMETER1_0 {
                    Constants: D3D12_ROOT_CONSTANTS {
                        ShaderRegister: 0,
                        RegisterSpace: 0,
                        Num32BitValues: user_dwords,
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            }];
            let mut dirty_param = None;
            for &u in &sh.root_uavs {
                assert_eq!(u, SLOT_DIRTY);
                dirty_param = Some(params.len() as u32);
                params.push(D3D12_ROOT_PARAMETER1 {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_UAV,
                    Anonymous: D3D12_ROOT_PARAMETER1_0 {
                        Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                            ShaderRegister: u32::from(u),
                            RegisterSpace: victory_shader::emit::ROOT_CBV_SPACE,
                            Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
                        },
                    },
                    ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                });
            }
            let versioned = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
                Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
                Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
                    Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                        NumParameters: params.len() as u32,
                        pParameters: params.as_ptr(),
                        NumStaticSamplers: 0,
                        pStaticSamplers: std::ptr::null(),
                        Flags: D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED
                            | D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED,
                    },
                },
            };
            let mut blob = None;
            let mut err = None;
            // SAFETY: valid root signature and validated SM 6.6 DXIL shader.
            let (root, pso) = unsafe {
                D3D12SerializeVersionedRootSignature(&versioned, &mut blob, Some(&mut err))
                    .expect("serialize root sig");
                let b = blob.unwrap();
                let root: ID3D12RootSignature = host
                    .device
                    .CreateRootSignature(
                        0,
                        std::slice::from_raw_parts(b.GetBufferPointer().cast(), b.GetBufferSize()),
                    )
                    .expect("create root sig");
                let pso: ID3D12PipelineState = host
                    .device
                    .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::transmute_copy(&root),
                        CS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: sh.dxil.as_ptr().cast(),
                            BytecodeLength: sh.dxil.len(),
                        },
                        ..Default::default()
                    })
                    .expect("create PSO");
                (root, pso)
            };
            let dirty_tracked = sh.dirty_tracked;
            Self {
                shader: sh,
                root,
                pso,
                user_dwords,
                dirty_param,
                dirty_tracked,
            }
        }

        #[allow(clippy::too_many_arguments)]
        fn record(
            &self,
            list: &crate::host::HostList,
            heaps: &[Option<ID3D12DescriptorHeap>],
            desc_index: u32,
            view_title_addr: u64,
            byte_base: u32,
            tag: u32,
            groups: u32,
        ) {
            let constants = [byte_base, tag, 0, 0];
            let mut user = vec![0u32; self.user_dwords as usize];
            for u in &self.shader.user_data {
                user[u.index as usize] = match u.root {
                    RootType::Constants => constants.get(u.dword as usize).copied().unwrap_or(0),
                    RootType::Table => desc_index * 32,
                    RootType::Other(k) if k == USER_VIEW_ADDRESS => view_title_addr as u32,
                    RootType::Other(k) if k == USER_VIEW_ADDRESS + 1 => {
                        (view_title_addr >> 32) as u32
                    }
                    _ => 0,
                };
            }
            // SAFETY: open host command list with live heaps, root signature, PSO, and `SLOT_DIRTY`.
            unsafe {
                list.list.SetDescriptorHeaps(heaps);
                list.list.SetComputeRootSignature(&self.root);
                list.list.SetPipelineState(&self.pso);
                list.list.SetComputeRoot32BitConstants(
                    0,
                    self.user_dwords,
                    user.as_ptr().cast(),
                    0,
                );
                if let Some(p) = self.dirty_param {
                    let gpu = list.dirty_gpu().expect("dirty_gpu");
                    list.list.SetComputeRootUnorderedAccessView(p, gpu);
                }
                list.list.Dispatch(groups, 1, 1);
            }
            list.bind_pipeline("bench_dirty_cs", &self.pso, &self.root);
        }
    }

    fn make_test_heaps(
        host: &crate::host::Host,
        shadow: &Shadow,
        views: &[(u64, u32)],
    ) -> (ID3D12DescriptorHeap, ID3D12DescriptorHeap) {
        // SAFETY: plain shader-visible descriptor heap creation on host device.
        unsafe {
            let view_heap: ID3D12DescriptorHeap = host
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 16,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .expect("view heap");
            let sampler_heap: ID3D12DescriptorHeap = host
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER,
                    NumDescriptors: 4,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .expect("sampler heap");
            let step = host
                .device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                as usize;
            let cpu0 = view_heap.GetCPUDescriptorHandleForHeapStart();
            for (i, &(byte_off, num_dwords)) in views.iter().enumerate() {
                assert_eq!(byte_off % 4, 0);
                let uav = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                    Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32_TYPELESS,
                    ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                    Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_UAV {
                            FirstElement: byte_off / 4,
                            NumElements: num_dwords,
                            StructureByteStride: 0,
                            CounterOffsetInBytes: 0,
                            Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                        },
                    },
                };
                let _ = D3D12_UAV_DIMENSION_BUFFER;
                host.device.CreateUnorderedAccessView(
                    &shadow.buffer,
                    None,
                    Some(&uav),
                    D3D12_CPU_DESCRIPTOR_HANDLE {
                        ptr: cpu0.ptr + i * step,
                    },
                );
            }
            (view_heap, sampler_heap)
        }
    }

    /// End-to-end GPU execution on RTX 5060 Ti through `Host::execute` + `Shadow`
    /// with overlapping sub-views at unaligned offsets, cross-64KiB-page writes,
    /// multi-dispatch submissions, and `VICTORY_DIRTY_CHECK=1` asserting zero missed pieces.
    #[test]
    fn gpu_overlapping_subviews_and_dirty_check_zero_missed_pieces() {
        let _serial = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let Some(host) = crate::host::host() else {
            return;
        };
        let cs = BenchCs::compile(host, true);
        assert!(cs.dirty_tracked);

        let len = 8 << 20; // 8 MiB
        let title_ptr = alloc_title(len);
        // SAFETY: `title_ptr` owns `len` committed bytes.
        let title = unsafe { std::slice::from_raw_parts_mut(title_ptr, len) };
        title[0x100..0x104].copy_from_slice(&0xCAFE_BABEu32.to_le_bytes());

        let shadow = make_non_carded_shadow(host, title_ptr, len);
        // Two overlapping sub-views of the same `Shadow`:
        // View 0 starts at `+0x0` (8 MiB), View 1 starts at `+0x1_4000` (unaligned to 2 MiB, +80 KiB!).
        let subview1_off = 0x1_4000u64;
        let (vh, sh) = make_test_heaps(
            host,
            &shadow,
            &[
                (0, (len / 4) as u32),
                (subview1_off, ((len - subview1_off as usize) / 4) as u32),
            ],
        );
        let heaps = [Some(vh.clone()), Some(sh.clone())];

        let checked_before = CHECKED_RANGES.load(Ordering::Relaxed);
        let missed_before = MISSED_PIECES.load(Ordering::Relaxed);
        set_check_for_test(Some(true));

        let list = host.new_list().expect("new_list");
        list.keep(&vh);
        list.keep(&sh);
        let start = title_ptr as u64;
        shadow.refresh(&list, start, len as u64);
        // Declare overlapping sub-views as tracked GPU writes!
        shadow.gpu_writes_dirty(&list, start, len as u64, true);
        shadow.gpu_writes_dirty(&list, start + subview1_off, len as u64 - subview1_off, true);

        // Dispatch 1 through View 0: writes across a 64 KiB boundary (`0xFFF0..0x100F0`! 64 lanes * 4 = 256 bytes!).
        cs.record(&list, &heaps, 0, start, 0xFFF0, 0x1000_0000, 2);
        // Dispatch 2 through overlapping View 1 (`start + 0x1_4000`): writes at `subview1 + 0x200`!
        cs.record(
            &list,
            &heaps,
            1,
            start + subview1_off,
            0x200,
            0x2000_0000,
            2,
        );
        list.close();
        host.execute(&[&list]);
        list.reset();

        set_check_for_test(None);
        assert!(CHECKED_RANGES.load(Ordering::Relaxed) > checked_before);
        assert_eq!(
            MISSED_PIECES.load(Ordering::Relaxed),
            missed_before,
            "VICTORY_DIRTY_CHECK=1 must observe zero missed dirty 64 KiB pieces"
        );

        // Verify exact landing in title memory across all written pages AND untouched sentinel!
        assert_eq!(u32::from_le_bytes(title[0x100..0x104].try_into().unwrap()), 0xCAFE_BABE);
        // Cross-64KiB-boundary lanes from Dispatch 1 (lane 0 at `0xFFF0` in page 0, lane 8 at `0x10010` in page 1, group 1 at `+3 MiB`!).
        assert_eq!(u32::from_le_bytes(title[0xFFF0..0xFFF4].try_into().unwrap()), 0x1000_0000);
        assert_eq!(u32::from_le_bytes(title[0x1_0010..0x1_0014].try_into().unwrap()), 0x1000_0008);
        assert_eq!(
            u32::from_le_bytes(title[0x30_FFF0..0x30_FFF4].try_into().unwrap()),
            0x1000_0000
        );
        // Overlapping sub-view Dispatch 2 (`0x1_4000 + 0x200 = 0x1_4200` and `+3 MiB = 0x31_4200`).
        assert_eq!(u32::from_le_bytes(title[0x1_4200..0x1_4204].try_into().unwrap()), 0x2000_0000);
        assert_eq!(
            u32::from_le_bytes(title[0x31_4200..0x31_4204].try_into().unwrap()),
            0x2000_0000
        );

        // Now run a second submission in pure `VICTORY_DIRTY_BITMAP=1` mode (no `Shadow::before`!)
        // while mutating `title[0x104..0x108]` on the CPU right before `execute`!
        let skipped_before = TRACKED_SKIPPED_BYTES.load(Ordering::Relaxed);
        let list2 = host.new_list().expect("new_list 2");
        list2.keep(&vh);
        list2.keep(&sh);
        shadow.gpu_writes_dirty(&list2, start + subview1_off, len as u64 - subview1_off, true);
        cs.record(
            &list2,
            &heaps,
            1,
            start + subview1_off,
            0x600,
            0x3000_0000,
            1,
        );
        list2.close();
        title[0x1_4100..0x1_4104].copy_from_slice(&0x7777_6666u32.to_le_bytes());
        host.execute(&[&list2]);
        list2.reset();

        assert!(TRACKED_SKIPPED_BYTES.load(Ordering::Relaxed) > skipped_before);
        assert_eq!(
            u32::from_le_bytes(title[0x1_4100..0x1_4104].try_into().unwrap()),
            0x7777_6666,
            "CPU write in same 64 KiB page during GPU submission must remain intact"
        );
        assert_eq!(u32::from_le_bytes(title[0x1_4600..0x1_4604].try_into().unwrap()), 0x3000_0000);

        drop(shadow);
        // SAFETY: `title_ptr` is no longer referenced.
        unsafe { free_title(title_ptr) };
    }

    /// Verifies mixed tracked (`SLOT_DIRTY`) + untracked (`Shadow::before`)
    /// dispatches in the same submission on overlapping ranges, plus
    /// `relay_merge_dirty` and `land_dirty` with concurrent CPU writes.
    #[test]
    fn mixed_tracked_and_untracked_and_relay_land_dirty() {
        let _serial = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let Some(host) = crate::host::host() else {
            return;
        };
        let cs_on = BenchCs::compile(host, true);
        let cs_off = BenchCs::compile(host, false);

        let len = 8 * PIECE;
        let title_ptr = alloc_title(len);
        // SAFETY: `title_ptr` owns `len` committed bytes.
        let title = unsafe { std::slice::from_raw_parts_mut(title_ptr, len) };
        let shadow = make_non_carded_shadow(host, title_ptr, len);
        let unt_off = 2 * PIECE as u64;
        let unt_len = 2 * PIECE as u64;
        let (vh, sh) = make_test_heaps(
            host,
            &shadow,
            &[
                (0, (len / 4) as u32),
                (unt_off, (unt_len / 4) as u32),
            ],
        );
        let heaps = [Some(vh.clone()), Some(sh.clone())];
        let start = title_ptr as u64;

        let list = host.new_list().expect("new_list");
        list.keep(&vh);
        list.keep(&sh);
        shadow.refresh(&list, start, len as u64);
        // Tracked dispatch declares the whole `[0, 8*PIECE)` FIRST, then untracked declares `[2*PIECE, 4*PIECE)`.
        shadow.gpu_writes_dirty(&list, start, len as u64, true);
        shadow.gpu_writes(&list, start + unt_off, unt_len);
        cs_on.record(&list, &heaps, 0, start, 0x400, 0x4400_0000, 1);
        cs_off.record(&list, &heaps, 1, start + unt_off, 0x800, 0x5500_0000, 1);
        list.close();
        host.execute(&[&list]);
        list.reset();

        assert_eq!(u32::from_le_bytes(title[0x400..0x404].try_into().unwrap()), 0x4400_0000);
        let unt_check = unt_off as usize + 0x800;
        assert_eq!(
            u32::from_le_bytes(title[unt_check..unt_check + 4].try_into().unwrap()),
            0x5500_0000
        );

        // Verify `relay_merge_dirty` and `land_dirty` preserve concurrent CPU writes in the same piece.
        shadow.writing_with(title_ptr as usize, len, true);
        // SAFETY: test owns `shadow.buffer` mapping (`len` bytes).
        unsafe {
            let gpu_map = std::slice::from_raw_parts_mut(shadow.card_data().cast_mut(), len);
            gpu_map[6 * PIECE + 32..6 * PIECE + 36]
                .copy_from_slice(&0x6600_1234u32.to_le_bytes());
            gpu_map[7 * PIECE + 48..7 * PIECE + 52]
                .copy_from_slice(&0x7700_5678u32.to_le_bytes());
        }
        title[6 * PIECE + 96..6 * PIECE + 100].copy_from_slice(&0xCAFE_1111u32.to_le_bytes());
        title[7 * PIECE + 96..7 * PIECE + 100].copy_from_slice(&0xCAFE_2222u32.to_le_bytes());

        let (mut now_buf, mut title_buf) = (Vec::new(), Vec::new());
        let r_written = shadow.relay_merge_dirty(6 * PIECE, PIECE, &mut now_buf, &mut title_buf);
        let mut notes = crate::land::Notes::default();
        let l_written = shadow.land_dirty(7 * PIECE, PIECE, &mut notes);
        notes.flush();
        shadow.copy_back_done(&[(title_ptr as usize, title_ptr as usize + len)]);

        assert!(r_written >= 4 && l_written >= 4);
        assert_eq!(
            u32::from_le_bytes(title[6 * PIECE + 32..6 * PIECE + 36].try_into().unwrap()),
            0x6600_1234
        );
        assert_eq!(
            u32::from_le_bytes(title[6 * PIECE + 96..6 * PIECE + 100].try_into().unwrap()),
            0xCAFE_1111
        );
        assert_eq!(
            u32::from_le_bytes(title[7 * PIECE + 48..7 * PIECE + 52].try_into().unwrap()),
            0x7700_5678
        );
        assert_eq!(
            u32::from_le_bytes(title[7 * PIECE + 96..7 * PIECE + 100].try_into().unwrap()),
            0xCAFE_2222
        );

        drop(shadow);
        // SAFETY: `title_ptr` is no longer referenced.
        unsafe { free_title(title_ptr) };
    }

    /// Measures end-to-end submission + GPU execution + copy-back latency on a
    /// **174 MiB (`0xAE0_0000`) non-carded `Shadow` buffer** with overlapping sub-views
    /// comparing `VICTORY_DIRTY_BITMAP=0` (`Shadow::before` 174 MiB snapshot + full diff)
    /// against `VICTORY_DIRTY_BITMAP=1` (`SLOT_DIRTY` bitmap + sparse dirty-piece merge).
    #[test]
    fn bench_174mb_non_carded_shadow_dirty_bitmap_vs_before() {
        let _serial = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let Some(host) = crate::host::host() else {
            return;
        };
        const HEAP_BYTES: usize = 174 << 20; // 0x0AE0_0000 (174 MiB)
        const ROUNDS: usize = 6;
        let cs_off = BenchCs::compile(host, false);
        let cs_on = BenchCs::compile(host, true);

        let title_ptr = alloc_title(HEAP_BYTES);
        // SAFETY: `title_ptr` owns `HEAP_BYTES` committed bytes.
        let title = unsafe { std::slice::from_raw_parts_mut(title_ptr, HEAP_BYTES) };
        // Touch pages upfront so OS page-commit faults don't skew either measurement.
        for off in (0..HEAP_BYTES).step_by(4096) {
            title[off] = (off >> 12) as u8;
        }

        let run_mode = |enable_dirty: bool, cs: &BenchCs| -> Vec<f64> {
            let shadow = make_non_carded_shadow(host, title_ptr, HEAP_BYTES);
            shadow.update(title_ptr as usize, HEAP_BYTES);
            let (vh, sh) = make_test_heaps(
                host,
                &shadow,
                &[
                    (0, (HEAP_BYTES / 4) as u32),
                    (0x10_0000, ((HEAP_BYTES - 0x10_0000) / 4) as u32),
                    (0x28_0000, ((HEAP_BYTES - 0x28_0000) / 4) as u32),
                ],
            );
            let heaps = [Some(vh.clone()), Some(sh.clone())];
            let start = title_ptr as u64;

            // Warm-up submission (populates `DirtyBefore` or `POOL` snapshot once).
            {
                let list = host.new_list().unwrap();
                list.keep(&vh);
                list.keep(&sh);
                shadow.gpu_writes_dirty(&list, start, HEAP_BYTES as u64, enable_dirty);
                cs.record(&list, &heaps, 0, start, 0x200, 0xAA00_0000, 8);
                list.close();
                host.execute(&[&list]);
                list.reset();
            }

            let mut samples_ms = Vec::with_capacity(ROUNDS);
            for r in 0..ROUNDS {
                let tag = 0x5000_0000u32 + (r as u32) * 0x100;
                let list = host.new_list().unwrap();
                list.keep(&vh);
                list.keep(&sh);
                // Declare 11 overlapping sub-view writes across the 174 MiB heap (like skinning/world passes),
                // touching 8 dirty 64 KiB pages (~512 KiB) out of 174 MiB.
                for k in 0..11u64 {
                    let off = k * 0x4_0000;
                    shadow.gpu_writes_dirty(
                        &list,
                        start + off,
                        HEAP_BYTES as u64 - off,
                        enable_dirty,
                    );
                }
                cs.record(&list, &heaps, 0, start, 0x400 + (r as u32) * 4, tag, 4);
                cs.record(
                    &list,
                    &heaps,
                    1,
                    start + 0x10_0000,
                    0x800 + (r as u32) * 4,
                    tag ^ 0x0F00_0000,
                    4,
                );
                list.close();

                let t0 = Instant::now();
                host.execute(&[&list]);
                samples_ms.push(t0.elapsed().as_secs_f64() * 1e3);
                list.reset();

                let check_off = 0x400 + r * 4;
                assert_eq!(
                    u32::from_le_bytes(title[check_off..check_off + 4].try_into().unwrap()),
                    tag
                );
                let check_off2 = 0x10_0800 + r * 4;
                assert_eq!(
                    u32::from_le_bytes(title[check_off2..check_off2 + 4].try_into().unwrap()),
                    tag ^ 0x0F00_0000
                );
            }
            samples_ms
        };

        let mut before_ms = run_mode(false, &cs_off);
        let mut dirty_ms = run_mode(true, &cs_on);
        before_ms.sort_by(f64::total_cmp);
        dirty_ms.sort_by(f64::total_cmp);
        let before_med = before_ms[ROUNDS / 2];
        let dirty_med = dirty_ms[ROUNDS / 2];
        let before_mean = before_ms.iter().sum::<f64>() / ROUNDS as f64;
        let dirty_mean = dirty_ms.iter().sum::<f64>() / ROUNDS as f64;
        println!(
            "174 MiB non-carded Shadow (8 dirty pages, 11 overlapping views, {ROUNDS} rounds): \
             VICTORY_DIRTY_BITMAP=0 median {before_med:.3} ms (mean {before_mean:.3} ms, {before_ms:.3?}) vs \
             VICTORY_DIRTY_BITMAP=1 median {dirty_med:.3} ms (mean {dirty_mean:.3} ms, {dirty_ms:.3?}) -> {:.1}x speedup",
            before_med / dirty_med.max(1e-6)
        );
        assert!(
            dirty_med < before_med,
            "SLOT_DIRTY bitmap ({dirty_med:.3} ms) should beat 174 MiB CPU snapshot+diff ({before_med:.3} ms)"
        );

        // SAFETY: `title_ptr` is no longer referenced.
        unsafe { free_title(title_ptr) };
    }
}
