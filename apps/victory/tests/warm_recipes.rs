#![cfg(windows)]

use std::fs;
use std::process::{Command, Output};
use tempfile::TempDir;

struct Cache(TempDir);

impl Cache {
    fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }

    fn warm(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_victory"))
            .arg("warm")
            .args(args)
            .current_dir(self.0.path())
            .env("VICTORY_CONFIG", "")
            .env("VICTORY_CACHE_DIR", self.0.path().join("cache"))
            .env("VICTORY_DATA_DIR", self.0.path().join("data"))
            .env("VICTORY_LOG", "off")
            .env_remove("VICTORY_LOG_FILE")
            .output()
            .unwrap()
    }
}

#[test]
fn exact_warm_without_a_book_fails_and_names_the_requested_index() {
    let cache = Cache::new();
    let output = cache.warm(&["--these", "42"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("pipeline recipe 42"), "{output:?}");
}

#[test]
fn bulk_warm_with_a_missing_shader_book_fails_without_blacklisting() {
    let cache = Cache::new();
    let pipelines = cache.0.path().join("cache/pipelines");
    fs::create_dir_all(&pipelines).unwrap();
    let mut book = Vec::new();
    add_record(&mut book, 0x11, &[0, 0]);
    fs::write(pipelines.join("known-pipelines.bin"), book).unwrap();

    let output = cache.warm(&["--jobs", "1"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("known-shaders.bin"), "{output:?}");
    assert!(!pipelines.join("warm-bad.txt").exists());
}

fn add_record(book: &mut Vec<u8>, byte: u8, body: &[u8]) {
    book.extend_from_slice(&(16 + body.len() as u32).to_le_bytes());
    book.extend_from_slice(&[byte; 16]);
    book.extend_from_slice(body);
}

#[test]
fn unreadable_recipe_stops_an_exact_batch_without_claiming_a_driver_refusal() {
    let cache = Cache::new();
    let pipelines = cache.0.path().join("cache/pipelines");
    fs::create_dir_all(&pipelines).unwrap();
    // A zero-stage record is parsed and refused before device creation.
    // The second record references a geometry shader absent from the book.
    let mut book = Vec::new();
    add_record(&mut book, 0x11, &[0, 0]);
    let mut unreadable = [0x42; 35];
    unreadable[0] = 1;
    unreadable[1] = 4;
    unreadable[34] = 0;
    add_record(&mut book, 0x22, &unreadable);
    add_record(&mut book, 0x33, &[0, 0]);
    fs::write(pipelines.join("known-pipelines.bin"), book).unwrap();
    fs::write(pipelines.join("known-shaders.bin"), []).unwrap();

    let output = cache.warm(&["--these", "0,1,2"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("pipeline recipe 1"), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let reports: Vec<_> = stdout.lines().collect();
    assert_eq!(reports.len(), 4, "{stdout}");
    assert_eq!(reports[0], "at 0");
    assert!(reports[1].starts_with("made 0 ") && reports[1].ends_with(" 0"), "{stdout}");
    assert_eq!(reports[2], "at 1");
    assert_eq!(reports[3], "unreadable-recipe 1");
}
