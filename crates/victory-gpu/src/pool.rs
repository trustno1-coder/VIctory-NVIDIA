//! Host memory for the textures the title fills, and for their staging
//! buffers, cut from blocks made once.
//!
//! Each such texture had an allocation of its own and its staging buffer
//! another. On an RX 7900 XTX an allocation takes about 310 microseconds
//! whatever its size, on one thread (`texture::allocation_costs`), and a
//! texture cut from a block 6: the pass that makes the textures shaders
//! pick by index (`draw.rs` `heap_textures`) got through 350-500 of the
//! 24,000 queued when the world loads in each of its 250 ms turns, some
//! forty seconds of a world without its textures.
//! `VICTORY_TEXTURE_POOL=0` makes every one an allocation of its own again.
//!
//! Without mixed heaps a piece given back is used again only two seconds later (nothing the card
//! was told before that is still running: Windows resets a card that takes
//! that long over one submission), and by then no list names it (the owners' rule, as for allocations of their own): a
//! texture's piece comes back when the last reference to the host texture
//! goes (an object of ours the texture holds says so), a staging buffer's
//! when the last list that recorded its copy is reset.
//! With `VICTORY_POOL_WINDOWS_MB` above zero, Tier2 heaps also admit window
//! buffers. Every piece of those heaps additionally waits for the actual
//! queue fence captured at final COM release; the CPU never waits here.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

/// Bytes of a block; pieces of more than half of one are not pooled (with
/// blocks made ahead, [`ahead`]: of more than one).
const BLOCK: u64 = 64 << 20;
/// Blocks with nothing in them that are kept.
const SPARE: usize = 2;
/// The same while the card has room ([`room`]): 4 GiB of them.
const SPARE_KEPT: usize = 64;
/// The same of blocks of staging buffers, which are the PC's memory: run
/// crash-1 held 5.2 GB of them.
const STAGING_KEPT: usize = 8;
/// How many are kept now.
static SPARES: AtomicUsize = AtomicUsize::new(SPARE);

/// Sets how many empty blocks stay, by the card's memory (budget and use):
/// [`SPARE_KEPT`] below 85 % ([`crate::host::keep_freed`]), else [`SPARE`].
/// Asked once a second.
pub(crate) fn room(memory: Option<[u64; 4]>) {
    let pressure = crate::residency::pressured();
    let limit = spares(memory, crate::host::keep_freed() && !pressure);
    SPARES.store(limit, Ordering::Relaxed);
    let tick = now();
    if window_limit() != 0 {
        static LAST: AtomicU64 = AtomicU64::new(0);
        let last = LAST.load(Ordering::Relaxed);
        if tick.saturating_sub(last) >= 5000 && LAST.compare_exchange(last, tick, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
            let [held, made, missed, capped, failed] = window_counts();
            tracing::info!(
                "pooled windows: {} KiB held of a {} MiB cap; since start {} offers, {} placements, \
                 {} hole misses, {} cap refusals, {} unsupported, {} native or owner failures; existing heap backing only",
                held >> 10, window_limit() >> 20, WINDOW_OFFERS.load(Ordering::Relaxed), made, missed, capped,
                WINDOW_UNSUPPORTED.load(Ordering::Relaxed), failed
            );
        }
    }
    // A full pool can become idle while another subsystem needs the VRAM.
    // A busy allocator observes the new limit on its next allocation or tick.
    if let Ok(mut pool) = TEXTURES.try_lock() {
        pool.reclaim(tick, limit, true);
    }
    if let Ok(mut pool) = STAGING.try_lock() {
        let staging_limit = if crate::residency::critical_commit() {
            0
        } else if pressure {
            1
        } else {
            limit
        };
        pool.reclaim(tick, staging_limit, true);
    }
}

fn spares(memory: Option<[u64; 4]>, keep: bool) -> usize {
    match memory {
        Some([budget, used, ..]) if keep && u128::from(used) * 100 < u128::from(budget) * 85 => SPARE_KEPT,
        _ => SPARE,
    }
}

/// Milliseconds until a piece given back is used again. (It was three
/// frames, a tenth of a second while the title shows a loading screen at 30
/// a second, with up to 18 submissions in the air.)
const REST: u64 = 2500;

fn now() -> u64 {
    crate::host::now_ms()
}

pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_POOL").is_none_or(|v| v != "0"))
}

fn parse_window_limit(value: Option<&str>) -> u64 {
    value.and_then(|v| v.trim().parse::<u64>().ok()).and_then(|mb| mb.checked_mul(1 << 20)).unwrap_or(0)
}

/// An opt-in cap on occupied placement bytes, not additional heap backing.
pub(crate) fn window_limit() -> u64 {
    static LIMIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| parse_window_limit(std::env::var("VICTORY_POOL_WINDOWS_MB").ok().as_deref()))
}

fn mixed_heaps(device: &ID3D12Device) -> bool {
    if window_limit() == 0 {
        return false;
    }
    let Some(host) = crate::host::made_host().filter(|host| host.device.as_raw() == device.as_raw()) else {
        return false;
    };
    if !host.pool_retirement_supported() {
        return false;
    }
    static MIXED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // The pool, like the production Host, belongs to one device.
    *MIXED.get_or_init(|| {
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
        // SAFETY: a plain feature query with its exact output size.
        let checked = unsafe {
            device.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, (&raw mut options).cast(), size_of_val(&options) as u32)
        };
        checked.is_ok() && options.ResourceHeapTier == D3D12_RESOURCE_HEAP_TIER_2
    })
}

/// Tier2 mixed heaps require every category-denial flag unset. Keep the
/// allocator's narrower non-RT/DS, single-sample policy in software.
fn mixed_texture_supported(desc: &D3D12_RESOURCE_DESC) -> bool {
    desc.SampleDesc.Count == 1
        && desc.Flags & (D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET | D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL) == D3D12_RESOURCE_FLAG_NONE
}

type FenceSource = (ID3D12Fence, Arc<AtomicU64>);

/// Snapshot only at final release; prepared and recorded users retain the
/// resource, so all work which could name a released piece is before it.
fn snapshot(source: &FenceSource) -> (ID3D12Fence, u64) {
    (source.0.clone(), source.1.load(Ordering::Acquire))
}

fn fenced_ready(now: u64, until: u64, completed: u64, target: u64) -> bool {
    now >= until && completed != u64::MAX && target != u64::MAX && completed >= target
}

struct Resting {
    until: u64,
    piece: Piece,
    /// None is the unchanged texture-only/staging path, or a creation
    /// failure whose piece was never exposed to any GPU work.
    fence: Option<(ID3D12Fence, u64)>,
    /// Mixed-heap accounting/cap stays charged until this extent is free.
    _charge: Option<Occupied<'static>>,
}

