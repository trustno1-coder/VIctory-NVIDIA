//! Host pipelines for title pipelines (decision 0007, step 3): the
//! title's graphics state with the translated shaders, under a root
//! signature of our own that the translated shaders' bindings define
//! (`victory_shader::emit`): per stage one block of root constants for the
//! user data, root views for the title's root views, static samplers for
//! the immediate samplers, and directly indexed heaps.

use std::ffi::{CStr, CString};
use std::sync::Arc;

use victory_shader::emit::{ROOT_CBV_SPACE, SAMPLER_SPACE, USER_DATA_SPACE};
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{Interface, PCSTR};

use super::{Shader, Translated};
use crate::host::host;

/// A stream's graphics state, owned (the title's stream lives only for the
/// creation call). Subobjects the stream leaves out keep D3D12's defaults.
pub(crate) struct GraphicsState {
    pub(super) blend: D3D12_BLEND_DESC,
    pub(super) sample_mask: u32,
    pub(super) rasterizer: D3D12_RASTERIZER_DESC,
    pub(super) depth: D3D12_DEPTH_STENCIL_DESC,
    pub(super) depth_bounds: bool,
    /// Input elements, their names pointing into `names`.
    pub(super) inputs: Vec<D3D12_INPUT_ELEMENT_DESC>,
    pub(super) names: Vec<CString>,
    pub(super) strip_cut: D3D12_INDEX_BUFFER_STRIP_CUT_VALUE,
    pub(crate) topology: D3D12_PRIMITIVE_TOPOLOGY_TYPE,
    pub(crate) targets: D3D12_RT_FORMAT_ARRAY,
    pub(super) depth_format: DXGI_FORMAT,
    pub(super) samples: DXGI_SAMPLE_DESC,
}

impl Default for GraphicsState {
    fn default() -> Self {
        let target = D3D12_RENDER_TARGET_BLEND_DESC {
            SrcBlend: D3D12_BLEND_ONE,
            DestBlend: D3D12_BLEND_ZERO,
            BlendOp: D3D12_BLEND_OP_ADD,
            SrcBlendAlpha: D3D12_BLEND_ONE,
            DestBlendAlpha: D3D12_BLEND_ZERO,
            BlendOpAlpha: D3D12_BLEND_OP_ADD,
            LogicOp: D3D12_LOGIC_OP_NOOP,
            RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
            ..Default::default()
        };
        let keep = D3D12_DEPTH_STENCILOP_DESC {
            StencilFailOp: D3D12_STENCIL_OP_KEEP,
            StencilDepthFailOp: D3D12_STENCIL_OP_KEEP,
            StencilPassOp: D3D12_STENCIL_OP_KEEP,
            StencilFunc: D3D12_COMPARISON_FUNC_ALWAYS,
        };
        GraphicsState {
            blend: D3D12_BLEND_DESC {
                RenderTarget: [target; 8],
                ..Default::default()
            },
            sample_mask: u32::MAX,
            rasterizer: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_BACK,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            depth: D3D12_DEPTH_STENCIL_DESC {
                DepthEnable: true.into(),
                DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
                DepthFunc: D3D12_COMPARISON_FUNC_LESS,
                StencilReadMask: 0xff,
                StencilWriteMask: 0xff,
                FrontFace: keep,
                BackFace: keep,
                ..Default::default()
            },
            depth_bounds: false,
            inputs: Vec::new(),
            names: Vec::new(),
            strip_cut: D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_DISABLED,
            topology: D3D12_PRIMITIVE_TOPOLOGY_TYPE_UNDEFINED,
            targets: D3D12_RT_FORMAT_ARRAY::default(),
            depth_format: DXGI_FORMAT_UNKNOWN,
            samples: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
        }
    }
}

/// Integer render target formats: D3D12 cannot blend into them.
pub(super) fn integer_format(f: DXGI_FORMAT) -> bool {
    matches!(
        f,
        DXGI_FORMAT_R32G32B32A32_UINT
            | DXGI_FORMAT_R32G32B32A32_SINT
            | DXGI_FORMAT_R32G32B32_UINT
            | DXGI_FORMAT_R32G32B32_SINT
            | DXGI_FORMAT_R16G16B16A16_UINT
            | DXGI_FORMAT_R16G16B16A16_SINT
            | DXGI_FORMAT_R32G32_UINT
            | DXGI_FORMAT_R32G32_SINT
            | DXGI_FORMAT_R10G10B10A2_UINT
            | DXGI_FORMAT_R8G8B8A8_UINT
            | DXGI_FORMAT_R8G8B8A8_SINT
            | DXGI_FORMAT_R16G16_UINT
            | DXGI_FORMAT_R16G16_SINT
            | DXGI_FORMAT_R32_UINT
            | DXGI_FORMAT_R32_SINT
            | DXGI_FORMAT_R8G8_UINT
            | DXGI_FORMAT_R8G8_SINT
            | DXGI_FORMAT_R16_UINT
            | DXGI_FORMAT_R16_SINT
            | DXGI_FORMAT_R8_UINT
            | DXGI_FORMAT_R8_SINT
    )
}

/// The title's blend state as the host takes it: blending off on integer
/// render targets. The console ignores blending there, and titles leave it
/// on (the world sets it on an `R8_UINT` target), but D3D12 does not allow
/// it: vkd3d-proton refuses such a pipeline with `E_INVALIDARG` ("using
/// integer format is not supported"), and its draws are lost. With one
/// description for every target, each target gets its own copy so the
/// others keep theirs.
fn host_blend(blend: &D3D12_BLEND_DESC, targets: &D3D12_RT_FORMAT_ARRAY) -> D3D12_BLEND_DESC {
    let count = (targets.NumRenderTargets as usize).min(8);
    let per_target = blend.IndependentBlendEnable.as_bool();
    let blends = |i: usize| {
        blend.RenderTarget[if per_target { i } else { 0 }]
            .BlendEnable
            .as_bool()
    };
    if !(0..count).any(|i| integer_format(targets.RTFormats[i]) && blends(i)) {
        return *blend;
    }
    let mut host = *blend;
    if !per_target {
        host.IndependentBlendEnable = true.into();
        host.RenderTarget = [blend.RenderTarget[0]; 8];
    }
    for i in 0..count {
        if integer_format(targets.RTFormats[i]) {
            host.RenderTarget[i].BlendEnable = false.into();
        }
    }
    host
}

impl GraphicsState {
    /// Whether the draws are a pass over the screen: no vertex input, no
    /// depth or stencil, a target. Such passes fill tables once; their shaders
    /// are few. (After Belobug/VIctory-Belobug-AMD 18e57db8.)
    pub(crate) fn screen_pass(&self) -> bool {
        self.inputs.is_empty()
            && (self.depth_format == DXGI_FORMAT_UNKNOWN
                || !self.depth.DepthEnable.as_bool() && !self.depth.StencilEnable.as_bool())
            && self.targets.NumRenderTargets > 0
            && self.topology == D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE
    }

    /// Takes in one subobject of a stream: its type, then its data at +4
    /// (+8 for those holding pointers).
    ///
    /// # Safety
    /// `at` points at a whole subobject of type `kind`.
    pub(crate) unsafe fn read(&mut self, kind: u32, at: *const u8) {
        // SAFETY: the caller passes a whole subobject.
        unsafe {
            let data = at.add(4);
            match kind {
                8 => self.blend = data.cast::<D3D12_BLEND_DESC>().read_unaligned(),
                9 => self.sample_mask = data.cast::<u32>().read_unaligned(),
                10 => self.rasterizer = data.cast::<D3D12_RASTERIZER_DESC>().read_unaligned(),
                11 => {
                    self.depth = data.cast::<D3D12_DEPTH_STENCIL_DESC>().read_unaligned();
                    self.depth_bounds = false;
                }
                21 => {
                    let d = data.cast::<D3D12_DEPTH_STENCIL_DESC1>().read_unaligned();
                    self.depth_bounds = d.DepthBoundsTestEnable.as_bool();
                    self.depth = D3D12_DEPTH_STENCIL_DESC {
                        DepthEnable: d.DepthEnable,
                        DepthWriteMask: d.DepthWriteMask,
                        DepthFunc: d.DepthFunc,
                        StencilEnable: d.StencilEnable,
                        StencilReadMask: d.StencilReadMask,
                        StencilWriteMask: d.StencilWriteMask,
                        FrontFace: d.FrontFace,
                        BackFace: d.BackFace,
                    };
                }
                12 => {
                    let layout = at.add(8).cast::<D3D12_INPUT_LAYOUT_DESC>().read_unaligned();
                    if layout.pInputElementDescs.is_null() {
                        return;
                    }
                    let elements = std::slice::from_raw_parts(
                        layout.pInputElementDescs,
                        layout.NumElements as usize,
                    );
                    self.names = elements
                        .iter()
                        .map(|e| CStr::from_ptr(e.SemanticName.0.cast()).to_owned())
                        .collect();
                    self.inputs = elements
                        .iter()
                        .zip(&self.names)
                        .map(|(e, name)| D3D12_INPUT_ELEMENT_DESC {
                            SemanticName: PCSTR(name.as_ptr().cast()),
                            Format: DXGI_FORMAT(
                                crate::resource::pc_format(e.Format.0 as u32) as i32
                            ),
                            ..*e
                        })
                        .collect();
                }
                13 => {
                    self.strip_cut =
                        D3D12_INDEX_BUFFER_STRIP_CUT_VALUE(data.cast::<i32>().read_unaligned())
                }
                14 => {
                    self.topology =
                        D3D12_PRIMITIVE_TOPOLOGY_TYPE(data.cast::<i32>().read_unaligned())
                }
                15 => {
                    self.targets = data.cast::<D3D12_RT_FORMAT_ARRAY>().read_unaligned();
                    for f in &mut self.targets.RTFormats {
                        f.0 = crate::resource::target_format(crate::resource::pc_format(
                            f.0 as u32,
                        )) as i32;
                    }
                }
                16 => {
                    let f = data.cast::<u32>().read_unaligned();
                    self.depth_format = DXGI_FORMAT(crate::resource::pc_format(f) as i32);
                }
                17 => self.samples = data.cast::<DXGI_SAMPLE_DESC>().read_unaligned(),
                _ => {}
            }
        }
    }

