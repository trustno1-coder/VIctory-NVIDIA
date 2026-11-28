//! `XboxBvhBuilder.dll`: the console's ray-tracing structure builder, as the
//! title calls it. Not the 52 compute programs it records (those the layer
//! sees as dispatches, `victory_gpu::pipeline`), but the class in front of
//! them: one object the title makes at start, and seven of its functions,
//! which take Direct3D's own descriptions of what to build (geometry and
//! instances by address, where the structure goes, where its scratch memory
//! is, where a size is to be stored afterwards).
//!
//! With `VICTORY_RT_NATIVE=1` the title's imports of those seven are bound to
//! the functions here ([`Interpose`]), which call the title's own DLL and
//! keep count: how many structures the title asks to be built, unpacked and
//! copied, and how long the DLL takes to record that on the title's thread.
//! The same switch lets the builder's programs run (as `VICTORY_BVH_BUILDER=1`
//! does, `victory_gpu::pipeline`), so the structures are built on the card,
//! in the layer's copies of title memory, and nothing of them is staged in
//! between. Without the switch the imports are bound as before and none of
//! this is reached.
//!
//! `VICTORY_RT_TRACE=1` (with the switch) also logs the first calls of each
//! kind with the descriptions they were given, as dwords: the batched build's
//! description is the console's own and its layout is not public, so it is
//! read off a run before anything here relies on it.
//!
//! This is the place where the title says what it wants in terms the host
//! understands. A dispatch of the builder names its memory in a spilled table
//! of addresses and constants (`victory_shader::spill`); a call here names a
//! destination, a source and a size.
// The exported names are the title's.
#![allow(non_snake_case)]
// Title code calls these through its import table.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use victory_loader::{ImportResolver, Provenance, Resolution};
use victory_pe::ImportSymbol;

/// The DLL the title imports the builder from (it ships beside the
/// executable and is mapped like it).
pub const BUILDER_DLL: &str = "XboxBvhBuilder.dll";

// The seven functions, by their place in [`NAMES`].
const CONSTRUCT: usize = 0;
const DESTROY: usize = 1;
const BUILD: usize = 2;
const COPY: usize = 3;
const PREBUILD: usize = 4;
const REHYDRATE: usize = 5;
const MORTON: usize = 6;

/// What the title imports, as its import table spells them (retail build
/// 1.0.2.1; the DLL exports five more that the title does not use).
const NAMES: [&str; 7] = [
    "??0XboxBvhBuilder@@QEAA@PEAUID3D12Device11@@_N@Z",
    "??1XboxBvhBuilder@@QEAA@XZ",
    "?BuildRaytracingAccelerationStructureBatched2X@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList6@@IPEBUD3D12XBOX_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_BATCHED_DESC2@@@Z",
    "?CopyRaytracingAccelerationStructure@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList4@@_K1W4D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE@@@Z",
    "?GetRaytracingAccelerationStructurePrebuildInfo@XboxBvhBuilder@@QEAAXPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS@@PEBUD3D12XBOX_XDXR_STREAM_DESC@@PEAUD3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO@@@Z",
    "?RehydrateRaytracingAccelerationStructureBatched2X@XboxBvhBuilder@@QEAAJPEAUID3D12GraphicsCommandList8@@IPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@PEB_K@Z",
    "?Set64BitMortonCodes@XboxBvhBuilder@@QEAAX_N@Z",
];

/// Short names for the log, in the same order.
const KINDS: [&str; 7] = ["construct", "destroy", "build", "copy", "prebuild", "rehydrate", "morton"];

/// The title's own functions, by [`NAMES`].
static ORIGINALS: [AtomicUsize; 7] = [const { AtomicUsize::new(0) }; 7];

