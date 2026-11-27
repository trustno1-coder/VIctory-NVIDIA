//! A frame-rate estimate without the title (`victory bench`): the per-frame work a run's log
//! counted (bytes refreshed, sent up, landed; askings of the write watch) is run again through
//! this build's own code on memory made as the runtime makes the title's, each step timed, and
//! the steps put together as the threads that carry them: the submitting thread, the relay
//! (landing what shaders wrote) and the card. The slowest of them sets the frame.
//!
//! Measured here (this build's code): each submission's one asking of the write watch
//! (`watch::ask_once`) and its refresh steps as a submission runs them (`host::bench_refreshes`:
//! the batch, the copies known to copy nothing told so as they come, the rest checked and
//! copied on the refreshing threads, into host copies made as the runtime makes them), the
//! sending up of submissions' bytes (`shadow::copy_all`), the landing of what the card
//! wrote (`land::piece`, on as many threads as the run had), and the heaps' records' asking
//! and compare (`watch::Tracked::take`, `safecopy::changed`). Taken from the log as it was:
//! the rest of the submitting thread's work (sending lists, D3D12 calls, other steps), the
//! relay's waits for the card, the records' making, and the card's own time. So the
//! estimate follows changes to the measured steps; others need a new log.
//!
//! # How the threads make a frame
//!
//! The title, the submitting thread, the relay and the card work as a pipeline: each frame
//! waits for the slowest of them, so the estimate is the largest of four times per frame.
//!
//! - **The submitting thread.** The log's "that thread worked" counts the whole of each
//!   submission, its waits for the card included ("stood waiting for the card"). In
//!   card-bound windows (below) those waits are the card's time showing through and are
//!   taken out (they come back through the card's term); elsewhere the few ms are the
//!   pipeline's friction and stay. Then the logged steps this build measures are put in
//!   place of the logged ones.
//! - **The relay**: its landing, with the merging measured.
//! - **The card.** The log has no clock of the card's work, only of its queue: "nothing of
//!   ours queued" is idle; the rest ("busy") is work *or* the card standing at a wait inside
//!   a submission, and in most windows it is about the submitting thread's own time, which
//!   is what a queue fed by a slower thread looks like either way. Busy is therefore an upper
//!   bound. It is the card's time only where the card held the pipeline up: the windows where
//!   the submitting thread stood waiting for the card (5 ms or more a frame; "card-bound"
//!   below). In the others the card's time is estimated from the card-bound windows of the
//!   same run, as a line through their busy time against their draws a frame, and never above
//!   that window's busy. (That line comes from windows where the card was slow, so it leans
//!   high: the estimate does not promise frame rates the card may not reach.) Without three
//!   card-bound windows to draw it through, busy is taken as it is. Our own passes that a
//!   build measures as gone from the card ([`Measured::card_saved_ms`]) come off this term.
//!   The report also gives the frame rate if the card kept up (only the CPU's threads and the
//!   card-bound windows' busy), the most CPU-side work could bring.
//! - **The title**, its own time a frame: frame time less what its threads waited at
//!   presents for us. That includes its waits on fences we set, so per window it is an upper
//!   bound; the floor used is the run's 10th percentile of it.
//!
//! The split of the card's frame printed: busy (of which our water passes, which have GPU
//! timestamps, `host::water_timing`), idle while the submitting thread worked (the card fed
//! late, waiting on us) and idle while the title had not submitted.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Instant;

const PAGE: usize = 4096;
const BLOCK: usize = crate::land::BLOCK;

/// One frame-rate line of a log (5 s) and the lines about the same seconds.
#[derive(Clone, Debug, Default)]
pub struct Window {
    pub at: String,
    pub fps: f64,
    pub frame_ms: f64,
    pub draws: f64,
    /// Frame line: refreshes' copies (ms, MB).
    pub copies_ms: f64,
    pub copies_mb: f64,
    /// The submitting thread: what it worked, and of it the write watch's askings.
    pub submit_ms: f64,
    pub watch_ms: f64,
    /// Of its list steps, the refreshes: ms, how many, how many known to copy nothing.
    pub refresh_ms: f64,
    pub refreshes: f64,
    pub refreshes_nothing: f64,
    /// Submissions per frame (each asks the write watch).
    pub submissions: f64,
    /// Bytes of submissions sent up: MB, runs, ms taking them.
    pub sent_mb: f64,
    pub sent_runs: f64,
    pub sent_ms: f64,
    /// The relay: landing in all, waiting for fetches.
    pub relay_ms: f64,
    pub fetch_wait_ms: f64,
    /// Landing: merging (ms), pieces, pages with a new byte.
    pub merge_ms: f64,
    pub pieces: f64,
    pub new_pages: f64,
    pub land_threads: f64,
    /// The card: time nothing of ours was queued; of that, while the submitting thread
    /// worked (the card fed late); the submitting thread standing waiting for the card.
    pub card_idle_ms: f64,
    pub card_behind_ms: f64,
    pub submit_waited_ms: f64,
    /// The title's threads waiting at presents (for us).
    pub title_waited_ms: f64,
    /// Our water passes on the card (GPU timestamps; `host::water_timing`).
    pub water_ms: f64,
    /// The present call's time a frame on the thread that presents (Streamline's line), and
    /// whether this run already presented on a thread of its own (`VICTORY_PRESENT_THREAD`).
    pub present_ms: f64,
    pub present_apart: bool,
    /// The title's busiest thread's own code a frame, sampled (`VICTORY_CPU_STAGES`, "the
    /// title's floor"); 0 where the log has no such line.
    pub title_own_ms: f64,
    /// Host copies in the card's memory (MB): the title memory the write watch covers.
    pub copies_held_mb: f64,
    /// Our passes on the card per frame: hash passes and their pieces, compare passes and
    /// the pieces compared.
    pub hash_passes: f64,
    pub hashed_pieces: f64,
    pub compare_passes: f64,
    pub compared_pieces: f64,
    /// The heaps' records copied as bytes (`crate::descriptor::sync_heaps`, a thread of its
    /// own): changed heap pages, new records on them, rounds, and its ms in all, asking the
    /// write watch, reading and comparing.
    pub heap_pages: f64,
    pub heap_changed: f64,
    pub heap_rounds: f64,
    pub heap_ms: f64,
    pub heap_ask_ms: f64,
    pub heap_read_ms: f64,
}

