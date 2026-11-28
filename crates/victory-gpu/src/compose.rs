//! Two presented planes as one image: the console's display puts the second
//! (the interface) over the first (the scene), each scaled to the screen.
//! The game presents so on Series S.

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{PCSTR, s};

use crate::host::Host;

const SOURCE: &str = r#"
#define RS "RootConstants(b0, num32BitConstants=6), DescriptorTable(SRV(t0, numDescriptors=2), visibility=SHADER_VISIBILITY_PIXEL), StaticSampler(s0, filter=FILTER_MIN_MAG_MIP_LINEAR, addressU=TEXTURE_ADDRESS_CLAMP, addressV=TEXTURE_ADDRESS_CLAMP)"
// The share of the screen width the interface may use (its aspect over the
// output's): 0 leaves both planes full-frame exactly as before; below 1 the
// interface is kept at its console aspect and the world keeps the bars.
// The rest is the HDR10 output (VICTORY_HDR): out_pq 0 keeps the original
// gamma-space blend; 1 blends in linear light and writes PQ/Rec.2020.
// Spaces: 0 gamma 2.2 / Rec.709, 1 linear scRGB (1.0 = 80 nits), 2 PQ / Rec.2020,
// 3 linear Rec.709 with 1.0 = paper white (what an _SRGB view samples as).
cbuffer Fit : register(b0) { float ui_fit; uint out_pq; uint base_space; uint over_space; float paper_nits; uint over_on; };
Texture2D<float4> scene : register(t0);
Texture2D<float4> over : register(t1);
SamplerState smooth_ : register(s0);
static const float m1 = 2610.0 / 16384.0;
static const float m2 = 2523.0 / 4096.0 * 128.0;
static const float c1 = 3424.0 / 4096.0;
static const float c2 = 2413.0 / 4096.0 * 32.0;
static const float c3 = 2392.0 / 4096.0 * 32.0;
// Rec.709 primaries to Rec.2020, row-major, applied as mul(M, c).
static const float3x3 p709_to_p2020 = float3x3(
    0.6274039, 0.3292830, 0.0433131,
    0.0690973, 0.9195404, 0.0113623,
    0.0163914, 0.0880133, 0.8955953);
// One colour of a plane to linear light in Rec.2020, in nits.
float3 decode(float3 c, uint space) {
    // half-float planes may carry negatives or NaN; neither is light
    c = isnan(c) ? float3(0, 0, 0) : max(c, 0);
    if (space == 2) {
        float3 p = pow(saturate(c), 1.0 / m2);
        return pow(max(p - c1, 0) / (c2 - c3 * p), 1.0 / m1) * 10000.0;
    }
    if (space == 1) {
        return mul(p709_to_p2020, c * 80.0);
    }
    if (space == 3) {
        return mul(p709_to_p2020, c * paper_nits);
    }
    // dxgi's G22 is a pure 2.2 power, not the piecewise srgb curve
    return mul(p709_to_p2020, pow(c, 2.2) * paper_nits);
}
float3 pq_encode(float3 nits) {
    float3 p = pow(saturate(nits / 10000.0), m1);
    return pow((c1 + c2 * p) / (1.0 + c3 * p), m2);
}
struct V { float4 at : SV_Position; float2 uv : TEXCOORD0; };
[RootSignature(RS)]
V vs(uint id : SV_VertexID) {
    V v;
    v.uv = float2((id << 1) & 2, id & 2);
    v.at = float4(v.uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return v;
}
[RootSignature(RS)]
float4 ps(V v) : SV_Target {
    float4 s = scene.SampleLevel(smooth_, v.uv, 0);
    float2 ouv = v.uv;
    float inside = 1.0;
    if (ui_fit > 0.0 && ui_fit < 1.0) {
        // Map the centered ui_fit-wide band onto the interface's whole width;
        // outside the band the plane reads as absent and the scene stands.
        ouv.x = (v.uv.x - 0.5) / ui_fit + 0.5;
        inside = (ouv.x >= 0.0 && ouv.x <= 1.0) ? 1.0 : 0.0;
    }
    float4 o = over.SampleLevel(smooth_, ouv, 0) * inside;
    if (out_pq == 0) {
        return float4(saturate(o.rgb + s.rgb * (1 - saturate(o.a))), 1);
    }
    float a = over_on != 0 ? saturate(o.a) : 0;
    float3 over_nits = float3(0, 0, 0);
    if (over_on != 0) {
        if (over_space == 1 || over_space == 3) {
            // linear light is homogeneous: the premultiplied value decodes
            // as it is, additive pixels (alpha 0, colour above 0) included
            over_nits = decode(o.rgb, over_space);
        } else {
            // the interface plane is premultiplied; a gamma curve over a
            // premultiplied value would darken its edges, so decode the
            // straight colour and scale. whatever premultiplied colour is
            // beyond what the alpha covers is additive light, as in the SDR
            // branch, and is decoded on its own.
            float3 covered = saturate(o.rgb / max(a, 1e-4));
            float3 extra = max(o.rgb - covered * a, 0);
            over_nits = decode(covered, over_space) * a + decode(extra, over_space);
        }
    }
    float3 scene_nits = decode(s.rgb, base_space);
    return float4(pq_encode(over_nits + scene_nits * (1 - a)), 1);
}
"#;

pub(crate) struct Composer {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    views: Vec<ID3D12DescriptorHeap>,
    step: u32,
}
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Composer {}
// SAFETY: as above.
unsafe impl Sync for Composer {}

fn compile(entry: PCSTR, target: PCSTR) -> windows::core::Result<ID3DBlob> {
    let mut code: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; the out pointer is
    // ours.
    unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            s!("compose"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            None,
        )?;
    }
    code.ok_or_else(windows::core::Error::empty)
}

