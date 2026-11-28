//! Textures shaders write that the title's own code then reads on the CPU,
//! and the persistently mapped pool of `D3D12_HEAP_TYPE_READBACK` buffers
//! shared with `VICTORY_READ_BACK`.
//!
//! On the console the CPU and the GPU share memory, so what a compute pass
//! stores into a texture is in title memory for the CPU to read. Here it stays
//! in the texture's host copy. The title's ocean is one: its wave passes write
//! a 256x256 height map (R32_FLOAT) and a displacement map (R32G32_FLOAT), and
//! its CPU code samples both, which is what boats float on; it read memory
//! nothing had written (run rw1, `VICTORY_READ_WATCH=1`: about 30 places in
//! the title read those two kinds), and boats sank.
//!
//! For textures of the kinds in `VICTORY_TEXTURE_BACK` (comma-separated
//! `<width>x<height>f<format>`; unset or `1`: the four water formats found by
//! the upstream trace; `0`: none), a list in which shaders write one copies it
//! into a read-back buffer of ours when it closes, and once the list has run
//! the texels are tiled into the texture's title memory in the console's
//! layout, as the console's GPU would have left them.
//!
//! shortcut: by kind, not by texture: textures of those kinds the title never
//! reads are copied back too (about a megabyte per such list). The copy shows
//! what the list's last write left; a title read in between sees the frame
//! before, as it would before the console's GPU has run.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

use windows::Win32::Graphics::Direct3D12::*;

use crate::object::This;

/// Minimum bucket capacity for pooled read-back buffers.
const POOL_MIN: u64 = 4096;
/// Idle byte cap for pooled read-back buffers (64 MiB).
const POOL_CAP: u64 = 64 << 20;
/// Idle byte cap on systems with limited host RAM (32 MiB).
const POOL_CAP_LIMITED: u64 = 32 << 20;
/// Maximum retained capacity of a thread's tiling scratch buffer (4 MiB).
const TILED_CAP_MOST: usize = 4 << 20;