    /// Everything the state holds, as text: part of a pipeline's name in
    /// the library and of the key a title pipeline is started early by.
    pub(crate) fn describe(&self) -> String {
        let inputs: Vec<String> = self
            .inputs
            .iter()
            .zip(&self.names)
            .map(|(e, name)| {
                format!(
                    "{name:?} {} {} {} {} {} {}",
                    e.SemanticIndex, e.Format.0, e.InputSlot, e.AlignedByteOffset, e.InputSlotClass.0, e.InstanceDataStepRate
                )
            })
            .collect();
        let bounds = if self.depth_bounds {
            " depth-bounds"
        } else {
            ""
        };
        format!(
            "{:?} {} {:?} {:?}{bounds} {inputs:?} {} {} {:?} {} {:?}",
            self.blend,
            self.sample_mask,
            self.rasterizer,
            self.depth,
            self.strip_cut.0,
            self.topology.0,
            self.targets,
            self.depth_format.0,
            self.samples
        )
    }

    fn depth1(&self) -> D3D12_DEPTH_STENCIL_DESC1 {
        let d = &self.depth;
        D3D12_DEPTH_STENCIL_DESC1 {
            DepthEnable: d.DepthEnable,
            DepthWriteMask: d.DepthWriteMask,
            DepthFunc: d.DepthFunc,
            StencilEnable: d.StencilEnable,
            StencilReadMask: d.StencilReadMask,
            StencilWriteMask: d.StencilWriteMask,
            FrontFace: d.FrontFace,
            BackFace: d.BackFace,
            DepthBoundsTestEnable: self.depth_bounds.into(),
        }
    }

    /// The vertex buffer slots the input layout reads.
    fn slots(&self) -> u32 {
        self.inputs
            .iter()
            .fold(0, |m, e| m | 1 << e.InputSlot.min(31))
    }

    /// The input elements with their byte offsets worked out.
    fn elements(&self) -> Vec<Input> {
        let mut next = [0u32; 32];
        self.inputs
            .iter()
            .map(|e| {
                let slot = e.InputSlot.min(31) as usize;
                let format = e.Format.0 as u32;
                let bytes = crate::resource::element(format).map_or(0, |(b, _)| b as u32);
                // An appended element starts at the next multiple of
                // min(4, its size) after the previous one, where D3D12 (and
                // vkd3d) place it; the host pipeline gets the title's
                // descriptions, so the offsets read here must match.
                let offset = if e.AlignedByteOffset == D3D12_APPEND_ALIGNED_ELEMENT {
                    next[slot]
                        .checked_next_multiple_of(bytes.clamp(1, 4))
                        .unwrap_or(u32::MAX)
                } else {
                    e.AlignedByteOffset
                };
                next[slot] = offset.saturating_add(bytes);
                Input {
                    slot: slot as u32,
                    offset,
                    format,
                    per_instance: e.InputSlotClass == D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA,
                }
            })
            .collect()
    }
}

/// One input element: vertex buffer slot, byte offset, format.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Input {
    pub(crate) slot: u32,
    pub(crate) offset: u32,
    pub(crate) format: u32,
    pub(crate) per_instance: bool,
}

/// The console's own primitive topology types.
const RECTANGLES: i32 = 19;
const QUADS: i32 = 32;

/// Whether the title's root SRVs are bound as one-view tables of the
/// buffer's host copy, whose reads past its end return zeros, rather than
/// as root descriptors, which have no size: a mesh shader's vertex fetch
/// has no record limit, and a read past the copy reaches whatever the host
/// placed after it (memory it has freed: the device is removed, upstream
/// PR #493). `VICTORY_ROOT_SRV_DIRECT=1` goes back to root descriptors.
pub(crate) fn bounded_root_srvs() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_ROOT_SRV_DIRECT").is_none())
}

/// `VICTORY_PIPELINE_SAMPLER_TABLE=1`: the immediate samplers of a stage are
/// a table in the sampler heap instead of static samplers of the root
/// signature, so the pipeline can be stored in and loaded from the pipeline
/// library like any other (`library::with_static_samplers`: 1,593 of 2,759
/// host pipelines, 58%, were made again by the driver in every run 5; the
/// tables are filled from the host's own descriptors, so nothing depends on
/// where the driver puts a static sampler).
///
/// Where the bytes are: a sampler description is 52 bytes of ours, written
/// once per sampler heap into [`SAMPLER_TAIL`] slots after the title's own
/// (`descriptor::host::HostHeap::new` makes sampler heaps that much longer
/// with the switch on), by the card's descriptor write, not a copy of title
/// memory; the shaders read them on the card; the title never sees them.
/// Without room in the bound sampler heap a draw of such a pipeline is
/// dropped (counted in `sampler_refused`).
pub(crate) fn sampler_table() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_SAMPLER_TABLE").is_some_and(|v| v == "1"))
}

/// Sampler slots kept after the title's in every sampler heap, for the
/// tables of [`sampler_table`].
pub(crate) const SAMPLER_TAIL: u32 = 256;

/// How many slots a sampler heap of the host gets past the title's: none
/// unless the sampler tables are on. Read by `HostHeap::new` when it makes
/// a sampler heap (the change is asked for in the notes of that file; the
/// tables have no room before it is made).
#[allow(dead_code)]
pub(crate) fn sampler_tail() -> u32 {
    if sampler_table() { SAMPLER_TAIL } else { 0 }
}

/// A sampler description in words, to tell two apart (floats by their bits).
type SamplerKey = [u32; 13];

/// The words of `d`: filter, three address modes, bias, anisotropy,
/// comparison, border colour, both LOD bounds.
fn sampler_key(d: &D3D12_SAMPLER_DESC) -> SamplerKey {
    let [r, g, b, a] = d.BorderColor.map(f32::to_bits);
    [
        d.Filter.0 as u32,
        d.AddressU.0 as u32,
        d.AddressV.0 as u32,
        d.AddressW.0 as u32,
        d.MipLODBias.to_bits(),
        d.MaxAnisotropy,
        d.ComparisonFunc.0 as u32,
        r,
        g,
        b,
        a,
        d.MinLOD.to_bits(),
        d.MaxLOD.to_bits(),
    ]
}

/// The heap descriptor of a stage's static sampler, or none where a heap
/// descriptor cannot say the same: a border colour of integer texels (the
/// description has only float borders). The border colour does not matter
/// to a sampler that never addresses by it.
pub(super) fn table_sampler(
    s: &victory_shader::emit::StaticSampler,
    aniso_point_mip_supported: bool,
) -> Option<D3D12_SAMPLER_DESC> {
    // D3D12_TEXTURE_ADDRESS_MODE_BORDER
    const BORDER: u32 = 4;
    let border = if s.address.contains(&BORDER) {
        match s.border {
            0 => [0.0; 4],
            1 => [0.0, 0.0, 0.0, 1.0],
            2 => [1.0; 4],
            _ => return None,
        }
    } else {
        [0.0; 4]
    };
    Some(D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER(s.filter_for_host(aniso_point_mip_supported) as i32),
        AddressU: D3D12_TEXTURE_ADDRESS_MODE(s.address[0] as i32),
        AddressV: D3D12_TEXTURE_ADDRESS_MODE(s.address[1] as i32),
        AddressW: D3D12_TEXTURE_ADDRESS_MODE(s.address[2] as i32),
        MipLODBias: s.mip_lod_bias,
        MaxAnisotropy: s.max_anisotropy,
        ComparisonFunc: D3D12_COMPARISON_FUNC(s.comparison as i32),
        BorderColor: border,
        MinLOD: s.min_lod,
        MaxLOD: s.max_lod,
    })
}

/// A run of sampler descriptors kept at `offset` of the tail, for every
/// stage whose samplers are exactly these.
struct SamplerBlock {
    offset: u32,
    keys: Vec<SamplerKey>,
    descs: Vec<D3D12_SAMPLER_DESC>,
}

/// The blocks made so far, in the order of their offsets, and the slots of
/// the tail they take. Taken for a moment by the threads that make pipelines
/// and by the first draw of a list that needs a block written.
struct SamplerBlocks {
    list: Vec<SamplerBlock>,
    used: u32,
}

static SAMPLER_BLOCKS: std::sync::Mutex<SamplerBlocks> =
    std::sync::Mutex::new(SamplerBlocks { list: Vec::new(), used: 0 });

/// How many blocks exist: what a list compares with before it asks for the
/// blocks to be written.
static SAMPLER_BLOCK_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The sampler tables' counters, for the stats line: stages given a table,
/// stages left with static samplers (no room, or a border a heap descriptor
/// cannot say), draws dropped for want of room in the bound sampler heap.
static SAMPLER_STATS: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Blocks, slots they take, and [`SAMPLER_STATS`].
pub(crate) fn sampler_stats() -> [u64; 5] {
    use std::sync::atomic::Ordering::Relaxed;
    let used = SAMPLER_BLOCKS.lock().unwrap_or_else(|e| e.into_inner()).used;
    [
        SAMPLER_BLOCK_COUNT.load(Relaxed) as u64,
        u64::from(used),
        SAMPLER_STATS[0].load(Relaxed),
        SAMPLER_STATS[1].load(Relaxed),
        SAMPLER_STATS[2].load(Relaxed),
    ]
}

