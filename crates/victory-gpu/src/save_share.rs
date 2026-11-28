//! Sharing game saves through the F1 menu (EXPORT SAVE, IMPORT SAVE).
//!
//! A save is the title's data folder's `saves` (the game's save containers)
//! and `storage` (its persistent local storage), under `<data>\<title id>`.
//! Export writes them into one zip file in `Saves` beside the game's
//! executable, to send as it is; another player puts that zip into their own
//! `Saves` and picks it in the menu's list. An import is staged
//! (`import-pending` in the data folder) and put in place at the next start,
//! before the title runs: the running game holds its saves and would write
//! over an import with its next autosave. The save it replaces is zipped into
//! `Saves` first ("backup before import ..."), so nothing is lost.
//!
//! A zip zipped again by hand (Windows' "Compress to ZIP file", with the
//! export's folder around it) imports too, and so does a save folder put in
//! `Saves` as it is (as the first test build exported them).

use std::io;
use std::path::{Path, PathBuf};

/// The folders of `<data>\<title id>` that make a save.
pub const PARTS: [&str; 2] = ["saves", "storage"];
/// An import must have this part.
const NEEDED: &str = "saves";
/// Where an import waits for the next start, in `<data>\<title id>`.
const PENDING: &str = "import-pending";
/// Where an import is staged before it waits ([`stage`]): renamed to
/// [`PENDING`] once all of it is on the disk.
const STAGING: &str = "import-pending.part";
/// Added to a part's name while an import takes its place ([`apply`]).
const ASIDE: &str = ".replaced";
/// In [`PENDING`] once an apply has begun to move parts: the backup's path
/// (empty: none), for the apply that finishes it.
const APPLYING: &str = "applying";
/// Backups go here when the game folder cannot be written.
const BACKUPS: &str = "save-backups";
/// The game keeps its own settings (picture, controls, sound) in a save container of this name
/// (`SettingsProfileSettings~<id>`): never exported nor imported, and the player's own is kept
/// when an import replaces the save (an import overwrote the player's settings, 2026-11-28).
const SETTINGS: &str = "SettingsProfileSettings";

fn is_settings(name: &str) -> bool {
    name.starts_with(SETTINGS)
}

/// Inside `saves`: the folder of the one player (`{:016X}` of the HLE's XUID), then the
/// title's (`<scid>`), then the save containers (`<name>~<16 hex digits>`). Zips shared by
/// hand often start at one of these instead of at `saves` (one did, 2026-11-28).
const USER: &str = "0009000000000001";
const SCID: &str = "00000000-0000-0000-0000-00007952987a";