impl Resting {
    fn ready(&self, now: u64) -> bool {
        if now < self.until {
            return false;
        }
        match &self.fence {
            None => true,
            // SAFETY: the captured queue fence is kept alive by this entry.
            Some((fence, target)) => fenced_ready(now, self.until, unsafe { fence.GetCompletedValue() }, *target),
        }
    }
}

/// How many blocks of textures are made ahead, so that one has been there
/// for a while when its first piece is taken (`VICTORY_RIPE_BLOCKS`, 12;
/// 0: none, and new blocks are used at once, as before).
///
/// A page fault of lists already on the card, in a texture made the same
/// tenth of a second as the first piece of a block made for it, lost the
/// device in runs n08 and n11 (2026-11-25): the title gives a slot that
/// those lists read by index a new texture. Why the card does not have the
/// new block's addresses yet is not known (n11 waited for `MakeResident`;
/// `indirect::tests::new_blocks_under_readers` never loses it). A block that
/// every submission begun before its making has left behind ([`Ripe`]) has
/// no such lists: pieces are cut from those first.
fn ahead() -> usize {
    static AHEAD: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *AHEAD.get_or_init(|| std::env::var("VICTORY_RIPE_BLOCKS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(12))
}

/// Submissions begun and finished ([`crate::host::submissions`]), and how
/// many blocks to make ahead (0: every block is ripe at once).
#[derive(Clone, Copy)]
struct Ripe {
    started: u64,
    finished: u64,
    ahead: usize,
}

impl Ripe {
    const ALL: Ripe = Ripe { started: 0, finished: 0, ahead: 0 };

    fn now() -> Ripe {
        let (started, finished) = crate::host::submissions();
        // Not more than are kept ([`room`]): they would go and be made again.
        Ripe { started, finished, ahead: ahead().min(SPARES.load(Ordering::Relaxed)) }
    }

    fn is<B>(self, block: &Block<B>) -> bool {
        self.ahead == 0 || block.made <= self.finished
    }
}

/// Pieces cut from blocks that were not ripe, for want of a ripe one.
static UNRIPE: AtomicU64 = AtomicU64::new(0);

/// The free ranges of a block: start and length, never two that touch.
struct Ranges(BTreeMap<u64, u64>);

impl Ranges {
    fn new(size: u64) -> Ranges {
        Ranges(BTreeMap::from([(0, size)]))
    }

    /// The lowest place for `len` bytes at a multiple of `align`.
    fn take(&mut self, len: u64, align: u64) -> Option<u64> {
        let (start, size, at) = self.0.iter().find_map(|(&start, &size)| {
            let at = start.next_multiple_of(align);
            (at + len <= start + size).then_some((start, size, at))
        })?;
        self.0.remove(&start);
        if at > start {
            self.0.insert(start, at - start);
        }
        if at + len < start + size {
            self.0.insert(at + len, start + size - at - len);
        }
        Some(at)
    }

    fn give(&mut self, mut start: u64, mut len: u64) {
        if let Some((&before, &size)) = self.0.range(..start).next_back()
            && before + size == start
        {
            self.0.remove(&before);
            (start, len) = (before, len + size);
        }
        if let Some(size) = self.0.remove(&(start + len)) {
            len += size;
        }
        self.0.insert(start, len);
    }

    fn whole(&self, size: u64) -> bool {
        self.0.get(&0) == Some(&size)
    }
}

/// A part of a block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Piece {
    block: usize,
    start: u64,
    len: u64,
}

struct Block<B> {
    memory: B,
    free: Ranges,
    /// The end of the highest piece ever taken: above it the memory is as
    /// it was made (zeros).
    used_to: u64,
    /// The newest submission begun when it was made ([`Ripe`]).
    made: u64,
    /// When that was (ms).
    #[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
    since: u64,
}

struct Pool<B> {
    /// `None`: a block let go, its place to be used again.
    blocks: Vec<Option<Block<B>>>,
    /// Pieces given back, and when each may be used again.
    resting: VecDeque<Resting>,
    /// The empty blocks it was last told to keep, and the most it keeps.
    spare: usize,
    most: usize,
    retry_growth_at: u64,
}

impl<B: Clone> Pool<B> {
    const fn new(most: usize) -> Pool<B> {
        Pool { blocks: Vec::new(), resting: VecDeque::new(), spare: 0, most, retry_growth_at: 0 }
    }

    /// A piece of `len` bytes and the memory it lies in, and whether that
    /// part was ever used before; `make` makes a block when none has room.
    fn take(&mut self, len: u64, align: u64, now: u64, make: impl FnMut() -> Option<B>) -> Option<(Piece, B, bool)> {
        self.take_ripe(len, align, now, Ripe::ALL, make)
    }

    /// No growth and no unripe fallback: window buffers borrow only free
    /// ranges of existing compatible blocks. Resting pieces are excluded.
    fn take_existing(
        &mut self,
        len: u64,
        align: u64,
        now: u64,
        ripe: Ripe,
        mut compatible: impl FnMut(&B) -> bool,
    ) -> Option<(Piece, B)> {
        if len == 0 || len > BLOCK || !align.is_power_of_two() || align > BLOCK {
            return None;
        }
        self.rested(now);
        for (i, block) in self.blocks.iter_mut().enumerate() {
            let Some(block) = block else { continue };
            if !ripe.is(block) || !compatible(&block.memory) {
                continue;
            }
            if let Some(start) = block.free.take(len, align) {
                block.used_to = block.used_to.max(start + len);
                return Some((Piece { block: i, start, len }, block.memory.clone()));
            }
        }
        None
    }

    /// The same from a block that is ripe, if one has room; `make` makes one
    /// ahead too, when few are empty.
    fn take_ripe(
        &mut self,
        len: u64,
        align: u64,
        now: u64,
        ripe: Ripe,
        mut make: impl FnMut() -> Option<B>,
    ) -> Option<(Piece, B, bool)> {
        // With blocks made ahead, what would be an allocation of its own (and
        // new when the card first reads it) lies in one of those too.
        if len == 0 || len > if ripe.ahead > 0 { BLOCK } else { BLOCK / 2 } {
            return None;
        }
        self.rested(now);
        let mut found = None;
        for (i, block) in self.blocks.iter_mut().enumerate() {
            if let Some(block) = block
                && ripe.is(block)
                && let Some(start) = block.free.take(len, align)
            {
                found = Some((i, start));
                break;
            }
        }
        if found.is_none() && ripe.ahead > 0 {
            // The one that has been there the longest.
            let mut unripe: Vec<(u64, usize)> =
                self.blocks.iter().enumerate().filter_map(|(i, b)| Some((b.as_ref()?.made, i))).collect();
            unripe.sort_unstable();
            found = unripe.into_iter().find_map(|(_, i)| Some((i, self.blocks[i].as_mut()?.free.take(len, align)?)));
        }
        // A failed 64 MiB block must not trigger another driver call for each
        // small texture. Existing free ranges remain usable during backoff.
        let (i, start) = match found {
            Some(found) => found,
            None => {
                let memory = self.grow(now, &mut make)?;
                let i = self.add(memory, ripe.started);
                (i, self.blocks[i].as_mut()?.free.take(len, align)?)
            }
        };
        if ripe.ahead > 0 {
            if !ripe.is(self.blocks[i].as_ref()?) {
                let n = UNRIPE.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_power_of_two() {
                    tracing::info!("textures cut from blocks made while lists ran that still run: {n}");
                }
            }
            let empty = self.blocks.iter().flatten().filter(|b| b.free.whole(BLOCK)).count();
            if empty < ripe.ahead
                && let Some(memory) = self.grow(now, &mut make)
            {
                self.add(memory, ripe.started);
            }
        }
        let block = self.blocks[i].as_mut()?;
        let again = start < block.used_to;
        block.used_to = block.used_to.max(start + len);
        Some((Piece { block: i, start, len }, block.memory.clone(), again))
    }

    fn grow(&mut self, now: u64, make: &mut impl FnMut() -> Option<B>) -> Option<B> {
        if now < self.retry_growth_at {
            return None;
        }
        let block = make();
        if block.is_none() {
            self.retry_growth_at = now.saturating_add(500);
        }
        block
    }

    fn add(&mut self, memory: B, made: u64) -> usize {
        let i = match self.blocks.iter().position(Option::is_none) {
            Some(i) => i,
            None => {
                self.blocks.push(None);
                self.blocks.len() - 1
            }
        };
        self.blocks[i] = Some(Block { memory, free: Ranges::new(BLOCK), used_to: 0, made, since: crate::host::now_ms() });
        i
    }

    fn give(&mut self, piece: Piece, now: u64) {
        self.give_fenced(piece, now, None);
    }

    fn give_fenced(&mut self, piece: Piece, now: u64, fence: Option<(ID3D12Fence, u64)>) {
        self.give_charged(piece, now, fence, None);
    }

    fn give_charged(&mut self, piece: Piece, now: u64, fence: Option<(ID3D12Fence, u64)>, charge: Option<Occupied<'static>>) {
        self.resting.push_back(Resting { until: now.saturating_add(REST), piece, fence, _charge: charge });
    }

    /// Makes the pieces that rested long enough free, and lets go of empty
    /// blocks past [`SPARE`].
    fn rested(&mut self, now: u64) {
        self.reclaim(now, SPARES.load(Ordering::Relaxed), false);
    }

    fn reclaim(&mut self, now: u64, limit: usize, sweep: bool) {
        let mut any = false;
        while self.resting.front().is_some_and(|r| r.ready(now))
        {
            let retired = self.resting.pop_front().unwrap();
            let piece = retired.piece;
            if let Some(block) = self.blocks[piece.block].as_mut() {
                block.free.give(piece.start, piece.len);
                any |= block.free.whole(BLOCK);
            }
            // Release accounting only after returning the range, with the
            // pool lock still excluding another allocation of that range.
            drop(retired);
        }
        // (Without a piece given back too, once there is less room.)
        let most = limit.min(self.most);
        let previous = std::mem::replace(&mut self.spare, most);
        if sweep || any || most < previous {
            let mut spare = 0;
            for block in &mut self.blocks {
                if block.as_ref().is_some_and(|b| b.free.whole(BLOCK)) {
                    spare += 1;
                    if spare > most {
                        *block = None;
                    }
                }
            }
        }
    }

    fn blocks(&self) -> usize {
        self.blocks.iter().flatten().count()
    }
}

/// A mapped upload buffer's memory.
#[derive(Clone)]
struct Upload(ID3D12Resource, *mut u8);
// SAFETY: the mapping is written by the owners of its pieces, each its own.
unsafe impl Send for Upload {}

static TEXTURES: Mutex<Pool<ID3D12Heap>> = Mutex::new(Pool::new(SPARE_KEPT));
static STAGING: Mutex<Pool<Upload>> = Mutex::new(Pool::new(STAGING_KEPT));

fn lock<T>(m: &'static Mutex<T>) -> std::sync::MutexGuard<'static, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Charged placement bytes. Mixed heaps include retired, unreusable pieces;
/// legacy texture-only heaps keep their original live-resource accounting.
static HELD: AtomicU64 = AtomicU64::new(0);

static WINDOW_BYTES: AtomicU64 = AtomicU64::new(0);
static WINDOW_OFFERS: AtomicU64 = AtomicU64::new(0);
static WINDOW_MADE: AtomicU64 = AtomicU64::new(0);
static WINDOW_MISSED: AtomicU64 = AtomicU64::new(0);
static WINDOW_CAPPED: AtomicU64 = AtomicU64::new(0);
static WINDOW_FAILED: AtomicU64 = AtomicU64::new(0);
static WINDOW_UNSUPPORTED: AtomicU64 = AtomicU64::new(0);

/// Cap reservation rolls back before an extent is taken. Thereafter it
/// follows that extent through COM lifetime and fence/REST quarantine.
struct WindowBytes<'a> {
    counter: &'a AtomicU64,
    bytes: u64,
}

impl<'a> WindowBytes<'a> {
    fn reserve(counter: &'a AtomicU64, bytes: u64, limit: u64) -> Option<Self> {
        if bytes == 0 {
            return None;
        }
        counter.fetch_update(Ordering::AcqRel, Ordering::Relaxed, |held| {
            held.checked_add(bytes).filter(|sum| *sum <= limit)
        }).ok()?;
        Some(Self { counter, bytes })
    }
}

impl Drop for WindowBytes<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// Only atomics/cap ownership: no resource or pool lock, and no COM cycle.
struct Occupied<'a> {
    held: &'a AtomicU64,
    bytes: u64,
    _window: Option<WindowBytes<'a>>,
}

