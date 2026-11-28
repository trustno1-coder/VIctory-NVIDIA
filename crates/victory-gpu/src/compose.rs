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
#define RS "DescriptorTable(SRV(t0, numDescriptors=2), visibility=SHADER_VISIBILITY_PIXEL), RootConstants(num32BitConstants=1, b0, visibility=SHADER_VISIBILITY_PIXEL), StaticSampler(s0, filter=FILTER_MIN_MAG_MIP_LINEAR, addressU=TEXTURE_ADDRESS_CLAMP, addressV=TEXTURE_ADDRESS_CLAMP)"
Texture2D<float4> scene : register(t0);
Texture2D<float4> over : register(t1);
cbuffer DisplayOptions : register(b0) { uint flags; };
SamplerState smooth_ : register(s0);
struct V { float4 at : SV_Position; float2 uv : TEXCOORD0; };
[RootSignature(RS)]
V vs(uint id : SV_VertexID) {
    V v;
    v.uv = float2((id << 1) & 2, id & 2);
    v.at = float4(v.uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return v;
}
float luminance(float3 rgb) { return dot(rgb, float3(0.299, 0.587, 0.114)); }
float3 color(float2 uv) { return scene.SampleLevel(smooth_, uv, 0).rgb; }
float3 fxaa(float2 uv, float3 center) {
    uint w, h;
    scene.GetDimensions(w, h);
    float2 px = 1.0 / float2(w, h);
    float nw = luminance(color(uv + px * float2(-1, -1)));
    float ne = luminance(color(uv + px * float2( 1, -1)));
    float sw = luminance(color(uv + px * float2(-1,  1)));
    float se = luminance(color(uv + px * float2( 1,  1)));
    float mid = luminance(center);
    float lo = min(mid, min(min(nw, ne), min(sw, se)));
    float hi = max(mid, max(max(nw, ne), max(sw, se)));
    if (hi - lo < max(0.0312, hi * 0.125)) return center;
    float2 dir = float2((sw + se) - (nw + ne), (nw + sw) - (ne + se));
    float reduce = max((nw + ne + sw + se) / 32.0, 1.0 / 128.0);
    dir = clamp(dir / (min(abs(dir.x), abs(dir.y)) + reduce), -8.0, 8.0) * px;
    float3 inner = (color(uv - dir / 6.0) + color(uv + dir / 6.0)) * 0.5;
    float3 outer = inner * 0.5 + (color(uv - dir * 0.5) + color(uv + dir * 0.5)) * 0.25;
    float l = luminance(outer);
    return (l < lo || l > hi) ? inner : outer;
}
[RootSignature(RS)]
float4 ps(V v) : SV_Target {
    float4 s = scene.SampleLevel(smooth_, v.uv, 0);
    if (flags & 1) s.rgb = fxaa(v.uv, s.rgb);
    if (!(flags & 2)) return s;
    float4 o = over.SampleLevel(smooth_, v.uv, 0);
    return float4(saturate(o.rgb + s.rgb * (1 - saturate(o.a))), 1);
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
        base: (&ID3D12Resource, DXGI_FORMAT),
        overlay: Option<(&ID3D12Resource, DXGI_FORMAT)>,
        fxaa: bool,
        back: &ID3D12Resource,
        (width, height): (u32, u32),
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
            for (i, plane) in [Some(base), overlay].into_iter().enumerate() {
                let at = D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: cpu.ptr + (i * self.step as usize),
                };
                let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: plane.map_or(base.1, |p| p.1),
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
                    .CreateShaderResourceView(plane.map(|p| p.0), Some(&desc), at);
            }
            let rtv = host.rtv(Some(back), None);
            list.OMSetRenderTargets(1, Some(&rtv), false, None);
            list.SetDescriptorHeaps(&[Some(views.clone())]);
            list.SetGraphicsRootSignature(&self.root);
            list.SetPipelineState(&self.pso);
            list.SetGraphicsRootDescriptorTable(0, gpu);
            list.SetGraphicsRoot32BitConstant(1, u32::from(fxaa) | (u32::from(overlay.is_some()) << 1), 0);
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

/// Some display surfaces forbid SRVs or have an sRGB view that would change
/// the direct copy's transfer function. Keep a sampleable copy in swap format.
pub(crate) fn sample_copy(host: &Host, width: u32, height: u32, format: DXGI_FORMAT) -> windows::core::Result<ID3D12Resource> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: u64::from(width), Height: height, DepthOrArraySize: 1, MipLevels: 1,
        Format: format, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        ..Default::default()
    };
    let mut resource = None;
    unsafe {
        host.device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
            D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut resource,
        )?;
    }
    resource.ok_or_else(windows::core::Error::empty)
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

