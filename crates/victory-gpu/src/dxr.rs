//! Watches the title's acceleration-structure builder so the compacted
//! size it asks for is present when the title reads it.
//!
//! The title writes a compacted-size query into a batched build element,
//! reads the answer slot a frame or two later and copies the structure into
//! memory of that size. The builder's own compute program stores the answer
//! on the card after the call has returned, so the title can read the slot
//! before it holds this structure's size and copy over its neighbours.
//!
//! With `VICTORY_BVH_SIZE_ANSWER` (on unless `0`, and with the builder's
//! stages enabled), the two builder methods involved are wrapped without
//! changing what the builder runs. The prebuild-info wrapper remembers the
//! maximum result size the builder told the title for each input shape. At
//! the batched build, that maximum is written into every compacted-size slot
//! before the real call and again after it. Later stores from the builder
//! into those slots are kept out of title memory ([`keep_answers`]); a slot
//! asked for another kind of information belongs to the builder again.
//!
//! `VICTORY_BVH_SIZE_ANSWER=0` binds the builder's own methods and leaves
//! its stores alone.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// `VICTORY_BVH_SIZE_ANSWER`'s meaning: on unless `0`.
pub fn parse_answers(value: Option<&str>) -> bool {
    value.is_none_or(|v| v.trim() != "0")
}

/// Whether compacted sizes are answered at the builder's call.
pub fn answers() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| parse_answers(std::env::var("VICTORY_BVH_SIZE_ANSWER").ok().as_deref())
        && crate::pipeline::builder_allowed())
}

/// `D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC`, as the title
/// passes it (48 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildDesc {
    pub dest: u64,
    pub inputs: Inputs,
    /// A refit or update's source structure (0 for a build).
    pub source: u64,
    pub scratch: u64,
}

/// `D3D12_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS` (24 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inputs {
    pub top: bool,
    pub flags: u32,
    /// Instances, or geometry descs.
    pub count: u32,
    /// The descs are an array of pointers (`DescsLayout` 1), else an array.
    pub pointers: bool,
    pub descs: u64,
}

/// `D3D12_RAYTRACING_GEOMETRY_DESC` (56 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Geometry {
    Triangles {
        flags: u32,
        transform: u64,
        index_format: u32,
        vertex_format: u32,
        indices: u32,
        vertices: u32,
        index_buffer: u64,
        vertex_buffer: u64,
        vertex_stride: u64,
    },
    Boxes {
        flags: u32,
        count: u64,
        boxes: u64,
        stride: u64,
    },
}

/// One element of the batched build's array: the standard desc, at 48 the
/// number of postbuild-info descs, and at 56 their address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Element {
    pub desc: BuildDesc,
    pub postbuilds: u32,
    pub postbuild_descs: u64,
}

pub const BUILD_DESC_BYTES: usize = 48;
pub const INPUTS_BYTES: usize = 24;
pub const GEOMETRY_DESC_BYTES: usize = 56;
pub const ELEMENT_BYTES: usize = 80;
pub const POSTBUILD_DESC_BYTES: usize = 16;
/// `D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_COMPACTED_SIZE`.
pub const COMPACTED_SIZE: u32 = 0;
const PERFORM_UPDATE: u32 = 0x20;

fn dword(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn qword(bytes: &[u8], at: usize) -> u64 {
    u64::from(dword(bytes, at)) | u64::from(dword(bytes, at + 4)) << 32
}

impl BuildDesc {
    fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < BUILD_DESC_BYTES {
            return None;
        }
        Some(Self {
            dest: qword(bytes, 0),
            inputs: Inputs::parse(&bytes[8..])?,
            source: qword(bytes, 32),
            scratch: qword(bytes, 40),
        })
    }

    fn plausible(&self) -> bool {
        let address = |a: u64| a != 0 && a < 1 << 40 && a.is_multiple_of(256);
        address(self.dest)
            && self.inputs.flags < 0x100
            && self.inputs.count < 1 << 24
            && (self.inputs.count == 0 || self.inputs.descs != 0)
            && (self.source == 0 || address(self.source))
            && (self.inputs.flags & PERFORM_UPDATE == 0 || self.source != 0)
    }
}

