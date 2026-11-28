//! Host-memory admission for speculative texture work and reusable upload storage,
//! and the placement ledger: which heap class the API reports for the host buffers
//! the layer creates, and which copies had to stay in system memory.
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
static PRESSURED: AtomicBool = AtomicBool::new(false);
static HEADROOM: AtomicU64 = AtomicU64::new(u64::MAX);

fn memory_status() -> Option<MEMORYSTATUSEX> {
    // SAFETY: the API initializes the fields of a correctly sized structure.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status)
}

// The texture sweep may run for seconds inside a single submitted frame.
static NEXT_COMMIT_SAMPLE: AtomicU64 = AtomicU64::new(0);
static AVAILABLE_COMMIT: AtomicU64 = AtomicU64::new(u64::MAX);
static RETRY_UPLOAD_AT: AtomicU64 = AtomicU64::new(0);

pub(crate) fn allocation_error(error: &windows::core::Error) {
    if error.code() == windows::Win32::Foundation::E_OUTOFMEMORY {
        RETRY_UPLOAD_AT.fetch_max(crate::host::coarse_ms().saturating_add(500), Ordering::Relaxed);
    }
}

thread_local! {
    static STAGING_DEFERRED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn note_staging_deferred() {
    STAGING_DEFERRED.set(true);
}

pub(crate) fn take_staging_deferred() -> bool {
    STAGING_DEFERRED.replace(false)
}

pub(crate) fn upload_refused() -> bool {
    crate::host::coarse_ms() < RETRY_UPLOAD_AT.load(Ordering::Relaxed)
}

fn sample_commit(now: u64) -> u64 {
    let next = NEXT_COMMIT_SAMPLE.load(Ordering::Relaxed);
    if now >= next
        && NEXT_COMMIT_SAMPLE
            .compare_exchange(next, now.saturating_add(32), Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        && let Some(status) = memory_status()
    {
        AVAILABLE_COMMIT.store(status.ullAvailPageFile, Ordering::Relaxed);
    }
    AVAILABLE_COMMIT.load(Ordering::Relaxed)
}

fn has_upload_reserve(commit: u64, bake: bool) -> bool {
    // Keep a protective floor against Rust/game OOM aborts without starving
    // bindless world textures or one-shot ped decoration bakes when steady-state
    // commit headroom sits in the 1-4 GiB range on 32 GiB hosts.
    commit >= if bake { 256 * MIB } else { 512 * MIB }
}

fn has_growth_reserve(commit: u64, bake: bool) -> bool {
    // Growing a fresh 64 MiB upload block for speculative bindless prefetch
    // requires more headroom than reusing slices already committed inside the
    // staging pool.
    commit >= if bake { 256 * MIB } else { 1024 * MIB }
}

/// Leave commit for game allocations and submissions already in flight while
/// still letting bindless textures and one-shot material bakes upload.
pub(crate) fn admit_texture_pass(bake: bool) -> bool {
    let now = crate::host::coarse_ms();
    if now < RETRY_UPLOAD_AT.load(Ordering::Relaxed) {
        return false;
    }
    has_upload_reserve(sample_commit(now), bake)
}

/// Whether the staging allocator may commit a brand-new OS upload buffer
/// (reusing existing free ranges in already-committed pool blocks stays open
/// whenever `admit_texture_pass` is true).
pub(crate) fn allow_staging_growth(bake: bool) -> bool {
    let now = crate::host::coarse_ms();
    if now < RETRY_UPLOAD_AT.load(Ordering::Relaxed) {
        return false;
    }
    has_growth_reserve(sample_commit(now), bake)
}

pub(crate) fn limited_host_ram() -> bool {
    static LIMITED: OnceLock<bool> = OnceLock::new();
    *LIMITED.get_or_init(|| memory_status().is_some_and(|s| s.ullTotalPhys <= 32 * GIB))
}

fn next_pressure(was: bool, physical: u64, commit: u64) -> bool {
    if was {
        physical < 5 * GIB || commit < 6 * GIB
    } else {
        physical <= 3 * GIB || commit <= 4 * GIB
    }
}

/// Called by the once-per-second submission sweep. A failed sample keeps the last good state.
pub(crate) fn refresh() {
    let Some(status) = memory_status() else { return };
    let (physical, commit) = (status.ullAvailPhys, status.ullAvailPageFile);
    HEADROOM.store(physical.min(commit), Ordering::Relaxed);
    let was = pressured();
    let pressure = next_pressure(was, physical, commit);
    if pressure != was {
        PRESSURED.store(pressure, Ordering::Relaxed);
        if pressure {
            tracing::warn!(
                pid = std::process::id(),
                frame = crate::frame::current(),
                physical_free_mib = physical >> 20,
                available_commit_mib = commit >> 20,
                vram_copy_mib = crate::shadow::live_span_bytes() >> 20,
                system_fallback_copy_mib = fallback_live_bytes() >> 20,
                texture_staging_mib = crate::texture::live_staging_bytes() >> 20,
                spare_scratch_mib = crate::host::ALIVE[4].load(Ordering::Relaxed) >> 20,
                game_filled_gpu_texture_mib = crate::resource::KINDS[1].load(Ordering::Relaxed) >> 20,
                "host memory pressure: limiting texture prefetch and idle upload storage"
            );
        } else {
            tracing::info!(physical_free_mib = physical >> 20, available_commit_mib = commit >> 20, "host memory pressure cleared");
        }
    }
}

pub(crate) fn headroom() -> Option<u64> {
    let bytes = HEADROOM.load(Ordering::Relaxed);
    (bytes != u64::MAX).then_some(bytes)
}

pub(crate) fn pressured() -> bool {
    PRESSURED.load(Ordering::Relaxed)
}

/// True when OS commit headroom is critically low (< 1.5 GiB) or an allocation
/// just failed with `E_OUTOFMEMORY`, requiring emergency throttling.
pub(crate) fn critical_commit() -> bool {
    let now = crate::host::coarse_ms();
    now < RETRY_UPLOAD_AT.load(Ordering::Relaxed) || sample_commit(now) < 1536 * MIB
}

/// A soft per-frame admission limit. A single texture can cross it. The
/// queue retains unserved slots so they can be retried on later frames.
pub(crate) fn prefetch_limit(bytes: u64, pressure: bool, critical: bool, limited: bool, staging: u64) -> u64 {
    if critical {
        bytes.min(16 << 20)
    } else if pressure || (limited && staging >= GIB) {
        bytes.min(64 << 20)
    } else {
        bytes
    }
}

// --- Where the copies went -------------------------------------------------
//
// The admission rules above decide how much host memory the layer may take.
// What the copy machinery (shadow.rs) and the host buffer creators (host.rs)
// then do is recorded here: the heap class the API reports for every host
// buffer made, and the copies that had to stay in system memory because no
// device-local side was available. This changes no behaviour; it is the
// ledger the separation work is measured with (`docs/memory-separation.md`).

use windows::Win32::Graphics::Direct3D12::{
    D3D12_CPU_PAGE_PROPERTY_WRITE_BACK, D3D12_HEAP_PROPERTIES, D3D12_HEAP_TYPE_CUSTOM, D3D12_HEAP_TYPE_DEFAULT,
    D3D12_HEAP_TYPE_GPU_UPLOAD, D3D12_HEAP_TYPE_READBACK, D3D12_HEAP_TYPE_UPLOAD, D3D12_MEMORY_POOL_L0,
    ID3D12Resource,
};

/// The heap class the API reports for a host buffer, from
/// `ID3D12Resource::GetHeapProperties` (what the source asked for, before a
/// driver's answer).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HeapClass {
    /// `DEFAULT`: GPU-owned, device-local memory as the system accounts it.
    DeviceLocal,
    /// `CUSTOM`/WRITE_BACK/L0: system memory the CPU maps and shaders reach.
    CpuVisible,
    /// `UPLOAD`: CPU-written staging.
    Staging,
    /// `READBACK`: GPU-to-CPU staging.
    Readback,
    /// `GPU_UPLOAD`: VRAM the CPU maps (resizable BAR).
    MappedVram,
    /// Anything else: the API answered differently than the creation site asked.
    Other,
}

const CLASSES: usize = 6;

impl HeapClass {
    fn index(self) -> usize {
        match self {
            HeapClass::DeviceLocal => 0,
            HeapClass::CpuVisible => 1,
            HeapClass::Staging => 2,
            HeapClass::Readback => 3,
            HeapClass::MappedVram => 4,
            HeapClass::Other => 5,
        }
    }
}

/// Buffers made since the start, as (count, bytes), by the class the API reports.
static MADE_BY_CLASS: [[AtomicU64; 2]; CLASSES] = [const { [const { AtomicU64::new(0) }; 2] }; CLASSES];

fn class_of_properties(props: &D3D12_HEAP_PROPERTIES) -> HeapClass {
    if props.Type == D3D12_HEAP_TYPE_DEFAULT {
        HeapClass::DeviceLocal
    } else if props.Type == D3D12_HEAP_TYPE_UPLOAD {
        HeapClass::Staging
    } else if props.Type == D3D12_HEAP_TYPE_READBACK {
        HeapClass::Readback
    } else if props.Type == D3D12_HEAP_TYPE_GPU_UPLOAD {
        HeapClass::MappedVram
    } else if props.Type == D3D12_HEAP_TYPE_CUSTOM
        && props.CPUPageProperty == D3D12_CPU_PAGE_PROPERTY_WRITE_BACK
        && props.MemoryPoolPreference == D3D12_MEMORY_POOL_L0
    {
        HeapClass::CpuVisible
    } else {
        HeapClass::Other
    }
}

/// The class of a host buffer, as the API reports it now.
pub(crate) fn class_of(resource: &ID3D12Resource) -> HeapClass {
    let mut props = D3D12_HEAP_PROPERTIES::default();
    // SAFETY: a live resource, and an out structure of the right type.
    match unsafe { resource.GetHeapProperties(Some(&raw mut props), None) } {
        Ok(()) => class_of_properties(&props),
        Err(_) => HeapClass::Other,
    }
}

/// A host buffer was created: count it by the class the API reports.
pub(crate) fn placed(resource: &ID3D12Resource, bytes: u64) {
    let [count, total] = &MADE_BY_CLASS[class_of(resource).index()];
    count.fetch_add(1, Ordering::Relaxed);
    total.fetch_add(bytes, Ordering::Relaxed);
}

/// Why a copy has no device-local side (see `shadow::card_admission`,
/// `shadow::window_admission` and `shadow::by_address`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Fallback {
    /// The switch that makes device-local sides is off.
    SwitchOff,
    /// Too small, too big, or the path does not apply to it.
    NotEligible,
    /// The host compare machinery or the device is missing.
    HostUnavailable,
    /// The DXGI budget line refused it.
    Budget,
    /// The path's own byte cap refused it.
    Cap,
    /// D3D12 refused the allocation.
    AllocationFailed,
    /// A copy that is never given a device-local side by design (a view's own bytes).
    ViewCopy,
}

