//! Descriptor heap hooks (`VICTORY_HEAP_HOOKS=1`; off unless asked for): the
//! title's view heap is not looked at page by page for what changed; what
//! writes it says which slots it wrote.
//!
//! What the layer does without them (`crate::descriptor::sync_heaps`): the
//! heap's memory is guarded, a write of the title's opens a 64 KiB block, and
//! each round of the heaps' records reads every block opened since, whole,
//! and compares its 2,048 slots with what it saw last (some 127 blocks a
//! frame at 1440p, 27 ms a frame of heap sync over all threads, lane book
//! lb4).
//!
//! What is done here: every store of the title's into the heap is hooked at
//! load (`victory_loader::heap_sites`), and so are the title's `memcpy`,
//! `memmove` and `memset` ([`import`]) and our own writes of records
//! ([`write`], [`copy`]). A hooked write goes through our own view of the
//! heap's pages (`crate::inplace::beside_page`) and notes its 32-byte slots
//! in a journal: a record written, bytes that are no record (the frame
//! allocator's own lists, which live in the same memory), or a slot given
//! back to the title's pool of persistent slots (its first word is then the
//! pool's link). A round of the sync takes the journal ([`take`]) and reads,
//! compares and makes the slots written, no others; it never reads the
//! others.
//!
//! The title's addresses of the heap stay read-only, as a guard: a write that
//! faults there is one no hook took ([`guard_fault`]). It is taken: its
//! instruction, caller, stack and blocks are kept for `heap-writers.tsv`, its
//! block is opened and noted written, and the next sync guards it again, so
//! such writers can be hooked from their trace later. With
//! `VICTORY_HEAP_HOOKS_STRICT=1` the process stops at the first one instead.
//!
//! What else changes the heap's bytes, and how that is taken:
//! - the runtime mapping or unmapping its pages, the kernel or DirectStorage
//!   writing them, our copy-back of what shaders wrote
//!   (`crate::descriptor::host::remapped`, `page_written`): every slot of the
//!   range is noted written ([`changed`]); where what the addresses show
//!   changed, the blocks' ways to our view are forgotten ([`views_gone`]);
//! - a block found not guarded at a round (a change of mapping opened it, a
//!   view of its pages was mapped elsewhere, our own writes at the title's
//!   address): guarded again, and what the watch says was written there is
//!   read ([`reguard`]).
//!
//! The heap shares memory with a buffer of the title's whose host copy the
//! write watch keeps (the frame allocator's memory, 174 MB, of which the
//! heap's last 27 MB are a part). A write through our view does not fault, so
//! each block written is told to the watch before it answers anything about
//! title memory ([`tell_watch`]), as `crate::watch::put_beside` tells it.
//!
//! Ordering: the journal is taken by a round of the heaps' records that
//! begins after the title's submission (`crate::descriptor::begin_sync_heaps`,
//! as before), so every write before the title submitted is in it; the
//! submission goes to the queue only after that round. The host slots are
//! made as before, by the same code: what is new is only which slots are
//! looked at. A slot given back to the pool, or overwritten by the frame
//! allocator's lists, is left showing what it showed: a list still in flight
//! that reads it reads the view it was recorded with, never a link.

use std::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU8, AtomicU64, AtomicUsize,
    Ordering::{AcqRel, Acquire, Relaxed, Release},
};

/// Whether the hooks are asked for.
pub fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // Read once, here: the switch of the heap hooks.
    *ON.get_or_init(|| std::env::var("VICTORY_HEAP_HOOKS").is_ok_and(|v| v.trim() == "1"))
}

/// What the journal says of a slot: a record written (or bytes that may be
/// one), bytes that are no record, a slot given back to the title's pool.
pub const RECORD: u8 = 1;
pub const OTHER: u8 = 2;
pub const FREED: u8 = 3;

/// The watch's unit, and the stubs'.
const BLOCK: usize = 64 << 10;
/// Bytes of a view record.
const SLOT: usize = 32;
/// Bytes between two blocks' marks (`victory_loader::heap_sites::LINE`): a
/// cache line each, so that threads writing different blocks do not pass one
/// line around.
const LINE: usize = 64;
const SLOTS_A_BLOCK: usize = BLOCK / SLOT;

/// The cells the stubs read (`victory_loader::heap_sites`, `CELLS_LEN`
/// bytes): the heap's first byte, its length (0: nothing is hooked), what to
/// add to a title address for ours per block (0: not known, 1: none), the
/// note per slot, a mark per block for the sync and one for the watch ([`LINE`]
/// bytes apart), and whether any block's mark for the watch was set since it
/// was last told.
#[repr(C)]
pub struct Cells {
    base: AtomicU64,
    len: AtomicU64,
    deltas: AtomicPtr<AtomicU64>,
    slots: AtomicPtr<AtomicU8>,
    blocks: AtomicPtr<AtomicU8>,
    told: AtomicPtr<AtomicU8>,
    any: AtomicU8,
    _pad: [u8; 7],
}

const _: () = assert!(size_of::<Cells>() == 56);

static CELLS: Cells = Cells {
    base: AtomicU64::new(0),
    len: AtomicU64::new(0),
    deltas: AtomicPtr::new(std::ptr::null_mut()),
    slots: AtomicPtr::new(std::ptr::null_mut()),
    blocks: AtomicPtr::new(std::ptr::null_mut()),
    told: AtomicPtr::new(std::ptr::null_mut()),
    any: AtomicU8::new(0),
    _pad: [0; 7],
};

/// Where the stubs' cells are, for `heap_sites::apply`.
pub fn cells_address() -> usize {
    std::ptr::from_ref(&CELLS) as usize
}

/// What the stubs call for a block's way to our view, for `heap_sites::apply`.
pub fn resolve_address() -> usize {
    resolve as *const () as usize
}

/// The title's image and the hooked stores' stubs: (span in the image, stub,
/// its bytes).
struct Image {
    base: usize,
    size: usize,
    stubs: Vec<(usize, usize, usize)>,
}

static IMAGE: std::sync::RwLock<Option<Image>> = std::sync::RwLock::new(None);
static HOOKED: AtomicBool = AtomicBool::new(false);

