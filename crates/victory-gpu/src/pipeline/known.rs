//! Every pipeline the title has ever created, kept on disk
//! (`VICTORY_PIPELINE_BOOK=1`), and their host pipelines made ahead of a run
//! (`victory warm`), so that the driver's own cache on disk has them before
//! a draw asks for one.
//!
//! The title creates 84,000 pipelines while it loads and draws with some
//! 2,000 of them; which ones is known from earlier runs (`library`), and
//! those are made during loading. One met for the first time (a place never
//! seen, or everything after a change of the translator or of the driver) is
//! compiled when a draw wants it. Making all of them in the running title
//! (`VICTORY_EARLY_ALL=1`) ended it inside the driver's compiler; made here,
//! a compiler that crashes or hangs ends a child process, costs that pipeline
//! alone and is named in `warm-bad.txt`.
//!
//! Two files beside the library, both only ever added to (a torn end is cut
//! off): `known-shaders.bin` (stage, hash, length, container) and
//! `known-pipelines.bin` (length, then the key, the shaders' stages and
//! hashes, and the graphics state). They hold the title's own code: they stay
//! in the cache folder.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::PCSTR;

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

const SHADERS: &str = "known-shaders.bin";
const PIPELINES: &str = "known-pipelines.bin";
/// Stages by their number in the files.
const STAGES: [&str; 8] =
    ["vertex", "pixel", "domain", "hull", "geometry", "compute", "amplification", "mesh"];

/// `VICTORY_PIPELINE_BOOK=1`: the pipelines the title creates are kept.
fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_BOOK").is_some_and(|v| v == "1"))
}

struct Book {
    dir: PathBuf,
    keys: HashSet<[u8; 16]>,
    shaders: HashSet<[u8; 32]>,
    /// Not yet written: shaders, then pipelines.
    new: [Vec<u8>; 2],
    added: usize,
}

static BOOK: OnceLock<Option<Mutex<Book>>> = OnceLock::new();

fn book() -> Option<&'static Mutex<Book>> {
    BOOK.get_or_init(|| {
        let dir = super::library::folder().filter(|_| on())?;
        std::fs::create_dir_all(&dir).ok()?;
        let mut keys = HashSet::new();
        let mut shaders = HashSet::new();
        let ends = [
            walk_shaders(&dir, |_, hash, _, _| {
                shaders.insert(hash);
            }),
            walk_pipelines(&dir, |_, bytes| {
                keys.extend(bytes.first_chunk::<16>().copied());
            }),
        ];
        for (file, end) in [SHADERS, PIPELINES].iter().zip(ends) {
            let path = dir.join(file);
            if std::fs::metadata(&path).is_ok_and(|m| m.len() > end)
                && let Ok(f) = std::fs::OpenOptions::new().write(true).open(&path)
            {
                let _ = f.set_len(end);
            }
        }
        tracing::info!(pipelines = keys.len(), shaders = shaders.len(), "title pipelines known from earlier runs");
        let spawned = std::thread::Builder::new().name("victory-known".into()).spawn(|| {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                flush();
            }
        });
        spawned.ok()?;
        Some(Mutex::new(Book { dir, keys, shaders, new: Default::default(), added: 0 }))
    })
    .as_ref()
}