/// Counts a draw dropped for want of room for its sampler table.
pub(crate) fn sampler_refused() {
    SAMPLER_STATS[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

impl SamplerBlocks {
    /// The offset in the tail of the block holding `descs`: the one made
    /// before for the same descriptions, else a new one. None for none or
    /// too many descriptions, or when the tail is full.
    fn block(&mut self, descs: &[D3D12_SAMPLER_DESC]) -> Option<u32> {
        let n = u32::try_from(descs.len()).ok().filter(|&n| n > 0 && n <= SAMPLER_TAIL)?;
        let keys: Vec<SamplerKey> = descs.iter().map(sampler_key).collect();
        if let Some(b) = self.list.iter().find(|b| b.keys == keys) {
            return Some(b.offset);
        }
        let offset = self.used;
        if offset + n > SAMPLER_TAIL {
            return None;
        }
        self.used += n;
        self.list.push(SamplerBlock { offset, keys, descs: descs.to_vec() });
        Some(offset)
    }
}

/// [`SamplerBlocks::block`] on the blocks of the process.
fn sampler_block(descs: &[D3D12_SAMPLER_DESC]) -> Option<u32> {
    let mut blocks = SAMPLER_BLOCKS.lock().unwrap_or_else(|e| e.into_inner());
    let offset = blocks.block(descs);
    SAMPLER_BLOCK_COUNT.store(blocks.list.len(), std::sync::atomic::Ordering::Release);
    offset
}

/// How many blocks exist now.
pub(crate) fn sampler_blocks() -> usize {
    SAMPLER_BLOCK_COUNT.load(std::sync::atomic::Ordering::Acquire)
}

/// A sampler heap of our own for lists with no title sampler heap bound
/// (the title's directly indexed samplers then read an arbitrary point
/// sampler, as they do from `Host::fallback_sampler_heap`), long enough for
/// the tail: [`SAMPLER_TAIL`] slots, all point samplers until a table is
/// written over them.
pub(crate) fn table_heap() -> Option<&'static ID3D12DescriptorHeap> {
    static HEAP: std::sync::OnceLock<Option<ID3D12DescriptorHeap>> = std::sync::OnceLock::new();
    HEAP.get_or_init(|| {
        let host = host()?;
        // SAFETY: plain shader-visible heap creation.
        let heap: ID3D12DescriptorHeap = unsafe {
            host.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER,
                NumDescriptors: SAMPLER_TAIL,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })
        }
        .ok()?;
        let desc = D3D12_SAMPLER_DESC {
            Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
            AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        // SAFETY: handles of the heap just made, written in its range.
        unsafe {
            let step = host.device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER) as usize;
            let start = heap.GetCPUDescriptorHandleForHeapStart();
            for i in 0..SAMPLER_TAIL as usize {
                host.device.CreateSampler(&desc, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: start.ptr + i * step });
            }
        }
        Some(heap)
    })
    .as_ref()
}

/// The sampler heaps the blocks were written into so far, with how many:
/// the heap is held (an address of a released heap cannot come back while
/// it is listed, so no new heap is taken for a written one). The oldest go
/// when there are more than `KEPT`; one met again is written again, which
/// holds the same descriptors.
static SAMPLER_FILLED: std::sync::Mutex<Vec<(ID3D12DescriptorHeap, usize)>> = std::sync::Mutex::new(Vec::new());

/// The tail of a bound sampler heap: where the blocks are, from the point
/// of view of a list.
pub(crate) struct SamplerTail {
    heap: ID3D12DescriptorHeap,
    cpu: usize,
    gpu: u64,
    step: u32,
    /// Slots of the heap from where the tail starts, at most [`SAMPLER_TAIL`].
    slots: u32,
}

impl SamplerTail {
    /// The tail of `heap` from slot `first` (the slots the title's
    /// descriptors take): none if no slot is left, or the heap is not one.
    pub(crate) fn new(heap: &ID3D12DescriptorHeap, first: u32) -> Option<Self> {
        let host = host()?;
        // SAFETY: plain queries of a live heap and the device.
        let (count, step, cpu, gpu) = unsafe {
            (
                heap.GetDesc().NumDescriptors,
                host.device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER),
                heap.GetCPUDescriptorHandleForHeapStart().ptr,
                heap.GetGPUDescriptorHandleForHeapStart().ptr,
            )
        };
        let slots = count.checked_sub(first)?.min(SAMPLER_TAIL);
        if slots == 0 || step == 0 {
            return None;
        }
        let skip = u64::from(first) * u64::from(step);
        Some(Self {
            heap: heap.clone(),
            cpu: cpu.checked_add(skip as usize)?,
            gpu: gpu.checked_add(skip)?,
            step,
            slots,
        })
    }

    /// The handle of the block of `n` descriptors at `offset`, if the tail
    /// has them all.
    pub(crate) fn handle(&self, offset: u32, n: u32) -> Option<u64> {
        (offset.checked_add(n)? <= self.slots).then(|| self.gpu + u64::from(offset) * u64::from(self.step))
    }

    /// Writes the blocks this heap does not hold yet; how many exist now
    /// (the caller remembers it and asks again only when [`sampler_blocks`]
    /// is more). Under one lock, so that a draw that finds a block written
    /// by another thread finds the descriptors there: the writes are small
    /// and happen once for a block and a heap.
    pub(crate) fn fill(&self) -> usize {
        const KEPT: usize = 256;
        let Some(host) = host() else { return 0 };
        let mut filled = SAMPLER_FILLED.lock().unwrap_or_else(|e| e.into_inner());
        let at = match filled.iter().position(|(h, _)| h.as_raw() == self.heap.as_raw()) {
            Some(at) => at,
            None => {
                if filled.len() >= KEPT {
                    filled.remove(0);
                }
                filled.push((self.heap.clone(), 0));
                filled.len() - 1
            }
        };
        let blocks = SAMPLER_BLOCKS.lock().unwrap_or_else(|e| e.into_inner());
        for block in blocks.list.iter().skip(filled[at].1) {
            // A block past this heap's tail is never bound (`handle`).
            if block.offset + block.descs.len() as u32 > self.slots {
                continue;
            }
            for (i, d) in block.descs.iter().enumerate() {
                let slot = (block.offset as usize + i) * self.step as usize;
                // SAFETY: a slot of the tail of a live sampler heap, in range
                // by the check above.
                unsafe { host.device.CreateSampler(d, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: self.cpu + slot }) };
            }
        }
        filled[at].1 = blocks.list.len();
        blocks.list.len()
    }
}

/// One stage of a host pipeline and where its bindings go.
pub(crate) struct Stage {
    pub(crate) shader: Arc<Shader>,
    /// The root parameter of its user data, and its size in dwords.
    pub(crate) user_data: Option<(u32, u32)>,
    /// Root parameters of its root views: (title root parameter, ours).
    pub(crate) root_cbvs: Vec<(u8, u32)>,
    pub(crate) root_srvs: Vec<(u8, u32)>,
    /// Root UAVs: one-view tables like the SRVs (or root descriptors with
    /// `VICTORY_ROOT_SRV_DIRECT=1`).
    pub(crate) root_uavs: Vec<(u8, u32)>,
    /// Its immediate samplers as a table of the sampler heap's tail
    /// ([`sampler_table`]): the root parameter, the offset of the block in
    /// the tail and its length; none for static samplers or no samplers.
    pub(crate) samplers: Option<(u32, u32, u32)>,
}

impl Stage {
    pub(crate) fn translated(&self) -> &Translated {
        self.shader
            .translated()
            .expect("host pipelines are made from translated shaders")
    }
}

pub(crate) struct HostPipeline {
    /// A compute pipeline (else graphics).
    pub(crate) compute: bool,
    /// A mesh pipeline (graphics without vertex input).
    pub(crate) mesh: bool,
    pub(crate) pso: ID3D12PipelineState,
    pub(crate) root: ID3D12RootSignature,
    /// Vertex, then pixel stage, or the compute stage.
    pub(crate) stages: Vec<Stage>,
    /// The vertex buffer slots it reads (bit per slot).
    pub(crate) slots: u32,
    pub(crate) inputs: Vec<Input>,
    /// Has the geometry stage that completes the console's rectangles
    /// (`victory_shader::emit::rect_list_gs`): its rectangle draws go out
    /// as the title gives them, three vertices a rectangle.
    pub(crate) rects: bool,
    /// One colour target blended premultiplied over what is there (source
    /// one, destination one minus source alpha): the shape of the title's
    /// interface composite, which draw.rs places in the middle of a wide
    /// picture. A shape, not a hash: the stage containers of a pipeline made
    /// from pieces are ours (`VICTORY_PIPELINE_PIECES`), so the same program
    /// has another hash there. A shape alone also fits a layer blended
    /// inside the interface's own target, so draw.rs asks for more than this
    /// flag before it takes a draw for the composite: the draw's one target
    /// must be the output's size (`State::interface_composite`, which
    /// `interface_band` and `native_frontend_viewport` both use). Always
    /// false for compute and mesh.
    pub(crate) over: bool,
    /// Its shaders, for reports of slow submissions.
    pub(crate) name: String,
    /// Its depth, stencil, cull and blend state in short, for the frame
    /// listing (empty for compute).
    #[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
    pub(crate) state: String,
}

