//! Pipelines the title makes from loose pieces: device methods 0x218
//! (compute) and 0x1c8 (graphics). Their descriptions name a root
//! signature of ours and then pieces that lie apart in title memory, each
//! as an address and a length (`victory_shader::pieces` has the layouts of
//! the pieces themselves, and how they were worked out):
//!
//! ```text
//! compute, 0x48 bytes           graphics, 0x108 bytes
//! +0x00 root signature          +0x00 root signature
//! +0x08 packet (0x1c bytes)     +0x08 packet
//! +0x18 0x14 bytes, not read    +0x18 state piece
//! +0x28 code                    +0x28 pixel code, +0x38 pixel metadata
//! +0x38 metadata                +0x48 vertex code, +0x58 vertex metadata
//!                               +0x68 20 words, zero in every call seen
//! ```
//!
//! Where the bytes are: all of it is in title memory when the method is
//! called, and stays the title's. The packet and the state piece (under
//! 1 KiB together) are read to the stack, parsed and dropped. Each stage's
//! code and metadata are read once, straight into the container that
//! [`Shader`] then keeps, and from there on the pipeline is one like any
//! other: [`hand_out`] makes the object, translation and the host pipeline
//! wait for the workers. Nothing here goes to the card.
//!
//! Which thread: the title's, during its loading (some 8,000 of these
//! calls arrive within a few milliseconds), so a call reads, hashes and
//! returns. It does not decode the code.

use std::borrow::Cow;
use std::ffi::CString;
use std::sync::Arc;

use victory_shader::ShaderError;
use victory_shader::pieces::{self, Draft, Graphics, Refusal, Registers};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::PCSTR;

use super::host::GraphicsState;
use super::{Shader, hand_out, shader_of};
use crate::object::{HRESULT, Iid, This};

/// Whether `VICTORY_PIPELINE_PIECES=1` is set: the switch that makes
/// pipelines from the pieces of the console's two creation methods. Off by
/// default. It is read here once so that `device::unknown_pipeline` and
/// `pipeline::use_pipeline_cache` see the same answer; the latter starts
/// the index of the pipeline book (`known::prime`) that lets a pipeline
/// from pieces match the stock hash lists.
pub(crate) fn asked() -> bool {
    static ASKED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ASKED.get_or_init(|| std::env::var("VICTORY_PIPELINE_PIECES").is_ok_and(|v| v == "1"))
}

/// A piece of a description: where it is in title memory and how long.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Pair {
    address: u64,
    len: u64,
}

/// The description method 0x218 takes. What follows it in title memory is
/// the title's own (the links and key of the list node that holds the next
/// description), so no more than this is read.
#[repr(C)]
#[derive(Default)]
struct ComputeDesc {
    root: u64,
    packet: Pair,
    /// 0x14 bytes of pipeline metadata nothing here needs; another length
    /// would be another format.
    pipeline: Pair,
    code: Pair,
    metadata: Pair,
}
const _: () = assert!(size_of::<ComputeDesc>() == 0x48);

/// The description method 0x1c8 takes: the pixel stage before the vertex
/// stage (all four of its words zero where the pipeline has none), then
/// words that were zero in every call seen. They would be more stages, so
/// a description with any of them set is refused rather than drawn with
/// two stages of three.
#[repr(C)]
#[derive(Default)]
struct GraphicsDesc {
    root: u64,
    packet: Pair,
    state: Pair,
    pixel_code: Pair,
    pixel_metadata: Pair,
    vertex_code: Pair,
    vertex_metadata: Pair,
    rest: [u64; 20],
}
const _: () = assert!(size_of::<GraphicsDesc>() == 0x108);

/// A description read where the title has it; `None` if fewer than `least`
/// of its bytes are there. `T` is one of the two above: integers only.
fn described<T: Default>(desc: *const u8, least: usize) -> Option<T> {
    let mut d = T::default();
    // SAFETY: the description structures are integers without padding, so
    // any bytes are a value of theirs; the slice is exactly `d`.
    let bytes =
        unsafe { std::slice::from_raw_parts_mut((&raw mut d).cast::<u8>(), size_of::<T>()) };
    (!desc.is_null() && crate::host::read_there(desc, bytes) >= least).then_some(d)
}

/// A piece's length, if it is one a piece of its kind can have.
fn len(piece: Pair, most: usize) -> Result<usize, Refusal> {
    // (nothing of the title's lies in the first 64 KiB)
    if piece.address < 0x1_0000 || piece.len == 0 || piece.len > most as u64 {
        return Err("a piece with no address or an implausible length");
    }
    Ok(piece.len as usize)
}

/// Reads all of a piece into `to`, which has its length.
fn read(piece: Pair, to: &mut [u8]) -> Result<(), Refusal> {
    if crate::host::read_there(piece.address as *const u8, to) != to.len() {
        return Err("a piece that is not all there to read");
    }
    Ok(())
}

/// A stage's container, whole: the code and the metadata read from title
/// memory into their places in `draft`, and the two checked against each
/// other.
fn filled(mut draft: Draft, code: Pair, metadata: Pair) -> Result<Vec<u8>, Refusal> {
    read(code, &mut draft.bytes[draft.code.clone()])?;
    read(metadata, &mut draft.bytes[draft.metadata.clone()])?;
    pieces::check(&draft.bytes).map_err(|e| match e {
        ShaderError::Malformed(why) => why,
        ShaderError::Unsupported { what, .. } => what,
        _ => "pieces that do not parse",
    })?;
    Ok(draft.bytes)
}

/// The root signature a description names must be an object of ours. It is
/// not used further: as with every other pipeline, the root signature in
/// force at a draw is the command list's.
fn rooted(root: u64) -> Result<(), Refusal> {
    if crate::root::is_ours(root as This) {
        Ok(())
    } else {
        Err("a description whose first word is not a root signature of ours")
    }
}

/// The container of the compute program `d` describes.
fn compute_container(d: &ComputeDesc) -> Result<Vec<u8>, Refusal> {
    if d.packet.len != pieces::COMPUTE_PACKET as u64
        || d.pipeline.len != pieces::COMPUTE_PIPELINE_PIECE as u64
    {
        return Err("a compute description whose first pieces are not 0x1c and 0x14 bytes");
    }
    let mut packet = [0u8; pieces::COMPUTE_PACKET];
    len(d.packet, packet.len())?;
    read(d.packet, &mut packet)?;
    let draft = pieces::compute(
        &packet,
        len(d.code, pieces::MAX_CODE)?,
        len(d.metadata, pieces::MAX_METADATA)?,
    )?;
    filled(draft, d.code, d.metadata)
}

/// `VICTORY_PIPELINE_PIECES=1`, read once (device.rs makes the pipelines
/// from pieces under the same switch).
pub(crate) fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_PIPELINE_PIECES").is_ok_and(|v| v == "1"))
}

