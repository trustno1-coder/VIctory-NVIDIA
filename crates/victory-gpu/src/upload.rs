//! Memory `allocate_upload` (`list.rs`) hands out: the title's CPU writes
//! it and its shaders read it at the same address (the structure builder's
//! instance descriptions lie there).
//!
//! The console's GPU addresses have 40 bits, and a translated shader that
//! reads by address has no more (`victory_shader`'s `Lower::memory`): memory
//! from the process heap lies above 2^40 and was read at another address, as
//! zeros. So this memory is taken in blocks of 64 KiB out of arenas in
//! [4 GiB, 2^37), as upstream VIctory 0f637570 and 6d021fe3 place it (there
//! an allocation of its own each). An arena is a title buffer to the host
//! copies ([`crate::shadow`]) and lives as long as the process: the directory
//! of memory by address ([`crate::memory`]) keeps the copy it found at an
//! address for good, so the same addresses must stay the same copy.
//! `VICTORY_UPLOAD_LOW=0`: from the process heap, as before.

use std::sync::{Mutex, OnceLock};

/// What blocks are cut on: a page of the directory of memory by address.
const BLOCK: usize = 64 << 10;
/// An arena's size, if one request does not need more.
const ARENA: usize = 4 << 20;

struct Arena {
    at: usize,
    /// Per block, the upload's exclusive end offset plus one. Zero means free.
    ends: Vec<usize>,
}

impl Arena {
    fn take(&mut self, first: usize, blocks: usize, len: usize) {
        let end = first * BLOCK + len;
        self.ends[first..first + blocks].fill(end + 1);
    }

    fn block_at(&self, address: usize) -> Option<(usize, usize)> {
        let offset = address.checked_sub(self.at)?;
        let first = offset / BLOCK;
        let end = self.ends.get(first)?.checked_sub(1)?;
        if offset >= end {
            return None;
        }
        let start = first * BLOCK;
        Some((self.at + start, end - start))
    }
}

static ARENAS: Mutex<Vec<Arena>> = Mutex::new(Vec::new());

/// Upload memory of one request, zeroed, given back when dropped.
pub(crate) struct Upload(Kind);

enum Kind {
    /// Address, bytes.
    Low(usize, usize),
    Heap(Vec<u8>),
}

fn low() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_UPLOAD_LOW").is_none_or(|v| v != "0"))
}

/// The first place of `n` free blocks side by side.
fn gap(ends: &[usize], n: usize) -> Option<usize> {
    let mut free = 0;
    for (i, &end) in ends.iter().enumerate() {
        free = if end == 0 { free + 1 } else { 0 };
        if free == n {
            return Some(i + 1 - n);
        }
    }
    None
}

const MEM_WRITE_WATCH: u32 = 0x20_0000;
const MEM_TOP_DOWN: u32 = 0x10_0000;

/// `VirtualAlloc2` in [4 GiB, 2^37): its address, or the error.
fn allocate_in_window(len: usize, kind: u32) -> Result<usize, u32> {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::Memory::{
        MEM_ADDRESS_REQUIREMENTS, MEM_EXTENDED_PARAMETER, MemExtendedParameterAddressRequirements, PAGE_READWRITE, VirtualAlloc2,
    };
    let mut requirements = MEM_ADDRESS_REQUIREMENTS {
        LowestStartingAddress: (1usize << 32) as *mut _,
        HighestEndingAddress: ((1usize << 37) - 1) as *mut _,
        Alignment: 0,
    };
    let mut parameter = MEM_EXTENDED_PARAMETER::default();
    parameter.Anonymous1._bitfield = MemExtendedParameterAddressRequirements as u64;
    parameter.Anonymous2.Pointer = (&raw mut requirements).cast();
    // SAFETY: a fresh allocation; the parameter points at `requirements`,
    // which outlives the call.
    let at = unsafe { VirtualAlloc2(std::ptr::null_mut(), std::ptr::null(), len, kind, PAGE_READWRITE, &mut parameter, 1) }
        as usize;
    if at == 0 {
        // SAFETY: plain call.
        Err(unsafe { GetLastError() })
    } else {
        Ok(at)
    }
}