/// The heap's first byte and length as the stubs compare them before they
/// save anything (16 bytes beside the stubs, given by the launcher): kept with
/// the cells' own.
static MIRROR: AtomicPtr<AtomicU64> = AtomicPtr::new(std::ptr::null_mut());

/// Sets the mirror (when the cells' base and length are set: the length last;
/// when the heap goes: the length first).
fn mirror(base: u64, len: u64) {
    let at = MIRROR.load(Acquire);
    if at.is_null() {
        return;
    }
    // SAFETY: 16 bytes of the stubs' memory, which stays for the run.
    unsafe {
        if len == 0 {
            (*at.add(1)).store(0, Release);
        }
        (*at).store(base, Release);
        (*at.add(1)).store(len, Release);
    }
}

/// Told by the launcher: every site of `victory_loader::heap_sites` in the
/// image at `base` (`size` bytes) is hooked, with these stubs, and the mirror
/// of the heap's place they compare is at `mirror_at`.
pub fn hooked(base: usize, size: usize, stubs: Vec<(usize, usize, usize)>, mirror_at: usize) {
    MIRROR.store(mirror_at as *mut AtomicU64, Release);
    let n = stubs.len();
    stubs_made(base, size, stubs);
    HOOKED.store(true, Release);
    tracing::info!(
        stores = n,
        "heap hooks: the title's stores into its view heap are hooked; the heap's memory is guarded, a write no hook takes is traced (VICTORY_HEAP_HOOKS; VICTORY_HEAP_HOOKS_STRICT=1 stops the process there)"
    );
}

/// The image at `base` (`size` bytes) and stubs of hooked stores in it (span, stub, its
/// bytes), for the names of places ([`place`]): the heap hooks', `crate::pool_hooks`'.
pub(crate) fn stubs_made(base: usize, size: usize, stubs: Vec<(usize, usize, usize)>) {
    let mut image = IMAGE.write().unwrap_or_else(|e| e.into_inner());
    match image.as_mut() {
        Some(image) => image.stubs.extend(stubs),
        None => *image = Some(Image { base, size, stubs }),
    }
}

/// Told by the launcher: the title's stores were not hooked, and why (a site
/// whose bytes differ: another build of the title). The heaps are synced by
/// their pages, as without the switch.
pub fn refused(why: &str) {
    tracing::warn!(
        why,
        "heap hooks: off for this executable, the view heap is synced by its pages (VICTORY_HEAP_HOOKS=1)"
    );
}

/// Whether a heap may be adopted: asked for, and the title's stores hooked.
fn working() -> bool {
    on() && HOOKED.load(Acquire)
}

/// The adopted heap: its first byte, slots and blocks, and the journal's
/// tables the sync reads (the cells point at these and the others).
struct Tables {
    start: usize,
    count: usize,
    blocks: usize,
    slots: &'static [AtomicU8],
    marks: &'static [AtomicU8],
}

static ADOPTED: std::sync::RwLock<Option<Tables>> = std::sync::RwLock::new(None);

/// A placed shader-visible view heap of `count` records at `start`: the first
/// such is hooked, if the hooks are on. Whether it is.
pub(crate) fn adopt(start: u64, count: u32) -> bool {
    working() && adopt_now(start as usize, count as usize)
}

fn adopt_now(start: usize, count: usize) -> bool {
    let mut adopted = ADOPTED.write().unwrap_or_else(|e| e.into_inner());
    if adopted.is_some() {
        tracing::info!(
            start = format_args!("{start:#x}"),
            count,
            "heap hooks: another placed view heap, synced by its pages as before"
        );
        return false;
    }
    if !start.is_multiple_of(BLOCK) || count == 0 {
        tracing::warn!(
            start = format_args!("{start:#x}"),
            count,
            "heap hooks: a view heap that does not begin on 64 KiB, synced by its pages as before"
        );
        return false;
    }
    let len = count * SLOT;
    let blocks = len.div_ceil(BLOCK);
    // (Never freed: a stub may be noting while the heap goes. One more of
    // each: a store's last byte may lie past the end.)
    let bytes =
        |n: usize| -> &'static [AtomicU8] { Box::leak((0..n).map(|_| AtomicU8::new(0)).collect()) };
    let deltas: &'static [AtomicU64] = Box::leak((0..=blocks).map(|_| AtomicU64::new(0)).collect());
    let (slots, marks, told) = (
        bytes(count + 1),
        bytes((blocks + 1) * LINE),
        bytes((blocks + 1) * LINE),
    );
    // Everything written before: the first round reads the heap whole.
    slots[..count].iter().for_each(|s| s.store(RECORD, Relaxed));
    (0..blocks).for_each(|block| marks[block * LINE].store(1, Relaxed));
    CELLS.deltas.store(deltas.as_ptr().cast_mut(), Release);
    CELLS.slots.store(slots.as_ptr().cast_mut(), Release);
    CELLS.blocks.store(marks.as_ptr().cast_mut(), Release);
    CELLS.told.store(told.as_ptr().cast_mut(), Release);
    CELLS.base.store(start as u64, Release);
    // Last: a stub that sees a length sees the rest (the mirror's, which it
    // compares first, after the cells').
    CELLS.len.store(len as u64, Release);
    mirror(start as u64, len as u64);
    *adopted = Some(Tables {
        start,
        count,
        blocks,
        slots,
        marks,
    });
    tracing::info!(
        start = format_args!("{start:#x}"),
        count,
        blocks,
        "heap hooks: the title's view heap is synced by what its hooked writers note, not by its pages; its memory stays guarded (VICTORY_HEAP_HOOKS)"
    );
    true
}

/// The heap at `start` goes: nothing more is hooked for it.
pub(crate) fn let_go(start: u64) {
    let mut adopted = ADOPTED.write().unwrap_or_else(|e| e.into_inner());
    if adopted.as_ref().is_some_and(|t| t.start == start as usize) {
        mirror(start, 0);
        CELLS.len.store(0, Release);
        *adopted = None;
        tracing::info!(
            start = format_args!("{start:#x}"),
            "heap hooks: the hooked view heap went"
        );
    }
}

