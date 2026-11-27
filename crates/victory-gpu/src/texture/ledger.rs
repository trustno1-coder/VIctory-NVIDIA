//! The texture book: what the title's DirectStorage stream fills, followed as
//! it happens, and the textures made ready for it as soon as it has landed.
//!
//! The title asks DirectStorage for texture data (`dstorage.rs`
//! `enqueue_request`) and names the texture in a descriptor only when it
//! uses it, often much later (at world entry: 1.8 million slot writes, a
//! queue of 26,000, minutes at 4-13 fps while the frame thread creates,
//! detiles and uploads 82,000 textures). Here each finished request that
//! filled title memory is noted as it completes ([`request_done`]). If the
//! emulator knows what texture lives at that address (a view that named it
//! earlier in this run, [`desc_known`] / [`named`]; or, for a request that
//! was seen to fill a named texture before, the description the book kept,
//! which only fills in metadata and never predicts a load) and the request
//! covers it, a worker makes the host texture, detiles and uploads it on the
//! copy queue (`ahead`). When the title names the slot, the pass finds the
//! host copy holding exactly the memory's bytes (`texture::unchanged`) and
//! adopts it without an upload.
//!
//! Nothing is loaded that the title did not ask for: only completed
//! requests, only into memory that is mapped, only while the card has room
//! (`resource::room`), host RAM admits (`residency`), and not more than
//! [`UNNAMED_MOST`] bytes made ready and not yet named. A made-ready texture
//! nobody names goes the way of any idle host copy (`resource::let_go_idle`).
//!
//! The book on disk keeps `request -> texture description` for requests whose
//! destination was named by a slot and stayed resident (no use count of
//! its own without that; entries seen evicted or overwritten are dropped,
//! entries unused for [`STALE_RUNS`] runs age out). It holds hashes, offsets
//! and layout words only: no asset bytes (docs/asset-policy.md).
//!
//! Also here: the loading gate. A loading screen is seen from the request
//! stream (requests completing, a `.rpf` opening) while few draws are made;
//! [`loading`] stays true for [`HOLD_MS`] after the last such sign.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};

/// What identifies a DirectStorage request across runs: the pack file's name
/// (hashed), where and how much was read, and how it was decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub file: u64,
    pub offset: u64,
    pub source_size: u32,
    pub dest_size: u32,
    pub options: u64,
}

const MAGIC: &[u8; 4] = b"VTB2";
/// Entries kept at most, most used first.
const MAX_ENTRIES: usize = 150_000;
/// Runs an entry may go unnamed before it ages out.
const STALE_RUNS: u32 = 6;
/// Named textures a session needs before it may rewrite the book (a session
/// that never left the menu leaves it as it was).
const MIN_NAMED_TO_SAVE: usize = 256;
const SAVE_EVERY_MS: u64 = 30_000;
/// Entries of each map before it is started over.
const MAP_CAP: usize = 1 << 19;
/// A description seen at an address is trusted this long for requests
/// landing there.
const DESC_FRESH_MS: u64 = 120_000;
/// Wait before a made-ready texture is started: a request whose memory is
/// overwritten or freed at once is not worth it.
const PREHEAT_DELAY_MS: u64 = 120;
const PREHEAT_QUEUE: usize = 16_384;
/// Bytes made ready and not named yet at most; they age out after
/// [`UNNAMED_MS`].
const UNNAMED_MOST: u64 = 2 << 30;
const UNNAMED_MS: u64 = 60_000;
/// Draws per frame under which the title is on a loading screen when its
/// request stream is busy.
const LOADING_DRAWS: u64 = 200;
/// The gate stays on this long after its last sign.
const HOLD_MS: u64 = 3_000;

/// What the book knows of a request that filled a named texture.
#[derive(Clone, Copy)]
struct Meta {
    resource: [u32; 14],
    first: u32,
    uses: u32,
    last_run: u32,
}

/// A description seen at an address: words, first level of the view that
/// named it (`u32::MAX`: not known), when, and whether a slot of the pass
/// named it (not just a texture made at that address).
#[derive(Clone, Copy)]
struct Seen {
    resource: [u32; 14],
    first: u32,
    at_ms: u64,
    by_pass: bool,
}