// Member functions of a class: the object first, then the arguments, in the
// host's one calling convention.
type Construct = unsafe extern "system" fn(*mut c_void, *mut c_void, u8) -> *mut c_void;
type Destroy = unsafe extern "system" fn(*mut c_void);
type Build = unsafe extern "system" fn(*mut c_void, *mut c_void, u32, *const c_void);
type Copy = unsafe extern "system" fn(*mut c_void, *mut c_void, u64, u64, i32);
type Prebuild = unsafe extern "system" fn(*mut c_void, *const Inputs, *const c_void, *mut PrebuildInfo);
type Rehydrate = unsafe extern "system" fn(*mut c_void, *mut c_void, u32, *const BuildDesc, *const u64) -> i32;
type Morton = unsafe extern "system" fn(*mut c_void, u8);

/// What is to be built, as Direct3D 12 describes it on every platform
/// (`D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS`): top or bottom
/// level, build flags, how many instances or geometries, how they are laid
/// out, and where they are (instances: a title address; geometries: an array
/// of descriptions, or of pointers to them).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Inputs {
    kind: u32,
    flags: u32,
    count: u32,
    layout: u32,
    descs: u64,
}

/// One build or one unpacking (`D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC`):
/// where the structure goes, what it is made of, the structure it updates or
/// is unpacked from, and its scratch memory. All title addresses.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct BuildDesc {
    dest: u64,
    inputs: Inputs,
    source: u64,
    scratch: u64,
}

/// What a build needs at most (`D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO`):
/// bytes of the result, of scratch memory, and of scratch memory for an update.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct PrebuildInfo {
    result: u64,
    scratch: u64,
    update_scratch: u64,
}

/// One build of a batch (`D3D12XBOX_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_BATCHED_DESC2`,
/// not public). Read off traced calls: every batch seen was an array of
/// 80-byte entries, each a [`BuildDesc`] followed by the facts the title
/// wants back about the structure once it is built.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct BatchedDesc {
    build: BuildDesc,
    /// How many [`PostbuildDesc`] `postbuild` points at (1 in every call
    /// seen, of builds that allow compaction).
    postbuild_count: u32,
    /// Zero in every call seen (padding before the pointer).
    unused: u32,
    /// Memory of the title's (its stack, in the calls seen).
    postbuild: u64,
    /// Zero in every call seen.
    rest: [u64; 2],
}

/// A fact asked for about a built structure
/// (`D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC`): the
/// title address the builder's programs store it at, and which fact (0:
/// the size the structure compacts to). The title reads that address a
/// frame or two later and allocates the compacted copy by it.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct PostbuildDesc {
    dest: u64,
    kind: u32,
    unused: u32,
}

const _: () = assert!(size_of::<Inputs>() == 24);
const _: () = assert!(size_of::<BuildDesc>() == 48);
const _: () = assert!(size_of::<BatchedDesc>() == 80);
const _: () = assert!(size_of::<PostbuildDesc>() == 16);
const _: () = assert!(size_of::<PrebuildInfo>() == 24);

/// `VICTORY_RT_NATIVE=1`: the title's ray tracing with the builder's calls
/// through here. Read once; the imports are bound before the title runs.
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_RT_NATIVE").is_some_and(|v| v == "1"))
}

/// `VICTORY_RT_TRACE=1`: the first calls of each kind are logged in full.
fn traced() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_RT_TRACE").is_some_and(|v| v == "1"))
}

/// Calls of each kind logged in full, at most.
const TRACED: u64 = 12;
/// Bytes of a description of unknown length that are logged, at most.
const PEEK: usize = 256;

/// Binds the title's imports of [`BUILDER_DLL`] to the functions here when
/// `VICTORY_RT_NATIVE=1`, keeping what `inner` binds them to (the title's own
/// DLL) for them to call; otherwise binds as `inner` does.
pub struct Interpose<'a> {
    pub inner: &'a dyn ImportResolver,
}

