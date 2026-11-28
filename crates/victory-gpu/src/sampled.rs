//! `VICTORY_SAMPLED_MARKS=1` (off by default): which view heap slots
//! shaders read textures through. Translated shaders set a slot's bit in a
//! buffer of ours (`victory_shader::emit::SLOT_SAMPLED`) when they read or
//! sample a texture picked from the heap; once a frame the bits are taken
//! into the frame each slot was last marked in. Slot eviction cannot tell
//! otherwise whether a texture picked by index is used
//! (`crate::resource::slot_evict`); for now this only counts how many such
//! textures it could let go, nothing changes by it.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use victory_shader::emit::SAMPLED_MARKS;
use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

struct Marks {
    _buffer: ID3D12Resource,
    /// The buffer's mapping: [`SAMPLED_MARKS`] dwords in the PC's memory.
    data: *mut u32,
    gpu: u64,
    /// By slot: the frame (low 32 bits, at least 1) it was last marked in.
    seen: Box<[AtomicU32]>,
}

// SAFETY: the mapping lives as long as the buffer, which lives as long as
// the process; its dwords are only touched atomically or by volatile reads.
unsafe impl Send for Marks {}
// SAFETY: as above.
unsafe impl Sync for Marks {}

static MARKS: std::sync::OnceLock<Option<Marks>> = std::sync::OnceLock::new();
/// The frame whose marks were last taken.
static TAKEN: AtomicU64 = AtomicU64::new(u64::MAX);

/// Parts taken ([`PARTS`] a sweep), slots marked so far in this sweep, and
/// the time of the last line.
static COUNTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Of slot eviction's last pass: textures it may not take as a slot
/// shaders pick by index shows them, of them those no shader marked within
/// the idle time, and their bytes.
pub(crate) static COLD: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

fn marks() -> Option<&'static Marks> {
    MARKS
        .get_or_init(|| {
            if !victory_shader::emit::sampled_marks_on() {
                return None;
            }
            let host = crate::host::host()?;
            let bytes = u64::from(SAMPLED_MARKS) * 4;
            // In the card's memory where the CPU can map it: a shader reads
            // a mark at every texture read, and in the PC's memory each of
            // those crossed the bus (GPU time 3 ms a frame went to 200 ms).
            let card = host.mapped_card_buffer(bytes, true);
            let in_card = card.is_some();
            let (buffer, data) = match card {
                Some(made) => made,
                None => host.shared_buffer(bytes)?,
            };
            crate::host::name(&buffer, || "texture slots shaders sampled".into());
            // SAFETY: a fresh mapping of at least that many bytes; a live buffer.
            let gpu = unsafe {
                std::ptr::write_bytes(data, 0, bytes as usize);
                buffer.GetGPUVirtualAddress()
            };
            tracing::info!(
                in_card,
                "texture slots shaders sample are marked (VICTORY_SAMPLED_MARKS=1){}",
                if in_card { "" } else { "; no card memory the CPU maps here, so in the PC's: slow" }
            );
            Some(Marks {
                _buffer: buffer,
                data: data.cast(),
                gpu,
                seen: (0..SAMPLED_MARKS as usize * 32).map(|_| AtomicU32::new(0)).collect(),
            })
        })
        .as_ref()
}

/// The marks' GPU address, for a draw whose shaders mark (`u<SLOT_SAMPLED>`);
/// the first call of a frame takes the marks of the frames before.
pub(crate) fn gpu() -> Option<u64> {
    let m = marks()?;
    let frame = crate::frame::current();
    if TAKEN.swap(frame, Relaxed) != frame {
        take(m, frame);
    }
    Some(m.gpu)
}

/// The marks are taken a [`PARTS`]th at a time, one part a frame: reading
/// all 512 KiB of card memory from the CPU took 20 ms (RTX 5070 Laptop).
/// A bit waits up to that many frames, far less than slot eviction's idle
/// time.
const PARTS: usize = 32;