#[derive(Clone, Copy)]
struct Output {
    len: u64,
    key: Option<Key>,
}

#[derive(Default)]
struct Book {
    dir: Option<PathBuf>,
    run: u32,
    meta: HashMap<Key, Meta>,
    descs: BTreeMap<u64, Seen>,
    outputs: BTreeMap<u64, Output>,
    /// Named and resident in this session, by address.
    named: BTreeMap<u64, Output>,
    touched: HashSet<Key>,
    evicted: HashSet<Key>,
    /// Made ready and not named yet: address -> (bytes, when).
    ready: HashMap<u64, (u64, u64)>,
    dirty: bool,
    last_save: u64,
}

static BOOK: Mutex<Option<Book>> = Mutex::new(None);

fn book() -> MutexGuard<'static, Option<Book>> {
    BOOK.lock().unwrap_or_else(|e| e.into_inner())
}

fn with<R>(f: impl FnOnce(&mut Book) -> R) -> R {
    f(book().get_or_insert_with(Book::default))
}

/// Requests finished and bytes written since the gate last looked.
static WIN: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static LAST_BUSY: AtomicU64 = AtomicU64::new(0);
static PACK_AT: AtomicU64 = AtomicU64::new(0);
static LOADING_UNTIL: AtomicU64 = AtomicU64::new(0);
static LOADING_SINCE: AtomicU64 = AtomicU64::new(0);
static TICKED: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
/// Bytes made ready and not named.
static UNNAMED: AtomicU64 = AtomicU64::new(0);
static SAVING: AtomicBool = AtomicBool::new(false);
/// Since the start: requests done, with a description at hand, made ready,
/// adopted by the pass (named while made ready), left out for: no room,
/// host memory, not mapped, taken by none.
static STATS: [AtomicU64; 10] = [const { AtomicU64::new(0) }; 10];
const DONE: usize = 0;
const KNOWN: usize = 1;
const READY: usize = 2;
const ADOPTED: usize = 3;
const NO_ROOM: usize = 4;
const NO_HOST_RAM: usize = 5;
const NOT_MAPPED: usize = 6;
const NOT_TAKEN: usize = 7;
const OVERWRITTEN: usize = 8;
const LOADS: usize = 9;

/// Keeps the book in `dir` (a folder of the user's cache, checked by the
/// caller) from now on, and reads what an earlier run kept.
pub fn use_dir(dir: PathBuf) {
    super::converted::use_dir(dir.join("converted"));
    let loaded = std::fs::read(dir.join("book.bin")).ok().and_then(|bytes| parse(&bytes));
    with(|b| {
        b.dir = Some(dir);
        if let Some((run, meta)) = loaded {
            tracing::info!(entries = meta.len(), run, "texture book: read what earlier runs kept");
            b.run = run;
            b.meta = meta;
        } else {
            b.run = 1;
        }
    });
}

/// A pack file was opened; the number to put in the [`Key`]s of its requests.
pub fn file_opened(path: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in path.bytes() {
        hash = (hash ^ u64::from(byte.to_ascii_lowercase())).wrapping_mul(0x100_0000_01b3);
    }
    if path.len() > 4 && path[path.len() - 4..].eq_ignore_ascii_case(".rpf") {
        PACK_AT.store(crate::host::coarse_ms().max(1), Relaxed);
    }
    hash | 1
}