/// Whether `container` is one made from pieces (it has their part).
pub(crate) fn is_pieces(container: &[u8]) -> bool {
    victory_shader::Container::parse(container)
        .is_ok_and(|c| c.part(victory_shader::FourCC::PIECES).is_some())
}

/// The list of containers from pieces that did not translate, kept beside
/// the library. A stage that does not translate leaves its pipeline with no
/// host pipeline, and every draw of it is dropped; the title was told the
/// pipeline exists and never falls back. A pipeline refused at creation
/// answers as with the switch off, and the title goes without it.
const UNTRANSLATED: &str = "pieces-untranslated.bin";

/// `VICTORY_PIPELINE_PIECES_REFUSE_UNTRANSLATED=1`, read once, off by
/// default, and only with `VICTORY_PIPELINE_PIECES=1`: a stage that did not
/// translate in an earlier run of this translator is refused at creation,
/// and the ones that fail now are added to the list. The run that first
/// meets such a stage still drops its draws; the next one does not make it.
fn refusing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        enabled()
            && std::env::var("VICTORY_PIPELINE_PIECES_REFUSE_UNTRANSLATED").is_ok_and(|v| v == "1")
    })
}

/// The keys in the file `path` (32 bytes each, a torn end ignored); empty
/// if there is no such file.
fn load_untranslated(path: &std::path::Path) -> std::collections::HashSet<[u8; 32]> {
    let bytes = std::fs::read(path).unwrap_or_default();
    let (keys, _) = bytes.as_chunks::<32>();
    keys.iter().copied().collect()
}

/// Adds `key` to the file `path`. Only ever appended to, under a lock: the
/// workers that translate write from several threads.
fn append_untranslated(path: &std::path::Path, key: &[u8; 32]) {
    use std::io::Write;
    static WRITING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(key));
    if let Err(e) = written {
        tracing::warn!(error = %e, "the list of pieces that did not translate was not written");
    }
}

/// The keys known from earlier runs: empty unless refusing is on.
fn untranslated() -> &'static std::sync::RwLock<std::collections::HashSet<[u8; 32]>> {
    static KNOWN: std::sync::OnceLock<std::sync::RwLock<std::collections::HashSet<[u8; 32]>>> =
        std::sync::OnceLock::new();
    KNOWN.get_or_init(|| {
        let keys = super::library::folder()
            .filter(|_| refusing())
            .map(|dir| load_untranslated(&dir.join(UNTRANSLATED)))
            .unwrap_or_default();
        if !keys.is_empty() {
            tracing::info!(stages = keys.len(), "pieces that did not translate in earlier runs are refused");
        }
        std::sync::RwLock::new(keys)
    })
}

/// Worker side of a failed translation: keeps `container`'s key for the next
/// run when it is one from pieces and refusing is on.
pub(crate) fn note_untranslated(container: &[u8]) {
    if !refusing() || !is_pieces(container) {
        return;
    }
    if let Some(dir) = super::library::folder() {
        let _ = std::fs::create_dir_all(&dir);
        append_untranslated(&dir.join(UNTRANSLATED), &crate::shader_cache::untranslated_key(container));
    }
}

/// Refuses a description with a stage that did not translate in an earlier
/// run. Title thread: an empty list costs nothing, otherwise one BLAKE3 per
/// stage and a lookup.
fn refuse_untranslated<'a>(stages: impl Iterator<Item = &'a [u8]>) -> Result<(), Refusal> {
    let known = untranslated().read().unwrap_or_else(|e| e.into_inner());
    if known.is_empty() {
        return Ok(());
    }
    for stage in stages {
        if known.contains(&crate::shader_cache::untranslated_key(stage)) {
            return Err("a stage that did not translate in an earlier run of this translator");
        }
    }
    Ok(())
}

/// Translates `shaders` now, on the title's thread, and refuses the pipeline
/// when one of them does not translate. A pipeline that is handed out and
/// only fails later on a worker can never draw, and the title, told it
/// exists, never falls back to the pipeline it makes when this call fails;
/// refused here it answers as with the switch off, from the first run on.
/// The shader cache makes a stage seen before cost a lookup; a new one is
/// translated here instead of on a worker, once.
fn refuse_failed(shaders: &[Arc<Shader>]) -> Result<(), Refusal> {
    if !refusing() {
        return Ok(());
    }
    if shaders.iter().any(|s| s.translated().is_none()) {
        return Err("a stage that does not translate");
    }
    Ok(())
}

/// `VICTORY_PIPELINE_PIECES_ONLY=compute` or `=graphics`, read once: only
/// that kind of pipeline is made from pieces and the other kind is refused
/// as with the switch off, so a wrong picture can be put down to one kind
/// in a run. Unset, both kinds are made.
fn kind_refused(kind: &str) -> Result<(), Refusal> {
    static ONLY: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let only = ONLY.get_or_init(|| std::env::var("VICTORY_PIPELINE_PIECES_ONLY").ok().filter(|v| !v.is_empty()));
    match only {
        Some(only) if only != kind => Err("a kind of pipeline left to the title (VICTORY_PIPELINE_PIECES_ONLY)"),
        _ => Ok(()),
    }
}

/// Device method 0x218: a compute pipeline from the pieces `desc` names,
/// or why it is not made.
pub(crate) fn compute(
    desc: *const u8,
    iid: *const Iid,
    out: *mut This,
) -> Result<HRESULT, Refusal> {
    kind_refused("compute")?;
    let d: ComputeDesc = described(desc, size_of::<ComputeDesc>())
        .ok_or("a description that is not there to read")?;
    rooted(d.root)?;
    let container = compute_container(&d)?;
    refuse_untranslated(std::iter::once(container.as_slice()))?;
    let shader = shader_of("compute", Cow::Owned(container));
    refuse_failed(std::slice::from_ref(&shader))?;
    Ok(hand_out(vec![shader], None, iid, out))
}