/// Whether the heap at `start` is the hooked one.
pub(crate) fn adopted(start: u64) -> bool {
    ADOPTED
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|t| t.start == start as usize)
}

/// The hooked heap's first byte and length (0: none).
#[inline]
fn heap() -> (usize, usize) {
    let len = CELLS.len.load(Acquire) as usize;
    (CELLS.base.load(Relaxed) as usize, len)
}

/// The hooked heap's first byte and length (0: none), for `crate::pool_hooks`.
/// (Kept as the donor has it. The pool hooks, its only caller, are not ported:
/// they note what they wrote in a journal this fork does not have.)
#[allow(dead_code)]
pub(crate) fn hooked_heap() -> (usize, usize) {
    heap()
}

/// Whether `[at, at + n)` touches the hooked heap.
#[inline]
fn touches(at: usize, n: usize) -> bool {
    let (base, len) = heap();
    len != 0 && n != 0 && at < base + len && base < at.saturating_add(n)
}

/// The stubs' call: what to add to a title address of the heap's block
/// `block` for ours, noted in its entry; 1 if there is no view of ours (the
/// store goes to the title's address).
extern "system" fn resolve(block: u64) -> u64 {
    std::panic::catch_unwind(|| way(block as usize)).unwrap_or(1)
}

fn way(block: usize) -> u64 {
    let (base, len) = heap();
    if len == 0 || block >= len.div_ceil(BLOCK) {
        return 1;
    }
    let page = base + block * BLOCK;
    // No change of mapping meanwhile (the runtime's own change, if one is
    // under way, ends first).
    let _reading = crate::watch::title_memory_reading();
    let Some((ours, _)) = crate::inplace::beside_page(page) else {
        // (Not noted: the page may be shown in a moment.)
        COUNTS[NO_WAY].fetch_add(1, Relaxed);
        return 1;
    };
    let delta = ours.wrapping_sub(page) as u64;
    // SAFETY: the tables are leaked, as long as the length says (`block` is inside).
    unsafe { (*CELLS.deltas.load(Acquire).add(block)).store(delta, Release) };
    COUNTS[WAYS].fetch_add(1, Relaxed);
    delta
}

/// The address to write the heap's byte at `at` at: ours, or the title's if
/// there is no view of ours. `at` lies in the heap.
fn ours(at: usize) -> usize {
    let (base, _) = heap();
    let block = (at - base) / BLOCK;
    // SAFETY: as in `way`.
    let known = unsafe { (*CELLS.deltas.load(Acquire).add(block)).load(Acquire) };
    match if known == 0 { way(block) } else { known } {
        1 => at,
        delta => at.wrapping_add(delta as usize),
    }
}

/// `put(address, offset, len)` for the pieces of `[at, at + n)`: in the heap
/// a block at a time, at our address; outside it at the title's.
fn pieces(at: usize, n: usize, mut put: impl FnMut(usize, usize, usize)) {
    let (base, len) = heap();
    let mut offset = 0;
    while offset < n {
        let here = at + offset;
        let piece = if len != 0 && here.wrapping_sub(base) < len {
            let piece = (BLOCK - (here - base) % BLOCK)
                .min(n - offset)
                .min(base + len - here);
            put(ours(here), offset, piece);
            piece
        } else {
            let piece = if len != 0 && here < base {
                (base - here).min(n - offset)
            } else {
                n - offset
            };
            put(here, offset, piece);
            piece
        };
        offset += piece;
    }
}

/// Notes the slots of `[at, at + n)` that lie in the heap as `note`, after
/// their bytes are there: their blocks for the sync and for the watch too.
fn note(at: usize, n: usize, note: u8) {
    let (base, len) = heap();
    let (from, to) = (at.max(base), at.saturating_add(n).min(base + len));
    if len == 0 || from >= to {
        return;
    }
    let (slots, marks, told) = (
        CELLS.slots.load(Acquire),
        CELLS.blocks.load(Acquire),
        CELLS.told.load(Acquire),
    );
    // SAFETY: the tables are leaked, as long as the length says; every index
    // is of a byte inside the heap.
    unsafe {
        for slot in (from - base) / SLOT..=(to - 1 - base) / SLOT {
            (*slots.add(slot)).store(note, Release);
        }
        for block in (from - base) / BLOCK..=(to - 1 - base) / BLOCK {
            (*marks.add(block * LINE)).store(1, Release);
            if (*told.add(block * LINE)).swap(1, AcqRel) == 0 {
                CELLS.any.store(1, Release);
            }
        }
    }
}

/// Our own record (`crate::descriptor` view creation) of `bytes` at `at`:
/// written, and noted, if it is in the hooked heap. Whether it was (if not,
/// the caller writes it as before).
pub(crate) fn write(at: u64, bytes: &[u8]) -> bool {
    let at = at as usize;
    if !touches(at, bytes.len()) {
        return false;
    }
    pieces(at, bytes.len(), |to, offset, n| {
        // SAFETY: a record's bytes at our view of the heap (or the title's
        // address outside it), which is the title's memory to write.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr().add(offset), to as *mut u8, n) };
    });
    note(at, bytes.len(), RECORD);
    COUNTS[OURS].fetch_add(1, Relaxed);
    true
}

/// Our own copy of descriptors (`crate::descriptor::copy_simple`) of `n`
/// bytes from `from` to `to`: made, and noted, if it lands in the hooked
/// heap; what is there already is not written (`VICTORY_SAME_RECORDS=0`:
/// always). Whether it was (if not, the caller copies as before).
///
/// # Safety
/// Both ranges are the title's descriptor memory.
pub(crate) unsafe fn copy(to: u64, from: u64, n: usize) -> bool {
    let (to, from) = (to as usize, from as usize);
    if !touches(to, n) {
        return false;
    }
    // SAFETY: by the caller; reading the title's addresses of the heap is allowed.
    let same = unsafe {
        std::slice::from_raw_parts(to as *const u8, n)
            == std::slice::from_raw_parts(from as *const u8, n)
    };
    if crate::toggled!("VICTORY_SAME_RECORDS") && same {
        return true;
    }
    // (Overlapping ranges: from a copy of the source, which our view would not see change.)
    let held: Vec<u8>;
    let source = if from < to + n && to < from + n {
        // SAFETY: by the caller.
        held = unsafe { std::slice::from_raw_parts(from as *const u8, n) }.to_vec();
        held.as_ptr()
    } else {
        from as *const u8
    };
    pieces(to, n, |at, offset, len| {
        // SAFETY: by the caller; `at` is ours or the title's address of the piece.
        unsafe { std::ptr::copy_nonoverlapping(source.add(offset), at as *mut u8, len) };
    });
    note(to, n, RECORD);
    COUNTS[OURS].fetch_add(1, Relaxed);
    true
}