/// The number right after `key` in `line` (spaces skipped).
fn after(line: &str, key: &str) -> Option<f64> {
    let rest = &line[line.find(key)? + key.len()..];
    let rest = rest.trim_start();
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == 'e')).unwrap_or(rest.len());
    rest[..end].trim_end_matches('.').parse().ok()
}

/// The number right before `key` in `line`.
fn before(line: &str, key: &str) -> Option<f64> {
    let head = line[..line.find(key)?].trim_end();
    let start = head.rfind(|c: char| !(c.is_ascii_digit() || c == '.')).map_or(0, |i| i + 1);
    head[start..].parse().ok()
}

/// The frame-rate windows of a log with what the lines about the same seconds said. A
/// window's other lines are those printed since the frame-rate line before it.
pub fn windows(log: &str) -> Vec<Window> {
    let mut out = Vec::new();
    let mut w = Window::default();
    for line in log.lines() {
        if line.contains("victory_gpu::host:") && line.contains(" fps, ") && line.contains("ms/frame") {
            let fields = || -> Option<Window> {
                let mut n = w.clone();
                n.at = line.get(..19).unwrap_or("").to_owned();
                n.fps = before(line, " fps,")?;
                n.frame_ms = before(line, " ms/frame")?;
                n.draws = before(line, " draws/frame")?;
                let copies = &line[line.find("copies ")?..];
                n.copies_ms = after(copies, "copies")?;
                n.copies_mb = before(copies, " MB")?;
                Some(n)
            };
            if let Some(n) = fields() {
                out.push(n);
            }
            let held = w.copies_held_mb;
            w = Window { copies_held_mb: held, ..Window::default() };
            continue;
        }
        if line.contains("preparing per frame, the submitting thread:") {
            w.watch_ms = before(line, " asking the write watch").unwrap_or(0.0);
            w.refresh_ms = after(line, "(refreshes").unwrap_or(0.0);
            w.refreshes = after(line, " flushes of").unwrap_or(0.0);
            w.refreshes_nothing = before(line, " of them known to copy nothing").unwrap_or(0.0);
        } else if line.contains("submissions prepared by a thread of ours") {
            w.submissions = after(line, "per frame (VICTORY_ASYNC_SUBMIT):").unwrap_or(0.0);
            w.submit_ms = after(line, "that thread worked").unwrap_or(0.0);
            w.title_waited_ms = before(line, " ms at presents").unwrap_or(0.0);
        } else if line.contains("bytes of submissions' own sent up per frame") {
            w.sent_mb = after(line, "(VICTORY_SEND_OWN):").unwrap_or(0.0);
            w.sent_runs = before(line, " runs").unwrap_or(0.0);
            w.sent_ms = before(line, " ms taking them").unwrap_or(0.0);
        } else if line.contains("the relay per frame") {
            w.relay_ms = before(line, " ms landing in all").unwrap_or(0.0);
            w.fetch_wait_ms = before(line, " ms for fetches").unwrap_or(0.0);
        } else if line.contains("landing per frame (VICTORY_LAND=") {
            w.land_threads = before(line, " threads)").unwrap_or(1.0);
            w.merge_ms = after(line, "merging what was fetched").unwrap_or(0.0);
        } else if line.contains("victory_gpu::land: landed per frame:") {
            w.pieces = after(line, "landed per frame:").unwrap_or(0.0);
            w.new_pages = before(line, " with a new byte (").unwrap_or(0.0);
        } else if line.contains("the card's clock per frame: nothing of ours queued") {
            w.card_idle_ms = after(line, "nothing of ours queued").unwrap_or(0.0);
            w.card_behind_ms = before(line, " ms of that while the submitting thread").unwrap_or(0.0);
            w.submit_waited_ms = after(line, "stood waiting for the card").unwrap_or(0.0);
        } else if line.contains("DLSS Frame Generation, time per frame") {
            w.present_ms = before(line, " ms in it").unwrap_or(0.0);
        } else if line.contains("presents on a thread of their own") {
            w.present_apart = true;
        } else if line.contains("cpu stages, the title's threads per frame") {
            w.title_own_ms = after(line, "the title's floor (its busiest thread's own code)").unwrap_or(0.0);
        } else if line.contains("water light GPU time") {
            w.water_ms += after(line, "ms_per_frame=").unwrap_or(0.0);
        } else if line.contains("host copies in the card's memory:") && line.contains(" copies ") {
            w.copies_held_mb = before(line, " MB, their bytes").unwrap_or(w.copies_held_mb);
            w.compared_pieces = before(line, " pieces compared on the card").unwrap_or(0.0);
        } else if line.contains("our passes over copies in the card's memory per frame:") {
            w.hash_passes = after(line, "memory per frame:").unwrap_or(0.0);
            w.hashed_pieces = before(line, " pieces after").unwrap_or(0.0);
            w.compare_passes = before(line, " compare passes").unwrap_or(0.0);
        } else if line.contains("descriptor records the title copied into its heap as bytes") {
            w.heap_pages = after(line, "not looked for): per frame").unwrap_or(0.0);
            w.heap_changed = before(line, " whose record was new").unwrap_or(0.0)
                + before(line, " new ones of draws' tables").unwrap_or(0.0)
                + before(line, " that waited for a draw first").unwrap_or(0.0);
            w.heap_ms = before(line, " ms (").unwrap_or(0.0);
            w.heap_ask_ms = before(line, " ms asking").unwrap_or(0.0);
            w.heap_read_ms = before(line, " reading and comparing").unwrap_or(0.0);
        } else if line.contains("the heaps' records, rounds per frame:") {
            w.heap_rounds = after(line, "rounds per frame:").unwrap_or(0.0);
        }
    }
    out
}

