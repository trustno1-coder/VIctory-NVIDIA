//! Measurement only (no behaviour change): which 64 KiB blocks of each host
//! copy shaders ever wrote, which of those the title's CPU wrote afterwards,
//! and which the title wrote while a flight's declared range held them
//! ("pending", the bytes `Shadow::merge_under_writes` puts in). Distinct
//! blocks since the copy was made, per copy, logged every ~10 s next to the
//! frame lines. It decides how much a learned GPU-write footprint (report 02,
//! R1) could narrow what is declared. `VICTORY_FOOTPRINT=0` turns it off.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const SHIFT: u32 = 16;
const EVERY: Duration = Duration::from_secs(10);

fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FOOTPRINT").is_none_or(|v| v != "0"))
}

#[derive(Default)]
struct Foot {
    start: u64,
    len: u64,
    blocks: usize,
    /// Blocks shaders wrote (bytes landed in title memory).
    gpu: Vec<u64>,
    /// Blocks the title's CPU changed after shaders had written them.
    cpu_after: Vec<u64>,
    /// Blocks the title wrote while a flight's range held them.
    pending: Vec<u64>,
    /// Events: landings, CPU changes seen, pending merges.
    events: [u64; 3],
    /// Window of the last event (pruning).
    last: u64,
}

fn mark(bits: &mut [u64], i: usize) {
    bits[i / 64] |= 1u64 << (i % 64);
}

fn has(bits: &[u64], i: usize) -> bool {
    bits[i / 64] >> (i % 64) & 1 != 0
}

fn count(bits: &[u64]) -> usize {
    bits.iter().map(|w| w.count_ones() as usize).sum()
}

fn count_and(a: &[u64], b: &[u64]) -> usize {
    a.iter().zip(b).map(|(x, y)| (x & y).count_ones() as usize).sum()
}

impl Foot {
    fn new(start: u64, len: u64) -> Foot {
        let blocks = ((start + len).div_ceil(1 << SHIFT) - (start >> SHIFT)) as usize;
        let words = blocks.div_ceil(64);
        Foot { start, len, blocks, gpu: vec![0; words], cpu_after: vec![0; words], pending: vec![0; words], ..Foot::default() }
    }

    /// The blocks that offsets `[from, to)` of the copy lie in.
    fn blocks_of(&self, from: usize, to: usize) -> std::ops::Range<usize> {
        if from >= to {
            return 0..0;
        }
        let first = (((self.start + from as u64) >> SHIFT) - (self.start >> SHIFT)) as usize;
        let last = (((self.start + to as u64 - 1) >> SHIFT) - (self.start >> SHIFT)) as usize;
        first.min(self.blocks)..(last + 1).min(self.blocks)
    }

    fn gpu_wrote(&mut self, from: usize, to: usize) {
        self.events[0] += 1;
        for i in self.blocks_of(from, to) {
            mark(&mut self.gpu, i);
        }
    }

    fn cpu_wrote(&mut self, from: usize, to: usize) {
        self.events[1] += 1;
        for i in self.blocks_of(from, to) {
            if has(&self.gpu, i) {
                mark(&mut self.cpu_after, i);
            }
        }
    }

    fn pending_wrote(&mut self, runs: usize, spans: &[(usize, usize)]) {
        self.events[2] += runs as u64;
        for &(from, to) in spans {
            for i in from.min(self.blocks)..to.min(self.blocks) {
                mark(&mut self.pending, i);
            }
        }
    }

    /// (blocks, shaders', title's after shaders', pending, pending and shaders').
    fn totals(&self) -> [usize; 5] {
        [self.blocks, count(&self.gpu), count(&self.cpu_after), count(&self.pending), count_and(&self.pending, &self.gpu)]
    }
}

static FEET: Mutex<Option<HashMap<u64, Foot>>> = Mutex::new(None);
static WINDOW: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn with(id: u64, start: u64, len: u64, f: impl FnOnce(&mut Foot)) {
    if !on() {
        return;
    }
    let window = WINDOW.load(std::sync::atomic::Ordering::Relaxed);
    let mut all = FEET.lock().unwrap_or_else(|e| e.into_inner());
    let foot = all.get_or_insert_with(Default::default).entry(id).or_insert_with(|| Foot::new(start, len));
    foot.last = window;
    f(foot);
}

/// Shaders wrote bytes that landed in offsets `[from, to)` of copy `id`
/// (title memory `[start, start + len)`).
pub(crate) fn gpu_wrote(id: u64, start: u64, len: u64, from: usize, to: usize) {
    crate::tight::landed(id, from, to);
    with(id, start, len, |f| f.gpu_wrote(from, to));
}