/// A DirectStorage request finished and wrote `len` bytes at title address
/// `at` (0: not known, or not in one run of addresses).
pub fn request_done(key: Option<Key>, at: usize, len: usize) {
    WIN[0].fetch_add(1, Relaxed);
    WIN[1].fetch_add(len as u64, Relaxed);
    STATS[DONE].fetch_add(1, Relaxed);
    // The gate is looked at from here too: on a loading screen the texture
    // pass may not run at all (twice a second at most).
    tick(crate::frame::current(), crate::draw::drawn());
    if at == 0 || len < 4096 {
        return;
    }
    let (at, len) = (at as u64, len as u64);
    let now = crate::host::coarse_ms();
    let job = with(|b| {
        let end = at + len;
        // Named textures this overwrites (other than by the same request
        // again) are not what the title kept.
        let overlapped: Vec<(u64, Output)> = b
            .named
            .range(..at)
            .next_back()
            .into_iter()
            .chain(b.named.range(at..end))
            .filter(|(a, n)| **a + n.len > at)
            .map(|(a, n)| (*a, *n))
            .collect();
        for (a, n) in overlapped {
            if n.key != key || key.is_none() {
                b.named.remove(&a);
                if let Some(k) = n.key {
                    b.evicted.insert(k);
                }
                b.dirty = true;
                STATS[OVERWRITTEN].fetch_add(1, Relaxed);
            }
        }
        if b.outputs.len() > MAP_CAP {
            b.outputs.clear();
        }
        b.outputs.insert(at, Output { len, key });
        let seen = b.descs.get(&at).filter(|s| now.saturating_sub(s.at_ms) < DESC_FRESH_MS).copied();
        let seen = seen.or_else(|| {
            key.and_then(|k| b.meta.get(&k)).map(|m| Seen { resource: m.resource, first: m.first, at_ms: now, by_pass: false })
        })?;
        STATS[KNOWN].fetch_add(1, Relaxed);
        if seen.by_pass {
            link(b, at, &seen.resource, seen.first, Output { len, key });
        }
        let first = if seen.first == u32::MAX { 0 } else { seen.first };
        let bytes = crate::resource::placed_bytes_from(&seen.resource, first)?;
        // The request must cover the texture's memory.
        if bytes == 0 || bytes > len || UNNAMED.load(Relaxed) + bytes > UNNAMED_MOST {
            return None;
        }
        Some(Job { at, resource: seen.resource, first, bytes, key, len, due: now + PREHEAT_DELAY_MS })
    });
    if let Some(job) = job {
        queue(job);
    }
}

/// The pass made `address`'s texture (description `resource`, view from level
/// `first`) whole and resident: the title named it and keeps it.
pub(crate) fn named(address: u64, resource: &[u32; 14], first: u32) {
    let mut r = *resource;
    r[1] = 0;
    r[13] = 0;
    let now = crate::host::coarse_ms();
    with(|b| {
        // Looked at again as it was (most calls): nothing new.
        if b.descs.get(&address).is_some_and(|s| s.by_pass && s.resource == r && s.first == first)
            && b.named.contains_key(&address)
            && !b.ready.contains_key(&address)
        {
            return;
        }
        if b.descs.len() > MAP_CAP {
            b.descs.clear();
        }
        b.descs.insert(address, Seen { resource: r, first, at_ms: now, by_pass: true });
        if let Some((bytes, _)) = b.ready.remove(&address) {
            UNNAMED.fetch_sub(bytes.min(UNNAMED.load(Relaxed)), Relaxed);
            STATS[ADOPTED].fetch_add(1, Relaxed);
        }
        if let Some(out) = b.outputs.get(&address).copied() {
            link(b, address, &r, first, out);
        }
    });
}

fn link(b: &mut Book, address: u64, r: &[u32; 14], first: u32, out: Output) {
    b.named.insert(address, out);
    let Some(key) = out.key else { return };
    let fresh = b.touched.insert(key);
    let run = b.run;
    let m = b.meta.entry(key).or_insert(Meta { resource: *r, first, uses: 0, last_run: run });
    m.resource = *r;
    m.first = first;
    m.last_run = run;
    if fresh {
        m.uses = m.uses.saturating_add(1);
        b.dirty = true;
    }
    b.evicted.remove(&key);
}

/// A texture was made at `address` for a view that names it (first time for
/// that address and description): what lives there is known from now on.
pub(crate) fn desc_known(address: u64, resource: &[u32; 14]) {
    let mut r = *resource;
    r[1] = 0;
    r[13] = 0;
    let now = crate::host::coarse_ms();
    with(|b| {
        if b.descs.get(&address).is_some_and(|s| s.resource == r) {
            return;
        }
        if b.descs.len() > MAP_CAP {
            b.descs.clear();
        }
        b.descs.insert(address, Seen { resource: r, first: u32::MAX, at_ms: now, by_pass: false });
    });
}

/// The memory of the texture at `address` is gone (streamed out): it is not
/// what the title kept.
pub(crate) fn gone(address: u64) {
    with(|b| {
        b.descs.remove(&address);
        if let Some(out) = b.named.remove(&address) {
            if let Some(k) = out.key {
                b.evicted.insert(k);
            }
            b.dirty = true;
        }
    });
}