/// Gameplay windows: drawing a world (many draws) with the submitting thread's lines there.
pub fn gameplay(all: &[Window]) -> Vec<Window> {
    all.iter().filter(|w| w.draws >= 1000.0 && w.submit_ms > 0.0 && w.fps > 0.0).cloned().collect()
}

/// Title memory as the runtime makes it: a view of a pagefile section, noted as it notes it.
/// Kept for the process (the journal of writes knows its addresses).
struct Title {
    at: usize,
    len: usize,
}

impl Title {
    fn new(len: usize) -> Option<Title> {
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE};
        // SAFETY: a section of our own and a view of it.
        unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                (len as u64 >> 32) as u32,
                len as u32,
                std::ptr::null(),
            );
            if section.is_null() {
                return None;
            }
            let at = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, len).Value as usize;
            if at == 0 {
                return None;
            }
            crate::inplace::title_memory_section(section as usize, len as u64, true);
            crate::title_memory_remapped(at, len);
            crate::inplace::title_memory_view(at, len, section as usize, 0);
            // Every page there (as a running title's are).
            std::ptr::write_bytes(at as *mut u8, 1, len);
            Some(Title { at, len })
        }
    }
}

/// Least of `runs` timings of `f`, in ms.
fn best(runs: usize, mut f: impl FnMut()) -> f64 {
    let mut least = f64::MAX;
    for _ in 0..runs {
        let t = Instant::now();
        f();
        least = least.min(t.elapsed().as_secs_f64() * 1e3);
    }
    least
}

/// This build's time for one window's work, per frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct Measured {
    pub watch_ms: f64,
    pub copies_ms: f64,
    pub sent_ms: f64,
    pub merge_ms: f64,
    /// The card's time a frame this build no longer spends against the logged run (our own
    /// passes measured less: 0 when nothing measures them). Comes off the card's term.
    pub card_saved_ms: f64,
    /// The heaps' records: asking the write watch, reading and comparing.
    pub heap_ask_ms: f64,
    pub heap_read_ms: f64,
}

/// A placed heap of descriptors as the records' rounds see it: title memory the write
/// watch tracks (`watch::Tracked`, as `crate::descriptor` keeps one per heap), and the
/// records last seen of it.
struct Heap {
    title: Title,
    tracked: crate::watch::Tracked,
    seen: Vec<u8>,
}

impl Heap {
    fn new(len: usize) -> Option<Heap> {
        let title = Title::new(len)?;
        let tracked = crate::watch::track(title.at, title.len, true)?;
        // (as last seen: what is there now)
        // SAFETY: the view, all of it.
        let seen = unsafe { std::slice::from_raw_parts(title.at as *const u8, title.len) }.to_vec();
        Some(Heap { title, tracked, seen })
    }
}