impl<'a> Occupied<'a> {
    fn new(held: &'a AtomicU64, bytes: u64, window: Option<WindowBytes<'a>>) -> Self {
        held.fetch_add(bytes, Ordering::Relaxed);
        Self { held, bytes, _window: window }
    }
}

impl Drop for Occupied<'_> {
    fn drop(&mut self) {
        self.held.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

/// Current occupied buffer bytes and cumulative successful placements,
/// hole misses, cap refusals and native/attachment failures (not residency).
pub(crate) fn window_counts() -> [u64; 5] {
    [&WINDOW_BYTES, &WINDOW_MADE, &WINDOW_MISSED, &WINDOW_CAPPED, &WINDOW_FAILED].map(|n| n.load(Ordering::Relaxed))
}

/// Uncharged pool backing. Mixed-heap fence/REST quarantine is excluded;
/// legacy texture-only heaps retain their original accounting.
pub(crate) fn texture_room() -> u64 {
    (lock(&TEXTURES).blocks() as u64 * BLOCK).saturating_sub(HELD.load(Ordering::Relaxed))
}

/// For the memory line: blocks of textures and of staging buffers, in MiB.
#[cfg(feature = "diagnostics")]
pub(crate) fn memory() -> [u64; 2] {
    [lock(&TEXTURES).blocks() as u64 * (BLOCK >> 20), lock(&STAGING).blocks() as u64 * (BLOCK >> 20)]
}

/// Part of a block of mapped upload memory, back in the pool when dropped.
pub(crate) struct StagingPiece(Piece);

impl Drop for StagingPiece {
    fn drop(&mut self) {
        lock(&STAGING).give(self.0, now());
    }
}

/// `bytes` of upload memory, all zeros: the buffer it lies in, where in
/// that buffer, and its mapping. `None`: not pooled.
pub(crate) fn staging(host: &crate::host::Host, bytes: u64) -> Option<(StagingPiece, ID3D12Resource, u64, *mut u8)> {
    if !on() {
        return None;
    }
    // Copies from a buffer begin at multiples of 512.
    let bytes = bytes.next_multiple_of(512);
    let (piece, memory, again) = lock(&STAGING).take(bytes, 512, now(), || {
        // Only gate speculative bindless prefetch on staging-pool growth
        // headroom; table-bound textures and decoration bakes are sampled
        // immediately and would otherwise read uninitialized/reused VRAM.
        let speculative = crate::texture::PICKING.get().is_some() && !crate::texture::BAKING.get();
        if speculative && !crate::residency::allow_staging_growth(false) {
            return None;
        }
        let (buffer, data) = host.upload_buffer(BLOCK)?;
        crate::host::name(&buffer, || "staging of textures (a block)".into());
        Some(Upload(buffer, data))
    })?;
    let owned = StagingPiece(piece);
    // SAFETY: the piece lies in the block's mapping.
    let data = unsafe { memory.1.add(piece.start as usize) };
    if again {
        // What is not detiled into it (an unmapped texture) uploads zeros,
        // as from a buffer of its own.
        // SAFETY: the piece is ours alone.
        unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
    }
    Some((owned, memory.0, piece.start, data))
}

/// An object the resource holds until its last COM reference goes. Mixed
/// pieces transfer their charge to retirement rather than releasing it.
#[repr(C)]
struct Reclaim {
    methods: &'static [*const (); 3],
    references: AtomicU32,
    piece: Piece,
    fence: Option<FenceSource>,
    charge: Option<Occupied<'static>>,
}

struct Methods([*const (); 3]);
// SAFETY: function addresses only.
unsafe impl Sync for Methods {}

static RECLAIM: Methods = Methods([query as *const (), add_ref as *const (), release as *const ()]);

unsafe extern "system" fn query(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    const E_NOINTERFACE: HRESULT = HRESULT(0x8000_4002_u32 as i32);
    // SAFETY: COM's arguments.
    unsafe {
        if out.is_null() {
            return HRESULT(0x8000_4003_u32 as i32);
        }
        if iid.as_ref().is_some_and(|iid| *iid == IUnknown::IID) {
            add_ref(this);
            *out = this;
            return HRESULT(0);
        }
        *out = std::ptr::null_mut();
    }
    E_NOINTERFACE
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    // SAFETY: one of ours, alive while a reference to it is.
    unsafe { (*this.cast::<Reclaim>()).references.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: as above; the last reference frees the box it was made as.
    unsafe {
        let left = (*this.cast::<Reclaim>()).references.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 {
            let mut gone = Box::from_raw(this.cast::<Reclaim>());
            let fence = gone.fence.as_ref().map(snapshot);
            let charge = gone.charge.take();
            if fence.is_some() {
                lock(&TEXTURES).give_charged(gone.piece, now(), fence, charge);
            } else {
                // Preserve off-path accounting at final resource release.
                drop(charge);
                lock(&TEXTURES).give(gone.piece, now());
            }
        }
        left
    }
}

/// Whether textures of 64 KiB and less are placed at multiples of 4 KiB in
/// their block, where the host allows it (as before; `VICTORY_POOL_SMALL=0`:
/// at multiples of 64 KiB, as every other texture).
///
/// Ported from Belobug/VIctory-Belobug-AMD c278f0d, where 64 KiB multiples
/// are the default: the two device losses of the night of 2026-11-25 with
/// the pool in that fork's report (runs steps-ab2 and start-3) faulted in
/// the same kind of texture: 256x256, block-compressed, 7 levels, placed at
/// a 4 KiB multiple that is no 64 KiB one (offsets 0x406000 and 0x3fe6000 of
/// their blocks), never used before, read by the card the moment its slot
/// was made; the descriptor's base of both lies 0xa000 into the piece, on a
/// 64 KiB multiple, and its first level of 32 KiB from there ends 0x2000
/// past the piece. Why the card faulted there is not proven; textures at
/// 64 KiB multiples have none of that, for some memory (at most 60 KiB a
/// small texture).
fn small_placement() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_POOL_SMALL") {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_POOL_SMALL").is_none_or(|v| v != "0"))
}

