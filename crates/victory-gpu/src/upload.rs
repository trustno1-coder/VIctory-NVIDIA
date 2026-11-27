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
    /// Per block whether it is handed out.
    taken: Vec<bool>,
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
fn gap(taken: &[bool], n: usize) -> Option<usize> {
    let mut free = 0;
    for (i, &t) in taken.iter().enumerate() {
        free = if t { 0 } else { free + 1 };
        if free == n {
            return Some(i + 1 - n);
        }
    }
    None
}

/// Fresh zeroed memory in [4 GiB, 2^37) (where the directory of memory by
/// address reaches, `MEMORY_PAGES`), write-watched as the title's own
/// pools are (copies then take only written pages).
pub(crate) fn reserve(len: usize) -> Option<usize> {
    reserve_between(len, 1 << 32, 1 << 37)
}

/// As [`reserve`], for memory of ours that no shader finds by its address
/// (the arenas of memory by address and its directory: a shader is given a
/// copy's number and an offset, never where our side of the copy lies):
/// where shaders reach if there is room, else anywhere below 2^40, which is
/// as far as the write watch keeps its bits.
///
/// The title's own memory fills [4 GiB, 2^37): with the world streamed in
/// there was no hole of 64 MiB left in it in two runs of three (rt-2 and
/// rt-4), the twentieth arena could not be had, and every builder dispatch
/// asked for it again.
///
/// `VICTORY_DISPLAY_WIDE` (read once, see [`wide`]) tries above 2^37 first: at
/// 5120x1440 the title's screen-sized memory leaves about 150 MiB of room in
/// the reachable window, and the arenas of memory by address (64 MiB each,
/// which no shader finds by address) took 80 MiB of it, so the upload arenas
/// ran out at the nineteenth and `allocate_upload` answered null to the
/// structure builder, which then ended the run. Unset, the order is as it was.
pub(crate) fn reserve_out_of_reach(len: usize) -> Option<usize> {
    let [first, second] = out_of_reach_order(wide());
    reserve_between(len, first.0, first.1).or_else(|| reserve_between(len, second.0, second.1))
}

/// The two windows [`reserve_out_of_reach`] tries, in order.
fn out_of_reach_order(wide: bool) -> [(usize, usize); 2] {
    let (near, far) = ((1 << 32, 1 << 37), (1 << 37, 1 << 40));
    if wide { [far, near] } else { [near, far] }
}

/// Whether `VICTORY_DISPLAY_WIDE` is set (the launcher keeps it in the
/// title's process): a wide output, whose bigger buffers leave the reachable
/// window short of room. Read once.
pub(crate) fn wide() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DISPLAY_WIDE").is_some())
}

/// The last error of this thread, for a log line after a failed call.
pub(crate) fn last_error() -> u32 {
    // SAFETY: a plain query of the calling thread's state.
    unsafe { windows_sys::Win32::Foundation::GetLastError() }
}

/// Gives memory of [`reserve`]'s back (all of it, from its start).
pub(crate) fn release(at: usize) {
    use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
    if at != 0 {
        // SAFETY: a reservation of ours that nothing uses any more.
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
    }
}

/// Fresh zeroed write-watched memory of `len` bytes in `[lowest, end)`.
fn reserve_between(len: usize, lowest: usize, end: usize) -> Option<usize> {
    use windows_sys::Win32::System::Memory::{
        MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_EXTENDED_PARAMETER, MEM_RESERVE, MemExtendedParameterAddressRequirements,
        PAGE_READWRITE, VirtualAlloc2,
    };
    const MEM_WRITE_WATCH: u32 = 0x20_0000;
    let mut requirements = MEM_ADDRESS_REQUIREMENTS {
        LowestStartingAddress: lowest as *mut _,
        HighestEndingAddress: (end - 1) as *mut _,
        Alignment: 0,
    };
    let mut parameter = MEM_EXTENDED_PARAMETER::default();
    parameter.Anonymous1._bitfield = MemExtendedParameterAddressRequirements as u64;
    parameter.Anonymous2.Pointer = (&raw mut requirements).cast();
    // SAFETY: a fresh allocation; the parameter points at `requirements`,
    // which outlives the call.
    let at = unsafe {
        VirtualAlloc2(
            std::ptr::null_mut(),
            std::ptr::null(),
            len,
            MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
            PAGE_READWRITE,
            &mut parameter,
            1,
        )
    } as usize;
    (at != 0).then_some(at)
}

impl Upload {
    pub(crate) fn new(len: usize) -> Option<Upload> {
        if !low() {
            return Some(Upload(Kind::Heap(vec![0; len])));
        }
        let n = len.max(1).div_ceil(BLOCK);
        let mut arenas = ARENAS.lock().unwrap_or_else(|e| e.into_inner());
        let found = arenas.iter().enumerate().find_map(|(a, arena)| Some((a, gap(&arena.taken, n)?)));
        let (a, first) = match found {
            Some(place) => place,
            None => {
                let blocks = n.max(ARENA / BLOCK);
                let Some(at) = reserve(blocks * BLOCK) else {
                    // the caller answers null and the title's structure builder ends the run on it
                    // ("Failed to allocate upload buffer"): what Windows said goes in the log first
                    // (1455: the PC's memory and page file are used up; 8: no free range left)
                    let error = last_error();
                    tracing::error!(
                        bytes = blocks * BLOCK,
                        error,
                        arenas = arenas.len(),
                        "no memory for lists' uploads where shaders reach it by address (between 4 GiB and 128 GiB)"
                    );
                    return None;
                };
                crate::shadow::register(at, at as u64, (blocks * BLOCK) as u64);
                tracing::info!(at = format_args!("{at:#x}"), bytes = blocks * BLOCK, "memory for lists' uploads, where shaders reach it by address");
                arenas.push(Arena { at, taken: vec![false; blocks] });
                (arenas.len() - 1, 0)
            }
        };
        let arena = &mut arenas[a];
        arena.taken[first..first + n].fill(true);
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
        if let Some(arena) = arenas.iter_mut().find(|a| (a.at..a.at + a.taken.len() * BLOCK).contains(&at)) {
            let first = (at - arena.at) / BLOCK;
            arena.taken[first..first + len.max(1).div_ceil(BLOCK)].fill(false);
        }
    }
}

/// The block handed out that holds `address`: its start and bytes (whole
/// [`BLOCK`]s).
pub(crate) fn block(address: usize) -> Option<(usize, usize)> {
    let arenas = ARENAS.lock().unwrap_or_else(|e| e.into_inner());
    let arena = arenas.iter().find(|a| (a.at..a.at + a.taken.len() * BLOCK).contains(&address))?;
    let first = (address - arena.at) / BLOCK;
    let n = arena.taken[first..].iter().take_while(|t| **t).count();
    (n > 0).then_some((arena.at + first * BLOCK, n * BLOCK))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn wide_tries_above_the_reach_first_and_16_9_is_unchanged() {
        assert_eq!(out_of_reach_order(false), [(1 << 32, 1 << 37), (1 << 37, 1 << 40)]);
        assert_eq!(out_of_reach_order(true), [(1 << 37, 1 << 40), (1 << 32, 1 << 37)]);
    }

    #[test]
    fn a_gap_is_blocks_side_by_side() {
        assert_eq!(gap(&[true, false, true, false, false, true], 1), Some(1));
        assert_eq!(gap(&[true, false, true, false, false, true], 2), Some(3));
        assert_eq!(gap(&[true, false, true, false, false, true], 3), None);
    }
}