/// The aspect the interface is kept at on a wider output (`VICTORY_UI_LETTERBOX=16x9`, or a bare ratio; `0` or anything unparseable: none), read once. Series X's interface composite (`draw.rs`) and Series S's two-plane composer both use it.
///
/// unset leaves both paths exactly as they were: no band, so the composer's
/// fit constant stays 0 and the shader takes its original path.
pub(crate) fn ui_aspect() -> Option<f32> {
    static ASPECT: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
    *ASPECT.get_or_init(|| {
        std::env::var("VICTORY_UI_LETTERBOX")
            .ok()
            .and_then(|v| {
                let v = v.trim().to_owned();
                match v.split_once(['x', 'X']) {
                    Some((w, h)) => {
                        Some(w.trim().parse::<f32>().ok()? / h.trim().parse::<f32>().ok()?)
                    }
                    None => v.parse::<f32>().ok(),
                }
            })
            // a zero, negative, nan or infinite ratio is a way of saying no
            .filter(|a| a.is_finite() && *a > 0.0)
    })
}

/// The pixels of a `width` x `height` output the interface fills when it is kept at its aspect: (left, band width). `None` when no aspect is asked for, or the output is not wider than it.
pub(crate) fn ui_band(width: u32, height: u32) -> Option<(u32, u32)> {
    band(ui_aspect()?, width, height)
}

/// The band maths on its own so it can be tested without the environment. the
/// band is rounded to whole pixels and centred; an output no wider than the
/// band has nothing to letterbox.
fn band(aspect: f32, width: u32, height: u32) -> Option<(u32, u32)> {
    if width == 0 || height == 0 || !aspect.is_finite() || aspect <= 0.0 {
        return None;
    }
    let band = ((height as f32 * aspect).round() as u32).max(1);
    if band >= width {
        return None;
    }
    Some(((width - band) / 2, band))
}