impl ImportResolver for Interpose<'_> {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        let found = self.inner.resolve(dll, symbol);
        if !dll.eq_ignore_ascii_case(BUILDER_DLL) || !on() {
            return found;
        }
        // Only what the title's DLL itself provides is wrapped: a stub for a
        // missing function has nothing behind it to call.
        match (&found, symbol) {
            (&Resolution::Found { address, provenance: Provenance::Title }, ImportSymbol::Name { name, .. }) => {
                match interpose(name, address) {
                    Some(ours) => Resolution::Found { address: ours, provenance: Provenance::Hle },
                    None => found,
                }
            }
            _ => found,
        }
    }
}

/// Keeps `original` as the title's `name` and gives ours, if we have one.
fn interpose(name: &str, original: usize) -> Option<usize> {
    let i = NAMES.iter().position(|n| *n == name)?;
    if original == 0 {
        return None;
    }
    ORIGINALS[i].store(original, Relaxed);
    let ours = [
        construct as *const () as usize,
        destroy as *const () as usize,
        build_batched as *const () as usize,
        copy_structure as *const () as usize,
        prebuild_info as *const () as usize,
        rehydrate_batched as *const () as usize,
        set_morton as *const () as usize,
    ][i];
    tracing::info!(
        kind = KINDS[i],
        original = format_args!("{original:#x}"),
        "ray tracing: the title's call of its structure builder goes through ours (VICTORY_RT_NATIVE)"
    );
    Some(ours)
}

fn original(i: usize) -> Option<*const ()> {
    let address = ORIGINALS.get(i)?.load(Relaxed);
    (address != 0).then_some(address as *const ())
}

/// Since the last report: calls of each kind, the structures they named, and
/// the time the title's DLL took for them on the calling thread (ns).
struct Counts {
    calls: [AtomicU64; 7],
    structures: [AtomicU64; 7],
    nanos: [AtomicU64; 7],
    /// Calls of each kind since the start (for [`TRACED`]).
    ever: [AtomicU64; 7],
}

static COUNTS: Counts = Counts {
    calls: [const { AtomicU64::new(0) }; 7],
    structures: [const { AtomicU64::new(0) }; 7],
    nanos: [const { AtomicU64::new(0) }; 7],
    ever: [const { AtomicU64::new(0) }; 7],
};

/// When the counts were last reported.
static REPORTED: Mutex<Option<Instant>> = Mutex::new(None);
/// How often they are.
const REPORT: Duration = Duration::from_secs(10);

/// Notes a call of kind `i` that named `structures` and took `took`; whether
/// it is one of the first, to be logged in full.
fn note(i: usize, structures: u64, took: Duration) {
    if i >= KINDS.len() {
        return;
    }
    COUNTS.calls[i].fetch_add(1, Relaxed);
    COUNTS.structures[i].fetch_add(structures, Relaxed);
    COUNTS.nanos[i].fetch_add(u64::try_from(took.as_nanos()).unwrap_or(u64::MAX), Relaxed);
    report();
}

/// Whether this call of kind `i` is logged in full (the first of them, with
/// `VICTORY_RT_TRACE=1`).
fn first(i: usize) -> bool {
    let Some(ever) = COUNTS.ever.get(i) else { return false };
    let n = ever.fetch_add(1, Relaxed);
    traced() && n < TRACED
}