const FALLBACKS: usize = 7;

impl Fallback {
    fn index(self) -> usize {
        match self {
            Fallback::SwitchOff => 0,
            Fallback::NotEligible => 1,
            Fallback::HostUnavailable => 2,
            Fallback::Budget => 3,
            Fallback::Cap => 4,
            Fallback::AllocationFailed => 5,
            Fallback::ViewCopy => 6,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Fallback::SwitchOff => "switch off",
            Fallback::NotEligible => "not eligible",
            Fallback::HostUnavailable => "host unavailable",
            Fallback::Budget => "budget",
            Fallback::Cap => "cap",
            Fallback::AllocationFailed => "allocation failed",
            Fallback::ViewCopy => "view copy",
        }
    }
}

/// Copies alive whose GPU-visible side is system memory, as (count, bytes), by reason;
/// and those made since the start.
static FALLBACK_LIVE: [[AtomicU64; 2]; FALLBACKS] = [const { [const { AtomicU64::new(0) }; 2] }; FALLBACKS];
static FALLBACK_TOTAL: [[AtomicU64; 2]; FALLBACKS] = [const { [const { AtomicU64::new(0) }; 2] }; FALLBACKS];

/// Directly backed copies: not copies at all, but buffers placed over the title's own memory
/// (`crate::inplace`, `crate::port`), so the GPU reads and writes the original backing bytes and
/// no mirror, refresh, send, compare or copy-back exists for them. Counted while they live and
/// since the start, windows (0) and title buffers (1).
const DIRECT: usize = 2;
static DIRECT_LIVE: [[AtomicU64; 2]; DIRECT] = [const { [const { AtomicU64::new(0) }; 2] }; DIRECT];
static DIRECT_TOTAL: [[AtomicU64; 2]; DIRECT] = [const { [const { AtomicU64::new(0) }; 2] }; DIRECT];

