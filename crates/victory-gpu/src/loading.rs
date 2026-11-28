//! What is loading or streaming right now, published for the lines under the
//! frame rate in `host/menu.rs`.
//!
//! The HLE crate writes two atomics (bytes the DirectStorage workers read,
//! reads waiting), the frame-rate meter takes a snapshot once a second (the
//! same tick that stores the frame rate), and the overlay only loads that
//! snapshot. Nothing is locked or computed per frame. The snapshot matters
//! as much as the atomics: the overlay repaints and uploads its whole box
//! whenever the text changes, so live counters that move every frame would
//! be a repaint every frame, while a snapshot changes at most once a second,
//! like the frame rate itself.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

/// Bytes the DirectStorage workers read from files since [`sample`] last
/// took them. Written with one `fetch_add` per finished file read by
/// victory-hle's dstorage module, swapped to 0 by [`sample`]. (Separate from
/// dstorage's own `FILE_BYTES`, which its 5 s summary swaps.)
pub static DSTORAGE_READ_BYTES: AtomicU64 = AtomicU64::new(0);

/// DirectStorage reads submitted and not yet started, over all queues.
/// Stored by dstorage.rs under its scheduler lock whenever that number
/// changes.
pub static DSTORAGE_WAITING: AtomicU32 = AtomicU32::new(0);

// slots of the snapshot, in the order the lines are drawn
const MB_PER_S: usize = 0;
const QUEUE: usize = 1;
const TEXTURES: usize = 2;
const UPLOADS: usize = 3;

/// The last snapshot: tenths of a MB/s read in the last window, requests
/// waiting, texture slots due, uploads on the copy queue.
static SEEN: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];

/// Called by the meter once its window of `secs` seconds is over
/// (`Meter::frame` in host.rs, present thread).
pub(crate) fn sample(secs: f64) {
    // the bytes are taken, not read, so the next window starts from zero
    let bytes = DSTORAGE_READ_BYTES.swap(0, Relaxed);
    // decimal megabytes, as the dstorage log lines count them
    let tenths = if secs > 0.0 {
        (bytes as f64 / 1e6 / secs * 10.0).round() as u32
    } else {
        0
    };
    SEEN[MB_PER_S].store(tenths, Relaxed);
    SEEN[QUEUE].store(DSTORAGE_WAITING.load(Relaxed), Relaxed);
    // slot 1 of that census is the slots due at the last pass. it also counts
    // slots due for a repeat look, which pass quickly, so it falls to zero
    // once the backlog is done
    let textures = u32::try_from(crate::draw::HEAP_TEXTURES_QUEUE[1].load(Relaxed)).unwrap_or(u32::MAX);
    SEEN[TEXTURES].store(textures, Relaxed);
    // zero unless VICTORY_UPLOAD_AHEAD is on, as nothing is sent ahead then
    let uploads = u32::try_from(crate::texture::ahead::in_flight()).unwrap_or(u32::MAX);
    SEEN[UPLOADS].store(uploads, Relaxed);
}

/// The extra lines for the frame-rate line, each starting with '\n', empty
/// when nothing is loading.
pub(crate) fn lines() -> String {
    render([0, 1, 2, 3].map(|i| SEEN[i].load(Relaxed)))
}

/// [`lines`] over given numbers, apart from it so tests need no globals.
pub(crate) fn render(seen: [u32; 4]) -> String {
    let mut out = String::new();
    // a zero line is left out, so an idle title draws exactly what it did
    // before. labels are upper case on purpose: host/font.rs draws upper
    // case, digits, ':' '.' '/' and space
    if seen[MB_PER_S] != 0 {
        out.push_str(&format!("\nDSTORAGE: {:.1} MB/S", f64::from(seen[MB_PER_S]) / 10.0));
    }
    if seen[QUEUE] != 0 {
        out.push_str(&format!("\nDSTORAGE QUEUE: {}", seen[QUEUE]));
    }
    if seen[TEXTURES] != 0 {
        out.push_str(&format!("\nTEXTURES: {}", seen[TEXTURES]));
    }
    if seen[UPLOADS] != 0 {
        out.push_str(&format!("\nUPLOADS: {}", seen[UPLOADS]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_loading_draws_nothing() {
        assert_eq!(render([0, 0, 0, 0]), "");
    }

    #[test]
    fn each_slot_alone_draws_its_one_line() {
        assert_eq!(render([1234, 0, 0, 0]), "\nDSTORAGE: 123.4 MB/S");
        assert_eq!(render([0, 7, 0, 0]), "\nDSTORAGE QUEUE: 7");
        assert_eq!(render([0, 0, 42, 0]), "\nTEXTURES: 42");
        assert_eq!(render([0, 0, 0, 3]), "\nUPLOADS: 3");
    }

    #[test]
    fn all_four_draw_in_order() {
        assert_eq!(
            render([50, 2, 9, 1]),
            "\nDSTORAGE: 5.0 MB/S\nDSTORAGE QUEUE: 2\nTEXTURES: 9\nUPLOADS: 1"
        );
    }

    #[test]
    fn every_byte_is_ascii() {
        // the overlay font has no glyph for anything else
        assert!(render([1, 1, 1, 1]).is_ascii());
    }
}
