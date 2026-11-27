//! Programs a title passes as loose pieces instead of a container.
//!
//! Two device methods of the console (table offsets 0x218 and 0x1c8) make a
//! pipeline from pieces that lie apart in title memory: a packet of register
//! values, a piece of pipeline state, and per stage the GFX10 code and its
//! metadata. The metadata is the record stream an `ACCC` part's kind 7 entry
//! holds ([`crate::native::fspx_program`]); only its header differs. This
//! module reads the packets, and wraps each stage in a container of our own
//! so that everything built on containers (content hashes, the translation
//! cache, [`crate::emit::translate`]) takes it unchanged.
//!
//! Layouts as observed in 48 calls of each method of one title (offsets in
//! bytes, little endian; "seen" is all that the captures show, a guess is
//! labelled):
//!
//! ```text
//! compute description, 0x48 bytes (9 u64)
//! +0x00 root signature (an object of the caller's)
//! +0x08 address, length: the packet, 0x1c bytes
//! +0x18 address, length: 0x14 bytes of pipeline metadata (not read)
//! +0x28 address, length: the code
//! +0x38 address, length: the metadata
//!
//! compute packet, 0x1c bytes
//! +0x0c u16 x3 thread-group size
//! +0x12 u16 flags: bit 4 wave32 (never set in the calls seen)
//! +0x14 COMPUTE_PGM_RSRC1: bits 0-5 VGPR granules - 1
//! +0x18 COMPUTE_PGM_RSRC2
//!
//! graphics description, 0x68 bytes read (13 u64); the 20 words after
//! them were zero in every call (room for more stages: a guess)
//! +0x00 root signature
//! +0x08 address, length: the packet
//! +0x18 address, length: the state piece
//! +0x28 address, length: pixel code; +0x38 pixel metadata (all four 0
//!       without a pixel stage)
//! +0x48 address, length: vertex code; +0x58 vertex metadata
//!
//! graphics packet: register values at fixed places, no register numbers
//! +0x28 u16 0x11c (size of the block from +0x1c), +0x2a u16 size of the
//!       block from +0x138; length = 0x138 + that
//! +0x38 SPI_SHADER_PGM_RSRC1_VS, +0x3c _RSRC2_VS (bits 1-5 user SGPRs)
//! +0x4c SPI_SHADER_PGM_RSRC1_PS (0 without a pixel stage), +0x50 _RSRC2_PS
//! +0x58 SPI_PS_INPUT_ADDR, +0x5c SPI_PS_IN_CONTROL (bits 0-5 interpolants)
//! +0x60 SPI_SHADER_Z_FORMAT, +0x68 DB_SHADER_CONTROL
//! +0x70 SPI_SHADER_COL_FORMAT, +0x7c CB_COLOR_CONTROL
//! +0x84 SPI_VS_OUT_CONFIG, +0x90 SPI_SHADER_POS_FORMAT
//! +0x98 PA_CL_VS_OUT_CNTL (nonzero in one call of 48, with a second
//!       position export), +0x9c PA_CL_CLIP_CNTL, +0xa0 PA_SU_SC_MODE_CNTL
//! +0xac three dwords, always 0 (depth bias: a guess)
//! +0xb8 DB_DEPTH_CONTROL, +0xbc DB_STENCIL_CONTROL
//! +0xc0 DB_STENCILREFMASK, +0xc4 DB_STENCILREFMASK_BF
//! +0xc8 always 0 (not identified), +0xcc CB_TARGET_MASK
//! +0xd0 CB_BLEND0_CONTROL .. CB_BLEND7_CONTROL
//! +0x170 SPI_PS_INPUT_CNTL_0.., as many as fit before the last 12 bytes
//!        (render target information, which repeats the state piece)
//!
//! state piece
//! +0x10 u8 x8 render target formats (DXGI numbers), 0 past the count
//! +0x1c u32 0xffffffff (sample mask: a guess)
//! +0x24 u8 depth format, +0x25 u8 render target count
//! +0x26 u8 primitive topology type (3 triangles, 19 the console's
//!       rectangles: by the two values seen)
//! +0x28 u32 1 (sample count: a guess)
//! +0x38 u32 size of the input layout block at +0x4c: u32 element count,
//!       then per element u32 name length with its NUL, the name, and six
//!       u32 of which the second is the DXGI format and the fourth the byte
//!       offset in the vertex. The other four were 0 in every element. They
//!       lie where D3D12's element description has its semantic index, slot,
//!       class and step rate (words 0, 2, 4 and 5), which no capture
//!       confirms, so a value in any of them is refused by its place
//!
//! metadata piece
//! +0x00 u32 0x403 (compute) or 2 (graphics)
//! +0x08 u32 code dwords: the code piece's length / 4 (compute), or that
//!       less 10 (graphics: 40 bytes at the end are not counted)
//! records of 32 bytes from +0x18 (compute) or +0x10 (graphics)
//! ```
//!
//! A vertex program has no fetch shader: it loads the 16-byte descriptor of
//! a vertex buffer from the table whose address arrives in s0 and s1, adds
//! s2 to the vertex index in v0 and reads its attributes with buffer loads
//! at the elements' byte offsets. Pixel exports are final (two 16-bit floats
//! per dword for the `FP16_ABGR` format): nothing patches them.
//!
//! The container made for a stage has one part of ours ([`FourCC::PIECES`])
//! and, for graphics stages, `ISG1` and `OSG1` parts made from the packet
//! (the pieces name no semantics: parameter n of the vertex program is
//! `TEXCOORD<n>` in register n on both sides). The part:
//!
//! ```text
//! +0x00 "VPP1"
//! +0x04 u8 stage (0 compute, 1 vertex, 2 pixel), u8 bit 0 wave32, u16 VGPRs
//! +0x08 u32 user SGPRs
//! +0x0c u32 x5 by stage
//!       compute: COMPUTE_PGM_RSRC2, thread-group size x, y, z
//!       vertex:  parameter exports, bit per parameter read flat, elements
//!       pixel:   SPI_PS_INPUT_ADDR, DB_SHADER_CONTROL, SPI_SHADER_COL_FORMAT,
//!                the vertex program's parameter exports, interpolants
//! +0x20 u32 size of what follows the header, u32 code size, u32 metadata
//!       size, u32 0
//! +0x30 vertex: elements of 48 bytes (32 of name, u32 semantic index, DXGI
//!       format, slot, byte offset); pixel: SPI_PS_INPUT_CNTL per interpolant
//! then  the code piece and the metadata piece, as the title has them
//! ```
//!
//! Only what a stage's translation depends on goes into its part, so two
//! pipelines that differ in blend state alone share their programs, and two
//! that differ in anything a program reads never do.

use std::ops::Range;

use crate::ShaderError::{self, Malformed};
use crate::dxbc::{FourCC, u16_at, u32_at};
use crate::native::{
    self, COMPUTE_PIECES, ComputeIo, HwStage, NativeProgram, PsInput, RecordRules, RootParam, RootType, StageIo,
    PIXEL_PIECES, Usage, VERTEX_BUFFERS, VsFetch, VsOutput,
};

/// What starts the part of ours.
pub const MAGIC: &[u8; 4] = b"VPP1";
/// The lengths of a compute description's first two pieces.
pub const COMPUTE_PACKET: usize = 0x1c;
pub const COMPUTE_PIPELINE_PIECE: usize = 0x14;
/// The longest pieces taken (the longest seen: 123 KiB of code, 6 KiB of
/// metadata, 0x18c bytes of packet, 0xbc of state).
pub const MAX_CODE: usize = 4 << 20;
pub const MAX_METADATA: usize = 64 << 10;
pub const MAX_PACKET: usize = 0x400;
pub const MAX_STATE: usize = 0x400;

/// Why a pipeline is not made from its pieces: something malformed, or
/// something understood too little to translate without guessing.
pub type Refusal = &'static str;

/// The reason for the first field of `value` that lies in `unhandled`, from
/// `fields` as (mask of its bits, reason); `other` where no listed field
/// does. A register refused for "some bit" says nothing the next run can
/// act on, so each refusal names the field it is about.
fn outside(value: u32, unhandled: u32, fields: &[(u32, Refusal)], other: Refusal) -> Refusal {
    fields
        .iter()
        .find(|(mask, _)| value & unhandled & mask != 0)
        .map_or(other, |&(_, why)| why)
}

/// `PA_CL_VS_OUT_CNTL` bits: `CLIP_DIST_ENA_0..7` (0-7), `CULL_DIST_ENA_0..7`
/// (8-15), the point size, edge flag, render target index, viewport index
/// and kill flag of the miscellaneous vector (16-20), `VS_OUT_MISC_VEC_ENA`
/// (21), the two clip and cull distance vectors (22, 23) and the side bus
/// for the miscellaneous vector (24).
const CL_CLIP_DIST_ENA: u32 = 0xff;
const CL_CCDIST0_VEC: u32 = 1 << 22;
const CL_CCDIST1_VEC: u32 = 1 << 23;

/// How many clip distances a vertex program exports, from
/// `SPI_SHADER_POS_FORMAT` and `PA_CL_VS_OUT_CNTL`; why not if the two
/// registers say more than that. The position is the first export, in the
/// four-component format. Distance n is channel n of the vectors that follow
/// (the first vector holds 0-3, the second 4-7: radeonsi's export order, and
/// the order the lowering of a position export takes), each vector exported
/// where `VS_OUT_CCDIST0_VEC_ENA` or `VS_OUT_CCDIST1_VEC_ENA` says and in
/// the four-component format. Cull distances and the miscellaneous vector
/// are refused: a capture has a cull distance 0 enabled beside the clip
/// distance 0 and a single exported channel, which does not say which
/// channel a cull distance takes.
fn clip_distances(pos_format: u32, out_cntl: u32) -> Result<u8, Refusal> {
    if pos_format & 0xf != 4 {
        return Err("SPI_SHADER_POS_FORMAT POS0_EXPORT_FORMAT other than 4COMP (4)");
    }
    let rest = out_cntl & !(CL_CLIP_DIST_ENA | CL_CCDIST0_VEC | CL_CCDIST1_VEC);
    if rest != 0 {
        return Err(outside(
            out_cntl,
            rest,
            &[
                (
                    0xff00,
                    "PA_CL_VS_OUT_CNTL CULL_DIST_ENA_0..7 (bits 8-15): the channel of a cull distance is not settled",
                ),
                (
                    0x1f_0000,
                    "PA_CL_VS_OUT_CNTL USE_VTX_POINT_SIZE, EDGE_FLAG, RENDER_TARGET_INDX, VIEWPORT_INDX or KILL_FLAG (bits 16-20)",
                ),
                (1 << 21, "PA_CL_VS_OUT_CNTL VS_OUT_MISC_VEC_ENA (bit 21)"),
                (
                    1 << 24,
                    "PA_CL_VS_OUT_CNTL VS_OUT_MISC_SIDE_BUS_ENA (bit 24)",
                ),
            ],
            "PA_CL_VS_OUT_CNTL bits above bit 24",
        ));
    }
    let clip = out_cntl & CL_CLIP_DIST_ENA;
    // Distances count up from 0: the lowering takes the signature's
    // distances in order, from channel 0 of the first vector.
    if clip & (clip + 1) != 0 {
        return Err("PA_CL_VS_OUT_CNTL CLIP_DIST_ENA_0..7 (bits 0-7) that do not count up from 0");
    }
    let vectors = [clip & 0xf != 0, clip & 0xf0 != 0];
    let enabled = (out_cntl & CL_CCDIST0_VEC != 0, out_cntl & CL_CCDIST1_VEC != 0);
    if enabled != (vectors[0], vectors[1]) {
        return Err(
            "PA_CL_VS_OUT_CNTL VS_OUT_CCDIST0_VEC_ENA or _CCDIST1_VEC_ENA (bits 22-23) that disagree with CLIP_DIST_ENA",
        );
    }
    let wanted = 4 | u32::from(vectors[0]) * 0x40 | u32::from(vectors[1]) * 0x400;
    if pos_format != wanted {
        return Err("SPI_SHADER_POS_FORMAT that disagrees with PA_CL_VS_OUT_CNTL");
    }
    Ok(clip.count_ones() as u8)
}

/// What a render target takes from a pixel program's exports: the type of
/// the host's output for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Float,
    Uint,
    Sint,
}

/// The kind of render target of DXGI format `format`: the unsigned and
/// signed integer formats (D3D12 does not blend into them and wants the
/// program's output declared as the integer it is), floats and the
/// normalised formats alike otherwise.
pub fn target_kind(format: u8) -> TargetKind {
    match format {
        // R32G32B32A32, R32G32B32, R16G16B16A16, R32G32, R10G10B10A2,
        // R8G8B8A8, R16G16, R32, R8G8, R16, R8 (UINT)
        3 | 7 | 12 | 17 | 25 | 30 | 36 | 42 | 50 | 57 | 62 => TargetKind::Uint,
        // the same without R10G10B10A2 (SINT)
        4 | 8 | 14 | 18 | 32 | 38 | 43 | 52 | 59 | 64 => TargetKind::Sint,
        _ => TargetKind::Float,
    }
}

const HEAD: usize = 0x30;
const RECORD: usize = 32;
const COMPUTE: u8 = 0;
const VERTEX: u8 = 1;
const PIXEL: u8 = 2;
const WAVE32: u8 = 1;
/// (metadata tag, where its records start, code bytes it does not count)
const COMPUTE_METADATA: (u32, usize, usize) = (0x403, 0x18, 0);
const GRAPHICS_METADATA: (u32, usize, usize) = (2, 0x10, 0x28);
const ELEMENT: usize = 48;
const NAME: usize = 32;
const MAX_ELEMENTS: usize = 16;

// The compute packet.
const C_THREADS: usize = 0x0c;
const C_FLAGS: usize = 0x12;
const C_WAVE32: u16 = 1 << 4;
const C_RSRC1: usize = 0x14;
const C_RSRC2: usize = 0x18;

// The graphics packet.
const G_FIXED_SIZE: usize = 0x28;
const G_SECOND_SIZE: usize = 0x2a;
const G_FIXED: u16 = 0x11c;
const G_SECOND: usize = 0x138;
const G_RSRC1_VS: usize = 0x38;
const G_RSRC2_VS: usize = 0x3c;
const G_RSRC1_PS: usize = 0x4c;
const G_RSRC2_PS: usize = 0x50;
const G_PS_INPUT_ADDR: usize = 0x58;
const G_PS_IN_CONTROL: usize = 0x5c;
const G_Z_FORMAT: usize = 0x60;
const G_DB_SHADER_CONTROL: usize = 0x68;
const G_COL_FORMAT: usize = 0x70;
const G_CB_COLOR_CONTROL: usize = 0x7c;
const G_VS_OUT_CONFIG: usize = 0x84;
const G_POS_FORMAT: usize = 0x90;
const G_VS_OUT_CNTL: usize = 0x98;
const G_CLIP_CNTL: usize = 0x9c;
const G_SU_SC_MODE_CNTL: usize = 0xa0;
const G_DEPTH_BIAS: usize = 0xac;
const G_DEPTH_CONTROL: usize = 0xb8;
const G_STENCIL_CONTROL: usize = 0xbc;
const G_STENCILREFMASK: usize = 0xc0;
const G_UNKNOWN: usize = 0xc8;
const G_TARGET_MASK: usize = 0xcc;
const G_BLEND: usize = 0xd0;
const G_INPUT_CNTL: usize = 0x170;
/// The second block before the input controls, and the render target
/// information after them.
const G_SECOND_HEAD: usize = 0x38;
const G_TARGET_INFO: usize = 12;