/// Every ten seconds: what the title asked of the builder since the last
/// line, a second, and what recording it cost the calling thread.
fn report() {
    // (Not waited for: another thread's call reports as well.)
    let Ok(mut last) = REPORTED.try_lock() else { return };
    let now = Instant::now();
    let since = match *last {
        Some(at) => now.duration_since(at),
        None => {
            *last = Some(now);
            return;
        }
    };
    if since < REPORT {
        return;
    }
    *last = Some(now);
    drop(last);
    let seconds = since.as_secs_f64().max(1e-3);
    let take = |a: &AtomicU64| a.swap(0, Relaxed);
    let per = |i: usize| take(&COUNTS.structures[i]) as f64 / seconds;
    // The three that name structures, then the time of all seven together.
    let (built, unpacked) = (per(BUILD), per(REHYDRATE));
    let copies = take(&COUNTS.calls[COPY]) as f64 / seconds;
    let calls: u64 = [BUILD, REHYDRATE, PREBUILD].iter().map(|&i| take(&COUNTS.calls[i])).sum();
    let nanos: u64 = COUNTS.nanos.iter().map(take).sum();
    tracing::info!(
        "ray tracing (VICTORY_RT_NATIVE): structures built {built:.1}/s, unpacked {unpacked:.1}/s, copied {copies:.1}/s; \
         {:.1} calls/s of the builder, {:.3} ms/s on the title's threads",
        calls as f64 / seconds,
        nanos as f64 / 1e6 / seconds,
    );
}

/// The bytes at `at` up to the end of its 4 KiB page, `most` of them at most:
/// what of a description can be read without knowing how long it is.
fn peek<'a>(at: *const u8, most: usize) -> &'a [u8] {
    if at.is_null() {
        return &[];
    }
    let left = 4096 - (at as usize & 4095);
    // SAFETY: a description the title passes is readable at `at`, and a page
    // that is readable at one byte is readable to its end.
    unsafe { std::slice::from_raw_parts(at, left.min(most)) }
}

/// `bytes` as dwords, eight to a line, each line with its byte offset.
fn dwords(bytes: &[u8]) -> String {
    let mut text = String::new();
    for (line, row) in bytes.chunks(32).enumerate() {
        text.push_str(&format!("\n    +{:#05x}:", line * 32));
        for word in row.chunks_exact(4) {
            let word = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
            text.push_str(&format!(" {word:08x}"));
        }
    }
    text
}

/// The title's builder object is made: `flag` is the constructor's second
/// argument (not named publicly).
extern "system" fn construct(this: *mut c_void, device: *mut c_void, flag: u8) -> *mut c_void {
    let Some(f) = original(CONSTRUCT) else {
        tracing::error!("ray tracing: the structure builder's constructor is not bound; the title's object is left as it is");
        return this;
    };
    let started = Instant::now();
    // SAFETY: the title's own constructor, with the title's arguments.
    let made = unsafe { std::mem::transmute::<*const (), Construct>(f)(this, device, flag) };
    note(CONSTRUCT, 0, started.elapsed());
    tracing::info!(
        object = format_args!("{:#x}", this as usize),
        device = format_args!("{:#x}", device as usize),
        flag,
        "ray tracing: the title made its structure builder"
    );
    made
}

extern "system" fn destroy(this: *mut c_void) {
    let Some(f) = original(DESTROY) else { return };
    let started = Instant::now();
    // SAFETY: the title's own destructor, on the title's object.
    unsafe { std::mem::transmute::<*const (), Destroy>(f)(this) };
    note(DESTROY, 0, started.elapsed());
}