/// The title created a pipeline.
pub(super) fn note(key: &str, shaders: &[Arc<Shader>], graphics: Option<&GraphicsState>) {
    let Some(book) = book() else { return };
    let mut id = [0u8; 16];
    for (at, b) in id.iter_mut().enumerate() {
        *b = key.get(2 * at..2 * at + 2).and_then(|x| u8::from_str_radix(x, 16).ok()).unwrap_or(0);
    }
    let mut book = book.lock().unwrap_or_else(|e| e.into_inner());
    if shaders.is_empty() || !book.keys.insert(id) {
        return;
    }
    let mut entry = id.to_vec();
    entry.push(shaders.len() as u8);
    for s in shaders {
        let hash = *blake3::hash(&s.container).as_bytes();
        let stage = STAGES.iter().position(|&x| x == s.stage).unwrap_or(0) as u8;
        entry.push(stage);
        entry.extend(hash);
        if book.shaders.insert(hash) {
            let new = &mut book.new[0];
            new.push(stage);
            new.extend(hash);
            new.extend((s.container.len() as u32).to_le_bytes());
            new.extend(&s.container);
        }
    }
    entry.push(u8::from(graphics.is_some()));
    if let Some(g) = graphics {
        g.put(&mut entry);
    }
    book.new[1].extend((entry.len() as u32).to_le_bytes());
    book.new[1].extend(entry);
    book.added += 1;
}

/// Writes what was noted.
fn flush() {
    let Some(Some(book)) = BOOK.get() else { return };
    let (dir, new, added) = {
        let mut book = book.lock().unwrap_or_else(|e| e.into_inner());
        if book.added == 0 {
            return;
        }
        (book.dir.clone(), std::mem::take(&mut book.new), std::mem::take(&mut book.added))
    };
    for (file, bytes) in [SHADERS, PIPELINES].iter().zip(&new) {
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(file))
            .and_then(|mut f| f.write_all(bytes));
        if let Err(e) = written {
            tracing::warn!(error = %e, file, "title pipelines not written down");
            return;
        }
    }
    tracing::info!(pipelines = added, "title pipelines written down");
}

/// Calls `f` with each container's stage, hash, place and length; where the
/// last whole one ends.
fn walk_shaders(dir: &Path, mut f: impl FnMut(u8, [u8; 32], u64, u32)) -> u64 {
    let Ok(file) = std::fs::File::open(dir.join(SHADERS)) else { return 0 };
    let len = file.metadata().map_or(0, |m| m.len());
    let mut file = std::io::BufReader::new(file);
    let mut at = 0;
    let mut head = [0u8; 37];
    while file.read_exact(&mut head).is_ok() {
        let size = u32::from_le_bytes([head[33], head[34], head[35], head[36]]);
        if at + 37 + u64::from(size) > len || file.seek_relative(i64::from(size)).is_err() {
            break;
        }
        let mut hash = [0; 32];
        hash.copy_from_slice(&head[1..33]);
        f(head[0], hash, at + 37, size);
        at += 37 + u64::from(size);
    }
    at
}

/// How much of each kept container is read to find its HASH part, which lies
/// after the small parts that precede it (a few KiB at most). A container
/// whose HASH part lies beyond it simply has no entry.
const PREFIX: usize = 8192;

/// The program names the layer leaves out by name (`make::left_out`), which a
/// container made from pieces does not carry as text. The index notes which
/// programs hold one, so such a pipeline is still named. In every container
/// of the book that has one, the name lies well inside `PREFIX`.
pub(super) const NAMES: [&[u8]; 2] = [b"CS_RaytracingResolve", b"CS_ScreenProbes_ActivateRays"];

/// What the index knows of one program of the title's own containers.
#[derive(Default)]
struct Known {
    /// The logged hashes (first 16 hex digits of the blake3) of the containers that hold it.
    /// Several containers can hold one program (variants of it share the hash), hence the list.
    stock: Vec<String>,
    /// Bit n is set when a container of it carries `NAMES[n]`.
    named: u8,
}