/// Pieces of address space in [4 GiB, 2^37), [`EARLY_PIECE`] each, reserved
/// at the start (`VICTORY_LOW_EARLY_MB`, 3,200 with the structure
/// builder, else 0). [`reserve`] commits a whole one for a request of that
/// size (an arena of memory by address) before it looks for room.
///
/// By the time the builder runs, the title's mappings and ours fill that
/// range: no 64 MiB gap was left after 19 of the builder's arenas (RTX 4090,
/// 11-27), and the pages it could not hold read zeros (broken reflections,
/// flicker). Reserved before the title spreads, the range costs no memory
/// until a piece is committed. Each piece is a reservation of its own: as
/// one reservation of 3.2 GB the submissions' write-watch askings ran
/// across the arenas in single long walks, 2 ms a frame more.
static EARLY: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// The size of an [`EARLY`] piece: `memory.rs`'s `ARENA`.
const EARLY_PIECE: usize = 64 << 20;

/// Reserves [`EARLY`]; once, as early as the host is made.
pub(crate) fn reserve_early() {
    use windows_sys::Win32::System::Memory::MEM_RESERVE;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let builder = std::env::var_os("VICTORY_BVH_BUILDER").is_some_and(|v| v == "1");
        let mb = std::env::var("VICTORY_LOW_EARLY_MB")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(if builder { 3200 } else { 0 });
        let mut early = EARLY.lock().unwrap_or_else(|e| e.into_inner());
        for _ in 0..(mb << 20) / EARLY_PIECE {
            let Ok(at) = allocate_in_window(EARLY_PIECE, MEM_RESERVE | MEM_WRITE_WATCH) else {
                break;
            };
            early.push(at);
        }
        // The lowest first.
        early.sort_unstable_by(|a, b| b.cmp(a));
        if mb > 0 {
            tracing::info!(mb, pieces = early.len(), "low memory reserved early (VICTORY_LOW_EARLY_MB)");
        }
    });
}

/// A whole [`EARLY`] piece, committed, for a request of its size.
fn from_early(len: usize) -> Option<usize> {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, PAGE_READWRITE, VirtualAlloc};
    if len != EARLY_PIECE {
        return None;
    }
    commit_early_piece(&EARLY, |at| {
        // SAFETY: a reservation of our own, not committed before.
        (unsafe { VirtualAlloc(at as *const _, len, MEM_COMMIT, PAGE_READWRITE) } as usize) == at
    })
}

fn commit_early_piece(pieces: &Mutex<Vec<usize>>, commit: impl FnOnce(usize) -> bool) -> Option<usize> {
    let mut pieces = pieces.lock().unwrap_or_else(|e| e.into_inner());
    let &at = pieces.last()?;
    if commit(at) {
        return pieces.pop();
    }
    None
}

/// The address space every run reserves first thing ([`reserve_space`]),
/// at the top of [4 GiB, 2^37): [`POOL_PIECES`] reservations of
/// [`EARLY_PIECE`] (one each, for the write-watch walks as above), before
/// anything else can fill that window. On players' PCs it was full by the
/// first ray-traced frame (the title's 2 GiB pool went above 2^37 there):
/// the first upload asked for failed, and the title threw "D3D12 XDXR
/// Error: Failed to allocate upload buffer" and ended the game (RTX 4090,
/// 5080, 5090, 11-27).
const POOL_PIECES: usize = 32;

/// Per piece of the pool: its start and how much of it is handed out.
static POOL: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

/// Reserves the pool (address space only, nothing committed). Called first
/// thing in `main`; [`reserve`] does it otherwise.
pub fn reserve_space() {
    use windows_sys::Win32::System::Memory::MEM_RESERVE;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
        for _ in 0..POOL_PIECES {
            let Ok(at) = allocate_in_window(EARLY_PIECE, MEM_RESERVE | MEM_WRITE_WATCH | MEM_TOP_DOWN) else {
                break;
            };
            pool.push((at, 0));
        }
    });
}

/// `len` bytes of the pool committed, from the first piece with room.
fn from_pool(len: usize) -> Option<usize> {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, PAGE_READWRITE, VirtualAlloc};
    static TOLD: std::sync::Once = std::sync::Once::new();
    let len = len.div_ceil(BLOCK).checked_mul(BLOCK)?;
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    TOLD.call_once(|| {
        let lowest = pool.iter().map(|p| p.0).min().unwrap_or(0);
        tracing::info!(
            pieces = pool.len(),
            lowest = format_args!("{lowest:#x}"),
            "address space for memory shaders read by address, reserved at the start"
        );
    });
    let (start, used) = pool.iter_mut().find(|(_, used)| EARLY_PIECE - *used >= len)?;
    let want = *start + *used;
    // SAFETY: committing pages of our own reservation, none handed out before.
    let at = unsafe { VirtualAlloc(want as *const _, len, MEM_COMMIT, PAGE_READWRITE) } as usize;
    (at == want).then(|| {
        *used += len;
        at
    })
}

