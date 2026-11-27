//! The converted-asset cache: what a DirectStorage request decoded (BCPack,
//! and zlib before it), kept in the user's cache folder so that the same
//! request in a later run (or later in this one) is a read of the result, not
//! a decode. Write-through: the first time a request is served its output is
//! stored ([`store`], on a thread of its own); every later time it is read
//! ([`open`]).
//!
//! An entry is found by the request ([`Key`]: pack file, offset, sizes,
//! options) and checked against a digest of the source bytes the request
//! read (`source`): a pack file that changed under the same name and offset
//! misses. The file is `<cache>/textures/converted/<xx>/<hex>`, where `<hex>`
//! is 32 hex digits of a digest of the key's bytes and `<xx>` its first two:
//!
//! ```text
//! 0   "VCV1"
//! 4   u32 format version (1)
//! 8   u64 file (FNV-1a of the lower-cased path the title opened)
//! 16  u64 offset
//! 24  u32 source size
//! 28  u32 destination size
//! 32  u64 request options (as the title set them)
//! 40  u128 digest of the source bytes (`texture::digest`)
//! 56  u64 output bytes
//! 64  output bytes, as written to the destination
//! ```
//!
//! Bounded at [`MOST_BYTES`] on disk: the least recently used entries (by
//! file time, touched on every hit) go first. Nothing here is ever written
//! inside the repository or the game's folder: the folder is a sibling of the
//! shader cache, which the configuration checks against the asset policy
//! (docs/asset-policy.md).

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Condvar, Mutex, OnceLock};

use super::ledger::Key;

const MAGIC: &[u8; 4] = b"VCV1";
const VERSION: u32 = 1;
const HEADER: usize = 64;
/// Bytes on disk at most; trimmed to nine tenths of it when passed.
const MOST_BYTES: u64 = 24 << 30;
/// Outputs smaller than this are not worth a file (the decode is cheaper
/// than opening one).
const SMALLEST: usize = 64 << 10;
/// Bytes waiting to be written at most: past it a miss is not stored.
const PENDING_MOST: u64 = 512 << 20;

static DIR: OnceLock<PathBuf> = OnceLock::new();
static PENDING: AtomicU64 = AtomicU64::new(0);
/// Since the start: hits, their bytes, misses stored, their bytes, misses
/// not stored (queue full), entries evicted.
static STATS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// Keeps the cache in `dir` from now on (`texture::ledger::use_dir` calls
/// this with its `converted` folder).
pub(crate) fn use_dir(dir: PathBuf) {
    let _ = DIR.set(dir);
}

/// A digest of a request's source bytes, to check an entry against.
pub fn source_hash(bytes: &[u8]) -> u128 {
    super::digest(bytes).0
}

/// Whether outputs of `len` bytes are kept here at all.
pub fn wanted(len: usize) -> bool {
    DIR.get().is_some() && len >= SMALLEST
}

fn key_bytes(key: &Key) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&key.file.to_le_bytes());
    b[8..16].copy_from_slice(&key.offset.to_le_bytes());
    b[16..20].copy_from_slice(&key.source_size.to_le_bytes());
    b[20..24].copy_from_slice(&key.dest_size.to_le_bytes());
    b[24..32].copy_from_slice(&key.options.to_le_bytes());
    b
}

fn path_for(key: &Key) -> Option<PathBuf> {
    let hex = format!("{:032x}", super::digest(&key_bytes(key)).0);
    Some(DIR.get()?.join(&hex[..2]).join(&hex[2..]))
}

fn header(key: &Key, source: u128, len: u64) -> [u8; HEADER] {
    let mut h = [0u8; HEADER];
    h[..4].copy_from_slice(MAGIC);
    h[4..8].copy_from_slice(&VERSION.to_le_bytes());
    h[8..40].copy_from_slice(&key_bytes(key));
    h[40..56].copy_from_slice(&source.to_le_bytes());
    h[56..64].copy_from_slice(&len.to_le_bytes());
    h
}

/// A stored output, open for reading.
pub struct Hit {
    file: std::fs::File,
    len: u64,
}

impl Hit {
    /// The output's bytes.
    pub fn bytes(&self) -> usize {
        self.len as usize
    }

    /// Reads the output into `out` (exactly [`Hit::bytes`] bytes); false if
    /// the file is cut short (the caller then decodes as without a hit).
    pub fn read_into(mut self, out: &mut [u8]) -> bool {
        if out.len() != self.len as usize || self.file.read_exact(out).is_err() {
            return false;
        }
        // Used now: the last to go.
        let _ = self.file.set_modified(std::time::SystemTime::now());
        STATS[0].fetch_add(1, Relaxed);
        STATS[1].fetch_add(self.len, Relaxed);
        true
    }
}