impl Composer {
    pub(crate) fn new(host: &Host, format: DXGI_FORMAT) -> windows::core::Result<Self> {
        let (vs, ps) = (
            compile(s!("vs"), s!("vs_5_1"))?,
            compile(s!("ps"), s!("ps_5_1"))?,
        );
        // SAFETY: the blobs' own bytes, alive through the calls.
        unsafe {
            let bytes = |b: &ID3DBlob| {
                std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize())
            };
            let device = &host.device;
            let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes(&vs))?;
            let code = |b: &ID3DBlob| D3D12_SHADER_BYTECODE {
                pShaderBytecode: b.GetBufferPointer(),
                BytecodeLength: b.GetBufferSize(),
            };
            let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                VS: code(&vs),
                PS: code(&ps),
                SampleMask: u32::MAX,
                RasterizerState: D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    ..Default::default()
                },
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                NumRenderTargets: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            };
            desc.RTVFormats[0] = format;
            desc.BlendState.RenderTarget[0].RenderTargetWriteMask =
                D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
            let pso = device.CreateGraphicsPipelineState(&desc)?;
            Ok(Composer {
                root,
                pso,
                views: Vec::new(),
                step: device
                    .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV),
            })
        }
    }

    /// Each presenter command-list slot owns its own pair of SRVs. The swap
    /// chain may have several outstanding presents; the slot is selected by
    /// the completion fence, and its descriptors are only overwritten on reuse.
    pub(crate) fn record(
        &mut self,
        host: &Host,
        slot: usize,
        list: &ID3D12GraphicsCommandList,
        planes: [(&ID3D12Resource, DXGI_FORMAT); 2],
        back: &ID3D12Resource,
        (width, height): (u32, u32),
        hdr: Option<crate::hdr::Compose>,
    ) -> windows::core::Result<()> {
        while self.views.len() <= slot {
            let heap = unsafe {
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                        NumDescriptors: 2,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })?
            };
            self.views.push(heap);
        }
        let views = &self.views[slot];
        // SAFETY: an open list; the views are made in slots no frame in flight
        // reads; live textures.
        unsafe {
            let (cpu, gpu) = (
                views.GetCPUDescriptorHandleForHeapStart(),
                views.GetGPUDescriptorHandleForHeapStart(),
            );
            for (i, plane) in planes.iter().enumerate() {
                let at = D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: cpu.ptr + (i * self.step as usize),
                };
                let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: plane.1,
                    ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture2D: D3D12_TEX2D_SRV {
                            MipLevels: 1,
                            ..Default::default()
                        },
                    },
                };
                host.device
                    .CreateShaderResourceView(Some(plane.0), Some(&desc), at);
            }
            let rtv = host.rtv(Some(back), None);
            list.OMSetRenderTargets(1, Some(&rtv), false, None);
            list.SetDescriptorHeaps(&[Some(views.clone())]);
            list.SetGraphicsRootSignature(&self.root);
            list.SetPipelineState(&self.pso);
            // The band the interface keeps at its console aspect on this
            // output: its share of the width (< 1 only on a wider screen),
            // 0 to leave the shader on its original path.
            let fit = ui_band(width, height).map_or(0.0, |(_, band)| band as f32 / width as f32);
            // (the same band maths places the series x interface draw.)
            // the six constants of the shader's Fit block; all zero but the fit
            // with hdr off, which leaves the shader on its original path. with
            // hdr on and no interface plane the caller passes the base twice.
            let constants: [u32; 6] = match hdr {
                Some(h) => [
                    fit.to_bits(),
                    1,
                    h.base as u32,
                    h.over as u32,
                    h.paper_nits.to_bits(),
                    h.over_on as u32,
                ],
                None => [fit.to_bits(), 0, 0, 0, 0, 0],
            };
            // SAFETY: the pointer is to six live u32, as many as the root signature declares.
            list.SetGraphicsRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
            list.SetGraphicsRootDescriptorTable(1, gpu);
            list.IASetPrimitiveTopology(
                windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
            );
            list.RSSetViewports(&[D3D12_VIEWPORT {
                Width: width as f32,
                Height: height as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]);
            list.RSSetScissorRects(&[windows::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            }]);
            list.DrawInstanced(3, 1, 0, 0);
        }
        Ok(())
    }
}