/// Logs a pipeline that gets no host pipeline, once per reason.
fn refuse(reason: &'static str, shaders: &[Arc<Shader>]) -> Option<HostPipeline> {
    static SEEN: std::sync::Mutex<Vec<&str>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.contains(&reason) {
        seen.push(reason);
        tracing::warn!(?shaders, "no host pipeline: {reason} (logged once)");
    }
    None
}

impl HostPipeline {
    /// A host pipeline for vertex and pixel shaders with the graphics
    /// state `g`, or for one compute shader.
    pub(crate) fn new(shaders: &[Arc<Shader>], g: Option<&GraphicsState>) -> Option<HostPipeline> {
        // For the report when the host device is removed: what this thread
        // has in the library from here on.
        super::library::label(format!("{shaders:?}"));
        let find = |stage| shaders.iter().find(|s| s.stage == stage);
        let compute = match shaders {
            [cs] if cs.stage == "compute" => Some(cs),
            _ => None,
        };
        if compute.is_none()
            && shaders
                .iter()
                .any(|s| !matches!(s.stage, "vertex" | "pixel" | "mesh"))
        {
            return refuse(
                "only vertex, mesh, pixel and compute stages are translated",
                shaders,
            );
        }
        let mesh = find("mesh");
        let stages = if let Some(cs) = compute {
            vec![(cs, D3D12_SHADER_VISIBILITY_ALL)]
        } else {
            let first = match (mesh, find("vertex")) {
                (Some(ms), _) => (ms, D3D12_SHADER_VISIBILITY_MESH),
                (None, Some(vs)) => (vs, D3D12_SHADER_VISIBILITY_VERTEX),
                (None, None) => return refuse("no vertex or mesh shader", shaders),
            };
            let mut stages = vec![first];
            stages.extend(find("pixel").map(|ps| (ps, D3D12_SHADER_VISIBILITY_PIXEL)));
            stages
        };
        if mesh.is_some() && !host()?.mesh_shaders() {
            return refuse("the host GPU has no mesh shaders (or no shader model 6.6)", shaders);
        }
        for (s, _) in &stages {
            s.translated()?;
        }
        let host = host()?;

        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS19::default();
        // SAFETY: the feature query writes the structure of the given size.
        // Older runtimes may not recognize OPTIONS19; that means no support.
        let aniso_point_mip_supported = unsafe {
            host.device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS19,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
        }
        .is_ok()
            && options.AnisoFilterWithPointMipSupported.as_bool();

        // The root signature the translated shaders' bindings define.
        let mut params: Vec<D3D12_ROOT_PARAMETER1> = Vec::new();
        // The ranges table parameters point at, alive until the root
        // signature is made.
        let mut ranges: Vec<Box<D3D12_DESCRIPTOR_RANGE1>> = Vec::new();
        let mut samplers = Vec::new();
        let mut bound = Vec::new();
        for &(shader, visibility) in &stages {
            let t = &shader.translated()?.host;
            let mut stage = Stage {
                shader: shader.clone(),
                user_data: None,
                root_cbvs: Vec::new(),
                root_srvs: Vec::new(),
                root_uavs: Vec::new(),
                samplers: None,
            };
            let dwords = t.user_data_dwords();
            if dwords > 0 {
                stage.user_data = Some((params.len() as u32, dwords));
                params.push(D3D12_ROOT_PARAMETER1 {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                    Anonymous: D3D12_ROOT_PARAMETER1_0 {
                        Constants: D3D12_ROOT_CONSTANTS {
                            ShaderRegister: 0,
                            RegisterSpace: USER_DATA_SPACE,
                            Num32BitValues: dwords,
                        },
                    },
                    ShaderVisibility: visibility,
                });
            }
            for (slots, kind, out) in [
                (
                    &t.root_cbvs,
                    D3D12_ROOT_PARAMETER_TYPE_CBV,
                    &mut stage.root_cbvs,
                ),
                (
                    &t.root_srvs,
                    D3D12_ROOT_PARAMETER_TYPE_SRV,
                    &mut stage.root_srvs,
                ),
                (
                    &t.root_uavs,
                    D3D12_ROOT_PARAMETER_TYPE_UAV,
                    &mut stage.root_uavs,
                ),
            ] {
                for &slot in slots {
                    let uav = kind == D3D12_ROOT_PARAMETER_TYPE_UAV;
                    // Ours, not the title's, stay root descriptors: the
                    // table of strides, the directory of title memory by
                    // address and its fault notes.
                    let ours = if uav {
                        slot == victory_shader::emit::SLOT_FAULTS
                    } else {
                        slot == victory_shader::emit::SLOT_STRIDES
                            || slot == victory_shader::emit::SLOT_TABLE_STRIDES
                            || slot == victory_shader::emit::SLOT_MEMORY
                    };
                    if (uav || kind == D3D12_ROOT_PARAMETER_TYPE_SRV) && !ours && bounded_root_srvs() {
                        // A one-view table: the view's size bounds the
                        // reads (and a root UAV's writes), which a root
                        // descriptor leaves unbounded.
                        // (Our own table of strides stays a descriptor.)
                        ranges.push(Box::new(D3D12_DESCRIPTOR_RANGE1 {
                            RangeType: if uav {
                                D3D12_DESCRIPTOR_RANGE_TYPE_UAV
                            } else {
                                D3D12_DESCRIPTOR_RANGE_TYPE_SRV
                            },
                            NumDescriptors: 1,
                            BaseShaderRegister: u32::from(slot),
                            RegisterSpace: ROOT_CBV_SPACE,
                            Flags: D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE
                                | D3D12_DESCRIPTOR_RANGE_FLAG_DATA_VOLATILE,
                            OffsetInDescriptorsFromTableStart: 0,
                        }));
                        let range: *const D3D12_DESCRIPTOR_RANGE1 = &**ranges.last()?;
                        out.push((slot, params.len() as u32));
                        params.push(D3D12_ROOT_PARAMETER1 {
                            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                            Anonymous: D3D12_ROOT_PARAMETER1_0 {
                                DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE1 {
                                    NumDescriptorRanges: 1,
                                    pDescriptorRanges: range,
                                },
                            },
                            ShaderVisibility: visibility,
                        });
                        continue;
                    }
                    out.push((slot, params.len() as u32));
                    params.push(D3D12_ROOT_PARAMETER1 {
                        ParameterType: kind,
                        Anonymous: D3D12_ROOT_PARAMETER1_0 {
                            Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                                ShaderRegister: u32::from(slot),
                                RegisterSpace: ROOT_CBV_SPACE,
                                Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
                            },
                        },
                        ShaderVisibility: visibility,
                    });
                }
            }
            // With the sampler tables on, a stage whose samplers a heap can
            // hold (and for which the tail has room) gets a table; else its
            // samplers stay static, and the pipeline stays out of the library.
            if sampler_table() && !t.static_samplers.is_empty() {
                let descs: Option<Vec<D3D12_SAMPLER_DESC>> = t
                    .static_samplers
                    .iter()
                    .map(|s| table_sampler(s, aniso_point_mip_supported))
                    .collect();
                let block = descs.as_deref().and_then(|d| sampler_block(d).map(|offset| (offset, d.len() as u32)));
                if let Some((offset, n)) = block {
                    ranges.push(Box::new(D3D12_DESCRIPTOR_RANGE1 {
                        RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SAMPLER,
                        NumDescriptors: n,
                        BaseShaderRegister: 0,
                        RegisterSpace: SAMPLER_SPACE,
                        Flags: D3D12_DESCRIPTOR_RANGE_FLAG_NONE,
                        OffsetInDescriptorsFromTableStart: 0,
                    }));
                    let range: *const D3D12_DESCRIPTOR_RANGE1 = &**ranges.last()?;
                    stage.samplers = Some((params.len() as u32, offset, n));
                    params.push(D3D12_ROOT_PARAMETER1 {
                        ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                        Anonymous: D3D12_ROOT_PARAMETER1_0 {
                            DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE1 {
                                NumDescriptorRanges: 1,
                                pDescriptorRanges: range,
                            },
                        },
                        ShaderVisibility: visibility,
                    });
                    SAMPLER_STATS[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    bound.push(stage);
                    continue;
                }
                SAMPLER_STATS[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            for (n, s) in t.static_samplers.iter().enumerate() {
                samplers.push(D3D12_STATIC_SAMPLER_DESC {
                    Filter: D3D12_FILTER(s.filter_for_host(aniso_point_mip_supported) as i32),
                    AddressU: D3D12_TEXTURE_ADDRESS_MODE(s.address[0] as i32),
                    AddressV: D3D12_TEXTURE_ADDRESS_MODE(s.address[1] as i32),
                    AddressW: D3D12_TEXTURE_ADDRESS_MODE(s.address[2] as i32),
                    MipLODBias: s.mip_lod_bias,
                    MaxAnisotropy: s.max_anisotropy,
                    ComparisonFunc: D3D12_COMPARISON_FUNC(s.comparison as i32),
                    BorderColor: D3D12_STATIC_BORDER_COLOR(s.border as i32),
                    MinLOD: s.min_lod,
                    MaxLOD: s.max_lod,
                    ShaderRegister: n as u32,
                    RegisterSpace: SAMPLER_SPACE,
                    ShaderVisibility: visibility,
                });
            }
            bound.push(stage);
        }
        let (root, root_blob) = match root_signature(&host.device, &params, &samplers, compute.is_some()) {
            Ok(root) => root,
            Err(e) => {
                tracing::warn!(error = %e, ?shaders, "host root signature refused");
                return None;
            }
        };
        // Not from the library if it has static samplers (their places in the
        // driver's table differ between runs: `library::with_static_samplers`).
        let _samplers = super::library::with_static_samplers(!samplers.is_empty());
        let code = |i: usize| {
            stages.get(i).and_then(|(s, _)| s.translated()).map_or(
                D3D12_SHADER_BYTECODE::default(),
                |t| D3D12_SHADER_BYTECODE {
                    pShaderBytecode: t.host.dxil.as_ptr().cast(),
                    BytecodeLength: t.host.dxil.len(),
                },
            )
        };
        if compute.is_some() {
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                // SAFETY: a borrowed pointer the description never releases.
                pRootSignature: unsafe { std::mem::transmute_copy(&root) },
                CS: code(0),
                ..Default::default()
            };
            let name = super::library::name("compute", &root_blob, &[&code_bytes(&code(0))], "");
            let made = super::library::pipeline(
                &name,
                // SAFETY: the description points at live data.
                |library, name| unsafe { library.LoadComputePipeline(name, &desc) },
                // SAFETY: as above.
                || unsafe { host.device.CreateComputePipelineState(&desc) },
            );
            let pso = match made {
                Ok(pso) => pso,
                Err(e) => {
                    tracing::warn!(error = %e, ?shaders, "host pipeline refused");
                    host.log_messages();
                    return None;
                }
            };
            tracing::info!(?shaders, "host pipeline");
            crate::host::name(&pso, || format!("{shaders:?}"));
            return Some(HostPipeline {
                compute: true,
                mesh: false,
                pso,
                root,
                stages: bound,
                name: format!("{shaders:?}"),
                slots: 0,
                inputs: Vec::new(),
                rects: false,
                over: false,
                state: String::new(),
            });
        }
        let Some(g) = g else {
            return refuse("graphics state not kept (plain description)", shaders);
        };
        let depth_bounds = if g.depth_bounds {
            if host.depth_bounds() {
                true
            } else {
                static SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                if SEEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 4 {
                    tracing::warn!(
                        "a pipeline enables the depth bounds test, which the host GPU does not support: its draws pass at every depth"
                    );
                }
                false
            }
        } else {
            false
        };
        let state = g.describe();
        if mesh.is_some() {
            // "triangles": the stream names its topology since it did not
            // load from the library without one (see `mesh_pipeline`); a
            // new name keeps such entries, stored before, from being
            // looked up again.
            let name = super::library::name(
                "mesh triangles",
                &root_blob,
                &[&code_bytes(&code(0)), &code_bytes(&code(1))],
                &state,
            );
            let pso = match mesh_pipeline(&host.device, &root, code(0), code(1), g, depth_bounds, &name) {
                Ok(pso) => pso,
                Err(e) => {
                    tracing::warn!(error = %e, ?shaders, "host mesh pipeline refused");
                    host.log_messages();
                    return None;
                }
            };
            tracing::info!(?shaders, "host mesh pipeline");
            crate::host::name(&pso, || format!("{shaders:?}"));
            return Some(HostPipeline {
                compute: false,
                mesh: true,
                pso,
                root,
                stages: bound,
                name: format!("{shaders:?}"),
                slots: 0,
                inputs: Vec::new(),
                rects: false,
                over: false,
                state: String::new(),
            });
        }

        // A pipeline the title makes for rectangles gets a geometry stage
        // that completes each one from the three vertices the vertex
        // shader made. Without it (refused here or by the host) the draws
        // fall back on what `draw` does for rectangles.
        let mut rect_gs = None;
        if g.topology.0 == RECTANGLES {
            let ps = stages.get(1).and_then(|(s, _)| s.translated());
            let vs = &stages[0].0.translated()?.host.dxil;
            match victory_shader::emit::rect_list_gs(vs, ps.map(|t| &t.host.dxil[..])) {
                Ok(gs) => rect_gs = Some(gs),
                Err(e) => tracing::warn!(error = %e, ?shaders, "no rectangle geometry stage"),
            }
        }
        let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            // SAFETY: a borrowed pointer the description never releases.
            pRootSignature: unsafe { std::mem::transmute_copy(&root) },
            VS: code(0),
            PS: code(1),
            GS: rect_gs.as_ref().map_or(D3D12_SHADER_BYTECODE::default(), |gs| {
                D3D12_SHADER_BYTECODE {
                    pShaderBytecode: gs.as_ptr().cast(),
                    BytecodeLength: gs.len(),
                }
            }),
            BlendState: host_blend(&g.blend, &g.targets),
            SampleMask: g.sample_mask,
            RasterizerState: g.rasterizer,
            DepthStencilState: g.depth,
            InputLayout: D3D12_INPUT_LAYOUT_DESC {
                pInputElementDescs: g.inputs.as_ptr(),
                NumElements: g.inputs.len() as u32,
            },
            IBStripCutValue: g.strip_cut,
            // The console's rectangle and quad types draw as triangles on
            // the host (see `draw`).
            PrimitiveTopologyType: match g.topology.0 {
                RECTANGLES | QUADS => D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                _ => g.topology,
            },
            NumRenderTargets: g.targets.NumRenderTargets,
            RTVFormats: g.targets.RTFormats,
            DSVFormat: g.depth_format,
            SampleDesc: g.samples,
            ..Default::default()
        };
        // A pipeline with the geometry stage has a name of its own in the
        // library; the others keep theirs.
        let name = |gs: Option<&Vec<u8>>| match gs {
            Some(gs) => super::library::name(
                "graphics rectangles",
                &root_blob,
                &[&code_bytes(&code(0)), &code_bytes(&code(1)), &gs[..]],
                &state,
            ),
            None => super::library::name(
                "graphics",
                &root_blob,
                &[&code_bytes(&code(0)), &code_bytes(&code(1))],
                &state,
            ),
        };
        let make = |desc: &D3D12_GRAPHICS_PIPELINE_STATE_DESC, name: &str| {
            if depth_bounds {
                return graphics_depth_bounds_pipeline(&host.device, &root, desc, g.depth1(), name);
            }
            super::library::pipeline(
                name,
                // SAFETY: the description points at live data.
                |library, name| unsafe { library.LoadGraphicsPipeline(name, desc) },
                // SAFETY: as above.
                || unsafe { host.device.CreateGraphicsPipelineState(desc) },
            )
        };
        let mut made = make(&desc, &name(rect_gs.as_ref()));
        if made.is_err() && rect_gs.is_some() {
            // The host refused the geometry stage: once more without it.
            tracing::warn!(?shaders, "host pipeline refused with the rectangle geometry stage");
            host.log_messages();
            desc.GS = D3D12_SHADER_BYTECODE::default();
            rect_gs = None;
            made = make(&desc, &name(None));
        }
        let pso = match made {
            Ok(pso) => pso,
            Err(e) => {
                tracing::warn!(error = %e, ?shaders, "host pipeline refused");
                host.log_messages();
                return None;
            }
        };
        tracing::info!(?shaders, "host pipeline");
        crate::host::name(&pso, || format!("{shaders:?}"));
        tracing::debug!(
            ?shaders,
            depth = ?g.depth,
            depth_bounds,
            dsv = g.depth_format.0,
            targets = ?&g.targets.RTFormats[..g.targets.NumRenderTargets.min(8) as usize],
            blend = ?g.blend.RenderTarget[0],
            cull = g.rasterizer.CullMode.0,
            "host graphics state"
        );
        Some(HostPipeline {
            compute: false,
            mesh: false,
            pso,
            root,
            stages: bound,
            name: format!("{shaders:?}"),
            slots: g.slots(),
            inputs: g.elements(),
            rects: rect_gs.is_some(),
            over: g.targets.NumRenderTargets == 1 && {
                let b = &g.blend.RenderTarget[0];
                b.BlendEnable.as_bool() && b.SrcBlend == D3D12_BLEND_ONE && b.DestBlend == D3D12_BLEND_INV_SRC_ALPHA
            },
            state: {
                let (d, r, b) = (&g.depth, &g.rasterizer, &g.blend.RenderTarget[0]);
                let face = |f: &D3D12_DEPTH_STENCILOP_DESC| {
                    format!(
                        "fail {} depth-fail {} pass {} func {}",
                        f.StencilFailOp.0, f.StencilDepthFailOp.0, f.StencilPassOp.0, f.StencilFunc.0
                    )
                };
                let bounds = if depth_bounds { " bounds true" } else { "" };
                format!(
                    "depth {} write {} func {}{bounds}; stencil {} read {:#x} write {:#x} front [{}] back [{}]; cull {} front-ccw {}; blend {} src {} dst {} op {} mask {:#x}",
                    d.DepthEnable.as_bool(),
                    d.DepthWriteMask.0,
                    d.DepthFunc.0,
                    d.StencilEnable.as_bool(),
                    d.StencilReadMask,
                    d.StencilWriteMask,
                    face(&d.FrontFace),
                    face(&d.BackFace),
                    r.CullMode.0,
                    r.FrontCounterClockwise.as_bool(),
                    b.BlendEnable.as_bool(),
                    b.SrcBlend.0,
                    b.DestBlend.0,
                    b.BlendOp.0,
                    b.RenderTargetWriteMask
                )
            },
        })
    }
}