/// `SPI_PS_IN_CONTROL.PS_W32_EN`.
const PS_W32_EN: u32 = 1 << 15;
/// `SPI_PS_INPUT_CNTL.OFFSET` of an input that is a constant
/// (`DEFAULT_VAL` says which).
const CONSTANT_OFFSET: u32 = 0x20;
/// What a pixel stage's `DB_SHADER_CONTROL` may set: Z_EXPORT_ENABLE (0),
/// Z_ORDER (4-5), KILL_ENABLE (6), DEPTH_BEFORE_SHADER (12),
/// CONSERVATIVE_Z_EXPORT (13-14) and DUAL_QUAD_DISABLE (15, which Mesa sets
/// where the render backend's blend optimisations are off). EXEC_ON_HIER_FAIL
/// (9), EXEC_ON_NOOP (10) and PRIMITIVE_ORDERED_PIXEL_SHADER (16) stay
/// refused: D3D12 has no field for the first two, which decide whether a
/// program that writes memory runs for pixels the depth test rejects, and
/// nothing in the host program carries rasteriser ordering for the third.
const DB_HANDLED: u32 = 0x1 | 0x30 | 0x40 | 0x1000 | 0x6000 | 0x8000;
/// The fields of `SPI_PS_INPUT_CNTL` that are not handled, by their bits.
const INPUT_CNTL_FIELDS: [(u32, Refusal); 11] = [
    (
        0x20,
        "SPI_PS_INPUT_CNTL OFFSET (bits 0-5) of 0x21 or more",
    ),
    (
        1 << 10,
        "SPI_PS_INPUT_CNTL FLAT_SHADE (bit 10) with the constant offset",
    ),
    (
        0x300,
        "SPI_PS_INPUT_CNTL DEFAULT_VAL (bits 8-9) with a parameter offset",
    ),
    (1 << 11, "SPI_PS_INPUT_CNTL ROTATE_PC_PTR (bit 11)"),
    (1 << 12, "SPI_PS_INPUT_CNTL PRIM_ATTR (bit 12)"),
    (0x1_e000, "SPI_PS_INPUT_CNTL CYL_WRAP (bits 13-16)"),
    (1 << 17, "SPI_PS_INPUT_CNTL PT_SPRITE_TEX (bit 17)"),
    (1 << 18, "SPI_PS_INPUT_CNTL DUP (bit 18)"),
    (1 << 19, "SPI_PS_INPUT_CNTL FP16_INTERP_MODE (bit 19)"),
    (
        0x00f0_0000,
        "SPI_PS_INPUT_CNTL second attribute defaults (bits 20-23)",
    ),
    (
        0x0300_0000,
        "SPI_PS_INPUT_CNTL ATTR0_VALID and ATTR1_VALID (bits 24-25)",
    ),
];

// The state piece.
const S_TARGETS: usize = 0x10;
const S_SAMPLE_MASK: usize = 0x1c;
const S_DEPTH: usize = 0x24;
const S_COUNT: usize = 0x25;
const S_TOPOLOGY: usize = 0x26;
const S_SAMPLES: usize = 0x28;
const S_BLOCK_SIZE: usize = 0x38;
const S_BLOCK: usize = 0x4c;

/// DXGI formats of the input elements whose fetch is known, as (bytes,
/// channels): the 32-bit float formats, which the code loads as dwords, and
/// `R8G8B8A8_UNORM`, which it loads through a typed format.
pub(crate) fn element_shape(format: u32) -> Option<(u32, u8)> {
    match format {
        2 => Some((16, 4)),
        6 => Some((12, 3)),
        16 => Some((8, 2)),
        41 => Some((4, 1)),
        R8G8B8A8_UNORM => Some((4, 4)),
        _ => None,
    }
}

/// The one byte-wise element format among [`element_shape`]'s.
pub(crate) const R8G8B8A8_UNORM: u32 = 28;

/// A container for one stage, whole but for the two pieces the caller
/// reads into it from title memory: `bytes[code]` and `bytes[metadata]`.
/// [`check`] then tells whether the pieces agree with each other.
#[derive(Debug)]
pub struct Draft {
    pub bytes: Vec<u8>,
    pub code: Range<usize>,
    pub metadata: Range<usize>,
}

/// The header of the part of ours.
struct Head {
    stage: u8,
    flags: u8,
    vgprs: u16,
    user_sgprs: u32,
    words: [u32; 5],
}

/// One row of a signature part.
struct Row<'a> {
    name: &'a str,
    index: u32,
    /// `D3D_NAME`: 0 none, 1 position, 9 front face, 64 target, 65 depth,
    /// 67 depth that is at least the position's, 68 at most.
    system: u32,
    /// `D3D_REGISTER_COMPONENT_TYPE`: 1 uint32, 2 sint32, 3 float32, 4 uint16,
    /// 5 sint16, 6 float16.
    component: u32,
    register: u32,
    mask: u8,
}

/// A signature part's data (`ISG1`, `OSG1`): count, offset of the rows,
/// rows of 32 bytes, then the names.
fn signature(rows: &[Row]) -> Vec<u8> {
    let mut names = 8 + 32 * rows.len();
    let mut out = Vec::with_capacity(names + rows.iter().map(|r| r.name.len() + 1).sum::<usize>());
    out.extend((rows.len() as u32).to_le_bytes());
    out.extend(8u32.to_le_bytes());
    for r in rows {
        // stream, name, semantic index, system value, type, register
        for v in [0, names as u32, r.index, r.system, r.component, r.register] {
            out.extend(v.to_le_bytes());
        }
        // mask, then the mask of what is always read or never written,
        // which nothing here reads; minimum precision none
        out.extend([r.mask, 0, 0, 0]);
        out.extend(0u32.to_le_bytes());
        names += r.name.len() + 1;
    }
    for r in rows {
        out.extend(r.name.as_bytes());
        out.push(0);
    }
    out
}

/// Whether the two pieces' lengths can be a code piece and a metadata piece
/// whose records start at `records`.
fn sizes(code: usize, metadata: usize, records: usize) -> Result<(), Refusal> {
    if code == 0 || !code.is_multiple_of(4) || code > MAX_CODE {
        return Err("a code piece that is empty, not whole dwords or implausibly long");
    }
    if metadata < records + RECORD
        || metadata > MAX_METADATA
        || !(metadata - records).is_multiple_of(RECORD)
    {
        return Err("a metadata piece that is not a header and whole records");
    }
    Ok(())
}

/// The container for a stage: the part of ours with `head` and `extra`
/// written and room for the code and the metadata, then the signatures.
fn draft(
    head: &Head,
    extra: &[u8],
    code: usize,
    metadata: usize,
    signatures: Option<[&[Row]; 2]>,
) -> Draft {
    let sigs = signatures.map(|[i, o]| [signature(i), signature(o)]);
    let part = HEAD + extra.len() + code + metadata;
    let mut parts = vec![(FourCC::PIECES, part)];
    if let Some([i, o]) = &sigs {
        parts.push((FourCC::ISG1, i.len()));
        parts.push((FourCC::OSG1, o.len()));
    }
    // The container: header, part offsets, then each part as its tag, its
    // size and its data.
    let table = 0x20 + 4 * parts.len();
    let total = table + parts.iter().map(|p| 8 + p.1).sum::<usize>();
    let mut bytes = vec![0u8; total];
    bytes[..4].copy_from_slice(b"DXBC");
    // (the digest stays zero: it names no shader anyone listed)
    bytes[0x14..0x16].copy_from_slice(&1u16.to_le_bytes());
    bytes[0x18..0x1c].copy_from_slice(&(total as u32).to_le_bytes());
    bytes[0x1c..0x20].copy_from_slice(&(parts.len() as u32).to_le_bytes());
    let mut at = table;
    let mut data = Vec::with_capacity(parts.len());
    for (i, (fourcc, size)) in parts.iter().enumerate() {
        bytes[0x20 + 4 * i..0x24 + 4 * i].copy_from_slice(&(at as u32).to_le_bytes());
        bytes[at..at + 4].copy_from_slice(&fourcc.0);
        bytes[at + 4..at + 8].copy_from_slice(&(*size as u32).to_le_bytes());
        data.push(at + 8);
        at += 8 + size;
    }
    // The part of ours.
    let p = data[0];
    bytes[p..p + 4].copy_from_slice(MAGIC);
    bytes[p + 4] = head.stage;
    bytes[p + 5] = head.flags;
    bytes[p + 6..p + 8].copy_from_slice(&head.vgprs.to_le_bytes());
    bytes[p + 8..p + 12].copy_from_slice(&head.user_sgprs.to_le_bytes());
    for (i, w) in head.words.iter().enumerate() {
        bytes[p + 12 + 4 * i..p + 16 + 4 * i].copy_from_slice(&w.to_le_bytes());
    }
    for (i, n) in [extra.len(), code, metadata].into_iter().enumerate() {
        bytes[p + 0x20 + 4 * i..p + 0x24 + 4 * i].copy_from_slice(&(n as u32).to_le_bytes());
    }
    let code_at = p + HEAD + extra.len();
    bytes[p + HEAD..code_at].copy_from_slice(extra);
    if let Some(sigs) = &sigs {
        for (sig, &at) in sigs.iter().zip(&data[1..]) {
            bytes[at..at + sig.len()].copy_from_slice(sig);
        }
    }
    Draft {
        bytes,
        code: code_at..code_at + code,
        metadata: code_at + code..code_at + code + metadata,
    }
}

/// VGPRs from a program resource register: granules of 4 (wave64) or 8.
fn vgprs(rsrc1: u32, wave32: bool) -> u16 {
    (((rsrc1 & 0x3f) + 1) * if wave32 { 8 } else { 4 }).min(256) as u16
}

/// The container for a compute program whose packet is `packet`, with room
/// for `code` and `metadata` bytes of the two other pieces.
pub fn compute(packet: &[u8], code: usize, metadata: usize) -> Result<Draft, Refusal> {
    if packet.len() != COMPUTE_PACKET {
        return Err("a compute packet that is not 0x1c bytes long");
    }
    sizes(code, metadata, COMPUTE_METADATA.1)?;
    let word = |at: usize| u32_at(packet, at).unwrap_or(0);
    let threads: [u32; 3] =
        std::array::from_fn(|i| u16_at(packet, C_THREADS + 2 * i).map_or(0, u32::from));
    // What a thread group can be on the host: x and y up to 1024, z up to
    // 64, 1024 threads in all. The sides are the title's 16-bit values, and
    // three of them multiplied in 32 bits wrap (0x8000 twice and 4 make 0),
    // so the product is taken in 64.
    let total: u64 = threads.iter().map(|&n| u64::from(n)).product();
    let limits = [1024, 1024, 64];
    if threads.iter().zip(limits).any(|(&n, max)| n == 0 || n > max) || total > 1024 {
        return Err("a compute packet with an implausible thread-group size");
    }
    let wave32 = u16_at(packet, C_FLAGS).unwrap_or(0) & C_WAVE32 != 0;
    let rsrc2 = word(C_RSRC2);
    let head = Head {
        stage: COMPUTE,
        flags: if wave32 { WAVE32 } else { 0 },
        vgprs: vgprs(word(C_RSRC1), wave32),
        user_sgprs: rsrc2 >> 1 & 0x1f,
        words: [rsrc2, threads[0], threads[1], threads[2], 0],
    };
    Ok(draft(&head, &[], code, metadata, None))
}

/// One element of the input layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    /// Its semantic name: letters, digits and underscores.
    pub name: String,
    /// DXGI format.
    pub format: u32,
    /// Byte offset in the vertex.
    pub offset: u32,
}

/// The fixed-function registers of a graphics packet, as stored. What each
/// means on the host is the caller's to decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Registers {
    pub cb_color_control: u32,
    pub pa_cl_clip_cntl: u32,
    pub pa_su_sc_mode_cntl: u32,
    pub db_depth_control: u32,
    pub db_stencil_control: u32,
    pub db_stencilrefmask: u32,
    pub db_stencilrefmask_bf: u32,
    pub cb_target_mask: u32,
    pub cb_blend_control: [u32; 8],
}

/// A pixel program's part of the packet.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pixel {
    vgprs: u16,
    /// `SPI_PS_IN_CONTROL.PS_W32_EN`: the program runs in waves of 32.
    wave32: bool,
    user_sgprs: u32,
    input_addr: u32,
    db_shader_control: u32,
    col_format: u32,
    /// `SPI_PS_INPUT_CNTL` of each interpolant.
    controls: Vec<u32>,
}

/// A graphics pipeline's packet and state piece, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Graphics {
    pub registers: Registers,
    /// DXGI formats of the render targets, `targets` of them.
    pub target_formats: [u8; 8],
    pub targets: u8,
    /// DXGI format of the depth buffer, 0 for none.
    pub depth_format: u8,
    /// 3 triangles, 19 the console's rectangles.
    pub topology: u8,
    /// Every element is of slot 0, per vertex, semantic index 0.
    pub elements: Vec<Element>,
    vertex_vgprs: u16,
    vertex_user_sgprs: u32,
    /// `exp param<n>` of the vertex program, n below this.
    params: u32,
    /// Clip distances the vertex program exports after its position.
    clips: u8,
    pixel: Option<Pixel>,
}

