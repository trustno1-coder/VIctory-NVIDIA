//! Files written whole under one name and then renamed to their own.

use std::io::Write;
use std::path::Path;

/// Writes `bytes` to `path` and waits until they are on the disk.
///
/// For a file about to be renamed over another: the system writes a file's
/// bytes and the rename to the disk later, each in its own time, and a crash
/// or power cut in between leaves the new name on a file of zeros, or of
/// nothing, in place of the old file that was whole.
pub fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Waits until what was written to the file at `path` is on the disk
/// ([`write_synced`], for a file written in another way: through a mapping,
/// say).
pub fn sync(path: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new().write(true).open(path)?.sync_all()
}

/// Writes `bytes` over the file at `path`: whole under another name first
/// (`<name>.new`, [`write_synced`]), then renamed over it, so that after a
/// crash too the file holds what it held or `bytes`, never a part of them or
/// nothing. A `.new` file a crash left is written over by the next replace.
/// Not for a file two processes replace at the same moment.
pub fn replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".new");
    let staged = path.with_file_name(name);
    let done = write_synced(&staged, bytes).and_then(|()| {
        crash_point("replace:written");
        std::fs::rename(&staged, path)
    });
    if done.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    done
}

/// Where the crash tests stop the process (`VICTORY_TEST_CRASH_AT=<name>`), at
/// once, the rest of the work undone, as a crash or a closed game would.
#[cfg(test)]
fn crash_point(name: &str) {
    if std::env::var("VICTORY_TEST_CRASH_AT").is_ok_and(|at| at == name) {
        std::process::exit(CRASHED);
    }
}

#[cfg(not(test))]
fn crash_point(_: &str) {}

/// The exit code of a process a crash test stopped.
#[cfg(test)]
const CRASHED: i32 = 86;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_and_synced_files_hold_their_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        write_synced(&path, b"first").unwrap();
        write_synced(&path, b"2nd").unwrap();
        sync(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"2nd");
        assert!(sync(&dir.path().join("none")).is_err());
    }

    /// The child of [`a_settings_file_replaced_by_a_process_that_dies_is_whole`]:
    /// replaces the file named in `VICTORY_TEST_CRASH_FILE`, stopped where
    /// `VICTORY_TEST_CRASH_AT` says.
    #[test]
    #[ignore = "run by the crash tests in a process of its own"]
    fn crash_child_replace() {
        let path = std::env::var_os("VICTORY_TEST_CRASH_FILE").expect("VICTORY_TEST_CRASH_FILE");
        replace(Path::new(&path), b"new settings, all of them").unwrap();
    }

    /// Acceptance: a process that dies while replacing a file (here, with the
    /// new bytes written and not yet renamed) leaves the old file whole, and
    /// the next replace succeeds and leaves nothing beside it.
    #[test]
    fn a_settings_file_replaced_by_a_process_that_dies_is_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("menu.txt");
        std::fs::write(&path, b"old settings").unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fs::tests::crash_child_replace", "--ignored", "--test-threads=1"])
            .env("VICTORY_TEST_CRASH_AT", "replace:written")
            .env("VICTORY_TEST_CRASH_FILE", &path)
            .stdout(std::process::Stdio::null()).status()
            .unwrap();
        assert_eq!(status.code(), Some(CRASHED), "the child stopped at the crash point");
        assert_eq!(std::fs::read(&path).unwrap(), b"old settings");
        replace(&path, b"later").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"later");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "the crash's .new file was used again");
    }

    #[test]
    fn a_replaced_file_holds_the_new_bytes_and_nothing_is_left_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.txt");
        replace(&path, b"one").unwrap();
        replace(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        assert!(replace(&dir.path().join("no folder").join("x"), b"x").is_err());
    }
}
