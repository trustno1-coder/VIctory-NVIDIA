//! Portable publication of derived files into a shared cache directory.
//!
//! A file is published as `<dir>/<key>/<name>`, where the key is the BLAKE3
//! of its bytes: a start that finds that file, of that length and with the
//! modified time we stamp on it, uses it without reading it (it held 115 MB
//! for the title, read and compared whole at every start). The bytes hashed
//! are the patched ones, so a change to the patches, ours or a switch's,
//! publishes beside the old copy, never takes it for its own. The name
//! itself stays the same (`victory-image-<build>.dll`): the module's name, by
//! which logs, profiles and `VICTORY_WRITE_SITES` know it.
//!
//! Several copies of the runner can prepare the same title at once. Each
//! writer stages into its own file, so two of them cannot truncate each
//! other's bytes or rename a file the other already published. The final
//! rename is atomic within the directory, and the staging file is removed
//! if publication fails. Copies of `name` under other keys (older patches)
//! are removed once they are a minute old and nothing has them mapped.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

static NEXT_STAGING: AtomicU64 = AtomicU64::new(0);

/// The modified time a published file is given: a file of that length and
/// that time under its key is taken as published whole by us. Written over
/// since (damaged, truncated, edited), it has another time, and is replaced.
/// (A whole second: exact on every file system's clock, FAT's too.)
const STAMP: Duration = Duration::from_secs(1_000_000_000);

/// How old a copy under another key must be before it is removed: one just
/// published by another runner (other patches of this build) may be about
/// to be mapped. One mapped already cannot be removed on Windows.
const STALE_AFTER: Duration = Duration::from_secs(60);

/// Publishes `bytes` under `name` in `dir` (see the module's comment) unless
/// they are there already; the file's path.
pub fn publish(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    publish_with(dir, name, bytes, STALE_AFTER).map(|(path, _)| path)
}

/// [`publish`], with the age at which other keys' copies go; and whether the
/// bytes were written.
fn publish_with(
    dir: &Path,
    name: &str,
    bytes: &[u8],
    stale_after: Duration,
) -> std::io::Result<(PathBuf, bool)> {
    let key = key(bytes);
    let path = dir.join(&key).join(name);
    let written = !is_published(&path, bytes.len());
    if written {
        write_published(&path, bytes)?;
    }
    remove_stale(dir, &key, name, stale_after);
    Ok((path, written))
}

/// The folder name for `bytes`: 128 bits of their BLAKE3, in hexadecimal.
fn key(bytes: &[u8]) -> String {
    crate::hash::hash(bytes).to_hex()[..KEY_LEN].to_owned()
}

const KEY_LEN: usize = 32;

fn stamp() -> SystemTime {
    SystemTime::UNIX_EPOCH + STAMP
}

/// The file at `path` is `len` bytes long and has not been written since we
/// published it.
fn is_published(path: &Path, len: usize) -> bool {
    std::fs::metadata(path).is_ok_and(|m| {
        m.is_file() && m.len() == len as u64 && m.modified().is_ok_and(|t| t == stamp())
    })
}

fn write_published(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let folder = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(folder)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image");
    let sequence = NEXT_STAGING.fetch_add(1, Ordering::Relaxed);
    let staging = folder.join(format!(".{name}.{}.{sequence}.tmp", std::process::id()));
    // On the disk before the rename, the stamp put on before the flush: a
    // crash in between left the name on a file of zeros, or of nothing, under
    // the image's name in place of the whole one.
    let staged = (|| {
        let mut file = std::fs::File::create(&staging)?;
        file.write_all(bytes)?;
        file.set_modified(stamp())?;
        file.sync_all()
    })();
    if let Err(error) = staged.and_then(|()| std::fs::rename(&staging, path)) {
        let _ = std::fs::remove_file(&staging);
        // Another runner published the same bytes first, and has them mapped:
        // Windows refuses a rename over a mapped image.
        if is_published(path, bytes.len()) {
            return Ok(());
        }
        return Err(error);
    }
    Ok(())
}

