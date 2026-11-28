//! `victory glyphs`: PlayStation button prompts (and, with `--picture`, the
//! Settings controller picture) built from the user's own copy of the game,
//! written as a `VICTORY_FILE_PATCH` list into the cache folder. A start
//! with `VICTORY_PS_PROMPTS=1` makes the same list there by itself, once
//! (`run::playstation_prompts`). Each line of the list is `<file name>
//! <offset> <blake3 of the original range> <file with the new bytes>`, and
//! the DirectStorage hook serves the new bytes over the title's reads of
//! that range: no file of the game is changed on disk.
//!
//! Why the list is kept rather than made at every start: finding the prompt
//! movie reads the whole of `data.rpf` (its table is encrypted, so the
//! entry's place is not known: every zlib stream at a 16-byte step is
//! tried), and the hook applies a patch only while the bytes it replaces
//! still hash as the list says, so a list made for another version of the
//! game changes nothing. It is made again when it is gone, or when the
//! Settings picture is new ([`stale`]).
//!
//! A patch replaces bytes where they are, and the archive's table (which
//! says how long the entry is) is not touched: so the rebuilt movie is as
//! long as the original (zeros after its `End` tag), and its zlib stream
//! exactly as long as the original stream (`victory_formats::zlib_exact`).
//!
//! The Settings picture is a PNG (any size, RGBA with transparency): the
//! controller the Settings menu shows. The user's own when they name one,
//! else the DualSense render that ships with VIctory ([`bundled_picture`]).
//! One that cannot be read or built is left out with a warning, and the
//! list says so: the prompts show all the same, and the archive is not read
//! again at every start for a picture that stays as it is.
//!
//! Nothing of the title is stored in the repository: what is written goes
//! to the cache folder, which `victory_core::ensure_outside_repo` checks.
//! (Ported from Marvisak/VIctory PR #3399 by anoyarou, GPL-3.0-or-later.)
//! Prompt symbols: Kenney's Input Prompts (kenney.nl, CC0), built into
//! `victory_game::prompt_glyphs`.

use std::fmt::Write as _;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use victory_formats::{swf, zlib_exact};
use victory_game::{prompt_glyphs, settings_diagram};

/// The archive holding the prompt movie, and the Settings picture's file,
/// relative to the game folder.
const ARCHIVE: &str = "data.rpf";
const DIAGRAM: &str = "data/ttdb/ui/platform.ttpf";
/// The list in the folder written to, and the file holding the Settings
/// picture's new bytes.
pub(crate) const LIST: &str = "patches.txt";
const PICTURE: &str = "settings-controller.bin";
/// How the list names the picture it was made from (one line, when it was
/// made with one): a different picture makes it [`stale`].
const FROM: &str = "# picture: ";
/// How the list notes a picture that was given and could not be built.
const LEFT_OUT: &str = "# settings-controller.bin was left out: ";
/// The longest side of the picture read (256 MiB of pixels at most). The
/// Settings texture it becomes is 1024x512; a file that claims more (a
/// broken one, gigabytes) is refused, not allocated.
const MAX_PICTURE_SIDE: u32 = 8192;
/// The DualSense render that ships with VIctory (1024x683 RGBA, its
/// contributor's own work under the repository's licence): the Settings
/// picture when the user names none.
const BUNDLED_PICTURE: &[u8] = include_bytes!("../assets/ps5-controller.png");
/// Its file in the folder the patches are written to.
const BUNDLED_PICTURE_FILE: &str = "ps5-controller.png";