impl Graphics {
    /// Reads a graphics packet and its state piece; `pixel` says whether
    /// the description has a pixel stage. Refuses what the layouts in the
    /// module notes do not cover, and every value whose meaning for a
    /// translated program is not known.
    pub fn parse(packet: &[u8], state: &[u8], pixel: bool) -> Result<Graphics, Refusal> {
        let word = |at: usize| u32_at(packet, at).unwrap_or(0);
        // The packet's two sizes must add up to it.
        let second = usize::from(u16_at(packet, G_SECOND_SIZE).unwrap_or(0));
        if packet.len() > MAX_PACKET
            || packet.len() < G_INPUT_CNTL + G_TARGET_INFO
            || u16_at(packet, G_FIXED_SIZE) != Ok(G_FIXED)
            || packet.len() != G_SECOND + second
        {
            return Err("a graphics packet of an unknown shape");
        }
        // Input controls fill what lies between the second block's head
        // and the render target information.
        let controls = second - G_SECOND_HEAD - G_TARGET_INFO;
        if !controls.is_multiple_of(4) {
            return Err("a graphics packet of an unknown shape");
        }
        // The vertex program's parameter exports.
        let out = word(G_VS_OUT_CONFIG);
        let params = match out {
            0x80 => 0,
            _ if out & !0x3e == 0 => (out >> 1 & 0x1f) + 1,
            _ => return Err("SPI_VS_OUT_CONFIG bits that are not handled"),
        };
        // The position export, and the clip distances after it if the
        // clipper has any enabled.
        let clips = clip_distances(word(G_POS_FORMAT), word(G_VS_OUT_CNTL))?;
        // Words that were zero in every call captured. The three before
        // DB_DEPTH_CONTROL sit where polygon offset values would (a guess),
        // the one after DB_STENCILREFMASK_BF is not identified; each is
        // named, so that a refusal says which one carried a value.
        for (i, why) in [
            "a graphics packet with a value at +0xac, where depth bias was guessed (zero in every call captured)",
            "a graphics packet with a value at +0xb0, where depth bias was guessed (zero in every call captured)",
            "a graphics packet with a value at +0xb4, where depth bias was guessed (zero in every call captured)",
        ]
        .into_iter()
        .enumerate()
        {
            if word(G_DEPTH_BIAS + 4 * i) != 0 {
                return Err(why);
            }
        }
        if word(G_UNKNOWN) != 0 {
            return Err(
                "a graphics packet with a value at +0xc8, a word not identified (zero in every call captured)",
            );
        }
        let pixel = if pixel {
            if word(G_RSRC1_PS) == 0 {
                return Err("a pixel stage whose packet has none");
            }
            let control = word(G_PS_IN_CONTROL);
            let interpolants = (control & 0x3f) as usize;
            // NUM_INTERP (bits 0-5) and PS_W32_EN (bit 15, a pixel program
            // that runs in waves of 32: Mesa sets it from the shader's wave
            // size). The rest are fields of their own, each refused by name.
            let handled = 0x3f | PS_W32_EN;
            if control & !handled != 0 {
                return Err(outside(
                    control,
                    !handled,
                    &[
                        (1 << 6, "SPI_PS_IN_CONTROL PARAM_GEN (bit 6)"),
                        (1 << 7, "SPI_PS_IN_CONTROL OFFCHIP_PARAM_EN (bit 7)"),
                        (1 << 8, "SPI_PS_IN_CONTROL LATE_PC_DEALLOC (bit 8)"),
                        (
                            0x3e00,
                            "SPI_PS_IN_CONTROL NUM_PRIM_INTERP (bits 9-13, per-primitive inputs)",
                        ),
                        (1 << 14, "SPI_PS_IN_CONTROL BC_OPTIMIZE_DISABLE (bit 14)"),
                    ],
                    "SPI_PS_IN_CONTROL bits above bit 15",
                ));
            }
            if interpolants > controls / 4 {
                return Err(
                    "SPI_PS_IN_CONTROL NUM_INTERP (bits 0-5) counts more inputs than the packet has controls",
                );
            }
            let wave32 = control & PS_W32_EN != 0;
            let controls: Vec<u32> = (0..interpolants)
                .map(|k| word(G_INPUT_CNTL + 4 * k))
                .collect();
            for &c in &controls {
                // An input that is a constant: OFFSET 0x20 and DEFAULT_VAL
                // (bits 8-9), nothing else.
                if c & 0x3f == CONSTANT_OFFSET {
                    if c & !(0x3f | 0x300) != 0 {
                        return Err(outside(
                            c,
                            !(0x3f | 0x300),
                            &INPUT_CNTL_FIELDS,
                            "SPI_PS_INPUT_CNTL bits outside its documented fields",
                        ));
                    }
                    continue;
                }
                // The parameter read (bits 0-4) and flat shading (bit 10).
                // DEFAULT_VAL means nothing beside a parameter offset.
                if c & !0x41f != 0 {
                    return Err(outside(
                        c,
                        !0x41f,
                        &INPUT_CNTL_FIELDS,
                        "SPI_PS_INPUT_CNTL bits outside its documented fields",
                    ));
                }
                if c & 0x1f >= params {
                    return Err(
                        "SPI_PS_INPUT_CNTL OFFSET (bits 0-5) names a parameter the vertex program does not export",
                    );
                }
            }
            // One parameter read both flat and interpolated has no one type.
            let is_param = |c: &u32| c & 0x3f != CONSTANT_OFFSET;
            for a in controls.iter().filter(|c| is_param(c)) {
                if controls
                    .iter()
                    .filter(|c| is_param(c))
                    .any(|b| a & 0x1f == b & 0x1f && a != b)
                {
                    return Err("a vertex parameter read both flat and interpolated");
                }
            }
            let db = word(G_DB_SHADER_CONTROL);
            if db & !DB_HANDLED != 0 {
                return Err(outside(
                    db,
                    !DB_HANDLED,
                    &[
                        (
                            1 << 1,
                            "DB_SHADER_CONTROL STENCIL_TEST_VAL_EXPORT_ENABLE (bit 1)",
                        ),
                        (
                            1 << 2,
                            "DB_SHADER_CONTROL STENCIL_OP_VAL_EXPORT_ENABLE (bit 2)",
                        ),
                        (1 << 3, "DB_SHADER_CONTROL bit 3, which has no field"),
                        (
                            1 << 7,
                            "DB_SHADER_CONTROL COVERAGE_TO_MASK_ENABLE (bit 7)",
                        ),
                        (1 << 8, "DB_SHADER_CONTROL MASK_EXPORT_ENABLE (bit 8)"),
                        (
                            1 << 9,
                            "DB_SHADER_CONTROL EXEC_ON_HIER_FAIL (bit 9)",
                        ),
                        (1 << 10, "DB_SHADER_CONTROL EXEC_ON_NOOP (bit 10)"),
                        (
                            1 << 11,
                            "DB_SHADER_CONTROL ALPHA_TO_MASK_DISABLE (bit 11)",
                        ),
                        (
                            1 << 16,
                            "DB_SHADER_CONTROL PRIMITIVE_ORDERED_PIXEL_SHADER (bit 16)",
                        ),
                        (
                            1 << 17,
                            "DB_SHADER_CONTROL EXEC_IF_OVERLAPPED (bit 17)",
                        ),
                        (
                            0x70_0000,
                            "DB_SHADER_CONTROL POPS_OVERLAP_NUM_SAMPLES (bits 20-22)",
                        ),
                        (
                            1 << 23,
                            "DB_SHADER_CONTROL PRE_SHADER_DEPTH_COVERAGE_ENABLE (bit 23)",
                        ),
                    ],
                    "DB_SHADER_CONTROL bits in a gap between its fields",
                ));
            }
            // CONSERVATIVE_Z_EXPORT (bits 13-14) qualifies a depth export;
            // 3 is reserved.
            let conservative = db >> 13 & 3;
            if conservative == 3 {
                return Err("DB_SHADER_CONTROL CONSERVATIVE_Z_EXPORT (bits 13-14) of 3, reserved");
            }
            if conservative != 0 && db & 1 == 0 {
                return Err(
                    "DB_SHADER_CONTROL CONSERVATIVE_Z_EXPORT (bits 13-14) without Z_EXPORT_ENABLE (bit 0)",
                );
            }
            // SPI_SHADER_Z_FORMAT 32_R is the depth export, and only it.
            if word(G_Z_FORMAT) != db & 1 {
                return Err(
                    "SPI_SHADER_Z_FORMAT other than 32_R with Z_EXPORT_ENABLE, or 32_R without it",
                );
            }
            // (the export formats are checked against the render targets,
            // once the state piece has them)
            Some(Pixel {
                vgprs: vgprs(word(G_RSRC1_PS), wave32),
                wave32,
                user_sgprs: word(G_RSRC2_PS) >> 1 & 0x1f,
                input_addr: word(G_PS_INPUT_ADDR),
                db_shader_control: db,
                col_format: word(G_COL_FORMAT),
                controls,
            })
        } else {
            if word(G_RSRC1_PS) != 0 {
                return Err("a packet for a pixel stage the description lacks");
            }
            None
        };

        // The state piece.
        let byte = |at: usize| state.get(at).copied().unwrap_or(0);
        let block = u32_at(state, S_BLOCK_SIZE).unwrap_or(u32::MAX) as usize;
        if state.len() > MAX_STATE
            || state.len() < S_BLOCK + 4
            || block < 4
            || state.len() != (S_BLOCK + block).next_multiple_of(4)
        {
            return Err("a state piece of an unknown shape");
        }
        let targets = byte(S_COUNT);
        let mut target_formats = [0u8; 8];
        target_formats.copy_from_slice(&state[S_TARGETS..S_TARGETS + 8]);
        if targets > 8
            || target_formats
                .iter()
                .enumerate()
                .any(|(i, &f)| (f != 0) != (i < usize::from(targets)))
        {
            return Err("a state piece whose render targets disagree with their count");
        }
        if u32_at(state, S_SAMPLES) != Ok(1) || u32_at(state, S_SAMPLE_MASK) != Ok(u32::MAX) {
            return Err("a multisampled pipeline, or one with a sample mask");
        }
        let topology = byte(S_TOPOLOGY);
        if !matches!(topology, 3 | 19) {
            return Err("a primitive topology type other than triangles and rectangles");
        }
        // Each target's export format against the kind of target it feeds.
        // The 32-bit formats carry the channels whole, whatever the target
        // is; the 16-bit float format feeds a float target; the 16-bit
        // integer formats feed the integer target of their signedness. A
        // target past the count has no format, and takes the export (if
        // any) as a float one.
        if let Some(p) = &pixel {
            for n in 0..8 {
                let kind = if n < usize::from(targets) {
                    target_kind(target_formats[n])
                } else {
                    TargetKind::Float
                };
                match (p.col_format >> (4 * n) & 0xf, kind) {
                    (0..=3 | 9, _) | (4, TargetKind::Float) => {}
                    (4, _) => {
                        return Err(
                            "SPI_SHADER_COL_FORMAT FP16_ABGR (4) for an integer render target",
                        );
                    }
                    (5, _) => return Err("SPI_SHADER_COL_FORMAT UNORM16_ABGR (5)"),
                    (6, _) => return Err("SPI_SHADER_COL_FORMAT SNORM16_ABGR (6)"),
                    (7, TargetKind::Uint) | (8, TargetKind::Sint) => {}
                    (7, _) => {
                        return Err(
                            "SPI_SHADER_COL_FORMAT UINT16_ABGR (7) for a target that is not an unsigned integer",
                        );
                    }
                    (8, _) => {
                        return Err(
                            "SPI_SHADER_COL_FORMAT SINT16_ABGR (8) for a target that is not a signed integer",
                        );
                    }
                    _ => return Err("SPI_SHADER_COL_FORMAT of 10 or more, which has no format"),
                }
            }
        }
        // The input layout: a count, then elements back to back.
        let layout = &state[S_BLOCK..S_BLOCK + block];
        let count = u32_at(layout, 0).unwrap_or(u32::MAX) as usize;
        if count > MAX_ELEMENTS {
            return Err("an input layout with an implausible element count");
        }
        let mut elements = Vec::with_capacity(count);
        let mut at = 4;
        for _ in 0..count {
            const BAD: Refusal = "an input layout that does not parse";
            let len = u32_at(layout, at).map_err(|_| BAD)? as usize;
            // The name with its NUL, then six words.
            let name = layout
                .get(at + 4..(at + 4).saturating_add(len))
                .ok_or(BAD)?;
            let Some((&0, name)) = name.split_last() else {
                return Err(BAD);
            };
            // (a name that ends in a digit would carry its semantic index,
            // which the host wants apart from the name: none was seen)
            if name.is_empty()
                || name.len() >= NAME
                || !name.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
                || name.last().is_some_and(u8::is_ascii_digit)
                || name[..name.len().min(3)].eq_ignore_ascii_case(b"SV_")
            {
                return Err("an input element with a name that is not a plain semantic");
            }
            at += 4 + len;
            let words: [u32; 6] = match layout.get(at..at + 24) {
                Some(w) => std::array::from_fn(|i| u32_at(w, 4 * i).unwrap_or(0)),
                None => return Err(BAD),
            };
            at += 24;
            // Words 0, 2, 4 and 5 lie where D3D12's element description has
            // the semantic index, the slot, the class and the step rate, in
            // that order (words 1 and 3 are where it has the format and the
            // offset). All were zero wherever seen, so that order is a
            // guess, and each refusal names the word that carried a value.
            for (at, why) in [
                (0, "an input element with a value in word 0 (the semantic index, by a guess)"),
                (2, "an input element with a value in word 2 (the slot, by a guess)"),
                (4, "an input element with a value in word 4 (the class, by a guess)"),
                (5, "an input element with a value in word 5 (the step rate, by a guess)"),
            ] {
                if words[at] != 0 {
                    return Err(why);
                }
            }
            if words[3] > 0x800 {
                return Err("an input element at an implausible byte offset");
            }
            elements.push(Element {
                // (checked ASCII above)
                name: String::from_utf8_lossy(name).into_owned(),
                format: words[1],
                offset: words[3],
            });
        }
        if at != block {
            return Err("an input layout that does not fill its block");
        }
        Ok(Graphics {
            registers: Registers {
                cb_color_control: word(G_CB_COLOR_CONTROL),
                pa_cl_clip_cntl: word(G_CLIP_CNTL),
                pa_su_sc_mode_cntl: word(G_SU_SC_MODE_CNTL),
                db_depth_control: word(G_DEPTH_CONTROL),
                db_stencil_control: word(G_STENCIL_CONTROL),
                db_stencilrefmask: word(G_STENCILREFMASK),
                db_stencilrefmask_bf: word(G_STENCILREFMASK + 4),
                cb_target_mask: word(G_TARGET_MASK),
                cb_blend_control: std::array::from_fn(|i| word(G_BLEND + 4 * i)),
            },
            target_formats,
            targets,
            depth_format: byte(S_DEPTH),
            topology,
            elements,
            vertex_vgprs: vgprs(word(G_RSRC1_VS), false),
            vertex_user_sgprs: word(G_RSRC2_VS) >> 1 & 0x1f,
            params,
            clips,
            pixel,
        })
    }