/// The title's CPU changed offsets `[from, to)` of the copy (an upper bound
/// where a refresh copies whole pages).
pub(crate) fn cpu_wrote(id: u64, start: u64, len: u64, from: usize, to: usize) {
    with(id, start, len, |f| f.cpu_wrote(from, to));
}

/// Collapse word runs to the 64 KiB blocks tracked by the report before
/// taking the lock shared by the submitter and the copy-back workers.
fn pending_blocks(start: u64, runs: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let block = 1usize << SHIFT;
    let skew = start as usize % block;
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for &(from, to) in runs {
        if from < to {
            let (from, to) = ((from + skew) / block, (to + skew).div_ceil(block));
            match spans.last_mut() {
                Some(last) if from <= last.1 && last.0 <= to => {
                    *last = (last.0.min(from), last.1.max(to));
                }
                _ => spans.push((from, to)),
            }
        }
    }
    spans
}

/// The title wrote these byte ranges (offsets in the copy) while flights
/// may have been writing the same copy.
pub(crate) fn pending_wrote(id: u64, start: u64, len: u64, runs: &[(usize, usize)]) {
    if runs.is_empty() || !on() {
        return;
    }
    let spans = pending_blocks(start, runs);
    with(id, start, len, |f| f.pending_wrote(runs.len(), &spans));
}

/// Every ~10 s, from the frame line: the totals and the eight copies shaders
/// wrote most blocks of.
pub(crate) fn log() {
    // What the refreshes took, on or off (its own few seconds).
    {
        static SAID: Mutex<Option<Instant>> = Mutex::new(None);
        let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
        match *said {
            Some(t) if t.elapsed() < EVERY => {}
            Some(t) => {
                crate::shadow::refreshed_line(t.elapsed().as_secs_f64());
                *said = Some(Instant::now());
            }
            None => *said = Some(Instant::now()),
        }
    }
    if !on() {
        return;
    }
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < EVERY) {
            return;
        }
        *last = Some(Instant::now());
    }
    let window = WINDOW.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut rows: Vec<(u64, [usize; 5], [u64; 3], u64, u64)> = {
        let mut all = FEET.lock().unwrap_or_else(|e| e.into_inner());
        let Some(all) = all.as_mut() else {
            return;
        };
        // Copies untouched for ten windows are gone.
        all.retain(|_, f| window.saturating_sub(f.last) < 10);
        all.iter().map(|(&id, f)| (id, f.totals(), f.events, f.start, f.len)).collect()
    };
    rows.sort_by_key(|r| std::cmp::Reverse(r.1[1] + r.1[3]));
    let sum = |i: usize| rows.iter().map(|r| r.1[i]).sum::<usize>();
    tracing::info!(
        "footprint of 64 KiB blocks since each copy was made (distinct; VICTORY_FOOTPRINT=0 off): {} copies, {} blocks, \
         {} written by shaders, {} of those changed by the title after, {} written by the title while pending ({} of them \
         also shaders'); the most:",
        rows.len(),
        sum(0),
        sum(1),
        sum(2),
        sum(3),
        sum(4)
    );
    for (id, t, e, start, len) in rows.into_iter().take(8) {
        tracing::info!(
            "  copy {id} {start:#x}+{len:#x}: {} blocks, shaders {} ({:.0} %), then title {}, pending title {} ({} also shaders'); \
             events {} landings, {} title changes, {} pending merges",
            t[0],
            t[1],
            100.0 * t[1] as f64 / t[0].max(1) as f64,
            t[2],
            t[3],
            t[4],
            e[0],
            e[1],
            e[2]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_distinct_blocks() {
        // A copy that starts in the middle of a block: 3 blocks.
        let mut f = Foot::new(0x1_8000, 0x2_8000);
        assert_eq!(f.blocks, 3);
        f.gpu_wrote(0, 0x100);
        f.gpu_wrote(0x10, 0x20);
        f.gpu_wrote(0x8000, 0x8001);
        assert_eq!(f.totals()[1], 2);
        f.cpu_wrote(0x8000, 0x8010);
        f.cpu_wrote(0x1_8000, 0x1_8010);
        assert_eq!(f.totals()[2], 1);
        let runs = [(0, 0x70), (0x10, 0x8001), (0x8004, 0x8008)];
        let spans = pending_blocks(f.start, &runs);
        assert_eq!(spans, [(0, 2)]);
        f.pending_wrote(runs.len(), &spans);
        assert_eq!(f.events[2], 3);
        assert_eq!(f.totals()[3], 2);
        assert_eq!(f.totals()[4], 2);
    }
}