/// The title's own program hash of each kept container, with what is known
/// of it. The book is on disk and only a prefix of each kept container is
/// read, once; nothing is kept but the 16-byte keys, the logged hashes and the
/// name bits. The second number is how many containers gave no program (no
/// HASH part inside the prefix), which would otherwise be a silent hole.
fn index(dir: &Path) -> (HashMap<victory_shader::pieces::ProgramHash, Known>, usize) {
    use std::os::windows::fs::FileExt;
    let mut places = Vec::new();
    walk_shaders(dir, |_, hash, at, len| places.push((hash, at, len)));
    let mut map: HashMap<victory_shader::pieces::ProgramHash, Known> = HashMap::new();
    let mut skipped = 0;
    let Ok(file) = std::fs::File::open(dir.join(SHADERS)) else { return (map, skipped) };
    let mut buf = vec![0u8; PREFIX];
    for (hash, at, len) in places {
        let want = (len as usize).min(PREFIX);
        let Ok(n) = file.seek_read(&mut buf[..want], at) else { continue };
        // containers made from pieces and anything without a HASH part have
        // no program to give and are left out
        let Some(program) = victory_shader::pieces::program_of_stock(&buf[..n]) else {
            skipped += 1;
            continue;
        };
        let known = map.entry(program).or_default();
        known.stock.push(blake3::Hash::from_bytes(hash).to_hex()[..16].to_owned());
        for (bit, name) in NAMES.iter().enumerate() {
            if buf[..n].windows(name.len()).any(|w| w == *name) {
                known.named |= 1 << bit;
            }
        }
    }
    (map, skipped)
}

/// The index of the book in the cache folder, built once. It does not depend
/// on `VICTORY_PIPELINE_BOOK`, which only controls writing.
fn programs() -> &'static HashMap<victory_shader::pieces::ProgramHash, Known> {
    static PROGRAMS: OnceLock<HashMap<victory_shader::pieces::ProgramHash, Known>> = OnceLock::new();
    PROGRAMS.get_or_init(|| {
        // no cache folder configured gives an empty map
        let began = std::time::Instant::now();
        let (map, skipped) = super::library::folder().map(|dir| index(&dir)).unwrap_or_default();
        // a digest held by several containers matches every list that names one of them, so the
        // count shows how far a match by program reaches past a match by hash
        let shared = map.values().filter(|k| k.stock.len() > 1).count();
        tracing::info!(
            programs = map.len(),
            shared,
            skipped,
            ms = began.elapsed().as_secs_f64() * 1e3,
            "programs of the title's containers known from earlier runs, which the stock hash lists match pipelines from pieces by (VICTORY_PIPELINE_PIECES=1); shared: digests of several containers; skipped: containers with no HASH part inside the prefix"
        );
        map
    })
}

/// Builds the index on a worker from startup so the title's threads do not
/// wait for it; a caller that asks before it is done waits on the `OnceLock`
/// inside `programs`, which is correct and rare (the wait is logged).
pub(super) fn prime() {
    let _ = std::thread::Builder::new().name("victory-programs".into()).spawn(|| {
        programs();
    });
}

/// `programs()`, logging how long the caller waited when the worker was not
/// done yet, since the title's thread may be the one asking.
fn programs_waited() -> &'static HashMap<victory_shader::pieces::ProgramHash, Known> {
    let began = std::time::Instant::now();
    let map = programs();
    let waited = began.elapsed();
    if waited.as_millis() >= 5 {
        tracing::info!(ms = waited.as_secs_f64() * 1e3, "a pipeline from pieces waited for the index of the book");
    }
    map
}

/// The logged hashes of the title's containers kept by earlier runs that hold
/// `program`. Empty when the book has none, in which case a pipeline from
/// pieces is matched by nothing and behaves as it did before.
pub(super) fn stock_hashes(program: &victory_shader::pieces::ProgramHash) -> Vec<String> {
    programs_waited().get(program).map(|k| k.stock.clone()).unwrap_or_default()
}

/// Whether a title container of `program` carries the name `name` (one of
/// `NAMES`).
pub(super) fn program_named(program: &victory_shader::pieces::ProgramHash, name: &[u8]) -> bool {
    let Some(bit) = NAMES.iter().position(|n| *n == name) else { return false };
    programs_waited().get(program).is_some_and(|k| k.named >> bit & 1 != 0)
}