impl Inputs {
    fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < INPUTS_BYTES {
            return None;
        }
        let (kind, layout) = (dword(bytes, 0), dword(bytes, 12));
        if kind > 1 || layout > 1 {
            return None;
        }
        Some(Self {
            top: kind == 0,
            flags: dword(bytes, 4),
            count: dword(bytes, 8),
            pointers: layout == 1,
            descs: qword(bytes, 16),
        })
    }

    /// What the maximum size follows from: the input kind and flags and
    /// each geometry's kind, flags, formats and counts. Buffer addresses
    /// are not part of it: the builder is told sizes before it places data.
    fn key(&self, geometries: &[Geometry]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (self.top, self.flags, self.count).hash(&mut h);
        for g in geometries {
            match *g {
                Geometry::Triangles { flags, index_format, vertex_format, indices, vertices, .. } => {
                    (0u8, flags, index_format, vertex_format, indices, vertices).hash(&mut h);
                }
                Geometry::Boxes { flags, count, .. } => (1u8, flags, count).hash(&mut h),
            }
        }
        h.finish()
    }
}

impl Geometry {
    fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < GEOMETRY_DESC_BYTES {
            return None;
        }
        let flags = dword(bytes, 4);
        match dword(bytes, 0) {
            0 => Some(Self::Triangles {
                flags,
                transform: qword(bytes, 8),
                index_format: dword(bytes, 16),
                vertex_format: dword(bytes, 20),
                indices: dword(bytes, 24),
                vertices: dword(bytes, 28),
                index_buffer: qword(bytes, 32),
                vertex_buffer: qword(bytes, 40),
                vertex_stride: qword(bytes, 48),
            }),
            1 => Some(Self::Boxes {
                flags,
                count: qword(bytes, 8),
                boxes: qword(bytes, 16),
                stride: qword(bytes, 24),
            }),
            _ => None,
        }
    }
}

impl Element {
    fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < ELEMENT_BYTES {
            return None;
        }
        Some(Self {
            desc: BuildDesc::parse(bytes)?,
            postbuilds: dword(bytes, 48),
            postbuild_descs: qword(bytes, 56),
        })
    }
}

/// `len` bytes of committed title CPU memory at `at`, if it can be read.
fn title_bytes(at: u64, len: usize) -> Option<Vec<u8>> {
    if at == 0 || len == 0 || !crate::texture::committed(at as *const u8, len) {
        return None;
    }
    // SAFETY: committed memory read while the title's calling thread waits
    // in one of the wrappers below.
    Some(unsafe { std::slice::from_raw_parts(at as *const u8, len) }.to_vec())
}

/// The geometry descs of bottom-level `inputs`, all or none.
fn geometries(inputs: &Inputs) -> Option<Vec<Geometry>> {
    if inputs.top {
        return Some(Vec::new());
    }
    let n = inputs.count as usize;
    if n == 0 || n > 4096 {
        return None;
    }
    if inputs.pointers {
        let pointers = title_bytes(inputs.descs, 8 * n)?;
        (0..n)
            .map(|k| Geometry::parse(&title_bytes(qword(&pointers, 8 * k), GEOMETRY_DESC_BYTES)?))
            .collect()
    } else {
        title_bytes(inputs.descs, GEOMETRY_DESC_BYTES * n)?
            .chunks_exact(GEOMETRY_DESC_BYTES)
            .map(Geometry::parse)
            .collect()
    }
}

/// Maximum result sizes the builder told the title, by input shape, with a
/// bound on how many are remembered.
#[derive(Default)]
struct Told {
    sizes: HashMap<u64, u64>,
    order: VecDeque<u64>,
}

const TOLD: usize = 4096;

impl Told {
    fn put(&mut self, key: u64, size: u64) {
        if self.sizes.insert(key, size).is_some() {
            return;
        }
        self.order.push_back(key);
        if self.order.len() > TOLD
            && let Some(oldest) = self.order.pop_front()
        {
            self.sizes.remove(&oldest);
        }
    }
}

static TOLD_SIZES: std::sync::LazyLock<Mutex<Told>> = std::sync::LazyLock::new(Mutex::default);
/// Title calls to the builder's prebuild method, without and with a stream
/// desc. A size asked now uses the title's form only when it has been seen.
static STREAMS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

type Prebuild = extern "system" fn(*mut c_void, *const c_void, *const c_void, *mut u64);

/// The real builder methods, where the title's imports pointed.
static REAL: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];
const BATCHED: usize = 0;
const PREBUILD: usize = 1;