    /// The registers [`parse`](Self::parse) judges, as `name=value` for a
    /// log line (what a packet that is refused held; a short packet gives
    /// what is there). Values of the run's own, never kept.
    pub fn describe(packet: &[u8], state: &[u8]) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        let mut note = |name: &str, at: usize, from: &[u8]| {
            match u32_at(from, at) {
                Ok(v) => write!(out, "{name}={v:#x} "),
                Err(_) => write!(out, "{name}=none "),
            }
            .ok();
        };
        for (name, at) in [
            ("SPI_PS_IN_CONTROL", G_PS_IN_CONTROL),
            ("SPI_PS_INPUT_ADDR", G_PS_INPUT_ADDR),
            ("DB_SHADER_CONTROL", G_DB_SHADER_CONTROL),
            ("SPI_SHADER_Z_FORMAT", G_Z_FORMAT),
            ("SPI_SHADER_COL_FORMAT", G_COL_FORMAT),
            ("CB_COLOR_CONTROL", G_CB_COLOR_CONTROL),
            ("SPI_VS_OUT_CONFIG", G_VS_OUT_CONFIG),
            ("SPI_SHADER_POS_FORMAT", G_POS_FORMAT),
            ("PA_CL_VS_OUT_CNTL", G_VS_OUT_CNTL),
            ("+0xac", G_DEPTH_BIAS),
            ("+0xb0", G_DEPTH_BIAS + 4),
            ("+0xb4", G_DEPTH_BIAS + 8),
            ("+0xc8", G_UNKNOWN),
        ] {
            note(name, at, packet);
        }
        // The input controls the packet counts, at most eight.
        let count = (u32_at(packet, G_PS_IN_CONTROL).unwrap_or(0) & 0x3f).min(8) as usize;
        for k in 0..count {
            note(&format!("SPI_PS_INPUT_CNTL_{k}"), G_INPUT_CNTL + 4 * k, packet);
        }
        // The state piece's formats: one byte each.
        let byte = |at: usize| state.get(at).map_or_else(|| "none".into(), |b| b.to_string());
        write!(
            out,
            "depth_format={} targets={} formats={:?}",
            byte(S_DEPTH),
            byte(S_COUNT),
            state.get(S_TARGETS..S_TARGETS + 8).unwrap_or(&[])
        )
        .ok();
        // The input layout's six words of each element, as stored, so that
        // a refusal for one of them says which word carried the value.
        let layout = state.get(S_BLOCK..).unwrap_or(&[]);
        let count = (u32_at(layout, 0).unwrap_or(0) as usize).min(MAX_ELEMENTS);
        let mut at = 4;
        for _ in 0..count {
            let Ok(len) = u32_at(layout, at) else { break };
            at = at.saturating_add(4).saturating_add(len as usize);
            if at > layout.len() {
                break;
            }
            let words: Vec<u32> = (0..6)
                .map_while(|k| u32_at(layout, at + 4 * k).ok())
                .collect();
            write!(out, " element={words:x?}").ok();
            at += 24;
        }
        out
    }

    /// Whether the packet has a pixel stage.
    pub fn has_pixel(&self) -> bool {
        self.pixel.is_some()
    }

    /// Bit per vertex parameter the pixel program reads flat. Such a
    /// parameter is declared a 32-bit integer on both sides, so that
    /// whatever the vertex program exports arrives bit for bit.
    fn flat(&self) -> u32 {
        let controls = self.pixel.iter().flat_map(|p| &p.controls);
        controls
            .filter(|&&c| c & 0x3f != CONSTANT_OFFSET && c & 0x400 != 0)
            .fold(0, |m, &c| m | 1 << (c & 0x1f))
    }

    /// The row of vertex parameter `p` in both stages' signatures.
    fn param(&self, p: u32) -> Row<'static> {
        Row {
            name: "TEXCOORD",
            index: p,
            system: 0,
            component: if self.flat() >> p & 1 != 0 { 1 } else { 3 },
            register: p,
            mask: 0xf,
        }
    }

    /// The position's row: after the parameters, on both sides.
    fn position(&self) -> Row<'static> {
        Row {
            name: "SV_Position",
            index: 0,
            system: 1,
            component: 3,
            register: self.params,
            mask: 0xf,
        }
    }

    /// The container for the vertex program, with room for its two pieces.
    pub fn vertex(&self, code: usize, metadata: usize) -> Result<Draft, Refusal> {
        sizes(code, metadata, GRAPHICS_METADATA.1)?;
        let mut extra = vec![0u8; ELEMENT * self.elements.len()];
        for (e, to) in self.elements.iter().zip(extra.chunks_exact_mut(ELEMENT)) {
            to[..e.name.len()].copy_from_slice(e.name.as_bytes());
            // semantic index, format, slot, byte offset
            for (i, v) in [0, e.format, 0, e.offset].into_iter().enumerate() {
                to[NAME + 4 * i..NAME + 4 * i + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        // The elements whose fetch is known are the input signature, each
        // in the register of its place in the layout. The host's input
        // layout may hold more than the program declares.
        let inputs: Vec<Row> = self
            .elements
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let (_, channels) = element_shape(e.format)?;
                Some(Row {
                    name: &e.name,
                    index: 0,
                    system: 0,
                    component: 3,
                    register: i as u32,
                    mask: (1 << channels) - 1,
                })
            })
            .collect();
        let mut outputs: Vec<Row> = (0..self.params).map(|p| self.param(p)).collect();
        outputs.push(self.position());
        // The clip distances: up to four to a row, in the registers after
        // the position's (D3D_NAME_CLIP_DISTANCE is 2).
        for v in 0..u32::from(self.clips).div_ceil(4) {
            let left = u32::from(self.clips) - 4 * v;
            outputs.push(Row {
                name: "SV_ClipDistance",
                index: v,
                system: 2,
                component: 3,
                register: self.params + 1 + v,
                mask: ((1u32 << left.min(4)) - 1) as u8,
            });
        }
        let head = Head {
            stage: VERTEX,
            flags: 0,
            vgprs: self.vertex_vgprs,
            user_sgprs: self.vertex_user_sgprs,
            words: [self.params, self.flat(), self.elements.len() as u32, 0, 0],
        };
        Ok(draft(
            &head,
            &extra,
            code,
            metadata,
            Some([&inputs[..], &outputs[..]]),
        ))
    }

    /// The container for the pixel program, with room for its two pieces.
    pub fn pixel(&self, code: usize, metadata: usize) -> Result<Draft, Refusal> {
        let Some(pixel) = &self.pixel else {
            return Err("a pixel stage whose packet has none");
        };
        sizes(code, metadata, GRAPHICS_METADATA.1)?;
        // Each parameter read, once, then the system values the packet
        // enables: the position (bits 8-11) where the vertex program has
        // it, the front face (bit 12) after it.
        // (an input that is a constant reads no parameter)
        let mut read: Vec<u32> = pixel
            .controls
            .iter()
            .filter(|&&c| c & 0x3f != CONSTANT_OFFSET)
            .map(|c| c & 0x1f)
            .collect();
        read.sort_unstable();
        read.dedup();
        let mut inputs: Vec<Row> = read.into_iter().map(|p| self.param(p)).collect();
        // (bit 15, the fixed-point position, is made from the position's x
        // and y, so it brings the row as well)
        if pixel.input_addr & 0x8f00 != 0 {
            inputs.push(self.position());
        }
        if pixel.input_addr & 0x1000 != 0 {
            inputs.push(Row {
                name: "SV_IsFrontFace",
                index: 0,
                system: 9,
                component: 1,
                register: self.params + 1,
                mask: 1,
            });
        }
        // A render target per export format: 16-bit values travel two to a
        // dword and are declared so, which is what lets the lowering take
        // a compressed export apart. The 32-bit formats carry the bits of
        // the target's own type: an integer target gets its program's
        // output declared as that integer.
        let mut outputs = Vec::new();
        for n in 0..8usize {
            let kind = if n < usize::from(self.targets) {
                target_kind(self.target_formats[n])
            } else {
                TargetKind::Float
            };
            let component = match (pixel.col_format >> (4 * n) & 0xf, kind) {
                (0, _) => continue,
                (4, _) => 6,
                (7, _) => 4,
                (8, _) => 5,
                (_, TargetKind::Float) => 3,
                (_, TargetKind::Uint) => 1,
                (_, TargetKind::Sint) => 2,
            };
            outputs.push(Row {
                name: "SV_Target",
                index: n as u32,
                system: 64,
                component,
                register: n as u32,
                mask: 0xf,
            });
        }
        if pixel.db_shader_control & 1 != 0 {
            // CONSERVATIVE_Z_EXPORT says the exported depth is never below
            // (2) or never above (1) the one the rasteriser made. Mesa
            // derives the field from the shader's depth layout, "greater"
            // setting EXPORT_GREATER_THAN_Z and "less" EXPORT_LESS_THAN_Z:
            // HLSL's SV_DepthGreaterEqual and SV_DepthLessEqual.
            let system = match pixel.db_shader_control >> 13 & 3 {
                1 => 68,
                2 => 67,
                _ => 65,
            };
            outputs.push(Row {
                name: "SV_Depth",
                index: 0,
                system,
                component: 3,
                register: u32::MAX,
                mask: 1,
            });
        }
        let extra: Vec<u8> = pixel
            .controls
            .iter()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        let head = Head {
            stage: PIXEL,
            flags: if pixel.wave32 { WAVE32 } else { 0 },
            vgprs: pixel.vgprs,
            user_sgprs: pixel.user_sgprs,
            words: [
                pixel.input_addr,
                pixel.db_shader_control,
                pixel.col_format,
                self.params,
                pixel.controls.len() as u32,
            ],
        };
        Ok(draft(
            &head,
            &extra,
            code,
            metadata,
            Some([&inputs[..], &outputs[..]]),
        ))
    }
}

/// Whether a filled [`Draft`] holds a program: what is wrong with its
/// pieces if not. Copies nothing (a pipeline's creation runs it on the
/// title's thread).
pub fn check(container: &[u8]) -> Result<(), ShaderError> {
    let c = crate::Container::parse(container)?;
    let part = c
        .part(FourCC::PIECES)
        .ok_or(Malformed("no part made from pieces"))?;
    read(part.data).map(|_| ())
}

/// The title's own hash of a program, the key its two kinds of container
/// share. In the title's container it is the 16 bytes after the dword of
/// flags in the `HASH` part; in a container made from pieces it is the 16
/// bytes after the tag of the 0x43 record that ends the metadata piece.
/// This was worked out by comparing the containers of earlier runs, kept
/// outside the repository: nearly every program made from pieces had a title
/// container with the same 16 bytes. The code bytes are no key, because the
/// code piece of a program differs from the title's own code for it (vertex
/// fetch and export handling, and trailing bytes).
pub type ProgramHash = [u8; 16];

/// The key of a container made from pieces. `None` for any other container,
/// or one cut short. Copies nothing and cannot panic on garbled bytes: every
/// read goes through a bounds check.
pub fn program_of_pieces(container: &[u8]) -> Option<ProgramHash> {
    let c = crate::Container::parse(container).ok()?;
    let part = c.part(FourCC::PIECES)?.data;
    if part.get(..4) != Some(&MAGIC[..]) {
        return None;
    }
    // the metadata is the third stretch after the header: its place is set by
    // the sizes of the two before it, as `read` finds it
    let size = |at: usize| u32_at(part, at).ok().map(|s| s as usize);
    let (extra, code, meta) = (size(0x20)?, size(0x24)?, size(0x28)?);
    let from = HEAD.checked_add(extra)?.checked_add(code)?;
    let meta = part.get(from..from.checked_add(meta)?)?;
    // the record that ends it holds the hash, if the piece has one
    let last = meta.get(meta.len().checked_sub(RECORD)?..)?;
    if last[0] != 0x43 {
        return None;
    }
    last.get(4..20)?.try_into().ok()
}

/// The key of the title's own container. Reads only the part table and the
/// one `HASH` part, so a container cut after that part still answers (this is
/// how the pipeline book's index reads a prefix of each kept container) where
/// `Container::parse` would refuse it. `None` when no `HASH` part lies inside
/// what was given.
pub fn program_of_stock(container: &[u8]) -> Option<ProgramHash> {
    if container.get(..4) != Some(&b"DXBC"[..]) {
        return None;
    }
    let count = u32_at(container, 0x1c).ok()? as usize;
    // the same bound `Container::parse` puts on a part table
    for i in 0..count.min(256) {
        let Ok(at) = u32_at(container, 0x20 + 4 * i) else {
            break;
        };
        let at = at as usize;
        let Some(tag) = container.get(at..at.checked_add(4)?) else {
            continue;
        };
        if tag != FourCC::HASH.0 {
            continue;
        }
        // 8 bytes of part header, a dword of flags, then the digest
        let size = u32_at(container, at.checked_add(4)?).ok()? as usize;
        if size < 20 {
            return None;
        }
        return container
            .get(at.checked_add(12)?..at.checked_add(28)?)?
            .try_into()
            .ok();
    }
    None
}

/// Whether a program with a view from the resource heap gets the heap's base
/// as a root parameter (`VICTORY_HEAP_VIEW` set; read once). Off, such a
/// program keeps today's refusal. Tests run with it on.
fn heap_views_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cfg!(test) || std::env::var_os("VICTORY_HEAP_VIEW").is_some())
}

/// The program in the data of a part of ours.
pub(crate) fn program(part: &[u8]) -> Result<NativeProgram, ShaderError> {
    let (mut program, code) = read(part)?;
    program.code = code.to_vec();
    Ok(program)
}

/// Moves the resource heap's base one SGPR down in a program made from pieces
/// when the code shows the last user SGPR is not it. [`read`] puts the heap
/// at the last user SGPR (or the one before a sampler heap), which is where
/// the console's own containers have it, but a pieces program that declares a
/// heap view and no heap sampler may keep an unused sampler heap there and
/// the base before it. The registers the code reads on entry tell: when the
/// last one is never read and the one before it is, the base is the one
/// before, and the constants that [`read`] stretched over it end one dword
/// earlier. It acts only on a heap that has no sampler heap beside it, and
/// leaves a layout alone when a table or descriptor holds the lower SGPR or
/// the constants would be left with none. It needs `p`, so it cannot run in
/// [`read`], whose [`check`] runs on the title's thread at pipeline creation;
/// the translation calls it on a worker, and only for a program whose
/// resources do not trace with the layout as [`read`] made it. Returns
/// whether it moved the heap.
pub fn place_heap(native: &mut NativeProgram, p: &crate::ir::Program) -> bool {
    use crate::ir::Reg;
    let made = native.layout.iter().any(|q| {
        matches!(q.root, RootType::Other(c) if c == COMPUTE_PIECES || c == PIXEL_PIECES || c == VERTEX_BUFFERS)
    });
    let heap = native
        .layout
        .iter()
        .position(|q| q.root == RootType::ResourceHeap);
    let (true, Some(heap)) = (made, heap) else {
        return false;
    };
    if native.layout.iter().any(|q| q.root == RootType::SamplerHeap) {
        return false;
    }
    let last = native.layout[heap].user_data;
    let read = |n: u8| p.inputs.iter().any(|&(r, _)| r == Reg::S(n));
    // (a vertex program's first three SGPRs are the system's)
    let floor = if native.stage == HwStage::Vs { 4 } else { 1 };
    if last < floor || read(last) || !read(last - 1) {
        return false;
    }
    // what holds the lower SGPR: only constants with a dword to spare give it up
    let lower = u32::from(last - 1);
    match native.layout.iter().position(|q| {
        u32::from(q.user_data) <= lower && lower < u32::from(q.user_data) + q.sgprs
    }) {
        Some(c) if native.layout[c].root == RootType::Constants && native.layout[c].sgprs > 1 => {
            native.layout[c].sgprs -= 1;
        }
        Some(_) => return false,
        None => {}
    }
    native.layout[heap].user_data = last - 1;
    true
}