/// The heaps' records' work of a window, per frame: its rounds each ask the write watch
/// about the heap after the title wrote that round's share of the changed pages, and the
/// changed pages' records are read where they are and compared with those last seen
/// (`safecopy::changed`, as with `VICTORY_SYNC_IN_PLACE=1`) on the records' threads, eight
/// pages a job. (ask ms, read ms); zeros without such work or a safe copy.
fn measure_heap(w: &Window, heap: &mut Heap, next: &mut dyn FnMut() -> usize) -> (f64, f64) {
    const HEAP_PAGE: usize = crate::watch::PAGE;
    const RECORD: usize = 32;
    if w.heap_pages < 1.0 || !crate::safecopy::on() {
        return (0.0, 0.0);
    }
    let heap_pages = heap.title.len / HEAP_PAGE;
    let pages = (w.heap_pages.round() as usize).clamp(1, heap_pages);
    let rounds = (w.heap_rounds.round() as usize).max(1);
    let per_page = ((w.heap_changed / w.heap_pages).round() as usize).clamp(1, HEAP_PAGE / RECORD);
    let (mut ask_ms, mut read_ms) = (f64::MAX, f64::MAX);
    let mut stamp = 0u32;
    for _ in 0..3 {
        stamp += 1;
        let chosen: Vec<usize> = (0..pages).map(|_| next() % heap_pages).collect();
        // The rounds: the title writes a share of the pages' records (not timed), the
        // round asks about all of the heap.
        let mut asking = 0.0;
        for share in chosen.chunks(pages.div_ceil(rounds)) {
            for &page in share {
                for r in 0..per_page {
                    let record = (r * (HEAP_PAGE / RECORD) / per_page) * RECORD;
                    // SAFETY: inside the view.
                    unsafe { std::ptr::write_volatile((heap.title.at + page * HEAP_PAGE + record) as *mut u32, stamp) };
                }
            }
            let t = Instant::now();
            std::hint::black_box(heap.tracked.take(heap.title.at, heap.title.at + heap.title.len));
            asking += t.elapsed().as_secs_f64() * 1e3;
        }
        for _ in share_rest(pages, rounds) {
            let t = Instant::now();
            std::hint::black_box(heap.tracked.take(heap.title.at, heap.title.at + heap.title.len));
            asking += t.elapsed().as_secs_f64() * 1e3;
        }
        ask_ms = ask_ms.min(asking);
        // Reading and comparing the changed pages, eight a job.
        let (title_at, seen_at) = (heap.title.at, heap.seen.as_mut_ptr() as usize);
        let jobs: Vec<&[usize]> = chosen.chunks(8).collect();
        let t = Instant::now();
        crate::land::each_view(true, jobs.len(), &|n| {
            let mut bits = vec![0u64; (HEAP_PAGE / RECORD).div_ceil(64)];
            for &page in jobs[n] {
                bits.iter_mut().for_each(|b| *b = 0);
                // SAFETY: the view's page and the records last seen of it; the jobs' pages
                // are read by one job each at a time (a page twice in a job is read twice).
                unsafe {
                    crate::safecopy::changed(
                        (title_at + page * HEAP_PAGE) as *const u8,
                        (seen_at + page * HEAP_PAGE) as *mut u8,
                        HEAP_PAGE / RECORD,
                        &mut bits,
                    )
                };
                std::hint::black_box(&bits);
            }
        });
        read_ms = read_ms.min(t.elapsed().as_secs_f64() * 1e3);
    }
    (ask_ms, read_ms)
}

/// The rounds past those that found a share of the pages (rounds that found nothing).
fn share_rest(pages: usize, rounds: usize) -> std::ops::Range<usize> {
    let asked = pages.div_ceil(pages.div_ceil(rounds).max(1));
    0..rounds.saturating_sub(asked)
}

/// Host copies of title memory for the refreshes, made once: [`Rig::COPIES`] copies
/// of [`Rig::COPY`] bytes each, side by side from the title memory's start, as the
/// heap's buffer views are (some 4,100 copies a frame refreshes). The first half
/// take the refreshes that copy; the second half the ones known to copy nothing
/// (refreshed whole once, here, and not written by the title since).
struct Rig {
    copies: Vec<std::sync::Arc<crate::shadow::Shadow>>,
}

impl Rig {
    const COPIES: usize = 4096;
    const COPY: usize = 128 << 10;

    fn new(title: &Title) -> Rig {
        let count = Self::COPIES.min(title.len / Self::COPY);
        // The copies' CPU sides: never let go, as the copies are not.
        let data: &'static mut [u8] = Box::leak(vec![0u8; count * Self::COPY].into_boxed_slice());
        let copies: Vec<_> = (0..count)
            .map(|i| {
                let start = (title.at + i * Self::COPY) as u64;
                // SAFETY: inside the leaked block, `COPY` bytes each, apart.
                let at = unsafe { data.as_mut_ptr().add(i * Self::COPY) };
                crate::shadow::Shadow::unbacked_for_bench(start, at, Self::COPY as u64, true)
            })
            .collect();
        // Every copy refreshed whole once (as a running title's are by now).
        let all: Vec<_> = copies.iter().map(|c| (c.clone(), c.span().0 as usize, Self::COPY)).collect();
        let _ = crate::host::bench_refreshes(&[all.clone()]);
        let _ = crate::host::bench_refreshes(&[all]);
        copies.iter().for_each(|c| {
            c.take_card_dirty();
        });
        Rig { copies }
    }
}