/// Diagnostics: whether the card reads a pooled texture inside the piece it
/// was placed in. A block's address is taken as the lowest of its textures'
/// descriptor bases less their pieces' starts (a texture's first level may
/// lie past its piece's start, never before it); a texture whose first level
/// the card would read from past its piece's end is said loudly. (Ported
/// from Belobug/VIctory-Belobug-AMD c278f0d.)
#[cfg(feature = "diagnostics")]
fn where_the_card_reads(heap: &ID3D12Heap, texture: &ID3D12Resource, piece: Piece) {
    static BLOCKS: Mutex<Vec<(usize, u64)>> = Mutex::new(Vec::new());
    static SAID: AtomicU64 = AtomicU64::new(0);
    let Some(base) = crate::book::address(texture) else { return };
    let Some(seen) = base.checked_sub(piece.start) else { return };
    let raw = heap.as_raw() as usize;
    let block = {
        let mut blocks = BLOCKS.lock().unwrap_or_else(|e| e.into_inner());
        match blocks.iter_mut().find(|b| b.0 == raw) {
            Some(b) => {
                b.1 = b.1.min(seen);
                b.1
            }
            None => {
                if blocks.len() >= 4096 {
                    blocks.remove(0);
                }
                blocks.push((raw, seen));
                seen
            }
        }
    };
    let into = base - block - piece.start;
    // SAFETY: a live texture.
    let desc = unsafe { texture.GetDesc() };
    // The first level's bytes, as a copy would have them.
    let mut level = 0u64;
    if let Some(host) = crate::host::made_host() {
        // SAFETY: plain query of a valid description.
        unsafe { host.device.GetCopyableFootprints(&desc, 0, 1, 0, None, None, None, Some(&raw mut level)) };
    }
    if into + level > piece.len {
        let n = SAID.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 50 || n.is_power_of_two() {
            tracing::error!(
                times = n,
                base = format_args!("{base:#x}"),
                piece_start = format_args!("{:#x}", piece.start),
                piece_len = format_args!("{:#x}", piece.len),
                into = format_args!("{into:#x}"),
                first_level_bytes = format_args!("{level:#x}"),
                width = desc.Width,
                height = desc.Height,
                levels = desc.MipLevels,
                format = desc.Format.0,
                "POOL: the card reads a pooled texture's first level past the end of the piece it was placed in"
            );
        }
    }
    // Every placement offset and where the first level lies, a few of them.
    static SHOWN: AtomicU64 = AtomicU64::new(0);
    if into > 0 && SHOWN.fetch_add(1, Ordering::Relaxed) < 40 {
        tracing::info!(
            into = format_args!("{into:#x}"),
            piece_start = format_args!("{:#x}", piece.start),
            piece_len = format_args!("{:#x}", piece.len),
            width = desc.Width,
            height = desc.Height,
            levels = desc.MipLevels,
            format = desc.Format.0,
            alignment = desc.Alignment,
            "pool: a texture's first level lies past its piece's start"
        );
    }
}