/// Select a legal float-sampled view of a display plane, preserving a typed
/// resource's format. Typeless resources use the corresponding display type.
pub(crate) fn view(d: &D3D12_RESOURCE_DESC) -> Option<DXGI_FORMAT> {
    if d.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D
        || d.DepthOrArraySize != 1
        || d.SampleDesc.Count != 1
        || d.Flags.contains(D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE)
    {
        return None;
    }
    match d.Format.0 {
        9 => Some(DXGI_FORMAT_R16G16B16A16_FLOAT),
        23 => Some(DXGI_FORMAT_R10G10B10A2_UNORM),
        27 => Some(DXGI_FORMAT_R8G8B8A8_UNORM),
        90 => Some(DXGI_FORMAT_B8G8R8A8_UNORM),
        10 | 11 | 13 | 24 | 26 | 28 | 29 | 31 | 67 | 87 | 91 => Some(d.Format),
        _ => None,
    }
}

#[cfg(all(test, feature = "diagnostics"))]
mod tests {
    use super::*;

    #[test]
    fn composes_scaled_premultiplied_plane_on_gpu() {
        let Some(host) = crate::host::host() else {
            eprintln!("skipping display-plane readback: no D3D12 device");
            return;
        };
        let format = DXGI_FORMAT_R8G8B8A8_UNORM;
        let mut compositor = Composer::new(host, format).unwrap();
        let list = host.new_list().unwrap();
        let texture = |width: u64| -> ID3D12Resource {
            let mut resource = None;
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: width,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                ..Default::default()
            };
            unsafe {
                host.device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES {
                            Type: D3D12_HEAP_TYPE_DEFAULT,
                            ..Default::default()
                        },
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_RENDER_TARGET,
                        None,
                        &mut resource,
                    )
                    .unwrap()
            };
            resource.unwrap()
        };
        // A half-red base at one resolution. The overlay at output resolution
        // contains transparent, half-opaque green and fully opaque blue pixels.
        let base = texture(1);
        let overlay = texture(3);
        let output = texture(3);
        let target = D3D12_RESOURCE_STATE_RENDER_TARGET;
        let read = D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;
        let cmds = &list.list;
        unsafe {
            cmds.ClearRenderTargetView(host.rtv(Some(&base), None), &[0.5, 0.0, 0.0, 1.0], None);
            let overlay_rtv = host.rtv(Some(&overlay), None);
            let colors = [
                [0.0, 0.0, 0.0, 0.0],
                [0.0, 0.5, 0.0, 0.5],
                [0.0, 0.0, 1.0, 1.0],
            ];
            for (i, color) in colors.iter().enumerate() {
                let rect = windows::Win32::Foundation::RECT {
                    left: i as i32,
                    right: i as i32 + 1,
                    top: 0,
                    bottom: 1,
                };
                cmds.ClearRenderTargetView(overlay_rtv, color, Some(&[rect]));
            }
            cmds.ResourceBarrier(&[
                crate::host::transition(&base, target, read),
                crate::host::transition(&overlay, target, read),
            ]);
            compositor
                .record(
                    host,
                    0,
                    cmds,
                    [(&base, format), (&overlay, format)],
                    &output,
                    (3, 1),
                    None,
                )
                .unwrap();
            cmds.ResourceBarrier(&[crate::host::transition(
                &output,
                target,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            let (buffer, footprint) =
                crate::capture::read_back(&host.device, cmds, &output).unwrap();
            list.close();
            host.execute(&[&list]);
            host.device.GetDeviceRemovedReason().unwrap();
            let mut mapped = std::ptr::null_mut();
            buffer.Map(0, None, Some(&mut mapped)).unwrap();
            let start = mapped.cast::<u8>().add(footprint.Offset as usize);
            let pixels: [u8; 12] = std::slice::from_raw_parts(start, 12).try_into().unwrap();
            buffer.Unmap(0, None);
            for (actual, expected) in pixels
                .iter()
                .zip([128, 0, 0, 255, 64, 128, 0, 255, 0, 0, 255, 255])
            {
                assert!(
                    actual.abs_diff(expected) <= 1,
                    "unexpected composite {pixels:?}"
                );
            }
            eprintln!("D3D12 display-plane readback RGBA: {pixels:?}");
        }
    }

    /// the encoder the shader implements, for the expected code of a constant.
    fn pq_encode(nits: f32) -> f32 {
        let (m1, m2) = (2610.0 / 16384.0, 2523.0 / 4096.0 * 128.0);
        let (c1, c2, c3) = (3424.0 / 4096.0, 2413.0 / 4096.0 * 32.0, 2392.0 / 4096.0 * 32.0);
        let p = (nits / 10000.0f32).clamp(0.0, 1.0).powf(m1);
        ((c1 + c2 * p) / (1.0 + c3 * p)).powf(m2)
    }

    #[test]
    fn encodes_sdr_white_as_paper_white_pq_on_gpu() {
        let Some(host) = crate::host::host() else {
            eprintln!("skipping hdr10 compose readback: no D3D12 device");
            return;
        };
        let texture = |format: DXGI_FORMAT| -> ID3D12Resource {
            let mut resource = None;
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 1,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                ..Default::default()
            };
            // SAFETY: a valid description and an out pointer of ours.
            unsafe {
                host.device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES {
                            Type: D3D12_HEAP_TYPE_DEFAULT,
                            ..Default::default()
                        },
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_RENDER_TARGET,
                        None,
                        &mut resource,
                    )
                    .unwrap()
            };
            resource.unwrap()
        };
        let sdr = DXGI_FORMAT_R8G8B8A8_UNORM;
        let out = DXGI_FORMAT_R10G10B10A2_UNORM;
        let mut compositor = Composer::new(host, out).unwrap();
        let list = host.new_list().unwrap();
        let (base, output) = (texture(sdr), texture(out));
        let cmds = &list.list;
        let target = D3D12_RESOURCE_STATE_RENDER_TARGET;
        // SAFETY: an open list over textures made above, alive to the end.
        unsafe {
            cmds.ClearRenderTargetView(host.rtv(Some(&base), None), &[1.0, 1.0, 1.0, 1.0], None);
            cmds.ResourceBarrier(&[crate::host::transition(
                &base,
                target,
                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            )]);
            let hdr = crate::hdr::Compose {
                base: crate::hdr::Space::Gamma22,
                over: crate::hdr::Space::Gamma22,
                over_on: false,
                paper_nits: 203.0,
            };
            // no interface plane: the base stands in for both views
            compositor
                .record(
                    host,
                    0,
                    cmds,
                    [(&base, sdr), (&base, sdr)],
                    &output,
                    (1, 1),
                    Some(hdr),
                )
                .unwrap();
            cmds.ResourceBarrier(&[crate::host::transition(
                &output,
                target,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            let (buffer, footprint) =
                crate::capture::read_back(&host.device, cmds, &output).unwrap();
            list.close();
            host.execute(&[&list]);
            host.device.GetDeviceRemovedReason().unwrap();
            let mut mapped = std::ptr::null_mut();
            buffer.Map(0, None, Some(&mut mapped)).unwrap();
            let start = mapped.cast::<u8>().add(footprint.Offset as usize);
            let word = u32::from_le_bytes(std::slice::from_raw_parts(start, 4).try_into().unwrap());
            buffer.Unmap(0, None);
            // white through the 709 to 2020 matrix keeps rows summing to one,
            // so the red channel lands on the paper white level
            let red = (word & 0x3ff) as f32;
            let expected = pq_encode(203.0) * 1023.0;
            assert!(
                (red - expected).abs() <= 2.0,
                "red code {red}, expected {expected}"
            );
        }
    }
}

#[cfg(test)]
mod band_tests {
    use super::band;

    const WIDE: f32 = 16.0 / 9.0;

    #[test]
    fn centres_the_interface_on_a_wider_output() {
        assert_eq!(band(WIDE, 5120, 1440), Some((1280, 2560)));
        assert_eq!(band(WIDE, 3440, 1440), Some((440, 2560)));
    }

    #[test]
    fn leaves_an_output_no_wider_than_the_interface_alone() {
        assert_eq!(band(WIDE, 2560, 1440), None);
        assert_eq!(band(WIDE, 1920, 1440), None);
    }

    #[test]
    fn refuses_garbled_inputs() {
        assert_eq!(band(0.0, 5120, 1440), None);
        assert_eq!(band(f32::NAN, 5120, 1440), None);
        assert_eq!(band(WIDE, 0, 1440), None);
    }
}