#[cfg(test)]
static TEST_POOL_OVERRIDE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Whether read-back buffers are pooled and kept persistently mapped
/// (`VICTORY_READBACK_POOL`, on by default; `0` disables pooling for A/B
/// benchmarking).
pub(crate) fn readback_pool() -> bool {
    #[cfg(test)]
    match TEST_POOL_OVERRIDE.load(Ordering::Relaxed) {
        1 => return false,
        2 => return true,
        _ => {}
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_READBACK_POOL").is_none_or(|v| v != "0"))
}

struct RawReadBack {
    buffer: ID3D12Resource,
    data: *mut u8,
    capacity: u64,
}

// SAFETY: `ID3D12Resource` is free-threaded COM, and `data` is the persistent
// CPU mapping of its `D3D12_HEAP_TYPE_READBACK` memory (`WRITE_BACK` coherent
// on x86-64). Only one `ReadBack` owns a `RawReadBack` at a time, and CPU reads
// happen after the GPU submission writing into it has signalled its fence.
unsafe impl Send for RawReadBack {}
// SAFETY: Shared references to `ReadBack` expose read-only access after fence
// completion and the free-threaded `ID3D12Resource`.
unsafe impl Sync for RawReadBack {}

impl Drop for RawReadBack {
    fn drop(&mut self) {
        // SAFETY: mapped once when created in `take_read_back`, never unmapped while pooled.
        unsafe { self.buffer.Unmap(0, None) };
    }
}

/// Idle persistently mapped read-back buffers, bucketed by power-of-two
/// capacity and ordered oldest-first so watermark trims evict untouched buffers.
struct ReadBackPool {
    kept: Vec<RawReadBack>,
    bytes: u64,
    low: u64,
}

impl ReadBackPool {
    const fn new() -> Self {
        Self { kept: Vec::new(), bytes: 0, low: 0 }
    }

    fn take(&mut self, capacity: u64) -> Option<RawReadBack> {
        let at = self.kept.iter().rposition(|b| b.capacity == capacity)?;
        self.bytes -= capacity;
        self.low = self.low.min(self.bytes);
        Some(self.kept.remove(at))
    }

    fn give(&mut self, raw: RawReadBack, most: u64) -> Option<RawReadBack> {
        if self.bytes.saturating_add(raw.capacity) > most {
            return Some(raw);
        }
        self.bytes += raw.capacity;
        self.kept.push(raw);
        None
    }

    fn trim_window(&mut self) -> Vec<RawReadBack> {
        let mut out = Vec::new();
        let mut left = self.low;
        while let Some(at) = self.kept.iter().position(|b| b.capacity <= left) {
            let raw = self.kept.remove(at);
            left -= raw.capacity;
            self.bytes -= raw.capacity;
            out.push(raw);
        }
        self.low = self.bytes;
        out
    }

    fn clear(&mut self) -> Vec<RawReadBack> {
        self.bytes = 0;
        self.low = 0;
        std::mem::take(&mut self.kept)
    }
}

static READBACK_POOL: Mutex<ReadBackPool> = Mutex::new(ReadBackPool::new());
static POOL_HITS: AtomicU64 = AtomicU64::new(0);
static POOL_MADE: AtomicU64 = AtomicU64::new(0);

fn bucket_capacity(size: u64) -> Option<u64> {
    size.max(POOL_MIN).checked_next_power_of_two()
}

fn pool_cap(pooled: bool, pressured: bool, limited_ram: bool) -> u64 {
    if !pooled || pressured {
        0
    } else if limited_ram {
        POOL_CAP_LIMITED
    } else {
        POOL_CAP
    }
}

/// A persistently mapped `D3D12_HEAP_TYPE_READBACK` buffer.
///
/// Recycled straight to [`READBACK_POOL`] in `HostList::reset()` once the
/// submission's fence has completed; if dropped directly (such as on an
/// aborted or unreset list), retired through `crate::host::retire` so an
/// in-flight GPU copy can never race with subsequent pool reuse or release.
pub(crate) struct ReadBack(Option<RawReadBack>);

struct RetiredReadBack(Option<RawReadBack>);

impl Drop for RetiredReadBack {
    fn drop(&mut self) {
        if let Some(raw) = self.0.take() {
            give_read_back(raw);
        }
    }
}

impl ReadBack {
    pub(crate) fn resource(&self) -> &ID3D12Resource {
        &self.0.as_ref().expect("live ReadBack").buffer
    }

    pub(crate) fn data(&self) -> *const u8 {
        self.0.as_ref().expect("live ReadBack").data.cast_const()
    }

    pub(crate) fn capacity(&self) -> u64 {
        self.0.as_ref().expect("live ReadBack").capacity
    }

    /// Returns this buffer directly to the pool. Call ONLY after every GPU
    /// command list that recorded a copy into it has finished executing (or if
    /// no command list naming it was ever submitted).
    pub(crate) fn recycle(mut self) {
        if let Some(raw) = self.0.take() {
            give_read_back(raw);
        }
    }
}

impl Drop for ReadBack {
    fn drop(&mut self) {
        if let Some(raw) = self.0.take() {
            crate::host::retire(RetiredReadBack(Some(raw)));
        }
    }
}

pub(crate) fn take_read_back(host: &crate::host::Host, size: u64) -> Option<ReadBack> {
    let pooled = readback_pool();
    let capacity = if pooled { bucket_capacity(size)? } else { size.max(1) };
    if pooled
        && !crate::residency::pressured()
        && let Some(raw) = READBACK_POOL.lock().unwrap_or_else(|e| e.into_inner()).take(capacity)
    {
        POOL_HITS.fetch_add(1, Ordering::Relaxed);
        return Some(ReadBack(Some(raw)));
    }
    let buffer = host.read_back_buffer(capacity)?;
    let mut data = std::ptr::null_mut();
    // SAFETY: a fresh `D3D12_HEAP_TYPE_READBACK` buffer of `capacity` bytes,
    // mapped once for its lifetime.
    if unsafe { buffer.Map(0, None, Some(&mut data)) }.is_err() || data.is_null() {
        tracing::warn!(capacity, "a read-back buffer could not be mapped");
        return None;
    }
    POOL_MADE.fetch_add(1, Ordering::Relaxed);
    Some(ReadBack(Some(RawReadBack { buffer, data: data.cast(), capacity })))
}

fn give_read_back(raw: RawReadBack) {
    let most = pool_cap(
        readback_pool(),
        crate::residency::pressured(),
        crate::residency::limited_host_ram(),
    );
    let mut freed = Vec::new();
    if let Some(rejected) = READBACK_POOL.lock().unwrap_or_else(|e| e.into_inner()).give(raw, most) {
        freed.push(rejected);
    }
    if crate::host::allocator_trim().is_some() {
        static LAST: AtomicU64 = AtomicU64::new(0);
        let now = crate::host::now_ms();
        let last = LAST.load(Ordering::Relaxed);
        if now >= last + 10_000 && LAST.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
            freed.extend(READBACK_POOL.lock().unwrap_or_else(|e| e.into_inner()).trim_window());
        }
    }
    // Dropped outside the pool lock (`D3DKMTDestroyAllocation2`).
    drop(freed);
}

/// Frees all idle read-back buffers when host memory is pressured.
pub(crate) fn trim_read_backs() {
    let freed = READBACK_POOL.lock().unwrap_or_else(|e| e.into_inner()).clear();
    drop(freed);
}