fn is_container(name: &str) -> bool {
    name.split_once('~').is_some_and(|(kind, id)| {
        !kind.is_empty() && id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

/// Where a zip that starts inside `saves` puts the entry `parts`, if it does.
fn inside_saves(parts: &[&str]) -> Option<PathBuf> {
    let found = |is: &dyn Fn(&str) -> bool| parts.iter().take(3).position(|p| is(p));
    let mut to = PathBuf::from(NEEDED);
    let at = if let Some(at) = found(&|p| p.eq_ignore_ascii_case(USER)) {
        at
    } else if let Some(at) = found(&|p| p.eq_ignore_ascii_case(SCID)) {
        to.push(USER);
        at
    } else {
        let at = found(&is_container)?;
        to.push(USER);
        to.push(SCID);
        at
    };
    Some(to.join(parts[at..].iter().collect::<PathBuf>()))
}

fn env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// `<data>\<title id>`: where the running title's saves are (`VICTORY_SAVE_DATA`, set by [`start`]).
pub fn data() -> Option<PathBuf> {
    env("VICTORY_SAVE_DATA")
}

/// `Saves` beside the game's executable (`VICTORY_SAVE_SHARE`, set by [`start`]).
pub fn share() -> Option<PathBuf> {
    env("VICTORY_SAVE_SHARE")
}

fn now() -> windows_sys::Win32::Foundation::SYSTEMTIME {
    let mut t: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    // SAFETY: fills a plain structure of ours.
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
    t
}

/// The local time as a file name can hold it: `2026-11-27 23-45-10`.
pub fn stamp() -> String {
    let t = now();
    format!("{:04}-{:02}-{:02} {:02}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

fn bad(text: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, text.into())
}

/// Zip files: written with deflate (stored where that is no smaller), read
/// stored or deflated. No zip64, no encryption: a save is a few megabytes.
mod zip {
    use super::bad;
    use std::io;

    /// The most a zip may unpack to (all its files together).
    pub const MOST: u64 = 1 << 30;
    const LOCAL: u32 = 0x0403_4b50;
    const CENTRAL: u32 = 0x0201_4b50;
    const END: u32 = 0x0605_4b50;

    /// `(name, bytes)`; a name ending in `/` is a folder.
    pub type Entry = (String, Vec<u8>);

    fn deflate(data: &[u8]) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; zlib_rs::compress_bound(data.len())];
        let config = zlib_rs::DeflateConfig { window_bits: -15, ..zlib_rs::DeflateConfig::new(6) };
        let (packed, rc) = zlib_rs::compress_slice(&mut buf, data, config);
        (rc == zlib_rs::ReturnCode::Ok && packed.len() < data.len()).then(|| packed.to_vec())
    }

    fn inflate(packed: &[u8], size: usize) -> Option<Vec<u8>> {
        if size == 0 {
            return Some(Vec::new());
        }
        let mut out = vec![0u8; size];
        let (got, rc) = zlib_rs::decompress_slice(&mut out, packed, zlib_rs::InflateConfig { window_bits: -15 });
        let n = got.len();
        (rc == zlib_rs::ReturnCode::Ok && n == size).then_some(out)
    }

    /// The zip of `entries`, dated `(time, date)` (MS-DOS form).
    pub fn write(entries: &[Entry], (time, date): (u16, u16)) -> io::Result<Vec<u8>> {
        if entries.len() >= 0xffff {
            return Err(bad("too many files for a zip"));
        }
        let (mut out, mut central) = (Vec::new(), Vec::new());
        for (name, data) in entries {
            let packed = if name.ends_with('/') { None } else { deflate(data) };
            let (method, body): (u16, &[u8]) = match &packed {
                Some(p) => (8, p),
                None => (0, data),
            };
            let offset = u32::try_from(out.len()).map_err(|_| bad("zip over 4 GB"))?;
            let crc = zlib_rs::crc32::crc32(0, data);
            let head = |sig: u32| {
                let mut h = Vec::with_capacity(46 + name.len());
                h.extend_from_slice(&sig.to_le_bytes());
                if sig == CENTRAL {
                    h.extend_from_slice(&20u16.to_le_bytes()); // made by: MS-DOS, 2.0
                }
                for v in [20u16, 0x0800, method, time, date] {
                    h.extend_from_slice(&v.to_le_bytes()); // 0x0800: the name is UTF-8
                }
                for v in [crc, body.len() as u32, data.len() as u32] {
                    h.extend_from_slice(&v.to_le_bytes());
                }
                h.extend_from_slice(&(name.len() as u16).to_le_bytes());
                h.extend_from_slice(&0u16.to_le_bytes()); // extra
                if sig == CENTRAL {
                    let attributes: u32 = if name.ends_with('/') { 0x10 } else { 0x20 };
                    for v in [0u16, 0, 0] {
                        h.extend_from_slice(&v.to_le_bytes()); // comment, disk, internal
                    }
                    h.extend_from_slice(&attributes.to_le_bytes());
                    h.extend_from_slice(&offset.to_le_bytes());
                }
                h.extend_from_slice(name.as_bytes());
                h
            };
            out.extend_from_slice(&head(LOCAL));
            out.extend_from_slice(body);
            central.extend_from_slice(&head(CENTRAL));
        }
        let at = u32::try_from(out.len()).map_err(|_| bad("zip over 4 GB"))?;
        out.extend_from_slice(&central);
        out.extend_from_slice(&END.to_le_bytes());
        for v in [0u16, 0, entries.len() as u16, entries.len() as u16] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&at.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        Ok(out)
    }

    fn u16_at(b: &[u8], at: usize) -> io::Result<u16> {
        b.get(at..at + 2).map(|s| u16::from_le_bytes([s[0], s[1]])).ok_or_else(|| bad("the zip is cut short"))
    }

    fn u32_at(b: &[u8], at: usize) -> io::Result<u32> {
        b.get(at..at + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).ok_or_else(|| bad("the zip is cut short"))
    }

    /// The entries of the zip `b`, unpacked and checked.
    pub fn read(b: &[u8]) -> io::Result<Vec<Entry>> {
        let lowest = b.len().saturating_sub(22 + 0xffff);
        let end = (lowest..=b.len().saturating_sub(22))
            .rev()
            .find(|&i| u32_at(b, i).is_ok_and(|s| s == END))
            .ok_or_else(|| bad("not a zip file"))?;
        let count = u16_at(b, end + 10)? as usize;
        let mut at = u32_at(b, end + 16)? as usize;
        let (mut out, mut total) = (Vec::with_capacity(count), 0u64);
        for _ in 0..count {
            if u32_at(b, at)? != CENTRAL {
                return Err(bad("the zip's list of files is damaged"));
            }
            let (flags, method) = (u16_at(b, at + 8)?, u16_at(b, at + 10)?);
            let crc = u32_at(b, at + 16)?;
            let (packed, size) = (u32_at(b, at + 20)? as usize, u32_at(b, at + 24)? as usize);
            let (name_len, extra, comment) = (u16_at(b, at + 28)? as usize, u16_at(b, at + 30)? as usize, u16_at(b, at + 32)? as usize);
            let local = u32_at(b, at + 42)? as usize;
            let name = b.get(at + 46..at + 46 + name_len).ok_or_else(|| bad("the zip is cut short"))?;
            let name = String::from_utf8_lossy(name).replace('\\', "/");
            at += 46 + name_len + extra + comment;
            if flags & 1 != 0 {
                return Err(bad("the zip has a password"));
            }
            if packed == 0xffff_ffff || size == 0xffff_ffff {
                return Err(bad("zip64 files are not read"));
            }
            total += size as u64;
            if total > MOST {
                return Err(bad("the zip unpacks to more than a save can be"));
            }
            if u32_at(b, local)? != LOCAL {
                return Err(bad("the zip is damaged"));
            }
            let start = local + 30 + u16_at(b, local + 26)? as usize + u16_at(b, local + 28)? as usize;
            let body = b.get(start..start + packed).ok_or_else(|| bad("the zip is cut short"))?;
            let data = match method {
                0 if packed == size => body.to_vec(),
                8 => inflate(body, size).ok_or_else(|| bad(format!("{name}: damaged in the zip")))?,
                _ => return Err(bad(format!("{name}: packed in a way that is not read (method {method})"))),
            };
            if zlib_rs::crc32::crc32(0, &data) != crc {
                return Err(bad(format!("{name}: damaged in the zip")));
            }
            out.push((name, data));
        }
        Ok(out)
    }
}

/// `(time, date)` now, as a zip holds them.
fn dos_now() -> (u16, u16) {
    let t = now();
    let time = (t.wHour << 11) | (t.wMinute << 5) | (t.wSecond / 2);
    let date = (t.wYear.saturating_sub(1980) << 9) | (t.wMonth << 5) | t.wDay;
    (time, date)
}

/// Every folder and file in `dir`, named `prefix/...` with `/` between parts.
fn gather(dir: &Path, prefix: &str, out: &mut Vec<zip::Entry>) -> io::Result<()> {
    out.push((format!("{prefix}/"), Vec::new()));
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<io::Result<_>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = format!("{prefix}/{}", entry.file_name().to_string_lossy());
        if entry.file_type()?.is_dir() && is_settings(&entry.file_name().to_string_lossy()) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            gather(&entry.path(), &name, out)?;
        } else {
            out.push((name, std::fs::read(entry.path())?));
        }
    }
    Ok(())
}

