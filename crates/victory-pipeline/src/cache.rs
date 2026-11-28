use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::Converter;

/// Identifies one conversion result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey(blake3::Hash);

impl CacheKey {
    pub fn new(converter: &dyn Converter, input: &[u8]) -> Self {
        let mut h = blake3::Hasher::new();
        h.update(converter.id().as_bytes());
        h.update(&[0]);
        h.update(&converter.version().to_le_bytes());
        h.update(input);
        CacheKey(h.finalize())
    }

    pub fn to_hex(&self) -> String {
        self.0.to_hex().to_string()
    }
}

/// Content-addressed store under the user's cache directory.
///
/// Cheap to clone (reference-counted inside). Within one process,
/// concurrent callers asking for the same key share one conversion: the
/// first thread does the work, the rest wait and read the published file.
/// Across processes two workers may still race; both outputs are
/// byte-identical (the key covers converter + input), so the publish
/// side treats a file already on disk as success rather than an error.
#[derive(Debug, Clone)]
pub struct Cache {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    root: PathBuf,
    // Per-key work slot. Threads converting the same key take the same
    // `Mutex` in turn: the first does the conversion, later arrivals see
    // the published file on their second check and skip `convert`.
    inflight: Mutex<HashMap<CacheKey, Arc<Mutex<()>>>>,
}