/// The kinds copied back: (width, height, format).
fn kinds() -> &'static [(u64, u32, u32)] {
    static KINDS: OnceLock<Vec<(u64, u32, u32)>> = OnceLock::new();
    KINDS.get_or_init(|| {
        const DEFAULT: &str = "256x256f41,256x256f16,64x64f16,128x128f36";
        let list = match std::env::var("VICTORY_TEXTURE_BACK").as_deref() {
            Ok("1") | Err(_) => DEFAULT.to_owned(),
            Ok(spec) => spec.to_owned(),
        };
        let kinds: Vec<(u64, u32, u32)> = list
            .split(',')
            .filter_map(|k| {
                let (size, format) = k.trim().split_once('f')?;
                let (w, h) = size.split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?, format.parse().ok()?))
            })
            .collect();
        if !kinds.is_empty() {
            tracing::info!(?kinds, "textures of these kinds that shaders write are copied back into title memory (VICTORY_TEXTURE_BACK)");
        }
        kinds
    })
}

/// Whether title texture `resource` is of a kind copied back.
pub(crate) fn wanted(resource: This) -> bool {
    let kinds = kinds();
    !kinds.is_empty()
        && crate::resource::texture_kind(resource).is_some_and(|k| kinds.contains(&k))
}

/// An outstanding write to a specific allocation. Mapping and resource
/// changes invalidate only the writes that could touch the replaced memory.
struct Ticket {
    owner: usize,
    at: usize,
    end: usize,
    valid: AtomicBool,
}

static TICKETS: Mutex<Vec<Weak<Ticket>>> = Mutex::new(Vec::new());
static TRACKING: AtomicBool = AtomicBool::new(false);

fn track(owner: usize, at: usize, end: usize) -> Arc<Ticket> {
    let ticket = Arc::new(Ticket { owner, at, end, valid: AtomicBool::new(true) });
    let mut tickets = TICKETS.lock().unwrap_or_else(|e| e.into_inner());
    tickets.retain(|weak| weak.strong_count() != 0);
    tickets.push(Arc::downgrade(&ticket));
    TRACKING.store(true, Ordering::Relaxed);
    ticket
}

fn invalidate(matches: impl Fn(&Ticket) -> bool) {
    if !TRACKING.load(Ordering::Relaxed) {
        return;
    }
    TICKETS.lock().unwrap_or_else(|e| e.into_inner()).retain(|weak| {
        let Some(ticket) = weak.upgrade() else { return false };
        if matches(&ticket) {
            ticket.valid.store(false, Ordering::Release);
        }
        true
    });
}

pub(crate) fn released(owner: usize) {
    invalidate(|ticket| ticket.owner == owner);
}

pub(crate) fn replaced(at: usize) {
    invalidate(|ticket| ticket.at == at);
}

pub(crate) fn remapped(at: usize, len: usize) {
    let end = at.saturating_add(len);
    invalidate(|ticket| at < ticket.end && ticket.at < end);
}

pub(crate) struct Target {
    pub(crate) copy: Arc<crate::resource::HostTexture>,
    layout: victory_formats::texture::Layout,
    ticket: Arc<Ticket>,
}

impl Target {
    pub(crate) fn new(resource: This) -> Option<Self> {
        // Keep the host copy independently of the title's resource lifetime.
        let copy = crate::resource::host_copy(resource)?;
        let _mapping = crate::watch::title_memory_reading();
        let (layout, at, size) = crate::resource::title_texture(resource)?;
        let end = at.checked_add(size).filter(|&end| at != 0 && end > at)?;
        let ticket = track(resource as usize, at, end);
        Some(Self { copy, layout, ticket })
    }

    pub(crate) fn owner(&self) -> usize {
        self.ticket.owner
    }
}

type FootprintKey = (u64, u32, u16, u16, i32);

struct Footprints {
    subresources: Vec<(D3D12_PLACED_SUBRESOURCE_FOOTPRINT, u32)>,
    mips: u32,
    total: u64,
}

fn footprints_cache() -> &'static RwLock<HashMap<FootprintKey, Arc<Footprints>>> {
    static CACHE: OnceLock<RwLock<HashMap<FootprintKey, Arc<Footprints>>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

fn footprints_of(device: &ID3D12Device, desc: &D3D12_RESOURCE_DESC) -> Option<Arc<Footprints>> {
    let key: FootprintKey = (desc.Width, desc.Height, desc.DepthOrArraySize, desc.MipLevels, desc.Format.0);
    if let Some(found) = footprints_cache().read().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return Some(found.clone());
    }
    let mips = u32::from(desc.MipLevels.max(1));
    let count = if desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        mips
    } else {
        mips * u32::from(desc.DepthOrArraySize.max(1))
    };
    let n = count as usize;
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); n];
    let mut rows = vec![0u32; n];
    let mut total = 0u64;
    // SAFETY: out arrays of `n` entries.
    unsafe {
        device.GetCopyableFootprints(
            desc,
            0,
            count,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        );
    }
    // The last footprint's advertised extent can omit row-end padding.
    // The tiler reads a pitched slice, so the mapped buffer must include it.
    let mut subresources = Vec::with_capacity(n);
    for (footprint, row_count) in footprints.into_iter().zip(rows) {
        let format = footprint.Footprint;
        let pitched = u64::from(format.RowPitch)
            .checked_mul(u64::from(row_count))?
            .checked_mul(u64::from(format.Depth))?;
        total = total.max(footprint.Offset.checked_add(pitched)?);
        subresources.push((footprint, row_count));
    }
    let built = Arc::new(Footprints { subresources, mips, total });
    let mut cache = footprints_cache().write().unwrap_or_else(|e| e.into_inner());
    Some(cache.entry(key).or_insert_with(|| built).clone())
}