/// Textures made in the last second ([`fresh`]), and when.
static RECENT: Mutex<VecDeque<(usize, u64)>> = Mutex::new(VecDeque::new());

/// Whether `texture` was made in a block in the last second.
pub(crate) fn fresh(texture: &ID3D12Resource) -> bool {
    let raw = texture.as_raw() as usize;
    let since = now().saturating_sub(1000);
    RECENT.lock().unwrap_or_else(|e| e.into_inner()).iter().rev().take_while(|r| r.1 >= since).any(|r| r.0 == raw)
}

fn made_now(texture: &ID3D12Resource) {
    let now = now();
    let raw = texture.as_raw() as usize;
    {
        let mut recent = RECENT.lock().unwrap_or_else(|e| e.into_inner());
        while recent.front().is_some_and(|r| r.1 + 1000 < now) {
            recent.pop_front();
        }
        recent.push_back((raw, now));
    }
    if !crate::descriptor::host::show_new_later() {
        return;
    }
    let begun = crate::host::submissions().0;
    NEWEST_BEGUN.fetch_max(begun, Ordering::Relaxed);
    let mut unseen = UNSEEN.lock().unwrap_or_else(|e| e.into_inner());
    let finished = crate::host::submissions().1;
    // Those every submission begun before them has left behind are no
    // one's concern any more (and a raw pointer may come again).
    unseen.retain(|_, b| *b > finished);
    unseen.insert(raw, begun);
}

/// Pooled textures made while submissions begun before them may still run:
/// the submissions begun when each was made (with
/// `crate::descriptor::host::show_new_later` only). Ported from
/// Belobug/VIctory-Belobug-AMD 4cd2d13.
static UNSEEN: std::sync::LazyLock<Mutex<std::collections::HashMap<usize, u64>>> = std::sync::LazyLock::new(Default::default);
/// The newest of those numbers (nothing to look up once it has finished).
static NEWEST_BEGUN: AtomicU64 = AtomicU64::new(0);

/// Whether a texture was made in a block while a submission begun before
/// it may still run ([`begun_before`]).
fn any_unseen() -> bool {
    NEWEST_BEGUN.load(Ordering::Relaxed) > crate::host::submissions().1
}

/// The submissions begun when `texture` was made in a block, if one of
/// those may still be on the card: lists already sent then must not find it
/// in a slot they read by index (`crate::descriptor::host::show_new_later`).
/// Every device loss with the pool in the report of
/// Belobug/VIctory-Belobug-AMD (runs n08, n11, n15, steps-ab2, start-3 and a
/// user's play-1125-2107) was a page fault at the first byte of a texture
/// made that very millisecond, its title slot changed "at once" while lists
/// sent before it read that slot by index.
pub(crate) fn begun_before(texture: &ID3D12Resource) -> Option<u64> {
    if !any_unseen() {
        return None;
    }
    let finished = crate::host::submissions().1;
    let raw = texture.as_raw() as usize;
    let unseen = UNSEEN.lock().unwrap_or_else(|e| e.into_inner());
    unseen.get(&raw).copied().filter(|&begun| begun > finished)
}

/// Makes `texture` give `piece` back when the last reference to it goes.
const RECLAIM_KEY: GUID = GUID::from_u128(0x6f0e2f0c_5a59_4b1e_9a3f_7f4d2c9be713);

fn give_back_with(texture: &ID3D12Resource, piece: Piece, fence: Option<FenceSource>, charge: Occupied<'static>) {
    let made = Box::into_raw(Box::new(Reclaim {
        methods: &RECLAIM.0, references: AtomicU32::new(1), piece, fence, charge: Some(charge),
    }));
    // SAFETY: a COM object of ours with one reference, which this takes and
    // drops at the end; the texture takes its own.
    unsafe {
        let object = IUnknown::from_raw(made.cast());
        if texture.SetPrivateDataInterface(&RECLAIM_KEY, &object).is_err() {
            // Never given back: the piece stays taken, nothing is reused
            // under a texture that may be alive.
            std::mem::forget(object);
        }
    }
}

