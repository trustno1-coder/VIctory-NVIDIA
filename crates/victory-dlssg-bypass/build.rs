use std::path::{Path, PathBuf};

fn collect_cpp_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_cpp_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "cpp") {
            out.push(path);
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/bridge.cpp");
    println!("cargo:rerun-if-changed=vendor");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest.join("vendor");
    let minhook = vendor.join("minhook");
    let lz4 = vendor.join("lz4");
    let libhat = vendor.join("libhat");
    let odg_src = vendor.join("opendlssg").join("src");
    let sl_inc = vendor.join("streamline").join("include");

    let is_msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");

    // 1. MinHook (C11)
    let mut mh = cc::Build::new();
    mh.include(minhook.join("include"))
        .include(minhook.join("src"))
        .define("WIN32_LEAN_AND_MEAN", None)
        .warnings(false)
        .files([
            minhook.join("src").join("buffer.c"),
            minhook.join("src").join("hook.c"),
            minhook.join("src").join("trampoline.c"),
            minhook.join("src").join("hde").join("hde64.c"),
        ]);
    mh.compile("victory_odg_minhook");

    // 2. LZ4 (C11)
    let mut lz = cc::Build::new();
    lz.include(lz4.join("lib"))
        .warnings(false)
        .file(lz4.join("lib").join("lz4.c"));
    lz.compile("victory_odg_lz4");

    // 3. libhat (C++20, Windows x86-64)
    let mut hat = cc::Build::new();
    hat.cpp(true)
        .include(libhat.join("include"))
        .include(libhat.join("src"))
        .define("LIBHAT_FEATURE_SSE", None)
        .define("LIBHAT_FEATURE_AVX512", None)
        .define("LIBHAT_HINT_X86_64", None)
        .define("WIN32_LEAN_AND_MEAN", None)
        .define("NOMINMAX", None)
        .warnings(false);
    if is_msvc {
        hat.flag("/std:c++20").flag("/EHsc");
    } else {
        hat.flag("-std=c++20");
    }
    hat.files([
        libhat.join("src").join("Scanner.cpp"),
        libhat.join("src").join("System.cpp"),
        libhat.join("src").join("arch").join("x86").join("AVX2.cpp"),
        libhat.join("src").join("arch").join("x86").join("AVX512.cpp"),
        libhat.join("src").join("arch").join("x86").join("SSE.cpp"),
        libhat.join("src").join("arch").join("x86").join("System.cpp"),
        libhat.join("src").join("os").join("win32").join("MemoryProtector.cpp"),
        libhat.join("src").join("os").join("win32").join("Process.cpp"),
        libhat.join("src").join("os").join("win32").join("Scanner.cpp"),
        libhat.join("src").join("os").join("win32").join("System.cpp"),
    ]);
    hat.compile("victory_odg_libhat");

    // 4. opendlssgenabler core + bridge (C++20)
    let mut odg_files = vec![manifest.join("src").join("bridge.cpp")];
    collect_cpp_files(&odg_src, &mut odg_files);

    let mut odg = cc::Build::new();
    odg.cpp(true)
        .include(&odg_src)
        .include(&sl_inc)
        .include(minhook.join("include"))
        .include(minhook.join("src"))
        .include(libhat.join("include"))
        .include(lz4.join("lib"))
        .define("UNICODE", None)
        .define("_UNICODE", None)
        .define("WIN32_LEAN_AND_MEAN", None)
        .define("NOMINMAX", None)
        .define("ODG_VERSION", "\"0.2.0\"")
        .define("LIBHAT_FEATURE_SSE", None)
        .define("LIBHAT_FEATURE_AVX512", None)
        .define("LIBHAT_HINT_X86_64", None)
        .warnings(false);
    if is_msvc {
        odg.flag("/std:c++20").flag("/EHsc");
    } else {
        odg.flag("-std=c++20");
    }
    odg.files(odg_files);
    odg.compile("victory_odg_core");

    println!("cargo:rustc-link-lib=advapi32");
    println!("cargo:rustc-link-lib=version");
}