/// Runs a window's per-frame work through this build's code.
fn measure(w: &Window, title: &Title, rig: &Rig, held: &mut [u8], card: &mut [u8], heap: Option<&mut Heap>) -> Measured {
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 8) as usize
    };
    // The submissions' refresh steps, through the real code (`host::bench_refreshes`):
    // each submission asks the write watch once for what it refreshes, then its steps go
    // into a batch and are flushed. Of the frame's refresh steps (the log's count), those
    // known to copy nothing are of copies the title did not write; the others are of copies
    // whose ranges it wrote since the last frame, the copies' bytes (the log's MB) between
    // them, a range each.
    let half = (rig.copies.len() / 2).max(1);
    let copying = ((w.refreshes - w.refreshes_nothing).round().max(1.0) as usize).min(half);
    let nothing = w.refreshes_nothing.round() as usize;
    let size = (((w.copies_mb * 1e6) as usize / copying).div_ceil(PAGE).max(1) * PAGE).min(Rig::COPY);
    let writing: Vec<usize> = (0..copying).collect();
    let asks = w.submissions.round().max(1.0) as usize;
    let mut steps: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> = Vec::with_capacity(copying + nothing);
    // (A no-op step for every copying one and more, spread through the submissions.)
    for k in 0..copying + nothing {
        if k < copying {
            let c = &rig.copies[writing[k]];
            steps.push((c.clone(), c.span().0 as usize, size));
        } else {
            let c = &rig.copies[half + (k - copying) % (rig.copies.len() - half).max(1)];
            let off = (next() % (Rig::COPY / PAGE)) * PAGE;
            steps.push((c.clone(), c.span().0 as usize + off, PAGE.min(Rig::COPY - off)));
        }
    }
    // Mixed as a frame's lists mix them.
    for i in (1..steps.len()).rev() {
        steps.swap(i, next() % (i + 1));
    }
    let per = steps.len().div_ceil(asks).max(1);
    let submissions: Vec<Vec<_>> = steps.chunks(per).map(<[_]>::to_vec).collect();
    let (mut watch_ms, mut copies_ms) = (f64::MAX, f64::MAX);
    for round in 0..3u32 {
        // The title's writes (its own cost, not timed): a dword in every KiB of the ranges
        // the copying refreshes take.
        for &i in &writing {
            let at = rig.copies[i].span().0 as usize;
            for p in (0..size).step_by(1024) {
                // SAFETY: inside the view (the copies lie in it).
                unsafe { std::ptr::write_volatile((at + p) as *mut u32, round.wrapping_add(7)) };
            }
        }
        let (asking, refreshing) = crate::host::bench_refreshes(&submissions);
        watch_ms = watch_ms.min(asking);
        copies_ms = copies_ms.min(refreshing);
        // (What went to the card's lists is sent: not timed here, see below.)
        for c in &rig.copies {
            c.take_card_dirty();
        }
    }
    // Sending up: submissions' bytes in their runs, by `copy_all` (its threads included).
    let sent = ((w.sent_mb * 1e6) as usize).min(card.len());
    let runs = w.sent_runs.round().max(1.0) as usize;
    let run = (sent / runs).max(1);
    let sent_ms = best(3, || {
        let copies: Vec<(usize, usize, usize)> = (0..runs)
            .map(|i| {
                let from = title.at + (next() % (title.len - run)) / 64 * 64;
                (from, card.as_mut_ptr() as usize + (i * run) % (card.len() - run), run)
            })
            .collect();
        crate::shadow::copy_all(copies);
    });
    // Landing: the pieces the card wrote, with their pages' new bytes (every dword), merged
    // into title memory and the held copy on the landing's threads.
    let pieces = (w.pieces.round() as usize).min(card.len() / BLOCK).min(title.len / BLOCK);
    let new_share = if w.pieces > 0.0 { w.new_pages / (w.pieces * (BLOCK / PAGE) as f64) } else { 0.0 };
    let threads = (w.land_threads.round() as usize).max(1);
    let blocks = title.len / BLOCK;
    let at: Vec<usize> = (0..pieces).map(|_| next() % blocks).collect();
    let mut round = 0u32;
    let mut merge_ms = f64::MAX;
    for _ in 0..3 {
        round += 1;
        for (k, &b) in at.iter().enumerate() {
            for page in 0..BLOCK / PAGE {
                let from = k * BLOCK + page * PAGE;
                let old = b * BLOCK + page * PAGE;
                card[from..from + PAGE].copy_from_slice(&held[old..old + PAGE]);
                if (page as f64 + 0.5) / (BLOCK / PAGE) as f64 <= new_share {
                    for d in card[from..from + PAGE].chunks_exact_mut(4) {
                        d.copy_from_slice(&round.wrapping_mul(0x0101_0101).to_le_bytes());
                    }
                }
            }
        }
        let work: Vec<(usize, usize, usize)> = at
            .iter()
            .enumerate()
            .map(|(k, &b)| (title.at + b * BLOCK, card.as_ptr() as usize + k * BLOCK, held.as_mut_ptr() as usize + b * BLOCK))
            .collect();
        let t = Instant::now();
        crate::land::bench_land(&work, threads);
        merge_ms = merge_ms.min(t.elapsed().as_secs_f64() * 1e3);
    }
    let (heap_ask_ms, heap_read_ms) = match heap {
        Some(heap) => measure_heap(w, heap, &mut next),
        None => (0.0, 0.0),
    };
    Measured { watch_ms, copies_ms, sent_ms, merge_ms, card_saved_ms: 0.0, heap_ask_ms, heap_read_ms }
}