fn direct_slot(window: bool) -> usize {
    usize::from(!window)
}

/// A copy was placed directly over title memory: count it while it lives.
pub(crate) fn direct_add(window: bool, bytes: u64) {
    let slot = &DIRECT_LIVE[direct_slot(window)];
    slot[0].fetch_add(1, Ordering::Relaxed);
    slot[1].fetch_add(bytes, Ordering::Relaxed);
    let slot = &DIRECT_TOTAL[direct_slot(window)];
    slot[0].fetch_add(1, Ordering::Relaxed);
    slot[1].fetch_add(bytes, Ordering::Relaxed);
}

/// A directly backed copy went.
pub(crate) fn direct_freed(window: bool, bytes: u64) {
    let slot = &DIRECT_LIVE[direct_slot(window)];
    slot[0].fetch_sub(1, Ordering::Relaxed);
    slot[1].fetch_sub(bytes, Ordering::Relaxed);
}

/// Bytes of copies alive now that the GPU reaches through the title's own backing.
pub(crate) fn direct_live_bytes() -> u64 {
    DIRECT_LIVE.iter().map(|slot| slot[1].load(Ordering::Relaxed)).sum()
}

/// Why a copy that asked for direct backing did not get it (the copy then went the old way).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NoDirect {
    /// The title memory is not openable: not private memory committed whole, or view memory
    /// that is not one contiguous run of one section (scattered, holes, unmapped).
    NotOpenable,
    /// The in-place byte cap (`VICTORY_INPLACE_MB`) or a region's own cap refused it.
    Cap,
    /// The device or driver refused the opening or the placed resource.
    HostRefused,
    /// No host device, or no `ID3D12Device13` for the part-opening call.
    HostUnavailable,
}