/// Where an entry of a zip goes in `<data>\<title id>`: `saves/...` or
/// `storage/...`, with at most one folder around them. `None`: not part of
/// a save (a readme, say). A name that could reach out of the folder is refused.
fn place(name: &str) -> io::Result<Option<PathBuf>> {
    let parts: Vec<&str> = name.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    for p in &parts {
        if *p == ".." || p.chars().any(|c| c.is_control() || r#"<>:"|?*"#.contains(c)) || p.ends_with([' ', '.']) {
            return Err(bad(format!("{name}: not a name a save has")));
        }
    }
    // (The game's settings, in zips made before they were left out: the player keeps theirs.)
    if parts.iter().any(|p| is_settings(p)) {
        return Ok(None);
    }
    match parts.iter().take(2).position(|p| PARTS.iter().any(|k| p.eq_ignore_ascii_case(k))) {
        Some(at) => Ok(Some(parts[at..].iter().collect::<PathBuf>())),
        None => Ok(inside_saves(&parts)),
    }
}

/// Writes the save in `data` into a new zip `<name>.zip` in `share`. The zip.
pub fn export(data: &Path, share: &Path, name: &str) -> io::Result<PathBuf> {
    if !data.join(NEEDED).is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no save yet"));
    }
    let to = share.join(format!("{name}.zip"));
    if to.exists() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} exists", to.display())));
    }
    let mut entries = Vec::new();
    for part in PARTS {
        let from = data.join(part);
        if from.is_dir() {
            gather(&from, part, &mut entries)?;
        }
    }
    let bytes = zip::write(&entries, dos_now())?;
    std::fs::create_dir_all(share)?;
    // Written whole under another name first: a zip is never seen half written.
    let part = share.join(format!("{name}.zip.part"));
    std::fs::write(&part, bytes)?;
    std::fs::rename(&part, &to).inspect_err(|_| drop(std::fs::remove_file(&part)))?;
    Ok(to)
}

/// What `share` holds to import, newest first: its zip files, and save
/// folders (with a `saves` folder) put there as they are.
pub fn exports(share: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(share) else { return Vec::new() };
    let mut found: Vec<(std::time::SystemTime, String)> = entries
        .flatten()
        .filter(|e| {
            let path = e.path();
            if path.is_dir() {
                path.join(NEEDED).is_dir()
            } else {
                path.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip"))
            }
        })
        .filter_map(|e| {
            let when = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            Some((when, e.file_name().into_string().ok()?))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    found.into_iter().map(|(_, name)| name).collect()
}

/// Copies the folder `from` into `to` (made), with everything in it (`settings`: also the
/// game's settings containers).
fn copy_dir(from: &Path, to: &Path, settings: bool) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            if !settings && is_settings(&entry.file_name().to_string_lossy()) {
                continue;
            }
            copy_dir(&entry.path(), &target, settings)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
            victory_core::fs::sync(&target)?;
        }
    }
    Ok(())
}