/// The program in the data of a part of ours, without its code, and the
/// code where it lies in the part.
fn read(part: &[u8]) -> Result<(NativeProgram, &[u8]), ShaderError> {
    const SHORT: ShaderError = Malformed("a part made from pieces is cut short");
    if part.get(..4) != Some(&MAGIC[..]) {
        return Err(Malformed("not a part made from pieces"));
    }
    let head = part.get(..HEAD).ok_or(SHORT)?;
    let (stage, wave64) = (head[4], head[5] & WAVE32 == 0);
    let vgprs = u16_at(head, 6)?;
    let user_sgprs = u32_at(head, 8)?;
    let words: [u32; 5] = std::array::from_fn(|i| u32_at(head, 12 + 4 * i).unwrap_or(0));
    // The three stretches after the header, by their sizes.
    let mut at = HEAD;
    let mut take = |size_at: usize| -> Result<&[u8], ShaderError> {
        let size = u32_at(head, size_at)? as usize;
        let piece = part
            .get(at..at.checked_add(size).ok_or(SHORT)?)
            .ok_or(SHORT)?;
        at += size;
        Ok(piece)
    };
    let (extra, code, meta) = (take(0x20)?, take(0x24)?, take(0x28)?);
    let (tag, records_at, uncounted) = match stage {
        COMPUTE => COMPUTE_METADATA,
        VERTEX | PIXEL => GRAPHICS_METADATA,
        _ => return Err(Malformed("a part made from pieces of an unknown stage")),
    };
    if vgprs > 256 || user_sgprs > 32 {
        return Err(Malformed("implausible register counts"));
    }
    // The metadata must be of the stage's kind and count the code.
    if u32_at(meta, 0)? != tag {
        return Err(Malformed("a metadata piece of an unknown kind"));
    }
    if (u32_at(meta, 8)? as usize)
        .checked_mul(4)
        .and_then(|n| n.checked_add(uncounted))
        != Some(code.len())
    {
        return Err(Malformed(
            "the metadata's code size disagrees with the code piece",
        ));
    }
    // Whole records, the shader hash last.
    let recs = meta.get(records_at..).unwrap_or(&[]);
    let (chunks, rest) = recs.as_chunks::<RECORD>();
    if !rest.is_empty() || chunks.last().is_none_or(|e| e[0] != 0x43) {
        return Err(Malformed(
            "metadata records that do not end in the shader hash",
        ));
    }
    let rules = RecordRules {
        constants: true,
        strides: true,
    };
    let mut r = native::records(recs, rules)?;
    if r.heap_view && heap_views_on() {
        // A view from the resource heap, whose base no record places: the
        // console's own containers put it at the last user SGPR, or the one
        // before it when the sampler heap follows. A program that has one but
        // no room for it keeps today's refusal at translation, as a refusal
        // here would leave the pipeline unmade; [`place_heap`] moves it when
        // the code shows the last SGPR is not it.
        let at = user_sgprs
            .checked_sub(1 + u32::from(r.heap_sampler))
            .filter(|&at| stage != VERTEX || at >= 3);
        if let Some(at) = at
            && r.layout.iter().all(|p| u32::from(p.user_data) < at)
        {
            r.layout.push(RootParam {
                root: RootType::ResourceHeap,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: at as u8,
                slot: u8::MAX,
            });
        }
    }
    if r.heap_sampler {
        // A sampler from the sampler heap, whose base no record places: it
        // was the last user SGPR in the eleven pixel programs seen with
        // one, all of one root signature.
        if stage == COMPUTE {
            return Err(ShaderError::Unsupported {
                offset: 0,
                what: "a sampler from the heap in a compute program made from pieces",
            });
        }
        let last = user_sgprs
            .checked_sub(1)
            .ok_or(Malformed("a heap sampler without user SGPRs"))?;
        if r.layout.iter().any(|p| u32::from(p.user_data) >= last) {
            return Err(Malformed("no user SGPR is left for the sampler heap"));
        }
        r.layout.push(RootParam {
            root: RootType::SamplerHeap,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: last as u8,
            slot: u8::MAX,
        });
    }
    native::size_constants(&mut r.layout, user_sgprs)?;
    let io = match stage {
        COMPUTE => {
            // Like a pixel program's entry below: it owns no user SGPR and
            // only says the program was made from pieces, which is what
            // lets the translation take the forms the pieces need (a
            // title's container never has one).
            r.layout.push(RootParam {
                root: RootType::Other(COMPUTE_PIECES),
                table_offset_dwords: 0,
                sgprs: 0,
                base_register: 0,
                user_data: 0,
                slot: u8::MAX,
            });
            StageIo::Cs(ComputeIo {
                rsrc2: words[0],
                threads: [words[1], words[2], words[3]],
            })
        }
        VERTEX => {
            // s0 and s1 are the vertex buffer table's address, s2 the base
            // vertex: no root parameter may claim them.
            if r.layout.iter().any(|p| p.user_data < 3) || user_sgprs < 3 {
                return Err(Malformed(
                    "a root parameter in the vertex program's system SGPRs",
                ));
            }
            r.layout.push(RootParam {
                root: RootType::Other(VERTEX_BUFFERS),
                table_offset_dwords: 0,
                sgprs: 2,
                base_register: 0,
                user_data: 0,
                slot: u8::MAX,
            });
            let (elements, rest) = extra.as_chunks::<ELEMENT>();
            if !rest.is_empty() || elements.len() != words[2] as usize || words[0] > 32 {
                return Err(Malformed(
                    "a vertex part whose elements disagree with their count",
                ));
            }
            StageIo::Vs {
                inputs: Vec::new(),
                outputs: (0..words[0] as u8)
                    .map(|n| VsOutput {
                        usage: Usage::Generic,
                        row: n,
                        param: n,
                        fp16: false,
                        pack2: false,
                    })
                    .collect(),
                fetch: elements
                    .iter()
                    .enumerate()
                    .map(|(i, e)| {
                        let word = |k: usize| u32_at(e, NAME + 4 * k).unwrap_or(0);
                        let format = word(1);
                        VsFetch {
                            row: i as u8,
                            slot: word(2),
                            offset: word(3),
                            // (0 for a format whose fetch is not known: no
                            // load then lies inside it)
                            bytes: element_shape(format).map_or(0, |s| s.0),
                            format,
                        }
                    })
                    .collect(),
            }
        }
        _ => {
            // A pixel program owns no user SGPR for this entry: it only says the
            // program was made from pieces, which is what lets its compressed
            // exports be taken apart (a title's container never has one).
            r.layout.push(RootParam {
                root: RootType::Other(PIXEL_PIECES),
                table_offset_dwords: 0,
                sgprs: 0,
                base_register: 0,
                user_data: 0,
                slot: u8::MAX,
            });
            let (controls, rest) = extra.as_chunks::<4>();
            if !rest.is_empty() || controls.len() != words[4] as usize || controls.len() > 32 {
                return Err(Malformed(
                    "a pixel part whose inputs disagree with their count",
                ));
            }
            StageIo::Ps {
                inputs: controls
                    .iter()
                    .enumerate()
                    .map(|(k, c)| {
                        let c = u32::from_le_bytes(*c);
                        // An input that is a constant names no parameter:
                        // its row says which constant.
                        if c & 0x3f == CONSTANT_OFFSET {
                            return PsInput {
                                usage: Usage::Generic,
                                row: PsInput::constant_row((c >> 8 & 3) as u8),
                                mask: 0xf,
                                attr: k as u8,
                                flat: false,
                                fp16: false,
                                pack2: false,
                                row2: 0,
                            };
                        }
                        PsInput {
                            usage: Usage::Generic,
                            row: (c & 0x1f) as u8,
                            mask: 0xf,
                            attr: k as u8,
                            flat: c & 0x400 != 0,
                            fp16: false,
                            pack2: false,
                            row2: 0,
                        }
                    })
                    .collect(),
                input_addr: words[0],
                db_shader_control: words[1],
                // The exports are final: nothing to patch.
                patches: Vec::new(),
            }
        }
    };
    let program = NativeProgram {
        stage: match stage {
            COMPUTE => HwStage::Cs,
            VERTEX => HwStage::Vs,
            _ => HwStage::Ps,
        },
        wave64,
        vgprs,
        layout: r.layout,
        used: r.used,
        io,
        code: Vec::new(),
    };
    Ok((program, code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::{ResourceType, UsedResource};

    /// `s_endpgm`.
    const END: u32 = 0xBF81_0000;

    fn put(to: &mut [u8], at: usize, v: u32) {
        to[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// A 0x41 record: resource type, root type, table offset, register,
    /// stride, descriptor dwords, user-data dword, root slot.
    fn resource(
        kind: u8,
        root: u8,
        offset: u32,
        register: u32,
        stride: u32,
        dwords: u8,
        user: u8,
        slot: u8,
    ) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[0] = 0x41;
        (e[4], e[5]) = (kind, root);
        put(&mut e, 8, offset);
        put(&mut e, 12, register);
        put(&mut e, 16, stride);
        (e[20], e[21], e[22]) = (dwords, user, slot);
        put(&mut e, 24, u32::MAX);
        e
    }

    /// A 0x42 record of `class` for `register`.
    fn declaration(class: u8, kind: u8, register: u32) -> [u8; 32] {
        declared(1, class, kind, register)
    }

    /// A 0x42 record with `byte8` (2 for a view taken from the heap).
    fn declared(byte8: u8, class: u8, kind: u8, register: u32) -> [u8; 32] {
        let mut e = [0u8; 32];
        (e[0], e[8], e[9], e[10]) = (0x42, byte8, class, kind);
        put(&mut e, 12, register);
        e
    }

    /// The 0x43 record, with a hash of our own.
    fn hash() -> [u8; 32] {
        let mut e = [0u8; 32];
        e[0] = 0x43;
        for (i, b) in e[4..20].iter_mut().enumerate() {
            *b = i as u8 + 1;
        }
        e
    }

    /// A metadata piece for a stage with `code` bytes of code.
    fn metadata(kind: (u32, usize, usize), code: usize, records: &[[u8; 32]]) -> Vec<u8> {
        let mut m = vec![0u8; kind.1];
        put(&mut m, 0, kind.0);
        put(&mut m, 8, ((code - kind.2) / 4) as u32);
        m.extend(records.iter().flatten());
        let after = (m.len() - 0x0c) as u32;
        put(&mut m, 4, after);
        m
    }

    /// A code piece of `dwords` dwords that ends the program at once.
    fn code(dwords: usize) -> Vec<u8> {
        let mut c = END.to_le_bytes().to_vec();
        c.resize(4 * dwords, 0);
        c
    }

    /// A compute packet: threads 8x8x1, 2 VGPR granules, `user` user SGPRs
    /// and the group id's x after them.
    fn compute_packet(user: u32) -> [u8; COMPUTE_PACKET] {
        let mut p = [0u8; COMPUTE_PACKET];
        p[C_THREADS..C_THREADS + 6].copy_from_slice(&[8, 0, 8, 0, 1, 0]);
        put(&mut p, C_RSRC1, 1);
        put(&mut p, C_RSRC2, user << 1 | 1 << 7);
        p
    }

    /// The program of a filled container, once [`check`] has passed it.
    fn checked(bytes: &[u8]) -> Result<NativeProgram, ShaderError> {
        check(bytes)?;
        let c = crate::Container::parse(bytes)?;
        program(c.part(FourCC::PIECES).unwrap().data)
    }

    /// Fills a draft's two pieces.
    fn fill(mut d: Draft, code: &[u8], meta: &[u8]) -> Vec<u8> {
        d.bytes[d.code].copy_from_slice(code);
        d.bytes[d.metadata].copy_from_slice(meta);
        d.bytes
    }

    #[test]
    fn a_compute_program_from_its_pieces() {
        let code = code(4);
        // A table in s4 with a texture at its start, a constant in s5.
        let meta = metadata(
            COMPUTE_METADATA,
            code.len(),
            &[
                resource(0, 3, 0, 7, 4, 8, 4, 0),
                resource(2, 0, 0, 9, 0, 4, 5, 1),
                declaration(0, 2, 7),
                hash(),
            ],
        );
        let draft = compute(&compute_packet(6), code.len(), meta.len()).unwrap();
        let bytes = fill(draft, &code, &meta);
        let p = checked(&bytes).unwrap();
        assert_eq!((p.stage, p.wave64, p.vgprs), (HwStage::Cs, true, 8));
        assert_eq!(p.code, code);
        let param = |root, user_data, slot, base_register| RootParam {
            root,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register,
            user_data,
            slot,
        };
        assert_eq!(
            p.layout,
            [
                param(RootType::Table, 4, 0, 7),
                param(RootType::Constants, 5, 1, 9),
                // (the mark of a program made from pieces, no user SGPR)
                RootParam {
                    root: RootType::Other(COMPUTE_PIECES),
                    table_offset_dwords: 0,
                    sgprs: 0,
                    base_register: 0,
                    user_data: 0,
                    slot: u8::MAX,
                },
            ]
        );
        assert_eq!(
            p.used[0],
            UsedResource {
                kind: ResourceType::Srv,
                root: RootType::Table,
                table_offset_dwords: 0,
                lower_bound: 7,
                stride: 4,
                dwords: 8,
                user_data: 4,
                slot: 0
            }
        );
        let StageIo::Cs(cs) = p.io else {
            panic!("{:?}", p.io)
        };
        assert_eq!((cs.threads, cs.user_sgprs()), ([8, 8, 1], 6));
        // The same pieces give the same bytes, other pieces other bytes.
        let again = fill(
            compute(&compute_packet(6), code.len(), meta.len()).unwrap(),
            &code,
            &meta,
        );
        assert_eq!(bytes, again);
        let other = fill(
            compute(&compute_packet(7), code.len(), meta.len()).unwrap(),
            &code,
            &meta,
        );
        assert_ne!(bytes, other);
        // And the program is found the way a container's is.
        let c = crate::Container::parse(&bytes).unwrap();
        let found = crate::native_programs(c.native_part().unwrap().data).unwrap();
        assert_eq!(found, [p]);
    }

    #[test]
    fn root_constants_run_to_the_next_parameter() {
        let code = code(1);
        // Constants in s4, a table in s6: the constants are s4 and s5.
        let records = [
            resource(2, 0, 0, 9, 0, 4, 4, 0),
            resource(3, 3, 0, 2, 4, 8, 6, 1),
            hash(),
        ];
        let meta = metadata(COMPUTE_METADATA, code.len(), &records);
        let p = checked(&fill(
            compute(&compute_packet(7), 4, meta.len()).unwrap(),
            &code,
            &meta,
        ))
        .unwrap();
        assert_eq!(
            (p.layout[0].root, p.layout[0].sgprs),
            (RootType::Constants, 2)
        );
        // A parameter past the user SGPRs is refused.
        let short = fill(
            compute(&compute_packet(6), 4, meta.len()).unwrap(),
            &code,
            &meta,
        );
        assert!(checked(&short).is_err());
    }

    #[test]
    fn compute_pieces_of_wrong_sizes_are_refused() {
        let packet = compute_packet(6);
        assert!(compute(&packet[..0x18], 4, 0x38).is_err(), "short packet");
        assert!(compute(&packet, 0, 0x38).is_err(), "no code");
        assert!(compute(&packet, 6, 0x38).is_err(), "half a dword");
        assert!(compute(&packet, MAX_CODE + 4, 0x38).is_err(), "absurd code");
        assert!(compute(&packet, 4, 0x18).is_err(), "no record");
        assert!(compute(&packet, 4, 0x39).is_err(), "part of a record");
        assert!(
            compute(&packet, 4, MAX_METADATA + 0x18).is_err(),
            "absurd metadata"
        );
        let mut threads = packet;
        threads[C_THREADS + 1] = 0x40;
        assert!(compute(&threads, 4, 0x38).is_err(), "absurd thread group");
        threads[C_THREADS..C_THREADS + 2].copy_from_slice(&[0, 0]);
        assert!(compute(&threads, 4, 0x38).is_err(), "no threads");
    }

    /// A compute packet with thread-group sides `sides`.
    fn with_threads(sides: [u16; 3]) -> [u8; COMPUTE_PACKET] {
        let mut p = compute_packet(6);
        for (i, n) in sides.into_iter().enumerate() {
            p[C_THREADS + 2 * i..C_THREADS + 2 * i + 2].copy_from_slice(&n.to_le_bytes());
        }
        p
    }

    #[test]
    fn thread_group_sides_are_bounded_one_by_one() {
        let made = |sides: [u16; 3]| compute(&with_threads(sides), 4, 0x38).is_ok();
        // Three 16-bit sides multiplied in 32 bits wrap to 0 here: the
        // product must be taken wider, and each side bounded.
        assert!(!made([0x8000, 0x8000, 4]));
        assert!(!made([0xffff, 0xffff, 0xffff]));
        assert!(!made([0, 1, 1]), "a side of 0");
        assert!(!made([1025, 1, 1]), "a side over 1024");
        assert!(!made([1, 1, 1025]));
        assert!(!made([1, 1, 65]), "z over 64");
        assert!(!made([1, 1, 1024]), "z over 64 within 1024 threads");
        assert!(!made([32, 32, 2]), "over 1024 threads in all");
        assert!(made([1024, 1, 1]));
        assert!(made([1, 1, 64]));
        assert!(made([32, 32, 1]));
        assert!(made([8, 8, 16]));
    }

    #[test]
    fn pieces_that_disagree_are_refused() {
        let code = code(2);
        let good = metadata(COMPUTE_METADATA, 8, &[hash()]);
        let make = |meta: &[u8]| {
            fill(
                compute(&compute_packet(6), 8, meta.len()).unwrap(),
                &code,
                meta,
            )
        };
        assert!(checked(&make(&good)).is_ok());
        // A code size that is not the code piece's.
        let mut wrong = good.clone();
        put(&mut wrong, 8, 3);
        assert!(checked(&make(&wrong)).is_err());
        // A graphics stage's tag on a compute stage's metadata.
        let mut tag = good.clone();
        put(&mut tag, 0, GRAPHICS_METADATA.0);
        assert!(checked(&make(&tag)).is_err());
        // Records of an unknown tag, and records without the last one.
        let mut unknown = hash();
        unknown[0] = 0x44;
        assert!(checked(&make(&metadata(COMPUTE_METADATA, 8, &[unknown, hash()]))).is_err());
        assert!(
            checked(&make(&metadata(
                COMPUTE_METADATA,
                8,
                &[declaration(0, 2, 1)]
            )))
            .is_err()
        );
        // A root type nothing is known of.
        let odd = resource(0, 7, 0, 1, 0, 4, 4, 0);
        assert!(checked(&make(&metadata(COMPUTE_METADATA, 8, &[odd, hash()]))).is_err());
        // A container cut anywhere does not parse.
        let whole = make(&good);
        for len in [0, 0x1f, 0x40, whole.len() - 1] {
            assert!(checked(&whole[..len]).is_err(), "{len:#x}");
        }
        // Nor does a part whose sizes reach past it.
        let c = crate::Container::parse(&whole).unwrap();
        let mut part = c.part(FourCC::PIECES).unwrap().data.to_vec();
        put(&mut part, 0x24, u32::MAX);
        assert!(program(&part).is_err());
    }

    /// A graphics packet: `interpolants` as (vertex parameter, flat), with
    /// a pixel stage when `pixel`, and `params` parameter exports.
    fn graphics_packet(pixel: bool, params: u32, interpolants: &[(u32, bool)]) -> Vec<u8> {
        let slots = interpolants.len().next_multiple_of(2);
        let second = G_SECOND_HEAD + 4 * slots + G_TARGET_INFO;
        let mut p = vec![0u8; G_SECOND + second];
        p[G_FIXED_SIZE..G_FIXED_SIZE + 2].copy_from_slice(&G_FIXED.to_le_bytes());
        p[G_SECOND_SIZE..G_SECOND_SIZE + 2].copy_from_slice(&(second as u16).to_le_bytes());
        // 2 VGPR granules and 10 user SGPRs for the vertex stage.
        put(&mut p, G_RSRC1_VS, 1);
        put(&mut p, G_RSRC2_VS, 10 << 1);
        put(
            &mut p,
            G_VS_OUT_CONFIG,
            if params == 0 { 0x80 } else { (params - 1) << 1 },
        );
        put(&mut p, G_POS_FORMAT, 4);
        if pixel {
            // 4 VGPR granules, 10 user SGPRs, perspective centre
            // barycentrics, the first target as 16-bit floats.
            put(&mut p, G_RSRC1_PS, 3);
            put(&mut p, G_RSRC2_PS, 10 << 1);
            put(&mut p, G_PS_INPUT_ADDR, 2);
            put(&mut p, G_PS_IN_CONTROL, interpolants.len() as u32);
            put(&mut p, G_DB_SHADER_CONTROL, 0x10);
            put(&mut p, G_COL_FORMAT, 4);
            for (k, &(param, flat)) in interpolants.iter().enumerate() {
                put(&mut p, G_INPUT_CNTL + 4 * k, param | u32::from(flat) << 10);
            }
        }
        p
    }

    /// A state piece: one target of format 28, triangles, and `elements`
    /// as (name, format, byte offset).
    fn state_piece(elements: &[(&str, u32, u32)]) -> Vec<u8> {
        let mut s = vec![0u8; S_BLOCK];
        s[S_TARGETS] = 28;
        (s[S_COUNT], s[S_TOPOLOGY]) = (1, 3);
        put(&mut s, S_SAMPLE_MASK, u32::MAX);
        put(&mut s, S_SAMPLES, 1);
        s.extend((elements.len() as u32).to_le_bytes());
        for &(name, format, offset) in elements {
            s.extend((name.len() as u32 + 1).to_le_bytes());
            s.extend(name.as_bytes());
            s.push(0);
            for v in [0, format, 0, offset, 0, 0] {
                s.extend(v.to_le_bytes());
            }
        }
        let block = (s.len() - S_BLOCK) as u32;
        put(&mut s, S_BLOCK_SIZE, block);
        s.resize(s.len().next_multiple_of(4), 0);
        s
    }

    const ELEMENTS: [(&str, u32, u32); 3] =
        [("POSITION", 6, 0), ("COLOR", 28, 12), ("TEXCOORD", 16, 16)];

    #[test]
    fn a_graphics_pipeline_from_its_pieces() {
        let packet = graphics_packet(true, 3, &[(0, false), (2, true), (1, false)]);
        let g = Graphics::parse(&packet, &state_piece(&ELEMENTS), true).unwrap();
        assert!(g.has_pixel());
        assert_eq!(
            (g.targets, g.target_formats[0], g.topology, g.depth_format),
            (1, 28, 3, 0)
        );
        assert_eq!(
            g.elements[1],
            Element {
                name: "COLOR".into(),
                format: 28,
                offset: 12
            }
        );
        let code = code(11);
        // A table in s8 and a sampler from the heap, which gets s9.
        let meta = metadata(
            GRAPHICS_METADATA,
            code.len(),
            &[
                resource(0, 3, 0, 1, 4, 8, 8, 0),
                declaration(0, 2, 1),
                declaration(3, 14, 0),
                hash(),
            ],
        );
        let vs = checked(&fill(
            g.vertex(code.len(), meta.len()).unwrap(),
            &code,
            &meta,
        ))
        .unwrap();
        assert_eq!((vs.stage, vs.vgprs), (HwStage::Vs, 8));
        let roots: Vec<_> = vs
            .layout
            .iter()
            .map(|p| (p.root, p.user_data, p.sgprs))
            .collect();
        assert_eq!(
            roots,
            [
                (RootType::Table, 8, 1),
                (RootType::SamplerHeap, 9, 1),
                (RootType::Other(VERTEX_BUFFERS), 0, 2)
            ]
        );
        let StageIo::Vs { outputs, fetch, .. } = &vs.io else {
            panic!("{:?}", vs.io)
        };
        assert_eq!(
            outputs.iter().map(|o| (o.row, o.param)).collect::<Vec<_>>(),
            [(0, 0), (1, 1), (2, 2)]
        );
        assert_eq!(
            fetch[2],
            VsFetch {
                row: 2,
                slot: 0,
                offset: 16,
                bytes: 8,
                format: 16
            }
        );
        let ps = checked(&fill(
            g.pixel(code.len(), meta.len()).unwrap(),
            &code,
            &meta,
        ))
        .unwrap();
        assert_eq!((ps.stage, ps.vgprs), (HwStage::Ps, 16));
        let StageIo::Ps {
            inputs,
            input_addr,
            db_shader_control,
            patches,
        } = &ps.io
        else {
            panic!("{:?}", ps.io)
        };
        assert_eq!(
            (*input_addr, *db_shader_control, patches.len()),
            (2, 0x10, 0)
        );
        assert_eq!(
            inputs
                .iter()
                .map(|i| (i.attr, i.row, i.flat))
                .collect::<Vec<_>>(),
            [(0, 0, false), (1, 2, true), (2, 1, false)]
        );
        // The two stages agree on every parameter: name, register, type.
        let rows = |bytes: &[u8], fourcc| {
            let c = crate::Container::parse(bytes).unwrap();
            let part = c.part(fourcc).unwrap().data;
            let count = u32_at(part, 0).unwrap() as usize;
            (0..count)
                .map(|i| {
                    let at = 8 + 32 * i;
                    let word = |k: usize| u32_at(part, at + 4 * k).unwrap();
                    (word(2), word(3), word(4), word(5))
                })
                .collect::<Vec<_>>()
        };
        let vs_bytes = fill(g.vertex(code.len(), meta.len()).unwrap(), &code, &meta);
        let ps_bytes = fill(g.pixel(code.len(), meta.len()).unwrap(), &code, &meta);
        let out = rows(&vs_bytes, FourCC::OSG1);
        // (index, system value, type, register): parameter 2 is flat.
        assert_eq!(
            out,
            [(0, 0, 3, 0), (1, 0, 3, 1), (2, 0, 1, 2), (0, 1, 3, 3)]
        );
        assert_eq!(rows(&ps_bytes, FourCC::ISG1), out[..3]);
        assert_eq!(rows(&ps_bytes, FourCC::OSG1), [(0, 64, 6, 0)]);
        assert_eq!(
            rows(&vs_bytes, FourCC::ISG1),
            [(0, 0, 3, 0), (0, 0, 3, 1), (0, 0, 3, 2)]
        );
    }

    #[test]
    fn both_containers_of_a_program_give_one_key() {
        let digest: ProgramHash = hash()[4..20].try_into().unwrap();
        // the title's own container: a dword of flags, then the digest
        let stock_of = |digest: &ProgramHash| {
            let mut part = vec![0u8; 4];
            part.extend(digest);
            crate::dxbc::build(&[
                (FourCC::SFI0, &[0; 8]),
                (FourCC::HASH, &part),
                (FourCC::DXIL, &[7; 5000]),
            ])
        };
        let made_of = |record: [u8; 32]| {
            let code = code(4);
            let meta = metadata(COMPUTE_METADATA, code.len(), &[record]);
            fill(
                compute(&compute_packet(7), code.len(), meta.len()).unwrap(),
                &code,
                &meta,
            )
        };
        let stock = stock_of(&digest);
        let made = made_of(hash());
        assert_eq!(program_of_pieces(&made), Some(digest));
        assert_eq!(program_of_stock(&stock), Some(digest));
        assert_eq!(program_of_pieces(&made), program_of_stock(&stock));
        // a container of one kind has no key of the other kind
        assert_eq!(program_of_pieces(&stock), None);
        assert_eq!(program_of_stock(&made), None);
        // a prefix cut right after the HASH part still answers
        let end = stock.windows(4).position(|w| w == b"HASH").unwrap() + 8 + 20;
        assert_eq!(program_of_stock(&stock[..end]), Some(digest));
        assert_eq!(program_of_stock(&stock[..end - 1]), None);
        // another program has another key, and its two containers still match
        let mut other = hash();
        other[10] ^= 0xff;
        let other_digest: ProgramHash = other[4..20].try_into().unwrap();
        assert_ne!(other_digest, digest);
        assert_eq!(program_of_pieces(&made_of(other)), Some(other_digest));
        assert_eq!(program_of_stock(&stock_of(&other_digest)), Some(other_digest));
        assert_ne!(program_of_pieces(&made_of(other)), program_of_pieces(&made));
        // a metadata piece that does not end in the hash record has no key
        let code = code(4);
        let meta = metadata(COMPUTE_METADATA, code.len(), &[declaration(0, 2, 1)]);
        let bare = fill(
            compute(&compute_packet(7), code.len(), meta.len()).unwrap(),
            &code,
            &meta,
        );
        assert_eq!(program_of_pieces(&bare), None);
        // no cut of either container may panic
        for n in 0..made.len() {
            let _ = program_of_pieces(&made[..n]);
            let _ = program_of_stock(&made[..n]);
        }
        for n in 0..stock.len() {
            let _ = program_of_pieces(&stock[..n]);
            let _ = program_of_stock(&stock[..n]);
        }
    }

    #[test]
    fn a_graphics_stage_gives_the_same_key() {
        let packet = graphics_packet(false, 0, &[]);
        let g = Graphics::parse(&packet, &state_piece(&[]), false).unwrap();
        let code = code(11);
        let meta = metadata(GRAPHICS_METADATA, code.len(), &[hash()]);
        let vs = fill(g.vertex(code.len(), meta.len()).unwrap(), &code, &meta);
        let digest: ProgramHash = hash()[4..20].try_into().unwrap();
        assert_eq!(program_of_pieces(&vs), Some(digest));
    }

    #[test]
    fn a_pipeline_without_a_pixel_stage() {
        let packet = graphics_packet(false, 0, &[]);
        let g = Graphics::parse(&packet, &state_piece(&[]), false).unwrap();
        assert!(!g.has_pixel() && g.elements.is_empty());
        assert!(g.pixel(44, 0x30).is_err());
        // Its vertex program exports the position alone.
        let code = code(11);
        let meta = metadata(GRAPHICS_METADATA, code.len(), &[hash()]);
        let vs = checked(&fill(
            g.vertex(code.len(), meta.len()).unwrap(),
            &code,
            &meta,
        ))
        .unwrap();
        let StageIo::Vs { outputs, fetch, .. } = &vs.io else {
            panic!("{:?}", vs.io)
        };
        assert!(outputs.is_empty() && fetch.is_empty());
        // The description and the packet must agree on the pixel stage.
        assert!(Graphics::parse(&packet, &state_piece(&[]), true).is_err());
        let with = graphics_packet(true, 1, &[(0, false)]);
        assert!(Graphics::parse(&with, &state_piece(&[]), false).is_err());
    }

    #[test]
    fn graphics_packets_not_understood_are_refused() {
        let state = state_piece(&ELEMENTS);
        let good = graphics_packet(true, 2, &[(0, false), (1, false)]);
        assert!(Graphics::parse(&good, &state, true).is_ok());
        let refused = |change: &dyn Fn(&mut Vec<u8>)| {
            let mut p = good.clone();
            change(&mut p);
            Graphics::parse(&p, &state, true).is_err()
        };
        assert!(refused(&|p| p.truncate(0x100)), "truncated");
        assert!(
            refused(&|p| p.truncate(p.len() - 4)),
            "sizes that do not add up"
        );
        assert!(refused(&|p| p.resize(MAX_PACKET + 4, 0)), "absurd length");
        assert!(refused(&|p| p[G_FIXED_SIZE] = 0x20), "another fixed block");
        assert!(
            refused(&|p| put(p, G_PS_IN_CONTROL, 9)),
            "more interpolants than controls"
        );
        assert!(
            refused(&|p| put(p, G_PS_IN_CONTROL, 2 | 1 << 14)),
            "BC_OPTIMIZE_DISABLE"
        );
        assert!(
            refused(&|p| put(p, G_PS_IN_CONTROL, 2 | 1 << 16)),
            "a bit above PS_W32_EN"
        );
        assert!(
            refused(&|p| put(p, G_INPUT_CNTL, 5)),
            "a parameter not exported"
        );
        assert!(
            refused(&|p| put(p, G_INPUT_CNTL, 1 << 19)),
            "16-bit interpolation"
        );
        assert!(
            refused(&|p| put(p, G_INPUT_CNTL + 4, 1 << 10)),
            "one parameter, two types"
        );
        assert!(
            refused(&|p| put(p, G_COL_FORMAT, 5)),
            "a 16-bit integer export"
        );
        assert!(
            refused(&|p| put(p, G_DB_SHADER_CONTROL, 0x110)),
            "a coverage export"
        );
        for bit in [9, 10, 16] {
            assert!(
                refused(&|p| put(p, G_DB_SHADER_CONTROL, 1 << bit)),
                "DB_SHADER_CONTROL bit {bit}"
            );
        }
        assert!(
            refused(&|p| put(p, G_DB_SHADER_CONTROL, 0x11)),
            "depth export without its format"
        );
        assert!(refused(&|p| put(p, G_POS_FORMAT, 0x44)), "clip distances");
        assert!(
            refused(&|p| put(p, G_VS_OUT_CONFIG, 0x100)),
            "unknown output bits"
        );
        assert!(refused(&|p| put(p, G_DEPTH_BIAS + 4, 1)), "depth bias");
    }

    /// The reason a packet is refused for, with the state piece `state`.
    fn why(packet: &[u8], state: &[u8]) -> Refusal {
        Graphics::parse(packet, state, true).unwrap_err()
    }

    #[test]
    fn a_pixel_program_in_waves_of_32() {
        let mut packet = graphics_packet(true, 2, &[(0, false), (1, false)]);
        put(&mut packet, G_PS_IN_CONTROL, 2 | PS_W32_EN);
        let g = Graphics::parse(&packet, &state_piece(&ELEMENTS), true).unwrap();
        let code = code(11);
        let meta = metadata(GRAPHICS_METADATA, code.len(), &[hash()]);
        let ps = checked(&fill(
            g.pixel(code.len(), meta.len()).unwrap(),
            &code,
            &meta,
        ))
        .unwrap();
        // 4 granules of 8 where the waves have 32 lanes, not of 4.
        assert!(!ps.wave64);
        assert_eq!(ps.vgprs, 32);
        // The vertex program stays in waves of 64.
        let vs = checked(&fill(
            g.vertex(code.len(), meta.len()).unwrap(),
            &code,
            &meta,
        ))
        .unwrap();
        assert!(vs.wave64);
        // Only the pixel program carries the marker that says it was made from
        // pieces, and the marker owns no user SGPR.
        let marked = |p: &NativeProgram| {
            p.layout
                .iter()
                .filter(|e| e.root == RootType::Other(PIXEL_PIECES))
                .count()
        };
        assert_eq!(marked(&ps), 1);
        assert_eq!(marked(&vs), 0);
        assert!(ps.layout.iter().all(|e| e.root != RootType::Other(PIXEL_PIECES) || e.sgprs == 0));
    }

    #[test]
    fn a_fixed_point_position_brings_the_position_row() {
        // The pixel signature names the parameter, and the position beside it when
        // the fixed-point position (bit 15) is enabled: it is made from the position.
        let rows = |input_addr: u32| {
            let mut packet = graphics_packet(true, 1, &[(0, false)]);
            put(&mut packet, G_PS_INPUT_ADDR, input_addr);
            let g = Graphics::parse(&packet, &state_piece(&ELEMENTS), true).unwrap();
            let code = code(11);
            let meta = metadata(GRAPHICS_METADATA, code.len(), &[hash()]);
            let bytes = fill(g.pixel(code.len(), meta.len()).unwrap(), &code, &meta);
            let c = crate::Container::parse(&bytes).unwrap();
            u32_at(c.part(FourCC::ISG1).unwrap().data, 0).unwrap()
        };
        assert_eq!(rows(0x0080), 1);
        assert_eq!(rows(0x8080), 2);
    }

    #[test]
    fn a_pixel_input_that_is_a_constant() {
        // Input 0 reads parameter 1, input 1 is the constant (0, 0, 0, 1),
        // input 2 the constant (1, 1, 1, 1).
        let mut packet = graphics_packet(true, 2, &[(1, false), (0, false), (0, false)]);
        put(&mut packet, G_INPUT_CNTL + 4, CONSTANT_OFFSET | 1 << 8);
        put(&mut packet, G_INPUT_CNTL + 8, CONSTANT_OFFSET | 3 << 8);
        let g = Graphics::parse(&packet, &state_piece(&ELEMENTS), true).unwrap();
        let code = code(11);
        let meta = metadata(GRAPHICS_METADATA, code.len(), &[hash()]);
        let ps = checked(&fill(
            g.pixel(code.len(), meta.len()).unwrap(),
            &code,
            &meta,
        ))
        .unwrap();
        let StageIo::Ps { inputs, .. } = &ps.io else {
            panic!("{:?}", ps.io)
        };
        assert_eq!(
            inputs.iter().map(|i| (i.attr, i.row, i.flat)).collect::<Vec<_>>(),
            [
                (0, 1, false),
                (1, PsInput::constant_row(1), false),
                (2, PsInput::constant_row(3), false)
            ]
        );
        // The pixel stage's signature names parameter 1 alone.
        let bytes = fill(g.pixel(code.len(), meta.len()).unwrap(), &code, &meta);
        let c = crate::Container::parse(&bytes).unwrap();
        assert_eq!(u32_at(c.part(FourCC::ISG1).unwrap().data, 0), Ok(1));
        // A constant with another field set, or flat, is refused by name.
        for (bit, reason) in [
            (1u32 << 10, "SPI_PS_INPUT_CNTL FLAT_SHADE (bit 10) with the constant offset"),
            (1 << 19, "SPI_PS_INPUT_CNTL FP16_INTERP_MODE (bit 19)"),
            (1 << 12, "SPI_PS_INPUT_CNTL PRIM_ATTR (bit 12)"),
        ] {
            let mut p = packet.clone();
            put(&mut p, G_INPUT_CNTL + 4, CONSTANT_OFFSET | bit);
            assert_eq!(why(&p, &state_piece(&ELEMENTS)), reason);
        }
        // An offset past the constant one is no parameter.
        let mut p = packet.clone();
        put(&mut p, G_INPUT_CNTL + 4, 0x21);
        assert_eq!(
            why(&p, &state_piece(&ELEMENTS)),
            "SPI_PS_INPUT_CNTL OFFSET (bits 0-5) of 0x21 or more"
        );
    }

    #[test]
    fn integer_targets_take_the_integer_exports() {
        // R8G8B8A8_UINT (30) and R8G8B8A8_SINT (32).
        let state = |format: u8| {
            let mut s = state_piece(&ELEMENTS);
            s[S_TARGETS] = format;
            s
        };
        let packet = |col: u32| {
            let mut p = graphics_packet(true, 1, &[(0, false)]);
            put(&mut p, G_COL_FORMAT, col);
            p
        };
        // The 16-bit float export is for float targets alone.
        assert!(Graphics::parse(&packet(4), &state(28), true).is_ok());
        assert_eq!(
            why(&packet(4), &state(30)),
            "SPI_SHADER_COL_FORMAT FP16_ABGR (4) for an integer render target"
        );
        // The 16-bit integer exports go with the signedness of the target.
        assert!(Graphics::parse(&packet(7), &state(30), true).is_ok());
        assert!(Graphics::parse(&packet(8), &state(32), true).is_ok());
        assert!(Graphics::parse(&packet(7), &state(32), true).is_err());
        assert!(Graphics::parse(&packet(8), &state(30), true).is_err());
        assert!(Graphics::parse(&packet(7), &state(28), true).is_err());
        // The 32-bit export of an unsigned target declares an unsigned
        // output; the 16-bit ones declare 16-bit integers.
        let code = code(11);
        let meta = metadata(GRAPHICS_METADATA, code.len(), &[hash()]);
        let component = |col: u32, format: u8| {
            let g = Graphics::parse(&packet(col), &state(format), true).unwrap();
            let bytes = fill(g.pixel(code.len(), meta.len()).unwrap(), &code, &meta);
            let c = crate::Container::parse(&bytes).unwrap();
            u32_at(c.part(FourCC::OSG1).unwrap().data, 8 + 16).unwrap()
        };
        assert_eq!(component(9, 30), 1);
        assert_eq!(component(9, 32), 2);
        assert_eq!(component(9, 28), 3);
        assert_eq!(component(7, 30), 4);
        assert_eq!(component(8, 32), 5);
        assert_eq!(component(4, 28), 6);
        // The formats that have no host form are refused by name.
        assert_eq!(
            why(&packet(5), &state(28)),
            "SPI_SHADER_COL_FORMAT UNORM16_ABGR (5)"
        );
        assert_eq!(
            why(&packet(6), &state(28)),
            "SPI_SHADER_COL_FORMAT SNORM16_ABGR (6)"
        );
        assert_eq!(
            why(&packet(10), &state(28)),
            "SPI_SHADER_COL_FORMAT of 10 or more, which has no format"
        );
    }

    #[test]
    fn clip_distance_registers_are_read_by_their_fields() {
        let vec0 = CL_CCDIST0_VEC;
        let vec1 = CL_CCDIST1_VEC;
        // None, then up to four in the first vector, then up to eight.
        assert_eq!(clip_distances(4, 0), Ok(0));
        assert_eq!(clip_distances(0x44, 1 | vec0), Ok(1));
        assert_eq!(clip_distances(0x44, 0xf | vec0), Ok(4));
        assert_eq!(clip_distances(0x444, 0x1f | vec0 | vec1), Ok(5));
        assert_eq!(clip_distances(0x444, 0xff | vec0 | vec1), Ok(8));
        // The registers must agree with each other.
        let disagree = "SPI_SHADER_POS_FORMAT that disagrees with PA_CL_VS_OUT_CNTL";
        assert_eq!(clip_distances(0x44, 0), Err(disagree));
        assert_eq!(clip_distances(4, 1 | vec0), Err(disagree));
        assert_eq!(clip_distances(0x44, 0x1f | vec0 | vec1), Err(disagree));
        assert!(clip_distances(0x44, 1).unwrap_err().contains("CCDIST0_VEC_ENA"));
        assert!(clip_distances(0x44, vec0).unwrap_err().contains("CCDIST0_VEC_ENA"));
        assert!(clip_distances(0x44, 0x1f | vec0).unwrap_err().contains("CCDIST0_VEC_ENA"));
        // A gap in the enables is not a count.
        assert!(clip_distances(0x44, 0b101 | vec0).unwrap_err().contains("count up"));
        // A position format that is not four components.
        assert!(clip_distances(3, 0).unwrap_err().contains("POS0_EXPORT_FORMAT"));
        assert!(clip_distances(0, 0).unwrap_err().contains("POS0_EXPORT_FORMAT"));
        // What is not settled is refused by its field.
        let named = |bits: u32, field: &str| {
            let why = clip_distances(0x44, 1 | vec0 | bits).unwrap_err();
            assert!(why.contains(field), "{bits:#x}: {why}");
        };
        named(1 << 8, "CULL_DIST_ENA");
        named(1 << 15, "CULL_DIST_ENA");
        named(1 << 16, "USE_VTX_POINT_SIZE");
        named(1 << 19, "VIEWPORT_INDX");
        named(1 << 21, "VS_OUT_MISC_VEC_ENA");
        named(1 << 24, "VS_OUT_MISC_SIDE_BUS_ENA");
        named(1 << 25, "above bit 24");
        named(1 << 31, "above bit 24");
        // The one value captured: a clip and a cull distance 0, a vector
        // and the side bus.
        let seen = 0x0140_0101;
        assert!(clip_distances(0x44, seen).unwrap_err().contains("CULL_DIST_ENA"));
    }

    #[test]
    fn clip_distances_reach_the_vertex_signature() {
        // One parameter and five distances.
        let mut packet = graphics_packet(true, 1, &[(0, false)]);
        put(&mut packet, G_POS_FORMAT, 0x444);
        put(&mut packet, G_VS_OUT_CNTL, 0x1f | CL_CCDIST0_VEC | CL_CCDIST1_VEC);
        let g = Graphics::parse(&packet, &state_piece(&ELEMENTS), true).unwrap();
        let code = code(11);
        let meta = metadata(GRAPHICS_METADATA, code.len(), &[hash()]);
        let bytes = fill(g.vertex(code.len(), meta.len()).unwrap(), &code, &meta);
        let c = crate::Container::parse(&bytes).unwrap();
        let part = c.part(FourCC::OSG1).unwrap().data;
        let count = u32_at(part, 0).unwrap() as usize;
        let rows: Vec<(u32, u32, u32, u8)> = (0..count)
            .map(|i| {
                let at = 8 + 32 * i;
                let word = |k: usize| u32_at(part, at + 4 * k).unwrap();
                (word(2), word(3), word(5), part[at + 24])
            })
            .collect();
        // (index, system value, register, mask): the parameter, the
        // position (register 1), then the distances after it.
        assert_eq!(
            rows,
            [(0, 0, 0, 0xf), (0, 1, 1, 0xf), (0, 2, 2, 0xf), (1, 2, 3, 0x1)]
        );
        // None without them.
        let plain = Graphics::parse(
            &graphics_packet(true, 1, &[(0, false)]),
            &state_piece(&ELEMENTS),
            true,
        )
        .unwrap();
        let bytes = fill(plain.vertex(code.len(), meta.len()).unwrap(), &code, &meta);
        let c = crate::Container::parse(&bytes).unwrap();
        assert_eq!(u32_at(c.part(FourCC::OSG1).unwrap().data, 0), Ok(2));
        // The pixel stage's inputs do not list them.
        let bytes = fill(g.pixel(code.len(), meta.len()).unwrap(), &code, &meta);
        let c = crate::Container::parse(&bytes).unwrap();
        assert_eq!(u32_at(c.part(FourCC::ISG1).unwrap().data, 0), Ok(1));
    }

    #[test]
    fn input_elements_with_values_are_refused_by_their_word() {
        let packet = graphics_packet(false, 0, &[]);
        let at = |word: usize| S_BLOCK + 17 + 4 * word;
        for (word, field) in [(0, "word 0"), (2, "word 2"), (4, "word 4"), (5, "word 5")] {
            let mut s = state_piece(&ELEMENTS);
            put(&mut s, at(word), 1);
            let reason = Graphics::parse(&packet, &s, false).unwrap_err();
            assert!(reason.contains(field), "{reason}");
        }
        // The format and the offset are read, wherever the others stand.
        let mut s = state_piece(&ELEMENTS);
        put(&mut s, at(1), 2);
        put(&mut s, at(3), 8);
        let g = Graphics::parse(&packet, &s, false).unwrap();
        assert_eq!((g.elements[0].format, g.elements[0].offset), (2, 8));
    }

    #[test]
    fn a_description_shows_the_element_words() {
        let packet = graphics_packet(false, 0, &[]);
        let mut s = state_piece(&ELEMENTS);
        put(&mut s, S_BLOCK + 17 + 8, 7);
        let text = Graphics::describe(&packet, &s);
        assert!(text.contains("element=[0, 6, 7, 0, 0, 0]"), "{text}");
        assert_eq!(text.matches("element=").count(), 3);
        // Cut pieces say what is there and nothing more.
        let _ = Graphics::describe(&packet[..0x20], &s[..S_BLOCK + 9]);
        let _ = Graphics::describe(&[], &[]);
    }

    #[test]
    fn state_pieces_not_understood_are_refused() {
        let packet = graphics_packet(false, 0, &[]);
        let good = state_piece(&ELEMENTS);
        let refused = |change: &dyn Fn(&mut Vec<u8>)| {
            let mut s = good.clone();
            change(&mut s);
            Graphics::parse(&packet, &s, false).is_err()
        };
        assert!(!refused(&|_| ()));
        assert!(refused(&|s| s.truncate(S_BLOCK)), "no layout block");
        assert!(refused(&|s| s.truncate(s.len() - 4)), "a cut layout");
        assert!(refused(&|s| s.resize(MAX_STATE + 4, 0)), "absurd length");
        assert!(refused(&|s| put(s, S_BLOCK, 3000)), "absurd element count");
        assert!(refused(&|s| put(s, S_BLOCK, 4)), "more elements than bytes");
        assert!(
            refused(&|s| put(s, S_BLOCK + 4, 4000)),
            "a name past the piece"
        );
        assert!(
            refused(&|s| s[S_BLOCK + 8] = b' '),
            "a name that is no semantic"
        );
        assert!(
            refused(&|s| s[S_BLOCK + 8 + 7] = b'1'),
            "a name with its index in it"
        );
        assert!(refused(&|s| s[S_COUNT] = 9), "nine targets");
        assert!(
            refused(&|s| s[S_TARGETS + 3] = 28),
            "a target past the count"
        );
        assert!(refused(&|s| s[S_TOPOLOGY] = 2), "lines");
        assert!(refused(&|s| put(s, S_SAMPLES, 4)), "multisampling");
        // The element's first word after its name (index, slot, class or
        // step rate: all must be zero).
        assert!(refused(&|s| put(s, S_BLOCK + 8 + 9, 1)));
        // An empty layout is one.
        assert!(Graphics::parse(&packet, &state_piece(&[]), false).is_ok());
    }

    /// Every description captured by `VICTORY_PIPELINE_DUMP` in the folder
    /// that variable names, through the parsers above and the translator.
    /// Skipped when it is not set: the files are a title's, and stay
    /// outside the repository. Prints counts and each distinct failure.
    /// Where `dxil.dll` loads, each translation is validated; where the
    /// software adapter opens, each whole pipeline is made on it (which
    /// links the two stages' signatures); the card is not touched. All of
    /// it twice: as on a host of 64-lane waves and as on one of 32.
    #[test]
    fn captured_descriptions_parse_and_translate() {
        use std::collections::BTreeMap;
        let Some(folder) = std::env::var_os("VICTORY_PIPELINE_DUMP").map(std::path::PathBuf::from)
        else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(&folder) else {
            panic!(
                "VICTORY_PIPELINE_DUMP names no folder: {}",
                folder.display()
            );
        };
        // The files of each call, by "<slot>-<call>".
        let mut calls: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for name in entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
        {
            if let Some(call) = name.get(..7).filter(|c| c.as_bytes()[3] == b'-') {
                calls.entry(call.to_owned()).or_default().push(name);
            }
        }
        // A description rebuilt from its files: the words, and each pair's
        // bytes where the dump has them (a blob whole, or the 256 bytes
        // taken at a word that had no length beside it).
        struct Call {
            words: Vec<u64>,
            pieces: BTreeMap<usize, Vec<u8>>,
        }
        let load = |call: &str, names: &[String]| -> Option<Call> {
            let read = |name: &str| std::fs::read(folder.join(name)).ok();
            let desc = read(&format!("{call}-desc.bin"))?;
            let words: Vec<u64> = desc
                .as_chunks::<8>()
                .0
                .iter()
                .map(|c| u64::from_le_bytes(*c))
                .collect();
            let mut pieces = BTreeMap::new();
            for name in names {
                let rest = &name[8..];
                if let Some(n) = rest
                    .strip_prefix("blob")
                    .and_then(|r| r.get(..2)?.parse::<usize>().ok())
                {
                    // Pair n is description words 1 + 2n and 2 + 2n.
                    pieces.insert(1 + 2 * n, read(name)?);
                } else if let Some(n) = rest
                    .strip_prefix("word")
                    .and_then(|r| r.get(..2)?.parse::<usize>().ok())
                {
                    pieces.entry(n).or_insert(read(name)?);
                }
            }
            Some(Call { words, pieces })
        };
        #[derive(Default)]
        struct Tally {
            calls: u32,
            parsed: u32,
            programs: u32,
            translated: u32,
            valid: u32,
            pipelines: u32,
            made: u32,
            why: BTreeMap<String, u32>,
        }
        #[cfg(windows)]
        let mut validator = {
            use crate::emit::validator::Validator;
            Validator::load(Validator::default_path().as_deref()).ok()
        };
        #[cfg(windows)]
        let host = crate::emit::host::Host::new(true).ok();
        #[cfg(windows)]
        println!(
            "validator: {}; software adapter: {}",
            if validator.is_some() {
                "loaded"
            } else {
                "not found"
            },
            if host.is_some() {
                "opened"
            } else {
                "not opened"
            }
        );
        for lanes in [64u32, 32] {
            crate::emit::with_test_host_wave(lanes, || {
                println!("as on a host of {lanes}-lane waves:");
                let mut tallies: BTreeMap<&str, Tally> = BTreeMap::new();
                for (call, names) in &calls {
                    let method = if call.starts_with("218") {
                        "0x218"
                    } else {
                        "0x1c8"
                    };
                    let t = tallies.entry(method).or_default();
                    t.calls += 1;
                    let note = |t: &mut Tally, why: String| *t.why.entry(why).or_default() += 1;
                    let Some(c) = load(call, names) else {
                        note(t, "the dump has no description".into());
                        continue;
                    };
                    // The piece at description word `at`, cut to the length the
                    // description gives it; none if the dump has fewer bytes.
                    let piece = |at: usize| -> Option<&[u8]> {
                        let len = *c.words.get(at + 1)? as usize;
                        c.pieces.get(&at)?.get(..len)
                    };
                    // One stage: its draft filled from the dump, then translated.
                    let mut stage = |t: &mut Tally,
                                     what: &str,
                                     draft: Result<Draft, Refusal>,
                                     at: usize| {
                        let draft = match draft {
                            Ok(d) => d,
                            Err(why) => {
                                note(t, format!("{what} refused: {why}"));
                                return None;
                            }
                        };
                        let (Some(code), Some(meta)) = (piece(at), piece(at + 2)) else {
                            note(t, format!("{what}: the dump does not hold the whole code"));
                            return None;
                        };
                        let bytes = fill(draft, code, meta);
                        if let Err(e) = checked(&bytes) {
                            note(t, format!("{what} does not parse: {e}"));
                            return None;
                        }
                        t.programs += 1;
                        match crate::emit::translate(&bytes) {
                            Ok(shader) => {
                                t.translated += 1;
                                #[cfg(windows)]
                                if let Some(v) = &mut validator {
                                    match v.validate(&mut shader.dxil.clone()) {
                                        Ok(()) => t.valid += 1,
                                        Err(e) => {
                                            let first = e.lines().next().unwrap_or("").to_owned();
                                            note(t, format!("{what} does not validate: {first}"));
                                        }
                                    }
                                }
                                Some(shader)
                            }
                            Err(e) => {
                                // (without the offset, so that one cause is one line)
                                let why = match e {
                                    ShaderError::Unsupported { what, .. } => what.to_owned(),
                                    e => e.to_string(),
                                };
                                note(t, format!("{what} does not translate: {why}"));
                                None
                            }
                        }
                    };
                    let len = |at: usize| c.words.get(at + 1).map_or(0, |&n| n as usize);
                    if method == "0x218" {
                        let Some(packet) = piece(1) else {
                            note(t, "the dump has no packet".into());
                            continue;
                        };
                        t.parsed += 1;
                        if let Some(cs) = stage(t, "compute", compute(packet, len(5), len(7)), 5) {
                            t.pipelines += 1;
                            #[cfg(windows)]
                            if let Some(host) = &host {
                                match host.compute_pipeline(&cs) {
                                    Ok(_) => t.made += 1,
                                    Err(e) => note(
                                        t,
                                        format!(
                                            "compute pipeline refused by the software adapter: {e}"
                                        ),
                                    ),
                                }
                            }
                            let _ = cs;
                        }
                        continue;
                    }
                    let (Some(packet), Some(state)) = (piece(1), piece(3)) else {
                        note(t, "the dump has no packet or state piece".into());
                        continue;
                    };
                    let pixel = c.words.get(5).is_some_and(|&w| w != 0);
                    let g = match Graphics::parse(packet, state, pixel) {
                        Ok(g) => g,
                        Err(why) => {
                            note(t, format!("pipeline refused: {why}"));
                            continue;
                        }
                    };
                    t.parsed += 1;
                    let vs = stage(t, "vertex", g.vertex(len(9), len(11)), 9);
                    let ps = if pixel {
                        stage(t, "pixel", g.pixel(len(5), len(7)), 5)
                    } else {
                        None
                    };
                    let (Some(vs), true) = (vs, ps.is_some() || !pixel) else {
                        continue;
                    };
                    t.pipelines += 1;
                    // With both stages: a pipeline on the software adapter, with
                    // the vertex program's own inputs as the input layout.
                    #[cfg(windows)]
                    if let (Some(host), Some(ps)) = (&host, &ps) {
                        match host.pipeline(&vs, ps) {
                            Ok(_) => t.made += 1,
                            Err(e) => note(
                                t,
                                format!("graphics pipeline refused by the software adapter: {e}"),
                            ),
                        }
                    }
                    let _ = vs;
                }
                for (method, t) in &tallies {
                    println!(
                        "{method}: {} calls, {} descriptions parsed, {} programs built, {} translated ({} valid), {} pipelines whole ({} made on the software adapter)",
                        t.calls, t.parsed, t.programs, t.translated, t.valid, t.pipelines, t.made
                    );
                    for (why, n) in &t.why {
                        println!("    {n:3} x {why}");
                    }
                }
                assert!(
                    !tallies.is_empty(),
                    "no captured description in {}",
                    folder.display()
                );
            });
        }
    }

    /// The layout of a compute program with `user` user SGPRs and `records`.
    fn compute_layout(user: u32, records: &[[u8; 32]]) -> Vec<(RootType, u8, u32)> {
        let code = code(1);
        let mut all = records.to_vec();
        all.push(hash());
        let meta = metadata(COMPUTE_METADATA, code.len(), &all);
        let bytes = fill(
            compute(&compute_packet(user), code.len(), meta.len()).unwrap(),
            &code,
            &meta,
        );
        let p = checked(&bytes).unwrap();
        p.layout
            .iter()
            .map(|q| (q.root, q.user_data, q.sgprs))
            .collect()
    }

    #[test]
    fn a_heap_view_gets_the_last_user_sgpr() {
        // Tables in s4 and s5, constants from s6 up to the heap in s7.
        let records = [
            resource(0, 3, 0, 7, 4, 8, 4, 0),
            resource(0, 3, 0, 8, 4, 8, 5, 1),
            resource(2, 0, 0, 9, 0, 4, 6, 2),
            declared(2, 0, 2, 0),
        ];
        let roots = compute_layout(8, &records);
        assert!(roots.contains(&(RootType::ResourceHeap, 7, 1)), "{roots:?}");
        assert!(roots.contains(&(RootType::Constants, 6, 1)), "{roots:?}");
        // Without the record the constants run to the end, as they did.
        let without = compute_layout(8, &records[..3]);
        assert!(without.contains(&(RootType::Constants, 6, 2)), "{without:?}");
        assert!(without.iter().all(|r| r.0 != RootType::ResourceHeap));
        // A parameter at the heap's place leaves no heap entry, and no refusal.
        let taken = compute_layout(8, &[resource(0, 3, 0, 7, 4, 8, 7, 0), declared(2, 0, 2, 0)]);
        assert!(taken.iter().all(|r| r.0 != RootType::ResourceHeap), "{taken:?}");
    }

    #[test]
    fn a_heap_view_sits_before_a_heap_sampler_and_after_vertex_buffers() {
        let g = Graphics::parse(
            &graphics_packet(true, 3, &[(0, false)]),
            &state_piece(&ELEMENTS),
            true,
        )
        .unwrap();
        let code = code(11);
        // The pixel stage has 10 user SGPRs: the sampler heap takes s9.
        let both = metadata(
            GRAPHICS_METADATA,
            code.len(),
            &[
                resource(0, 3, 0, 1, 4, 8, 4, 0),
                declared(2, 0, 2, 0),
                declaration(3, 14, 0),
                hash(),
            ],
        );
        let ps = checked(&fill(g.pixel(code.len(), both.len()).unwrap(), &code, &both)).unwrap();
        let roots: Vec<_> = ps.layout.iter().map(|p| (p.root, p.user_data)).collect();
        assert!(roots.contains(&(RootType::ResourceHeap, 8)), "{roots:?}");
        assert!(roots.contains(&(RootType::SamplerHeap, 9)), "{roots:?}");
        // The vertex buffer entry stays where it was.
        let alone = metadata(
            GRAPHICS_METADATA,
            code.len(),
            &[declared(2, 0, 2, 0), hash()],
        );
        let vs = checked(&fill(g.vertex(code.len(), alone.len()).unwrap(), &code, &alone)).unwrap();
        let roots: Vec<_> = vs
            .layout
            .iter()
            .map(|p| (p.root, p.user_data, p.sgprs))
            .collect();
        assert!(roots.contains(&(RootType::ResourceHeap, 9, 1)), "{roots:?}");
        assert!(
            roots.contains(&(RootType::Other(VERTEX_BUFFERS), 0, 2)),
            "{roots:?}"
        );
    }

    /// A compute program of 8 user SGPRs that takes a descriptor from the
    /// heap through `s_load_dwordx4 s[20:23], s[16:17], s9`, with the heap's
    /// base copied from s6 and the index (constants) from s5, then runs
    /// `extra`, with `records` as its tables and constants.
    fn heap_program(extra: &[u32], records: &[[u8; 32]]) -> (NativeProgram, crate::ir::Program) {
        let mut words = vec![
            0xBE90_0306, // s_mov_b32 s16, s6
            0xBE91_0380, // s_mov_b32 s17, 0
            0x8F09_8505, // s_lshl_b32 s9, s5, 5
            0xF408_0508,
            0x1200_0000, // s_load_dwordx4 s[20:23], s[16:17], s9
        ];
        words.extend_from_slice(extra);
        words.push(END);
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let mut all = records.to_vec();
        all.push(hash());
        let meta = metadata(COMPUTE_METADATA, code.len(), &all);
        let bytes = fill(
            compute(&compute_packet(8), code.len(), meta.len()).unwrap(),
            &code,
            &meta,
        );
        let native = checked(&bytes).unwrap();
        let p = crate::ir::build(&native.code, false, 256).unwrap();
        (native, p)
    }

    #[test]
    fn the_heap_moves_down_when_the_last_sgpr_is_never_read() {
        // Constants from s5 up to the heap guessed at s7.
        let records = [
            resource(0, 3, 0, 7, 4, 8, 4, 0),
            resource(2, 0, 0, 9, 0, 4, 5, 1),
            declared(2, 0, 2, 0),
        ];
        let (mut native, p) = heap_program(&[], &records);
        place_heap(&mut native, &p);
        let roots: Vec<_> = native
            .layout
            .iter()
            .map(|q| (q.root, q.user_data, q.sgprs))
            .collect();
        assert!(roots.contains(&(RootType::ResourceHeap, 6, 1)), "{roots:?}");
        assert!(roots.contains(&(RootType::Constants, 5, 1)), "{roots:?}");
        // The load's base now traces to the heap, with the index computed.
        let heap = native
            .layout
            .iter()
            .position(|q| q.root == RootType::ResourceHeap)
            .unwrap();
        let r = crate::resource::track(&p, &native.layout, native.stage, None).unwrap();
        assert_eq!(r.scalar[0].1.param, heap);
        assert!(r.scalar[0].1.dynamic.is_some());
        // The last SGPR read too: it is the heap, nothing moves.
        let (mut native, p) = heap_program(&[0xBE92_0307], &records); // s_mov_b32 s18, s7
        let before = native.layout.clone();
        place_heap(&mut native, &p);
        assert_eq!(native.layout, before);
        // A table holds s6: nothing moves.
        let held = [
            resource(0, 3, 0, 7, 4, 8, 4, 0),
            resource(0, 3, 0, 8, 4, 8, 6, 1),
            resource(2, 0, 0, 9, 0, 4, 5, 2),
            declared(2, 0, 2, 0),
        ];
        let (mut native, p) = heap_program(&[], &held);
        let before = native.layout.clone();
        place_heap(&mut native, &p);
        assert_eq!(native.layout, before);
    }

    #[test]
    fn the_heap_stays_when_neither_sgpr_is_read() {
        let records = [resource(2, 0, 0, 9, 0, 4, 5, 1), declared(2, 0, 2, 0)];
        let (mut native, _) = heap_program(&[], &records);
        // the same layout against code that reads no user SGPR
        let end = END.to_le_bytes();
        let none = crate::ir::build(&end, false, 256).unwrap();
        let before = native.layout.clone();
        place_heap(&mut native, &none);
        assert_eq!(native.layout, before);
    }
}