/// Calls `f` with each pipeline's number and entry; where the last whole one
/// ends.
fn walk_pipelines(dir: &Path, mut f: impl FnMut(usize, &[u8])) -> u64 {
    let bytes = std::fs::read(dir.join(PIPELINES)).unwrap_or_default();
    let (mut at, mut n) = (0, 0);
    while let Some(head) = bytes.get(at..at + 4) {
        let size = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let Some(entry) = bytes.get(at + 4..at + 4 + size) else { break };
        f(n, entry);
        (at, n) = (at + 4 + size, n + 1);
    }
    at as u64
}

/// How many pipelines are known in `dir`.
pub fn count(dir: &Path) -> usize {
    let mut n = 0;
    walk_pipelines(dir, |_, _| n += 1);
    n
}

/// The key of each pipeline known in `dir`, as the early list has it.
pub fn keys(dir: &Path) -> Vec<String> {
    let mut all = Vec::new();
    walk_pipelines(dir, |_, e| {
        all.push(e.get(..16).unwrap_or_default().iter().map(|b| format!("{b:02x}")).collect());
    });
    all
}

/// What a driver's cached pipelines depend on besides the title's: the
/// translator with its settings, and the driver.
pub fn fingerprint() -> String {
    let mut h = blake3::Hasher::new();
    h.update(&crate::shader_cache::translation());
    if let Some(host) = crate::host::host() {
        h.update(&host.driver().to_le_bytes());
    }
    h.update(&super::samplers::order());
    h.finalize().to_hex()[..16].to_owned()
}

/// The translations of earlier runs are in `dir` (the pipeline library is
/// not used: what is made here is the driver's to keep).
pub fn use_translations(dir: PathBuf) {
    crate::shader_cache::use_shader_cache(dir);
}

/// The translations and the host pipeline library of a run are in the cache
/// folder `shaders`'s neighbours: what [`make`] makes from now on is stored in
/// that library (`victory preload`), which the runs load at their start.
pub fn use_library(shaders: PathBuf) {
    super::library::use_pipeline_cache(shaders.with_file_name("pipelines"));
    crate::shader_cache::use_shader_cache(shaders);
}

/// Writes the library [`use_library`] filled.
pub fn keep_library() {
    super::library::save(true);
}

/// Makes the host pipelines of those of `which` (numbers in the file) and
/// lets each go again: the driver's cache on disk keeps them. `each` is told
/// the number before, and what came of it after: milliseconds and whether
/// there is one.
pub fn make(dir: &Path, which: &[usize], mut each: impl FnMut(usize, Option<(f64, bool)>)) {
    let mut places = HashMap::new();
    walk_shaders(dir, |stage, hash, at, len| {
        places.insert(hash, (stage, at, len));
    });
    let Ok(mut file) = std::fs::File::open(dir.join(SHADERS)) else { return };
    let wanted: HashSet<usize> = which.iter().copied().collect();
    let mut entries = HashMap::new();
    walk_pipelines(dir, |n, bytes| {
        if wanted.contains(&n) {
            entries.insert(n, bytes.to_vec());
        }
    });
    let mut read = |hash: &[u8; 32]| {
        let &(_, at, len) = places.get(hash)?;
        let mut bytes = vec![0; len as usize];
        file.seek(SeekFrom::Start(at)).ok()?;
        file.read_exact(&mut bytes).ok()?;
        Some(bytes)
    };
    for n in which {
        let Some(entry) = entries.get(n) else { continue };
        each(*n, None);
        let began = std::time::Instant::now();
        let made = parse(entry, &mut read)
            .and_then(|(shaders, g)| HostPipeline::new(&shaders, g.as_deref()))
            .is_some();
        each(*n, Some((began.elapsed().as_secs_f64() * 1e3, made)));
    }
}