/// A stream subobject: its type, then its description, pointer-aligned.
#[repr(C, align(8))]
struct Sub<T>(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE, T);

fn stream_pipeline<S>(
    device: &ID3D12Device,
    stream: &mut S,
    name: &str,
) -> windows::core::Result<ID3D12PipelineState> {
    use windows::core::Interface;
    let desc = D3D12_PIPELINE_STATE_STREAM_DESC {
        SizeInBytes: size_of::<S>(),
        pPipelineStateSubobjectStream: std::ptr::from_mut(stream).cast(),
    };
    let device = device.cast::<ID3D12Device2>()?;
    super::library::pipeline(
        name,
        // SAFETY: the stream holds whole subobjects pointing at live data.
        |library, name| unsafe { library.LoadPipeline(name, &desc) },
        // SAFETY: as above.
        || unsafe { device.CreatePipelineState(&desc) },
    )
}

/// A graphics pipeline enabling the depth bounds test: `D3D12_GRAPHICS_PIPELINE_STATE_DESC`
/// has no field for `DepthBoundsTestEnable`, so it is created and loaded as a
/// pipeline-state stream with `DEPTH_STENCIL1`.
fn graphics_depth_bounds_pipeline(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
    d: &D3D12_GRAPHICS_PIPELINE_STATE_DESC,
    depth: D3D12_DEPTH_STENCIL_DESC1,
    name: &str,
) -> windows::core::Result<ID3D12PipelineState> {
    use windows::core::Interface;
    #[repr(C)]
    struct Stream {
        root: Sub<*mut std::ffi::c_void>,
        vs: Sub<D3D12_SHADER_BYTECODE>,
        ps: Sub<D3D12_SHADER_BYTECODE>,
        blend: Sub<D3D12_BLEND_DESC>,
        mask: Sub<u32>,
        raster: Sub<D3D12_RASTERIZER_DESC>,
        depth: Sub<D3D12_DEPTH_STENCIL_DESC1>,
        input: Sub<D3D12_INPUT_LAYOUT_DESC>,
        strip_cut: Sub<D3D12_INDEX_BUFFER_STRIP_CUT_VALUE>,
        topology: Sub<D3D12_PRIMITIVE_TOPOLOGY_TYPE>,
        targets: Sub<D3D12_RT_FORMAT_ARRAY>,
        dsv: Sub<DXGI_FORMAT>,
        samples: Sub<DXGI_SAMPLE_DESC>,
    }
    #[repr(C)]
    struct StreamGs {
        base: Stream,
        gs: Sub<D3D12_SHADER_BYTECODE>,
    }
    let targets = D3D12_RT_FORMAT_ARRAY {
        RTFormats: d.RTVFormats,
        NumRenderTargets: d.NumRenderTargets,
    };
    let base = Stream {
        root: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE, root.as_raw()),
        vs: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VS, d.VS),
        ps: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS, d.PS),
        blend: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND, d.BlendState),
        mask: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK, d.SampleMask),
        raster: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER, d.RasterizerState),
        depth: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL1, depth),
        input: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_INPUT_LAYOUT, d.InputLayout),
        strip_cut: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_IB_STRIP_CUT_VALUE, d.IBStripCutValue),
        topology: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PRIMITIVE_TOPOLOGY, d.PrimitiveTopologyType),
        targets: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS, targets),
        dsv: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT, d.DSVFormat),
        samples: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC, d.SampleDesc),
    };
    if d.GS.pShaderBytecode.is_null() {
        let mut stream = base;
        stream_pipeline(device, &mut stream, name)
    } else {
        let mut stream = StreamGs {
            base,
            gs: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_GS, d.GS),
        };
        stream_pipeline(device, &mut stream, name)
    }
}