/// The readback resources and the allocation the pixels belong to.
pub(crate) struct Back {
    buffer: ReadBack,
    footprints: Arc<Footprints>,
    layout: victory_formats::texture::Layout,
    ticket: Arc<Ticket>,
}

impl Back {
    pub(crate) fn range(&self) -> (usize, usize) {
        (self.ticket.at, self.ticket.end)
    }

    pub(crate) fn recycle(self) {
        self.buffer.recycle();
    }

    #[cfg(test)]
    pub(crate) fn test_range(at: usize, end: usize) -> Self {
        use victory_formats::texture::{Dimension, Swizzle, Texture, TextureDesc};
        let tex = Texture::new(&TextureDesc {
            dimension: Dimension::Tex2d,
            width: 1,
            height: 1,
            depth_or_array: 1,
            mips: 1,
            format: 41,
            samples: 1,
            swizzle: Swizzle::LINEAR,
        })
        .expect("1x1 test layout");
        Self {
            buffer: ReadBack(None),
            footprints: Arc::new(Footprints { subresources: Vec::new(), mips: 1, total: 0 }),
            layout: tex.planes.into_iter().next().expect("single plane").layout,
            ticket: track(0, at, end),
        }
    }
}

fn record_texture(
    host: &crate::host::Host,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
    layout: victory_formats::texture::Layout,
    ticket: Arc<Ticket>,
) -> Option<Back> {
    // SAFETY: a live texture kept through the command list's state tracking.
    let desc = unsafe { texture.GetDesc() };
    let footprints = footprints_of(&host.device, &desc)?;
    let buffer = host.read_back(footprints.total)?;
    let raw_dst = buffer.resource();
    for (i, (footprint, _)) in footprints.subresources.iter().enumerate() {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(raw_dst) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *footprint },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: i as u32 },
        };
        // SAFETY: an open list; the texture is in the copy-source state.
        unsafe { list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    Some(Back { buffer, footprints, layout, ticket })
}

/// Records a host copy from a texture already in COPY_SOURCE.
pub(crate) fn record(list: &ID3D12GraphicsCommandList, target: Target) -> Option<Back> {
    if !target.ticket.valid.load(Ordering::Acquire) {
        return None;
    }
    let host = crate::host::host()?;
    record_texture(host, list, &target.copy.texture, target.layout, target.ticket)
}