/// `count` builds recorded on `list`, described by the console's batched
/// descriptions at `descs` ([`BatchedDesc`]; when traced, the first of
/// them are logged as read and as dwords, with the facts each asks back).
extern "system" fn build_batched(this: *mut c_void, list: *mut c_void, count: u32, descs: *const c_void) {
    let Some(f) = original(BUILD) else { return };
    if first(BUILD) {
        let shown = (count as usize).min(4);
        let batch = descs.cast::<BatchedDesc>();
        // SAFETY: `count` descriptions at `descs`, as the title says; null is skipped.
        let some: Vec<BatchedDesc> = if batch.is_null() {
            Vec::new()
        } else {
            (0..shown).map(|n| unsafe { batch.add(n).read_unaligned() }).collect()
        };
        // What each asks back: read through `peek`, which stops at the end
        // of the page (the count is the title's and is not trusted).
        let asked: Vec<Vec<PostbuildDesc>> = some
            .iter()
            .map(|d| {
                let bytes = peek(d.postbuild as *const u8, (d.postbuild_count as usize).min(4) * size_of::<PostbuildDesc>());
                bytes
                    .chunks_exact(size_of::<PostbuildDesc>())
                    // SAFETY: sixteen readable bytes, read unaligned as plain integers.
                    .map(|c| unsafe { c.as_ptr().cast::<PostbuildDesc>().read_unaligned() })
                    .collect()
            })
            .collect();
        tracing::info!(
            list = format_args!("{:#x}", list as usize),
            count,
            descs = format_args!("{:#x}", descs as usize),
            some = format_args!("{some:x?}"),
            asked = format_args!("{asked:x?}"),
            "ray tracing trace: build batched{}",
            dwords(peek(descs.cast(), PEEK))
        );
        // What the first build is made of: its geometry descriptions (a
        // bottom level's; the console's layout of them is not known to be
        // the PC's) or the start of its instances (a top level's), as
        // dwords. The title's memory, which its builder reads on the CPU
        // for the sizes.
        if let Some(first) = some.first() {
            tracing::info!(
                kind = first.build.inputs.kind,
                count = first.build.inputs.count,
                layout = first.build.inputs.layout,
                "ray tracing trace: the first build's inputs{}",
                dwords(peek(first.build.inputs.descs as *const u8, PEEK))
            );
        }
    }
    extents(this, descs.cast(), count);
    let started = Instant::now();
    // SAFETY: the title's own function, with the title's arguments.
    unsafe { std::mem::transmute::<*const (), Build>(f)(this, list, count, descs) };
    note(BUILD, u64::from(count), started.elapsed());
}

/// Builds of one batch looked at for their extents, at most (the title's
/// batches are of 16).
const BATCH_MOST: u32 = 4096;

/// By a built structure's address: the title address its build was told to
/// store the compacted size at (the first fact of its `PostbuildDesc`s that
/// is of kind 0). Read again when the structure is copied.
static SIZE_SLOTS: Mutex<std::collections::BTreeMap<u64, u64>> = Mutex::new(std::collections::BTreeMap::new());
/// Slots kept at most; past it all are let go (a copy whose slot is not
/// known is sized by the GPU side's guess, as without this).
const SLOTS_MOST: usize = 1 << 16;

/// Tells the GPU side how long each structure and each scratch memory of a
/// batch is, before the builder's dispatches for it are recorded
/// (`victory_gpu::structure_extent`): the dispatches name both by their
/// start alone. The lengths are the builder's own, from the calculation the
/// title used to allocate them (asked again here: it runs on the CPU and
/// records nothing).
fn extents(this: *mut c_void, batch: *const BatchedDesc, count: u32) {
    let Some(sizes) = original(PREBUILD) else { return };
    if batch.is_null() {
        return;
    }
    for n in 0..count.min(BATCH_MOST) as usize {
        // SAFETY: `count` descriptions at `batch`, as the title says.
        let desc = unsafe { batch.add(n).read_unaligned() };
        let build = desc.build;
        if build.dest == 0 {
            continue;
        }
        // Where the compacted size is to be stored, for the copy to come.
        let asked = peek(desc.postbuild as *const u8, (desc.postbuild_count as usize).min(4) * size_of::<PostbuildDesc>());
        let slot = asked
            .chunks_exact(size_of::<PostbuildDesc>())
            // SAFETY: sixteen readable bytes, read unaligned as plain integers.
            .map(|c| unsafe { c.as_ptr().cast::<PostbuildDesc>().read_unaligned() })
            .find(|fact| fact.kind == 0 && fact.dest != 0);
        if let Some(fact) = slot {
            let mut slots = SIZE_SLOTS.lock().unwrap_or_else(|e| e.into_inner());
            if slots.len() >= SLOTS_MOST {
                slots.clear();
            }
            slots.insert(build.dest, fact.dest);
        }
        let mut needs = PrebuildInfo::default();
        // SAFETY: the title's own function, as the title calls it (no stream
        // description in any call seen), on a copy of the title's inputs.
        unsafe { std::mem::transmute::<*const (), Prebuild>(sizes)(this, &raw const build.inputs, std::ptr::null(), &raw mut needs) };
        victory_gpu::structure_extent(build.dest, needs.result);
        if build.scratch != 0 {
            // (An update has a scratch size of its own; the larger covers both.)
            victory_gpu::structure_extent(build.scratch, needs.scratch.max(needs.update_scratch));
        }
    }
}

