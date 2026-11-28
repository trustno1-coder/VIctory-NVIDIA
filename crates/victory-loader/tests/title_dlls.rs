//! Title DLL loading errors and confinement to `title_dir`.
#![cfg(windows)]

#[test]
fn a_title_dll_that_does_not_prepare_is_named() {
    let title = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(title.path().join("broken.dll"), b"MZ not a PE").unwrap();
    let wanted = |_: &str| true;
    let loaded = victory_loader::load_title_dll("broken.dll", title.path(), cache.path(), &wanted);
    let Err(error) = loaded else {
        panic!("a DLL that is not a PE loaded");
    };
    let text = format!("{error:#}");
    assert!(text.contains("broken.dll"), "{text}");
}

#[test]
fn title_dll_imports_cannot_escape_the_title_folder() {
    let parent = tempfile::tempdir().unwrap();
    let title = parent.path().join("title");
    let nested = title.join("sub");
    std::fs::create_dir_all(&nested).unwrap();
    let cache = tempfile::tempdir().unwrap();

    // Non-PE files placed outside or below `title`: if `load_title_dll` ever
    // tried to read them, `prepare` would fail with an error; ignoring them
    // returns `Ok(vec![])`.
    let outside = parent.path().join("outside.dll");
    std::fs::write(&outside, b"MZ not a PE").unwrap();
    std::fs::write(nested.join("nested.dll"), b"MZ not a PE").unwrap();

    let wanted = |_: &str| true;
    let cases = [
        r"..\outside.dll".to_owned(),
        "../outside.dll".to_owned(),
        r"sub\nested.dll".to_owned(),
        "sub/nested.dll".to_owned(),
        outside.to_string_lossy().into_owned(),
        r"C:\Windows\System32\version.dll".to_owned(),
        r"\\127.0.0.1\share\outside.dll".to_owned(),
        r"C:outside.dll".to_owned(),
        "broken.dll:stream".to_owned(),
        "..".to_owned(),
        ".".to_owned(),
        String::new(),
    ];
    for name in &cases {
        let loaded = victory_loader::load_title_dll(name, &title, cache.path(), &wanted)
            .unwrap_or_else(|e| panic!("loader tried to read escaped path {name:?}: {e:#}"));
        assert!(loaded.is_empty(), "escaped path {name:?} should not load");
    }
}