/// A made-ready job: which texture, from which level, how many bytes, and
/// the request it came from.
struct Job {
    at: u64,
    resource: [u32; 14],
    first: u32,
    bytes: u64,
    key: Option<Key>,
    len: u64,
    due: u64,
}

static QUEUE: (Mutex<VecDeque<Job>>, Condvar) = (Mutex::new(VecDeque::new()), Condvar::new());

fn queue(job: Job) {
    static STARTED: OnceLock<bool> = OnceLock::new();
    let started = *STARTED.get_or_init(|| std::thread::Builder::new().name("texture book".into()).spawn(worker).is_ok());
    if !started {
        return;
    }
    let mut q = QUEUE.0.lock().unwrap_or_else(|e| e.into_inner());
    if q.len() >= PREHEAT_QUEUE {
        return;
    }
    q.push_back(job);
    QUEUE.1.notify_one();
}

fn worker() {
    // As the pass's thread: the locks it takes are counted with its own.
    crate::descriptor::IN_TEXTURE_PASS.set(true);
    loop {
        let job = {
            let mut q = QUEUE.0.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                let now = crate::host::coarse_ms();
                match q.front().map(|j| j.due) {
                    None => q = QUEUE.1.wait(q).unwrap_or_else(|e| e.into_inner()),
                    Some(due) if due > now => {
                        q = QUEUE.1.wait_timeout(q, std::time::Duration::from_millis(due - now)).unwrap_or_else(|e| e.into_inner()).0;
                    }
                    Some(_) => break q.pop_front(),
                }
            }
        };
        if let Some(job) = job {
            make_ready(job);
        }
    }
}

/// Makes the host texture of a finished request's destination and sends its
/// upload to the copy queue.
fn make_ready(job: Job) {
    // Still what that memory holds: not overwritten by another request since.
    let kept = with(|b| b.outputs.get(&job.at).is_some_and(|o| o.key == job.key && o.len == job.len));
    if !kept || !super::ahead::on() {
        return;
    }
    let memory = crate::host::host().and_then(|h| h.video_memory());
    if !crate::resource::room(memory) {
        STATS[NO_ROOM].fetch_add(1, Relaxed);
        return;
    }
    if !crate::residency::admit_texture_pass(false) {
        STATS[NO_HOST_RAM].fetch_add(1, Relaxed);
        return;
    }
    if crate::texture::mapped_bytes(job.at as *const u8, job.bytes as usize) < job.bytes {
        STATS[NOT_MAPPED].fetch_add(1, Relaxed);
        return;
    }
    let Some(texture) = crate::resource::texture_at(job.at, &job.resource) else {
        return;
    };
    crate::resource::want_level(texture, job.first);
    match super::ahead::take(texture, job.first, job.bytes) {
        Some(super::ahead::Taken::New(upload)) => {
            super::ahead::send(&upload);
            // As the pass does after sending: the job holds the staging
            // buffer, the texture keeps none (a texture with one is read as
            // one draws read through a table).
            crate::texture::unstage(texture);
            UNNAMED.fetch_add(job.bytes, Relaxed);
            with(|b| {
                b.ready.insert(job.at, (job.bytes, crate::host::coarse_ms()));
            });
            STATS[READY].fetch_add(1, Relaxed);
        }
        Some(super::ahead::Taken::Known(_)) => {}
        None => {
            STATS[NOT_TAKEN].fetch_add(1, Relaxed);
        }
    }
}

/// Whether the title is on a loading screen: its request stream is busy (or
/// a pack file opened a moment ago) while it draws little.
pub(crate) fn loading() -> bool {
    crate::host::coarse_ms() < LOADING_UNTIL.load(Relaxed)
}