/// Moves the bits set in this frame's part since it was last taken into
/// `seen` (as of this frame) and clears them.
fn take(m: &Marks, frame: u64) {
    let stamp = (frame as u32).max(1);
    let mut n = 0u64;
    let mut chunk = [0u32; 4];
    let part = SAMPLED_MARKS as usize / PARTS;
    let from = (COUNTS[0].load(Relaxed) as usize % PARTS) * part;
    for w in (from..from + part).step_by(4) {
        // SAFETY: inside the mapping (above), 16-byte aligned (a buffer
        // starts on 64 KiB); the card may be writing it.
        unsafe { read16(m.data.add(w), &mut chunk) };
        for (k, &word) in chunk.iter().enumerate() {
            if word == 0 {
                continue;
            }
            // SAFETY: as above. A bit the card sets between the read and
            // this store is lost until it marks the slot again (at its
            // next read of the texture, at most a frame later).
            unsafe { m.data.add(w + k).write_volatile(0) };
            let mut bits = word;
            n += u64::from(bits.count_ones());
            while bits != 0 {
                m.seen[(w + k) * 32 + bits.trailing_zeros() as usize].store(stamp, Relaxed);
                bits &= bits - 1;
            }
        }
    }
    let takes = COUNTS[0].fetch_add(1, Relaxed) + 1;
    let sweep = COUNTS[1].fetch_add(n, Relaxed) + n;
    if !(takes as usize).is_multiple_of(PARTS) {
        return;
    }
    // A sweep over every part is over.
    COUNTS[1].store(0, Relaxed);
    let now = crate::host::now_ms();
    if now >= COUNTS[2].load(Relaxed) + 5000 {
        COUNTS[2].store(now, Relaxed);
        let [blocked, cold, bytes] = COLD.each_ref().map(|c| c.load(Relaxed));
        tracing::info!(
            "texture slots shaders sampled (VICTORY_SAMPLED_MARKS=1): {sweep} marked over the last {PARTS} frames; \
             slot eviction's last pass: of {blocked} textures it may not take as a slot shaders pick by index shows them, \
             {cold} were marked by no shader within its idle time ({} MB)",
            bytes >> 20
        );
    }
}

/// Reads 16 bytes at `at` into `out`: by a streaming load where the CPU has
/// one, which reads card memory a line at a time instead of a few bytes
/// per trip across the bus.
///
/// # Safety
/// `at` is 16-byte aligned with 16 readable bytes.
unsafe fn read16(at: *const u32, out: &mut [u32; 4]) {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("sse4.1") {
        // SAFETY: the caller's; SSE4.1 is there.
        unsafe { read16_stream(at, out) };
        return;
    }
    for (k, o) in out.iter_mut().enumerate() {
        // SAFETY: the caller's.
        *o = unsafe { at.add(k).read_volatile() };
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.1")]
unsafe fn read16_stream(at: *const u32, out: &mut [u32; 4]) {
    use std::arch::x86_64::{_mm_storeu_si128, _mm_stream_load_si128};
    // SAFETY: the caller's.
    unsafe { _mm_storeu_si128(out.as_mut_ptr().cast(), _mm_stream_load_si128(at.cast())) };
}

/// Whether a shader marked slot `index` in frame `cutoff` or later; `None`
/// without marks.
pub(crate) fn sampled_since(index: u32, cutoff: u64) -> Option<bool> {
    let m = MARKS.get()?.as_ref()?;
    Some(seen_since(m.seen.get(index as usize)?.load(Relaxed), cutoff))
}

/// Whether a `seen` stamp is of frame `cutoff` or later (both by their low
/// 32 bits; 0: never marked).
fn seen_since(stamp: u32, cutoff: u64) -> bool {
    stamp != 0 && stamp.wrapping_sub(cutoff as u32) < 1 << 31
}

#[cfg(test)]
mod tests {
    /// By hand on a card: whether the marks can lie in card memory the CPU
    /// maps, and what reading them once a frame costs
    /// (`cargo test -p victory-gpu marks_read_cost -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn marks_read_cost() {
        let Some(host) = crate::host::host() else {
            return;
        };
        let bytes = u64::from(super::SAMPLED_MARKS) * 4;
        let card = host.mapped_card_buffer(bytes, true);
        println!("card memory the CPU maps: {}", card.is_some());
        let Some((_buffer, data)) = card.or_else(|| host.shared_buffer(bytes)) else {
            return;
        };
        let data = data.cast::<u32>();
        let mut chunk = [0u32; 4];
        let mut sum = 0u64;
        let t = std::time::Instant::now();
        for _ in 0..10 {
            for w in (0..super::SAMPLED_MARKS as usize).step_by(4) {
                // SAFETY: inside the mapping, aligned.
                unsafe { super::read16(data.add(w), &mut chunk) };
                sum += u64::from(chunk[0] | chunk[3]);
            }
        }
        println!("reading {} KiB: {:.2} ms ({sum})", bytes >> 10, t.elapsed().as_secs_f64() * 100.0);
    }

    #[test]
    fn a_stamp_counts_from_the_cutoff_on() {
        use super::seen_since;
        assert!(!seen_since(0, 0));
        assert!(seen_since(10, 10));
        assert!(seen_since(11, 10));
        assert!(!seen_since(9, 10));
        // Across the wrap of the low 32 bits.
        assert!(seen_since(3, u64::from(u32::MAX) - 2));
        assert!(!seen_since(u32::MAX - 3, (1u64 << 32) + 2));
    }
}
