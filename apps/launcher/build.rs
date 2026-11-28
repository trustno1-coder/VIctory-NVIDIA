//! The icon is `ui/assets/ico.ico` where it is present. The repository holds
//! no binary files (docs/asset-policy.md), so without it one is drawn here.
//! The music in `ui/assets/song`, if any, is listed for the page.

use std::path::{Path, PathBuf};

const SIZE: u32 = 32;
const OWN_ICON: &str = "ui/assets/ico.ico";
const SONGS: &str = "ui/assets/song";
const SONG_TYPES: [&str; 4] = ["mp3", "ogg", "m4a", "wav"];

fn main() {
    list_songs();
    println!("cargo:rerun-if-changed={OWN_ICON}");
    let icon = if Path::new(OWN_ICON).is_file() {
        PathBuf::from(OWN_ICON)
            .canonicalize()
            .expect("resolving the icon's path")
    } else {
        let out = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR");
        let drawn = Path::new(&out).join("icon.ico");
        std::fs::write(&drawn, ico(&pixels())).expect("writing the icon");
        drawn
    };
    let icon = PathBuf::from(icon.to_string_lossy().trim_start_matches(r"\\?\"));
    // `generate_context!` takes the window's icon from the configuration;
    // TAURI_CONFIG is merged over tauri.conf.json.
    let path = icon.to_string_lossy().replace('\\', "\\\\");
    println!("cargo:rustc-env=TAURI_CONFIG={{\"bundle\":{{\"icon\":[\"{path}\"]}}}}");
    let windows = tauri_build::WindowsAttributes::new().window_icon_path(&icon);
    tauri_build::try_build(tauri_build::Attributes::new().windows_attributes(windows))
        .expect("tauri-build");
}

/// `LAUNCHER_SONGS`: the music files' names, `|` between them.
fn list_songs() {
    println!("cargo:rerun-if-changed={SONGS}");
    let mut names: Vec<String> = std::fs::read_dir(SONGS)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| {
            let ext = n.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
            ext.is_some_and(|e| SONG_TYPES.contains(&e.as_str())) && !n.contains('|')
        })
        .collect();
    names.sort();
    println!("cargo:rustc-env=LAUNCHER_SONGS={}", names.join("|"));
}

/// A rounded green square with a white play triangle, as BGRA rows top first.
fn pixels() -> Vec<[u8; 4]> {
    let mut px = Vec::with_capacity((SIZE * SIZE) as usize);
    let (s, r) = (SIZE as f32, 7.0f32);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let dx = (r - fx).max(fx - (s - r)).max(0.0);
            let dy = (r - fy).max(fy - (s - r)).max(0.0);
            let inside = dx * dx + dy * dy <= r * r;
            let in_play = fx >= 12.0 && (fy - 16.0).abs() <= (fx - 12.0) * 0.58 && fx <= 23.0;
            px.push(match (inside, in_play) {
                (false, _) => [0, 0, 0, 0],
                (true, true) => [255, 255, 255, 255],
                (true, false) => [0x36, 0xb9, 0x76, 255],
            });
        }
    }
    px
}

/// One 32-bit image in an ICO file: a bitmap stored bottom row first, then
/// an all-clear AND mask (the alpha channel does the masking).
fn ico(px: &[[u8; 4]]) -> Vec<u8> {
    let mask_row = SIZE.div_ceil(32) * 4;
    let image = 40 + SIZE * SIZE * 4 + mask_row * SIZE;
    let mut out = Vec::new();
    out.extend([0, 0, 1, 0, 1, 0]);
    out.extend([SIZE as u8, SIZE as u8, 0, 0]);
    out.extend(1u16.to_le_bytes());
    out.extend(32u16.to_le_bytes());
    out.extend(image.to_le_bytes());
    out.extend(22u32.to_le_bytes());
    out.extend(40u32.to_le_bytes());
    out.extend(SIZE.to_le_bytes());
    out.extend((SIZE * 2).to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(32u16.to_le_bytes());
    out.extend([0u8; 24]);
    for row in px.chunks(SIZE as usize).rev() {
        for p in row {
            out.extend(p);
        }
    }
    out.extend(std::iter::repeat_n(0u8, (mask_row * SIZE) as usize));
    out
}