const NO_DIRECT: usize = 4;

impl NoDirect {
    fn index(self) -> usize {
        match self {
            NoDirect::NotOpenable => 0,
            NoDirect::Cap => 1,
            NoDirect::HostRefused => 2,
            NoDirect::HostUnavailable => 3,
        }
    }

    fn name(self) -> &'static str {
        match self {
            NoDirect::NotOpenable => "not openable",
            NoDirect::Cap => "cap",
            NoDirect::HostRefused => "host refused",
            NoDirect::HostUnavailable => "host unavailable",
        }
    }
}

/// Direct-backing asks refused since the start, as (count, bytes), by reason.
static NO_DIRECT_TOTAL: [[AtomicU64; 2]; NO_DIRECT] = [const { [const { AtomicU64::new(0) }; 2] }; NO_DIRECT];

/// A copy asked for direct backing and was refused: count it and its bytes with the reason.
pub(crate) fn direct_refused(bytes: u64, why: NoDirect) {
    let slot = &NO_DIRECT_TOTAL[why.index()];
    slot[0].fetch_add(1, Ordering::Relaxed);
    slot[1].fetch_add(bytes, Ordering::Relaxed);
}

/// A copy without a device-local side was made: count it while it lives.
pub(crate) fn fallback_add(bytes: u64, why: Fallback) {
    if bytes == 0 {
        return;
    }
    let i = why.index();
    FALLBACK_LIVE[i][0].fetch_add(1, Ordering::Relaxed);
    FALLBACK_LIVE[i][1].fetch_add(bytes, Ordering::Relaxed);
    FALLBACK_TOTAL[i][0].fetch_add(1, Ordering::Relaxed);
    FALLBACK_TOTAL[i][1].fetch_add(bytes, Ordering::Relaxed);
}

/// Bytes of copies alive now whose GPU-visible side is system memory.
pub(crate) fn fallback_live_bytes() -> u64 {
    let mut bytes = 0u64;
    for reason in [
        Fallback::SwitchOff,
        Fallback::NotEligible,
        Fallback::HostUnavailable,
        Fallback::Budget,
        Fallback::Cap,
        Fallback::AllocationFailed,
        Fallback::ViewCopy,
    ] {
        bytes += FALLBACK_LIVE[reason.index()][1].load(Ordering::Relaxed);
    }
    bytes
}