/// Refuses a description whose words after the vertex stage are not zero.
/// They are room for five more stage slots of four words (code, then
/// metadata, as the two before them; a guess, no capture had any set), and
/// a title's description of a pipeline with a stage besides the pixel and
/// the vertex stage sets one: run 10 had 11,000 such calls. Which stage a
/// slot is was not captured either (the console's order of stages would
/// make them the geometry, export, hull and local stages, a guess), so
/// each refusal names the slot and the first call of each logs its words.
///
/// What support would take: the host translates the vertex, mesh, pixel and
/// compute stages and builds no pipeline with a geometry, hull or domain
/// stage (`HostPipeline::new` says so), so any such pipeline needs those
/// stage kinds first, whatever the description says. A slot that holds a
/// stage the host does build (a vertex stage that runs as an export stage
/// of the console's geometry path, say) needs its metadata kind and the
/// packet's registers for it understood, which only a capture of such a
/// description gives.
fn other_stages(rest: &[u64; 20]) -> Result<(), Refusal> {
    let Some(slot) = rest.chunks(4).position(|w| w.iter().any(|&v| v != 0)) else {
        return Ok(());
    };
    let why = [
        "a graphics description with a stage in its first extra slot (+0x68)",
        "a graphics description with a stage in its second extra slot (+0x88)",
        "a graphics description with a stage in its third extra slot (+0xa8)",
        "a graphics description with a stage in its fourth extra slot (+0xc8)",
        "a graphics description with a stage in its fifth extra slot (+0xe8)",
    ][slot.min(4)];
    // Once per slot: the words say whether it is a stage (an address and a
    // length that look like one) or something else.
    static SEEN: std::sync::Mutex<[bool; 5]> = std::sync::Mutex::new([false; 5]);
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !std::mem::replace(&mut seen[slot.min(4)], true) {
        let w = &rest[slot * 4..slot * 4 + 4];
        tracing::warn!(
            words = format_args!("{:#x} {:#x} {:#x} {:#x}", w[0], w[1], w[2], w[3]),
            "a pipeline from pieces is not made: {why} (logged once for this slot)"
        );
    }
    Err(why)
}