/// Unpacks the save in the zip `bytes` into `to`. Nothing is written unless the whole zip reads.
fn unzip(bytes: &[u8], to: &Path) -> io::Result<()> {
    let mut placed = Vec::new();
    for (name, data) in zip::read(bytes)? {
        if let Some(at) = place(&name)? {
            placed.push((name.ends_with('/'), at, data));
        }
    }
    if !placed.iter().any(|(_, at, _)| at.starts_with(NEEDED)) {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no save in that zip (no saves folder)"));
    }
    for (folder, at, data) in placed {
        let path = to.join(at);
        if folder {
            std::fs::create_dir_all(&path)?;
        } else {
            if let Some(up) = path.parent() {
                std::fs::create_dir_all(up)?;
            }
            victory_core::fs::write_synced(&path, &data)?;
            crash_point("stage:file");
        }
    }
    Ok(())
}

/// Stages the save `name` of `share` (a zip, or a save folder) to be put in
/// place at the next start ([`start`]). It is written whole, and on the disk,
/// under another name first: an import cut short (the game closed, a crash)
/// waited half written, and the next start put the half in place.
pub fn stage(data: &Path, share: &Path, name: &str) -> io::Result<()> {
    let from = share.join(name);
    let pending = data.join(PENDING);
    let staging = data.join(STAGING);
    for old in [&pending, &staging] {
        if old.exists() {
            std::fs::remove_dir_all(old)?;
        }
    }
    let done = if from.is_file() {
        std::fs::read(&from).and_then(|bytes| unzip(&bytes, &staging))
    } else if from.join(NEEDED).is_dir() {
        PARTS.iter().filter(|p| from.join(p).is_dir()).try_for_each(|p| copy_dir(&from.join(p), &staging.join(p), false))
    } else {
        Err(io::Error::new(io::ErrorKind::NotFound, format!("{} has no save", from.display())))
    };
    let done = done.and_then(|()| {
        crash_point("stage:written");
        std::fs::rename(&staging, &pending)
    });
    if done.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    done
}

