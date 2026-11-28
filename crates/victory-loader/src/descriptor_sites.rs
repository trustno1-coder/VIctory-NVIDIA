//! The six descriptor record stores of the title's table fillers, as the heap
//! hooks need them: their places in the image, so that
//! `victory_loader::heap_sites::sites` can hook them like the others.
//!
//! The donor kept these in the file of renderer port #2
//! (`VICTORY_PORT_DESCRIPTORS=1`), which patches the same stores to mark the
//! slots they write. That port is not here, and neither is its patcher: with
//! the heap hooks (`VICTORY_HEAP_HOOKS=1`) the same stores are hooked to
//! write through our view and note their slots instead, so only the table of
//! places comes over. What is said below of the stores is what `heap_sites`
//! relies on.
//!
//! What the title does (game_scarlett_master.exe): each draw's and each
//! dispatch's descriptor tables are allocated from its frame allocator
//! (image +0x4d9f400; the allocator's memory is the placed shader-visible
//! view heap) and filled by two functions, +0x4d9efd0 (graphics) and
//! +0x4eec8e0 (compute), three loops each, one record at a time: `and r,
//! 7FE0h` (the slot's byte offset in the table), `vmovups ymm0, [record]`,
//! `vmovups [table + r], ymm0`. Found with `dumpbin /disasm` from the write
//! faults of run faults-6 (book records "the instruction at a made the
//! fault"); a search of all of `.text` for the same instruction shape finds
//! these six stores and no other. The flags are clobbered: at every site the
//! next instructions are a `movzx` and a `cmp`, which sets them before any
//! use (which is why a window runs to the `cmp` and not further).
//!
//! A row is a place, a length and a hash, not the title's code: the window's
//! BLAKE3-64 (the low eight bytes of its BLAKE3-256 digest, little-endian).
//! `heap_sites::Site::window` reads the bytes from the mapped image and takes
//! the site only when they hash to what is recorded here, so a site of another
//! build of the title is left alone and said so.

use crate::heap_sites::Tls;

/// A store of the title's to hook: the window's offset in the image, its length
/// and its BLAKE3-64, the store's offset in the window and its length, and the
/// TLS-array loads in the window (`crate::tls` may have made them calls of its
/// thunk; these six windows have none).
pub struct Site {
    pub window: usize,
    pub store: usize,
    pub store_len: u8,
    pub len: u16,
    pub digest: u64,
    pub tls: &'static [Tls],
}

/// The six record stores of the table fillers (game_scarlett_master.exe
/// 1.0.2.1, image offsets).
#[rustfmt::skip]
pub const SITES: [Site; 6] = [
    // +0x4d9efd0, loop 1: [rcx+rsi]
    Site { window: 0x4d9f04f, store: 10, store_len: 5, len: 21, digest: 0x24aaddbb91f5d641, tls: &[] },
    // loop 2: [rcx+rsi]
    Site { window: 0x4d9f10d, store: 10, store_len: 5, len: 22, digest: 0x01c469e424969744, tls: &[] },
    // loop 3: [r9+rsi]
    Site { window: 0x4d9f19c, store: 12, store_len: 6, len: 26, digest: 0x5d1242630e29f130, tls: &[] },
    // +0x4eec8e0, loop 1: [r9+r8]
    Site { window: 0x4eec95d, store: 11, store_len: 6, len: 23, digest: 0x4873c890f037004c, tls: &[] },
    // loop 2: [rdi+r8]
    Site { window: 0x4eeca08, store: 10, store_len: 6, len: 22, digest: 0xdafe62094030da4e, tls: &[] },
    // loop 3: [r10+r8]
    Site { window: 0x4eeca82, store: 11, store_len: 6, len: 24, digest: 0xccfe4b9b6cb8f5b1, tls: &[] },
];