/// Borrows a free range from a mixed texture heap; never grows that pool or
/// creates a committed fallback. The caller MUST order an aliasing barrier
/// and a complete first upload before any GPU use (including zero bytes).
/// That is the existing window Card path's responsibility, not this allocator.
pub(crate) fn window_buffer(host: &crate::host::Host, size: u64) -> Option<ID3D12Resource> {
    if !on() || window_limit() == 0 {
        return None;
    }
    WINDOW_OFFERS.fetch_add(1, Ordering::Relaxed);
    if size == 0 || size > BLOCK || !mixed_heaps(&host.device) || !host.card_ready() {
        WINDOW_UNSUPPORTED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    // SAFETY: the exact buffer description which will be placed.
    let info = unsafe { host.device.GetResourceAllocationInfo(0, &[desc]) };
    if info.SizeInBytes == 0 || info.SizeInBytes > BLOCK || !info.Alignment.is_power_of_two() || info.Alignment > BLOCK {
        WINDOW_UNSUPPORTED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let Some(reserved) = WindowBytes::reserve(&WINDOW_BYTES, info.SizeInBytes, window_limit()) else {
        WINDOW_CAPPED.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    let found = lock(&TEXTURES).take_existing(info.SizeInBytes, info.Alignment, now(), Ripe::now(), |heap| {
        // SAFETY: a live pool heap. Texture-only heaps never admit a buffer.
        let flags = unsafe { heap.GetDesc() }.Flags;
        flags & (D3D12_HEAP_FLAG_DENY_BUFFERS | D3D12_HEAP_FLAG_DENY_NON_RT_DS_TEXTURES) == D3D12_HEAP_FLAG_NONE
    });
    let Some((piece, heap)) = found else {
        WINDOW_MISSED.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    let charge = Occupied::new(&HELD, piece.len, Some(reserved));
    let source = host.pool_retirement_fence();
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: the validated range belongs to this resource alone. Its
    // resource-owned COM lease keeps it occupied through every bare clone.
    let placed = unsafe {
        host.device.CreatePlacedResource(&heap, piece.start, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut made)
    };
    let made = match (placed, made) {
        (Ok(()), Some(made)) => made,
        _ => {
            lock(&TEXTURES).give_charged(piece, now(), Some(snapshot(&source)), Some(charge));
            WINDOW_FAILED.fetch_add(1, Ordering::Relaxed);
            return None;
        }
    };
    let owner = Box::into_raw(Box::new(Reclaim {
        methods: &RECLAIM.0, references: AtomicU32::new(1), piece, fence: Some(source), charge: Some(charge),
    }));
    // SAFETY: the local reference owns our COM object; successful attachment
    // retains its own. On failure neither resource nor owner escapes, so
    // dropping them quarantines the charged extent instead of leaking it.
    let attached = unsafe {
        let object = IUnknown::from_raw(owner.cast());
        made.SetPrivateDataInterface(&RECLAIM_KEY, &object)
    };
    if attached.is_err() {
        WINDOW_FAILED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    WINDOW_MADE.fetch_add(1, Ordering::Relaxed);
    #[cfg(feature = "diagnostics")]
    {
        crate::book::made(&made, || "a window buffer in a texture block".into());
        crate::book::block(&heap, &made, piece.start, BLOCK);
    }
    Some(made)
}

/// A texture of `desc` (no target, depth or unordered access) in a block;
/// `None`: not pooled, or the host refused.
///
/// # Safety
/// `desc` is a valid description.
pub(crate) unsafe fn texture(
    device: &ID3D12Device,
    desc: *const D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
) -> Option<ID3D12Resource> {
    if !on() {
        return None;
    }
    // Small textures lie at multiples of 4 KiB where the host allows it
    // (the top mip is 64 KiB at most) and [`small_placement`] says so: 64 KiB
    // each otherwise.
    // SAFETY: the caller's description.
    let mut d = unsafe { *desc };
    let mixed = mixed_heaps(device);
    if mixed && !mixed_texture_supported(&d) {
        return None;
    }
    d.Alignment = if small_placement() { u64::from(D3D12_SMALL_RESOURCE_PLACEMENT_ALIGNMENT) } else { 0 };
    // SAFETY: plain queries.
    let mut info = unsafe { device.GetResourceAllocationInfo(0, &[d]) };
    if info.SizeInBytes == u64::MAX || info.Alignment != d.Alignment {
        d.Alignment = 0;
        info = unsafe { device.GetResourceAllocationInfo(0, &[d]) };
    }
    if info.SizeInBytes == u64::MAX {
        #[cfg(test)]
        eprintln!("pool texture: invalid allocation info size={} alignment={} desc={d:?}", info.SizeInBytes, info.Alignment);
        return None;
    }
    let fence = if mixed {
        // A device-only caller must use this pool's production Host. Unknown
        // devices fall back rather than retiring behind the wrong fence.
        let host = crate::host::made_host().filter(|host| host.device.as_raw() == device.as_raw())?;
        Some(host.pool_retirement_fence())
    } else {
        None
    };
    let taken = lock(&TEXTURES).take_ripe(info.SizeInBytes, info.Alignment.max(1), now(), Ripe::now(), || {
        let desc = D3D12_HEAP_DESC {
            SizeInBytes: BLOCK,
            Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
            Alignment: 0,
            Flags: (if fence.is_some() { D3D12_HEAP_FLAG_NONE } else { D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES })
                | crate::host::not_resident(),
        };
        let mut heap: Option<ID3D12Heap> = None;
        // SAFETY: plain creation.
        if let Err(e) = unsafe { device.CreateHeap(&desc, &mut heap) } {
            #[cfg(test)]
            eprintln!("pool texture: CreateHeap failed code={:?} error={e} desc={desc:?}", e.code());
            #[cfg(test)]
            if let Some(host) = crate::host::made_host() {
                eprintln!("pool texture: native validation {:?}", host.log_messages());
            }
            crate::residency::allocation_error(&e);
            tracing::warn!(error = %e, "no block of texture memory");
        }
        if let Some(h) = &heap {
            if let Err(e) = crate::host::resident(device, h) {
                #[cfg(test)]
                eprintln!("pool texture: resident failed code={:?} error={e} desc={desc:?}", e.code());
                tracing::warn!(error = %e, "a block of texture memory is not resident");
                return None;
            }
            if let Err(e) = crate::host::mapped(device) {
                #[cfg(test)]
                eprintln!("pool texture: mapped failed code={:?} error={e} desc={desc:?}", e.code());
                tracing::warn!(error = %e, "a block of texture memory is not mapped");
                return None;
            }
        } else {
            #[cfg(test)]
            eprintln!("pool texture: CreateHeap supplied no heap desc={desc:?}");
        }
        heap
    });
    #[cfg(test)]
    if taken.is_none() {
        eprintln!("pool texture: no pool range for size={} alignment={} desc={d:?}", info.SizeInBytes, info.Alignment);
    }
    #[cfg_attr(not(feature = "diagnostics"), allow(unused_variables))]
    let (piece, heap, again) = taken?;
    let mut charge = fence.as_ref().map(|_| Occupied::new(&HELD, piece.len, None));
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: the piece is no one else's; the heap lives while a piece of
    // it is taken.
    let result = unsafe { device.CreatePlacedResource(&heap, piece.start, &d, state, None, &mut made) };
    #[cfg(test)]
    if result.is_err() || made.is_none() {
        eprintln!("pool texture: CreatePlacedResource result={result:?} returned_resource={} offset={} size={} alignment={} state={state:?} desc={d:?}",
                  made.is_some(), piece.start, piece.len, info.Alignment);
    }
    match (result, made) {
        (Ok(()), Some(made)) => {
            let charge = charge.take().unwrap_or_else(|| Occupied::new(&HELD, piece.len, None));
            give_back_with(&made, piece, fence, charge);
            made_now(&made);
            #[cfg(feature = "diagnostics")]
            {
                crate::book::made(&made, || "a texture in a block".into());
                where_the_card_reads(&heap, &made, piece);
                crate::book::block(&heap, &made, piece.start, BLOCK);
                crate::book::note(made.as_raw() as usize, || {
                    let block = lock(&TEXTURES).blocks.get(piece.block).and_then(Option::as_ref).map(|b| (b.made, b.since));
                    let (submission, since) = block.unwrap_or_default();
                    format!(
                        "; piece {:#x}+{:#x} of block {} {:#x} made {:.1} s before it at submission {submission}, used before: {again}, made at submissions {:?}",
                        piece.start,
                        piece.len,
                        piece.block,
                        heap.as_raw() as usize,
                        now().saturating_sub(since) as f64 / 1e3,
                        crate::host::submissions()
                    )
                });
            }
            Some(made)
        }
        _ => {
            lock(&TEXTURES).give_charged(piece, now(), fence.as_ref().map(snapshot), charge);
            None
        }
    }
}

#[cfg(test)]
mod window_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_texture_policy_rejects_targets_depth_and_multisampling() {
        let mut desc = D3D12_RESOURCE_DESC::default();
        desc.SampleDesc.Count = 1;
        assert!(mixed_texture_supported(&desc));
        for flags in [D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET, D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL] {
            desc.Flags = flags;
            assert!(!mixed_texture_supported(&desc));
        }
        desc.Flags = D3D12_RESOURCE_FLAG_NONE;
        for count in [0, 2, 4, 8] {
            desc.SampleDesc.Count = count;
            assert!(!mixed_texture_supported(&desc));
        }
    }

    #[test]
    fn window_cap_is_opt_in_and_rejects_overflow() {
        assert_eq!(parse_window_limit(None), 0);
        assert_eq!(parse_window_limit(Some("0")), 0);
        assert_eq!(parse_window_limit(Some(" 128 ")), 128 << 20);
        assert_eq!(parse_window_limit(Some("-1")), 0);
        assert_eq!(parse_window_limit(Some("not a number")), 0);
        assert_eq!(parse_window_limit(Some("18446744073709551615")), 0);
    }

    #[test]
    fn window_cap_reservations_rollback_and_do_not_overbook_concurrently() {
        let held = AtomicU64::new(0);
        {
            let reservation = WindowBytes::reserve(&held, 4096, 4096).unwrap();
            assert!(WindowBytes::reserve(&held, 1, 4096).is_none());
            assert_eq!(held.load(Ordering::Relaxed), 4096);
            drop(reservation);
        }
        assert_eq!(held.load(Ordering::Relaxed), 0);
        held.store(u64::MAX, Ordering::Relaxed);
        assert!(WindowBytes::reserve(&held, 1, u64::MAX).is_none());
        held.store(0, Ordering::Relaxed);
        let begin = std::sync::Barrier::new(9);
        let reserved = std::sync::Barrier::new(9);
        let accepted = AtomicU64::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let (held, begin, reserved, accepted) = (&held, &begin, &reserved, &accepted);
                scope.spawn(move || {
                    begin.wait();
                    let reservation = WindowBytes::reserve(held, 65536, 2 * 65536);
                    if reservation.is_some() {
                        accepted.fetch_add(1, Ordering::Relaxed);
                    }
                    reserved.wait();
                    drop(reservation);
                });
            }
            begin.wait();
            reserved.wait();
        });
        assert_eq!(accepted.load(Ordering::Relaxed), 2);
        assert_eq!(held.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn window_holes_never_grow_or_take_incompatible_or_resting_memory() {
        let mut pool = Pool::<u32>::new(2);
        assert!(pool.take_existing(BLOCK, 65536, 0, Ripe::ALL, |_| true).is_none());
        assert_eq!(pool.blocks(), 0);
        pool.add(1, 0);
        pool.add(2, 0);
        let (piece, heap) = pool.take_existing(BLOCK, 65536, 0, Ripe::ALL, |heap| *heap == 2).unwrap();
        assert_eq!(heap, 2);
        assert_eq!(piece, Piece { block: 1, start: 0, len: BLOCK });
        assert!(pool.take_existing(65536, 65536, 0, Ripe::ALL, |heap| *heap == 2).is_none());
        assert_eq!(pool.blocks(), 2);
        pool.give(piece, 0);
        assert!(pool.take_existing(BLOCK, 65536, REST - 1, Ripe::ALL, |heap| *heap == 2).is_none());
        assert_eq!(pool.take_existing(BLOCK, 65536, REST, Ripe::ALL, |heap| *heap == 2).unwrap().0, piece);
        assert_eq!(pool.blocks(), 2);
    }

    #[test]
    fn mixed_piece_retirement_needs_the_native_fence_and_the_rest() {
        assert!(!fenced_ready(REST - 1, REST, 99, 10));
        assert!(!fenced_ready(REST, REST, 9, 10));
        assert!(fenced_ready(REST, REST, 10, 10));
        assert!(fenced_ready(REST + 1, REST, 11, 10));
        assert!(!fenced_ready(u64::MAX, REST, u64::MAX, 10));
        assert!(!fenced_ready(u64::MAX, REST, u64::MAX, u64::MAX));
        assert!(!fenced_ready(u64::MAX, REST, 10, u64::MAX));
        assert!(fenced_ready(REST, REST, 0, 0));
    }

    #[test]
    fn retired_extent_keeps_its_cap_and_accounting_until_reclaimed() {
        static CAP: AtomicU64 = AtomicU64::new(0);
        static ACCOUNTED: AtomicU64 = AtomicU64::new(0);
        let mut pool = Pool::<u32>::new(2);
        pool.add(1, 0);
        let (piece, _) = pool.take_existing(65536, 65536, 0, Ripe::ALL, |_| true).unwrap();
        let window = WindowBytes::reserve(&CAP, piece.len, piece.len).unwrap();
        let charge = Occupied::new(&ACCOUNTED, piece.len, Some(window));
        // The same transfer is used on COM release and on failed native
        // placement after an extent was taken. Native-fence refusal is
        // covered separately and by the explicit native extent fixture.
        pool.give_charged(piece, 0, None, Some(charge));
        for tick in [0, REST - 1] {
            pool.reclaim(tick, 2, true);
            assert_eq!(CAP.load(Ordering::Relaxed), piece.len);
            assert_eq!(ACCOUNTED.load(Ordering::Relaxed), piece.len);
            assert!(WindowBytes::reserve(&CAP, 1, piece.len).is_none());
        }
        pool.reclaim(REST, 2, true);
        assert_eq!(CAP.load(Ordering::Relaxed), 0);
        assert_eq!(ACCOUNTED.load(Ordering::Relaxed), 0);
        assert_eq!(pool.take_existing(piece.len, 65536, REST, Ripe::ALL, |_| true).unwrap().0, piece);
        assert!(WindowBytes::reserve(&CAP, piece.len, piece.len).is_some());
    }

    #[test]
    fn ranges_are_cut_aligned_and_join_again() {
        let mut r = Ranges::new(1024);
        assert_eq!(r.take(100, 64), Some(0));
        assert_eq!(r.take(100, 64), Some(128));
        assert_eq!(r.take(28, 4), Some(100));
        assert_eq!(r.take(2000, 4), None);
        r.give(128, 100);
        r.give(0, 100);
        assert!(!r.whole(1024));
        r.give(100, 28);
        assert!(r.whole(1024));
    }

    #[test]
    fn empty_blocks_stay_while_the_card_has_room() {
        assert_eq!(spares(Some([100, 84, 0, 0]), true), SPARE_KEPT);
        assert_eq!(spares(Some([100, 85, 0, 0]), true), SPARE);
        assert_eq!(spares(Some([100, 10, 0, 0]), false), SPARE);
        assert_eq!(spares(None, true), SPARE);
    }

    #[test]
    fn a_failed_block_is_not_retried_for_each_small_upload() {
        let mut pool: Pool<u32> = Pool::new(2);
        let calls = std::cell::Cell::new(0);
        let refuse = || { calls.set(calls.get() + 1); None };
        for size in [704008, 179720, 179720, 24080, 48648, 524288, 89616, 351760] {
            assert!(pool.take(size, 512, 12, refuse).is_none());
        }
        assert_eq!(calls.get(), 1);
        assert!(pool.take(24080, 512, 1012, || Some(1)).is_some());
    }

    #[test]
    fn budget_tick_reclaims_idle_blocks_without_reusing_live_or_resting_pieces() {
        let mut pool = Pool::new(STAGING_KEPT);
        let (held, _, _) = pool.take(BLOCK / 2, 1, 0, || Some(1)).unwrap();
        let (resting, _, _) = pool.take(BLOCK / 2, 1, 0, || Some(2)).unwrap();
        pool.give(resting, 0);
        for i in 0..12 {
            pool.add(i, 0);
        }
        pool.reclaim(0, SPARE_KEPT, true);
        assert_eq!(pool.blocks(), STAGING_KEPT + 1);
        pool.reclaim(REST - 1, SPARE, true);
        assert_eq!(pool.blocks(), SPARE + 1);
        assert_eq!(pool.resting.len(), 1);
        pool.reclaim(REST, 0, true);
        assert_eq!(pool.blocks(), 1);
        assert!(pool.resting.is_empty());
        pool.give(held, REST);
        pool.reclaim(2 * REST, 0, true);
        assert_eq!(pool.blocks(), 0);
    }

    #[test]
    fn pieces_never_overlap_and_rest_before_they_are_used_again() {
        let mut pool: Pool<u32> = Pool::new(SPARE);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let mut taken: Vec<Piece> = Vec::new();
        for i in 0..500u64 {
            let (p, _, again) = pool.take(65536 * (1 + i % 7), 65536, 0, make).unwrap();
            assert!(!again);
            assert!(taken.iter().all(|q| q.block != p.block || p.start + p.len <= q.start || q.start + q.len <= p.start));
            taken.push(p);
        }
        let blocks = pool.blocks();
        assert!(blocks >= 2);
        // Given back at 10: not used again before 10 + REST.
        let first = taken[0];
        pool.give(first, 10);
        let (p, _, _) = pool.take(first.len, 65536, 10 + REST - 1, make).unwrap();
        assert_ne!((p.block, p.start), (first.block, first.start));
        let (q, _, again) = pool.take(first.len, 65536, 10 + REST, make).unwrap();
        assert_eq!((q.block, q.start, again), (first.block, first.start, true));
        // Everything back: the empty blocks past the spare ones go.
        for p in taken.drain(1..).chain([p, q]) {
            pool.give(p, 20);
        }
        pool.rested(20 + REST);
        assert_eq!(pool.blocks(), blocks.min(SPARE));
        // Too big for a block, and a block that cannot be made.
        assert!(pool.take(BLOCK, 65536, 30, make).is_none());
        let mut none: Pool<u32> = Pool::new(SPARE);
        assert!(none.take(65536, 65536, 0, || None).is_none());
    }

    #[test]
    fn pieces_come_from_blocks_older_than_the_lists_that_run() {
        let mut pool: Pool<u64> = Pool::new(SPARE_KEPT);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let at = |started, finished| Ripe { started, finished, ahead: 2 };
        let take = |pool: &mut Pool<u64>, ripe| pool.take_ripe(BLOCK / 2, 65536, 0, ripe, make).unwrap().1;
        // Nothing runs: at once, and two are made ahead, one a time.
        assert_eq!(take(&mut pool, at(0, 0)), 1);
        assert_eq!(made.get(), 2);
        assert_eq!(take(&mut pool, at(0, 0)), 1);
        assert_eq!(made.get(), 3);
        // 4 is made while submission 5 runs.
        assert_eq!(take(&mut pool, at(5, 4)), 2);
        assert_eq!(made.get(), 4);
        assert_eq!(take(&mut pool, at(5, 4)), 2);
        assert_eq!(take(&mut pool, at(6, 4)), 3);
        assert_eq!(take(&mut pool, at(6, 4)), 3);
        // None is ripe: the oldest; it is ripe once 5 has finished.
        assert_eq!(take(&mut pool, at(7, 4)), 4);
        assert_eq!(take(&mut pool, at(7, 5)), 4);
        // A whole block's worth too.
        assert_eq!(pool.take_ripe(BLOCK, 65536, 0, at(8, 7), make).unwrap().1, 5);
        // Off: the first with room, whatever its age, none ahead.
        let n = made.get();
        assert_eq!(pool.take(BLOCK / 2, 65536, 0, make).unwrap().1, 6);
        assert_eq!(made.get(), n);
        assert!(pool.take(BLOCK, 65536, 0, make).is_none());
    }

    /// A pooled texture's piece comes back when its last reference goes, not
    /// before; its staging piece begins as zeros.
    #[test]
    fn a_texture_gives_its_piece_back_when_it_goes() {
        let Some(host) = crate::host::host() else { return };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 256,
            Height: 256,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        // SAFETY: a valid description.
        let Some(t) = (unsafe { texture(&host.device, &desc, D3D12_RESOURCE_STATE_COMMON) }) else { return };
        assert_eq!(crate::host::references(&t), 1, "what the texture holds is no reference to it");
        let resting = || lock(&TEXTURES).resting.len();
        let before = resting();
        let other = t.clone();
        drop(t);
        assert_eq!(resting(), before);
        drop(other);
        assert_eq!(resting(), before + 1);
        // SAFETY: the piece's mapping, 4096 bytes of it ours.
        unsafe {
            let (piece, _, at, data) = staging(host, 4096).unwrap();
            assert!(at.is_multiple_of(512));
            std::ptr::write_bytes(data, 0xAB, 4096);
            drop(piece);
            lock(&STAGING).rested(u64::MAX);
            let (_piece, _, _, data) = staging(host, 4096).unwrap();
            assert!((0..4096).all(|i| *data.add(i) == 0));
            // SAFETY: plain query.
            assert!(host.device.GetDeviceRemovedReason().is_ok());
        }
    }
}
