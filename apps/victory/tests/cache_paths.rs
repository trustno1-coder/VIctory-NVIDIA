use std::{fs, process::Command};

#[test]
fn warming_an_empty_folder_uses_the_configured_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("AppData");
    let home = tmp.path().join("VIctory 空 é");
    let input = tmp.path().join("empty");
    fs::create_dir(&input).unwrap();

    let mut warm = Command::new(env!("CARGO_BIN_EXE_victory"));
    warm.current_dir(tmp.path())
        .args(["translate", "--warm", "all"])
        .arg(input)
        .env("LOCALAPPDATA", &local)
        .env("VICTORY_HOME", &home)
        .env_remove("VICTORY_CONFIG")
        .env_remove("VICTORY_CACHE_DIR")
        .env_remove("VICTORY_LOG_FILE");

    let output = warm.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(home.join("cache/shaders").is_dir());
    assert!(!local.exists());

    let explicit = tmp.path().join("separate cache");
    let output = warm.env("VICTORY_CACHE_DIR", &explicit).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(explicit.join("shaders").is_dir());
    assert!(!local.exists());
}