#[allow(clippy::type_complexity)]
fn parse(
    entry: &[u8],
    read: &mut impl FnMut(&[u8; 32]) -> Option<Vec<u8>>,
) -> Option<(Vec<Arc<Shader>>, Option<Box<GraphicsState>>)> {
    let mut rest = entry.get(16..)?;
    let (&count, after) = rest.split_first()?;
    rest = after;
    let mut shaders = Vec::new();
    for _ in 0..count {
        let (head, after) = rest.split_first_chunk::<33>()?;
        rest = after;
        let mut hash = [0; 32];
        hash.copy_from_slice(&head[1..]);
        shaders.push(super::shader(STAGES.get(usize::from(head[0]))?, &read(&hash)?));
    }
    let (&graphics, rest) = rest.split_first()?;
    let g = match graphics {
        0 => None,
        _ => Some(Box::new(GraphicsState::get(rest)?)),
    };
    Some((shaders, g))
}

/// Calls `$f` with each number of a graphics state but its input elements',
/// as a place of 32 bits, in one order.
macro_rules! numbers {
    ($g:expr, $f:ident) => {{
        $f!($g.blend.AlphaToCoverageEnable.0);
        $f!($g.blend.IndependentBlendEnable.0);
        for t in &mut $g.blend.RenderTarget {
            $f!(t.BlendEnable.0);
            $f!(t.LogicOpEnable.0);
            $f!(t.SrcBlend.0);
            $f!(t.DestBlend.0);
            $f!(t.BlendOp.0);
            $f!(t.SrcBlendAlpha.0);
            $f!(t.DestBlendAlpha.0);
            $f!(t.BlendOpAlpha.0);
            $f!(t.LogicOp.0);
            $f!(t.RenderTargetWriteMask);
        }
        $f!($g.sample_mask);
        $f!($g.rasterizer.FillMode.0);
        $f!($g.rasterizer.CullMode.0);
        $f!($g.rasterizer.FrontCounterClockwise.0);
        $f!($g.rasterizer.DepthBias);
        $f!($g.rasterizer.DepthClipEnable.0);
        $f!($g.rasterizer.MultisampleEnable.0);
        $f!($g.rasterizer.AntialiasedLineEnable.0);
        $f!($g.rasterizer.ForcedSampleCount);
        $f!($g.rasterizer.ConservativeRaster.0);
        $f!($g.depth.DepthEnable.0);
        $f!($g.depth.DepthWriteMask.0);
        $f!($g.depth.DepthFunc.0);
        $f!($g.depth.StencilEnable.0);
        $f!($g.depth.StencilReadMask);
        $f!($g.depth.StencilWriteMask);
        for face in [&mut $g.depth.FrontFace, &mut $g.depth.BackFace] {
            $f!(face.StencilFailOp.0);
            $f!(face.StencilDepthFailOp.0);
            $f!(face.StencilPassOp.0);
            $f!(face.StencilFunc.0);
        }
        $f!($g.strip_cut.0);
        $f!($g.topology.0);
        $f!($g.targets.NumRenderTargets);
        for format in &mut $g.targets.RTFormats {
            $f!(format.0);
        }
        $f!($g.depth_format.0);
        $f!($g.samples.Count);
        $f!($g.samples.Quality);
    }};
}

impl GraphicsState {
    /// The state as bytes.
    fn put(&self, out: &mut Vec<u8>) {
        out.push(u8::from(self.depth_bounds));
        for f in [self.rasterizer.DepthBiasClamp, self.rasterizer.SlopeScaledDepthBias] {
            out.extend(f.to_le_bytes());
        }
        let mut plain = GraphicsState {
            blend: self.blend,
            sample_mask: self.sample_mask,
            rasterizer: self.rasterizer,
            depth: self.depth,
            strip_cut: self.strip_cut,
            topology: self.topology,
            targets: self.targets,
            depth_format: self.depth_format,
            samples: self.samples,
            ..Default::default()
        };
        macro_rules! write {
            ($x:expr) => {
                out.extend(($x as u32).to_le_bytes())
            };
        }
        numbers!(plain, write);
        out.push(self.inputs.len() as u8);
        for (e, name) in self.inputs.iter().zip(&self.names) {
            let name = name.as_bytes();
            out.push(name.len() as u8);
            out.extend(name);
            for v in [
                e.SemanticIndex,
                e.Format.0 as u32,
                e.InputSlot,
                e.AlignedByteOffset,
                e.InputSlotClass.0 as u32,
                e.InstanceDataStepRate,
            ] {
                out.extend(v.to_le_bytes());
            }
        }
    }