/// A mesh pipeline from the title's graphics state: the pipeline-state
/// stream form (no input layout).
///
/// The stream names a triangle topology, which D3D12 does not need for a
/// mesh shader: without it the pipeline library refuses to load a stored
/// pipeline with the very stream it was made from (`E_INVALIDARG`; debug
/// layer message 972, "does not match the one used to create the PSO"), so
/// every mesh pipeline was compiled again, and its store then refused (970,
/// "already exists"). The translation makes triangle mesh shaders only
/// (`mesh_prologue` refuses others), and the debug layer wants the two to
/// agree (1323).
fn mesh_pipeline(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
    ms: D3D12_SHADER_BYTECODE,
    ps: D3D12_SHADER_BYTECODE,
    g: &GraphicsState,
    depth_bounds: bool,
    name: &str,
) -> windows::core::Result<ID3D12PipelineState> {
    use windows::core::Interface;
    #[repr(C)]
    struct Stream<D> {
        root: Sub<*mut std::ffi::c_void>,
        ms: Sub<D3D12_SHADER_BYTECODE>,
        ps: Sub<D3D12_SHADER_BYTECODE>,
        blend: Sub<D3D12_BLEND_DESC>,
        mask: Sub<u32>,
        raster: Sub<D3D12_RASTERIZER_DESC>,
        depth: Sub<D>,
        targets: Sub<D3D12_RT_FORMAT_ARRAY>,
        dsv: Sub<DXGI_FORMAT>,
        samples: Sub<DXGI_SAMPLE_DESC>,
        topology: Sub<D3D12_PRIMITIVE_TOPOLOGY_TYPE>,
    }
    let build = |depth: Sub<_>| {
        let mut stream = Stream {
            root: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE, root.as_raw()),
            ms: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS, ms),
            ps: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS, ps),
            blend: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND, host_blend(&g.blend, &g.targets)),
            mask: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK, g.sample_mask),
            raster: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER, g.rasterizer),
            depth,
            targets: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS, g.targets),
            dsv: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT, g.depth_format),
            samples: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC, g.samples),
            topology: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PRIMITIVE_TOPOLOGY,
                D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            ),
        };
        stream_pipeline(device, &mut stream, name)
    };
    if depth_bounds {
        let mut stream = Stream {
            root: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE, root.as_raw()),
            ms: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS, ms),
            ps: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS, ps),
            blend: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND, host_blend(&g.blend, &g.targets)),
            mask: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK, g.sample_mask),
            raster: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER, g.rasterizer),
            depth: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL1, g.depth1()),
            targets: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS, g.targets),
            dsv: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT, g.depth_format),
            samples: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC, g.samples),
            topology: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PRIMITIVE_TOPOLOGY,
                D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            ),
        };
        stream_pipeline(device, &mut stream, name)
    } else {
        build(Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL, g.depth))
    }
}

/// The bytes of shader code (none for an absent stage).
fn code_bytes(code: &D3D12_SHADER_BYTECODE) -> Vec<u8> {
    if code.pShaderBytecode.is_null() {
        return Vec::new();
    }
    // SAFETY: a translation's own bytes, alive while its shader is.
    unsafe { std::slice::from_raw_parts(code.pShaderBytecode.cast::<u8>(), code.BytecodeLength) }.to_vec()
}