/// What the card's term of a run is drawn from ([`card_model`]): a line through the
/// card-bound windows' busy time against their draws, and the title's floor.
#[derive(Clone, Copy, Debug, Default)]
pub struct CardModel {
    /// Busy ms = `.0` + `.1` × draws, if three card-bound windows or more drew it.
    pub line: Option<(f64, f64)>,
    pub card_bound_windows: usize,
    /// The title's own time a frame: the run's 10th percentile of frame less its waits at
    /// presents.
    pub title_floor_ms: f64,
}

/// The submitting thread stood waiting for the card this long a frame or more: the card
/// held the pipeline up, and its busy time is its own.
const CARD_BOUND_MS: f64 = 5.0;

fn busy(w: &Window) -> f64 {
    (w.frame_ms - w.card_idle_ms).max(0.0)
}

fn card_bound(w: &Window) -> bool {
    w.submit_waited_ms >= CARD_BOUND_MS
}

/// [`CardModel`] of a run's gameplay windows.
pub fn card_model(play: &[Window]) -> CardModel {
    let points: Vec<(f64, f64)> = play.iter().filter(|w| card_bound(w)).map(|w| (w.draws, busy(w))).collect();
    let line = (points.len() >= 3).then(|| {
        let n = points.len() as f64;
        let mx = points.iter().map(|p| p.0).sum::<f64>() / n;
        let my = points.iter().map(|p| p.1).sum::<f64>() / n;
        let sxx: f64 = points.iter().map(|p| (p.0 - mx).powi(2)).sum();
        let sxy: f64 = points.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
        // (more draws never make less card work: a falling line is taken flat)
        let b = if sxx > 0.0 { (sxy / sxx).max(0.0) } else { 0.0 };
        (my - b * mx, b)
    });
    let mut own: Vec<f64> = play.iter().map(|w| (w.frame_ms - w.title_waited_ms).max(0.0)).collect();
    own.sort_by(f64::total_cmp);
    let title_floor_ms = own.get(own.len() / 10).copied().unwrap_or(0.0);
    CardModel { line, card_bound_windows: points.len(), title_floor_ms }
}

/// A window's frame put together: the threads' times per frame (ms).
#[derive(Clone, Copy, Debug, Default)]
pub struct Estimate {
    /// The submitting thread's own work (its waits for the card out).
    pub submit_ms: f64,
    pub relay_ms: f64,
    /// The card's time (the model), and its queue's busy time (the upper bound).
    pub card_ms: f64,
    pub card_busy_ms: f64,
    pub title_ms: f64,
    /// The heaps' records' thread (submissions wait for its rounds).
    pub records_ms: f64,
    pub frame_ms: f64,
    /// The frame if the card kept up wherever the log does not show it holding the
    /// pipeline up (the CPU's threads, the title, and the card-bound windows' card).
    pub cpu_frame_ms: f64,
    /// Which of them sets the frame.
    pub bound: &'static str,
}

/// The submitting thread with the measured steps in place of the logged ones; the relay
/// likewise, and the heaps' records; the card as [`card_model`] has it; the title's floor.
fn present_apart_on() -> bool {
    std::env::var_os("VICTORY_PRESENT_THREAD").is_none_or(|v| v != "0")
}