/// `XboxBvhBuilder::GetRaytracingAccelerationStructurePrebuildInfo`: the
/// sizes the builder says a structure and its scratch memory need.
extern "system" fn prebuild(this: *mut c_void, inputs: *const c_void, stream: *const c_void, info: *mut u64) {
    let real = REAL[PREBUILD].load(Ordering::Relaxed);
    if real == 0 {
        return;
    }
    // SAFETY: the builder's method, with the title's arguments.
    let real: Prebuild = unsafe { std::mem::transmute(real) };
    real(this, inputs, stream, info);
    STREAMS[usize::from(!stream.is_null())].fetch_add(1, Ordering::Relaxed);
    let read = title_bytes(inputs as u64, INPUTS_BYTES).as_deref().and_then(Inputs::parse);
    let said = title_bytes(info as u64, 24).map(|b| [0, 8, 16].map(|at| qword(&b, at)));
    let key = read.as_ref().and_then(|i| Some(i.key(&geometries(i)?)));
    if let (Some(key), Some([size, ..])) = (key, said) {
        TOLD_SIZES.lock().unwrap_or_else(|e| e.into_inner()).put(key, size);
    }
}

/// The most bytes a structure of `inputs` can take, as the builder itself
/// said: remembered from the title's prebuild call, else asked again when
/// the title was seen to ask without a stream desc.
fn max_size(this: *mut c_void, inputs: &Inputs, at: u64) -> Option<(u64, &'static str)> {
    let told = geometries(inputs).and_then(|g| {
        TOLD_SIZES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sizes
            .get(&inputs.key(&g))
            .copied()
    });
    let size = match told {
        Some(size) => (size, "told to the title"),
        None => {
            let real = REAL[PREBUILD].load(Ordering::Relaxed);
            if real == 0 || STREAMS[0].load(Ordering::Relaxed) == 0 || STREAMS[1].load(Ordering::Relaxed) != 0 {
                return None;
            }
            let mut info = [0u64; 3];
            // SAFETY: the builder's method, with the builder the title is
            // calling and the inputs the title passed to the batched build.
            let real: Prebuild = unsafe { std::mem::transmute(real) };
            real(this, at as *const c_void, std::ptr::null(), info.as_mut_ptr());
            (info[0], "asked again")
        }
    };
    (size.0 != 0 && size.0 < 1 << 32).then_some(size)
}

/// Slots holding an answer of ours, and the span they lie in.
static SLOTS: Mutex<BTreeSet<u64>> = Mutex::new(BTreeSet::new());
static SPAN: [AtomicU64; 2] = [AtomicU64::new(u64::MAX), AtomicU64::new(0)];
/// The title uses a ring of slots in a small buffer.
const SLOTS_MOST: usize = 1 << 16;

fn hold_slot(slot: u64) -> bool {
    let mut slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
    if slots.len() >= SLOTS_MOST && !slots.contains(&slot) {
        return false;
    }
    slots.insert(slot);
    SPAN[0].fetch_min(slot, Ordering::Relaxed);
    SPAN[1].fetch_max(slot + 8, Ordering::Relaxed);
    true
}

fn free_slot(slot: u64) {
    if slot < SPAN[1].load(Ordering::Relaxed) {
        SLOTS.lock().unwrap_or_else(|e| e.into_inner()).remove(&slot);
    }
}

#[cfg(test)]
static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
fn reset_slots() {
    SLOTS.lock().unwrap_or_else(|e| e.into_inner()).clear();
    SPAN[0].store(u64::MAX, Ordering::Relaxed);
    SPAN[1].store(0, Ordering::Relaxed);
}

/// Before what shaders wrote goes into title memory at `at`, the bytes of
/// slots holding an answer stay as they are. The builder's store of a
/// compacted size lands frames later, by when the slot may be another
/// structure's. `None` if `merged` is already right.
pub(crate) fn keep_answers(at: usize, title: &[u8], merged: &[u8]) -> Option<Vec<u8>> {
    let (from, to) = (at as u64, (at + merged.len().min(title.len())) as u64);
    if from >= SPAN[1].load(Ordering::Relaxed) || to <= SPAN[0].load(Ordering::Relaxed) {
        return None;
    }
    let slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
    let mut kept: Option<Vec<u8>> = None;
    for &slot in slots.range(from.saturating_sub(7)..to) {
        let (a, b) = ((slot.max(from) - from) as usize, ((slot + 8).min(to) - from) as usize);
        if merged[a..b] == title[a..b] {
            continue;
        }
        kept.get_or_insert_with(|| merged.to_vec())[a..b].copy_from_slice(&title[a..b]);
    }
    kept
}

