//! Lab (2026-11-27): switches read from a file while the title runs, counts
//! for the frame-rate block, and upload and shared buffers kept for use
//! again when their last holder lets them go.
//!
//! Run 32 (prison room) had about 85 allocations made and 85 destroyed per
//! frame: `MakeResident` was waited for 173,000 times in 200 seconds (77.6 s
//! in all), and the thread that drops what was let go stood in
//! `NtGdiDdDDIDestroyAllocation2` 80 % of its time, 42 ms a frame.

use std::collections::{BTreeMap, VecDeque};
use std::panic::Location;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard, Once};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::{D3D12_RESOURCE_DIMENSION_BUFFER, ID3D12Resource};
use windows::core::GUID;

/// Buffers nothing holds any more are used again (0: destroyed, as before).
pub(crate) const REUSE: usize = 0;
/// A buffer used again is zeroed first, as a new one is.
pub(crate) const ZERO: usize = 1;
/// Free for whatever a build tries.
pub(crate) const A: usize = 2;
pub(crate) const B: usize = 3;
pub(crate) const C: usize = 4;
pub(crate) const D: usize = 5;

/// The switches' names in the file and what they are without it.
const KNOBS: [(&str, i64); 6] = [("reuse", 1), ("zero", 1), ("a", 0), ("b", 0), ("c", 0), ("d", 0)];
static VALUES: [AtomicI64; 6] = [const { AtomicI64::new(0) }; 6];

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// A switch as it is now. `VICTORY_LAB_FILE=<file>`: lines `name=value`,
/// read again ten times a second; a name the file does not give has its
/// default. Every change is logged, so the frame-rate block's lines can be
/// told apart by what was on.
pub(crate) fn knob(which: usize) -> i64 {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        for (value, (_, default)) in VALUES.iter().zip(KNOBS) {
            value.store(default, Relaxed);
        }
        let Some(path) = std::env::var_os("VICTORY_LAB_FILE") else {
            return;
        };
        let mut last = String::new();
        read(&path, &mut last);
        let _ = std::thread::Builder::new().name("victory-lab".into()).spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(100));
                read(&path, &mut last);
            }
        });
    });
    VALUES[which].load(Relaxed)
}

fn read(path: &std::ffi::OsString, last: &mut String) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    // (A file caught half written has no end of line yet.)
    if text == *last || !text.ends_with('\n') {
        return;
    }
    for (i, (name, default)) in KNOBS.iter().enumerate() {
        let value = text
            .lines()
            .filter_map(|line| line.split_once('='))
            .filter(|(n, _)| n.trim() == *name)
            .filter_map(|(_, v)| v.trim().parse::<i64>().ok())
            .last()
            .unwrap_or(*default);
        if VALUES[i].swap(value, Relaxed) != value {
            tracing::info!("lab switch {name}={value}");
        }
    }
    *last = text;
}