    fn get(bytes: &[u8]) -> Option<GraphicsState> {
        let mut g = GraphicsState::default();
        let (&bounds, mut rest) = bytes.split_first()?;
        g.depth_bounds = bounds != 0;
        let mut next = || {
            let (head, after) = rest.split_first_chunk::<4>()?;
            rest = after;
            Some(u32::from_le_bytes(*head))
        };
        g.rasterizer.DepthBiasClamp = f32::from_bits(next()?);
        g.rasterizer.SlopeScaledDepthBias = f32::from_bits(next()?);
        macro_rules! read {
            ($x:expr) => {
                $x = next()? as _
            };
        }
        numbers!(g, read);
        let (&count, mut rest) = rest.split_first()?;
        let mut elements = Vec::new();
        for _ in 0..count {
            let (&len, after) = rest.split_first()?;
            let (name, after) = after.split_at_checked(usize::from(len))?;
            let (numbers, after) = after.split_first_chunk::<24>()?;
            rest = after;
            let mut v = [0u32; 6];
            for (v, b) in v.iter_mut().zip(numbers.chunks_exact(4)) {
                *v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            }
            elements.push((CString::new(name).ok()?, v));
        }
        g.names = elements.iter().map(|e| e.0.clone()).collect();
        g.inputs = g
            .names
            .iter()
            .zip(&elements)
            .map(|(name, (_, v))| D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(name.as_ptr().cast()),
                SemanticIndex: v[0],
                Format: DXGI_FORMAT(v[1] as i32),
                InputSlot: v[2],
                AlignedByteOffset: v[3],
                InputSlotClass: D3D12_INPUT_CLASSIFICATION(v[4] as i32),
                InstanceDataStepRate: v[5],
            })
            .collect();
        Some(g)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_comes_back_as_it_was() {
        let mut g = GraphicsState { sample_mask: 7, depth_bounds: true, ..Default::default() };
        g.rasterizer.DepthBias = -3;
        g.rasterizer.SlopeScaledDepthBias = 1.25;
        g.blend.RenderTarget[3].RenderTargetWriteMask = 5;
        g.depth.StencilReadMask = 9;
        g.targets.NumRenderTargets = 2;
        g.targets.RTFormats[1] = DXGI_FORMAT_R8_UNORM;
        g.depth_format = DXGI_FORMAT_D32_FLOAT;
        g.names = vec![CString::new("POSITION").unwrap(), CString::new("T").unwrap()];
        g.inputs = g
            .names
            .iter()
            .enumerate()
            .map(|(n, name)| D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(name.as_ptr().cast()),
                SemanticIndex: n as u32,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 2,
                AlignedByteOffset: D3D12_APPEND_ALIGNED_ELEMENT,
                InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA,
                InstanceDataStepRate: 4,
            })
            .collect();
        let mut bytes = Vec::new();
        g.put(&mut bytes);
        let back = GraphicsState::get(&bytes).expect("whole");
        assert_eq!(back.describe(), g.describe());
        assert!(GraphicsState::get(&bytes[..bytes.len() - 1]).is_none());
        assert!(GraphicsState::get(&bytes[..9]).is_none());
    }

    /// A container laid out like the title's own: header, one part (HASH: a
    /// dword of flags, then the digest), and `extra` bytes after the part that
    /// stand for a variant of the same program.
    fn stock_with(digest: [u8; 16], extra: &[u8]) -> Vec<u8> {
        let total = 0x24 + 8 + 20 + extra.len() as u32;
        let mut c = b"DXBC".to_vec();
        c.extend([0u8; 16]);
        c.extend(1u32.to_le_bytes());
        c.extend(total.to_le_bytes());
        c.extend(1u32.to_le_bytes());
        c.extend(0x24u32.to_le_bytes());
        c.extend(b"HASH");
        c.extend(20u32.to_le_bytes());
        c.extend(0u32.to_le_bytes());
        c.extend(digest);
        c.extend(extra);
        c
    }

    fn stock(digest: [u8; 16]) -> Vec<u8> {
        stock_with(digest, &[])
    }

    /// One record of the shader book: stage, blake3, length, container.
    fn record(stage: u8, container: &[u8]) -> Vec<u8> {
        let mut r = vec![stage];
        r.extend(blake3::hash(container).as_bytes());
        r.extend((container.len() as u32).to_le_bytes());
        r.extend(container);
        r
    }

    fn logged(container: &[u8]) -> String {
        blake3::hash(container).to_hex()[..16].to_owned()
    }

    // programs() and stock_hashes() are not tested: they read the process-wide
    // cache folder, which a test must not touch.
    #[test]
    fn the_book_gives_the_stock_hashes_of_a_program() {
        let dir = std::env::temp_dir().join(format!("victory-programs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (d, e) = ([0x11u8; 16], [0x22u8; 16]);
        let (one, two) = (stock(d), stock(e));
        let made = b"VPCS filler".to_vec();
        let variant = stock_with(d, &[7, 7, 7, 7]);
        let mut book = Vec::new();
        for c in [&one, &two, &made, &variant] {
            book.extend(record(5, c));
        }
        // a torn tail
        book.extend_from_slice(&record(5, &one)[..20]);
        std::fs::write(dir.join(SHADERS), &book).unwrap();
        let (map, skipped) = index(&dir);
        assert_eq!(map.len(), 2);
        // the one container with no HASH part (the one made from pieces)
        assert_eq!(skipped, 1);
        assert_eq!(map[&d].stock, [logged(&one), logged(&variant)]);
        assert_eq!(map[&e].stock, [logged(&two)]);
        assert_eq!(map[&d].named, 0);
        assert!(index(&dir.join("missing")).0.is_empty());
        // a container that carries a name from `NAMES` marks its program with that name only
        let named = stock_with([0x33; 16], b"..CS_ScreenProbes_ActivateRays\0");
        std::fs::write(dir.join(SHADERS), record(5, &named)).unwrap();
        let (map, _) = index(&dir);
        assert_eq!(map[&[0x33; 16]].named, 0b10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_torn_end_is_not_read() {
        let dir = std::env::temp_dir().join(format!("victory-known-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut bytes = Vec::new();
        for n in 0..3u8 {
            bytes.extend(20u32.to_le_bytes());
            bytes.extend([n; 20]);
        }
        std::fs::write(dir.join(PIPELINES), &bytes[..bytes.len() - 5]).unwrap();
        assert_eq!(count(&dir), 2);
        assert_eq!(keys(&dir)[1], "01".repeat(16));
        assert_eq!(walk_pipelines(&dir, |_, _| ()), 48);
        let mut shader = vec![5u8];
        shader.extend([9; 32]);
        shader.extend(4u32.to_le_bytes());
        shader.extend([1, 2, 3, 4]);
        let one = shader.len() as u64;
        shader.extend_from_slice(&shader.clone()[..20]);
        std::fs::write(dir.join(SHADERS), &shader).unwrap();
        let mut seen = Vec::new();
        assert_eq!(walk_shaders(&dir, |stage, hash, at, len| seen.push((stage, hash[0], at, len))), one);
        assert_eq!(seen, [(5, 9, 37, 4)]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