/// The bundled picture as a file in `out`, written when it is not there or
/// differs (a newer VIctory's): its modification time then makes the list
/// [`stale`], so the Settings picture is built again from it.
pub(crate) fn bundled_picture(out: &Path) -> Result<PathBuf> {
    let path = out.join(BUNDLED_PICTURE_FILE);
    if std::fs::read(&path).ok().as_deref() != Some(BUNDLED_PICTURE) {
        victory_core::ensure_outside_repo(out)?;
        std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
        std::fs::write(&path, BUNDLED_PICTURE).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(path)
}

/// `victory glyphs`: says what was built, and where the list is.
pub fn run(game: Option<PathBuf>, picture: Option<PathBuf>, out: Option<PathBuf>) -> Result<()> {
    let cfg = victory_core::Config::load()?;
    let game = match game {
        Some(g) => g,
        None => cfg.require_game_dir()?.to_path_buf(),
    };
    let out = out.unwrap_or_else(|| cfg.cache_dir.join("file-patches").join("playstation"));
    let picture = match picture {
        Some(p) => p,
        None => bundled_picture(&out)?,
    };
    println!("looking for the prompt icons in {} ...", game.join(ARCHIVE).display());
    let built = make(&game, Some(&picture), &out)?;
    for (offset, stream_len) in &built.prompts {
        println!("  prompt icons at {offset:#x} ({stream_len} bytes): rebuilt");
    }
    let from = picture.display();
    match &built.picture {
        Ok(Some(stream_len)) => println!("  Settings controller picture ({stream_len} bytes): rebuilt from {from}"),
        Err(why) => println!("  Settings controller picture: skipped ({why}: {from})"),
        Ok(None) => println!("  Settings controller picture: skipped"),
    }
    println!(
        "\nwritten: {}\nto use it: set VICTORY_FILE_PATCH={}",
        built.list.display(),
        built.list.display()
    );
    Ok(())
}

/// Builds the patches from the game in `game` (the Settings picture from
/// the PNG `picture`, when one is given) into `out`, and returns the list
/// written there ([`LIST`]). A picture that cannot be built is warned about
/// and left out: the prompts are built all the same.
pub(crate) fn build(game: &Path, picture: Option<&Path>, out: &Path) -> Result<PathBuf> {
    Ok(make(game, picture, out)?.list)
}

/// Whether the list in `out` has to be made again for `picture`: there is
/// none yet, or, with a picture at hand, it was made from another one (or
/// none), or the picture file is newer than the list. A list that notes a
/// picture it left out counts as made from it: that picture is not tried
/// again at every start until its file changes.
pub(crate) fn stale(out: &Path, picture: Option<&Path>) -> bool {
    let list = out.join(LIST);
    let Ok(text) = std::fs::read_to_string(&list) else {
        return true;
    };
    picture.is_some_and(|picture| !made_from(&text, picture) || newer(picture, &list))
}

/// Whether the list's text says it was made from the picture `picture`
/// (its [`FROM`] line).
fn made_from(list: &str, picture: &Path) -> bool {
    let from = format!("{FROM}{}", picture.display());
    list.lines().any(|line| line.trim_end() == from)
}

/// Whether `file` was changed after `other`, by their modification times:
/// not, when either cannot be read.
fn newer(file: &Path, other: &Path) -> bool {
    let changed = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified());
    matches!((changed(file), changed(other)), (Ok(file), Ok(other)) if file > other)
}

/// What [`make`] wrote.
struct Built {
    /// The list, [`LIST`] in the folder given.
    list: PathBuf,
    /// Each prompt movie: its offset in the archive, and its stream's length.
    prompts: Vec<(usize, usize)>,
    /// The Settings picture: `Ok(None)` when none was given, else its
    /// stream's length, or why it was left out.
    picture: Result<Option<usize>, String>,
}