/// The containers of the stages `d` describes, vertex first, and the
/// pipeline's state.
fn graphics_parts(
    d: &GraphicsDesc,
) -> Result<(Vec<(&'static str, Vec<u8>)>, GraphicsState), Refusal> {
    other_stages(&d.rest)?;
    // A pixel stage is there whole or not at all.
    let pixel = [d.pixel_code, d.pixel_metadata];
    let has_pixel = pixel.iter().any(|p| p.address != 0 || p.len != 0);
    // The packet and the state piece, on the stack.
    let (mut packet, mut state) = ([0u8; pieces::MAX_PACKET], [0u8; pieces::MAX_STATE]);
    let packet = &mut packet[..len(d.packet, pieces::MAX_PACKET)?];
    let state = &mut state[..len(d.state, pieces::MAX_STATE)?];
    read(d.packet, packet)?;
    read(d.state, state)?;
    let g = Graphics::parse(packet, state, has_pixel).inspect_err(|why| unparsed(why, packet, state))?;
    // The state before any code is read: most refusals are decided here.
    let host = state_of(&g)?;
    let stage = |code: Pair,
                 metadata: Pair,
                 draft: fn(&Graphics, usize, usize) -> Result<Draft, Refusal>| {
        let draft = draft(
            &g,
            len(code, pieces::MAX_CODE)?,
            len(metadata, pieces::MAX_METADATA)?,
        )?;
        filled(draft, code, metadata)
    };
    let mut stages = vec![(
        "vertex",
        stage(d.vertex_code, d.vertex_metadata, Graphics::vertex)?,
    )];
    if has_pixel {
        stages.push((
            "pixel",
            stage(d.pixel_code, d.pixel_metadata, Graphics::pixel)?,
        ));
    }
    Ok((stages, host))
}

/// Device method 0x1c8: a graphics pipeline from the pieces `desc` names,
/// or why it is not made.
pub(crate) fn graphics(
    desc: *const u8,
    iid: *const Iid,
    out: *mut This,
) -> Result<HRESULT, Refusal> {
    // (what is not there of the words after the stages reads as zero)
    kind_refused("graphics")?;
    let d: GraphicsDesc = described(desc, 0x68).ok_or("a description that is not there to read")?;
    rooted(d.root)?;
    let (stages, state) = graphics_parts(&d)?;
    refuse_untranslated(stages.iter().map(|(_, bytes)| bytes.as_slice()))?;
    let shaders: Vec<Arc<Shader>> = stages
        .into_iter()
        .map(|(stage, bytes)| shader_of(stage, Cow::Owned(bytes)))
        .collect();
    refuse_failed(&shaders)?;
    Ok(hand_out(shaders, Some(Box::new(state)), iid, out))
}

/// Says once per reason what the registers of a packet that does not parse
/// hold. A reason names a field; the values say what it held, which is what
/// a later change has to cover. Log only: nothing is kept.
fn unparsed(why: Refusal, packet: &[u8], state: &[u8]) {
    static SEEN: std::sync::Mutex<Vec<&str>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.contains(&why) {
        return;
    }
    seen.push(why);
    tracing::warn!(
        registers = %Graphics::describe(packet, state),
        "a graphics packet is not parsed: {why} (logged once for this reason)"
    );
}

/// A register value this module has no host state for.
fn unknown(what: Refusal, register: &'static str, value: u32) -> Refusal {
    // Once per reason: the value says what a later change must cover, and
    // a register refused for two fields shows the value of each.
    static SEEN: std::sync::Mutex<Vec<&str>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.contains(&what) {
        seen.push(what);
        tracing::warn!(
            register,
            value = format_args!("{value:#x}"),
            "a pipeline from pieces is not made: {what} (logged once for this reason)"
        );
    }
    what
}

/// The depth-stencil view format for the state piece's depth format byte,
/// `None` for one that has no depth-stencil format. D3D12 has four of
/// those (D32_FLOAT_S8X24_UINT 20, D32_FLOAT 40, D24_UNORM_S8_UINT 45,
/// D16_UNORM 55) and a pipeline's must be one of them. The same bits are
/// also named by the typeless and shader-view formats of their families,
/// which a title may give for the texture it renders depth into, so those
/// map to the member of the family that is a depth-stencil format. The
/// console's 16-bit depth with 8-bit stencil (118, and its two view
/// formats) is the 24-bit one on the host, as for the title's textures.
fn depth_format_of(format: u8) -> Option<DXGI_FORMAT> {
    let family = match crate::resource::pc_format(format.into()) {
        0 => return Some(DXGI_FORMAT_UNKNOWN),
        // R32G8X24_TYPELESS, R32_FLOAT_X8X24_TYPELESS, X32_TYPELESS_G8X24_UINT
        19 | 20 | 21 | 22 => DXGI_FORMAT_D32_FLOAT_S8X24_UINT,
        // R32_TYPELESS and R32_FLOAT (as a depth texture's view)
        39 | 40 | 41 => DXGI_FORMAT_D32_FLOAT,
        // R24G8_TYPELESS, R24_UNORM_X8_TYPELESS, X24_TYPELESS_G8_UINT
        44 | 45 | 46 | 47 => DXGI_FORMAT_D24_UNORM_S8_UINT,
        // R16_TYPELESS, D16_UNORM and R16_UNORM
        53 | 55 | 56 => DXGI_FORMAT_D16_UNORM,
        _ => return None,
    };
    Some(family)
}

/// The host's graphics state for a packet's registers and its state piece.
///
/// The registers are the console GPU's own, and nothing in a description
/// says what the title asked for in D3D12 terms, so each field is mapped by
/// the hardware's definition of it and a value outside what that covers
/// refuses the pipeline: a pipeline that draws with the wrong state is
/// worse than one that is missing. Seen in the calls captured: blend
/// factors 0, 1, 4, 5 and 8 with function 0, comparisons 1, 2, 3, 5, 6 and
/// 7, stencil operations 0, 1, 3 and 7, back-face culling or none, and
/// `FACE` 0 only (taken as counter-clockwise front faces, as the hardware
/// defines it; one capture, a pass over the screen drawn that way round
/// with back faces culled, agrees).
fn state_of(g: &Graphics) -> Result<GraphicsState, Refusal> {
    let r: &Registers = &g.registers;
    let mut s = GraphicsState::default();

    // Render targets and the depth buffer: DXGI numbers, as in a stream.
    s.targets.NumRenderTargets = u32::from(g.targets);
    for (to, &format) in s.targets.RTFormats.iter_mut().zip(&g.target_formats) {
        let host = crate::resource::target_format(crate::resource::pc_format(format.into()));
        *to = DXGI_FORMAT(host as i32);
        // (an integer target is fed by the program's own integer exports,
        // which the pixel stage's signature declares as such; the host
        // creation turns blending off on it, as D3D12 requires)
    }
    let Some(depth_format) = depth_format_of(g.depth_format) else {
        return Err(unknown(
            "a depth format that is not handled",
            "the state piece's depth format",
            g.depth_format.into(),
        ));
    };
    s.depth_format = depth_format;
    s.topology = D3D12_PRIMITIVE_TOPOLOGY_TYPE(g.topology.into());

    // The input layout: every element in slot 0, per vertex, at the byte
    // offset the piece gives.
    s.names = g
        .elements
        .iter()
        .filter_map(|e| CString::new(e.name.as_str()).ok())
        .collect();
    if s.names.len() != g.elements.len() {
        return Err("an input element with a name that is not a plain semantic");
    }
    s.inputs = g
        .elements
        .iter()
        .zip(&s.names)
        .map(|(e, name)| D3D12_INPUT_ELEMENT_DESC {
            SemanticName: PCSTR(name.as_ptr().cast()),
            SemanticIndex: 0,
            Format: DXGI_FORMAT(crate::resource::pc_format(e.format) as i32),
            InputSlot: 0,
            AlignedByteOffset: e.offset,
            InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        })
        .collect();

    // CB_COLOR_CONTROL: DISABLE_DUAL_QUAD (bit 0), ENABLE_1FRAG_PS_INVOKE
    // (1), DEGAMMA_ENABLE (3), MODE (4-6: 1 normal, 0 no colour written)
    // and ROP3 (16-23: 0xcc copies the source). DISABLE_DUAL_QUAD turns the
    // render backend's blend optimisations off (Mesa sets it for dual
    // source blending and logic operations, which they cannot do); what is
    // drawn is the same, so it maps to nothing.
    let cc = r.cb_color_control;
    if cc & !0x00ff_0071 != 0 {
        return Err(unknown(
            if cc & 2 != 0 {
                "CB_COLOR_CONTROL ENABLE_1FRAG_PS_INVOKE (bit 1)"
            } else if cc & 8 != 0 {
                "CB_COLOR_CONTROL DEGAMMA_ENABLE (bit 3)"
            } else {
                "CB_COLOR_CONTROL bits outside its documented fields"
            },
            "CB_COLOR_CONTROL",
            cc,
        ));
    }
    if cc >> 16 != 0xcc {
        return Err(unknown(
            "CB_COLOR_CONTROL ROP3 (bits 16-23) other than copy",
            "CB_COLOR_CONTROL",
            cc,
        ));
    }
    let colour = match cc >> 4 & 7 {
        1 => true,
        0 => false,
        _ => {
            return Err(unknown(
                "CB_COLOR_CONTROL MODE (bits 4-6) other than disabled and normal",
                "CB_COLOR_CONTROL",
                cc,
            ));
        }
    };
    // CB_BLENDn_CONTROL: colour source (bits 0-4), function (5-7) and
    // destination (8-12), the same for alpha from bit 16, separate alpha
    // (29), enable (30). CB_TARGET_MASK: four write bits a target.
    let factor = |v: u32| match v {
        // zero, one, source colour and its inverse, source alpha and its
        // inverse, destination alpha and its inverse, destination colour
        // and its inverse, source alpha saturated
        0..=10 => Some(D3D12_BLEND(v as i32 + 1)),
        _ => None,
    };
    // The alpha channel has no colour to blend with: the hardware reads a
    // colour factor there as the alpha of the same operand, while D3D12
    // refuses the colour factors in the alpha slots outright. This matters
    // most when SEPARATE_ALPHA_BLEND is clear and alpha reuses colour's
    // table, but a separate alpha table may name them too.
    let for_alpha = |b: Option<D3D12_BLEND>| {
        b.map(|b| match b {
            D3D12_BLEND_SRC_COLOR => D3D12_BLEND_SRC_ALPHA,
            D3D12_BLEND_INV_SRC_COLOR => D3D12_BLEND_INV_SRC_ALPHA,
            D3D12_BLEND_DEST_COLOR => D3D12_BLEND_DEST_ALPHA,
            D3D12_BLEND_INV_DEST_COLOR => D3D12_BLEND_INV_DEST_ALPHA,
            other => other,
        })
    };
    let function = |v: u32| match v {
        0 => Some(D3D12_BLEND_OP_ADD),
        1 => Some(D3D12_BLEND_OP_SUBTRACT),
        2 => Some(D3D12_BLEND_OP_MIN),
        3 => Some(D3D12_BLEND_OP_MAX),
        4 => Some(D3D12_BLEND_OP_REV_SUBTRACT),
        _ => None,
    };
    for (n, to) in s.blend.RenderTarget.iter_mut().enumerate() {
        let v = r.cb_blend_control[n];
        let mask = if colour {
            r.cb_target_mask >> (4 * n) & 0xf
        } else {
            0
        };
        to.RenderTargetWriteMask = mask as u8;
        if v & 1 << 30 == 0 {
            continue;
        }
        // With one set of factors for both, alpha takes colour's (as alpha
        // factors, see `for_alpha`).
        let alpha = if v & 1 << 29 != 0 { v >> 16 } else { v };
        let fields = (
            factor(v & 0x1f),
            factor(v >> 8 & 0x1f),
            function(v >> 5 & 7),
            for_alpha(factor(alpha & 0x1f)),
            for_alpha(factor(alpha >> 8 & 0x1f)),
            function(alpha >> 5 & 7),
        );
        let (Some(src), Some(dst), Some(op), Some(src_alpha), Some(dst_alpha), Some(op_alpha)) =
            fields
        else {
            return Err(unknown(
                "a blend factor or function that is not handled",
                "CB_BLEND_CONTROL",
                v,
            ));
        };
        // (bits 13-15 and 31, the latter the raster operation's switch,
        // were set or clear without any other change)
        to.BlendEnable = true.into();
        (to.SrcBlend, to.DestBlend, to.BlendOp) = (src, dst, op);
        (to.SrcBlendAlpha, to.DestBlendAlpha, to.BlendOpAlpha) = (src_alpha, dst_alpha, op_alpha);
    }
    // One description for all targets where they agree, as the title's
    // other pipelines have it.
    let first = (
        r.cb_blend_control[0],
        s.blend.RenderTarget[0].RenderTargetWriteMask,
    );
    let same = (0..8).all(|n| {
        (
            r.cb_blend_control[n],
            s.blend.RenderTarget[n].RenderTargetWriteMask,
        ) == first
    });
    s.blend.IndependentBlendEnable = (!same).into();

    // PA_SU_SC_MODE_CNTL: cull front (bit 0), cull back (1), FACE (2);
    // every other bit as in all calls seen (filled polygons, no offset in
    // use: the three dwords of the offset were zero, or the packet was
    // refused).
    let raster = r.pa_su_sc_mode_cntl;
    if raster & !7 != 0x0020_1a40 {
        return Err(unknown(
            "a polygon mode that is not handled",
            "PA_SU_SC_MODE_CNTL",
            raster,
        ));
    }
    s.rasterizer.CullMode = match raster & 3 {
        0 => D3D12_CULL_MODE_NONE,
        1 => D3D12_CULL_MODE_FRONT,
        2 => D3D12_CULL_MODE_BACK,
        _ => {
            return Err(unknown(
                "culling of both faces",
                "PA_SU_SC_MODE_CNTL",
                raster,
            ));
        }
    };
    s.rasterizer.FrontCounterClockwise = (raster & 4 == 0).into();
    // PA_CL_CLIP_CNTL: the D3D clip space, with or without the depth clip
    // (near and far together).
    s.rasterizer.DepthClipEnable = match r.pa_cl_clip_cntl {
        0x0008_0000 => true.into(),
        0x0c08_0000 => false.into(),
        v => {
            return Err(unknown(
                "a clip control that is not handled",
                "PA_CL_CLIP_CNTL",
                v,
            ));
        }
    };

    // DB_DEPTH_CONTROL: stencil enable (bit 0), depth enable (1), depth
    // write (2), depth function (4-6), back face stencil (7), stencil
    // function (8-10), back face stencil function (20-22). Bit 3 is the
    // depth bounds test, which needs bounds no one sets for these.
    let depth = r.db_depth_control;
    if depth & !0x0070_07f7 != 0 {
        return Err(unknown(
            "a depth control bit that is not handled",
            "DB_DEPTH_CONTROL",
            depth,
        ));
    }
    // The hardware counts comparisons from never (0) to always (7), D3D12
    // from 1.
    let compare = |v: u32| D3D12_COMPARISON_FUNC((v & 7) as i32 + 1);
    s.depth.DepthEnable = (depth & 2 != 0).into();
    s.depth.DepthWriteMask = if depth & 4 != 0 {
        D3D12_DEPTH_WRITE_MASK_ALL
    } else {
        D3D12_DEPTH_WRITE_MASK_ZERO
    };
    s.depth.DepthFunc = compare(depth >> 4);
    s.depth.StencilEnable = (depth & 1 != 0).into();
    if depth & 1 != 0 {
        // DB_STENCILREFMASK: read mask (bits 8-15), write mask (16-23),
        // the value operations add (24-31); D3D12 has one of each for both
        // faces and adds 1. The reference is the draw's.
        let masks = r.db_stencilrefmask;
        if masks != r.db_stencilrefmask_bf || masks >> 24 != 1 {
            return Err(unknown(
                "stencil masks that differ by face",
                "DB_STENCILREFMASK",
                masks,
            ));
        }
        s.depth.StencilReadMask = (masks >> 8) as u8;
        s.depth.StencilWriteMask = (masks >> 16) as u8;
        // DB_STENCIL_CONTROL: fail, pass, depth fail, four bits each, then
        // the same for back faces.
        let operation = |v: u32| match v & 0xf {
            0 => Some(D3D12_STENCIL_OP_KEEP),
            1 => Some(D3D12_STENCIL_OP_ZERO),
            3 => Some(D3D12_STENCIL_OP_REPLACE),
            5 => Some(D3D12_STENCIL_OP_INCR_SAT),
            6 => Some(D3D12_STENCIL_OP_DECR_SAT),
            7 => Some(D3D12_STENCIL_OP_INVERT),
            8 => Some(D3D12_STENCIL_OP_INCR),
            9 => Some(D3D12_STENCIL_OP_DECR),
            _ => None,
        };
        let face = |ops: u32, func: u32| {
            Some(D3D12_DEPTH_STENCILOP_DESC {
                StencilFailOp: operation(ops)?,
                StencilPassOp: operation(ops >> 4)?,
                StencilDepthFailOp: operation(ops >> 8)?,
                StencilFunc: compare(func),
            })
        };
        let ops = r.db_stencil_control;
        let front = face(ops, depth >> 8);
        // Without the back face's own state, back faces take the front's.
        let back = if depth & 0x80 != 0 {
            face(ops >> 12, depth >> 20)
        } else {
            front
        };
        let (Some(front), Some(back)) = (front, back) else {
            return Err(unknown(
                "a stencil operation that is not handled",
                "DB_STENCIL_CONTROL",
                ops,
            ));
        };
        (s.depth.FrontFace, s.depth.BackFace) = (front, back);
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{S_OK, slot};
    use std::io::Write;

    fn put(to: &mut [u8], at: usize, v: u32) {
        to[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn pair(bytes: &[u8]) -> Pair {
        Pair {
            address: bytes.as_ptr() as u64,
            len: bytes.len() as u64,
        }
    }

    /// A metadata piece whose only record is the last one: `tag` and
    /// `records` say the stage's kind, `dwords` the code it counts.
    fn metadata(tag: u32, records: usize, dwords: u32) -> Vec<u8> {
        let mut m = vec![0u8; records + 32];
        put(&mut m, 0, tag);
        put(&mut m, 8, dwords);
        m[records] = 0x43;
        m
    }

    /// A compute packet: an 8x8x1 group, 6 user SGPRs.
    fn compute_packet() -> [u8; 0x1c] {
        let mut p = [0u8; 0x1c];
        p[0x0c..0x12].copy_from_slice(&[8, 0, 8, 0, 1, 0]);
        put(&mut p, 0x18, 6 << 1);
        p
    }

    /// A root signature of ours, as the title would have one.
    fn root() -> This {
        let blob = [1u8, 2, 3, 4];
        let iid = crate::object::guid(
            0xc54a6b66,
            0x72df,
            0x4ee8,
            [0x8b, 0xe5, 0xa9, 0x46, 0xa1, 0x42, 0x92, 0x14],
        );
        let mut out = std::ptr::null();
        assert_eq!(
            crate::root::create(
                std::ptr::null(),
                0,
                blob.as_ptr(),
                blob.len(),
                &iid,
                &mut out
            ),
            S_OK
        );
        out
    }

    #[test]
    fn a_compute_pipeline_from_pieces_is_a_pipeline() {
        let packet = compute_packet();
        let piece = [0u8; 0x14];
        // `s_endpgm`, then a dword the program never reaches.
        let code = [0x00, 0x00, 0x81, 0xbf, 0, 0, 0, 0];
        let meta = metadata(0x403, 0x18, 2);
        let mut d = ComputeDesc {
            root: root() as u64,
            packet: pair(&packet),
            pipeline: pair(&piece),
            code: pair(&code),
            metadata: pair(&meta),
        };
        let iid = super::super::PIPELINE_IIDS[0];
        let mut out = std::ptr::null();
        assert_eq!(compute((&raw const d).cast(), &iid, &mut out), Ok(S_OK));
        let p = super::super::pipeline(out);
        assert_eq!(p.shaders.len(), 1);
        assert_eq!(p.shaders[0].stage, "compute");
        // The container holds the pieces as they were read, and is found
        // as the program it is.
        let programs = p.shaders[0].programs().unwrap();
        assert_eq!(programs[0].code, code);
        assert_eq!(programs[0].stage, victory_shader::HwStage::Cs);
        // The same pieces again share the shader.
        let mut again = std::ptr::null();
        assert_eq!(compute((&raw const d).cast(), &iid, &mut again), Ok(S_OK));
        assert!(Arc::ptr_eq(
            &p.shaders[0],
            &super::super::pipeline(again).shaders[0]
        ));
        for p in [out, again] {
            assert_eq!(slot::<extern "system" fn(This) -> u32>(p, 0x10)(p), 0);
        }

        // What is not a description of this kind makes nothing.
        let mut none = std::ptr::null();
        let mut refused =
            |d: &ComputeDesc| compute((&raw const *d).cast(), &iid, &mut none).is_err();
        d.root = packet.as_ptr() as u64;
        assert!(refused(&d), "a first word that is no root signature");
        d.root = 8;
        assert!(refused(&d), "a first word that is no address");
        d.root = root() as u64;
        d.packet.len = 0x20;
        assert!(refused(&d), "a packet of another length");
        d.packet = pair(&packet);
        d.code.len = 6;
        assert!(refused(&d), "code that is not whole dwords");
        d.code.len = u64::MAX;
        assert!(refused(&d), "an absurd code length");
        d.code = pair(&code);
        d.code.address = 0x10;
        assert!(refused(&d), "code at no address");
        d.code = pair(&code);
        d.metadata.len = 0x39;
        assert!(refused(&d), "metadata that is not whole records");
        d.metadata = pair(&meta[..0x18]);
        assert!(refused(&d), "metadata without a record");
        let wrong = metadata(0x403, 0x18, 3);
        d.metadata = pair(&wrong);
        assert!(refused(&d), "metadata that counts another code size");
        assert!(none.is_null());
        assert!(compute(std::ptr::null(), &iid, &mut none).is_err());
        assert!(compute(0x10 as *const u8, &iid, &mut none).is_err());
    }

    /// A graphics packet without a pixel stage: registers as the calls
    /// seen had them, with nothing enabled.
    fn graphics_packet() -> Vec<u8> {
        let mut p = vec![0u8; 0x17c];
        p[0x28..0x2c].copy_from_slice(&[0x1c, 0x01, 0x44, 0x00]);
        // 2 VGPR granules and 9 user SGPRs for the vertex stage, no
        // parameter exports, one position.
        put(&mut p, 0x38, 1);
        put(&mut p, 0x3c, 9 << 1);
        put(&mut p, 0x84, 0x80);
        put(&mut p, 0x90, 4);
        put(&mut p, 0x7c, 0x00cc_0000);
        put(&mut p, 0x9c, 0x0008_0000);
        put(&mut p, 0xa0, 0x0020_1a42);
        // Depth test and write, less-or-equal; stencil off.
        put(&mut p, 0xb8, 0x36);
        put(&mut p, 0xc0, 0x0100_ff00);
        put(&mut p, 0xc4, 0x0100_ff00);
        put(&mut p, 0xcc, u32::MAX);
        for n in 0..8 {
            put(&mut p, 0xd0 + 4 * n, 1 << 31);
        }
        p
    }

    /// A state piece: no target, a 32-bit float depth buffer, triangles,
    /// one element of three floats.
    fn state_piece() -> Vec<u8> {
        let mut s = vec![0u8; 0x4c];
        (s[0x24], s[0x26]) = (40, 3);
        put(&mut s, 0x1c, u32::MAX);
        put(&mut s, 0x28, 1);
        s.extend(1u32.to_le_bytes());
        s.extend(9u32.to_le_bytes());
        s.extend(b"POSITION\0");
        for v in [0u32, 6, 0, 0, 0, 0] {
            s.extend(v.to_le_bytes());
        }
        let block = (s.len() - 0x4c) as u32;
        put(&mut s, 0x38, block);
        s.resize(s.len().next_multiple_of(4), 0);
        s
    }

    #[test]
    fn a_graphics_pipeline_from_pieces_keeps_its_state() {
        let (packet, state) = (graphics_packet(), state_piece());
        // `s_endpgm` and the ten dwords the metadata does not count.
        let mut code = vec![0u8; 44];
        code[..4].copy_from_slice(&[0x00, 0x00, 0x81, 0xbf]);
        let meta = metadata(2, 0x10, 1);
        let mut d = GraphicsDesc {
            root: root() as u64,
            packet: pair(&packet),
            state: pair(&state),
            vertex_code: pair(&code),
            vertex_metadata: pair(&meta),
            ..Default::default()
        };
        let iid = super::super::PIPELINE_IIDS[0];
        let mut out = std::ptr::null();
        assert_eq!(graphics((&raw const d).cast(), &iid, &mut out), Ok(S_OK));
        let p = super::super::pipeline(out);
        assert_eq!(
            p.shaders.iter().map(|s| s.stage).collect::<Vec<_>>(),
            ["vertex"]
        );
        assert_eq!(
            p.shaders[0].programs().unwrap()[0].stage,
            victory_shader::HwStage::Vs
        );
        let g = p.host.graphics.as_deref().unwrap();
        assert_eq!(
            (g.topology.0, g.targets.NumRenderTargets, g.depth_format.0),
            (3, 0, 40)
        );
        assert!(g.depth.DepthEnable.as_bool() && !g.depth.StencilEnable.as_bool());
        assert_eq!(
            (g.depth.DepthWriteMask, g.depth.DepthFunc),
            (D3D12_DEPTH_WRITE_MASK_ALL, D3D12_COMPARISON_FUNC_LESS_EQUAL)
        );
        assert_eq!(g.rasterizer.CullMode, D3D12_CULL_MODE_BACK);
        assert!(
            g.rasterizer.FrontCounterClockwise.as_bool() && g.rasterizer.DepthClipEnable.as_bool()
        );
        // No colour is written, and nothing blends.
        assert!(
            g.blend
                .RenderTarget
                .iter()
                .all(|t| t.RenderTargetWriteMask == 0 && !t.BlendEnable.as_bool())
        );
        assert_eq!(
            (
                g.inputs.len(),
                g.inputs[0].Format.0,
                g.inputs[0].AlignedByteOffset
            ),
            (1, 6, 0)
        );
        assert_eq!(g.names[0].to_bytes(), b"POSITION");
        assert_eq!(slot::<extern "system" fn(This) -> u32>(out, 0x10)(out), 0);

        // A third stage, a pixel stage the packet has none for, and
        // registers outside what is mapped make nothing.
        let mut none = std::ptr::null();
        let mut refused =
            |d: &GraphicsDesc| graphics((&raw const *d).cast(), &iid, &mut none).is_err();
        d.rest[0] = 1;
        assert!(refused(&d), "a word after the two stages");
        d.rest[0] = 0;
        d.pixel_code = pair(&code);
        d.pixel_metadata = pair(&meta);
        assert!(refused(&d), "a pixel stage without its registers");
        (d.pixel_code, d.pixel_metadata) = Default::default();
        d.vertex_code = Pair::default();
        assert!(refused(&d), "no vertex stage");
        d.vertex_code = pair(&code);
        for (at, value) in [
            (0xa0, 0x0020_1a48),
            (0xb8, 0x3e),
            (0x9c, 0x0408_0000),
            (0x7c, 0x00aa_0010),
        ] {
            let mut other = packet.clone();
            put(&mut other, at, value);
            d.packet = pair(&other);
            assert!(refused(&d), "register at {at:#x}");
        }
        assert!(none.is_null());
    }

    #[test]
    fn registers_become_blend_and_stencil_state() {
        let mut packet = graphics_packet();
        // Colour on, target 1 without its alpha; source alpha over, with
        // its own alpha factors (one, inverse source alpha).
        put(&mut packet, 0x7c, 0x00cc_0010);
        put(&mut packet, 0xcc, 0xffff_ff7f);
        let over = 1 << 30 | 1 << 29 | 5 << 24 | 1 << 16 | 5 << 8 | 4;
        for n in 0..8 {
            put(&mut packet, 0xd0 + 4 * n, over);
        }
        // Stencil on with its own back face: front replaces where it
        // passes, back inverts where depth fails; functions equal and
        // not-equal; masks 0xf0 and 0x0f.
        put(&mut packet, 0xb8, 1 | 0x80 | 2 << 8 | 5 << 20 | 7 << 4);
        put(&mut packet, 0xbc, 3 << 4 | 7 << 20);
        for at in [0xc0, 0xc4] {
            put(&mut packet, at, 1 << 24 | 0x0f << 16 | 0xf0 << 8);
        }
        let g = Graphics::parse(&packet, &state_piece(), false).unwrap();
        let s = state_of(&g).unwrap();
        let t = &s.blend.RenderTarget[0];
        assert!(t.BlendEnable.as_bool());
        assert_eq!(
            (t.SrcBlend, t.DestBlend, t.BlendOp),
            (
                D3D12_BLEND_SRC_ALPHA,
                D3D12_BLEND_INV_SRC_ALPHA,
                D3D12_BLEND_OP_ADD
            )
        );
        assert_eq!(
            (t.SrcBlendAlpha, t.DestBlendAlpha),
            (D3D12_BLEND_ONE, D3D12_BLEND_INV_SRC_ALPHA)
        );
        assert_eq!(
            (
                t.RenderTargetWriteMask,
                s.blend.RenderTarget[1].RenderTargetWriteMask
            ),
            (0xf, 0x7)
        );
        assert!(s.blend.IndependentBlendEnable.as_bool());
        assert!(s.depth.StencilEnable.as_bool() && !s.depth.DepthEnable.as_bool());
        assert_eq!(
            (s.depth.StencilReadMask, s.depth.StencilWriteMask),
            (0xf0, 0x0f)
        );
        assert_eq!(
            (
                s.depth.FrontFace.StencilPassOp,
                s.depth.FrontFace.StencilFunc
            ),
            (D3D12_STENCIL_OP_REPLACE, D3D12_COMPARISON_FUNC_EQUAL)
        );
        assert_eq!(
            (
                s.depth.BackFace.StencilDepthFailOp,
                s.depth.BackFace.StencilFunc
            ),
            (D3D12_STENCIL_OP_INVERT, D3D12_COMPARISON_FUNC_NOT_EQUAL)
        );
        assert_eq!(s.depth.DepthFunc, D3D12_COMPARISON_FUNC_ALWAYS);
        // A factor outside the table, and masks that differ by face.
        let mut odd = packet.clone();
        put(&mut odd, 0xd0, over | 13);
        assert!(state_of(&Graphics::parse(&odd, &state_piece(), false).unwrap()).is_err());
        let mut faces = packet.clone();
        put(&mut faces, 0xc4, 1 << 24);
        assert!(state_of(&Graphics::parse(&faces, &state_piece(), false).unwrap()).is_err());
    }

    /// The state of a packet with `blend` in every target's blend register.
    fn blended(blend: u32) -> GraphicsState {
        let mut packet = graphics_packet();
        put(&mut packet, 0x7c, 0x00cc_0010);
        for n in 0..8 {
            put(&mut packet, 0xd0 + 4 * n, blend);
        }
        state_of(&Graphics::parse(&packet, &state_piece(), false).unwrap()).unwrap()
    }

    #[test]
    fn colour_factors_in_the_alpha_slots_become_alpha_factors() {
        // One table for both (bit 29 clear): source and inverse source
        // colour, then destination and its inverse. D3D12 refuses the
        // colour factors for alpha, the hardware reads them as alpha.
        for (src, dst, want) in [
            (2, 3, (D3D12_BLEND_SRC_ALPHA, D3D12_BLEND_INV_SRC_ALPHA)),
            (8, 9, (D3D12_BLEND_DEST_ALPHA, D3D12_BLEND_INV_DEST_ALPHA)),
            // factors that are alpha ones already stay
            (4, 5, (D3D12_BLEND_SRC_ALPHA, D3D12_BLEND_INV_SRC_ALPHA)),
            (0, 1, (D3D12_BLEND_ZERO, D3D12_BLEND_ONE)),
        ] {
            let s = blended(1 << 30 | dst << 8 | src);
            let t = &s.blend.RenderTarget[0];
            assert_eq!((t.SrcBlendAlpha, t.DestBlendAlpha), want);
            // colour keeps the colour factors
            assert_eq!(t.SrcBlend.0, src as i32 + 1);
            assert_eq!(t.DestBlend.0, dst as i32 + 1);
        }
        // Separate alpha factors that name colour ones are read the same.
        let s = blended(1 << 30 | 1 << 29 | 9 << 24 | 8 << 16 | 1 << 8 | 1);
        let t = &s.blend.RenderTarget[0];
        assert_eq!(
            (t.SrcBlendAlpha, t.DestBlendAlpha),
            (D3D12_BLEND_DEST_ALPHA, D3D12_BLEND_INV_DEST_ALPHA)
        );
    }

    #[test]
    fn colour_control_with_the_dual_quad_bit_is_a_plain_copy() {
        let mut packet = graphics_packet();
        put(&mut packet, 0x7c, 0x00cc_0011);
        let g = Graphics::parse(&packet, &state_piece(), false).unwrap();
        assert!(state_of(&g).is_ok());
        // A raster operation other than copy, and a mode other than normal.
        for cc in [0x0066_0010, 0x00cc_0020] {
            put(&mut packet, 0x7c, cc);
            let g = Graphics::parse(&packet, &state_piece(), false).unwrap();
            assert!(state_of(&g).is_err(), "{cc:#x}");
        }
    }

    #[test]
    fn depth_formats_are_depth_stencil_formats() {
        assert_eq!(depth_format_of(0), Some(DXGI_FORMAT_UNKNOWN));
        for (given, want) in [
            (20, DXGI_FORMAT_D32_FLOAT_S8X24_UINT),
            (19, DXGI_FORMAT_D32_FLOAT_S8X24_UINT),
            (40, DXGI_FORMAT_D32_FLOAT),
            (39, DXGI_FORMAT_D32_FLOAT),
            (41, DXGI_FORMAT_D32_FLOAT),
            (45, DXGI_FORMAT_D24_UNORM_S8_UINT),
            (44, DXGI_FORMAT_D24_UNORM_S8_UINT),
            (118, DXGI_FORMAT_D24_UNORM_S8_UINT),
            (119, DXGI_FORMAT_D24_UNORM_S8_UINT),
            (55, DXGI_FORMAT_D16_UNORM),
            (53, DXGI_FORMAT_D16_UNORM),
            (56, DXGI_FORMAT_D16_UNORM),
        ] {
            assert_eq!(depth_format_of(given), Some(want), "{given}");
        }
        // Colour formats are no depth format.
        for given in [1, 2, 10, 28, 87, 255] {
            assert_eq!(depth_format_of(given), None, "{given}");
        }
    }

    #[test]
    fn extra_stage_slots_are_refused_by_slot() {
        assert_eq!(other_stages(&[0; 20]), Ok(()));
        let mut rest = [0u64; 20];
        let mut reasons = Vec::new();
        for slot in 0..5 {
            rest = [0; 20];
            rest[slot * 4 + 3] = 0x40;
            reasons.push(other_stages(&rest).unwrap_err());
        }
        // Each slot has a reason of its own.
        reasons.sort_unstable();
        reasons.dedup();
        assert_eq!(reasons.len(), 5);
        assert_ne!(rest, [0; 20]);
    }

    /// The packet and state piece of every graphics description captured
    /// by `VICTORY_PIPELINE_DUMP`, in the folder that variable names,
    /// through the parser and [`state_of`]: how many get a host state, and
    /// each distinct reason one does not. Skipped when the variable is not
    /// set: the files are a title's and stay outside the repository.
    #[test]
    fn captured_states_map() {
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
        let names: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        // Pair n of call `call` (`1c8-<call>-blob<n>-<length>.bin`).
        let blob = |call: &str, n: usize| {
            let start = format!("{call}-blob{n:02}-");
            let name = names.iter().find(|name| name.starts_with(&start))?;
            std::fs::read(folder.join(name)).ok()
        };
        let (mut calls, mut mapped) = (0, 0);
        let mut why: BTreeMap<Refusal, u32> = BTreeMap::new();
        for desc in names
            .iter()
            .filter(|n| n.starts_with("1c8-") && n.ends_with("-desc.bin"))
        {
            let call = &desc[..desc.len() - "-desc.bin".len()];
            calls += 1;
            // The description's sixth word is the pixel code's address.
            let pixel = std::fs::read(folder.join(desc))
                .ok()
                .and_then(|d| d.get(0x28..0x30).map(|w| w.iter().any(|&b| b != 0)));
            let (Some(pixel), Some(packet), Some(state)) = (pixel, blob(call, 0), blob(call, 1))
            else {
                *why.entry("the dump lacks the packet or the state piece")
                    .or_default() += 1;
                continue;
            };
            match Graphics::parse(&packet, &state, pixel).and_then(|g| state_of(&g)) {
                Ok(_) => mapped += 1,
                Err(reason) => *why.entry(reason).or_default() += 1,
            }
        }
        println!("0x1c8: {calls} calls, {mapped} with a host state");
        for (reason, n) in &why {
            println!("    {n:3} x {reason}");
        }
        assert!(
            calls != 0,
            "no captured description in {}",
            folder.display()
        );
    }

    /// A stage known not to translate is refused at creation (the title
    /// then goes without the pipeline, as with the switch off), and made
    /// again once it is no longer on the list.
    #[test]
    fn a_stage_that_did_not_translate_is_refused() {
        // another group size than the test above, so that no other test
        // shares this container while its key is on the list
        let mut packet = compute_packet();
        packet[0x0c..0x12].copy_from_slice(&[4, 0, 4, 0, 1, 0]);
        let piece = [0u8; 0x14];
        let code = [0x00, 0x00, 0x81, 0xbf, 0, 0, 0, 0];
        let meta = metadata(0x403, 0x18, 2);
        let d = ComputeDesc {
            root: root() as u64,
            packet: pair(&packet),
            pipeline: pair(&piece),
            code: pair(&code),
            metadata: pair(&meta),
        };
        let iid = super::super::PIPELINE_IIDS[0];
        let key = crate::shader_cache::untranslated_key(&compute_container(&d).unwrap());
        untranslated().write().unwrap().insert(key);
        let mut out = std::ptr::null();
        assert!(compute((&raw const d).cast(), &iid, &mut out).is_err());
        assert!(out.is_null());
        untranslated().write().unwrap().remove(&key);
        assert_eq!(compute((&raw const d).cast(), &iid, &mut out), Ok(S_OK));
        assert!(!out.is_null());
    }

    /// The list on disk is only added to, reads back whole keys, and cuts
    /// a torn end.
    #[test]
    fn the_list_of_untranslated_stages_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(UNTRANSLATED);
        assert!(load_untranslated(&path).is_empty());
        let (a, b) = ([1u8; 32], [2u8; 32]);
        append_untranslated(&path, &a);
        append_untranslated(&path, &b);
        append_untranslated(&path, &a);
        // a write that stopped half way
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&[9u8; 7])
            .unwrap();
        assert_eq!(load_untranslated(&path), [a, b].into_iter().collect());
    }
}