/// Fresh zeroed memory in [4 GiB, 2^37) (where the directory of memory by
/// address reaches, `MEMORY_PAGES`), write-watched as the title's own
/// pools are (copies then take only written pages). From [`EARLY`], then
/// the pool, then wherever the window has room. A search of a full window
/// takes long in the kernel; after one failed, none is made for a second
/// (PR #88: thousands of searches a frame, 30 s frames, the title's
/// watchdog).
pub(crate) fn reserve(len: usize) -> Option<usize> {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE};
    static FAILED_AT_MS: AtomicU64 = AtomicU64::new(0);
    static FAILURES: AtomicU64 = AtomicU64::new(0);
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    reserve_space();
    if let Some(at) = from_early(len).or_else(|| from_pool(len)) {
        return Some(at);
    }
    let now = START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + 1;
    let failed = FAILED_AT_MS.load(Relaxed);
    if failed != 0 && now < failed + 1000 {
        return None;
    }
    match allocate_in_window(len, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH) {
        Ok(at) => Some(at),
        Err(error) => {
            FAILED_AT_MS.store(now, Relaxed);
            let n = FAILURES.fetch_add(1, Relaxed) + 1;
            if n.is_power_of_two() {
                tracing::warn!(
                    bytes = len,
                    error,
                    times = n,
                    "no room left below 2^37 for memory shaders read by address; none looked for again for a second"
                );
            }
            None
        }
    }
}

impl Upload {
    fn from_heap(len: usize) -> Option<Self> {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(len).ok()?;
        bytes.resize(len, 0);
        Some(Upload(Kind::Heap(bytes)))
    }

    pub(crate) fn new(len: usize) -> Option<Upload> {
        if !low() {
            return Self::from_heap(len);
        }
        let n = len.max(1).div_ceil(BLOCK);
        let mut arenas = ARENAS.lock().unwrap_or_else(|e| e.into_inner());
        let found = arenas.iter().enumerate().find_map(|(a, arena)| Some((a, gap(&arena.ends, n)?)));
        let (a, first) = match found {
            Some(place) => place,
            None => {
                let blocks = n.max(ARENA / BLOCK);
                let bytes = blocks.checked_mul(BLOCK)?;
                let Some(at) = reserve(bytes) else {
                    // Better than null, which the title throws on, ending
                    // the game: its shaders may read zeros from this.
                    static TOLD: std::sync::Once = std::sync::Once::new();
                    TOLD.call_once(|| tracing::warn!(bytes = len, "an upload from the process heap instead: ray tracing may miss some of it"));
                    return Self::from_heap(len);
                };
                crate::shadow::register(at, at as u64, bytes as u64);
                tracing::info!(at = format_args!("{at:#x}"), bytes, "memory for lists' uploads, where shaders reach it by address");
                arenas.push(Arena { at, ends: vec![0; blocks] });
                (arenas.len() - 1, 0)
            }
        };
        let arena = &mut arenas[a];
        arena.take(first, n, len);
        let at = arena.at + first * BLOCK;
        // SAFETY: the blocks are the arena's and nobody else's now.
        unsafe { std::ptr::write_bytes(at as *mut u8, 0, len) };
        Some(Upload(Kind::Low(at, len)))
    }

    pub(crate) fn start(&mut self) -> *mut u8 {
        match &mut self.0 {
            Kind::Low(at, _) => *at as *mut u8,
            Kind::Heap(block) => block.as_mut_ptr(),
        }
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        let Kind::Low(at, len) = self.0 else { return };
        let mut arenas = ARENAS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(arena) = arenas.iter_mut().find(|a| (a.at..a.at + a.ends.len() * BLOCK).contains(&at)) {
            let first = (at - arena.at) / BLOCK;
            arena.ends[first..first + len.max(1).div_ceil(BLOCK)].fill(0);
        }
    }
}