impl Cache {
    /// Opens (creating) a cache at `root`. Refuses locations inside the repo.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        victory_core::ensure_outside_repo(&root)?;
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(Cache {
            inner: Arc::new(Inner {
                root,
                inflight: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub fn path_for(&self, key: &CacheKey) -> PathBuf {
        let hex = key.to_hex();
        self.inner.root.join(&hex[..2]).join(&hex[2..])
    }

    /// Path of a cached entry, if one is on disk. No conversion is run.
    pub fn peek(&self, key: &CacheKey) -> Option<PathBuf> {
        let path = self.path_for(key);
        path.is_file().then_some(path)
    }

    /// Returns the cached output for `input`, converting it first if needed.
    ///
    /// Concurrent callers of this `Cache` with the same key share the
    /// work: only one `Converter::convert` runs, and the rest wait and
    /// read the published path.
    pub fn get_or_convert(&self, converter: &dyn Converter, input: &[u8]) -> Result<PathBuf> {
        let key = CacheKey::new(converter, input);
        let path = self.path_for(&key);
        if path.is_file() {
            tracing::trace!(converter = converter.id(), key = %key.to_hex(), "cache hit");
            return Ok(path);
        }

        // Single-flight: one worker per key inside this process.
        let slot = self.slot_for(key);
        let _work = lock(&slot);

        // Another thread may have published while we waited.
        if path.is_file() {
            drop(_work);
            self.release_slot(&key, slot);
            tracing::trace!(
                converter = converter.id(),
                key = %key.to_hex(),
                "cache hit after wait",
            );
            return Ok(path);
        }

        let result = (|| -> Result<PathBuf> {
            let output = converter.convert(input).with_context(|| {
                format!("converter {} v{}", converter.id(), converter.version())
            })?;
            let dir = path.parent().expect("hashed path has a parent");
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
            publish(&output, dir, &path)?;
            tracing::trace!(
                converter = converter.id(),
                key = %key.to_hex(),
                bytes = output.len(),
                "cache miss: converted and published",
            );
            Ok(path.clone())
        })();

        drop(_work);
        self.release_slot(&key, slot);
        result
    }

    fn slot_for(&self, key: CacheKey) -> Arc<Mutex<()>> {
        let mut guard = lock(&self.inner.inflight);
        guard
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn release_slot(&self, key: &CacheKey, slot: Arc<Mutex<()>>) {
        let mut guard = lock(&self.inner.inflight);
        // Drop the caller's clone first, then only remove the entry when
        // the map's Arc is the sole remaining one (no other waiter is
        // about to take this slot).
        drop(slot);
        if let Some(slot) = guard.get(key)
            && Arc::strong_count(slot) == 1
        {
            guard.remove(key);
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Writes `output` into `dir` under a temporary name and renames it over
/// `path`. If the rename fails but `path` is already a file (a sibling
/// process won the race with byte-identical content), it counts as success.
fn publish(output: &[u8], dir: &Path, path: &Path) -> Result<()> {
    let (tmp, mut file) =
        tempfile_in(dir).with_context(|| format!("staging in {}", dir.display()))?;
    let written = file.write_all(output);
    drop(file);
    let renamed = written.and_then(|()| std::fs::rename(&tmp, path));
    match renamed {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            if path.is_file() {
                // Another worker (another process, typically) published
                // the same content first. The key is content-addressed,
                // so by definition the bytes match.
                tracing::trace!(
                    path = %path.display(),
                    "publish lost race; accepting peer's entry",
                );
                Ok(())
            } else {
                Err(e).with_context(|| format!("publishing {}", path.display()))
            }
        }
    }
}

fn tempfile_in(dir: &Path) -> io::Result<(PathBuf, File)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    create_tempfile_in(dir, &COUNTER)
}

fn create_tempfile_in(dir: &Path, counter: &AtomicU64) -> io::Result<(PathBuf, File)> {
    // An interrupted writer can leave a file behind, and process IDs are
    // reused. Keep it intact and try a new name rather than failing a miss.
    for _ in 0..128 {
        let n = counter.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(".tmp-{}-{n}", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique cache staging file after 128 attempts",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Upper {
        version: u32,
        runs: AtomicUsize,
    }

    impl Converter for Upper {
        fn id(&self) -> &'static str {
            "test.upper"
        }
        fn version(&self) -> u32 {
            self.version
        }
        fn convert(&self, input: &[u8]) -> Result<Vec<u8>> {
            self.runs.fetch_add(1, Ordering::Relaxed);
            Ok(input.to_ascii_uppercase())
        }
    }

    #[test]
    fn converts_once_then_hits() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let a = cache.get_or_convert(&conv, b"abc").unwrap();
        let b = cache.get_or_convert(&conv, b"abc").unwrap();
        assert_eq!(a, b);
        assert_eq!(std::fs::read(&a).unwrap(), b"ABC");
        assert_eq!(conv.runs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn peek_finds_only_entries_that_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let key = CacheKey::new(&conv, b"abc");
        assert!(cache.peek(&key).is_none());
        cache.get_or_convert(&conv, b"abc").unwrap();
        assert_eq!(cache.peek(&key), Some(cache.path_for(&key)));
    }

    #[test]
    fn failed_publication_removes_the_staging_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let path = cache.path_for(&CacheKey::new(&conv, b"abc"));
        std::fs::create_dir_all(&path).unwrap();

        assert!(cache.get_or_convert(&conv, b"abc").is_err());
        assert!(path.is_dir());
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn concurrent_callers_share_one_conversion() {
        use std::thread;
        use std::time::Duration;

        struct Slow {
            runs: AtomicUsize,
        }
        impl Converter for Slow {
            fn id(&self) -> &'static str {
                "test.slow"
            }
            fn version(&self) -> u32 {
                1
            }
            fn convert(&self, input: &[u8]) -> Result<Vec<u8>> {
                // Sleep so later arrivals pile up on the slot behind us.
                thread::sleep(Duration::from_millis(80));
                self.runs.fetch_add(1, Ordering::Relaxed);
                Ok(input.to_vec())
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let slow = Arc::new(Slow {
            runs: AtomicUsize::new(0),
        });

        let threads: Vec<_> = (0..8)
            .map(|i| {
                let cache = cache.clone();
                let slow = Arc::clone(&slow);
                thread::spawn(move || {
                    // Stagger starts a hair so one arrives first.
                    if i > 0 {
                        thread::sleep(Duration::from_millis(5));
                    }
                    cache.get_or_convert(&*slow, b"payload").unwrap()
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(slow.runs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn inflight_map_cleans_up_after_use() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        cache.get_or_convert(&conv, b"abc").unwrap();
        assert!(lock(&cache.inner.inflight).is_empty());
        // Error path too (a directory at the target makes publish fail).
        let path = cache.path_for(&CacheKey::new(&conv, b"xyz"));
        std::fs::create_dir_all(&path).unwrap();
        assert!(cache.get_or_convert(&conv, b"xyz").is_err());
        assert!(lock(&cache.inner.inflight).is_empty());
    }

    #[test]
    fn publish_accepts_a_prewritten_entry_when_rename_fails() {
        // Make the rename fail (destination is a non-empty directory) and
        // then satisfy the race-recovery path by dropping a plain file at
        // the same path. The first call returns an error; the second,
        // after cleanup + peer-publish, succeeds.
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("blocked");
        std::fs::create_dir_all(&blocked).unwrap();
        std::fs::write(blocked.join("x"), b"x").unwrap();
        let err = publish(b"ABC", dir.path(), &blocked).unwrap_err();
        assert!(format!("{err:#}").contains("publishing"));

        // Peer publishes the same bytes first; our rename still fails
        // (destination is still a dir) but path.is_file() stays false,
        // so we expect an error. Replace the dir with a plain file to
        // exercise the recovery branch.
        std::fs::remove_dir_all(&blocked).unwrap();
        std::fs::write(&blocked, b"ABC").unwrap();
        // Hold a read handle open so a Windows rename-over would hit a
        // sharing violation; on Linux the rename simply succeeds.
        let _held = std::fs::OpenOptions::new().read(true).open(&blocked).unwrap();
        publish(b"ABC", dir.path(), &blocked).unwrap();
        assert_eq!(std::fs::read(&blocked).unwrap(), b"ABC");
    }

    #[test]
    fn staging_retries_collisions_without_changing_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let counter = AtomicU64::new(0);
        for n in 0..2 {
            std::fs::write(
                dir.path().join(format!(".tmp-{}-{n}", std::process::id())),
                b"stale",
            )
            .unwrap();
        }

        let (path, mut file) = create_tempfile_in(dir.path(), &counter).unwrap();
        file.write_all(b"fresh").unwrap();
        drop(file);
        assert_eq!(
            path.file_name().unwrap(),
            format!(".tmp-{}-2", std::process::id()).as_str()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"fresh");
        for n in 0..2 {
            assert_eq!(
                std::fs::read(dir.path().join(format!(".tmp-{}-{n}", std::process::id()))).unwrap(),
                b"stale"
            );
        }
        assert_eq!(counter.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn staging_collision_retries_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let counter = AtomicU64::new(0);
        for n in 0..128 {
            std::fs::write(
                dir.path().join(format!(".tmp-{}-{n}", std::process::id())),
                b"stale",
            )
            .unwrap();
        }

        let error = create_tempfile_in(dir.path(), &counter).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(counter.load(Ordering::Relaxed), 128);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 128);
    }

    #[test]
    fn staging_does_not_retry_other_io_errors() {
        let dir = tempfile::tempdir().unwrap();
        let counter = AtomicU64::new(0);
        let missing = dir.path().join("missing");

        let error = create_tempfile_in(&missing, &counter).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        assert!(!missing.exists());
    }

    #[test]
    fn version_bump_invalidates() {
        let v1 = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let v2 = Upper {
            version: 2,
            runs: AtomicUsize::new(0),
        };
        assert_ne!(CacheKey::new(&v1, b"x"), CacheKey::new(&v2, b"x"));
    }

    #[test]
    fn refuses_cache_inside_repo() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("cache");
        assert!(Cache::open(inside).is_err());
    }
}