/// Moves the game's settings containers in `old` (the save being replaced) to the same places
/// in `new` (the import, which has none), so the player's settings outlast the import.
fn keep_settings(old: &Path, new: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(old)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let target = new.join(entry.file_name());
        if is_settings(&entry.file_name().to_string_lossy()) {
            if !target.exists() {
                std::fs::create_dir_all(new)?;
                if std::fs::rename(entry.path(), &target).is_err() {
                    copy_dir(&entry.path(), &target, true)?;
                }
            }
        } else {
            keep_settings(&entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Whether an import waits for the next start: one staged whole, or one an
/// apply began to put in place.
pub fn staged(data: &Path) -> bool {
    let pending = data.join(PENDING);
    pending.join(NEEDED).is_dir() || pending.join(APPLYING).is_file()
}

/// Puts a staged import in place: the save there now is zipped first (into
/// `share`, else `<data>\save-backups`), then each part of the import takes
/// the place of the one it replaces (a part the import lacks is kept). The
/// backup, or `None` when nothing was staged.
///
/// A part replaced is renamed aside ([`ASIDE`]) and removed only once the
/// import's part has its name: removed first, a crash in between left no
/// save at all. An apply cut short is finished by the next start's, with
/// the backup the first one made ([`APPLYING`]): the save is then the old
/// one or the import, whole, never one part of each.
pub fn apply(data: &Path, share: &Path, stamp: &str) -> io::Result<Option<PathBuf>> {
    let pending = data.join(PENDING);
    tidy(data);
    if !staged(data) {
        return Ok(None);
    }
    let marker = pending.join(APPLYING);
    let backup = match std::fs::read_to_string(&marker) {
        Ok(path) => (!path.is_empty()).then(|| PathBuf::from(path)),
        Err(_) => {
            let name = format!("backup before import {stamp}");
            let backup = if data.join(NEEDED).is_dir() {
                match export(data, share, &name) {
                    Ok(to) => Some(to),
                    // (A game folder that cannot be written, such as one under Program Files.)
                    Err(_) => Some(export(data, &data.join(BACKUPS), &name)?),
                }
            } else {
                None
            };
            crash_point("apply:backed-up");
            let text = backup.as_deref().map(|b| b.to_string_lossy().into_owned()).unwrap_or_default();
            victory_core::fs::write_synced(&marker, text.as_bytes())?;
            backup
        }
    };
    for part in PARTS {
        let new = pending.join(part);
        if !new.is_dir() {
            continue;
        }
        let old = data.join(part);
        let aside = data.join(format!("{part}{ASIDE}"));
        if old.exists() {
            if old.is_dir() {
                keep_settings(&old, &new)?;
            }
            if aside.exists() {
                std::fs::remove_dir_all(&aside)?;
            }
            std::fs::rename(&old, &aside)?;
            crash_point(&format!("apply:{part}-aside"));
        }
        // (Without `old` and with `aside`: an apply cut short after that rename;
        // this one goes on from there.)
        if std::fs::rename(&new, &old).is_err()
            && let Err(e) = copy_dir(&new, &old, true)
        {
            // The part as it was, the player's settings moved back into it too.
            let _ = std::fs::remove_dir_all(&old);
            let _ = std::fs::rename(&aside, &old);
            let _ = keep_settings(&new, &old);
            return Err(e);
        }
        crash_point(&format!("apply:{part}-placed"));
        let _ = std::fs::remove_dir_all(&aside);
    }
    crash_point("apply:placed");
    std::fs::remove_dir_all(&pending)?;
    Ok(Some(backup.unwrap_or_else(|| data.join(PARTS[0]))))
}

/// Where the crash tests stop the process (`VICTORY_TEST_CRASH_AT=<name>`), at
/// once, the rest of the work undone, as a crash or a closed game would.
#[cfg(test)]
fn crash_point(name: &str) {
    if std::env::var("VICTORY_TEST_CRASH_AT").is_ok_and(|at| at == name) {
        std::process::exit(tests::CRASHED);
    }
}

#[cfg(not(test))]
fn crash_point(_: &str) {}

/// What an import cut short left behind, with nothing staged to finish it: a
/// part set aside goes back where its place is still empty, and is removed
/// where the import's part took it; a half-staged import is removed.
fn tidy(data: &Path) {
    let pending = data.join(PENDING);
    for part in PARTS {
        let aside = data.join(format!("{part}{ASIDE}"));
        if !aside.exists() || pending.join(part).is_dir() {
            continue;
        }
        let old = data.join(part);
        if old.exists() {
            let _ = std::fs::remove_dir_all(&aside);
        } else {
            let _ = std::fs::rename(&aside, &old);
        }
    }
    let _ = std::fs::remove_dir_all(data.join(STAGING));
    if pending.exists() && !staged(data) {
        let _ = std::fs::remove_dir_all(&pending);
    }
}

/// At start, before the title runs: the folders the menu uses (`data`,
/// `<data>\<title id>`; `Saves` in `game`, the executable's folder), and a
/// staged import put in place. A line for the console, if one was.
pub fn start(data: &Path, game: &Path) -> Option<String> {
    let share = game.join("Saves");
    // SAFETY: set once at start, before the title's and our threads read them.
    unsafe {
        std::env::set_var("VICTORY_SAVE_DATA", data);
        std::env::set_var("VICTORY_SAVE_SHARE", &share);
    }
    match apply(data, &share, &stamp()) {
        Ok(None) => None,
        Ok(Some(backup)) => {
            tracing::info!(backup = %backup.display(), "save import: put in place; the save it replaced was zipped first");
            Some(format!("save imported (the save it replaced was zipped to {})", backup.display()))
        }
        Err(e) => {
            tracing::warn!(error = %e, "save import: not put in place; the save is as it was");
            Some(format!("the save import failed ({e}); the save is as it was"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("victory-save-share-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn put(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    const SLOT: &str = "saves/0009000000000001/00000000-0000-0000-0000-00007952987a/Player~333dc56ddffd8ea0/v34/0";

    #[test]
    fn an_exported_zip_imports_on_another_pc_and_the_old_save_is_kept() {
        let (mine, theirs, share, third) = (folder("mine"), folder("theirs"), folder("share"), folder("third"));
        let big = "progress ".repeat(10_000); // packs small: deflated
        put(&mine.join(SLOT), &big);
        put(&mine.join("storage/settings/a"), "my storage");
        std::fs::create_dir_all(mine.join("saves/empty folder")).unwrap();
        put(&theirs.join(SLOT), "their progress");
        // Export: one zip, the save left as it was.
        let out = export(&mine, &share, "save 1").unwrap();
        assert_eq!(out, share.join("save 1.zip"));
        assert!(std::fs::metadata(&out).unwrap().len() < 10_000, "deflated");
        assert_eq!(read(&mine.join(SLOT)), big);
        assert!(export(&mine, &share, "save 1").is_err(), "an export never writes over another");
        assert_eq!(exports(&share), ["save 1.zip"]);
        // Import on the other PC: staged, put in place at the next start.
        stage(&theirs, &share, "save 1.zip").unwrap();
        assert!(staged(&theirs));
        assert_eq!(read(&theirs.join(SLOT)), "their progress", "nothing changes while the game runs");
        let backup = apply(&theirs, &share, "t1").unwrap().unwrap();
        assert_eq!(read(&theirs.join(SLOT)), big);
        assert_eq!(read(&theirs.join("storage/settings/a")), "my storage");
        assert!(theirs.join("saves/empty folder").is_dir());
        assert!(!staged(&theirs));
        assert_eq!(apply(&theirs, &share, "t2").unwrap(), None, "applied once");
        // The backup is a zip of the save it replaced.
        assert_eq!(backup, share.join("backup before import t1.zip"));
        stage(&third, &share, "backup before import t1.zip").unwrap();
        apply(&third, &share, "t3").unwrap();
        assert_eq!(read(&third.join(SLOT)), "their progress");
        for f in [mine, theirs, share, third] {
            let _ = std::fs::remove_dir_all(f);
        }
    }

    #[test]
    fn a_zip_zipped_again_by_hand_imports_and_one_reaching_out_is_refused() {
        let (data, share) = (folder("byhand"), folder("share-byhand"));
        let t = (0, 0);
        // As Windows' "Compress to ZIP file" makes it: the export's folder around it.
        let wrapped = zip::write(
            &[
                ("save 1/".to_owned(), Vec::new()),
                (format!("save 1\\{}", SLOT.replace('/', "\\")), b"progress".to_vec()),
                ("save 1/readme.txt".to_owned(), b"hi".to_vec()),
            ],
            t,
        )
        .unwrap();
        std::fs::write(share.join("wrapped.zip"), wrapped).unwrap();
        stage(&data, &share, "wrapped.zip").unwrap();
        apply(&data, &share, "t").unwrap();
        assert_eq!(read(&data.join(SLOT)), "progress");
        assert!(!data.join("readme.txt").exists());
        // Names that would land outside the save are refused, and nothing is staged.
        for name in ["saves/../../evil.dll", "saves/C:/evil", "../saves/x"] {
            let zipped = zip::write(&[("saves/ok".to_owned(), b"1".to_vec()), (name.to_owned(), b"x".to_vec())], t).unwrap();
            std::fs::write(share.join("evil.zip"), zipped).unwrap();
            assert!(stage(&data, &share, "evil.zip").is_err(), "{name}");
            assert!(!staged(&data), "{name}");
        }
        // A damaged zip, and one with no save in it.
        let mut damaged = zip::write(&[("saves/a".to_owned(), b"some bytes here".to_vec())], t).unwrap();
        damaged[30 + "saves/a".len() + 2] ^= 0xff; // in the file's bytes, after its local header
        std::fs::write(share.join("damaged.zip"), damaged).unwrap();
        assert!(stage(&data, &share, "damaged.zip").is_err());
        std::fs::write(share.join("other.zip"), zip::write(&[("photo.png".to_owned(), vec![1])], t).unwrap()).unwrap();
        assert_eq!(stage(&data, &share, "other.zip").unwrap_err().kind(), io::ErrorKind::NotFound);
        std::fs::write(share.join("not a zip.zip"), b"hello").unwrap();
        assert!(stage(&data, &share, "not a zip.zip").is_err());
        assert!(!staged(&data));
        for f in [data, share] {
            let _ = std::fs::remove_dir_all(f);
        }
    }

    #[test]
    fn a_zip_of_what_is_inside_the_saves_folder_imports() {
        let (data, share) = (folder("inside"), folder("share-inside"));
        let t = (0, 0);
        let slot = SLOT.strip_prefix("saves/").unwrap();
        let from_scid = slot.strip_prefix("0009000000000001/").unwrap();
        let from_container = from_scid.strip_prefix("00000000-0000-0000-0000-00007952987a/").unwrap();
        // Zipped from the player's folder (as one shared save was), from the title's, from the
        // containers themselves, and inside another folder of the zip's own.
        for (n, name) in [slot.to_owned(), from_scid.to_owned(), from_container.to_owned(), format!("mine/{slot}")].iter().enumerate() {
            let text = format!("progress {n}");
            let mut dirs: Vec<(String, Vec<u8>)> = Vec::new();
            let mut at = 0;
            while let Some(i) = name[at..].find('/') {
                at += i + 1;
                dirs.push((name[..at].to_owned(), Vec::new()));
            }
            dirs.push((name.clone(), text.clone().into_bytes()));
            dirs.push(("readme.txt".to_owned(), b"hi".to_vec()));
            std::fs::write(share.join("inside.zip"), zip::write(&dirs, t).unwrap()).unwrap();
            stage(&data, &share, "inside.zip").unwrap();
            apply(&data, &share, &format!("t{n}")).unwrap();
            assert_eq!(read(&data.join(SLOT)), text, "{name}");
            assert!(!data.join("readme.txt").exists() && !data.join("saves").join("readme.txt").exists());
        }
        for f in [data, share] {
            let _ = std::fs::remove_dir_all(f);
        }
    }

    #[test]
    fn the_game_settings_are_never_shared_and_the_players_own_are_kept() {
        let (mine, theirs, share) = (folder("set-mine"), folder("set-theirs"), folder("set-share"));
        let user = "saves/0009000000000001/00000000-0000-0000-0000-00007952987a";
        let settings = format!("{user}/SettingsProfileSettings~0b4e25fca06268d0/v5/0");
        put(&mine.join(SLOT), "my progress");
        put(&mine.join(&settings), "my settings");
        put(&theirs.join(SLOT), "their progress");
        put(&theirs.join(&settings), "their settings");
        // Export: no settings in the zip.
        let out = export(&mine, &share, "save").unwrap();
        let names: Vec<String> = zip::read(&std::fs::read(&out).unwrap()).unwrap().into_iter().map(|e| e.0).collect();
        assert!(names.iter().any(|n| n == SLOT), "{names:?}");
        assert!(!names.iter().any(|n| n.contains("SettingsProfileSettings")), "{names:?}");
        // Import: their progress replaced, their settings kept.
        stage(&theirs, &share, "save.zip").unwrap();
        apply(&theirs, &share, "t").unwrap();
        assert_eq!(read(&theirs.join(SLOT)), "my progress");
        assert_eq!(read(&theirs.join(&settings)), "their settings");
        // A zip made before settings were left out: they are passed over on import.
        let older = zip::write(&[(SLOT.to_owned(), b"old progress".to_vec()), (settings.clone(), b"old settings".to_vec())], (0, 0)).unwrap();
        std::fs::write(share.join("older.zip"), older).unwrap();
        stage(&theirs, &share, "older.zip").unwrap();
        apply(&theirs, &share, "t2").unwrap();
        assert_eq!(read(&theirs.join(SLOT)), "old progress");
        assert_eq!(read(&theirs.join(&settings)), "their settings");
        for f in [mine, theirs, share] {
            let _ = std::fs::remove_dir_all(f);
        }
    }

    /// An import cut short leaves the old save or the new one: a half-staged
    /// import is never put in place, and an apply stopped after setting a part
    /// aside is finished by the next start.
    #[test]
    fn an_import_cut_short_leaves_one_save_whole() {
        let (from, data, share) = (folder("cut-from"), folder("cut-data"), folder("cut-share"));
        put(&from.join(SLOT), "new progress");
        export(&from, &share, "new").unwrap();
        put(&data.join(SLOT), "old progress");
        // Staging stopped before the rename: nothing waits, and the next start removes it.
        put(&data.join(STAGING).join(SLOT), "half");
        assert!(!staged(&data));
        assert_eq!(apply(&data, &share, "c0").unwrap(), None);
        assert!(!data.join(STAGING).exists());
        assert_eq!(read(&data.join(SLOT)), "old progress");
        // An apply stopped after the old save was set aside: finished at the next start.
        stage(&data, &share, "new.zip").unwrap();
        assert!(!data.join(STAGING).exists());
        std::fs::rename(data.join("saves"), data.join(format!("saves{ASIDE}"))).unwrap();
        apply(&data, &share, "c1").unwrap();
        assert_eq!(read(&data.join(SLOT)), "new progress");
        assert!(!data.join(format!("saves{ASIDE}")).exists());
        // Stopped after the import took the name, before the part set aside was removed.
        put(&data.join(format!("saves{ASIDE}")).join(SLOT), "old");
        assert_eq!(apply(&data, &share, "c2").unwrap(), None);
        assert!(!data.join(format!("saves{ASIDE}")).exists());
        assert_eq!(read(&data.join(SLOT)), "new progress");
        // Set aside with nothing to take its place: put back.
        std::fs::rename(data.join("saves"), data.join(format!("saves{ASIDE}"))).unwrap();
        apply(&data, &share, "c3").unwrap();
        assert_eq!(read(&data.join(SLOT)), "new progress");
        for f in [from, data, share] {
            let _ = std::fs::remove_dir_all(f);
        }
    }

    /// The exit code of a process a crash test stopped.
    pub(super) const CRASHED: i32 = 86;

    /// A save of both parts, `what` in each file.
    fn save_of(data: &Path, what: &str) {
        put(&data.join(SLOT), &format!("{what} progress"));
        put(&data.join("storage/settings/a"), &format!("{what} storage"));
    }

    /// Which save `data` holds: "old" or "new", the same in every file, or a
    /// panic.
    fn whole_save(data: &Path) -> &'static str {
        let (slot, storage) = (read(&data.join(SLOT)), read(&data.join("storage/settings/a")));
        match (slot.as_str(), storage.as_str()) {
            ("old progress", "old storage") => "old",
            ("new progress", "new storage") => "new",
            _ => panic!("a save of two: {slot:?} and {storage:?}"),
        }
    }

    /// The child of [`a_game_that_dies_anywhere_in_an_import_leaves_one_save_whole`]:
    /// stages the import in `VICTORY_TEST_CRASH_SHARE` into the save in
    /// `VICTORY_TEST_CRASH_DATA`, then puts it in place (the next start's
    /// work), stopped where `VICTORY_TEST_CRASH_AT` says.
    #[test]
    #[ignore = "run by the crash tests in a process of its own"]
    fn crash_child_import() {
        let var = |name| PathBuf::from(std::env::var_os(name).expect(name));
        let (data, share) = (var("VICTORY_TEST_CRASH_DATA"), var("VICTORY_TEST_CRASH_SHARE"));
        stage(&data, &share, "new.zip").unwrap();
        apply(&data, &share, "crash").unwrap();
    }

    /// Acceptance: the game dies (killed, crashed, closed) at each step of
    /// importing a save: while the import is written, before it is marked as
    /// waiting, after the backup, with each part set aside, with each part in
    /// place, and before the import's folder is removed. Whatever the step, the
    /// next start leaves the save whole, old or new and never a part of each,
    /// with the player's own game settings, and nothing behind; once the
    /// import was marked as waiting, the save is the new one.
    #[test]
    fn a_game_that_dies_anywhere_in_an_import_leaves_one_save_whole() {
        let settings = format!("saves/{USER}/{SCID}/{SETTINGS}~1/v1/0");
        let steps = [
            ("stage:file", "old"),
            ("stage:written", "old"),
            ("apply:backed-up", "new"),
            ("apply:saves-aside", "new"),
            ("apply:saves-placed", "new"),
            ("apply:storage-aside", "new"),
            ("apply:storage-placed", "new"),
            ("apply:placed", "new"),
        ];
        for (step, after) in steps {
            let base = folder(&format!("crash-{}", step.replace(':', "-")));
            let (from, data, share) = (base.join("from"), base.join("data"), base.join("share"));
            save_of(&from, "new");
            export(&from, &share, "new").unwrap();
            save_of(&data, "old");
            put(&data.join(&settings), "my picture settings");
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "save_share::tests::crash_child_import", "--ignored", "--test-threads=1"])
                .env("VICTORY_TEST_CRASH_AT", step)
                .env("VICTORY_TEST_CRASH_DATA", &data)
                .env("VICTORY_TEST_CRASH_SHARE", &share)
                .stdout(std::process::Stdio::null()).status()
                .unwrap();
            assert_eq!(status.code(), Some(CRASHED), "{step}: the child stopped there");
            // The next start.
            let applied = apply(&data, &share, "next").unwrap();
            assert_eq!(whole_save(&data), after, "{step}");
            assert_eq!(read(&data.join(&settings)), "my picture settings", "{step}: the player's settings");
            if after == "new" {
                let backup = applied.expect("finished at the next start");
                // (Stopped before the apply was marked as begun, the next start backs
                // the old save up again.)
                let first = if step == "apply:backed-up" { "next" } else { "crash" };
                let name = format!("backup before import {first}.zip");
                assert_eq!(backup.file_name().unwrap().to_string_lossy(), name, "{step}: the backup");
                let check = base.join("check");
                stage(&check, &share, &name).unwrap();
                apply(&check, &share, "check").unwrap();
                assert_eq!(whole_save(&check), "old", "{step}: the backup is the old save");
            }
            let mut left: Vec<String> =
                std::fs::read_dir(&data).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
            left.sort();
            assert_eq!(left, ["saves", "storage"], "{step}: nothing left behind");
            assert!(!staged(&data));
            assert_eq!(apply(&data, &share, "again").unwrap(), None, "{step}");
            let _ = std::fs::remove_dir_all(base);
        }
    }

    /// A part of the import that can be neither moved nor copied (a file of it
    /// held open by another program) fails the apply with the save as it was,
    /// the player's settings in it; the next start puts the import in place.
    #[test]
    fn an_import_that_cannot_be_put_in_place_leaves_the_save_as_it_was() {
        use std::os::windows::fs::OpenOptionsExt;
        let base = folder("held");
        let (from, data, share) = (base.join("from"), base.join("data"), base.join("share"));
        let settings = format!("saves/{USER}/{SCID}/{SETTINGS}~1/v1/0");
        save_of(&from, "new");
        export(&from, &share, "new").unwrap();
        save_of(&data, "old");
        put(&data.join(&settings), "my picture settings");
        stage(&data, &share, "new.zip").unwrap();
        // Not shared at all: the folder cannot be renamed, nor the file read.
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(data.join(PENDING).join(SLOT))
            .unwrap();
        assert!(apply(&data, &share, "h1").is_err());
        assert_eq!(whole_save(&data), "old");
        assert_eq!(read(&data.join(&settings)), "my picture settings");
        assert!(staged(&data), "still waits for the next start");
        drop(held);
        apply(&data, &share, "h2").unwrap().unwrap();
        assert_eq!(whole_save(&data), "new");
        assert_eq!(read(&data.join(&settings)), "my picture settings");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn nothing_to_export_or_import_is_refused() {
        let (data, share) = (folder("empty"), folder("share-empty"));
        assert!(export(&data, &share, "x").is_err());
        std::fs::create_dir_all(share.join("not a save")).unwrap();
        std::fs::write(share.join("notes.txt"), "x").unwrap();
        assert!(exports(&share).is_empty());
        assert!(stage(&data, &share, "not a save").is_err());
        assert!(!staged(&data));
        // A save folder put in Saves as it is (the first test build's exports) still imports.
        put(&share.join("old export").join(SLOT), "old");
        assert_eq!(exports(&share), ["old export"]);
        stage(&data, &share, "old export").unwrap();
        apply(&data, &share, "t").unwrap();
        assert_eq!(read(&data.join(SLOT)), "old");
        for f in [data, share] {
            let _ = std::fs::remove_dir_all(f);
        }
    }
}