/// Removes `name` from the other keys' folders in `dir` (and the folders,
/// once empty), and the copy straight in `dir` that runners before the keys
/// wrote. Best effort: what is mapped, or younger than `stale_after`, stays.
fn remove_stale(dir: &Path, key: &str, name: &str, stale_after: Duration) {
    let old = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|m| m.created())
            .is_ok_and(|created| created.elapsed().is_ok_and(|age| age >= stale_after))
    };
    let legacy = dir.join(name);
    if old(&legacy) {
        let _ = std::fs::remove_file(&legacy);
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let folder = entry.file_name();
        let Some(folder) = folder.to_str() else {
            continue;
        };
        let is_key = folder.len() == KEY_LEN && folder.bytes().all(|b| b.is_ascii_hexdigit());
        if !is_key || folder == key || !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let copy = entry.path().join(name);
        if old(&copy) && std::fs::remove_file(&copy).is_ok() {
            // Fails while another build's file or a staging file is in it.
            let _ = std::fs::remove_dir(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files_in(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap().filter_map(Result::ok) {
            if entry.file_type().unwrap().is_dir() {
                out.extend(files_in(&entry.path()));
            } else {
                out.push(entry.path());
            }
        }
        out
    }

    /// Two publishers staging the same destination at once both succeed,
    /// and neither leaves a staging file behind.
    #[test]
    fn concurrent_publishers_do_not_share_a_staging_file() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"a prepared image".to_vec();
        let publishers: Vec<_> = (0..2)
            .map(|_| {
                let (dir, bytes) = (dir.path().to_path_buf(), bytes.clone());
                std::thread::spawn(move || publish(&dir, "image.bin", &bytes))
            })
            .collect();
        let paths: Vec<PathBuf> = publishers
            .into_iter()
            .map(|p| p.join().unwrap().unwrap())
            .collect();
        assert_eq!(paths[0], paths[1]);
        assert_eq!(std::fs::read(&paths[0]).unwrap(), bytes);
        assert_eq!(files_in(dir.path()), vec![paths[0].clone()], "staging files were removed");
    }

    #[test]
    fn a_failed_publication_removes_its_staging_file() {
        let dir = tempfile::tempdir().unwrap();
        // A file where the key's folder would go.
        std::fs::write(dir.path().join("blocker"), b"").unwrap();
        assert!(publish(&dir.path().join("blocker"), "image.bin", b"bytes").is_err());
        assert_eq!(files_in(dir.path()), vec![dir.path().join("blocker")]);
    }

    /// Published once, the same bytes are found and not written again: the
    /// file is the one written the first time.
    #[test]
    fn an_unchanged_image_is_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let publish = |bytes: &[u8]| publish_with(dir.path(), "image.bin", bytes, STALE_AFTER).unwrap();
        let (path, written) = publish(b"the patched image");
        assert!(written);
        assert_eq!(publish(b"the patched image"), (path.clone(), false));
        assert_eq!(std::fs::read(&path).unwrap(), b"the patched image");
        assert_eq!(files_in(dir.path()), vec![path]);
    }

    /// Other bytes under the same name (a patch changed) go to their own key,
    /// and the old copy, once old enough, is removed with its folder.
    #[test]
    fn changed_bytes_publish_a_new_image_and_remove_the_old() {
        let dir = tempfile::tempdir().unwrap();
        let first = publish(dir.path(), "image.bin", b"patched one way").unwrap();
        let young = publish(dir.path(), "image.bin", b"patched another way").unwrap();
        assert_ne!(first, young);
        assert_eq!(std::fs::read(&young).unwrap(), b"patched another way");
        assert!(first.is_file(), "a copy just published stays for a while");
        let (second, written) =
            publish_with(dir.path(), "image.bin", b"patched another way", Duration::ZERO).unwrap();
        assert_eq!((&second, written), (&young, false));
        assert!(!first.parent().unwrap().exists());
        assert_eq!(files_in(dir.path()), vec![second]);
    }

    /// Only the same name is removed from other keys' folders: another
    /// build's image in the same cache stays, and so does what is not a key.
    #[test]
    fn other_names_and_folders_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let other = publish(dir.path(), "other-build.bin", b"another build").unwrap();
        let variant = dir.path().join("native-1440p-sdr").join("image.bin");
        std::fs::create_dir_all(variant.parent().unwrap()).unwrap();
        std::fs::write(&variant, b"x").unwrap();
        let legacy = dir.path().join("image.bin");
        std::fs::write(&legacy, b"from before the keys").unwrap();
        let (path, _) = publish_with(dir.path(), "image.bin", b"this build", Duration::ZERO).unwrap();
        assert!(other.is_file() && variant.is_file() && path.is_file());
        assert!(!legacy.exists(), "the copy from before the keys is removed");
    }

    /// An old copy another runner has mapped stays (Windows refuses to delete
    /// a mapped image), and goes at the first publication after it is let go.
    #[cfg(windows)]
    #[test]
    fn a_mapped_old_copy_is_not_removed() {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::FreeLibrary;
        use windows_sys::Win32::System::LibraryLoader::{LOAD_LIBRARY_AS_IMAGE_RESOURCE, LoadLibraryExW};
        let dir = tempfile::tempdir().unwrap();
        let dll = std::fs::read(r"C:\Windows\System32\version.dll").unwrap();
        let old = publish(dir.path(), "image.dll", &dll).unwrap();
        let wide: Vec<u16> = old.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: a NUL-terminated path; mapped as an image, nothing of it runs.
        let module = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_AS_IMAGE_RESOURCE) };
        assert!(!module.is_null(), "{}", std::io::Error::last_os_error());
        publish_with(dir.path(), "image.dll", b"other patches", Duration::ZERO).unwrap();
        assert!(old.is_file(), "a mapped copy stays");
        // SAFETY: loaded above, used by nothing else.
        unsafe { FreeLibrary(module) };
        publish_with(dir.path(), "image.dll", b"other patches", Duration::ZERO).unwrap();
        assert!(!old.exists());
    }

    /// A published file written over (same length, other bytes) or cut short
    /// is noticed by its time or length, and published again.
    #[test]
    fn a_damaged_or_truncated_image_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"the whole patched image".to_vec();
        let path = publish(dir.path(), "image.bin", &bytes).unwrap();

        let republish = || publish_with(dir.path(), "image.bin", &bytes, STALE_AFTER).unwrap();
        std::fs::write(&path, vec![0; bytes.len()]).unwrap();
        assert!(!is_published(&path, bytes.len()));
        assert_eq!(republish(), (path.clone(), true));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);

        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(5).unwrap();
        // Even with the stamp put back: the length gives it away.
        file.set_modified(stamp()).unwrap();
        drop(file);
        assert_eq!(republish(), (path.clone(), true));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(republish(), (path, false));
    }

    /// The publish step at a start, on a title-sized image (115 MB) published
    /// by the start before: the time and the process's peak memory. `cargo
    /// test -p victory-loader --release -- --ignored --exact
    /// publish::tests::bench_publish_step --nocapture`
    #[test]
    #[ignore = "a benchmark: 115 MB on the disk"]
    fn bench_publish_step() {
        let dir = std::env::temp_dir().join("victory-publish-bench");
        let exe = dir.join("title.exe");
        std::fs::create_dir_all(&dir).unwrap();
        if !exe.is_file() {
            let mut bytes = std::fs::read(std::env::current_exe().unwrap()).unwrap();
            let mut x = 0x9e37_79b9_7f4a_7c15_u64;
            while bytes.len() < 115 << 20 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                bytes.extend_from_slice(&x.to_le_bytes());
            }
            std::fs::write(&exe, bytes).unwrap();
        }
        let images = dir.join("images");
        let start = || {
            let original = std::fs::read(&exe).unwrap();
            let prepared = crate::prepare_owned(original).unwrap();
            publish(&images, &prepared.file_name(), &prepared.bytes).unwrap();
        };
        start();
        let mut times: Vec<f64> = (0..9)
            .map(|_| {
                let at = std::time::Instant::now();
                start();
                at.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        times.sort_by(f64::total_cmp);
        println!("publish step: median {:.1} ms, {times:.1?}", times[times.len() / 2]);
        #[cfg(windows)]
        println!("peak private memory: {} MB", peak_private_bytes() >> 20);
    }

    #[cfg(windows)]
    fn peak_private_bytes() -> usize {
        use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: plain data, filled by the call.
        let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        // SAFETY: the pseudo handle of this process, a buffer of the size given.
        unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
        counters.PeakPagefileUsage
    }
}