/// What the title's `memcpy`, `memmove` and `memset` were bound to before.
static REAL: [AtomicUsize; 3] = [const { AtomicUsize::new(0) }; 3];

type CopyFn = unsafe extern "system-unwind" fn(*mut u8, *const u8, usize) -> *mut u8;
type SetFn = unsafe extern "system-unwind" fn(*mut u8, i32, usize) -> *mut u8;

/// What the title's import of `name` (bound to `address`) is bound to: with
/// the hooks asked for, its `memcpy`, `memmove` and `memset` write the heap
/// through our view and note what they wrote; everything else they leave to
/// `address`.
pub fn import(name: &str, address: usize) -> usize {
    let (which, ours) = match name {
        "memcpy" => (0, copy_import as *const () as usize),
        "memmove" => (1, move_import as *const () as usize),
        "memset" => (2, set_import as *const () as usize),
        _ => return address,
    };
    if !on() {
        return address;
    }
    REAL[which].store(address, Relaxed);
    ours
}

unsafe extern "system-unwind" fn copy_import(to: *mut u8, from: *const u8, n: usize) -> *mut u8 {
    // SAFETY: the title's copy, by the function it was bound to.
    unsafe { copied(to, from, n, REAL[0].load(Relaxed)) }
}

unsafe extern "system-unwind" fn move_import(to: *mut u8, from: *const u8, n: usize) -> *mut u8 {
    // SAFETY: as above.
    unsafe { copied(to, from, n, REAL[1].load(Relaxed)) }
}

/// # Safety
/// `real` copies as `memmove` does; the rest are the title's.
#[inline]
unsafe fn copied(to: *mut u8, from: *const u8, n: usize, real: usize) -> *mut u8 {
    // SAFETY: the function the import was bound to, noted before the title ran.
    let real: CopyFn = unsafe { std::mem::transmute(real) };
    if !touches(to as usize, n) {
        // SAFETY: by the caller.
        return unsafe { real(to, from, n) };
    }
    let held: Vec<u8>;
    let source = if (from as usize) < to as usize + n && (to as usize) < from as usize + n {
        // SAFETY: by the caller.
        held = unsafe { std::slice::from_raw_parts(from, n) }.to_vec();
        held.as_ptr()
    } else {
        from
    };
    pieces(to as usize, n, |at, offset, len| {
        // SAFETY: by the caller; `at` is ours or the title's address of the piece.
        unsafe { real(at as *mut u8, source.add(offset), len) };
    });
    note(to as usize, n, RECORD);
    COUNTS[COPIES].fetch_add(1, Relaxed);
    to
}

unsafe extern "system-unwind" fn set_import(to: *mut u8, byte: i32, n: usize) -> *mut u8 {
    // SAFETY: as in `copied`.
    let real: SetFn = unsafe { std::mem::transmute(REAL[2].load(Relaxed)) };
    if !touches(to as usize, n) {
        // SAFETY: the title's, as it asked.
        return unsafe { real(to, byte, n) };
    }
    pieces(to as usize, n, |at, _, len| {
        // SAFETY: as in `copied`.
        unsafe { real(at as *mut u8, byte, len) };
    });
    note(to as usize, n, RECORD);
    COUNTS[COPIES].fetch_add(1, Relaxed);
    to
}

/// What the title has at `[at, at + len)` changed with no hooked write: the
/// runtime mapped or unmapped it, or the kernel, DirectStorage or our
/// copy-back wrote it. Every slot of the heap there is noted written. Called
/// with the runtime's locks held: takes no lock, calls nothing that does.
pub(crate) fn changed(at: usize, len: usize) {
    let (base, n) = heap();
    let (from, to) = (at.max(base), at.saturating_add(len).min(base + n));
    if n == 0 || from >= to {
        return;
    }
    note(from, to - from, RECORD);
    COUNTS[CHANGED].fetch_add(1, Relaxed);
}

/// What the title's addresses `[at, at + len)` show is no longer what they
/// showed (the runtime maps or unmaps them, `crate::inplace::views_gone`):
/// the heap's blocks there have their way to our view asked for again at
/// their next write. Takes no lock.
pub(crate) fn views_gone(at: usize, len: usize) {
    let (base, n) = heap();
    let (from, to) = (at.max(base), at.saturating_add(len).min(base + n));
    if n == 0 || from >= to {
        return;
    }
    let deltas = CELLS.deltas.load(Acquire);
    for block in (from - base) / BLOCK..=(to - 1 - base) / BLOCK {
        // SAFETY: the tables are leaked, as long as the length says.
        unsafe { (*deltas.add(block)).store(0, Release) };
    }
}

/// Tells the write watch of the heap's blocks written through our view since
/// it was last told: before any answer it gives about title memory
/// (`crate::watch`), so that a host copy of the buffer the heap shares
/// memory with takes them again.
#[inline]
pub(crate) fn tell_watch() {
    if !HOOKED.load(Relaxed) || CELLS.any.load(Relaxed) == 0 || CELLS.any.swap(0, AcqRel) == 0 {
        return;
    }
    let (base, len) = heap();
    let told = CELLS.told.load(Acquire);
    if len == 0 || told.is_null() {
        return;
    }
    for block in 0..len.div_ceil(BLOCK) {
        // SAFETY: the table is leaked, as long as the length says.
        let mark = unsafe { &*told.add(block * LINE) };
        if mark.load(Relaxed) != 0 && mark.swap(0, AcqRel) != 0 {
            crate::watch::written_beside(base + block * BLOCK);
            COUNTS[TOLD].fetch_add(1, Relaxed);
        }
    }
}