/// From the pass and from finished requests, twice a second at most: the gate, the one line every five seconds, the
/// book's saving, and the made-ready textures nobody named letting go of
/// their count.
pub(crate) fn tick(frame: u64, drawn: u64) {
    let now = crate::host::coarse_ms();
    let before = TICKED[0].load(Relaxed);
    if now.saturating_sub(before) < 500 || TICKED[0].compare_exchange(before, now, Relaxed, Relaxed).is_err() {
        return;
    }
    let frames = frame.saturating_sub(TICKED[1].swap(frame, Relaxed));
    let draws = drawn.saturating_sub(TICKED[2].swap(drawn, Relaxed));
    // (No frame at all while the stream is busy: a load the title waits for.)
    let per_frame = if frames == 0 { 0 } else { draws / frames };
    let (requests, bytes) = (WIN[0].swap(0, Relaxed), WIN[1].swap(0, Relaxed));
    if requests >= 4 || bytes >= 4 << 20 {
        LAST_BUSY.store(now, Relaxed);
    }
    let recently = |at: u64| at != 0 && now.saturating_sub(at) < 2_500;
    let opened = PACK_AT.load(Relaxed) != 0 && now.saturating_sub(PACK_AT.load(Relaxed)) < 8_000;
    let busy = recently(LAST_BUSY.load(Relaxed)) || opened;
    // (`LOADING_SINCE` is not 0 from the line that says the screen is
    // there until the one that says it is over.)
    let announced = LOADING_SINCE.load(Relaxed) != 0;
    if busy && per_frame < LOADING_DRAWS {
        LOADING_UNTIL.store(now + HOLD_MS, Relaxed);
        if !announced {
            LOADING_SINCE.store(now, Relaxed);
            STATS[LOADS].fetch_add(1, Relaxed);
            tracing::info!(
                requests,
                mb = bytes >> 20,
                draws_per_frame = per_frame.min(u64::from(u32::MAX)),
                "texture book: loading screen seen in the request stream; staging budgets raised"
            );
        }
    }
    if announced && now >= LOADING_UNTIL.load(Relaxed) {
        tracing::info!(
            seconds = now.saturating_sub(LOADING_SINCE.swap(0, Relaxed)) / 1000,
            "texture book: the loading screen is over; staging budgets back to normal"
        );
    }
    // Those made ready that nobody named in time stop counting against the
    // limit (their host copies go the way of idle ones).
    let late = with(|b| {
        let old: Vec<u64> = b.ready.iter().filter(|(_, (_, at))| now.saturating_sub(*at) > UNNAMED_MS).map(|(a, _)| *a).collect();
        old.into_iter().filter_map(|a| b.ready.remove(&a)).map(|(bytes, _)| bytes).sum::<u64>()
    });
    if late != 0 {
        UNNAMED.fetch_sub(late.min(UNNAMED.load(Relaxed)), Relaxed);
    }
    if now.saturating_sub(TICKED[3].load(Relaxed)) >= 5_000 {
        TICKED[3].store(now, Relaxed);
        let s = |i: usize| STATS[i].load(Relaxed);
        if s(DONE) != 0 {
            tracing::info!(
                requests_done = s(DONE),
                texture_known = s(KNOWN),
                made_ready = s(READY),
                adopted_by_slots = s(ADOPTED),
                unnamed_mb = UNNAMED.load(Relaxed) >> 20,
                no_room = s(NO_ROOM),
                no_host_ram = s(NO_HOST_RAM),
                not_mapped = s(NOT_MAPPED),
                not_taken = s(NOT_TAKEN),
                overwritten = s(OVERWRITTEN),
                loading_screens = s(LOADS),
                loading = loading(),
                "texture book: the title's requests since the start"
            );
        }
    }
    let due = with(|b| {
        let named = b.named.len();
        b.dirty && named >= MIN_NAMED_TO_SAVE && b.dir.is_some() && now.saturating_sub(b.last_save) >= SAVE_EVERY_MS
    });
    if due && !SAVING.swap(true, Relaxed) {
        with(|b| b.last_save = now);
        let _ = std::thread::Builder::new().name("texture book save".into()).spawn(|| {
            save();
            SAVING.store(false, Relaxed);
        });
    }
}