/// Memory made since the last frame-rate line, by the place that asked:
/// times, bytes, microseconds, and how many were buffers used again.
static SITES: Mutex<Vec<((&'static str, u32), [u64; 4])>> = Mutex::new(Vec::new());

/// Notes one piece of memory made (or a buffer used `again`) for `site`.
pub(crate) fn made(site: &'static Location<'static>, bytes: u64, began: Instant, again: bool) {
    let us = began.elapsed().as_micros() as u64;
    let key = (site.file(), site.line());
    let mut sites = lock(&SITES);
    let at = match sites.iter().position(|s| s.0 == key) {
        Some(at) => at,
        None => {
            sites.push((key, [0; 4]));
            sites.len() - 1
        }
    };
    let s = &mut sites[at].1;
    s[0] += 1;
    s[1] += bytes;
    s[2] += us;
    s[3] += u64::from(again);
}

/// What a buffer of ours says of itself: its kind (0 in the upload heap, 1
/// shared with the processor) and the size it was asked with.
const TAG: GUID = GUID::from_u128(0x2b7d5c1e_93a4_4f60_8d2b_51c0a7e4f9d3);

/// A buffer nothing holds, and since when.
struct Free(ID3D12Resource, Instant);
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Free {}

/// The buffers kept, by kind and size, the one let go last at the back.
static FREE: [Mutex<BTreeMap<u64, VecDeque<Free>>>; 2] = [const { Mutex::new(BTreeMap::new()) }; 2];
/// Kept now: how many, their bytes.
static HELD: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// Since the last frame-rate line: taken again, kept at their last holder's
/// letting go, destroyed after resting.
static POOL: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
/// Since the last frame-rate line, last references dropped (the object
/// destroyed) and their nanoseconds, by what it was: an upload buffer, a
/// shared one, another buffer, a texture.
static LAST: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
/// At most this many bytes are kept (they are the PC's memory).
const MOST: u64 = 1 << 30;

/// Marks a new buffer of `kind` as one that may be kept ([`let_go`]).
pub(crate) fn tag(buffer: &ID3D12Resource, kind: u64, size: u64) {
    let tag = [kind, size];
    // SAFETY: 16 bytes, which the object copies.
    let _ = unsafe { buffer.SetPrivateData(&TAG, 16, Some(tag.as_ptr().cast())) };
}

fn tagged(buffer: &ID3D12Resource) -> Option<(usize, u64)> {
    let (mut tag, mut len) = ([0u64; 2], 16u32);
    // SAFETY: room for the 16 bytes asked for.
    unsafe { buffer.GetPrivateData(&TAG, &mut len, Some(tag.as_mut_ptr().cast())) }.ok()?;
    (len == 16 && tag[0] < 2).then_some((tag[0] as usize, tag[1]))
}

/// A buffer of `kind` and exactly `size` bytes that nothing holds, mapped
/// (and zeroed, as a new one is): the one let go last.
pub(crate) fn take(kind: usize, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
    if knob(REUSE) == 0 {
        return None;
    }
    let Free(buffer, _) = lock(&FREE[kind]).get_mut(&size)?.pop_back()?;
    HELD[0].fetch_sub(1, Relaxed);
    HELD[1].fetch_sub(size, Relaxed);
    let mut data = std::ptr::null_mut();
    // SAFETY: a live buffer of ours in a heap the processor maps; mapping it
    // again gives its address.
    if unsafe { buffer.Map(0, None, Some(&mut data)) }.is_err() || data.is_null() {
        return None;
    }
    if knob(ZERO) != 0 {
        // SAFETY: the buffer's mapping, `size` bytes of it; nothing else
        // holds the buffer.
        unsafe { std::ptr::write_bytes(data.cast::<u8>(), 0, size as usize) };
    }
    POOL[0].fetch_add(1, Relaxed);
    Some((buffer, data.cast()))
}

/// Drops one reference to `resource`. If it is the last and the resource a
/// buffer of ours ([`tag`]), the buffer is kept for whoever next asks for
/// one of its kind and size ([`take`]) instead of being destroyed: whoever
/// held it last has let it go when it would have been destroyed, no sooner.
pub(crate) fn let_go(resource: ID3D12Resource) {
    if crate::host::references(&resource) != 1 {
        return;
    }
    let class = match tagged(&resource) {
        Some((kind, size)) => {
            if knob(REUSE) != 0 && HELD[1].load(Relaxed) + size <= MOST {
                lock(&FREE[kind]).entry(size).or_default().push_back(Free(resource, Instant::now()));
                HELD[0].fetch_add(1, Relaxed);
                HELD[1].fetch_add(size, Relaxed);
                POOL[1].fetch_add(1, Relaxed);
                return;
            }
            kind
        }
        // SAFETY: a live resource.
        None if unsafe { resource.GetDesc() }.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER => 2,
        None => 3,
    };
    let began = Instant::now();
    drop(resource);
    LAST[2 * class].fetch_add(1, Relaxed);
    LAST[2 * class + 1].fetch_add(began.elapsed().as_nanos() as u64, Relaxed);
}

/// Destroys the buffers kept that nothing asked for in three seconds (all
/// of them with the switch off). Looks four times a second at most.
pub(crate) fn trim() {
    const REST: Duration = Duration::from_secs(3);
    static LOOKED: Mutex<Option<Instant>> = Mutex::new(None);
    let now = Instant::now();
    {
        let mut looked = lock(&LOOKED);
        if looked.is_some_and(|t| now.saturating_duration_since(t) < Duration::from_millis(250)) {
            return;
        }
        *looked = Some(now);
    }
    let off = knob(REUSE) == 0;
    let mut gone = Vec::new();
    for (kind, free) in FREE.iter().enumerate() {
        lock(free).retain(|size, line| {
            while line.front().is_some_and(|f| off || now.saturating_duration_since(f.1) >= REST) {
                if let Some(f) = line.pop_front() {
                    gone.push((kind, *size, f.0));
                }
            }
            !line.is_empty()
        });
    }
    for (kind, size, buffer) in gone {
        HELD[0].fetch_sub(1, Relaxed);
        HELD[1].fetch_sub(size, Relaxed);
        POOL[2].fetch_add(1, Relaxed);
        let began = Instant::now();
        drop(buffer);
        LAST[2 * kind].fetch_add(1, Relaxed);
        LAST[2 * kind + 1].fetch_add(began.elapsed().as_nanos() as u64, Relaxed);
    }
}

/// For the frame-rate line.
pub(crate) fn log(frames: f64) {
    static BEFORE: Mutex<Option<Instant>> = Mutex::new(None);
    let now = Instant::now();
    let ms = lock(&BEFORE).replace(now).map_or(0.0, |t| now.saturating_duration_since(t).as_secs_f64() * 1000.0);
    let frames = frames.max(1.0);
    let switches: Vec<String> = KNOBS.iter().zip(&VALUES).map(|((name, _), v)| format!("{name}={}", v.load(Relaxed))).collect();
    tracing::info!("lab: {frames:.0} frames in {ms:.0} ms, {:.2} ms a frame; switches: {}", ms / frames, switches.join(" "));
    let mut sites = std::mem::take(&mut *lock(&SITES));
    if !sites.is_empty() {
        sites.sort_by_key(|s| std::cmp::Reverse(s.1[2]));
        let (times, us) = sites.iter().fold((0, 0), |(n, us), s| (n + s.1[0], us + s.1[2]));
        let text: Vec<String> = sites
            .iter()
            .take(12)
            .map(|&((file, line), s)| {
                format!(
                    "{}:{line} {:.1} ({:.1} used again) {:.0} KB {:.2} ms",
                    file.rsplit(['/', '\\']).next().unwrap_or(file),
                    s[0] as f64 / frames,
                    s[3] as f64 / frames,
                    s[1] as f64 / 1024.0 / frames,
                    s[2] as f64 / 1000.0 / frames
                )
            })
            .collect();
        tracing::info!(
            "memory made per frame, by the place that asked (lab): {:.1} times, {:.2} ms in all; {}",
            times as f64 / frames,
            us as f64 / 1000.0 / frames,
            text.join("; ")
        );
    }
    let p: Vec<f64> = POOL.iter().map(|p| p.swap(0, Relaxed) as f64 / frames).collect();
    let l: Vec<f64> = LAST.iter().map(|l| l.swap(0, Relaxed) as f64 / frames).collect();
    tracing::info!(
        "buffers kept for use again per frame (lab, reuse): {:.1} taken again, {:.1} kept when let go, {:.1} destroyed after resting; \
         kept now {} ({} MB); last references dropped per frame: {:.1} upload buffers {:.2} ms, {:.1} shared buffers {:.2} ms, \
         {:.1} other buffers {:.2} ms, {:.1} textures {:.2} ms",
        p[0],
        p[1],
        p[2],
        HELD[0].load(Relaxed),
        HELD[1].load(Relaxed) >> 20,
        l[0],
        l[1] / 1e6,
        l[2],
        l[3] / 1e6,
        l[4],
        l[5] / 1e6,
        l[6],
        l[7] / 1e6,
    );
}