fn make(game: &Path, picture: Option<&Path>, out: &Path) -> Result<Built> {
    victory_core::ensure_outside_repo(out)?;
    std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    let mut list = String::from(
        "# VICTORY_FILE_PATCH list made by `victory glyphs` from this copy of the game (keep it local).\n\
         # <file name> <offset> <blake3 of the original range> <file with the new bytes>\n",
    );

    if let Some(path) = picture {
        writeln!(list, "{FROM}{}", path.display())?;
    }
    // The picture before the archive: little work, and whether it can be
    // built is known before the long read. (Its line goes last.)
    let pic = picture.map(|path| (path, picture_patch(game, path)));
    let archive = game.join(ARCHIVE);
    let found = find_prompt_movies(&archive)?;
    if found.is_empty() {
        bail!("no prompt-icon movie found in {}", archive.display());
    }
    let mut prompts = Vec::with_capacity(found.len());
    for (offset, stream_len, movie) in found {
        let new = prompt_glyphs::rebuild(&movie).map_err(anyhow::Error::msg)?;
        let bytes = prompt_glyphs::pack(&new, stream_len).map_err(anyhow::Error::msg)?;
        Patch {
            target: ARCHIVE,
            offset,
            original: read_range(&archive, offset, stream_len)?,
            name: format!("prompt-icons-{offset:x}.bin"),
            bytes,
        }
        .write(out, &mut list)?;
        prompts.push((offset, stream_len));
    }
    let outcome = match pic {
        None => Ok(None),
        Some((_, Ok(patch))) => {
            let stream_len = patch.bytes.len();
            patch.write(out, &mut list)?;
            Ok(Some(stream_len))
        }
        Some((path, Err(why))) => {
            let why = why.replace(['\r', '\n'], " ");
            writeln!(list, "{LEFT_OUT}{why}")?;
            tracing::warn!(
                picture = %path.display(),
                "playstation prompts: the Settings controller picture was left out ({why}); the button prompts are built all the same"
            );
            Err(why)
        }
    };
    let list_path = out.join(LIST);
    std::fs::write(&list_path, list).with_context(|| format!("writing {}", list_path.display()))?;
    Ok(Built { list: list_path, prompts, picture: outcome })
}

/// One patch as the list names it: a range of one of the title's files,
/// served from a file of new bytes instead.
struct Patch {
    /// The title's file, by the name the hook matches a patch on.
    target: &'static str,
    offset: usize,
    /// What the title's file has in the range. Its hash goes into the list,
    /// so that the hook applies the patch only while the bytes are these.
    original: Vec<u8>,
    /// The new bytes, and the file in the folder written to they go to.
    name: String,
    bytes: Vec<u8>,
}

impl Patch {
    /// Writes the new bytes, and the patch's line into `list`.
    fn write(&self, out: &Path, list: &mut String) -> Result<()> {
        let file = out.join(&self.name);
        std::fs::write(&file, &self.bytes).with_context(|| format!("writing {}", file.display()))?;
        writeln!(
            list,
            "{} {:#x} {} {}",
            self.target,
            self.offset,
            blake3::hash(&self.original).to_hex(),
            self.name
        )?;
        Ok(())
    }
}

/// The Settings picture's patch in the game in `game`, made from the PNG
/// `picture`: `Err` says why there is none.
fn picture_patch(game: &Path, picture: &Path) -> Result<Patch, String> {
    let file = game.join(DIAGRAM);
    let file_bytes = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let (offset, stream_len) = settings_diagram::find_stream(&file_bytes)
        .ok_or("the Settings picture's texture was not found in the title's file")?;
    let (width, height, px) = load_png(picture).map_err(|e| format!("{e:#}"))?;
    let tiled = settings_diagram::texture_from_picture(&px, width, height);
    let bytes = settings_diagram::pack(&tiled, stream_len)?;
    let target = Path::new(DIAGRAM).file_name().and_then(|n| n.to_str()).unwrap_or(DIAGRAM);
    let original = file_bytes[offset..offset + stream_len].to_vec();
    Ok(Patch { target, offset, original, name: PICTURE.to_owned(), bytes })
}

/// The PNG at `path` as RGBA8: its width, its height, and its pixels (rows
/// of four bytes). Palettes and 16-bit samples are expanded, and an image
/// without an alpha channel gets a full one.
fn load_png(path: &Path) -> Result<(usize, usize, Vec<u8>)> {
    read_png(path, MAX_PICTURE_SIDE)
}

/// [`load_png`] with the longest side a picture may have.
fn read_png(path: &Path, max_side: u32) -> Result<(usize, usize, Vec<u8>)> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::STRIP_16 | png::Transformations::ALPHA,
    );
    let mut reader = decoder.read_info()?;
    let (width, height) = reader.info().size();
    anyhow::ensure!(
        width <= max_side && height <= max_side,
        "{}: {width}x{height} is too large a picture (at most {max_side} on a side)",
        path.display()
    );
    let mut px = vec![0; reader.output_buffer_size().context("the picture does not fit in memory")?];
    let info = reader.next_frame(&mut px)?;
    px.truncate(info.buffer_size());
    if info.color_type != png::ColorType::Rgba {
        bail!("{}: not an RGBA picture: save it as 32-bit colour with alpha", path.display());
    }
    Ok((info.width as usize, info.height as usize, px))
}