std::thread_local! {
    static TILED: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

fn with_tiled_scratch<R>(size: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
    TILED.with(|cell| {
        if let Ok(mut scratch) = cell.try_borrow_mut() {
            if scratch.len() < size {
                scratch.resize(size, 0);
            }
            let out = f(&mut scratch[..size]);
            if scratch.capacity() > TILED_CAP_MOST {
                *scratch = Vec::new();
            }
            out
        } else {
            let mut fallback = vec![0u8; size];
            f(&mut fallback)
        }
    })
}

/// Once the list has run, tile its texels back into the same title allocation.
pub(crate) fn land(back: &Back) {
    static COPIED: AtomicU64 = AtomicU64::new(0);
    static STALE: AtomicU64 = AtomicU64::new(0);
    let _mapping = crate::watch::title_memory_reading();
    let _ticket_guard = TICKETS.lock().unwrap_or_else(|e| e.into_inner());
    if !back.ticket.valid.load(Ordering::Acquire) {
        let skipped = STALE.fetch_add(1, Ordering::Relaxed) + 1;
        if skipped.is_power_of_two() {
            tracing::info!(skipped, "skipping texture readbacks whose title allocation changed");
        }
        return;
    }
    let (at, end) = back.range();
    let size = end - at;
    let data = back.buffer.data();
    let capacity = back.buffer.capacity() as usize;
    let landed = with_tiled_scratch(size, |tiled| {
        // Keep title bytes the subresources do not cover.
        if !crate::host::read_into(at as *const u8, tiled) {
            return false;
        }
        for (i, (footprint, rows)) in back.footprints.subresources.iter().enumerate() {
            let mips = back.footprints.mips;
            let (mip, slice) = (i as u32 % mips, i as u32 / mips);
            let f = footprint.Footprint;
            let pitch = f.RowPitch as usize;
            let len = pitch * (*rows as usize) * (f.Depth as usize);
            let offset = footprint.Offset as usize;
            if offset.saturating_add(len) > capacity {
                return false;
            }
            // SAFETY: persistently mapped `D3D12_HEAP_TYPE_READBACK` buffer of
            // `capacity` bytes, written by the GPU list whose fence has completed.
            let linear = unsafe { std::slice::from_raw_parts(data.add(offset), len) };
            if let Err(e) = back.layout.tile(tiled, mip, slice, linear, pitch, pitch * (*rows as usize)) {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| tracing::warn!(?e, mip, slice, "a texture copied back could not be tiled"));
                return false;
            }
        }
        crate::host::copy_mapped(at as *mut u8, tiled.as_ptr(), size, true);
        true
    });
    if !landed {
        return;
    }
    let copied = COPIED.fetch_add(1, Ordering::Relaxed) + 1;
    if copied == 1 || copied.is_multiple_of(128) {
        tracing::info!(copied, bytes = size, "texture readback landed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::Interface;

    /// Serializes tests that toggle `TEST_POOL_OVERRIDE` or inspect `READBACK_POOL`.
    static POOL_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct PoolModeGuard(u8);

    impl PoolModeGuard {
        fn set(pooled: bool) -> Self {
            let prev = TEST_POOL_OVERRIDE.swap(if pooled { 2 } else { 1 }, Ordering::SeqCst);
            Self(prev)
        }
    }

    impl Drop for PoolModeGuard {
        fn drop(&mut self) {
            TEST_POOL_OVERRIDE.store(self.0, Ordering::SeqCst);
        }
    }

    fn adapter_label(host: &crate::host::Host) -> String {
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter1, IDXGIFactory4};
        // SAFETY: queries the DXGI adapter matching `host.device`'s LUID.
        unsafe {
            let luid = host.device.GetAdapterLuid();
            let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0)) else {
                return "unknown D3D12 adapter".into();
            };
            let Ok(adapter) = factory.EnumAdapterByLuid::<IDXGIAdapter1>(luid) else {
                return "unknown D3D12 adapter".into();
            };
            let Ok(desc) = adapter.GetDesc1() else {
                return "unknown D3D12 adapter".into();
            };
            let end = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
            String::from_utf16_lossy(&desc.Description[..end])
        }
    }

    #[test]
    fn only_readbacks_for_changed_allocations_are_invalidated() {
        let first = track(91, 0x1000, 0x2000);
        let next = track(92, 0x2000, 0x3000);
        let far = track(93, 0x4000, 0x5000);

        remapped(0x0f00, 0x200);
        assert!(!first.valid.load(Ordering::Acquire));
        assert!(next.valid.load(Ordering::Acquire));
        replaced(0x2000);
        assert!(!next.valid.load(Ordering::Acquire));
        assert!(far.valid.load(Ordering::Acquire));

        released(93);
        assert!(!far.valid.load(Ordering::Acquire));
        assert!(track(94, 0x2000, 0x3000).valid.load(Ordering::Acquire));
    }

    #[test]
    fn bucket_sizing_caps_and_tiled_scratch_bound_hold() {
        assert_eq!(bucket_capacity(0), Some(4096));
        assert_eq!(bucket_capacity(80), Some(4096));
        assert_eq!(bucket_capacity(4096), Some(4096));
        assert_eq!(bucket_capacity(4097), Some(8192));
        assert_eq!(bucket_capacity(512 << 10), Some(512 << 10));
        assert_eq!(bucket_capacity((512 << 10) + 256), Some(1 << 20));

        assert_eq!(pool_cap(true, false, false), 64 << 20);
        assert_eq!(pool_cap(true, false, true), 32 << 20);
        assert_eq!(pool_cap(true, true, false), 0);
        assert_eq!(pool_cap(false, false, false), 0);

        // Scratch <= 4 MiB stays retained across calls; > 4 MiB is released immediately after use.
        with_tiled_scratch(512 << 10, |s| assert_eq!(s.len(), 512 << 10));
        TILED.with(|c| assert!(c.borrow().capacity() >= 512 << 10 && c.borrow().capacity() <= TILED_CAP_MOST));
        with_tiled_scratch(TILED_CAP_MOST + 4096, |s| assert_eq!(s.len(), TILED_CAP_MOST + 4096));
        TILED.with(|c| assert_eq!(c.borrow().capacity(), 0, "oversized tiling scratch must not stay pinned"));
    }

    #[test]
    fn recycle_reuses_immediately_while_drop_retires_and_footprints_cache() {
        let _serial = POOL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(host) = crate::host::host() else {
            return;
        };
        let _mode = PoolModeGuard::set(true);
        trim_read_backs();

        let rb = host.read_back(80).expect("4 KiB bucket read-back");
        assert_eq!(rb.capacity(), 4096);
        let ptr1 = rb.resource().as_raw();
        let map1 = rb.data();
        rb.recycle();

        let rb2 = host.read_back(2048).expect("reused 4 KiB bucket read-back");
        assert_eq!(rb2.resource().as_raw(), ptr1, "recycle() must return straight to the pool");
        assert_eq!(rb2.data(), map1, "persistent mapping must be preserved across pool reuse");

        // Dropping without `recycle()` routes through `host::retire` rather than immediately
        // re-entering the pool while `bury_lag` holds retired GPU objects.
        drop(rb2);
        let pooled_now = READBACK_POOL.lock().unwrap_or_else(|e| e.into_inner()).bytes;
        if std::env::var("VICTORY_BURY_LAG").ok().and_then(|v| v.parse::<u64>().ok()) != Some(0) {
            assert_eq!(pooled_now, 0, "unrecycled Drop must retire rather than race in-flight GPU work");
        }

        // `footprints_of` returns the exact same `Arc<Footprints>` on subsequent calls.
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 256,
            Height: 256,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32G32_FLOAT,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        let fp1 = footprints_of(&host.device, &desc).expect("footprints 1");
        let fp2 = footprints_of(&host.device, &desc).expect("footprints 2");
        assert!(Arc::ptr_eq(&fp1, &fp2), "GetCopyableFootprints must hit the cache after first sight");
        assert_eq!(fp1.total, 512 << 10);
        trim_read_backs();
    }

    /// End-to-end GPU verification and 50-iteration A/B benchmark (`VICTORY_READBACK_POOL=0`
    /// vs `VICTORY_READBACK_POOL=1`) for both `texback` (`256x256` `R32G32_FLOAT` 512 KiB
    /// ocean displacement texture) and `read_backs` (`5x1` `R32G32B32A32_FLOAT` exposure
    /// metering rows).
    #[test]
    fn gpu_texback_and_read_backs_bit_exact_and_pool_benchmark() {
        use victory_formats::texture::{Dimension, Swizzle, Texture, TextureDesc};
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_R32G32_FLOAT, DXGI_FORMAT_R32G32B32A32_FLOAT, DXGI_SAMPLE_DESC,
        };
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree};

        let _serial = POOL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(host) = crate::host::host() else {
            return;
        };
        let adapter = adapter_label(host);

        // 1. Create 256x256 R32G32_FLOAT (512 KiB) displacement texture + 5x2 R32G32B32A32_FLOAT exposure texture on GPU.
        const W: u32 = 256;
        const H: u32 = 256;
        const DISP_PITCH: usize = (W as usize) * 8; // 2048 bytes/row (256-aligned)
        const DISP_BYTES: usize = DISP_PITCH * (H as usize); // 512 KiB
        const EXP_W: u32 = 5;
        const EXP_ROWS: usize = 2;
        const EXP_ROW_BYTES: usize = (EXP_W as usize) * 16; // 80 bytes/row
        const EXP_SRC_PITCH: usize = 256;
        const EXP_DST_PITCH: usize = 128;

        let default_heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let make_tex = |width: u64, height: u32, format| -> ID3D12Resource {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: width,
                Height: height,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
                ..Default::default()
            };
            let mut tex: Option<ID3D12Resource> = None;
            // SAFETY: committed GPU texture creation for test.
            unsafe {
                host.device
                    .CreateCommittedResource(&default_heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut tex)
                    .expect("test GPU texture");
            }
            tex.unwrap()
        };
        let disp_tex = make_tex(u64::from(W), H, DXGI_FORMAT_R32G32_FLOAT);
        let exp_tex = make_tex(u64::from(EXP_W), EXP_ROWS as u32, DXGI_FORMAT_R32G32B32A32_FLOAT);

        // Deterministic f32x2 displacement field + exposure metering floats.
        let mut expected_disp = vec![0u8; DISP_BYTES];
        for y in 0..H as usize {
            for x in 0..W as usize {
                let u = ((x as f32) * 0.125 - 16.0) + (y as f32) * 0.03125;
                let v = ((y as f32) * -0.25 + 8.5) - (x as f32) * 0.015625;
                let off = y * DISP_PITCH + x * 8;
                expected_disp[off..off + 4].copy_from_slice(&u.to_le_bytes());
                expected_disp[off + 4..off + 8].copy_from_slice(&v.to_le_bytes());
            }
        }
        let mut expected_exp = vec![0u8; EXP_SRC_PITCH * EXP_ROWS];
        for r in 0..EXP_ROWS {
            for c in 0..(EXP_ROW_BYTES / 4) {
                let val = (r as f32) * 10.0 + (c as f32) * 0.5 + 1.25;
                let off = r * EXP_SRC_PITCH + c * 4;
                expected_exp[off..off + 4].copy_from_slice(&val.to_le_bytes());
            }
        }

        // Upload both textures on the GPU and transition to COPY_SOURCE.
        {
            let init_list = host.new_list().expect("init list");
            let (up_disp, up_disp_ptr) = host.upload_buffer(DISP_BYTES as u64).expect("disp upload");
            let (up_exp, up_exp_ptr) = host.upload_buffer((EXP_SRC_PITCH * EXP_ROWS) as u64).expect("exp upload");
            // SAFETY: mapped upload buffers sized to exact slices; open command list.
            unsafe {
                std::ptr::copy_nonoverlapping(expected_disp.as_ptr(), up_disp_ptr, DISP_BYTES);
                std::ptr::copy_nonoverlapping(expected_exp.as_ptr(), up_exp_ptr, EXP_SRC_PITCH * EXP_ROWS);
                let disp_src = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&up_disp),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                            Offset: 0,
                            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                                Format: DXGI_FORMAT_R32G32_FLOAT,
                                Width: W,
                                Height: H,
                                Depth: 1,
                                RowPitch: DISP_PITCH as u32,
                            },
                        },
                    },
                };
                let disp_dst = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&disp_tex),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
                };
                init_list.list.CopyTextureRegion(&disp_dst, 0, 0, 0, &disp_src, None);

                let exp_src = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&up_exp),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                            Offset: 0,
                            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                                Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                                Width: EXP_W,
                                Height: EXP_ROWS as u32,
                                Depth: 1,
                                RowPitch: EXP_SRC_PITCH as u32,
                            },
                        },
                    },
                };
                let exp_dst = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&exp_tex),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
                };
                init_list.list.CopyTextureRegion(&exp_dst, 0, 0, 0, &exp_src, None);

                let barriers = [
                    crate::host::transition(&disp_tex, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE),
                    crate::host::transition(&exp_tex, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE),
                ];
                init_list.list.ResourceBarrier(&barriers);
            }
            init_list.close();
            host.execute(&[&init_list]);
            init_list.reset();
        }

        let console_tex = Texture::new(&TextureDesc {
            dimension: Dimension::Tex2d,
            width: W,
            height: H,
            depth_or_array: 1,
            mips: 1,
            format: DXGI_FORMAT_R32G32_FLOAT.0 as u32,
            samples: 1,
            swizzle: Swizzle::SW_4KB_S,
        })
        .expect("console SW_4KB_S layout for 256x256 R32G32_FLOAT");
        let layout = console_tex.planes[0].layout.clone();
        let tiled_size = console_tex.size as usize;
        assert_eq!(tiled_size, DISP_BYTES);

        let exp_title_size = EXP_DST_PITCH * EXP_ROWS;
        let alloc_bytes = tiled_size + 4096 + exp_title_size;
        // SAFETY: committed readable/writable title memory for the duration of the test.
        let title_mem = unsafe { VirtualAlloc(std::ptr::null(), alloc_bytes, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) }.cast::<u8>();
        assert!(!title_mem.is_null());

        const ITERS: usize = 50;
        let run_mode = |pooled: bool| -> (Vec<f64>, Vec<f64>, u64, u64) {
            let _guard = PoolModeGuard::set(pooled);
            trim_read_backs();
            let list = host.new_list().expect("bench list");
            let disp_at = title_mem as usize;
            let disp_end = disp_at + tiled_size;
            // SAFETY: within `alloc_bytes`.
            let exp_dst = unsafe { title_mem.add(tiled_size + 4096) };

            let record_once = |host_list: &crate::host::HostList| {
                let ticket = track(0xdead_beef, disp_at, disp_end);
                let back = record_texture(host, &host_list.list, &disp_tex, layout.clone(), ticket).expect("texback record");
                host_list.defer_texture_back(back);

                let exp_rb = host.read_back((EXP_SRC_PITCH * EXP_ROWS) as u64).expect("exp read_back");
                // SAFETY: open command list; `exp_tex` is in COPY_SOURCE and `exp_rb` in COPY_DEST.
                unsafe {
                    let to = D3D12_TEXTURE_COPY_LOCATION {
                        pResource: std::mem::transmute_copy(exp_rb.resource()),
                        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                            PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                                Offset: 0,
                                Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                                    Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                                    Width: EXP_W,
                                    Height: EXP_ROWS as u32,
                                    Depth: 1,
                                    RowPitch: EXP_SRC_PITCH as u32,
                                },
                            },
                        },
                    };
                    let from = D3D12_TEXTURE_COPY_LOCATION {
                        pResource: std::mem::transmute_copy(&exp_tex),
                        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
                    };
                    host_list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
                }
                host_list.defer_read_back(exp_rb, exp_dst, [EXP_ROW_BYTES, EXP_ROWS, EXP_DST_PITCH]);
            };

            // Warmup 3 iterations so steady-state pool/driver state is measured cleanly.
            for _ in 0..3 {
                record_once(&list);
                list.close();
                host.execute(&[&list]);
                list.reset();
            }

            let hits_before = POOL_HITS.load(Ordering::Relaxed);
            let made_before = POOL_MADE.load(Ordering::Relaxed);
            let mut total_samples_us = Vec::with_capacity(ITERS);
            let mut cpu_samples_us = Vec::with_capacity(ITERS);

            for iter in 0..ITERS {
                if iter == ITERS - 1 {
                    // Poison title memory before the final iteration to prove bit-exact overwrite.
                    // SAFETY: within `alloc_bytes`.
                    unsafe {
                        title_mem.write_bytes(0xcd, tiled_size);
                        exp_dst.write_bytes(0xfe, exp_title_size);
                    }
                }
                let t0 = std::time::Instant::now();
                record_once(&list);
                list.close();
                let t_rec = t0.elapsed();

                host.execute(&[&list]);

                let t_rst0 = std::time::Instant::now();
                list.reset();
                let t_rst = t_rst0.elapsed();

                total_samples_us.push(t0.elapsed().as_secs_f64() * 1e6);
                cpu_samples_us.push((t_rec + t_rst).as_secs_f64() * 1e6);
            }
            let hits = POOL_HITS.load(Ordering::Relaxed) - hits_before;
            let made = POOL_MADE.load(Ordering::Relaxed) - made_before;

            // Bit-exact verification of tiled 256x256 R32G32_FLOAT displacement map in title memory.
            // SAFETY: `tiled_size` bytes at `title_mem` were written by `texback::land`.
            let landed_tiled = unsafe { std::slice::from_raw_parts(title_mem, tiled_size) };
            let mut detiled = vec![0u8; DISP_BYTES];
            layout.detile(landed_tiled, 0, 0, &mut detiled, DISP_PITCH, DISP_BYTES).expect("detile landed output");
            assert_eq!(detiled, expected_disp, "bit-exact 256x256 R32G32_FLOAT texback mismatch (pooled={pooled})");

            // Bit-exact verification of pitched `read_backs` rows + untouched padding between rows.
            // SAFETY: `exp_title_size` bytes at `exp_dst`.
            let landed_exp = unsafe { std::slice::from_raw_parts(exp_dst, exp_title_size) };
            for r in 0..EXP_ROWS {
                let row_slice = &landed_exp[r * EXP_DST_PITCH..r * EXP_DST_PITCH + EXP_ROW_BYTES];
                assert_eq!(row_slice, &expected_exp[r * EXP_SRC_PITCH..r * EXP_SRC_PITCH + EXP_ROW_BYTES]);
                assert!(
                    landed_exp[r * EXP_DST_PITCH + EXP_ROW_BYTES..(r + 1) * EXP_DST_PITCH].iter().all(|&b| b == 0xfe),
                    "inter-row title padding must remain untouched"
                );
            }

            (total_samples_us, cpu_samples_us, hits, made)
        };

        let (mut unpooled_total, mut unpooled_cpu, unpooled_hits, unpooled_made) = run_mode(false);
        let (mut pooled_total, mut pooled_cpu, pooled_hits, pooled_made) = run_mode(true);
        trim_read_backs();

        // SAFETY: freeing test VirtualAlloc block after GPU & CPU work has settled.
        unsafe {
            assert_ne!(VirtualFree(title_mem.cast(), 0, MEM_RELEASE), 0);
        }

        assert_eq!((unpooled_hits, unpooled_made), (0, (ITERS * 2) as u64));
        assert_eq!((pooled_hits, pooled_made), ((ITERS * 2) as u64, 0));

        unpooled_total.sort_by(f64::total_cmp);
        pooled_total.sort_by(f64::total_cmp);
        unpooled_cpu.sort_by(f64::total_cmp);
        pooled_cpu.sort_by(f64::total_cmp);

        let sum_ms = |v: &[f64]| v.iter().sum::<f64>() / 1e3;
        let med_us = |v: &[f64]| v[v.len() / 2];
        let p95_us = |v: &[f64]| v[(v.len() * 95) / 100];

        println!(
            "[{adapter}] {ITERS} iterations (256x256 R32G32_FLOAT 512 KiB texback + 5x2 RGBA32F read_backs):\n  \
             VICTORY_READBACK_POOL=0: total {:.2} ms (median {:.1} us, p95 {:.1} us) | record+reset CPU total {:.2} ms (median {:.1} us)\n  \
             VICTORY_READBACK_POOL=1: total {:.2} ms (median {:.1} us, p95 {:.1} us) | record+reset CPU total {:.2} ms (median {:.1} us)\n  \
             Speedup: {:.2}x end-to-end ({:.2}x median), {:.2}x record+reset CPU ({:.2}x median)",
            sum_ms(&unpooled_total),
            med_us(&unpooled_total),
            p95_us(&unpooled_total),
            sum_ms(&unpooled_cpu),
            med_us(&unpooled_cpu),
            sum_ms(&pooled_total),
            med_us(&pooled_total),
            p95_us(&pooled_total),
            sum_ms(&pooled_cpu),
            med_us(&pooled_cpu),
            sum_ms(&unpooled_total) / sum_ms(&pooled_total),
            med_us(&unpooled_total) / med_us(&pooled_total),
            sum_ms(&unpooled_cpu) / sum_ms(&pooled_cpu),
            med_us(&unpooled_cpu) / med_us(&pooled_cpu),
        );
        assert_eq!(host.log_messages(), None);
    }
}