/// A structure copied (or compacted, by `mode`) from `source` to `dest`, both
/// title addresses, recorded on `list`.
extern "system" fn copy_structure(this: *mut c_void, list: *mut c_void, dest: u64, source: u64, mode: i32) {
    let Some(f) = original(COPY) else { return };
    if first(COPY) {
        tracing::info!(
            list = format_args!("{:#x}", list as usize),
            dest = format_args!("{dest:#x}"),
            source = format_args!("{source:#x}"),
            mode,
            "ray tracing trace: copy"
        );
    }
    // The copy's length is the compacted size the title read back from the
    // slot its build named (`SIZE_SLOTS`); the GPU side reads it there.
    let slot = SIZE_SLOTS.lock().unwrap_or_else(|e| e.into_inner()).get(&source).copied().unwrap_or(0);
    let sized = victory_gpu::structure_copied(dest, source, slot);
    if traced() && COUNTS.ever[COPY].load(Relaxed) <= 64 {
        tracing::info!(
            dest = format_args!("{dest:#x}"),
            source = format_args!("{source:#x}"),
            slot = format_args!("{slot:#x}"),
            whole = ?victory_gpu::structure_extent_from(source),
            compacted = ?sized,
            "ray tracing trace: a copy's length, from the size the title read (none: what its slot held was not a size of this structure)"
        );
    }
    let started = Instant::now();
    // SAFETY: the title's own function, with the title's arguments.
    unsafe { std::mem::transmute::<*const (), Copy>(f)(this, list, dest, source, mode) };
    note(COPY, 1, started.elapsed());
}

/// The sizes a build of `inputs` needs at most, stored at `info` (computed
/// on the CPU by the title's DLL; nothing is recorded).
extern "system" fn prebuild_info(this: *mut c_void, inputs: *const Inputs, stream: *const c_void, info: *mut PrebuildInfo) {
    let Some(f) = original(PREBUILD) else { return };
    let started = Instant::now();
    // SAFETY: the title's own function, with the title's arguments.
    unsafe { std::mem::transmute::<*const (), Prebuild>(f)(this, inputs, stream, info) };
    note(PREBUILD, 1, started.elapsed());
    if first(PREBUILD) {
        // SAFETY: the title's description, read where it is; null is skipped.
        let asked = (!inputs.is_null()).then(|| unsafe { inputs.read_unaligned() });
        // SAFETY: what the title's function has just stored; null is skipped.
        let needs = (!info.is_null()).then(|| unsafe { info.read_unaligned() });
        tracing::info!(
            ?asked,
            ?needs,
            stream = format_args!("{:#x}", stream as usize),
            "ray tracing trace: prebuild info{}",
            dwords(peek(stream.cast(), 64))
        );
    }
}