fn read_range(path: &Path, offset: usize, len: usize) -> Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset as u64))?;
    let mut buf = vec![0; len];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// Every zlib stream (at a 16-byte step) in `archive` that inflates to the
/// prompt-icon movie: its offset, stream length and the movie.
fn find_prompt_movies(archive: &Path) -> Result<Vec<(usize, usize, Vec<u8>)>> {
    const CHUNK: usize = 32 << 20;
    const READ_AHEAD: usize = 4 << 20;
    let mut f =
        std::fs::File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let size = f.metadata()?.len() as usize;
    let mut found = Vec::new();
    let mut buf = vec![0u8; CHUNK + 64];
    let mut base = 0usize;
    while base < size {
        let n = (CHUNK + 64).min(size - base);
        f.seek(SeekFrom::Start(base as u64))?;
        f.read_exact(&mut buf[..n])?;
        for at in (0..n.min(CHUNK)).step_by(16) {
            if buf[at] != 0x78 {
                continue;
            }
            let Some(head) = zlib_exact::inflate_prefix(&buf[at..n], 16) else {
                continue;
            };
            if head.len() < 16 || !swf::is_movie(&head) {
                continue;
            }
            let len = swf::declared_len(&head).unwrap_or(0);
            let stream = read_range(archive, base + at, READ_AHEAD.min(size - base - at))?;
            if let Some(inflated) = zlib_exact::inflate(&stream, len)
                && prompt_glyphs::is_prompt_movie(&inflated.data)
            {
                found.push((base + at, inflated.stream_len, inflated.data));
            }
        }
        base += CHUNK;
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    use super::{BUNDLED_PICTURE, FROM, LIST, PICTURE, Patch, bundled_picture, load_png, made_from, read_png, stale};

    const PROMPTS: &str = "# made\ndata.rpf 0x1234 <hash> prompt-icons-1234.bin\n";

    /// Sets when `path` was last changed.
    fn changed_at(path: &Path, when: SystemTime) {
        std::fs::OpenOptions::new().write(true).open(path).unwrap().set_modified(when).unwrap();
    }

    /// A list names the picture it was made from, and only that one.
    #[test]
    fn the_list_says_which_picture_it_was_made_from() {
        let (mine, other) = (Path::new(r"C:\pics\mine.png"), Path::new(r"C:\pics\other.png"));
        assert!(!made_from(PROMPTS, mine));
        let list = format!("{FROM}{}\r\n{PROMPTS}platform.ttpf 0x20 <hash> {PICTURE}\r\n", mine.display());
        assert!(made_from(&list, mine));
        assert!(!made_from(&list, other));
    }

    /// The bundled picture is written once as an RGBA PNG the picture is
    /// built from, and not again while it is there unchanged (so the list
    /// made from it stays current).
    #[test]
    fn the_bundled_picture_is_written_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = bundled_picture(dir.path()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), BUNDLED_PICTURE);
        let (width, height, _) = load_png(&path).unwrap();
        assert_eq!((width, height), (1024, 683));
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        changed_at(&path, old);
        bundled_picture(dir.path()).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), old, "rewritten unchanged");
        std::fs::write(&path, b"an older picture").unwrap();
        bundled_picture(dir.path()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), BUNDLED_PICTURE);
    }

    /// The list is made again when there is none, when a picture is at hand
    /// it was not made from, or when the picture file is newer: not for one
    /// it notes as left out, nor without a picture at hand.
    #[test]
    fn the_list_is_made_again_only_when_needed() {
        let dir = tempfile::tempdir().unwrap();
        let (out, list) = (dir.path().join("out"), dir.path().join("out").join(LIST));
        let picture = dir.path().join("controller.png");
        std::fs::write(&picture, b"a png").unwrap();
        let from = format!("{FROM}{}\n", picture.display());
        assert!(stale(&out, None), "no folder yet");
        std::fs::create_dir_all(&out).unwrap();
        assert!(stale(&out, Some(&picture)), "no list yet, with a picture at hand");
        std::fs::write(&list, PROMPTS).unwrap();
        assert!(!stale(&out, None), "a list, no picture at hand");
        assert!(stale(&out, Some(&picture)), "the list was made without the picture at hand");
        std::fs::write(&list, format!("{FROM}another.png\n{PROMPTS}platform.ttpf 0x20 <hash> {PICTURE}\n")).unwrap();
        assert!(stale(&out, Some(&picture)), "the list was made from another picture");
        std::fs::write(&list, format!("{from}{PROMPTS}{}not a PNG", super::LEFT_OUT)).unwrap();
        assert!(!stale(&out, Some(&picture)), "a picture left out is not tried again");
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        changed_at(&picture, old - Duration::from_secs(60));
        std::fs::write(&list, format!("{from}{PROMPTS}platform.ttpf 0x20 <hash> {PICTURE}\n")).unwrap();
        changed_at(&list, old);
        assert!(!stale(&out, Some(&picture)), "the picture is older than the list");
        changed_at(&picture, old + Duration::from_secs(60));
        assert!(stale(&out, Some(&picture)), "the picture is newer than the list");
    }

    /// A patch's line is what the hook reads: the file's name, the offset,
    /// the hash of the bytes replaced, the file of the new bytes.
    #[test]
    fn a_patch_is_written_as_the_hook_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut list = String::new();
        Patch {
            target: "data.rpf",
            offset: 0x1234,
            original: vec![7; 300],
            name: "new.bin".to_owned(),
            bytes: vec![9; 5],
        }
        .write(dir.path(), &mut list)
        .unwrap();
        let mut fields = list.trim_end().split(' ');
        assert_eq!(fields.next(), Some("data.rpf"));
        assert_eq!(fields.next(), Some("0x1234"));
        let hash = fields.next().unwrap();
        assert_eq!(hash.len(), 64, "{hash}");
        assert_eq!(hash, blake3::hash(&[7; 300]).to_hex().as_str());
        assert_eq!(fields.next(), Some("new.bin"));
        assert_eq!(fields.next(), None);
        assert_eq!(std::fs::read(dir.path().join("new.bin")).unwrap(), [9u8; 5]);
    }

    /// An 8-bit PNG of `color`, `width`x`height`, with the pixels `px`.
    fn png_bytes(color: png::ColorType, width: u32, height: u32, px: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(px).unwrap();
        writer.finish().unwrap();
        bytes
    }

    /// A picture is read as RGBA8: one without alpha gets a full one; one
    /// too large is refused before it is decoded; one that is grey with
    /// alpha says so; one that is not a PNG fails.
    #[test]
    fn a_picture_is_read_as_rgba() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str, bytes: &[u8]| {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let rgba = file("rgba.png", &png_bytes(png::ColorType::Rgba, 2, 1, &[1, 2, 3, 4, 5, 6, 7, 0]));
        assert_eq!(load_png(&rgba).unwrap(), (2, 1, vec![1, 2, 3, 4, 5, 6, 7, 0]));
        let rgb = file("rgb.png", &png_bytes(png::ColorType::Rgb, 2, 1, &[1, 2, 3, 5, 6, 7]));
        assert_eq!(load_png(&rgb).unwrap(), (2, 1, vec![1, 2, 3, 255, 5, 6, 7, 255]));
        let grey = file("grey.png", &png_bytes(png::ColorType::GrayscaleAlpha, 1, 1, &[10, 200]));
        let error = load_png(&grey).unwrap_err().to_string();
        assert!(error.ends_with("not an RGBA picture: save it as 32-bit colour with alpha"), "{error}");
        let error = read_png(&rgba, 1).unwrap_err().to_string();
        assert!(error.contains("2x1 is too large a picture"), "{error}");
        assert!(load_png(&file("text.png", b"not a png")).is_err());
    }
}