fn put_answer(slot: u64, size: u64) {
    let bytes = size.to_le_bytes();
    crate::host::write_mapped(slot as *mut u8, bytes.as_ptr(), bytes.len());
}

/// Writes the answer into each compacted-size slot of a batched build.
fn answer_sizes(this: *mut c_void, n: u32, descs: u64) -> Vec<(u64, u64)> {
    let mut answered = Vec::new();
    let Some(bytes) = title_bytes(descs, ELEMENT_BYTES * (n as usize).min(4096)) else {
        return answered;
    };
    for (k, bytes) in bytes.chunks_exact(ELEMENT_BYTES).enumerate() {
        let Some(element) = Element::parse(bytes).filter(|e| e.desc.plausible() && (1..=8).contains(&e.postbuilds)) else {
            continue;
        };
        let Some(posts) = title_bytes(element.postbuild_descs, POSTBUILD_DESC_BYTES * element.postbuilds as usize) else {
            continue;
        };
        for post in posts.chunks_exact(POSTBUILD_DESC_BYTES) {
            let (slot, kind) = (qword(post, 0), dword(post, 8));
            if kind != COMPACTED_SIZE || title_bytes(slot, 8).is_none() {
                free_slot(slot);
                continue;
            }
            let inputs_at = descs + (k * ELEMENT_BYTES + 8) as u64;
            let Some((size, _)) = max_size(this, &element.desc.inputs, inputs_at) else {
                free_slot(slot);
                continue;
            };
            if !hold_slot(slot) {
                free_slot(slot);
                continue;
            }
            put_answer(slot, size);
            answered.push((slot, size));
        }
    }
    answered
}

/// `XboxBvhBuilder::BuildRaytracingAccelerationStructureBatched2X`.
extern "system" fn batched(this: *mut c_void, list: *mut c_void, n: u32, descs: *const c_void) {
    let answered = if answers() {
        answer_sizes(this, n, descs as u64)
    } else {
        Vec::new()
    };
    let real = REAL[BATCHED].load(Ordering::Relaxed);
    if real != 0 {
        // SAFETY: the method the title imported, with the title's arguments.
        let real: extern "system" fn(*mut c_void, *mut c_void, u32, *const c_void) = unsafe { std::mem::transmute(real) };
        real(this, list, n, descs);
    }
    // The builder may have recorded a size while it ran; the title needs
    // ours when it reads the slot.
    for (slot, size) in answered {
        if title_bytes(slot, 8).is_some_and(|b| qword(&b, 0) != size) {
            put_answer(slot, size);
        }
    }
}

const BATCHED_SYMBOL: &str = "?BuildRaytracingAccelerationStructureBatched2X@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList6@@IPEBUD3D12XBOX_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_BATCHED_DESC2@@@Z";
const PREBUILD_SYMBOL: &str = "?GetRaytracingAccelerationStructurePrebuildInfo@XboxBvhBuilder@@QEAAXPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS@@PEBUD3D12XBOX_XDXR_STREAM_DESC@@PEAUD3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO@@@Z";

fn wrapper_of(symbol: &str) -> Option<(usize, usize)> {
    match symbol {
        BATCHED_SYMBOL => Some((BATCHED, batched as *const () as usize)),
        PREBUILD_SYMBOL => Some((PREBUILD, prebuild as *const () as usize)),
        _ => None,
    }
}