/// Such a copy went.
pub(crate) fn fallback_freed(bytes: u64, why: Fallback) {
    if bytes == 0 {
        return;
    }
    let i = why.index();
    FALLBACK_LIVE[i][0].fetch_sub(1, Ordering::Relaxed);
    FALLBACK_LIVE[i][1].fetch_sub(bytes, Ordering::Relaxed);
}

/// The MB figures the placement line prints for fallback bytes. Split out so a
/// unit slip (the line once printed raw bytes as MB) has a test.
fn fallback_mib(bytes: u64) -> u64 {
    bytes >> 20
}

/// The placement line: what the API reported for the host buffers made since
/// the start, the copies still living in system memory because no device-local
/// side was available and why, and the system's DXGI groups to compare with.
/// Logged every 5 seconds while `VICTORY_MEMORY_REPORT=1` is set or a
/// fallback copy is alive.
pub(crate) fn placement_line(memory: Option<[[u64; 4]; 2]>) {
    /// Most useful first.
    const ORDER: [Fallback; FALLBACKS] = [
        Fallback::Budget,
        Fallback::Cap,
        Fallback::AllocationFailed,
        Fallback::HostUnavailable,
        Fallback::NotEligible,
        Fallback::SwitchOff,
        Fallback::ViewCopy,
    ];
    // `VICTORY_MEMORY_REPORT=0` silences the line; =1 forces it when no copy is
    // in system memory; unset prints it only while such a copy is alive.
    let asked = std::env::var_os("VICTORY_MEMORY_REPORT");
    if asked.as_deref().is_some_and(|v| v == "0") {
        return;
    }
    let forced = asked.is_some();
    let live_bytes: u64 = ORDER.iter().map(|r| FALLBACK_LIVE[r.index()][1].load(Ordering::Relaxed)).sum();
    // Also while copies are directly backed: the line is then the measurement of how much of the
    // copy machinery the direct path replaced (VICTORY_INPLACE / VICTORY_PORT_CONSTANTS runs).
    let live_direct = direct_live_bytes();
    if !forced && live_bytes == 0 && live_direct == 0 {
        return;
    }
    let (mut live_count, mut total_count, mut total_bytes) = (0u64, 0u64, 0u64);
    let mut reasons = String::new();
    for reason in ORDER {
        let i = reason.index();
        let n = FALLBACK_LIVE[i][0].load(Ordering::Relaxed);
        let b = FALLBACK_LIVE[i][1].load(Ordering::Relaxed);
        live_count += n;
        total_count += FALLBACK_TOTAL[i][0].load(Ordering::Relaxed);
        total_bytes += FALLBACK_TOTAL[i][1].load(Ordering::Relaxed);
        if n != 0 {
            reasons.push_str(&format!("{} {} MB, ", reason.name(), b >> 20));
        }
    }
    if reasons.ends_with(", ") {
        reasons.truncate(reasons.len() - 2);
    }
    let made = |class: HeapClass| {
        let [count, bytes] = &MADE_BY_CLASS[class.index()];
        (count.load(Ordering::Relaxed), bytes.load(Ordering::Relaxed) >> 20)
    };
    let (dl_n, dl_mb) = made(HeapClass::DeviceLocal);
    let (cv_n, cv_mb) = made(HeapClass::CpuVisible);
    let (up_n, up_mb) = made(HeapClass::Staging);
    let (rb_n, rb_mb) = made(HeapClass::Readback);
    let (mv_n, mv_mb) = made(HeapClass::MappedVram);
    let (ot_n, ot_mb) = made(HeapClass::Other);
    let (local_used, local_budget, shared_used, shared_budget) = match memory {
        Some([[budget, used, ..], [shared_budget, shared_used, ..]]) => {
            (used >> 20, budget >> 20, shared_used >> 20, shared_budget >> 20)
        }
        None => (0, 0, 0, 0),
    };
    let (live_mb, total_mb) = (fallback_mib(live_bytes), fallback_mib(total_bytes));
    let detail = if reasons.is_empty() { String::new() } else { format!(": {reasons}") };
    // The copies the GPU reads and writes through the title's own backing: not host buffers at
    // all, so they are counted apart from the API heap classes above.
    let direct = |window: bool| {
        let i = direct_slot(window);
        (
            DIRECT_LIVE[i][0].load(Ordering::Relaxed),
            DIRECT_LIVE[i][1].load(Ordering::Relaxed),
            DIRECT_TOTAL[i][0].load(Ordering::Relaxed),
            DIRECT_TOTAL[i][1].load(Ordering::Relaxed),
        )
    };
    let (dw_n, dw_b, dw_tn, dw_tb) = direct(true);
    let (db_n, db_b, db_tn, db_tb) = direct(false);
    let (direct_live_count, direct_live) = (dw_n + db_n, live_direct);
    let (direct_total_count, direct_total_bytes) = (dw_tn + db_tn, dw_tb + db_tb);
    let mut refused = String::new();
    for why in [NoDirect::NotOpenable, NoDirect::Cap, NoDirect::HostRefused, NoDirect::HostUnavailable] {
        let n = NO_DIRECT_TOTAL[why.index()][0].load(Ordering::Relaxed);
        if n != 0 {
            let bytes = NO_DIRECT_TOTAL[why.index()][1].load(Ordering::Relaxed);
            refused.push_str(&format!("{} {} MB, ", why.name(), bytes >> 20));
        }
    }
    if refused.ends_with(", ") {
        refused.truncate(refused.len() - 2);
    }
    let direct_detail = format!(
        "; directly backed copies (title memory in place, no host copy): alive {direct_live_count} windows {dw_n} {} MB, \
         title buffers {db_n} {} MB, {} MB in all; {direct_total_count} made since the start, {} MB{}",
        dw_b >> 20,
        db_b >> 20,
        direct_live >> 20,
        direct_total_bytes >> 20,
        if refused.is_empty() { String::new() } else { format!("; no direct backing: {refused}") }
    );
    tracing::info!(
        "memory placement: host buffers the copy and transfer machinery made since the start, as the API reports them: \
         device-local {dl_n} ({dl_mb} MB), CPU-visible {cv_n} ({cv_mb} MB), staging {up_n} ({up_mb} MB), \
         readback {rb_n} ({rb_mb} MB), CPU-mapped VRAM {mv_n} ({mv_mb} MB), other {ot_n} ({ot_mb} MB); copies whose \
         GPU-visible side is system memory, alive {live_count} ({live_mb} MB){detail}, {total_count} ({total_mb} MB) \
         made since the start{direct_detail}; DXGI local {local_used}/{local_budget} MB, shared {shared_used}/{shared_budget} MB"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heap_classes_come_from_the_heap_the_api_reports() {
        use windows::Win32::Graphics::Direct3D12::*;
        let of = |props: D3D12_HEAP_PROPERTIES| class_of_properties(&props);
        assert_eq!(
            of(D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            }),
            HeapClass::DeviceLocal
        );
        assert_eq!(
            of(D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_UPLOAD,
                ..Default::default()
            }),
            HeapClass::Staging
        );
        assert_eq!(
            of(D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_READBACK,
                ..Default::default()
            }),
            HeapClass::Readback
        );
        assert_eq!(
            of(D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_GPU_UPLOAD,
                ..Default::default()
            }),
            HeapClass::MappedVram
        );
        assert_eq!(
            of(D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_CUSTOM,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
                MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                ..Default::default()
            }),
            HeapClass::CpuVisible
        );
        // A custom heap that is not the CPU-visible write-back one is not classified
        // as system memory the shaders reach.
        assert_eq!(
            of(D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_CUSTOM,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_NOT_AVAILABLE,
                MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                ..Default::default()
            }),
            HeapClass::Other
        );
    }

    #[test]
    fn fallback_bytes_are_printed_in_mib() {
        assert_eq!(fallback_mib(0), 0);
        assert_eq!(fallback_mib((1 << 20) - 1), 0);
        assert_eq!(fallback_mib(1 << 20), 1);
        assert_eq!(fallback_mib(4 << 20), 4);
        assert_eq!(fallback_mib(3 << 30), 3 * 1024);
    }

    #[test]
    fn a_fallback_copy_is_counted_while_it_lives_and_after() {
        let live = || FALLBACK_LIVE[Fallback::Cap.index()][1].load(Ordering::Relaxed);
        let made = || FALLBACK_TOTAL[Fallback::Cap.index()][1].load(Ordering::Relaxed);
        let (live_before, made_before) = (live(), made());
        fallback_add(1 << 20, Fallback::Cap);
        assert_eq!(live(), live_before + (1 << 20));
        assert_eq!(made(), made_before + (1 << 20));
        fallback_freed(1 << 20, Fallback::Cap);
        assert_eq!(live(), live_before);
        assert_eq!(made(), made_before + (1 << 20), "what was made stays counted");
    }

    #[test]
    fn a_directly_backed_copy_is_counted_while_it_lives_and_after() {
        // The counters are process-wide: other tests may count too, so compare deltas, on the
        // window slot no other test places into.
        let live = || (DIRECT_LIVE[direct_slot(true)][0].load(Ordering::Relaxed), DIRECT_LIVE[direct_slot(true)][1].load(Ordering::Relaxed));
        let made = || (DIRECT_TOTAL[direct_slot(true)][0].load(Ordering::Relaxed), DIRECT_TOTAL[direct_slot(true)][1].load(Ordering::Relaxed));
        let (live_before, made_before) = (live(), made());
        direct_add(true, 3 << 20);
        assert_eq!(live(), (live_before.0 + 1, live_before.1 + (3 << 20)));
        assert_eq!(made(), (made_before.0 + 1, made_before.1 + (3 << 20)));
        direct_freed(true, 3 << 20);
        assert_eq!(live(), live_before);
        assert_eq!(made(), (made_before.0 + 1, made_before.1 + (3 << 20)), "what was made stays counted");
    }

    #[test]
    fn direct_refusals_keep_their_reason_and_bytes() {
        let slot = |why: NoDirect| NO_DIRECT_TOTAL[why.index()][1].load(Ordering::Relaxed);
        let before = (slot(NoDirect::NotOpenable), slot(NoDirect::Cap), slot(NoDirect::HostUnavailable));
        direct_refused(2 << 20, NoDirect::NotOpenable);
        direct_refused(5 << 20, NoDirect::HostUnavailable);
        assert_eq!(slot(NoDirect::NotOpenable), before.0 + (2 << 20));
        assert_eq!(slot(NoDirect::Cap), before.1);
        assert_eq!(slot(NoDirect::HostUnavailable), before.2 + (5 << 20));
    }

    #[test]
    fn bake_and_lookahead_leave_room_for_live_game_allocations() {
        for (commit, bake, lookahead, growth) in [
            (128 * MIB, false, false, false),
            (256 * MIB, true, false, false),
            (512 * MIB, true, true, false),
            (768 * MIB, true, true, false),
            (1024 * MIB, true, true, true),
        ] {
            assert_eq!(has_upload_reserve(commit, true), bake);
            assert_eq!(has_upload_reserve(commit, false), lookahead);
            assert_eq!(has_growth_reserve(commit, false), growth);
        }
    }

    #[test]
    fn either_constraint_enters_pressure_and_both_must_recover() {
        assert!(next_pressure(false, 12 * GIB, 4 * GIB));
        assert!(next_pressure(false, 3 * GIB, 16 * GIB));
        assert!(next_pressure(true, 4 * GIB, 7 * GIB));
        assert!(next_pressure(true, 6 * GIB, 5 * GIB));
        assert!(!next_pressure(true, 5 * GIB, 6 * GIB));
        assert!(!next_pressure(false, 4 * GIB, 5 * GIB));
    }

    #[test]
    fn prefetch_respects_existing_budgets_and_resumes_after_recovery() {
        assert_eq!(prefetch_limit(512 << 20, false, false, true, GIB), 64 << 20);
        assert_eq!(prefetch_limit(512 << 20, false, false, true, GIB - 1), 512 << 20);
        assert_eq!(prefetch_limit(512 << 20, false, false, false, 2 * GIB), 512 << 20);
        assert_eq!(prefetch_limit(0, true, true, false, 0), 0);
        assert_eq!(prefetch_limit(8 << 20, true, true, false, 0), 8 << 20);
        assert_eq!(prefetch_limit(512 << 20, true, false, false, 0), 64 << 20);
        assert_eq!(prefetch_limit(512 << 20, true, true, false, 0), 16 << 20);
        assert_eq!(prefetch_limit(192 << 20, false, false, false, 0), 192 << 20);
    }
}