/// Initialized bytes from the block containing `address` to the end of
/// that upload.
pub(crate) fn block(address: usize) -> Option<(usize, usize)> {
    let arenas = ARENAS.lock().unwrap_or_else(|e| e.into_inner());
    arenas.iter().find_map(|arena| arena.block_at(address))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_commit_can_retry_the_same_reservation() {
        let pieces = Mutex::new(vec![0x10000, 0x20000]);
        assert_eq!(
            commit_early_piece(&pieces, |at| {
                assert_eq!(at, 0x20000);
                false
            }),
            None,
        );
        assert_eq!(commit_early_piece(&pieces, |at| at == 0x20000), Some(0x20000));
        assert_eq!(commit_early_piece(&pieces, |_| true), Some(0x10000));
    }

    #[test]
    fn an_unrepresentable_upload_returns_none() {
        assert!(Upload::new(usize::MAX).is_none());
    }

    #[test]
    fn an_unrepresentable_pool_size_returns_none() {
        assert!(from_pool(usize::MAX).is_none());
    }

    #[test]
    fn the_heap_fallback_allocates_zeroed_bytes_and_rejects_unrepresentable_capacity() {
        let upload = Upload::from_heap(100).unwrap();
        let Kind::Heap(bytes) = &upload.0 else {
            panic!("expected a heap upload");
        };
        assert_eq!(bytes.as_slice(), [0; 100]);
        assert!(Upload::from_heap(usize::MAX).is_none());
    }

    #[test]
    fn an_upload_scan_stops_before_the_next_allocation() {
        let at = 1 << 32;
        let mut arena = Arena { at, ends: vec![0; 4] };
        arena.take(0, 2, 2 * BLOCK);
        arena.take(2, 1, BLOCK);

        assert_eq!(arena.block_at(at + BLOCK + 8), Some((at + BLOCK, BLOCK)));
    }

    #[test]
    fn an_upload_scan_excludes_uninitialized_block_padding() {
        let at = 1 << 32;
        let mut arena = Arena { at, ends: vec![0; 2] };
        arena.take(0, 1, 24);

        assert_eq!(arena.block_at(at + 8), Some((at, 24)));
        assert_eq!(arena.block_at(at + 24), None);
    }

    #[test]
    fn blocks_lie_below_40_bits_and_are_used_again_zeroed() {
        let mut a = Upload::new(100).unwrap();
        let mut b = Upload::new(BLOCK + 1).unwrap();
        let (pa, pb) = (a.start(), b.start());
        assert!((pa as usize) >= 1 << 32 && (pb as usize) + 2 * BLOCK <= 1 << 40);
        assert!((pa as usize).is_multiple_of(BLOCK) && pa != pb);
        // SAFETY: 100 bytes were handed out.
        unsafe { pa.write_bytes(0xab, 100) };
        drop(a);
        // The lowest free place (another test may have taken it meanwhile;
        // zeroed either way).
        let mut c = Upload::new(100).unwrap();
        // SAFETY: as above.
        assert!(unsafe { std::slice::from_raw_parts(c.start(), 100) }.iter().all(|&x| x == 0));
        // More than an arena.
        let mut d = Upload::new(ARENA + 1).unwrap();
        // SAFETY: the bytes were handed out.
        unsafe { d.start().add(ARENA).write(1) };
        drop(b);
    }

    #[test]
    fn the_pool_is_reserved_in_the_window_and_handed_out_once() {
        reserve_space();
        assert!(!POOL.lock().unwrap().is_empty(), "the pool is reserved");
        let a = reserve(100).unwrap();
        let b = reserve(BLOCK).unwrap();
        // Larger than a piece: from the window.
        let c = reserve(EARLY_PIECE + BLOCK).unwrap();
        for at in [a, b, c] {
            assert!(at >= 1 << 32 && at + BLOCK <= 1 << 37 && at.is_multiple_of(BLOCK));
        }
        assert!(a != b);
        // SAFETY: committed, at least a block each.
        unsafe {
            assert_eq!(*(a as *const u8), 0);
            (b as *mut u8).add(BLOCK - 1).write(7);
        }
    }

    #[test]
    fn a_gap_is_blocks_side_by_side() {
        assert_eq!(gap(&[1, 0, 1, 0, 0, 1], 1), Some(1));
        assert_eq!(gap(&[1, 0, 1, 0, 0, 1], 2), Some(3));
        assert_eq!(gap(&[1, 0, 1, 0, 0, 1], 3), None);
    }
}