pub fn estimate(w: &Window, m: &Measured, model: &CardModel) -> Estimate {
    // (its waits for the card out where they are the card's time; short ones, in windows the
    // card did not hold up, are the pipeline's friction and stay)
    let waits = if card_bound(w) { w.submit_waited_ms } else { 0.0 };
    // A run that presented on the submitting thread: this build presents on a thread of its
    // own (`VICTORY_PRESENT_THREAD`, on unless 0), so the present call's time comes off it.
    // (A model, not a measurement: the call's time moves to the presenting thread.)
    let moved = if !w.present_apart && present_apart_on() { w.present_ms } else { 0.0 };
    let submit_ms = (w.submit_ms - waits - moved - w.watch_ms - w.refresh_ms - w.sent_ms).max(0.0)
        + m.watch_ms
        + m.copies_ms
        + m.sent_ms;
    let relay_ms = (w.relay_ms - w.merge_ms).max(0.0) + m.merge_ms;
    let card_busy_ms = busy(w);
    let card_ms = if card_bound(w) {
        card_busy_ms
    } else {
        model.line.map_or(card_busy_ms, |(a, b)| (a + b * w.draws).clamp(0.0, card_busy_ms))
    };
    let card_ms = (card_ms - m.card_saved_ms).max(0.0);
    // (sampled where the log has it: the busiest title thread's own code; else the run's floor)
    let title_ms = if w.title_own_ms > 0.0 { w.title_own_ms } else { model.title_floor_ms };
    let records_ms = if w.heap_ms > 0.0 {
        (w.heap_ms - w.heap_ask_ms - w.heap_read_ms).max(0.0) + m.heap_ask_ms + m.heap_read_ms
    } else {
        0.0
    };
    let terms = [(submit_ms, "submit"), (relay_ms, "relay"), (card_ms, "card"), (title_ms, "title"), (records_ms, "records")];
    let (frame_ms, bound) = terms.iter().copied().fold((0.0, "submit"), |a, t| if t.0 > a.0 { t } else { a });
    let card_kept_up = if card_bound(w) { card_ms } else { 0.0 };
    let cpu_frame_ms = submit_ms.max(relay_ms).max(title_ms).max(records_ms).max(card_kept_up);
    Estimate { submit_ms, relay_ms, card_ms, card_busy_ms, title_ms, records_ms, frame_ms, cpu_frame_ms, bound }
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// The number on a report's first line that begins with `head`, right after `key`.
fn reported(report: &str, head: &str, key: &str) -> Option<f64> {
    report.lines().find(|l| l.starts_with(head)).and_then(|l| after(l, key))
}

/// The report of `victory bench` for a log: per gameplay window the estimate against the
/// frame rate the run had, and the steps measured against the logged ones. With `compare`,
/// an earlier report (of another build, on the same log) to set this one's against.
pub fn run(log_path: &Path, compare: Option<&Path>) -> anyhow::Result<String> {
    let earlier = compare.map(std::fs::read_to_string).transpose()?;
    let log = std::fs::read_to_string(log_path)?;
    let all = windows(&log);
    let play = gameplay(&all);
    anyhow::ensure!(!play.is_empty(), "no gameplay frame-rate lines (1,000 draws a frame or more) in {}", log_path.display());
    let model = card_model(&play);
    let held_mb = play.iter().map(|w| w.copies_held_mb).fold(0.0, f64::max).clamp(256.0, 2048.0);
    let title = Title::new(((held_mb * 1e6) as usize).div_ceil(BLOCK) * BLOCK)
        .ok_or_else(|| anyhow::anyhow!("no title memory of {held_mb:.0} MB"))?;
    let most = |f: fn(&Window) -> f64| play.iter().map(f).fold(0.0, f64::max);
    let rig = Rig::new(&title);
    let mut held = vec![0u8; title.len];
    let card_len = ((most(|w| w.sent_mb).max(most(|w| w.pieces) * BLOCK as f64 / 1e6) * 1e6) as usize).max(4 << 20) + (4 << 20);
    let mut card = vec![0u8; card_len];
    // A heap of descriptors of the size the title's placed one has (2,048 records of 32 bytes
    // a 64 KiB page; 185 MB in the runs of 2026-11-27).
    let mut heap = play.iter().any(|w| w.heap_pages > 0.0).then(|| Heap::new(185 << 20)).flatten();
    // (warm: first touches, the pool's threads, the write watch's first asking)
    let _ = measure(&play[0], &title, &rig, &mut held, &mut card, heap.as_mut());

    let mut out = String::new();
    writeln!(out, "log: {}", log_path.display())?;
    writeln!(out, "title memory watched: {:.0} MB; {} gameplay windows of {} (5 s each)", title.len as f64 / 1e6, play.len(), all.len())?;
    match model.line {
        Some((a, b)) => writeln!(
            out,
            "card model: {} card-bound windows (the submitting thread waited {CARD_BOUND_MS:.0} ms or more for the card); busy = {a:.1} ms + {:.2} ms per 1,000 draws, never above a window's busy",
            model.card_bound_windows,
            b * 1000.0
        )?,
        None => writeln!(
            out,
            "card model: {} card-bound windows, too few for a line: the card's busy time taken as its time (an upper bound)",
            model.card_bound_windows
        )?,
    }
    writeln!(out, "title floor: {:.1} ms a frame (10th percentile of frame less its waits at presents)\n", model.title_floor_ms)?;
    writeln!(
        out,
        "{:<19} {:>6} {:>7} | {:>7} {:>7} {:>7} {:>7} {:>7} | {:>7} {:>6} {:>6} {:<7}",
        "window", "fps", "ms", "submit", "relay", "card", "busy", "title", "est ms", "est", "error", "bound"
    )?;
    let (mut errors, mut est_fps, mut log_fps, mut cpu_fps) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut steps: Vec<(Measured, Window, Estimate)> = Vec::new();
    for w in &play {
        let m = measure(w, &title, &rig, &mut held, &mut card, heap.as_mut());
        let e = estimate(w, &m, &model);
        let fps = 1000.0 / e.frame_ms.max(1e-3);
        let error = (fps - w.fps) / w.fps * 100.0;
        let bound = if card_bound(w) { format!("{}*", e.bound) } else { e.bound.to_owned() };
        writeln!(
            out,
            "{:<19} {:>6.1} {:>7.1} | {:>7.1} {:>7.1} {:>7.1} {:>7.1} {:>7.1} | {:>7.1} {:>6.1} {:>+5.0}% {:<7}",
            w.at, w.fps, w.frame_ms, e.submit_ms, e.relay_ms, e.card_ms, e.card_busy_ms, e.title_ms, e.frame_ms, fps, error, bound
        )?;
        errors.push(error.abs());
        est_fps.push(fps);
        log_fps.push(w.fps);
        cpu_fps.push(1000.0 / e.cpu_frame_ms.max(1e-3));
        steps.push((m, w.clone(), e));
    }
    writeln!(out, "(* a card-bound window: its card time is its busy time)")?;
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    writeln!(out)?;
    writeln!(out, "measured steps, median ms a frame (this build | the run's log):")?;
    let pair = |f: fn(&(Measured, Window, Estimate)) -> (f64, f64)| {
        let (a, b): (Vec<f64>, Vec<f64>) = steps.iter().map(f).unzip();
        (median(a), median(b))
    };
    for (name, (a, b)) in [
        ("write watch (the submission's asking)", pair(|s| (s.0.watch_ms, s.1.watch_ms))),
        ("refreshes (batch, checks, copies)", pair(|s| (s.0.copies_ms, s.1.refresh_ms))),
        ("sending submissions' bytes up", pair(|s| (s.0.sent_ms, s.1.sent_ms))),
        ("landing (merging)", pair(|s| (s.0.merge_ms, s.1.merge_ms))),
        ("heap records: asking", pair(|s| (s.0.heap_ask_ms, s.1.heap_ask_ms))),
        ("heap records: reading, comparing", pair(|s| (s.0.heap_read_ms, s.1.heap_read_ms))),
    ] {
        writeln!(out, "  {name:<38} {a:>6.1} | {b:>6.1}")?;
    }
    writeln!(out)?;
    let logged = |f: fn(&Window) -> f64| median(play.iter().map(f).collect());
    writeln!(
        out,
        "the card's frame, median ms (from the log): busy {:.1} (of it our water passes {:.2}; the rest the title's work, our other passes and waits inside submissions, which the log does not tell apart), idle while the submitting thread worked {:.1} (fed late, waiting on us), idle while the title had not submitted {:.1}; the submitting thread stood waiting for the card {:.1}",
        logged(busy),
        logged(|w| w.water_ms),
        logged(|w| w.card_behind_ms),
        logged(|w| (w.card_idle_ms - w.card_behind_ms).max(0.0)),
        logged(|w| w.submit_waited_ms),
    )?;
    let bounds = |name: &str| steps.iter().filter(|s| s.2.bound == name).count();
    writeln!(
        out,
        "what sets the frame, windows: the submitting thread {}, the relay {}, the card {}, the title {}",
        bounds("submit"),
        bounds("relay"),
        bounds("card"),
        bounds("title")
    )?;
    writeln!(out)?;
    let (est, cpu) = (mean(&est_fps), mean(&cpu_fps));
    writeln!(
        out,
        "average frame rate: estimated {:.1} fps, the run {:.1} fps (median {:.1} | {:.1}); mean error per window {:.1} %",
        est,
        mean(&log_fps),
        median(est_fps.clone()),
        median(log_fps.clone()),
        mean(&errors)
    )?;
    writeln!(out, "if the card kept up: {cpu:.1} fps (the CPU's threads and the title alone: the most CPU-side work could bring)")?;
    writeln!(
        out,
        "(measured: the write watch's asking and the refreshes as submissions run them, sending up, landing, the heaps' records' asking and compare; from the log: the rest of the submitting thread, the relay's waits, the records' making, the card, the title)"
    )?;
    // Our own passes on the card (hashes before submissions, compares after them), on this
    // PC's card with the log's counts: a pass per run as the log's build recorded them, and
    // as table passes (`VICTORY_CARD_TABLE`, a pass per submission and kind here). The log
    // has no card time of its own for them.
    let counted: Vec<&Window> = play.iter().filter(|w| w.compare_passes > 0.0 || w.hash_passes > 0.0).collect();
    match (counted.is_empty(), crate::bench_gpu::Card::new()) {
        (true, _) => writeln!(out, "host GPU passes: the log counts none")?,
        (false, None) => writeln!(out, "host GPU passes: no hardware D3D12 device here; not measured")?,
        (false, Some(mut card)) => {
            let mut runs = Vec::new();
            let mut tables = Vec::new();
            for w in &counted {
                if let Some((run, table)) =
                    card.frame(w.hash_passes, w.hashed_pieces, w.compare_passes, w.compared_pieces, w.submissions.max(1.0))
                {
                    runs.push(run);
                    tables.push(table);
                }
            }
            let card_ms = median(counted.iter().map(|w| (w.frame_ms - w.card_idle_ms).max(0.0)).collect());
            writeln!(
                out,
                "host GPU passes measured, median ms a frame: {:.2} as a pass per run | {:.2} as table passes (log: none timed; the card busy {:.1})",
                median(runs),
                median(tables),
                card_ms
            )?;
        }
    }
    if let (Some(earlier), Some(path)) = (earlier, compare) {
        writeln!(out)?;
        let change = |a: f64, b: f64| if a > 0.0 { (b - a) / a * 100.0 } else { 0.0 };
        match reported(&earlier, "average frame rate: estimated", "estimated") {
            Some(before) => {
                writeln!(out, "against {}: estimated {before:.1} -> {est:.1} fps ({:+.1} %)", path.display(), change(before, est))?
            }
            None => writeln!(out, "against {}: it has no \"average frame rate\" line", path.display())?,
        }
        if let Some(before) = reported(&earlier, "if the card kept up:", "if the card kept up:") {
            writeln!(out, "  if the card kept up: {before:.1} -> {cpu:.1} fps ({:+.1} %)", change(before, cpu))?;
        }
        let same_log = earlier.lines().next().is_some_and(|l| l == format!("log: {}", log_path.display()));
        if !same_log {
            writeln!(out, "  (the earlier report is of another log: the change says little)")?;
        }
    }
    Ok(out)
}