#[cfg(test)]
#[test]
fn display_shaders_compile() {
    compile(s!("vs"), s!("vs_5_1")).unwrap();
    compile(s!("ps"), s!("ps_5_1")).unwrap();
}

#[cfg(all(test, feature = "diagnostics"))]
mod tests {
    use super::*;

    fn texture(host: &Host, width: u64, height: u32) -> ID3D12Resource {
        let mut resource = None;
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: width,
            Height: height,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
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
    }

    #[test]
    fn single_plane_fxaa_smooths_diagonal_and_preserves_flat_pixels() {
        let Some(host) = crate::host::host() else {
            eprintln!("skipping FXAA readback: no D3D12 device");
            return;
        };
        let format = DXGI_FORMAT_R8G8B8A8_UNORM;
        let mut composer = Composer::new(host, format).unwrap();
        let input = texture(host, 16, 16);
        let output = texture(host, 16, 16);
        let target = D3D12_RESOURCE_STATE_RENDER_TARGET;
        let read = D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;
        let list = host.new_list().unwrap();
        let cmds = &list.list;
        unsafe {
            let rtv = host.rtv(Some(&input), None);
            cmds.ClearRenderTargetView(rtv, &[0.0, 0.0, 0.0, 1.0], None);
            for y in 0..16 {
                let row = windows::Win32::Foundation::RECT { left: y, top: y, right: 16, bottom: y + 1 };
                cmds.ClearRenderTargetView(rtv, &[1.0; 4], Some(&[row]));
            }
            cmds.ResourceBarrier(&[crate::host::transition(&input, target, read)]);
            composer.record(host, 0, cmds, (&input, format), None, true, &output, (16, 16)).unwrap();
            cmds.ResourceBarrier(&[crate::host::transition(&output, target, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            let (buffer, footprint) = crate::capture::read_back(&host.device, cmds, &output).unwrap();
            list.close();
            host.execute(&[&list]);
            host.device.GetDeviceRemovedReason().unwrap();
            let mut mapped = std::ptr::null_mut();
            buffer.Map(0, None, Some(&mut mapped)).unwrap();
            let row = mapped.cast::<u8>().add(footprint.Offset as usize + footprint.Footprint.RowPitch as usize * 8);
            let levels: Vec<_> = (0..16).map(|x| *row.add(4 * x)).collect();
            buffer.Unmap(0, None);
            assert_eq!((levels[1], levels[14]), (0, 255));
            assert!(levels[6..11].iter().any(|v| (1..255).contains(v)), "{levels:?}");
            eprintln!("FXAA diagonal readback: {levels:?}");
        }
    }

    #[test]
    fn composes_scaled_premultiplied_plane_on_gpu() {
        let Some(host) = crate::host::host() else {
            eprintln!("skipping display-plane readback: no D3D12 device");
            return;
        };
        let format = DXGI_FORMAT_R8G8B8A8_UNORM;
        let mut compositor = Composer::new(host, format).unwrap();
        let list = host.new_list().unwrap();
        // A half-red base at one resolution. The overlay at output resolution
        // contains transparent, half-opaque green and fully opaque blue pixels.
        let base = texture(host, 1, 1);
        let overlay = texture(host, 3, 1);
        let output = texture(host, 3, 1);
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
                    (&base, format),
                    Some((&overlay, format)),
                    false,
                    &output,
                    (3, 1),
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
}