/// At a round of the sync of the heap at `start` (`take`: its watch's
/// changed pages of a range, which guards them again): its blocks that are
/// neither guarded nor known unmapped are guarded again, and what was written
/// there is noted. A block stays read-only at the title's addresses for as
/// long as the heap is hooked, or else this reads it.
pub(crate) fn reguard(start: u64, take: impl Fn(usize, usize) -> Vec<(usize, usize)>) {
    let (base, len) = heap();
    if len == 0 || base != start as usize {
        return;
    }
    let (first, last) = (base / BLOCK, (base + len).div_ceil(BLOCK));
    if (first..last).all(|block| crate::watch::guards_any(block * BLOCK, 1)) {
        return;
    }
    let open = crate::watch::unguarded(first, last);
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for block in open {
        match runs.last_mut() {
            Some((_, end)) if *end == block => *end += 1,
            _ => runs.push((block, block + 1)),
        }
    }
    for (from, to) in runs {
        COUNTS[REGUARDED].fetch_add((to - from) as u64, Relaxed);
        for (at, end) in take(from * BLOCK, to * BLOCK) {
            note(at, end - at, RECORD);
        }
    }
}

/// The slots of the heap at `start` written with records since the last
/// call, as runs `(first, count)` within a block each, their notes cleared;
/// `None` if it is not the hooked heap. Slots noted as no record, or given
/// back, are cleared and left out: they are not read.
pub(crate) fn take(start: u64) -> Option<Vec<(u32, u32)>> {
    let adopted = ADOPTED.read().unwrap_or_else(|e| e.into_inner());
    let t = adopted.as_ref().filter(|t| t.start == start as usize)?;
    let mut runs: Vec<(u32, u32)> = Vec::new();
    let mut seen = [0u64; 4];
    let mut blocks = 0;
    for block in 0..t.blocks {
        let mark = &t.marks[block * LINE];
        // Cleared before its slots are read: a note made meanwhile marks it again.
        if mark.load(Relaxed) == 0 || mark.swap(0, AcqRel) == 0 {
            continue;
        }
        blocks += 1;
        let first = block * SLOTS_A_BLOCK;
        for (i, slot) in t.slots[first..(first + SLOTS_A_BLOCK).min(t.count)]
            .iter()
            .enumerate()
        {
            if slot.load(Relaxed) == 0 {
                continue;
            }
            let state = slot.swap(0, AcqRel);
            seen[usize::from(state.min(3))] += 1;
            if state != RECORD {
                continue;
            }
            let index = (first + i) as u32;
            match runs.last_mut() {
                Some((at, n))
                    if *at + *n == index && !(index as usize).is_multiple_of(SLOTS_A_BLOCK) =>
                {
                    *n += 1
                }
                _ => runs.push((index, 1)),
            }
        }
    }
    for (count, n) in [
        (RECORDS, seen[1]),
        (OTHERS, seen[2]),
        (FREES, seen[3]),
        (BLOCKS, blocks),
        (RUNS, runs.len() as u64),
        (TAKES, 1),
    ] {
        COUNTS[count].fetch_add(n, Relaxed);
    }
    Some(runs)
}

/// Whether a write into the hooked heap that no hook took stops the process
/// (`VICTORY_HEAP_HOOKS_STRICT=1`); else it is taken as before and its writer
/// is traced ([`dynamic`]), for a hook later.
fn strict() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_HEAP_HOOKS_STRICT").is_ok_and(|v| v.trim() == "1"))
}

/// A writer of hooked memory that no hook took, by what it wrote (0: the
/// hooked heap, 1: the pools of `crate::pool_hooks`), its place and its
/// caller's: the stack of its first faults, how often it faulted (in all and
/// since the last line), the blocks, the frames of its first and last fault,
/// an address it wrote, and where that lies (the pools': which pool).
struct Writer {
    stack: Vec<usize>,
    count: u64,
    recent: u64,
    blocks: std::collections::BTreeSet<usize>,
    first: u64,
    last: u64,
    at: usize,
    region: Option<String>,
}

/// [`Writer::region`]'s kinds: the heap's writers, the pools'.
pub(crate) const HEAP_WRITER: u8 = 0;
pub(crate) const POOL_WRITER: u8 = 1;

