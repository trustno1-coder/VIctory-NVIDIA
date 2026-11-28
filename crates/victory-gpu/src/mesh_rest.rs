//! A mesh pipeline is never left bound behind its own draws on an Intel
//! card. The Arc B580 hung (`DXGI_ERROR_DEVICE_HUNG`, now and then a page
//! fault at a small address) on the first big frame of the world whenever a
//! mesh pipeline stayed bound on a list while copies, clears, dispatches or
//! barriers were recorded after it. No mesh draw was needed, a trivial
//! synthetic mesh pipeline did the same, and the more mesh binds a frame had,
//! the likelier the hang: the driver's, not the title's.
//!
//! So a list that has a mesh pipeline bound gets a plain vertex pipeline (the
//! rest pipeline) before anything else is recorded into it
//! (`HostList::leave_mesh`): before every title call that records, before a
//! draw, dispatch or indirect execution binds a pipeline of another kind, and
//! before the list closes. A run of mesh dispatches with nothing recorded
//! between them keeps its one bind.
//!
//! `VICTORY_MESH_REST`: `off` (never), `bind` (bind the rest pipeline),
//! `draw` (bind it and draw one point it clips away, for a driver that only
//! takes in a pipeline at a draw). Default: `bind` on an Intel card, `off` on
//! others. The frame-rate line is followed by one with the mesh and rest
//! binds per frame ([`line`]).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Since the last frame line: mesh pipelines bound on host lists (with or
/// without the rest pipeline; again after a rest included), rest pipelines
/// bound, and rest draws ([`line`]).
static COUNTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// A mesh pipeline was bound on a host list.
pub(crate) fn note_mesh_bind() {
    COUNTS[0].fetch_add(1, Ordering::Relaxed);
}

/// The binds of [`COUNTS`] per frame, for the frame-rate line: what the
/// rest pipeline costs in binds, read beside the frame's preparation time
/// and its draws dropped without a host pipeline. With
/// `VICTORY_MESH_REST=off` the mesh binds are those the title asks for.
pub(crate) fn line(frames: f64) {
    let [mesh, rests, draws] = [0, 1, 2].map(|i| COUNTS[i].swap(0, Ordering::Relaxed) as f64 / frames);
    if mesh + rests > 0.0 {
        tracing::info!(
            "mesh pipelines per frame (VICTORY_MESH_REST={:?}): {mesh:.1} bound, {rests:.1} rest pipelines bound in their place before other work ({draws:.1} with a draw)",
            mode()
        );
    }
}

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY_POINTLIST, ID3DBlob};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::s;

/// What [`leave`] does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    Off,
    Bind,
    Draw,
}

/// [`Mode`], from `VICTORY_MESH_REST` or the card.
pub(crate) fn mode() -> Mode {
    static MODE: OnceLock<Mode> = OnceLock::new();
    *MODE.get_or_init(|| {
        let mode = match std::env::var("VICTORY_MESH_REST").as_deref() {
            Ok("off" | "0") => Mode::Off,
            Ok("bind" | "1" | "on") => Mode::Bind,
            Ok("draw" | "2") => Mode::Draw,
            _ if crate::host::host().is_some_and(|h| intel(&h.device)) => Mode::Bind,
            _ => Mode::Off,
        };
        if mode != Mode::Off {
            tracing::info!(?mode, "mesh pipelines are unbound before other work (VICTORY_MESH_REST)");
        }
        mode
    })
}

/// Whether `device` is an Intel card's.
fn intel(device: &ID3D12Device) -> bool {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter1, IDXGIFactory4};
    // SAFETY: plain queries.
    unsafe {
        CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            .and_then(|f| f.EnumAdapterByLuid::<IDXGIAdapter1>(device.GetAdapterLuid()))
            .and_then(|a| a.GetDesc1())
            .is_ok_and(|d| d.VendorId == 0x8086)
    }
}

/// A vertex shader alone, with an empty root signature; its one point is
/// outside the clip volume.
const SOURCE: &str = r#"
[RootSignature("")]
float4 vs(uint id : SV_VertexID) : SV_Position { return float4(2, 2, 2, 1); }
"#;

/// The rest pipeline and its root signature.
pub(crate) struct Rest {
    pub(crate) root: ID3D12RootSignature,
    pub(crate) pso: ID3D12PipelineState,
}
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Rest {}
// SAFETY: as above.
unsafe impl Sync for Rest {}

/// The rest pipeline, made on first use; `None` if it cannot be made
/// (logged once).
pub(crate) fn rest() -> Option<&'static Rest> {
    static REST: OnceLock<Option<Rest>> = OnceLock::new();
    REST.get_or_init(|| {
        let made = crate::host::host().ok_or_else(windows::core::Error::empty).and_then(|h| make(&h.device));
        match made {
            Ok(rest) => Some(rest),
            Err(e) => {
                tracing::warn!(error = %e, "no rest pipeline: mesh pipelines stay bound");
                None
            }
        }
    })
    .as_ref()
}

fn make(device: &ID3D12Device) -> windows::core::Result<Rest> {
    let mut code: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the calls; the blob's bytes
    // live while it does.
    unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            s!("mesh_rest"),
            None,
            None,
            s!("vs"),
            s!("vs_5_1"),
            0,
            0,
            &mut code,
            None,
        )?;
        let vs = code.ok_or_else(windows::core::Error::empty)?;
        let bytes = std::slice::from_raw_parts(vs.GetBufferPointer().cast::<u8>(), vs.GetBufferSize());
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
        let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            VS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: vs.GetBufferPointer(),
                BytecodeLength: vs.GetBufferSize(),
            },
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_POINT,
            NumRenderTargets: 0,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        let pso = device.CreateGraphicsPipelineState(&desc)?;
        Ok(Rest { root, pso })
    }
}

/// Records the rest pipeline into `list` (and with [`Mode::Draw`] its
/// point). The list's graphics root signature and pipeline change, and with
/// a draw its topology: the caller sets them again. False if nothing was
/// recorded.
pub(crate) fn record(list: &ID3D12GraphicsCommandList) -> bool {
    let mode = mode();
    if mode == Mode::Off {
        return false;
    }
    let Some(rest) = rest() else { return false };
    // SAFETY: an open list and live objects.
    unsafe {
        list.SetGraphicsRootSignature(&rest.root);
        list.SetPipelineState(&rest.pso);
        if mode == Mode::Draw {
            list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_POINTLIST);
            list.DrawInstanced(1, 1, 0, 0);
        }
    }
    COUNTS[1].fetch_add(1, Ordering::Relaxed);
    if mode == Mode::Draw {
        COUNTS[2].fetch_add(1, Ordering::Relaxed);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rest pipeline is made, and a list that binds it and draws its
    /// point, then copies, closes and runs with the device still there.
    #[test]
    fn rest_pipeline_binds_draws_and_runs() {
        let Some(host) = crate::host::host() else { return };
        let rest = make(&host.device).expect("the rest pipeline");
        let list = host.new_list().unwrap();
        // SAFETY: an open list and live objects.
        unsafe {
            list.list.SetGraphicsRootSignature(&rest.root);
            list.list.SetPipelineState(&rest.pso);
            list.list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_POINTLIST);
            list.list.DrawInstanced(1, 1, 0, 0);
        }
        list.close();
        host.execute(&[&list]);
        // SAFETY: a plain query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }
}