fn root_signature(
    device: &ID3D12Device,
    params: &[D3D12_ROOT_PARAMETER1],
    samplers: &[D3D12_STATIC_SAMPLER_DESC],
    compute: bool,
) -> windows::core::Result<(ID3D12RootSignature, Vec<u8>)> {
    let input = if compute {
        D3D12_ROOT_SIGNATURE_FLAG_NONE
    } else {
        D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT
    };
    let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
        Version: windows::Win32::Graphics::Direct3D12::D3D_ROOT_SIGNATURE_VERSION_1_1,
        Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
            Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                NumParameters: params.len() as u32,
                pParameters: params.as_ptr(),
                NumStaticSamplers: samplers.len() as u32,
                pStaticSamplers: samplers.as_ptr(),
                Flags: input
                    | D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED
                    | D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED,
            },
        },
    };
    // SAFETY: the description points at live arrays; the blob is read in
    // full before it is released.
    unsafe {
        let mut blob: Option<ID3DBlob> = None;
        let mut error: Option<ID3DBlob> = None;
        if let Err(e) = D3D12SerializeVersionedRootSignature(&desc, &mut blob, Some(&mut error)) {
            if let Some(error) = error {
                let text = std::slice::from_raw_parts(
                    error.GetBufferPointer().cast::<u8>(),
                    error.GetBufferSize(),
                );
                tracing::warn!("root signature: {}", String::from_utf8_lossy(text));
            }
            return Err(e);
        }
        let blob = blob.ok_or_else(windows::core::Error::empty)?;
        let bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
        Ok((device.CreateRootSignature(0, bytes)?, bytes.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;

    use super::*;

    /// A static sampler as the translator hands them over.
    fn static_sampler(filter: u32, address: u32, border: u32) -> victory_shader::emit::StaticSampler {
        victory_shader::emit::StaticSampler {
            filter,
            address: [address; 3],
            mip_lod_bias: 0.5,
            max_anisotropy: 4,
            comparison: 1,
            border,
            min_lod: 0.0,
            max_lod: 12.0,
        }
    }

    /// A static sampler becomes the heap descriptor with the same fields.
    #[test]
    fn a_table_sampler_keeps_the_static_ones_fields() {
        let d = table_sampler(&static_sampler(0x15, 3, 0), true).unwrap();
        assert_eq!(d.Filter.0, 0x15);
        assert_eq!((d.AddressU.0, d.AddressV.0, d.AddressW.0), (3, 3, 3));
        assert_eq!((d.MipLODBias, d.MaxAnisotropy, d.ComparisonFunc.0), (0.5, 4, 1));
        assert_eq!((d.MinLOD, d.MaxLOD), (0.0, 12.0));
        assert_eq!(d.BorderColor, [0.0; 4]);
    }

    /// Border colours matter only where the sampler addresses by border; an
    /// integer border colour has no heap descriptor.
    #[test]
    fn border_colours_follow_the_address_mode() {
        let by_border = |c| table_sampler(&static_sampler(0, 4, c), true).map(|d| d.BorderColor);
        assert_eq!(by_border(0), Some([0.0; 4]));
        assert_eq!(by_border(1), Some([0.0, 0.0, 0.0, 1.0]));
        assert_eq!(by_border(2), Some([1.0; 4]));
        assert_eq!(by_border(3), None);
        assert_eq!(by_border(4), None);
        // Wrapping never reads the border: any value is a plain table sampler.
        assert!(table_sampler(&static_sampler(0, 1, 4), true).is_some());
    }

    /// The same descriptions share a block; others take the next slots; a
    /// full tail and garbage lengths refuse.
    #[test]
    fn sampler_blocks_are_shared_and_bounded() {
        let one = |filter| table_sampler(&static_sampler(filter, 1, 0), true).unwrap();
        let mut blocks = SamplerBlocks { list: Vec::new(), used: 0 };
        assert_eq!(blocks.block(&[one(0), one(0x15)]), Some(0));
        assert_eq!(blocks.block(&[one(0)]), Some(2));
        assert_eq!(blocks.block(&[one(0), one(0x15)]), Some(0));
        // Same samplers in another order are another block.
        assert_eq!(blocks.block(&[one(0x15), one(0)]), Some(3));
        assert_eq!((blocks.list.len(), blocks.used), (3, 5));
        assert_eq!(blocks.block(&[]), None);
        let many = vec![one(0); SAMPLER_TAIL as usize + 1];
        assert_eq!(blocks.block(&many), None);
        let rest = vec![one(0x30); (SAMPLER_TAIL - 5) as usize];
        assert_eq!(blocks.block(&rest), Some(5));
        assert_eq!(blocks.block(&[one(0x31)]), None);
        // What is there is still found when the tail is full.
        assert_eq!(blocks.block(&[one(0)]), Some(2));
    }

    /// The tail is none unless the switch is on, and a heap too short for
    /// it has none either.
    #[test]
    fn the_tail_needs_the_switch_and_room() {
        assert_eq!(sampler_tail(), if sampler_table() { SAMPLER_TAIL } else { 0 });
        let Some(host) = host() else { return };
        // SAFETY: plain heap creation.
        let heap: ID3D12DescriptorHeap = unsafe {
            host.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER,
                NumDescriptors: 40,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })
        }
        .unwrap();
        // Title slots up to 40: no tail. Up to 30: ten slots of it.
        assert!(SamplerTail::new(&heap, 40).is_none());
        assert!(SamplerTail::new(&heap, 41).is_none());
        let tail = SamplerTail::new(&heap, 30).unwrap();
        assert!(tail.handle(0, 10).is_some());
        assert!(tail.handle(5, 6).is_none());
        assert!(tail.handle(u32::MAX, 2).is_none());
        let (a, b) = (tail.handle(0, 1).unwrap(), tail.handle(1, 1).unwrap());
        assert!(b > a);
    }

    /// A root signature with a sampler table next to the directly indexed
    /// heaps is accepted, and a pipeline made under it keeps no static
    /// sampler: its name does not depend on the descriptors in the heap.
    #[test]
    fn a_root_signature_with_a_sampler_table_is_made() {
        let Some(host) = host() else { return };
        let range = D3D12_DESCRIPTOR_RANGE1 {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SAMPLER,
            NumDescriptors: 3,
            BaseShaderRegister: 0,
            RegisterSpace: SAMPLER_SPACE,
            Flags: D3D12_DESCRIPTOR_RANGE_FLAG_NONE,
            OffsetInDescriptorsFromTableStart: 0,
        };
        let params = [D3D12_ROOT_PARAMETER1 {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
            Anonymous: D3D12_ROOT_PARAMETER1_0 {
                DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE1 { NumDescriptorRanges: 1, pDescriptorRanges: &range },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
        }];
        let (_, blob) = root_signature(&host.device, &params, &[], true).expect("a sampler table beside directly indexed heaps");
        assert!(!blob.is_empty());
    }


    /// A triangle mesh shader of our own and a pixel shader for it.
    const MS: &str = "struct V { float4 p : SV_Position; };
        [outputtopology(\"triangle\")] [numthreads(1, 1, 1)]
        void main(out vertices V v[3], out indices uint3 t[1]) {
            SetMeshOutputCounts(3, 1);
            v[0].p = float4(0, 0, 0, 1);
            v[1].p = float4(1, 0, 0, 1);
            v[2].p = float4(0, 1, 0, 1);
            t[0] = uint3(0, 1, 2);
        }";
    const PS: &str = "float4 main() : SV_Target { return 1; }";

    /// The PC `dxc.exe` (`VICTORY_PC_DXC`, else the newest Windows SDK's).
    fn dxc() -> Option<PathBuf> {
        let dxc = match std::env::var_os("VICTORY_PC_DXC") {
            Some(p) => PathBuf::from(p),
            None => victory_shader::emit::validator::Validator::default_path()?
                .with_file_name("dxc.exe"),
        };
        dxc.exists().then_some(dxc)
    }

    fn compile(dxc: &Path, dir: &Path, src: &str, profile: &str) -> Vec<u8> {
        let (hlsl, out) = (
            dir.join(format!("{profile}.hlsl")),
            dir.join(format!("{profile}.dxil")),
        );
        std::fs::write(&hlsl, src).unwrap();
        let status = std::process::Command::new(dxc)
            .args(["-nologo", "-T", profile, "-E", "main", "-Fo"])
            .arg(&out)
            .arg(&hlsl)
            .status()
            .unwrap();
        assert!(status.success(), "dxc failed for {profile}");
        std::fs::read(out).unwrap()
    }

    fn code(bytes: &[u8]) -> D3D12_SHADER_BYTECODE {
        D3D12_SHADER_BYTECODE {
            pShaderBytecode: bytes.as_ptr().cast(),
            BytecodeLength: bytes.len(),
        }
    }

    /// A mesh pipeline made once is loaded from the library the next time,
    /// not compiled again (a stream without a topology never loads).
    #[test]
    fn a_mesh_pipeline_loads_from_the_library() {
        let Some(host) = host() else {
            return; // no host GPU here
        };
        if !host.mesh_shaders() {
            eprintln!("no host mesh shader support; skipped");
            return;
        }
        let Some(dxc) = dxc() else {
            eprintln!("no PC dxc.exe; skipped");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let ms = compile(&dxc, dir.path(), MS, "ms_6_5");
        let ps = compile(&dxc, dir.path(), PS, "ps_6_5");
        let (root, root_blob) = root_signature(&host.device, &[], &[], false).unwrap();
        let mut g = GraphicsState::default();
        // This pipeline has only a color target, with no depth buffer.
        g.depth.DepthEnable = false.into();
        g.targets.NumRenderTargets = 1;
        g.targets.RTFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
        let name = super::super::library::name("mesh test", &root_blob, &[&ms, &ps], &g.describe());
        mesh_pipeline(&host.device, &root, code(&ms), code(&ps), &g, false, &name).expect("made");
        let loaded = super::super::library::LOADED.load(Ordering::Relaxed);
        mesh_pipeline(&host.device, &root, code(&ms), code(&ps), &g, false, &name).expect("made again");
        assert!(
            super::super::library::LOADED.load(Ordering::Relaxed) > loaded,
            "the mesh pipeline was compiled again instead of loaded from the library"
        );
    }

    fn targets(formats: &[DXGI_FORMAT]) -> D3D12_RT_FORMAT_ARRAY {
        let mut t = D3D12_RT_FORMAT_ARRAY {
            NumRenderTargets: formats.len() as u32,
            ..Default::default()
        };
        t.RTFormats[..formats.len()].copy_from_slice(formats);
        t
    }

    fn blending(independent: bool) -> D3D12_BLEND_DESC {
        let mut b = GraphicsState::default().blend;
        b.IndependentBlendEnable = independent.into();
        for t in &mut b.RenderTarget {
            t.BlendEnable = true.into();
            t.SrcBlend = D3D12_BLEND_SRC_ALPHA;
            t.DestBlend = D3D12_BLEND_INV_SRC_ALPHA;
        }
        b
    }

    #[test]
    fn integer_targets_do_not_blend() {
        let t = targets(&[
            DXGI_FORMAT_R8G8B8A8_UNORM,
            DXGI_FORMAT_R16G16B16A16_FLOAT,
            DXGI_FORMAT_R8_UINT,
        ]);
        for independent in [false, true] {
            let host = host_blend(&blending(independent), &t);
            assert!(host.IndependentBlendEnable.as_bool());
            assert!(host.RenderTarget[0].BlendEnable.as_bool());
            assert!(host.RenderTarget[1].BlendEnable.as_bool());
            assert!(!host.RenderTarget[2].BlendEnable.as_bool());
            // The rest of the target's state is the title's.
            assert_eq!(host.RenderTarget[2].SrcBlend, D3D12_BLEND_SRC_ALPHA);
            assert_eq!(host.RenderTarget[0].DestBlend, D3D12_BLEND_INV_SRC_ALPHA);
        }
    }

    #[test]
    fn blend_states_without_integer_blending_are_kept() {
        // No integer target.
        let b = blending(false);
        let t = targets(&[DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R11G11B10_FLOAT]);
        assert_eq!(host_blend(&b, &t), b);
        // An integer target that does not blend.
        let mut b = blending(true);
        b.RenderTarget[1].BlendEnable = false.into();
        let t = targets(&[DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R32_UINT]);
        assert_eq!(host_blend(&b, &t), b);
        // Integer formats past the targets in use.
        let mut t = targets(&[DXGI_FORMAT_R8G8B8A8_UNORM]);
        t.RTFormats[1] = DXGI_FORMAT_R8_UINT;
        assert_eq!(host_blend(&blending(false), &t), blending(false));
    }

    const VS: &str = "float4 main(uint i : SV_VertexID) : SV_Position {
        float2 p = float2((i << 1) & 2, i & 2);
        return float4(p * 2.0 - 1.0, 0.5, 1.0);
    }";

    /// A graphics pipeline with `DepthBoundsTestEnable` loads back from the
    /// pipeline library and culls or passes fragments according to
    /// `OMSetDepthBounds` (including across a command-list cut).
    #[test]
    fn depth_bounds_pipeline_loads_and_filters_draws() {
        let Some(host) = host() else {
            return;
        };
        if !host.depth_bounds() {
            eprintln!("no host depth bounds support; skipped");
            return;
        }
        let Some(dxc) = dxc() else {
            eprintln!("no PC dxc.exe; skipped");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let vs = compile(&dxc, dir.path(), VS, "vs_6_5");
        let ps = compile(&dxc, dir.path(), PS, "ps_6_5");
        let (root, root_blob) = root_signature(&host.device, &[], &[], false).unwrap();

        let mut g = GraphicsState::default();
        // Subobject 21 (`DEPTH_STENCIL1`) with `DepthBoundsTestEnable = true`,
        // depth test itself off so only depth bounds gates the draw.
        #[repr(C, align(8))]
        struct Sub21(u32, D3D12_DEPTH_STENCIL_DESC1);
        let sub = Sub21(
            21,
            D3D12_DEPTH_STENCIL_DESC1 {
                DepthEnable: false.into(),
                DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ZERO,
                DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
                DepthBoundsTestEnable: true.into(),
                ..Default::default()
            },
        );
        // SAFETY: `sub` is a complete subobject 21.
        unsafe { g.read(21, (&raw const sub).cast()) };
        assert!(g.depth_bounds);
        assert!(g.describe().contains("depth-bounds"));
        g.rasterizer.CullMode = D3D12_CULL_MODE_NONE;
        g.targets.NumRenderTargets = 1;
        g.targets.RTFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
        g.depth_format = DXGI_FORMAT_D32_FLOAT;

        let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            // SAFETY: borrowed pointer, never released by the descriptor.
            pRootSignature: unsafe { std::mem::transmute_copy(&root) },
            VS: code(&vs),
            PS: code(&ps),
            BlendState: g.blend,
            SampleMask: g.sample_mask,
            RasterizerState: g.rasterizer,
            DepthStencilState: g.depth,
            IBStripCutValue: g.strip_cut,
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            RTVFormats: g.targets.RTFormats,
            DSVFormat: g.depth_format,
            SampleDesc: g.samples,
            ..Default::default()
        };
        let name = super::super::library::name(
            "graphics depth-bounds test",
            &root_blob,
            &[&vs, &ps],
            &g.describe(),
        );
        let pso = graphics_depth_bounds_pipeline(&host.device, &root, &desc, g.depth1(), &name)
            .expect("depth-bounds PSO made");
        let loaded = super::super::library::LOADED.load(Ordering::Relaxed);
        graphics_depth_bounds_pipeline(&host.device, &root, &desc, g.depth1(), &name)
            .expect("depth-bounds PSO made again");
        assert!(
            super::super::library::LOADED.load(Ordering::Relaxed) > loaded,
            "the depth-bounds pipeline was compiled again instead of loaded from the library"
        );

        // Execute two draws against a depth buffer cleared to 0.5:
        // 1) bounds [0.6, 1.0] -> rejected (target stays 0)
        // 2) bounds [0.4, 0.6] replayed onto a fresh HostList -> passes (target becomes 255)
        let make_tex = |format, flags, state, clear: D3D12_CLEAR_VALUE| {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            let rd = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 4,
                Height: 4,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Flags: flags,
                ..Default::default()
            };
            let mut out: Option<ID3D12Resource> = None;
            // SAFETY: valid heap and resource descriptions.
            unsafe {
                host.device
                    .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &rd, state, Some(&clear), &mut out)
                    .unwrap();
            }
            out.unwrap()
        };
        let color = make_tex(
            DXGI_FORMAT_R8G8B8A8_UNORM,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            D3D12_CLEAR_VALUE {
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                Anonymous: D3D12_CLEAR_VALUE_0 { Color: [0.0; 4] },
            },
        );
        let depth = make_tex(
            DXGI_FORMAT_D32_FLOAT,
            D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
            D3D12_RESOURCE_STATE_DEPTH_WRITE,
            D3D12_CLEAR_VALUE {
                Format: DXGI_FORMAT_D32_FLOAT,
                Anonymous: D3D12_CLEAR_VALUE_0 {
                    DepthStencil: D3D12_DEPTH_STENCIL_VALUE {
                        Depth: 0.5,
                        Stencil: 0,
                    },
                },
            },
        );
        let readback = {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_READBACK,
                ..Default::default()
            };
            let rd = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 1024,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let mut out: Option<ID3D12Resource> = None;
            // SAFETY: valid readback buffer description.
            unsafe {
                host.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &rd,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                        None,
                        &mut out,
                    )
                    .unwrap();
            }
            out.unwrap()
        };

        let run_draw = |min: f32, max: f32| -> u8 {
            let mut state = crate::draw::State::default();
            let first_list = host.new_list().unwrap();
            state.set_depth_bounds(&first_list, min, max);
            first_list.close();
            // Replay onto a second host list, exercising `State::replay`.
            let list = host.new_list().unwrap();
            state.replay(&list);
            let rtv = host.rtv(Some(&color), None);
            let dsv = host.dsv(&depth, None);
            let vp = D3D12_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: 4.0,
                Height: 4.0,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            let rect = windows::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: 4,
                bottom: 4,
            };
            // SAFETY: open command list, live resources and views.
            unsafe {
                list.list.ClearRenderTargetView(rtv, &[0.0; 4], None);
                list.list.ClearDepthStencilView(dsv, D3D12_CLEAR_FLAG_DEPTH, 0.5, 0, None);
                list.list.OMSetRenderTargets(1, Some(&rtv), false, Some(&dsv));
                list.list.RSSetViewports(&[vp]);
                list.list.RSSetScissorRects(&[rect]);
                list.list.SetGraphicsRootSignature(&root);
                list.list.SetPipelineState(&pso);
                list.list
                    .IASetPrimitiveTopology(windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                list.list.DrawInstanced(3, 1, 0, 0);
                list.list.ResourceBarrier(&[crate::host::transition(
                    &color,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                let dst = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&readback),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                            Offset: 0,
                            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                                Width: 4,
                                Height: 4,
                                Depth: 1,
                                RowPitch: 256,
                            },
                        },
                    },
                };
                let src = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&color),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
                };
                list.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
                list.list.ResourceBarrier(&[crate::host::transition(
                    &color,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                )]);
            }
            list.close();
            host.execute(&[&list]);
            let mut ptr = std::ptr::null_mut();
            // SAFETY: `execute` waited for the GPU; readback holds 1024 bytes.
            unsafe {
                readback.Map(0, None, Some(&mut ptr)).unwrap();
                let pixel = *ptr.cast::<u8>();
                readback.Unmap(0, None);
                pixel
            }
        };

        assert_eq!(run_draw(0.6, 1.0), 0, "depth 0.5 outside [0.6, 1.0] must be rejected");
        assert_eq!(run_draw(0.4, 0.6), 255, "depth 0.5 inside [0.4, 0.6] must pass");
    }

    fn element(format: DXGI_FORMAT, offset: u32) -> D3D12_INPUT_ELEMENT_DESC {
        D3D12_INPUT_ELEMENT_DESC {
            SemanticName: PCSTR(c"TEXCOORD".as_ptr().cast()),
            SemanticIndex: 0,
            Format: format,
            InputSlot: 0,
            AlignedByteOffset: offset,
            InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        }
    }

    fn offsets(inputs: &[D3D12_INPUT_ELEMENT_DESC]) -> Vec<u32> {
        let state = GraphicsState {
            inputs: inputs.to_vec(),
            ..GraphicsState::default()
        };
        state.elements().iter().map(|e| e.offset).collect()
    }

    /// An appended element after one of 1 or 2 bytes started right after
    /// it; D3D12 aligns it to min(4, its size), so the rectangle expansion
    /// read its vertices from the wrong bytes.
    #[test]
    fn appended_elements_are_aligned() {
        const APPEND: u32 = D3D12_APPEND_ALIGNED_ELEMENT;
        // RG8 (2 bytes) then RG32 float: 4, not 2.
        let a = [
            element(DXGI_FORMAT_R8G8_UNORM, 0),
            element(DXGI_FORMAT_R32G32_FLOAT, APPEND),
        ];
        assert_eq!(offsets(&a), [0, 4]);
        // R8 then R16 float: 2, not 1; then R8 packs right after it.
        let b = [
            element(DXGI_FORMAT_R8_UNORM, 0),
            element(DXGI_FORMAT_R16_FLOAT, APPEND),
            element(DXGI_FORMAT_R8_UNORM, APPEND),
        ];
        assert_eq!(offsets(&b), [0, 2, 4]);
        // Already aligned layouts are unchanged.
        let c = [
            element(DXGI_FORMAT_R8G8B8A8_UNORM, 0),
            element(DXGI_FORMAT_R32G32_FLOAT, APPEND),
            element(DXGI_FORMAT_R32G32B32_FLOAT, APPEND),
        ];
        assert_eq!(offsets(&c), [0, 4, 12]);
    }

    /// An explicit offset near 4 GiB overflowed the next appended offset
    /// (a panic in debug builds).
    #[test]
    fn a_huge_explicit_offset_does_not_overflow() {
        let inputs = [
            element(DXGI_FORMAT_R32G32B32A32_FLOAT, u32::MAX - 8),
            element(DXGI_FORMAT_R32_FLOAT, D3D12_APPEND_ALIGNED_ELEMENT),
        ];
        let _ = offsets(&inputs);
    }
}