static WRITERS: std::sync::Mutex<std::collections::BTreeMap<(u8, usize, usize), Writer>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Where the trace of the writers goes (`heap-writers.tsv` beside the crash
/// log, told by the launcher).
static TRACE: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

/// Told by the launcher: the trace of the writers no hook took goes to `path`
/// (every ten seconds and at the end, [`write_trace`]).
pub fn trace_to(path: std::path::PathBuf) {
    *TRACE.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
}

/// A write of `writer` (called from `caller`; the stack of the fault in
/// `context`) into the heap at `heap_at`, that no hook took: traced, and the
/// whole block noted written, since what it wrote is not known. The fault is
/// then taken as any (the block is opened), and the block guarded again at the
/// next round of the sync ([`reguard`]), so that its next write is seen too.
fn dynamic(
    rip: usize,
    context: *const windows_sys::Win32::System::Diagnostics::Debug::CONTEXT,
    heap_at: usize,
) {
    traced(HEAP_WRITER, rip, context, heap_at, || None);
    COUNTS[DYNAMIC].fetch_add(1, Relaxed);
    changed(heap_at / BLOCK * BLOCK, BLOCK);
}

/// A write at `at` that no hook took, of the kind `kind` ([`HEAP_WRITER`],
/// [`POOL_WRITER`]), by the instruction at `rip` (the stack of the fault in
/// `context`): kept for its trace; `region` says where `at` lies, asked at its
/// writer's first fault.
pub(crate) fn traced(
    kind: u8,
    rip: usize,
    context: *const windows_sys::Win32::System::Diagnostics::Debug::CONTEXT,
    at: usize,
    region: impl FnOnce() -> Option<String>,
) {
    let frame = crate::frame::current();
    let caller = caller(context);
    let mut writers = WRITERS.lock().unwrap_or_else(|e| e.into_inner());
    let w = writers
        .entry((kind, rip, caller))
        .or_insert_with(|| Writer {
            stack: Vec::new(),
            count: 0,
            recent: 0,
            blocks: Default::default(),
            first: frame,
            last: frame,
            at,
            region: region(),
        });
    // (The stack of its first few faults: walking it costs.)
    if w.count < 4 && w.stack.len() < 2 {
        w.stack = stack(context, 16);
    }
    w.count += 1;
    w.recent += 1;
    w.last = frame;
    if w.blocks.len() < 4096 {
        w.blocks.insert(at / BLOCK);
    }
}

/// The return addresses of the stack of the fault in `context`, from the
/// faulting instruction on, by the images' unwind data (the word at the stack
/// pointer where a function has none), at most `most`.
fn stack(
    context: *const windows_sys::Win32::System::Diagnostics::Debug::CONTEXT,
    most: usize,
) -> Vec<usize> {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        RtlLookupFunctionEntry, RtlVirtualUnwind, UNW_FLAG_NHANDLER,
    };
    let mut out = Vec::new();
    if context.is_null() {
        return out;
    }
    // SAFETY: the context the system passed for the faulting thread.
    let mut copy = unsafe { *context };
    while out.len() < most && copy.Rip != 0 {
        out.push(copy.Rip as usize);
        let mut base = 0u64;
        // SAFETY: a lookup in the loaded images' tables.
        let entry = unsafe { RtlLookupFunctionEntry(copy.Rip, &mut base, std::ptr::null_mut()) };
        if entry.is_null() {
            let mut back = 0usize;
            // SAFETY: eight bytes into a local; a stack that is not there fails the copy.
            let read = crate::host::read_into(copy.Rsp as *const u8, unsafe {
                std::slice::from_raw_parts_mut((&raw mut back).cast::<u8>(), 8)
            });
            if !read {
                break;
            }
            copy.Rip = back as u64;
            copy.Rsp += 8;
            continue;
        }
        let (mut data, mut frame) = (std::ptr::null_mut(), 0u64);
        // SAFETY: the function's own unwind data, over a copy of its context.
        unsafe {
            RtlVirtualUnwind(
                UNW_FLAG_NHANDLER,
                base,
                copy.Rip,
                entry,
                &mut copy,
                &mut data,
                &mut frame,
                std::ptr::null_mut(),
            )
        };
    }
    out
}

/// The writers of kind `kind` no hook took, most often first: (place, caller,
/// count since the last call, in all, blocks).
pub(crate) fn writers_line(kind: u8) -> String {
    let mut writers = WRITERS.lock().unwrap_or_else(|e| e.into_inner());
    let mut v: Vec<(u64, u64, usize, usize, usize)> = writers
        .iter()
        .filter(|(key, _)| key.0 == kind)
        .map(|(&(_, rip, caller), w)| (w.recent, w.count, rip, caller, w.blocks.len()))
        .collect();
    writers
        .iter_mut()
        .filter(|(key, _)| key.0 == kind)
        .for_each(|(_, w)| w.recent = 0);
    drop(writers);
    v.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    v.iter()
        .filter(|w| w.0 != 0)
        .take(12)
        .map(|&(recent, count, rip, caller, blocks)| {
            format!(
                "{} from {} {recent} ({count} in all, {blocks} blocks)",
                place(rip),
                place(caller)
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Writes the traces of the writers no hook took: the heap's
/// (`heap-writers.tsv`) and the pools' (`pool-writers.tsv` beside it, of
/// `crate::pool_hooks`).
pub fn write_trace() {
    let Some(path) = TRACE.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
        return;
    };
    write_trace_of(HEAP_WRITER, &path);
    write_trace_of(POOL_WRITER, &path.with_file_name("pool-writers.tsv"));
}

/// The trace of the writers of kind `kind`, to `path`: place, caller, stack,
/// count, blocks, first and last frame, and where the address it wrote lies
/// (the heap's: in a buffer of the title's too, the frame allocator's memory,
/// "tail", or not, "persistent"; the pools': which pool).
fn write_trace_of(kind: u8, path: &std::path::Path) {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        usize,
        usize,
        Vec<usize>,
        u64,
        usize,
        u64,
        u64,
        usize,
        Option<String>,
    )> = WRITERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(key, _)| key.0 == kind)
        .map(|(&(_, rip, caller), w)| {
            (
                rip,
                caller,
                w.stack.clone(),
                w.count,
                w.blocks.len(),
                w.first,
                w.last,
                w.at,
                w.region.clone(),
            )
        })
        .collect();
    if rows.is_empty() {
        return;
    }
    let buffers: Vec<(u64, u64, bool)> = crate::shadow::spans()
        .into_iter()
        .filter(|s| !s.2)
        .collect();
    let mut out =
        String::from("site\tcaller\tstack\tcount\tblocks\tfirst_frame\tlast_frame\tregion\n");
    for (rip, caller, stack, count, blocks, first, last, at, region) in rows {
        let region = match region {
            Some(region) => region,
            None if buffers
                .iter()
                .any(|&(s, n, _)| (at as u64).wrapping_sub(s) < n) =>
            {
                "tail".into()
            }
            None => "persistent".into(),
        };
        let stack: Vec<String> = stack.iter().map(|&a| place(a)).collect();
        out.push_str(&format!(
            "{}\t{}\t{}\t{count}\t{blocks}\t{first}\t{last}\t{region}\n",
            place(rip),
            place(caller),
            stack.join(" < ")
        ));
    }
    if let Err(e) = std::fs::write(path, out) {
        tracing::warn!(path = %path.display(), %e, "heap hooks: the writers' trace could not be written");
    }
}

/// A write fault at `at` from the instruction at `rip`, on a block the watch
/// guards. If it is the hooked heap's (at its address, or at another the
/// title shows the same page at), no hook took that write: it is traced and
/// taken ([`dynamic`]), or with `VICTORY_HEAP_HOOKS_STRICT=1` the process
/// stops, naming the writer, its caller and the slot.
pub(crate) fn guard_fault(
    at: usize,
    rip: usize,
    context: *const windows_sys::Win32::System::Diagnostics::Debug::CONTEXT,
) {
    let (base, len) = heap();
    if len == 0 || !HOOKED.load(Relaxed) {
        return;
    }
    let heap_at = if at.wrapping_sub(base) < len {
        Some(at)
    } else {
        crate::inplace::pages_of(at / BLOCK * BLOCK)
            .into_iter()
            .find(|page| page.wrapping_sub(base) < len)
            .map(|page| page + at % BLOCK)
    };
    let Some(heap_at) = heap_at else {
        return;
    };
    if !strict() {
        dynamic(rip, context, heap_at);
        return;
    }
    let slot = (heap_at - base) / SLOT;
    let writer = place(rip);
    let caller = place(caller(context));
    let through = if heap_at == at {
        String::new()
    } else {
        format!(" (through another address of its page, {at:#x})")
    };
    tracing::error!(
        writer,
        caller,
        slot,
        at = format_args!("{heap_at:#x}"),
        "heap hooks: a write into the hooked view heap that no hook took{through}; the process stops (VICTORY_HEAP_HOOKS=1: hook this writer, or turn the hooks off)"
    );
    crate::host::fail(format_args!(
        "heap hooks: {writer} (called from {caller}) wrote slot {slot} of the hooked view heap at {heap_at:#x}{through}, and no hook took it"
    ));
}

/// Where `address` is: a hooked store's stub, the title's image, another
/// module, or none.
pub(crate) fn place(address: usize) -> String {
    if let Some(image) = IMAGE.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
        if let Some(&(span, _, _)) = image
            .stubs
            .iter()
            .find(|&&(_, stub, n)| address.wrapping_sub(stub) < n)
        {
            return format!("the stub of the hooked store at title+{span:#x}");
        }
        if address.wrapping_sub(image.base) < image.size {
            return format!("title+{:#x}", address - image.base);
        }
    }
    match crate::titlewrite::place(address) {
        Some((image, offset)) => format!("{image}+{offset:#x}"),
        None => format!("{address:#x}"),
    }
}