fn save() {
    let Some((dir, bytes)) = with(|b| {
        let dir = b.dir.clone()?;
        let run = b.run;
        let Book { meta, evicted, touched, .. } = b;
        meta.retain(|k, m| !(evicted.contains(k) && !touched.contains(k)) && run.saturating_sub(m.last_run) < STALE_RUNS);
        if meta.len() > MAX_ENTRIES {
            let mut uses: Vec<u32> = meta.values().map(|m| m.uses).collect();
            uses.sort_unstable_by(|a, b| b.cmp(a));
            let least = uses[MAX_ENTRIES - 1];
            let mut room = MAX_ENTRIES;
            meta.retain(|_, m| {
                let keep = m.uses > least || (m.uses == least && room > 0);
                if keep {
                    room = room.saturating_sub(1);
                }
                keep
            });
        }
        b.dirty = false;
        Some((dir, encode(b.run, &b.meta)))
    }) else {
        return;
    };
    let tmp = dir.join("book.tmp");
    let done = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(&tmp, &bytes))
        .and_then(|()| std::fs::rename(&tmp, dir.join("book.bin")));
    match done {
        Ok(()) => tracing::debug!(bytes = bytes.len(), "texture book: saved"),
        Err(e) => {
            tracing::warn!(error = %e, "texture book: not saved");
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

fn encode(run: u32, meta: &HashMap<Key, Meta>) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + meta.len() * 100);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&run.to_le_bytes());
    out.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    for (k, m) in meta {
        out.extend_from_slice(&k.file.to_le_bytes());
        out.extend_from_slice(&k.offset.to_le_bytes());
        out.extend_from_slice(&k.source_size.to_le_bytes());
        out.extend_from_slice(&k.dest_size.to_le_bytes());
        out.extend_from_slice(&k.options.to_le_bytes());
        for w in m.resource {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out.extend_from_slice(&m.first.to_le_bytes());
        out.extend_from_slice(&m.uses.to_le_bytes());
        out.extend_from_slice(&m.last_run.to_le_bytes());
    }
    out
}

/// The run number to use now, and the entries. `None` for a file that is not
/// ours or cut short (the book starts over).
fn parse(bytes: &[u8]) -> Option<(u32, HashMap<Key, Meta>)> {
    const ENTRY: usize = 32 + 56 + 12;
    let (head, mut rest) = bytes.split_at_checked(12)?;
    if &head[..4] != MAGIC {
        return None;
    }
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap_or([0; 4]));
    let u64_at = |b: &[u8], i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap_or([0; 8]));
    let (run, count) = (u32_at(head, 4), u32_at(head, 8) as usize);
    if rest.len() != count.checked_mul(ENTRY)? {
        return None;
    }
    let mut meta = HashMap::with_capacity(count);
    while let Some((e, tail)) = rest.split_at_checked(ENTRY) {
        rest = tail;
        let key = Key {
            file: u64_at(e, 0),
            offset: u64_at(e, 8),
            source_size: u32_at(e, 16),
            dest_size: u32_at(e, 20),
            options: u64_at(e, 24),
        };
        let mut resource = [0u32; 14];
        for (i, w) in resource.iter_mut().enumerate() {
            *w = u32_at(e, 32 + 4 * i);
        }
        let last_run = u32_at(e, 32 + 56 + 8);
        meta.insert(key, Meta { resource, first: u32_at(e, 88), uses: u32_at(e, 92), last_run });
    }
    // A new session counts as the run after the newest the file knows.
    Some((run.saturating_add(1), meta))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_book_round_trips() {
        let key = Key { file: 7, offset: 1 << 33, source_size: 5, dest_size: 9, options: 0x1e };
        let mut meta = HashMap::new();
        meta.insert(key, Meta { resource: [3, 0, 0, 0, 256, 0, 256, 0x9_0001, 98, 1, 0, 0x105, 0, 0], first: 2, uses: 4, last_run: 3 });
        let bytes = encode(3, &meta);
        let (run, back) = parse(&bytes).unwrap();
        assert_eq!(run, 4);
        let m = back[&key];
        assert_eq!((m.first, m.uses, m.last_run, m.resource[4]), (2, 4, 3, 256));
        assert!(parse(&bytes[..bytes.len() - 1]).is_none());
        assert!(parse(b"nope").is_none());
    }

    #[test]
    fn a_pack_is_known_by_its_name_in_any_case() {
        assert_eq!(file_opened("X:\\Pack\\A.rpf"), file_opened("x:\\pack\\a.RPF"));
        assert_ne!(file_opened("a"), file_opened("b"));
    }
}