/// The stored output of `key`, if there is one made from the same source
/// bytes.
pub fn open(key: &Key, source: u128) -> Option<Hit> {
    let path = path_for(key)?;
    let mut file = std::fs::OpenOptions::new().read(true).write(true).open(path).ok()?;
    let mut h = [0u8; HEADER];
    file.read_exact(&mut h).ok()?;
    let len = u64::from_le_bytes(h[56..64].try_into().ok()?);
    (h == header(key, source, len) && len <= u64::from(key.dest_size)).then_some(Hit { file, len })
}

/// Stores `bytes`, the output of `key` from source bytes of digest `source`
/// (on the writer thread; not stored while too much waits).
pub fn store(key: Key, source: u128, bytes: Vec<u8>) {
    let len = bytes.len() as u64;
    if DIR.get().is_none() || PENDING.load(Relaxed) + len > PENDING_MOST {
        STATS[4].fetch_add(1, Relaxed);
        return;
    }
    static STARTED: OnceLock<bool> = OnceLock::new();
    if !*STARTED.get_or_init(|| std::thread::Builder::new().name("converted cache".into()).spawn(writer).is_ok()) {
        return;
    }
    PENDING.fetch_add(len, Relaxed);
    let mut q = QUEUE.0.lock().unwrap_or_else(|e| e.into_inner());
    q.push_back((key, source, bytes));
    QUEUE.1.notify_one();
}

type Entry = (Key, u128, Vec<u8>);
static QUEUE: (Mutex<VecDeque<Entry>>, Condvar) = (Mutex::new(VecDeque::new()), Condvar::new());

fn writer() {
    let mut on_disk = scan().iter().map(|(_, _, n)| n).sum::<u64>();
    if on_disk > MOST_BYTES {
        on_disk = trim(on_disk);
    }
    tracing::info!(mb = on_disk >> 20, "converted cache: decoded DirectStorage outputs kept on disk");
    let mut since_line = std::time::Instant::now();
    loop {
        let (key, source, bytes) = {
            let mut q = QUEUE.0.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                match q.pop_front() {
                    Some(entry) => break entry,
                    None => q = QUEUE.1.wait(q).unwrap_or_else(|e| e.into_inner()),
                }
            }
        };
        let len = bytes.len() as u64;
        PENDING.fetch_sub(len.min(PENDING.load(Relaxed)), Relaxed);
        if write(&key, source, &bytes) {
            STATS[2].fetch_add(1, Relaxed);
            STATS[3].fetch_add(len, Relaxed);
            on_disk += len + HEADER as u64;
            if on_disk > MOST_BYTES {
                on_disk = trim(on_disk);
            }
        }
        if since_line.elapsed().as_secs() >= 30 {
            since_line = std::time::Instant::now();
            let s = |i: usize| STATS[i].load(Relaxed);
            tracing::info!(
                hits = s(0),
                hit_mb = s(1) >> 20,
                stored = s(2),
                stored_mb = s(3) >> 20,
                not_stored = s(4),
                evicted = s(5),
                on_disk_mb = on_disk >> 20,
                "converted cache: since the start"
            );
        }
    }
}

fn write(key: &Key, source: u128, bytes: &[u8]) -> bool {
    let Some(path) = path_for(key) else { return false };
    let Some(dir) = path.parent() else { return false };
    let tmp = dir.join(format!(".tmp-{}", std::process::id()));
    let done = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::File::create(&tmp))
        .and_then(|mut f| {
            f.write_all(&header(key, source, bytes.len() as u64))?;
            f.write_all(bytes)
        })
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = done {
        let _ = std::fs::remove_file(&tmp);
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| tracing::warn!(error = %e, "converted cache: an output was not stored (said once)"));
        return false;
    }
    true
}

/// Every entry: when last used, where, bytes.
fn scan() -> Vec<(std::time::SystemTime, PathBuf, u64)> {
    let Some(root) = DIR.get() else { return Vec::new() };
    let mut all = Vec::new();
    for dir in std::fs::read_dir(root).into_iter().flatten().flatten() {
        for file in std::fs::read_dir(dir.path()).into_iter().flatten().flatten() {
            if let Ok(meta) = file.metadata()
                && meta.is_file()
            {
                all.push((meta.modified().unwrap_or(std::time::UNIX_EPOCH), file.path(), meta.len()));
            }
        }
    }
    all
}

/// Lets the least recently used entries go until nine tenths of
/// [`MOST_BYTES`] are left; the bytes left.
fn trim(_counted: u64) -> u64 {
    let mut all = scan();
    let mut left: u64 = all.iter().map(|(_, _, n)| n).sum();
    all.sort_unstable_by_key(|(at, _, _)| *at);
    for (_, path, n) in all {
        if left <= MOST_BYTES / 10 * 9 {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            left -= n;
            STATS[5].fetch_add(1, Relaxed);
        }
    }
    left
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_names_its_request_and_source() {
        let key = Key { file: 3, offset: 4096, source_size: 100, dest_size: 400, options: 0x1e };
        let h = header(&key, 7, 400);
        assert_eq!(&h[..4], MAGIC);
        assert_ne!(h, header(&key, 8, 400));
        assert_ne!(h, header(&Key { offset: 0, ..key }, 7, 400));
    }
}