/// The return address of the function the fault is in, by its unwind data;
/// the word at the stack pointer where it has none.
pub(crate) fn caller(
    context: *const windows_sys::Win32::System::Diagnostics::Debug::CONTEXT,
) -> usize {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        RtlLookupFunctionEntry, RtlVirtualUnwind, UNW_FLAG_NHANDLER,
    };
    if context.is_null() {
        return 0;
    }
    // SAFETY: the context the system passed for the faulting thread.
    let mut copy = unsafe { *context };
    let mut base = 0u64;
    // SAFETY: a lookup in the loaded images' tables.
    let entry = unsafe { RtlLookupFunctionEntry(copy.Rip, &mut base, std::ptr::null_mut()) };
    if entry.is_null() {
        let mut back = 0usize;
        // SAFETY: eight bytes into a local; a stack that is not there fails the copy.
        let read = crate::host::read_into(copy.Rsp as *const u8, unsafe {
            std::slice::from_raw_parts_mut((&raw mut back).cast::<u8>(), 8)
        });
        return if read { back } else { 0 };
    }
    let (mut data, mut frame) = (std::ptr::null_mut(), 0u64);
    // SAFETY: the function's own unwind data, over a copy of its context.
    unsafe {
        RtlVirtualUnwind(
            UNW_FLAG_NHANDLER,
            base,
            copy.Rip,
            entry,
            &mut copy,
            &mut data,
            &mut frame,
            std::ptr::null_mut(),
        )
    };
    copy.Rip as usize
}

/// Since the last line: slots taken as records, as no records, as given
/// back; blocks looked at, runs, takings; blocks guarded again; ranges
/// changed by others; our own writes, the title's copies into the heap; ways
/// to our view found, not found; blocks told to the watch.
static COUNTS: [AtomicU64; 14] = [const { AtomicU64::new(0) }; 14];
const RECORDS: usize = 0;
const OTHERS: usize = 1;
const FREES: usize = 2;
const BLOCKS: usize = 3;
const RUNS: usize = 4;
const TAKES: usize = 5;
const REGUARDED: usize = 6;
const CHANGED: usize = 7;
const OURS: usize = 8;
const COPIES: usize = 9;
const WAYS: usize = 10;
const NO_WAY: usize = 11;
const TOLD: usize = 12;
const DYNAMIC: usize = 13;