/// The address to bind instead of the builder's own method, when compacted
/// sizes are answered and this is one of the two methods involved.
pub fn hook(symbol: &str, real: usize) -> Option<usize> {
    if !answers() {
        return None;
    }
    let (which, wrapper) = wrapper_of(symbol)?;
    REAL[which].store(real, Ordering::Relaxed);
    Some(wrapper)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc_bytes(dest: u64, flags: u32, count: u32, descs: u64) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(dest.to_le_bytes()); // result
        b.extend(1u32.to_le_bytes()); // bottom level
        b.extend(flags.to_le_bytes());
        b.extend(count.to_le_bytes());
        b.extend(0u32.to_le_bytes()); // desc layout: an array
        b.extend(descs.to_le_bytes());
        b.extend(0u64.to_le_bytes()); // source
        b.extend(0x1_dc00_0000u64.to_le_bytes()); // scratch
        assert_eq!(b.len(), BUILD_DESC_BYTES);
        b
    }

    fn element_bytes(desc: &[u8], postbuilds: u32, postbuild_descs: u64) -> Vec<u8> {
        let mut b = desc.to_vec();
        b.extend(u64::from(postbuilds).to_le_bytes());
        b.extend(postbuild_descs.to_le_bytes());
        b.extend([0u8; 16]);
        assert_eq!(b.len(), ELEMENT_BYTES);
        b
    }

    fn triangles(indices: u32, vertices: u32, buffer: u64) -> Geometry {
        Geometry::Triangles {
            flags: 1,
            transform: 0,
            index_format: 57,
            vertex_format: 6,
            indices,
            vertices,
            index_buffer: buffer,
            vertex_buffer: buffer + 0x100,
            vertex_stride: 12,
        }
    }

    fn geometry_bytes(indices: u32, vertices: u32) -> Vec<u8> {
        let mut b = vec![0u8; GEOMETRY_DESC_BYTES];
        b[0..4].copy_from_slice(&0u32.to_le_bytes()); // triangles
        b[4..8].copy_from_slice(&1u32.to_le_bytes());
        b[16..20].copy_from_slice(&57u32.to_le_bytes());
        b[20..24].copy_from_slice(&6u32.to_le_bytes());
        b[24..28].copy_from_slice(&indices.to_le_bytes());
        b[28..32].copy_from_slice(&vertices.to_le_bytes());
        b
    }

    #[test]
    fn sizes_are_answered_unless_switched_off() {
        assert!(parse_answers(None));
        assert!(parse_answers(Some("1")));
        assert!(!parse_answers(Some(" 0 ")));
    }

    #[test]
    fn only_the_two_methods_are_wrapped() {
        assert_eq!(wrapper_of(BATCHED_SYMBOL).map(|w| w.0), Some(BATCHED));
        assert_eq!(wrapper_of(PREBUILD_SYMBOL).map(|w| w.0), Some(PREBUILD));
        assert_eq!(wrapper_of("?SomethingElse@XboxBvhBuilder@@QEAAXXZ"), None);
    }

    #[test]
    fn a_batched_element_holds_its_postbuild_descs() {
        let desc = desc_bytes(0xd_6000_8700, 0xa, 1, 0xd_1000_6ed0);
        let e = element_bytes(&desc, 1, 0x5f_f000_5d50);
        let got = Element::parse(&e).unwrap();
        assert_eq!(
            (got.desc.dest, got.postbuilds, got.postbuild_descs),
            (0xd_6000_8700, 1, 0x5f_f000_5d50)
        );
        assert!(got.desc.plausible());
        assert_eq!(got.desc.inputs, Inputs::parse(&desc[8..]).unwrap());
        assert_eq!(Element::parse(&e[..72]), None);
    }

    #[test]
    fn sizes_follow_from_counts_not_from_addresses() {
        let inputs = |flags, descs| Inputs {
            top: false,
            flags,
            count: 1,
            pointers: false,
            descs,
        };
        let key = inputs(0xa, 0x1000).key(&[triangles(300, 120, 0x7000)]);
        assert_eq!(key, inputs(0xa, 0x2000).key(&[triangles(300, 120, 0x9000)]));
        assert_ne!(key, inputs(0xa, 0x1000).key(&[triangles(303, 120, 0x7000)]));
        assert_ne!(key, inputs(0xa, 0x1000).key(&[triangles(300, 121, 0x7000)]));
        assert_ne!(key, inputs(0xb, 0x1000).key(&[triangles(300, 120, 0x7000)]));
        assert_ne!(
            key,
            inputs(0xa, 0x1000).key(&[Geometry::Boxes {
                flags: 1,
                count: 100,
                boxes: 0x7000,
                stride: 24
            }])
        );
    }

    #[test]
    fn told_sizes_are_bounded() {
        let mut told = Told::default();
        for key in 0..TOLD as u64 + 10 {
            told.put(key, key + 1);
        }
        told.put(TOLD as u64 + 9, 7);
        assert_eq!((told.sizes.len(), told.order.len()), (TOLD, TOLD));
        assert_eq!(told.sizes.get(&9), None);
        assert_eq!(told.sizes.get(&10), Some(&11));
        assert_eq!(told.sizes.get(&(TOLD as u64 + 9)), Some(&7));
    }

    #[test]
    fn a_late_store_stays_off_an_answered_slot() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_slots();
        let title = Box::new([0x11u8; 32]);
        let at = title.as_ptr() as usize;
        let mut merged = *title;
        merged[4..20].fill(0x22);
        assert_eq!(keep_answers(at, &*title, &merged), None);
        let slot = at as u64 + 8;
        assert!(hold_slot(slot));
        let kept = keep_answers(at, &*title, &merged).unwrap();
        assert_eq!(
            (&kept[4..8], &kept[8..16], &kept[16..20]),
            (&[0x22; 4][..], &[0x11; 8][..], &[0x22; 4][..])
        );
        let kept = keep_answers(at + 12, &title[12..], &merged[12..]).unwrap();
        assert_eq!((&kept[..4], &kept[4..8]), (&[0x11; 4][..], &[0x22; 4][..]));
        assert_eq!(keep_answers(at, &*title, &*title), None);
        assert_eq!(keep_answers(at + 16, &title[16..], &merged[16..]), None);
        free_slot(slot);
        assert_eq!(keep_answers(at, &*title, &merged), None);
    }

    /// The builder's prebuild method in the test below: 0x1000 a geometry.
    extern "system" fn told(_: *mut c_void, inputs: *const c_void, _: *const c_void, info: *mut u64) {
        // SAFETY: the test's inputs and its three qwords.
        unsafe {
            let n = inputs.cast::<u32>().add(2).read_unaligned();
            info.write(u64::from(n) * 0x1000);
            info.add(1).write(0x40);
            info.add(2).write(0x20);
        }
    }

    #[test]
    fn a_compacted_size_is_answered_with_what_the_builder_said() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_slots();
        let post = |slot: u64, kind: u32| {
            let mut b = slot.to_le_bytes().to_vec();
            b.extend(u64::from(kind).to_le_bytes());
            b
        };
        REAL[PREBUILD].store(told as *const () as usize, Ordering::Relaxed);
        let slots = Box::new([0xAAAAu64; 4]);
        let slot = |k: usize| std::ptr::from_ref(&slots[k]) as u64;
        let posts = [
            post(slot(0), COMPACTED_SIZE),
            post(slot(1), COMPACTED_SIZE),
            [post(slot(2), COMPACTED_SIZE), post(slot(3), 3)].concat(),
        ];
        // The title asks the first structure's size before the build.
        let asked = geometry_bytes(300, 120);
        let asked_desc = desc_bytes(0x100, 0xa, 2, asked.as_ptr() as u64);
        let mut info = [0u64; 3];
        prebuild(
            std::ptr::null_mut(),
            asked_desc[8..].as_ptr().cast(),
            std::ptr::null(),
            info.as_mut_ptr(),
        );
        assert_eq!(info[0], 0x2000);
        // Three builds: one was told, two are asked of the builder again.
        let (one, two, three) = (geometry_bytes(600, 120), geometry_bytes(30, 90), geometry_bytes(30, 90));
        let descs = [
            element_bytes(&desc_bytes(0xd_6000_0100, 0xa, 2, one.as_ptr() as u64), 1, posts[0].as_ptr() as u64),
            element_bytes(&desc_bytes(0xd_6000_4100, 0xa, 3, two.as_ptr() as u64), 1, posts[1].as_ptr() as u64),
            element_bytes(&desc_bytes(0xd_6000_8100, 0xa, 1, three.as_ptr() as u64), 2, posts[2].as_ptr() as u64),
        ];
        let elements = [descs[0].as_slice(), descs[1].as_slice(), descs[2].as_slice()].concat();
        // A slot that held an older answer and is now asked for another
        // kind of information is the builder's again.
        assert!(hold_slot(slot(3)));
        let answered = answer_sizes(std::ptr::null_mut(), 3, elements.as_ptr() as u64);
        assert_eq!(answered, [(slot(0), 0x2000), (slot(1), 0x3000), (slot(2), 0x1000)]);
        assert_eq!(*slots, [0x2000, 0x3000, 0x1000, 0xAAAA]);
        // The builder's stores land late: off the answered slots only.
        let title: Vec<u8> = slots.iter().flat_map(|s| s.to_le_bytes()).collect();
        let kept = keep_answers(slot(0) as usize, &title, &[0x77; 32]).unwrap();
        assert_eq!((&kept[..24], &kept[24..]), (&title[..24], &[0x77; 8][..]));
        for s in [slot(0), slot(1), slot(2)] {
            free_slot(s);
        }
    }
}
