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
use windows::core::PCSTR;

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
fn integer_format(f: DXGI_FORMAT) -> bool {
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

        let aniso_point_mip_supported = super::samplers::aniso_point_mip(&host.device);

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
            super::samplers::know(&t.static_samplers);
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

/// Whether host pipelines with identical root-signature descriptions on the
/// same device share one `ID3D12RootSignature` and serialized blob
/// (`VICTORY_ROOT_CACHE=0`: serialize and create a fresh root signature per
/// pipeline, as before).
fn root_cache_on() -> bool {
    crate::toggled!("VICTORY_ROOT_CACHE")
}

/// Every field of `D3D12_STATIC_SAMPLER_DESC` is encoded into [`root_sig_key`].
const _: () = assert!(size_of::<D3D12_STATIC_SAMPLER_DESC>() == 13 * 4);

/// Stack-inline word buffer for root-signature cache lookups, avoiding heap
/// allocation on cache hits.
struct RootSigKeyBuf {
    inline: [u32; 256],
    len: usize,
    spill: Vec<u32>,
}

impl RootSigKeyBuf {
    fn new() -> Self {
        Self {
            inline: [0; 256],
            len: 0,
            spill: Vec::new(),
        }
    }

    fn push(&mut self, word: u32) {
        if self.spill.is_empty() {
            if self.len < self.inline.len() {
                self.inline[self.len] = word;
                self.len += 1;
                return;
            }
            self.spill.reserve(self.inline.len() * 2);
            self.spill.extend_from_slice(&self.inline);
        }
        self.spill.push(word);
    }

    fn extend<const N: usize>(&mut self, words: [u32; N]) {
        for w in words {
            self.push(w);
        }
    }

    fn as_slice(&self) -> &[u32] {
        if self.spill.is_empty() {
            &self.inline[..self.len]
        } else {
            &self.spill
        }
    }

    fn into_boxed_slice(self) -> Box<[u32]> {
        if self.spill.is_empty() {
            self.inline[..self.len].into()
        } else {
            self.spill.into_boxed_slice()
        }
    }
}

/// Builds a lossless, self-delimiting key over `device`, `flags`, `params`,
/// and `samplers` (`None` if a table parameter carries a null range pointer
/// with a non-zero range count or an unknown parameter type).
fn root_sig_key(
    device: &ID3D12Device,
    flags: D3D12_ROOT_SIGNATURE_FLAGS,
    params: &[D3D12_ROOT_PARAMETER1],
    samplers: &[D3D12_STATIC_SAMPLER_DESC],
) -> Option<RootSigKeyBuf> {
    use windows::core::Interface;
    let mut key = RootSigKeyBuf::new();
    let raw = device.as_raw() as usize as u64;
    key.extend([
        raw as u32,
        (raw >> 32) as u32,
        flags.0 as u32,
        params.len() as u32,
    ]);
    for p in params {
        key.extend([p.ParameterType.0 as u32, p.ShaderVisibility.0 as u32]);
        match p.ParameterType {
            D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS => {
                // SAFETY: `ParameterType` selects `Constants`.
                let c = unsafe { p.Anonymous.Constants };
                key.extend([c.ShaderRegister, c.RegisterSpace, c.Num32BitValues]);
            }
            D3D12_ROOT_PARAMETER_TYPE_CBV
            | D3D12_ROOT_PARAMETER_TYPE_SRV
            | D3D12_ROOT_PARAMETER_TYPE_UAV => {
                // SAFETY: `ParameterType` selects `Descriptor`.
                let d = unsafe { p.Anonymous.Descriptor };
                key.extend([d.ShaderRegister, d.RegisterSpace, d.Flags.0 as u32]);
            }
            D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE => {
                // SAFETY: `ParameterType` selects `DescriptorTable`.
                let t = unsafe { p.Anonymous.DescriptorTable };
                if t.NumDescriptorRanges > 0 && t.pDescriptorRanges.is_null() {
                    return None;
                }
                key.push(t.NumDescriptorRanges);
                if t.NumDescriptorRanges > 0 {
                    // SAFETY: caller supplies `NumDescriptorRanges` live ranges.
                    let ranges = unsafe {
                        std::slice::from_raw_parts(t.pDescriptorRanges, t.NumDescriptorRanges as usize)
                    };
                    for r in ranges {
                        key.extend([
                            r.RangeType.0 as u32,
                            r.NumDescriptors,
                            r.BaseShaderRegister,
                            r.RegisterSpace,
                            r.Flags.0 as u32,
                            r.OffsetInDescriptorsFromTableStart,
                        ]);
                    }
                }
            }
            _ => return None,
        }
    }
    key.push(samplers.len() as u32);
    for s in samplers {
        key.extend([
            s.Filter.0 as u32,
            s.AddressU.0 as u32,
            s.AddressV.0 as u32,
            s.AddressW.0 as u32,
            s.MipLODBias.to_bits(),
            s.MaxAnisotropy,
            s.ComparisonFunc.0 as u32,
            s.BorderColor.0 as u32,
            s.MinLOD.to_bits(),
            s.MaxLOD.to_bits(),
            s.ShaderRegister,
            s.RegisterSpace,
            s.ShaderVisibility.0 as u32,
        ]);
    }
    Some(key)
}

struct CachedRootSig {
    /// Retains the parent device so its COM interface pointer stays live
    /// (`refcount >= 1`) and can never be reused for a later device (`ABA`
    /// across standalone test devices).
    _device: ID3D12Device,
    root: ID3D12RootSignature,
    blob: Arc<[u8]>,
}

// SAFETY: D3D12 devices and root signatures are free-threaded COM objects;
// `blob` is an immutable `Arc<[u8]>`.
unsafe impl Send for CachedRootSig {}
// SAFETY: as above; concurrent readers only clone `root` and `blob`.
unsafe impl Sync for CachedRootSig {}

type RootSigMap = std::collections::HashMap<Box<[u32]>, CachedRootSig>;

static ROOT_SIGNATURES: std::sync::RwLock<Option<RootSigMap>> = std::sync::RwLock::new(None);

fn root_signature(
    device: &ID3D12Device,
    params: &[D3D12_ROOT_PARAMETER1],
    samplers: &[D3D12_STATIC_SAMPLER_DESC],
    compute: bool,
) -> windows::core::Result<(ID3D12RootSignature, Arc<[u8]>)> {
    root_signature_in(device, params, samplers, compute, root_cache_on(), &ROOT_SIGNATURES)
}

fn root_signature_in(
    device: &ID3D12Device,
    params: &[D3D12_ROOT_PARAMETER1],
    samplers: &[D3D12_STATIC_SAMPLER_DESC],
    compute: bool,
    cached: bool,
    cache: &std::sync::RwLock<Option<RootSigMap>>,
) -> windows::core::Result<(ID3D12RootSignature, Arc<[u8]>)> {
    let input = if compute {
        D3D12_ROOT_SIGNATURE_FLAG_NONE
    } else {
        D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT
    };
    let flags = input
        | D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED
        | D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED;
    let key = cached.then(|| root_sig_key(device, flags, params, samplers)).flatten();
    if let Some(key) = &key
        && let Some(entry) = cache
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|m| m.get(key.as_slice()))
    {
        return Ok((entry.root.clone(), entry.blob.clone()));
    }
    let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
        Version: windows::Win32::Graphics::Direct3D12::D3D_ROOT_SIGNATURE_VERSION_1_1,
        Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
            Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                NumParameters: params.len() as u32,
                pParameters: params.as_ptr(),
                NumStaticSamplers: samplers.len() as u32,
                pStaticSamplers: samplers.as_ptr(),
                Flags: flags,
            },
        },
    };
    // SAFETY: the description points at live arrays; the blob is read in
    // full before it is released.
    let (root, blob) = unsafe {
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
        let made: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
        (made, Arc::<[u8]>::from(bytes))
    };
    if let Some(key) = key {
        let mut guard = cache.write().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .get_or_insert_with(std::collections::HashMap::new)
            .entry(key.into_boxed_slice())
            .or_insert_with(|| CachedRootSig {
                _device: device.clone(),
                root,
                blob,
            });
        return Ok((entry.root.clone(), entry.blob.clone()));
    }
    Ok((root, blob))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;

    use super::*;

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

    fn compile_named(dxc: &Path, dir: &Path, name: &str, src: &str, profile: &str) -> Vec<u8> {
        let (hlsl, out) = (
            dir.join(format!("{name}.hlsl")),
            dir.join(format!("{name}.dxil")),
        );
        std::fs::write(&hlsl, src).unwrap();
        let status = std::process::Command::new(dxc)
            .args(["-nologo", "-T", profile, "-E", "main", "-Fo"])
            .arg(&out)
            .arg(&hlsl)
            .status()
            .unwrap();
        assert!(status.success(), "dxc failed for {name} ({profile})");
        std::fs::read(out).unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn synthetic_shader(
        stage: &'static str,
        hw: victory_shader::HwStage,
        hash: &str,
        dxil: Vec<u8>,
        user_data: Vec<victory_shader::emit::UserData>,
        root_cbvs: Vec<u8>,
        root_srvs: Vec<u8>,
        root_uavs: Vec<u8>,
        static_samplers: Vec<victory_shader::emit::StaticSampler>,
        stores: bool,
    ) -> Arc<Shader> {
        let translated = std::sync::OnceLock::new();
        let _ = translated.set(Some(Translated {
            host: victory_shader::emit::HostShader {
                stage: hw,
                dxil,
                user_data,
                samplers: Vec::new(),
                static_samplers,
                root_cbvs,
                root_srvs,
                root_uavs,
                root_indexed_views: Vec::new(),
            },
            threads: (hw == victory_shader::HwStage::Cs).then_some([1, 1, 1]),
            used: Vec::new(),
            tables: crate::draw::TablePlan::new(&[], &[]),
            stores,
            stores_by_address: false,
            picks: Default::default(),
        }));
        Arc::new(Shader {
            stage,
            hash: hash.to_owned(),
            container: hash.as_bytes().to_vec(),
            ambient: None,
            translated,
        })
    }

    fn sample_static_sampler(filter: u32, bias: f32) -> D3D12_STATIC_SAMPLER_DESC {
        D3D12_STATIC_SAMPLER_DESC {
            Filter: D3D12_FILTER(filter as i32),
            AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressV: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
            AddressW: D3D12_TEXTURE_ADDRESS_MODE_MIRROR,
            MipLODBias: bias,
            MaxAnisotropy: 4,
            ComparisonFunc: D3D12_COMPARISON_FUNC_LESS_EQUAL,
            BorderColor: D3D12_STATIC_BORDER_COLOR_OPAQUE_BLACK,
            MinLOD: 0.0,
            MaxLOD: 12.0,
            ShaderRegister: 0,
            RegisterSpace: SAMPLER_SPACE,
            ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
        }
    }

    #[test]
    fn root_sig_key_buf_spills_past_inline_capacity() {
        let mut buf = RootSigKeyBuf::new();
        for i in 0..400u32 {
            buf.push(i.wrapping_mul(7));
        }
        assert_eq!(buf.as_slice().len(), 400);
        assert!(!buf.spill.is_empty());
        let expected: Vec<u32> = (0..400u32).map(|i| i.wrapping_mul(7)).collect();
        assert_eq!(buf.as_slice(), &expected[..]);
        assert_eq!(&*buf.into_boxed_slice(), &expected[..]);
    }

    #[test]
    fn root_signatures_deduplicate_by_exact_layout_and_device() {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        use windows::Win32::Graphics::Dxgi::{
            CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter, IDXGIFactory4,
        };
        use windows::core::Interface;

        let Some(host) = host() else {
            return;
        };
        let cache: std::sync::RwLock<Option<RootSigMap>> = std::sync::RwLock::new(None);

        let range_srv = D3D12_DESCRIPTOR_RANGE1 {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
            NumDescriptors: 1,
            BaseShaderRegister: 2,
            RegisterSpace: ROOT_CBV_SPACE,
            Flags: D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE
                | D3D12_DESCRIPTOR_RANGE_FLAG_DATA_VOLATILE,
            OffsetInDescriptorsFromTableStart: 0,
        };
        let base_params = [
            D3D12_ROOT_PARAMETER1 {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                Anonymous: D3D12_ROOT_PARAMETER1_0 {
                    Constants: D3D12_ROOT_CONSTANTS {
                        ShaderRegister: 0,
                        RegisterSpace: USER_DATA_SPACE,
                        Num32BitValues: 4,
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            },
            D3D12_ROOT_PARAMETER1 {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_CBV,
                Anonymous: D3D12_ROOT_PARAMETER1_0 {
                    Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                        ShaderRegister: 1,
                        RegisterSpace: ROOT_CBV_SPACE,
                        Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            },
            D3D12_ROOT_PARAMETER1 {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                Anonymous: D3D12_ROOT_PARAMETER1_0 {
                    DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE1 {
                        NumDescriptorRanges: 1,
                        pDescriptorRanges: &range_srv,
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
            },
        ];
        let base_sampler = sample_static_sampler(D3D12_FILTER_MIN_MAG_MIP_LINEAR.0 as u32, 0.0);

        let (r1, b1) =
            root_signature_in(&host.device, &base_params, &[base_sampler], false, true, &cache)
                .unwrap();
        let (r2, b2) =
            root_signature_in(&host.device, &base_params, &[base_sampler], false, true, &cache)
                .unwrap();
        assert_eq!(r1.as_raw(), r2.as_raw(), "identical root layouts must share COM pointer");
        assert!(Arc::ptr_eq(&b1, &b2), "identical root layouts must share serialized blob Arc");

        // With `cached = false` (`VICTORY_ROOT_CACHE=0`), the cache is neither read nor populated
        // and each call serializes a fresh blob (`!Arc::ptr_eq`), even on Windows 11 runtimes where
        // `ID3D12Device::CreateRootSignature` itself deduplicates live COM objects.
        let bypassed_cache: std::sync::RwLock<Option<RootSigMap>> = std::sync::RwLock::new(None);
        let (u1, ub1) = root_signature_in(
            &host.device,
            &base_params,
            &[base_sampler],
            false,
            false,
            &bypassed_cache,
        )
        .unwrap();
        let (u2, ub2) = root_signature_in(
            &host.device,
            &base_params,
            &[base_sampler],
            false,
            false,
            &bypassed_cache,
        )
        .unwrap();
        assert!(bypassed_cache.read().unwrap().is_none());
        assert!(!Arc::ptr_eq(&ub1, &ub2));
        assert!(!Arc::ptr_eq(&ub1, &b1));
        assert_eq!(&ub1[..], &b1[..]);
        assert_eq!(&ub2[..], &b1[..]);
        assert!(!u1.as_raw().is_null() && !u2.as_raw().is_null());

        // Compute vs graphics flag (`ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT`) produces a distinct root signature.
        let (r_compute, b_compute) =
            root_signature_in(&host.device, &base_params, &[base_sampler], true, true, &cache)
                .unwrap();
        assert_ne!(r_compute.as_raw(), r1.as_raw());
        assert_ne!(&b_compute[..], &b1[..]);

        // Changing constant count, visibility, descriptor register, table range, or sampler bits yields distinct objects.
        let mut alt_params = base_params;
        alt_params[0].Anonymous.Constants.Num32BitValues = 8;
        let (r_const, _) =
            root_signature_in(&host.device, &alt_params, &[base_sampler], false, true, &cache)
                .unwrap();
        assert_ne!(r_const.as_raw(), r1.as_raw());

        let mut alt_vis = base_params;
        alt_vis[0].ShaderVisibility = D3D12_SHADER_VISIBILITY_VERTEX;
        let (r_vis, _) =
            root_signature_in(&host.device, &alt_vis, &[base_sampler], false, true, &cache)
                .unwrap();
        assert_ne!(r_vis.as_raw(), r1.as_raw());

        let range_uav = D3D12_DESCRIPTOR_RANGE1 {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
            ..range_srv
        };
        let mut alt_table = base_params;
        alt_table[2].Anonymous.DescriptorTable.pDescriptorRanges = &range_uav;
        let (r_table, _) =
            root_signature_in(&host.device, &alt_table, &[base_sampler], false, true, &cache)
                .unwrap();
        assert_ne!(r_table.as_raw(), r1.as_raw());

        let neg_zero_sampler = sample_static_sampler(D3D12_FILTER_MIN_MAG_MIP_LINEAR.0 as u32, -0.0);
        let count_before_neg_zero = cache.read().unwrap().as_ref().unwrap().len();
        let _ =
            root_signature_in(&host.device, &base_params, &[neg_zero_sampler], false, true, &cache)
                .unwrap();
        assert_eq!(
            cache.read().unwrap().as_ref().unwrap().len(),
            count_before_neg_zero + 1,
            "-0.0 and +0.0 MipLODBias must have distinct bit-exact keys"
        );

        let biased_sampler = sample_static_sampler(D3D12_FILTER_MIN_MAG_MIP_LINEAR.0 as u32, -0.5);
        let (r_biased, _) =
            root_signature_in(&host.device, &base_params, &[biased_sampler], false, true, &cache)
                .unwrap();
        assert_ne!(r_biased.as_raw(), r1.as_raw());

        let point_sampler = sample_static_sampler(D3D12_FILTER_MIN_MAG_MIP_POINT.0 as u32, 0.0);
        let (r_point, _) =
            root_signature_in(&host.device, &base_params, &[point_sampler], false, true, &cache)
                .unwrap();
        assert_ne!(r_point.as_raw(), r1.as_raw());

        // Multi-device safety & ABA prevention: a standalone WARP device gets its own
        // `OPTIONS19` answer and its own `ID3D12RootSignature`, and retaining `_device`
        // keeps the device COM reference alive even after dropping the local handle.
        if let Ok(factory) = unsafe { CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0)) }
            && let Ok(adapter) = unsafe { factory.EnumWarpAdapter::<IDXGIAdapter>() }
        {
            let mut warp: Option<ID3D12Device> = None;
            if unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut warp) }.is_ok()
                && let Some(warp) = warp
                && warp.as_raw() != host.device.as_raw()
            {
                let host_opt19 = super::super::samplers::aniso_point_mip(&host.device);
                let warp_opt19 = super::super::samplers::aniso_point_mip(&warp);
                assert_eq!(super::super::samplers::aniso_point_mip(&host.device), host_opt19);
                assert_eq!(super::super::samplers::aniso_point_mip(&warp), warp_opt19);

                let (rw1, _) =
                    root_signature_in(&warp, &base_params, &[base_sampler], false, true, &cache)
                        .unwrap();
                let (rw2, _) =
                    root_signature_in(&warp, &base_params, &[base_sampler], false, true, &cache)
                        .unwrap();
                assert_eq!(rw1.as_raw(), rw2.as_raw());
                assert_ne!(rw1.as_raw(), r1.as_raw(), "WARP and hardware devices must never share COM root signatures");
                // Cache holds `_device` + `super::super::samplers` holds `HeldDevice`, so refcount > 1.
                assert!(crate::host::references(&warp) >= 2);
            }
        }
    }

    #[test]
    fn shared_root_signatures_load_from_library_and_execute_across_pso_switches() {
        use victory_shader::emit::{StaticSampler, UserData};
        use victory_shader::{HwStage, RootType};
        use windows::core::{HSTRING, Interface, PCWSTR};

        let Some(host) = host() else {
            return;
        };
        let Some(dxc) = dxc() else {
            eprintln!("no PC dxc.exe; skipped");
            return;
        };
        let dir = tempfile::tempdir().unwrap();

        // Two distinct compute shaders sharing the exact same root layout:
        // param 0: 4 root constants (`b0, space0`)
        // param 1: root CBV (`b1, space1`)
        // param 2: bounded 1-descriptor SRV table (`t2, space1`)
        // param 3: bounded 1-descriptor UAV table (`u3, space1`)
        let cs1_src = format!(
            "cbuffer U : register(b0, space{USER_DATA_SPACE}) {{ uint4 u; }};
             cbuffer C : register(b1, space{ROOT_CBV_SPACE}) {{ uint4 cb; }};
             ByteAddressBuffer Srv : register(t2, space{ROOT_CBV_SPACE});
             RWByteAddressBuffer Uav : register(u3, space{ROOT_CBV_SPACE});
             [numthreads(1, 1, 1)]
             void main() {{
                 Uav.Store(0, u.x * Srv.Load(0) + cb.x + u.y + 11u);
             }}"
        );
        let cs2_src = format!(
            "cbuffer U : register(b0, space{USER_DATA_SPACE}) {{ uint4 u; }};
             cbuffer C : register(b1, space{ROOT_CBV_SPACE}) {{ uint4 cb; }};
             ByteAddressBuffer Srv : register(t2, space{ROOT_CBV_SPACE});
             RWByteAddressBuffer Uav : register(u3, space{ROOT_CBV_SPACE});
             [numthreads(1, 1, 1)]
             void main() {{
                 Uav.Store(0, u.x + Srv.Load(0) * 3u + cb.x * 2u + u.y + 29u);
             }}"
        );
        let cs_other_src = format!(
            "cbuffer U : register(b0, space{USER_DATA_SPACE}) {{ uint4 u; }};
             cbuffer C : register(b1, space{ROOT_CBV_SPACE}) {{ uint4 cb; }};
             cbuffer C2 : register(b4, space{ROOT_CBV_SPACE}) {{ uint4 cb2; }};
             ByteAddressBuffer Srv : register(t2, space{ROOT_CBV_SPACE});
             RWByteAddressBuffer Uav : register(u3, space{ROOT_CBV_SPACE});
             [numthreads(1, 1, 1)]
             void main() {{
                 Uav.Store(0, u.x + cb.x + cb2.x + Srv.Load(0));
             }}"
        );

        let cs_user = vec![
            UserData { index: 0, slot: 0, root: RootType::Constants, dword: 0 },
            UserData { index: 1, slot: 0, root: RootType::Constants, dword: 1 },
        ];
        let cs1_dxil = compile_named(&dxc, dir.path(), "cs1", &cs1_src, "cs_6_6");
        let cs2_dxil = compile_named(&dxc, dir.path(), "cs2", &cs2_src, "cs_6_6");
        let cs_other_dxil = compile_named(&dxc, dir.path(), "cs_other", &cs_other_src, "cs_6_6");

        let cs1_shader = synthetic_shader(
            "compute",
            HwStage::Cs,
            "rootsig_test_cs1",
            cs1_dxil.clone(),
            cs_user.clone(),
            vec![1],
            vec![2],
            vec![3],
            Vec::new(),
            true,
        );
        let cs2_shader = synthetic_shader(
            "compute",
            HwStage::Cs,
            "rootsig_test_cs2",
            cs2_dxil.clone(),
            cs_user,
            vec![1],
            vec![2],
            vec![3],
            Vec::new(),
            true,
        );
        let cs_other_shader = synthetic_shader(
            "compute",
            HwStage::Cs,
            "rootsig_test_cs3",
            cs_other_dxil,
            vec![UserData { index: 0, slot: 0, root: RootType::Constants, dword: 0 }],
            vec![1, 4],
            vec![2],
            vec![3],
            Vec::new(),
            true,
        );

        let cs1 = HostPipeline::new(std::slice::from_ref(&cs1_shader), None).expect("cs1 PSO");
        let cs2 = HostPipeline::new(std::slice::from_ref(&cs2_shader), None).expect("cs2 PSO");
        let cs_other =
            HostPipeline::new(std::slice::from_ref(&cs_other_shader), None).expect("cs_other PSO");

        assert_ne!(cs1.pso.as_raw(), cs2.pso.as_raw());
        assert_eq!(
            cs1.root.as_raw(),
            cs2.root.as_raw(),
            "compute PSOs with identical root layouts must share ID3D12RootSignature"
        );
        assert_ne!(cs1.root.as_raw(), cs_other.root.as_raw());

        // Two distinct graphics PSOs sharing the exact same VS+PS root layout:
        // VS: 4 root constants (`b0, space0`) + root CBV (`b1, space1`)
        // PS: 4 root constants (`b0, space0`) + bounded SRV table (`t2, space1`)
        let vs_src = format!(
            "cbuffer U : register(b0, space{USER_DATA_SPACE}) {{ uint4 u; }};
             cbuffer C : register(b1, space{ROOT_CBV_SPACE}) {{ uint4 cb; }};
             struct Out {{
                 float4 pos : SV_Position;
                 nointerpolation uint val : TEXCOORD0;
             }};
             Out main(uint i : SV_VertexID) {{
                 Out o;
                 float2 p = float2((i << 1) & 2, i & 2);
                 o.pos = float4(p * 2.0 - 1.0, 0.5, 1.0);
                 o.val = u.x + cb.x;
                 return o;
             }}"
        );
        let ps1_src = format!(
            "cbuffer U : register(b0, space{USER_DATA_SPACE}) {{ uint4 u; }};
             ByteAddressBuffer Srv : register(t2, space{ROOT_CBV_SPACE});
             uint main(float4 pos : SV_Position, nointerpolation uint v : TEXCOORD0) : SV_Target {{
                 return v + u.x + Srv.Load(0) * 5u + 100u;
             }}"
        );
        let ps2_src = format!(
            "cbuffer U : register(b0, space{USER_DATA_SPACE}) {{ uint4 u; }};
             ByteAddressBuffer Srv : register(t2, space{ROOT_CBV_SPACE});
             uint main(float4 pos : SV_Position, nointerpolation uint v : TEXCOORD0) : SV_Target {{
                 return v * 3u + u.x + Srv.Load(0) * 7u + 200u;
             }}"
        );
        let ps_other_src = format!(
            "cbuffer U : register(b0, space{USER_DATA_SPACE}) {{ uint4 u; }};
             cbuffer Extra : register(b4, space{ROOT_CBV_SPACE}) {{ uint4 ex; }};
             ByteAddressBuffer Srv : register(t2, space{ROOT_CBV_SPACE});
             uint main(float4 pos : SV_Position, nointerpolation uint v : TEXCOORD0) : SV_Target {{
                 return v + u.x + ex.x + Srv.Load(0);
             }}"
        );

        let vs_dxil = compile_named(&dxc, dir.path(), "vs", &vs_src, "vs_6_6");
        let ps1_dxil = compile_named(&dxc, dir.path(), "ps1", &ps1_src, "ps_6_6");
        let ps2_dxil = compile_named(&dxc, dir.path(), "ps2", &ps2_src, "ps_6_6");
        let ps_other_dxil = compile_named(&dxc, dir.path(), "ps_other", &ps_other_src, "ps_6_6");

        let vs_shader = synthetic_shader(
            "vertex",
            HwStage::Vs,
            "rootsig_test_vs1",
            vs_dxil.clone(),
            vec![UserData { index: 0, slot: 0, root: RootType::Constants, dword: 0 }],
            vec![1],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            false,
        );
        let ps1_shader = synthetic_shader(
            "pixel",
            HwStage::Ps,
            "rootsig_test_ps1",
            ps1_dxil.clone(),
            vec![UserData { index: 0, slot: 0, root: RootType::Constants, dword: 1 }],
            Vec::new(),
            vec![2],
            Vec::new(),
            Vec::new(),
            false,
        );
        let ps2_shader = synthetic_shader(
            "pixel",
            HwStage::Ps,
            "rootsig_test_ps2",
            ps2_dxil.clone(),
            vec![UserData { index: 0, slot: 0, root: RootType::Constants, dword: 1 }],
            Vec::new(),
            vec![2],
            Vec::new(),
            Vec::new(),
            false,
        );
        let ps_other_shader = synthetic_shader(
            "pixel",
            HwStage::Ps,
            "rootsig_test_ps3",
            ps_other_dxil,
            vec![UserData { index: 0, slot: 0, root: RootType::Constants, dword: 1 }],
            vec![4],
            vec![2],
            Vec::new(),
            Vec::new(),
            false,
        );

        let mut g = GraphicsState::default();
        g.depth.DepthEnable = false.into();
        g.rasterizer.CullMode = D3D12_CULL_MODE_NONE;
        g.topology = D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE;
        g.targets.NumRenderTargets = 1;
        g.targets.RTFormats[0] = DXGI_FORMAT_R32_UINT;

        let gfx1 = HostPipeline::new(&[vs_shader.clone(), ps1_shader.clone()], Some(&g))
            .expect("gfx1 PSO");
        let gfx2 = HostPipeline::new(&[vs_shader.clone(), ps2_shader.clone()], Some(&g))
            .expect("gfx2 PSO");
        let gfx_other = HostPipeline::new(&[vs_shader.clone(), ps_other_shader], Some(&g))
            .expect("gfx_other PSO");

        assert_ne!(gfx1.pso.as_raw(), gfx2.pso.as_raw());
        assert_eq!(
            gfx1.root.as_raw(),
            gfx2.root.as_raw(),
            "graphics PSOs with identical root layouts must share ID3D12RootSignature"
        );
        assert_ne!(gfx1.root.as_raw(), gfx_other.root.as_raw());
        assert_ne!(gfx1.root.as_raw(), cs1.root.as_raw());

        // Static-sampler graphics PSOs also share their root signature (and exercise cached OPTIONS19).
        let static_samp = StaticSampler {
            filter: 0x55, // D3D12_FILTER_ANISOTROPIC
            address: [3, 3, 3],
            mip_lod_bias: 0.0,
            max_anisotropy: 8,
            comparison: 4,
            border: 0,
            min_lod: 0.0,
            max_lod: 12.0,
        };
        let ps_samp1 = synthetic_shader(
            "pixel",
            HwStage::Ps,
            "rootsig_samp_ps1",
            ps1_dxil.clone(),
            vec![UserData { index: 0, slot: 0, root: RootType::Constants, dword: 1 }],
            Vec::new(),
            vec![2],
            Vec::new(),
            vec![static_samp],
            false,
        );
        let ps_samp2 = synthetic_shader(
            "pixel",
            HwStage::Ps,
            "rootsig_samp_ps2",
            ps2_dxil.clone(),
            vec![UserData { index: 0, slot: 0, root: RootType::Constants, dword: 1 }],
            Vec::new(),
            vec![2],
            Vec::new(),
            vec![static_samp],
            false,
        );
        let gfx_samp1 =
            HostPipeline::new(&[vs_shader.clone(), ps_samp1], Some(&g)).expect("gfx_samp1 PSO");
        let gfx_samp2 =
            HostPipeline::new(&[vs_shader.clone(), ps_samp2], Some(&g)).expect("gfx_samp2 PSO");
        assert_ne!(gfx_samp1.pso.as_raw(), gfx_samp2.pso.as_raw());
        assert_eq!(gfx_samp1.root.as_raw(), gfx_samp2.root.as_raw());
        assert_ne!(gfx_samp1.root.as_raw(), gfx1.root.as_raw());

        // Verify `ID3D12PipelineLibrary` reload through `super::library` AND explicit
        // `StorePipeline` + `Serialize` + `CreatePipelineLibrary` + `Load*Pipeline`
        // with the shared `ID3D12RootSignature`.
        let loaded_before = super::super::library::LOADED.load(Ordering::Relaxed);
        let cs1_reloaded = HostPipeline::new(std::slice::from_ref(&cs1_shader), None).unwrap();
        let cs2_reloaded = HostPipeline::new(std::slice::from_ref(&cs2_shader), None).unwrap();
        let gfx1_reloaded =
            HostPipeline::new(&[vs_shader.clone(), ps1_shader], Some(&g)).unwrap();
        let gfx2_reloaded = HostPipeline::new(&[vs_shader, ps2_shader], Some(&g)).unwrap();
        assert_eq!(
            super::super::library::LOADED.load(Ordering::Relaxed),
            loaded_before + 4,
            "all 4 PSOs must reload from the process pipeline library with shared root signatures"
        );
        assert_eq!(cs1_reloaded.root.as_raw(), cs1.root.as_raw());
        assert_eq!(cs2_reloaded.root.as_raw(), cs1.root.as_raw());
        assert_eq!(gfx1_reloaded.root.as_raw(), gfx1.root.as_raw());
        assert_eq!(gfx2_reloaded.root.as_raw(), gfx1.root.as_raw());

        let dev1: ID3D12Device1 = host.device.cast().unwrap();
        let make_lib = |bytes: &[u8]| -> ID3D12PipelineLibrary1 {
            let ptr = if bytes.is_empty() {
                std::ptr::null()
            } else {
                bytes.as_ptr().cast()
            };
            let mut lib: Option<ID3D12PipelineLibrary> = None;
            // SAFETY: valid device and live serialized library slice.
            unsafe {
                (Interface::vtable(&dev1).CreatePipelineLibrary)(
                    Interface::as_raw(&dev1),
                    ptr,
                    bytes.len(),
                    &ID3D12PipelineLibrary::IID,
                    (&raw mut lib).cast(),
                )
                .ok()
                .unwrap();
            }
            lib.unwrap().cast().unwrap()
        };
        let lib_out = make_lib(&[]);
        for (n, pso) in [
            ("cs1", &cs1.pso),
            ("cs2", &cs2.pso),
            ("gfx1", &gfx1.pso),
            ("gfx2", &gfx2.pso),
        ] {
            let wide = HSTRING::from(n);
            // SAFETY: live PSOs stored under unique names.
            unsafe { lib_out.StorePipeline(PCWSTR(wide.as_ptr()), pso) }.unwrap();
        }
        // SAFETY: plain size query and serialization into matching buffer.
        let serialized = unsafe {
            let size = lib_out.GetSerializedSize();
            let mut buf = vec![0u8; size];
            lib_out.Serialize(&mut buf).unwrap();
            buf
        };
        let lib_in = make_lib(&serialized);
        for (n, dxil) in [("cs1", &cs1_dxil), ("cs2", &cs2_dxil)] {
            let wide = HSTRING::from(n);
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                // SAFETY: borrowed root signature pointer.
                pRootSignature: unsafe { std::mem::transmute_copy(&cs1.root) },
                CS: code(dxil),
                ..Default::default()
            };
            // SAFETY: live serialized library and matching shared root signature.
            let loaded_pso: ID3D12PipelineState =
                unsafe { lib_in.LoadComputePipeline(PCWSTR(wide.as_ptr()), &desc) }.unwrap();
            assert!(!loaded_pso.as_raw().is_null());
        }
        for (n, ps_dxil) in [("gfx1", &ps1_dxil), ("gfx2", &ps2_dxil)] {
            let wide = HSTRING::from(n);
            let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                // SAFETY: borrowed root signature pointer.
                pRootSignature: unsafe { std::mem::transmute_copy(&gfx1.root) },
                VS: code(&vs_dxil),
                PS: code(ps_dxil),
                BlendState: host_blend(&g.blend, &g.targets),
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
            // SAFETY: live serialized library and matching shared root signature.
            let loaded_pso: ID3D12PipelineState =
                unsafe { lib_in.LoadGraphicsPipeline(PCWSTR(wide.as_ptr()), &desc) }.unwrap();
            assert!(!loaded_pso.as_raw().is_null());
        }

        // Execute consecutive dispatches `cs1 -> cs2 -> cs1` AND consecutive draws
        // `gfx1 -> gfx2 -> gfx1` through `draw::State::bind`, verifying `host_roots`
        // stays equal (`Set*RootSignature` called once per bind point instead of 3 times),
        // `HostList::deferred.kept` deduplicates shared root signatures, zero InfoQueue
        // errors, and exact GPU readback results!
        let make_test_heap = |samplers: bool, count: u32| -> crate::object::This {
            #[repr(C)]
            struct RawHeapDesc {
                kind: u32,
                count: u32,
                flags: u32,
                nodes: u32,
                placement: [u64; 2],
            }
            let raw = RawHeapDesc {
                kind: u32::from(samplers),
                count,
                flags: 1,
                nodes: 1,
                placement: [0; 2],
            };
            let mut out = std::ptr::null();
            assert_eq!(
                crate::descriptor::create_heap(
                    std::ptr::null(),
                    (&raw const raw).cast(),
                    &crate::resource::PAGEABLE_IIDS[0],
                    &mut out,
                ),
                crate::object::S_OK
            );
            out
        };
        let view_heap = make_test_heap(false, 64);
        let sampler_heap = make_test_heap(true, 16);
        let buf_addr = |b: crate::object::This| {
            crate::object::slot::<extern "system" fn(crate::object::This) -> u64>(b, 0x58)(b)
        };
        let make_words_buf = |words: [u32; 4]| {
            let b = crate::resource::test_buffer(256);
            let addr = buf_addr(b);
            // SAFETY: `test_buffer(256)` maps 256 writable bytes at `addr`.
            unsafe { (addr as *mut [u32; 4]).write(words) };
            (b, addr)
        };

        let cbvs = [
            make_words_buf([100, 0, 0, 0]),
            make_words_buf([200, 0, 0, 0]),
            make_words_buf([300, 0, 0, 0]),
        ];
        let srvs = [
            make_words_buf([7, 0, 0, 0]),
            make_words_buf([9, 0, 0, 0]),
            make_words_buf([13, 0, 0, 0]),
        ];
        let uavs = [
            make_words_buf([0; 4]),
            make_words_buf([0; 4]),
            make_words_buf([0; 4]),
        ];

        // 4x1 R32_UINT render target + readback buffer for the 3 graphics draws.
        let rt = {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            let rd = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 4,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_R32_UINT,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                ..Default::default()
            };
            let mut out: Option<ID3D12Resource> = None;
            // SAFETY: valid render-target description.
            unsafe {
                host.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &rd,
                        D3D12_RESOURCE_STATE_RENDER_TARGET,
                        None,
                        &mut out,
                    )
                    .unwrap();
            }
            out.unwrap()
        };
        let readback = {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_READBACK,
                ..Default::default()
            };
            let rd = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 256,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
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

        let _ = host.log_messages();
        let mut state = crate::draw::State::default();
        state.set_heaps(&[view_heap, sampler_heap]);
        let list = host.new_list().unwrap();

        // Consecutive compute dispatches: cs1 -> cs2 -> cs1.
        let cs_calls = [
            (&cs1, [3u32, 5u32], 0usize),
            (&cs2, [4u32, 6u32], 1usize),
            (&cs1, [8u32, 2u32], 2usize),
        ];
        let mut kept_after_first_cs = 0;
        for (idx, (pso, consts, i)) in cs_calls.iter().enumerate() {
            state.set_constants(true, 0, consts, 0);
            state.set_view(true, 1, cbvs[*i].1);
            state.set_view(true, 2, srvs[*i].1);
            state.set_view(true, 3, uavs[*i].1);
            assert!(state.bind(&list, pso, true), "compute bind {idx} failed");
            assert_eq!(state.host_roots()[1], cs1.root.as_raw() as usize);
            assert_eq!(
                state.root_binds[1], 1,
                "SetComputeRootSignature must be called only once across cs1 -> cs2 -> cs1"
            );
            if idx == 0 {
                kept_after_first_cs = list.kept_len();
            } else if idx == 1 {
                assert_eq!(
                    list.kept_len(),
                    kept_after_first_cs + 1,
                    "binding cs2 must keep cs2.pso without duplicating the shared root signature"
                );
            } else {
                assert_eq!(list.kept_len(), kept_after_first_cs + 1);
            }
            // SAFETY: open command list with compute pipeline and root arguments bound.
            unsafe { list.list.Dispatch(1, 1, 1) };
        }

        // Consecutive graphics draws: gfx1 -> gfx2 -> gfx1 into pixels x = 0, 1, 2.
        let rtv = host.rtv(Some(&rt), None);
        // SAFETY: open command list and live RTV.
        unsafe {
            list.list.OMSetRenderTargets(1, Some(&rtv), false, None);
            list.list.IASetPrimitiveTopology(
                windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
            );
        }
        let gfx_calls = [
            (&gfx1, [10u32, 20u32], 0usize),
            (&gfx2, [12u32, 25u32], 1usize),
            (&gfx1, [15u32, 30u32], 2usize),
        ];
        for (idx, (pso, consts, i)) in gfx_calls.iter().enumerate() {
            state.set_constants(false, 0, consts, 0);
            state.set_view(false, 1, cbvs[*i].1);
            state.set_view(false, 2, srvs[*i].1);
            assert!(state.bind(&list, pso, false), "graphics bind {idx} failed");
            assert_eq!(state.host_roots()[0], gfx1.root.as_raw() as usize);
            assert_eq!(
                state.root_binds[0], 1,
                "SetGraphicsRootSignature must be called only once across gfx1 -> gfx2 -> gfx1"
            );
            let vp = D3D12_VIEWPORT {
                TopLeftX: idx as f32,
                TopLeftY: 0.0,
                Width: 1.0,
                Height: 1.0,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            let sc = windows::Win32::Foundation::RECT {
                left: idx as i32,
                top: 0,
                right: idx as i32 + 1,
                bottom: 1,
            };
            // SAFETY: open command list with graphics pipeline and root arguments bound.
            unsafe {
                list.list.RSSetViewports(&[vp]);
                list.list.RSSetScissorRects(&[sc]);
                list.list.DrawInstanced(3, 1, 0, 0);
            }
        }

        // Copy the 4x1 R32_UINT target to readback.
        // SAFETY: open command list and live resources.
        unsafe {
            list.list.ResourceBarrier(&[crate::host::transition(
                &rt,
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
                            Format: DXGI_FORMAT_R32_UINT,
                            Width: 4,
                            Height: 1,
                            Depth: 1,
                            RowPitch: 256,
                        },
                    },
                },
            };
            let src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&rt),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            list.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        }

        list.close();
        host.execute(&[&list]);
        state.tables_done();

        assert_eq!(host.log_messages(), None, "unexpected D3D12 InfoQueue error");
        if let Ok(iq) = host.device.cast::<ID3D12InfoQueue>() {
            // SAFETY: plain query on live InfoQueue.
            assert_eq!(unsafe { iq.GetNumStoredMessages() }, 0);
        }

        // Verify exact compute UAV readbacks:
        // call 0 (cs1): 3 * 7 + 100 + 5 + 11 = 137
        // call 1 (cs2): 4 + 9 * 3 + 200 * 2 + 6 + 29 = 466
        // call 2 (cs1): 8 * 13 + 300 + 2 + 11 = 417
        let read_uav = |i: usize| unsafe { *(uavs[i].1 as *const u32) };
        assert_eq!([read_uav(0), read_uav(1), read_uav(2)], [137, 466, 417]);

        // Verify exact graphics R32_UINT pixel readbacks:
        // call 0 (gfx1): (10 + 100) + 20 + 7 * 5 + 100 = 265
        // call 1 (gfx2): (12 + 200) * 3 + 25 + 9 * 7 + 200 = 924
        // call 2 (gfx1): (15 + 300) + 30 + 13 * 5 + 100 = 510
        let mut ptr = std::ptr::null_mut();
        // SAFETY: GPU execution finished; readback holds 256 bytes.
        let pixels = unsafe {
            readback.Map(0, None, Some(&mut ptr)).unwrap();
            let out = *ptr.cast::<[u32; 3]>();
            readback.Unmap(0, None);
            out
        };
        assert_eq!(pixels, [265, 924, 510]);

        let release = |o: crate::object::This| {
            crate::object::slot::<extern "system" fn(crate::object::This) -> u32>(o, 0x10)(o)
        };
        for (b, _) in cbvs.into_iter().chain(srvs).chain(uavs) {
            release(b);
        }
        release(view_heap);
        release(sampler_heap);
    }

    #[test]
    fn benchmark_500_host_pipeline_root_signatures() {
        let Some(host) = host() else {
            return;
        };
        // Build 500 realistic PSO root-signature specifications spanning 25 unique
        // root layouts (graphics + compute, user_data constants, root CBVs, bounded
        // SRV/UAV descriptor tables, and static samplers).
        const PSOS: usize = 500;
        const LAYOUTS: usize = 25;

        struct Layout {
            compute: bool,
            _ranges: Box<[D3D12_DESCRIPTOR_RANGE1]>,
            params: Vec<D3D12_ROOT_PARAMETER1>,
            raw_samplers: Vec<victory_shader::emit::StaticSampler>,
        }

        let layouts: Vec<Layout> = (0..LAYOUTS)
            .map(|k| {
                let compute = k % 5 == 0;
                let vis_a = if compute {
                    D3D12_SHADER_VISIBILITY_ALL
                } else if k % 3 == 0 {
                    D3D12_SHADER_VISIBILITY_MESH
                } else {
                    D3D12_SHADER_VISIBILITY_VERTEX
                };
                let ranges: Box<[D3D12_DESCRIPTOR_RANGE1]> = (0..((k % 4) + 1) as u32)
                    .map(|srv| D3D12_DESCRIPTOR_RANGE1 {
                        RangeType: if compute && srv == 0 {
                            D3D12_DESCRIPTOR_RANGE_TYPE_UAV
                        } else {
                            D3D12_DESCRIPTOR_RANGE_TYPE_SRV
                        },
                        NumDescriptors: 1,
                        BaseShaderRegister: 4 + srv,
                        RegisterSpace: ROOT_CBV_SPACE,
                        Flags: D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE
                            | D3D12_DESCRIPTOR_RANGE_FLAG_DATA_VOLATILE,
                        OffsetInDescriptorsFromTableStart: 0,
                    })
                    .collect();
                let mut params = Vec::new();
                params.push(D3D12_ROOT_PARAMETER1 {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                    Anonymous: D3D12_ROOT_PARAMETER1_0 {
                        Constants: D3D12_ROOT_CONSTANTS {
                            ShaderRegister: 0,
                            RegisterSpace: USER_DATA_SPACE,
                            Num32BitValues: ((k as u32 % 8) + 1) * 4,
                        },
                    },
                    ShaderVisibility: vis_a,
                });
                for cbv in 0..(k % 3) as u32 {
                    params.push(D3D12_ROOT_PARAMETER1 {
                        ParameterType: D3D12_ROOT_PARAMETER_TYPE_CBV,
                        Anonymous: D3D12_ROOT_PARAMETER1_0 {
                            Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                                ShaderRegister: cbv,
                                RegisterSpace: ROOT_CBV_SPACE,
                                Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
                            },
                        },
                        ShaderVisibility: vis_a,
                    });
                }
                for r in &ranges {
                    params.push(D3D12_ROOT_PARAMETER1 {
                        ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                        Anonymous: D3D12_ROOT_PARAMETER1_0 {
                            DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE1 {
                                NumDescriptorRanges: 1,
                                pDescriptorRanges: r,
                            },
                        },
                        ShaderVisibility: if compute {
                            D3D12_SHADER_VISIBILITY_ALL
                        } else {
                            D3D12_SHADER_VISIBILITY_PIXEL
                        },
                    });
                }
                let raw_samplers: Vec<_> = (0..(k % 4))
                    .map(|s| victory_shader::emit::StaticSampler {
                        filter: if s % 2 == 0 { 0x55 } else { 0x15 },
                        address: [1 + (s as u32 % 3); 3],
                        mip_lod_bias: 0.0,
                        max_anisotropy: 8,
                        comparison: 4,
                        border: 0,
                        min_lod: 0.0,
                        max_lod: 13.0,
                    })
                    .collect();
                Layout {
                    compute,
                    _ranges: ranges,
                    params,
                    raw_samplers,
                }
            })
            .collect();

        // `keep_alive = true`: live gameplay (`Pending::made` holds every `HostPipeline`).
        // `keep_alive = false`: `victory warm` (`known::make` drops each `HostPipeline` right
        // after compiling it, which without `ROOT_SIGNATURES` destroys the driver root
        // signature on every PSO and forces full driver recreation on the next!).
        let uncached_batch = |keep_alive: bool| {
            let dummy: std::sync::RwLock<Option<RootSigMap>> = std::sync::RwLock::new(None);
            let mut out = Vec::with_capacity(if keep_alive { PSOS } else { 0 });
            let t0 = std::time::Instant::now();
            for i in 0..PSOS {
                let l = &layouts[i % LAYOUTS];
                let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS19::default();
                // SAFETY: matches the uncached feature query previously run per PSO.
                let opt19 = unsafe {
                    host.device.CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS19,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                }
                .is_ok()
                    && options.AnisoFilterWithPointMipSupported.as_bool();
                let samplers: Vec<_> = l
                    .raw_samplers
                    .iter()
                    .enumerate()
                    .map(|(n, s)| D3D12_STATIC_SAMPLER_DESC {
                        Filter: D3D12_FILTER(s.filter_for_host(opt19) as i32),
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
                        ShaderVisibility: if l.compute {
                            D3D12_SHADER_VISIBILITY_ALL
                        } else {
                            D3D12_SHADER_VISIBILITY_PIXEL
                        },
                    })
                    .collect();
                let made =
                    root_signature_in(&host.device, &l.params, &samplers, l.compute, false, &dummy)
                        .unwrap();
                if keep_alive {
                    out.push(made);
                } else {
                    std::hint::black_box(made);
                }
            }
            let ms = t0.elapsed().as_secs_f64() * 1e3;
            drop(out);
            ms
        };

        let cached_batch = |cache: &std::sync::RwLock<Option<RootSigMap>>| {
            let mut out = Vec::with_capacity(PSOS);
            let t0 = std::time::Instant::now();
            for i in 0..PSOS {
                let l = &layouts[i % LAYOUTS];
                let opt19 = super::super::samplers::aniso_point_mip(&host.device);
                let samplers: Vec<_> = l
                    .raw_samplers
                    .iter()
                    .enumerate()
                    .map(|(n, s)| D3D12_STATIC_SAMPLER_DESC {
                        Filter: D3D12_FILTER(s.filter_for_host(opt19) as i32),
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
                        ShaderVisibility: if l.compute {
                            D3D12_SHADER_VISIBILITY_ALL
                        } else {
                            D3D12_SHADER_VISIBILITY_PIXEL
                        },
                    })
                    .collect();
                out.push(
                    root_signature_in(&host.device, &l.params, &samplers, l.compute, true, cache)
                        .unwrap(),
                );
            }
            let ms = t0.elapsed().as_secs_f64() * 1e3;
            let unique: std::collections::HashSet<usize> =
                out.iter().map(|(r, _)| windows::core::Interface::as_raw(r) as usize).collect();
            assert_eq!(unique.len(), LAYOUTS);
            drop(out);
            ms
        };

        // Measure `uncached_batch` BEFORE populating `warm_cache` so D3D12Core's internal
        // live-object table does not hide cold driver root-signature creation!
        let _ = uncached_batch(false);
        const ROUNDS: usize = 9;
        let mut off_drop_ms = Vec::with_capacity(ROUNDS);
        let mut off_keep_ms = Vec::with_capacity(ROUNDS);
        let mut cold_ms = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            off_drop_ms.push(uncached_batch(false));
            off_keep_ms.push(uncached_batch(true));
            let fresh: std::sync::RwLock<Option<RootSigMap>> = std::sync::RwLock::new(None);
            cold_ms.push(cached_batch(&fresh));
        }
        let warm_cache: std::sync::RwLock<Option<RootSigMap>> = std::sync::RwLock::new(None);
        let _ = cached_batch(&warm_cache);
        let mut warm_ms = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            warm_ms.push(cached_batch(&warm_cache));
        }

        off_drop_ms.sort_by(f64::total_cmp);
        off_keep_ms.sort_by(f64::total_cmp);
        cold_ms.sort_by(f64::total_cmp);
        warm_ms.sort_by(f64::total_cmp);
        let (med_off_drop, med_off_keep, med_cold, med_warm) = (
            off_drop_ms[ROUNDS / 2],
            off_keep_ms[ROUNDS / 2],
            cold_ms[ROUNDS / 2],
            warm_ms[ROUNDS / 2],
        );
        eprintln!(
            "bench 500 HostPipeline root signatures (25 unique layouts, {ROUNDS} rounds):\n  \
             VICTORY_ROOT_CACHE=0 (victory warm drop-each): median {med_off_drop:.3} ms\n  \
             VICTORY_ROOT_CACHE=0 (live keep-alive):        median {med_off_keep:.3} ms\n  \
             VICTORY_ROOT_CACHE=1 (cold 25 misses+475 hits): median {med_cold:.3} ms ({:.2}x vs drop-each, {:.2}x vs keep-alive)\n  \
             VICTORY_ROOT_CACHE=1 (warm 500 hits):           median {med_warm:.3} ms ({:.1}x vs drop-each, {:.1}x vs keep-alive)",
            med_off_drop / med_cold.max(1e-6),
            med_off_keep / med_cold.max(1e-6),
            med_off_drop / med_warm.max(1e-6),
            med_off_keep / med_warm.max(1e-6),
        );
        assert!(
            med_cold < med_off_keep && med_cold < med_off_drop,
            "deduplicated batch ({med_cold:.3} ms) must beat uncached ({med_off_keep:.3} / {med_off_drop:.3} ms)"
        );
    }
}