/// With the frame line: what the hooks did per frame over `frames`.
pub(crate) fn line(frames: f64) {
    if !on() {
        return;
    }
    if !working() {
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| tracing::info!("heap hooks (VICTORY_HEAP_HOOKS=1): not working, the title's stores were not hooked (the launcher's log says why)"));
        return;
    }
    let c: [f64; 14] = std::array::from_fn(|i| COUNTS[i].swap(0, Relaxed) as f64 / frames);
    tracing::info!(
        "heap hooks (VICTORY_HEAP_HOOKS=1) per frame: slots taken {:.0} records, {:.0} no records, {:.0} given back; {:.1} blocks in {:.0} runs, {:.1} takings; \
         {:.2} blocks guarded again, {:.1} ranges changed by others; {:.0} records of ours, {:.0} title copies into the heap; {:.2} ways to our view found, {:.2} not; {:.1} blocks told to the watch",
        c[RECORDS],
        c[OTHERS],
        c[FREES],
        c[BLOCKS],
        c[RUNS],
        c[TAKES],
        c[REGUARDED],
        c[CHANGED],
        c[OURS],
        c[COPIES],
        c[WAYS],
        c[NO_WAY],
        c[TOLD]
    );
    // The writes no hook took, by writer: once a line, and their trace every
    // ten lines.
    let writers = writers_line(HEAP_WRITER);
    if !writers.is_empty() {
        tracing::info!(
            "heap hooks: writes no hook took, {:.1} per frame, taken and traced (VICTORY_HEAP_HOOKS_STRICT=1: stop instead): {writers}",
            c[DYNAMIC]
        );
    }
    static LINES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if LINES.fetch_add(1, Relaxed) % 10 == 9 {
        write_trace();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hooked heap is one global: one test at a time.
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A heap of `count` slots at `start` adopted with nothing noted, the ways
    /// to our view all "none"; dropped, it goes.
    struct Adopted(usize);

    impl Adopted {
        fn new(start: usize, count: usize) -> Adopted {
            assert!(adopt_now(start, count));
            let t = ADOPTED.read().unwrap();
            let t = t.as_ref().unwrap();
            t.slots.iter().for_each(|s| s.store(0, Relaxed));
            t.marks.iter().for_each(|m| m.store(0, Relaxed));
            CELLS.any.store(0, Relaxed);
            let deltas = CELLS.deltas.load(Acquire);
            for block in 0..=t.blocks {
                // SAFETY: the heap's table, one longer than its blocks.
                unsafe { (*deltas.add(block)).store(1, Relaxed) };
            }
            Adopted(start)
        }
    }

    impl Drop for Adopted {
        fn drop(&mut self) {
            let_go(self.0 as u64);
        }
    }

    #[test]
    fn cells_are_laid_out_as_the_stubs_read_them() {
        assert_eq!(size_of::<Cells>(), 56);
        assert_eq!(std::mem::offset_of!(Cells, base), 0);
        assert_eq!(std::mem::offset_of!(Cells, len), 8);
        assert_eq!(std::mem::offset_of!(Cells, deltas), 16);
        assert_eq!(std::mem::offset_of!(Cells, slots), 24);
        assert_eq!(std::mem::offset_of!(Cells, blocks), 32);
        assert_eq!(std::mem::offset_of!(Cells, told), 40);
        assert_eq!(std::mem::offset_of!(Cells, any), 48);
    }

    /// A heap adopted reads whole at its first round; then only what is noted.
    #[test]
    fn the_first_round_reads_everything() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let start = 0x7_0000_0000usize;
        assert!(adopt_now(start, 5000));
        assert_eq!(
            take(start as u64).unwrap(),
            [(0, 2048), (2048, 2048), (4096, 904)]
        );
        assert_eq!(take(start as u64).unwrap(), []);
        let_go(start as u64);
        assert_eq!(heap().1, 0);
    }

    /// Records noted come out once, as runs within a block; slots noted as no
    /// record or given back are cleared and left out; the last note of a slot
    /// counts; nothing outside the heap is noted.
    #[test]
    fn the_journal_gives_written_records_once() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let start = 0x7_1000_0000usize;
        let _heap = Adopted::new(start, 3 * SLOTS_A_BLOCK);
        let at = |slot: usize| start + slot * SLOT;
        note(at(5), 64, RECORD); // slots 5, 6
        note(at(7) + 8, 8, OTHER); // slot 7
        note(at(9), 4, FREED); // slot 9
        note(at(10) + 31, 2, RECORD); // slots 10, 11 (a store over the edge)
        note(at(20), 32, FREED);
        note(at(20), 32, RECORD); // given back, then written again
        note(at(21), 32, RECORD);
        note(at(21), 32, OTHER); // written, then other bytes over it
        // A run over a block's end is two.
        note(at(SLOTS_A_BLOCK - 1), 64, RECORD);
        // Outside: before and after.
        note(start - 64, 64, RECORD);
        note(at(3 * SLOTS_A_BLOCK), 64, RECORD);
        assert_eq!(
            take(start as u64).unwrap(),
            [
                (5, 2),
                (10, 2),
                (20, 1),
                (SLOTS_A_BLOCK as u32 - 1, 1),
                (SLOTS_A_BLOCK as u32, 1)
            ]
        );
        assert_eq!(take(start as u64).unwrap(), []);
        assert!(take(start as u64 + BLOCK as u64).is_none(), "another heap");
        // Each block noted for the watch once; the flag says so.
        assert_eq!(CELLS.any.load(Relaxed), 1);
    }

    /// A change by others notes every slot of its range in the heap; a change
    /// of what the addresses show has their blocks' ways to our view asked for
    /// again.
    #[test]
    fn a_change_by_others_is_read_whole() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let start = 0x7_2000_0000usize;
        let _heap = Adopted::new(start, 3 * SLOTS_A_BLOCK);
        changed(start + BLOCK - 64, 128);
        assert_eq!(
            take(start as u64).unwrap(),
            [(SLOTS_A_BLOCK as u32 - 2, 2), (SLOTS_A_BLOCK as u32, 2)]
        );
        let deltas = CELLS.deltas.load(Acquire);
        // SAFETY: the test's heap's table.
        let way = |block: usize| unsafe { (*deltas.add(block)).load(Relaxed) };
        assert_eq!(
            (way(0), way(1), way(2)),
            (1, 1, 1),
            "a write by others leaves the ways"
        );
        views_gone(start + BLOCK - 64, 128);
        assert_eq!((way(0), way(1), way(2)), (0, 0, 1));
        changed(0, start);
        changed(start + 3 * BLOCK, BLOCK);
        assert_eq!(take(start as u64).unwrap(), []);
    }

    /// Our own writes and copies land at our address of the heap (here: the
    /// title's, there being no view of ours) and are noted; outside the heap
    /// they are not ours to make.
    #[test]
    fn our_writes_are_made_and_noted() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let mut memory = vec![0u8; 3 * BLOCK];
        let start = (memory.as_mut_ptr() as usize).next_multiple_of(BLOCK);
        let _heap = Adopted::new(start, SLOTS_A_BLOCK);
        let record = [7u8; 12];
        assert!(write((start + 3 * SLOT) as u64, &record));
        // SAFETY: inside `memory`.
        assert_eq!(
            unsafe { std::slice::from_raw_parts((start + 3 * SLOT) as *const u8, 12) },
            &record
        );
        assert!(!write((start + BLOCK) as u64, &record), "past the heap");
        let source = [9u8; 64];
        // SAFETY: both are memory of the test's.
        assert!(unsafe { copy((start + 8 * SLOT) as u64, source.as_ptr() as u64, 64) });
        // The same bytes again: nothing to write, nothing noted.
        assert_eq!(take(start as u64).unwrap(), [(3, 1), (8, 2)]);
        // SAFETY: as above.
        assert!(unsafe { copy((start + 8 * SLOT) as u64, source.as_ptr() as u64, 64) });
        if crate::toggled!("VICTORY_SAME_RECORDS") {
            assert_eq!(take(start as u64).unwrap(), []);
        }
        drop(memory);
    }
}