/// `count` packed structures unpacked, recorded on `list`: each of `descs`
/// names where the packed bytes are and where the structure goes; `more` is
/// an array of one value a structure (not named publicly).
extern "system" fn rehydrate_batched(
    this: *mut c_void,
    list: *mut c_void,
    count: u32,
    descs: *const BuildDesc,
    more: *const u64,
) -> i32 {
    // (E_NOTIMPL: the title is told the structures were not unpacked.)
    let Some(f) = original(REHYDRATE) else { return 0x8000_4001_u32 as i32 };
    if first(REHYDRATE) {
        let shown = (count as usize).min(4);
        // SAFETY: `count` descriptions at `descs`, as the title says; null is skipped.
        let some: Vec<BuildDesc> = if descs.is_null() {
            Vec::new()
        } else {
            (0..shown).map(|n| unsafe { descs.add(n).read_unaligned() }).collect()
        };
        tracing::info!(
            list = format_args!("{:#x}", list as usize),
            count,
            ?some,
            "ray tracing trace: rehydrate batched, descriptions then the second array{}{}",
            dwords(peek(descs.cast(), shown * size_of::<BuildDesc>())),
            dwords(peek(more.cast(), shown * 8))
        );
    }
    let started = Instant::now();
    // SAFETY: the title's own function, with the title's arguments.
    let result = unsafe { std::mem::transmute::<*const (), Rehydrate>(f)(this, list, count, descs, more) };
    note(REHYDRATE, u64::from(count), started.elapsed());
    if result < 0 {
        tracing::warn!(result = format_args!("{result:#x}"), count, "ray tracing: the title's builder refused to unpack structures");
    }
    result
}

extern "system" fn set_morton(this: *mut c_void, wide: u8) {
    let Some(f) = original(MORTON) else { return };
    // (The title sets it around every batch: only the first few are logged.)
    if first(MORTON) {
        tracing::info!(wide, "ray tracing trace: the title sets the builder's Morton codes to 64 bits (1) or 32 (0)");
    }
    let started = Instant::now();
    // SAFETY: the title's own function, with the title's arguments.
    unsafe { std::mem::transmute::<*const (), Morton>(f)(this, wide) };
    note(MORTON, 0, started.elapsed());
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed;
    impl ImportResolver for Fixed {
        fn resolve(&self, dll: &str, _: &ImportSymbol) -> Resolution {
            if dll.eq_ignore_ascii_case(BUILDER_DLL) {
                Resolution::Found { address: 0x1234, provenance: Provenance::Title }
            } else {
                Resolution::Missing
            }
        }
    }

    /// Without the switch the title's imports bind as before, whatever their
    /// name; the names we serve are the seven the title imports.
    #[test]
    fn without_the_switch_the_titles_own_binds() {
        let name = |n: &str| ImportSymbol::Name { hint: 0, name: n.into() };
        let interposed = Interpose { inner: &Fixed };
        // (The test process has no VICTORY_RT_NATIVE.)
        assert!(!on());
        for n in NAMES {
            assert_eq!(
                interposed.resolve("XBOXBVHBUILDER.DLL", &name(n)),
                Resolution::Found { address: 0x1234, provenance: Provenance::Title }
            );
        }
        assert_eq!(interposed.resolve("other.dll", &name(NAMES[BUILD])), Resolution::Missing);
        assert_eq!(interpose("?CpuValidateResultX@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList4@@PEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@@Z", 1), None);
        assert_eq!(interpose(NAMES[COPY], 0), None);
        assert!(interpose(NAMES[COPY], 0x5678).is_some());
        assert_eq!(original(COPY), Some(0x5678 as *const ()));
        ORIGINALS[COPY].store(0, Relaxed);
    }

    /// A description of unknown length is read to the end of its page and no
    /// further, and shown as dwords with their offsets.
    #[test]
    fn a_description_is_read_to_the_end_of_its_page() {
        let page = vec![0u8; 8192];
        let start = page.as_ptr() as usize;
        // The last 8 bytes of a 4 KiB page inside the allocation.
        let edge = ((start + 4095) & !4095) + 4096 - 8;
        assert!(edge + 8 <= start + page.len());
        assert_eq!(peek(edge as *const u8, PEEK).len(), 8);
        assert_eq!(peek(std::ptr::null(), PEEK).len(), 0);
        assert_eq!(dwords(&[1, 0, 0, 0, 0xff, 0xff, 0, 0x80]), "\n    +0x000: 00000001 8000ffff");
    }
}
