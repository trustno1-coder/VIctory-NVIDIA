//! Host-memory admission for speculative texture work and reusable upload storage.
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

/// What is left to commit, as the layer's decisions count it: what is left
/// under the system's commit limit now, plus what its pagefiles may still
/// grow by ([`commit_growth`]).
fn available_commit(status: &MEMORYSTATUSEX) -> u64 {
    status.ullAvailPageFile.saturating_add(commit_growth(status))
}

/// How much more the system's commit limit can grow: Windows grows a
/// system-managed pagefile (the default) when commit reaches the limit, up to
/// three times the RAM (at least 4 GB) and an eighth of its volume, and a
/// custom one up to its maximum. Counting only the limit of the moment, the
/// layer declared "host memory pressure" with 0.9 GB left to commit while 23
/// GB of RAM was free (RTX 5070 Ti PC, 61.6 GB RAM, a 7.9 GB system-managed
/// pagefile that Windows grew to 13 GB in the same run), and then held
/// texture uploads to 16 MB a frame (textures late) and swept
/// every 1.5 s (stutter). What a pagefile can still grow by is also held to
/// the free space of its volume, less 8 GB. Read from `HKLM\SYSTEM\
/// CurrentControlSet\Control\Session Manager\Memory Management\PagingFiles`;
/// the free space is looked at again every 5 s.
/// `VICTORY_COMMIT_GROWTH=0`: the limit of the moment alone, as before.
fn commit_growth(status: &MEMORYSTATUSEX) -> u64 {
    static OFF: OnceLock<bool> = OnceLock::new();
    if *OFF.get_or_init(|| std::env::var_os("VICTORY_COMMIT_GROWTH").is_some_and(|v| v == "0")) {
        return 0;
    }
    static FILES: OnceLock<Vec<Pagefile>> = OnceLock::new();
    let files = FILES.get_or_init(|| {
        let files = pagefiles();
        let said: Vec<String> = files
            .iter()
            .map(|f| match f.max_mb {
                Some(mb) => format!("{} up to {mb} MB", f.root),
                None => format!("{} system-managed", f.root),
            })
            .collect();
        tracing::info!(
            pagefiles = ?said,
            commit_limit_mib = status.ullTotalPageFile >> 20,
            "pagefiles: commit can grow beyond the limit of the moment (VICTORY_COMMIT_GROWTH=0: not counted)"
        );
        files
    });
    if files.is_empty() {
        return 0;
    }
    static NEXT: AtomicU64 = AtomicU64::new(0);
    static GROWTH: AtomicU64 = AtomicU64::new(0);
    let now = crate::host::coarse_ms();
    let next = NEXT.load(Ordering::Relaxed);
    if now >= next && NEXT.compare_exchange(next, now.saturating_add(5000), Ordering::Relaxed, Ordering::Relaxed).is_ok() {
        // The pagefiles' size now: the commit limit less the RAM (near enough).
        let size_now = status.ullTotalPageFile.saturating_sub(status.ullTotalPhys);
        let mut most = 0u64;
        let mut room = 0u64;
        let mut roots: Vec<&str> = Vec::new();
        for file in files {
            let Some((total, free)) = volume_space(&file.root) else { continue };
            most += match file.max_mb {
                Some(mb) => mb * MIB,
                None => (3 * status.ullTotalPhys).max(4 * GIB).min(total / 8),
            };
            if !roots.contains(&file.root.as_str()) {
                roots.push(&file.root);
                room += free.saturating_sub(8 * GIB);
            }
        }
        GROWTH.store(most.saturating_sub(size_now).min(room), Ordering::Relaxed);
    }
    GROWTH.load(Ordering::Relaxed)
}

/// A pagefile of the system's: its volume (`C:\`) and its largest size in MB
/// (`None`: system-managed).
struct Pagefile {
    root: String,
    max_mb: Option<u64>,
}

/// The system's pagefiles, from the registry (empty if it cannot be read).
fn pagefiles() -> Vec<Pagefile> {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_MULTI_SZ, RegGetValueW};
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let key = wide(r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management");
    let value = wide("PagingFiles");
    let mut bytes = 0u32;
    // SAFETY: a size query, then the read into a buffer of that size.
    let sized = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_MULTI_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut bytes,
        )
    };
    if sized != 0 || bytes == 0 {
        return Vec::new();
    }
    let mut buffer = vec![0u16; (bytes as usize).div_ceil(2)];
    // SAFETY: as above; the buffer holds `bytes` bytes.
    let read = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_MULTI_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if read != 0 {
        return Vec::new();
    }
    let system_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let len = (bytes as usize / 2).min(buffer.len());
    String::from_utf16_lossy(&buffer[..len]).split('\0').filter_map(|entry| parse_pagefile(entry, &system_drive)).collect()
}

/// One `PagingFiles` entry: `<path> [<initial MB> <max MB>]`. No sizes, or a
/// maximum of 0: system-managed; `?:\pagefile.sys`: managed automatically,
/// on the system drive.
fn parse_pagefile(entry: &str, system_drive: &str) -> Option<Pagefile> {
    let mut parts = entry.split_whitespace();
    let path = parts.next()?;
    let drive = path.get(..2).filter(|d| d.ends_with(':'))?;
    let drive = if drive.starts_with('?') { system_drive.get(..2)? } else { drive };
    let sizes: Vec<u64> = parts.filter_map(|p| p.parse().ok()).collect();
    let max_mb = match sizes.as_slice() {
        [_, max] if *max > 0 => Some(*max),
        _ => None,
    };
    Some(Pagefile { root: format!(r"{}\", drive.to_ascii_uppercase()), max_mb })
}

/// A volume's size and its free space, in bytes.
fn volume_space(root: &str) -> Option<(u64, u64)> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = root.encode_utf16().chain([0]).collect();
    let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
    // SAFETY: a path and three out-values of ours.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, &mut total_free) } != 0;
    ok.then_some((total, free))
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
        AVAILABLE_COMMIT.store(available_commit(&status), Ordering::Relaxed);
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
    let (physical, commit) = (status.ullAvailPhys, available_commit(&status));
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
                buffer_mirror_span_mib = crate::shadow::live_span_bytes() >> 20,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pagefile_entries_say_their_volume_and_largest_size() {
        let f = parse_pagefile(r"?:\pagefile.sys", "C:").unwrap();
        assert_eq!((f.root.as_str(), f.max_mb), (r"C:\", None));
        let f = parse_pagefile(r"d:\pagefile.sys 0 0", "C:").unwrap();
        assert_eq!((f.root.as_str(), f.max_mb), (r"D:\", None));
        let f = parse_pagefile(r"C:\pagefile.sys 16384 49152", "C:").unwrap();
        assert_eq!((f.root.as_str(), f.max_mb), (r"C:\", Some(49152)));
        assert!(parse_pagefile("", "C:").is_none());
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
