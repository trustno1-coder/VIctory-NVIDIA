//! Remember which MiB blocks DirectStorage read. The offline preloader can then
//! prime the same buffered file reads without copying the game's archives to disk.

use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::windows::{fs::MetadataExt, io::AsRawHandle};
use std::path::{Path, PathBuf};
use std::sync::{
    OnceLock,
    atomic::{AtomicU8, Ordering},
};
use windows_sys::Win32::{Foundation::CloseHandle, System::Memory::*};

const BLOCK: u64 = 1 << 20;
static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();

pub fn record_in(dir: PathBuf) {
    let _ = DIRECTORY.set(dir);
}

fn map_path(dir: &Path, name: &str, meta: &Metadata) -> PathBuf {
    dir.join(format!(
        "{}.{}.{}.blocks",
        name.to_ascii_lowercase(),
        meta.len(),
        meta.last_write_time()
    ))
}

pub(super) fn track(name: &str, file: &File) -> Option<ReadMap> {
    let dir = DIRECTORY.get()?;
    let (drive, name) = name.split_at_checked(3)?;
    if !drive.eq_ignore_ascii_case("G:\\") && !drive.eq_ignore_ascii_case("G:/") {
        return None;
    }
    if name.contains(['/', '\\', ':']) || !name.to_ascii_lowercase().ends_with(".rpf") {
        return None;
    }
    let meta = file.metadata().ok()?;
    ReadMap::open(&map_path(dir, name, &meta), meta.len().div_ceil(BLOCK))
}

pub(super) struct ReadMap {
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
}

// SAFETY: the view stays mapped for the lifetime of ReadMap. All workers access
// only atomic bytes, and the map can only transition a block from unread to read.
unsafe impl Send for ReadMap {}
unsafe impl Sync for ReadMap {}

impl ReadMap {
    fn open(path: &Path, blocks: u64) -> Option<Self> {
        let len = u32::try_from(blocks).ok().filter(|&n| n != 0)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .ok()?;
        // SAFETY: Windows extends the file to len if necessary. The view holds
        // the mapping after its creation handle closes, including between runs.
        let view = unsafe {
            let mapping = CreateFileMappingW(
                file.as_raw_handle(),
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                len,
                std::ptr::null(),
            );
            if mapping.is_null() {
                return None;
            }
            let view = MapViewOfFile(mapping, FILE_MAP_WRITE, 0, 0, len as usize);
            CloseHandle(mapping);
            view
        };
        (!view.Value.is_null()).then(|| Self {
            view,
            len: len as usize,
        })
    }

    pub(super) fn note(&self, offset: u64, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let end = offset
            .saturating_add(bytes as u64)
            .div_ceil(BLOCK)
            .min(self.len as u64);
        for index in offset / BLOCK..end {
            // SAFETY: index is inside this writable, byte-aligned mapping.
            let flag = unsafe { &*self.view.Value.cast::<AtomicU8>().add(index as usize) };
            if flag.load(Ordering::Relaxed) == 0 {
                flag.store(1, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for ReadMap {
    fn drop(&mut self) {
        // SAFETY: this is the view created by open and no reference survives its owner.
        unsafe { UnmapViewOfFile(self.view) };
    }
}

struct Archive {
    file: File,
    len: u64,
    // Header, previously streamed blocks, and shared assets for first-time players.
    groups: [Vec<usize>; 3],
}

fn recipe(len: u64, marks: &[u8], shared: bool) -> [Vec<usize>; 3] {
    let mut groups = [Vec::new(), Vec::new(), Vec::new()];
    for n in 0..len.div_ceil(BLOCK) as usize {
        if n == 0 {
            groups[0].push(n);
        } else if marks.get(n) == Some(&1) {
            groups[1].push(n);
        } else if shared {
            groups[2].push(n);
        }
    }
    groups
}

pub fn warm(root: &Path, cache: &Path) -> io::Result<()> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut memory = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: the size is set and the output is writable.
    if unsafe { GlobalMemoryStatusEx(&mut memory) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let limit = (memory.ullAvailPhys / 4).min(2048 * BLOCK);
    let mut paths = Vec::new();
    for item in std::fs::read_dir(root)? {
        let item = item?;
        let path = item.path();
        if item.file_type()?.is_file()
            && path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("rpf"))
        {
            paths.push(path);
        }
    }
    paths.sort_unstable();
    let mut archives = Vec::new();
    for path in paths {
        let file = File::open(&path)?;
        let meta = file.metadata()?;
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase();
        let mut marks = Vec::new();
        if let Ok(map) = File::open(map_path(cache, &name, &meta)) {
            map.take(meta.len().div_ceil(BLOCK))
                .read_to_end(&mut marks)?;
        }
        let shared = matches!(
            name.as_str(),
            "appdata0.rpf" | "common.rpf" | "script.rpf" | "data.rpf" | "textures.rpf"
        );
        archives.push(Archive {
            file,
            len: meta.len(),
            groups: recipe(meta.len(), &marks, shared),
        });
    }
    let mut buf = vec![0; BLOCK as usize];
    let mut bytes = [0; 3];
    let mut left = limit;
    for group in 0..3 {
        // Round robin across archives so a large level cannot consume the whole allowance.
        let count = archives
            .iter()
            .map(|a| a.groups[group].len())
            .max()
            .unwrap_or(0);
        for i in 0..count {
            for a in &mut archives {
                let Some(&n) = a.groups[group].get(i) else {
                    continue;
                };
                let offset = n as u64 * BLOCK;
                let len = (a.len - offset).min(BLOCK);
                if len > left {
                    continue;
                }
                a.file.seek(SeekFrom::Start(offset))?;
                a.file.read_exact(&mut buf[..len as usize])?;
                left -= len;
                bytes[group] += len;
            }
            if left == 0 {
                break;
            }
        }
    }
    println!(
        "Asset warm-up: {} MiB from {} archives ({} MiB previously streamed, {} MiB shared assets, plus archive headers). Windows can reclaim these pages.",
        bytes.iter().sum::<u64>() / BLOCK,
        archives.len(),
        bytes[1] / BLOCK,
        bytes[2] / BLOCK
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_reads_cross_boundaries_and_warm_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("range-map");
        let map = ReadMap::open(&path, 6).unwrap();
        map.note(2 * BLOCK - 1, 2);
        map.note(u64::MAX, 4);
        map.note(4 * BLOCK, 0);
        drop(map);
        let marks = std::fs::read(path).unwrap();
        assert_eq!(marks, [0, 1, 1, 0, 0, 0]);
        assert_eq!(
            recipe(5 * BLOCK + 1, &marks, true),
            [vec![0], vec![1, 2], vec![3, 4, 5]]
        );
        assert_eq!(recipe(5 * BLOCK + 1, &marks, false)[2], []);
    }
}
