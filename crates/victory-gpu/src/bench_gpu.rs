//! `victory bench`: the host layer's own passes on the card ([`crate::host::CARD_COMPARE`]: the
//! hashes before a submission's lists and the compares after them), timed on the first
//! hardware adapter with the counts a run's log gave: as a pass per run of pieces, and as the
//! table passes `VICTORY_CARD_TABLE` makes (a pass per copy and submission).

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::{Interface, s};

const PIECE: u64 = 64 << 10;
/// The copy the passes walk: 1,024 pieces (64 MB) in the card's memory.
const COPY_PIECES: u32 = 1024;
/// Result dwords (`differs`): as many as a frame's pieces may need.
const SLOTS: u64 = 1 << 18;

/// The device and what the passes need.
pub(crate) struct Card {
    _device: ID3D12Device,
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    event: HANDLE,
    submitted: u64,
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    now: ID3D12Resource,
    hashes: ID3D12Resource,
    differs: ID3D12Resource,
    differs_data: usize,
    stamps: ID3D12QueryHeap,
    back: ID3D12Resource,
    back_data: usize,
    ticks_per_ms: f64,
}

fn buffer(device: &ID3D12Device, heap: D3D12_HEAP_PROPERTIES, size: u64, flags: D3D12_RESOURCE_FLAGS, state: D3D12_RESOURCE_STATES) -> windows::core::Result<ID3D12Resource> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    };
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: plain creation.
    unsafe { device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut made)? };
    made.ok_or_else(windows::core::Error::empty)
}

impl Card {
    /// On the first hardware adapter; `None` without one.
    pub(crate) fn new() -> Option<Card> {
        // SAFETY: plain DXGI and D3D12 object creation and mapping.
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
            let adapter: IDXGIAdapter = factory.EnumAdapters(0).ok()?;
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            let device = device?;
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() })
                .ok()?;
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).ok()?;
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).ok()?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
            let event = CreateEventW(None, false, false, None).ok()?;
            let source = crate::host::CARD_COMPARE;
            let mut code: Option<ID3DBlob> = None;
            D3DCompile(source.as_ptr().cast(), source.len(), s!("card compare"), None, None, s!("main"), s!("cs_5_1"), 0, 0, &mut code, None)
                .ok()?;
            let code = code?;
            let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
            let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes).ok()?;
            let pso: ID3D12PipelineState = device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytes.as_ptr().cast(), BytecodeLength: bytes.len() },
                    ..Default::default()
                })
                .ok()?;
            let default = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
            let shared = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_CUSTOM,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
                MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                ..Default::default()
            };
            let readback = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
            let uav = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
            let now = buffer(&device, default, u64::from(COPY_PIECES) * PIECE, uav, D3D12_RESOURCE_STATE_COMMON).ok()?;
            let hashes = buffer(&device, default, 16 * u64::from(COPY_PIECES), uav, D3D12_RESOURCE_STATE_COMMON).ok()?;
            // The results in PC memory, as the host keeps them.
            let differs = buffer(&device, shared, 4 * SLOTS, uav, D3D12_RESOURCE_STATE_COMMON).ok()?;
            let mut data = std::ptr::null_mut();
            differs.Map(0, None, Some(&mut data)).ok()?;
            let mut stamps: Option<ID3D12QueryHeap> = None;
            device
                .CreateQueryHeap(&D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: 2, NodeMask: 0 }, &mut stamps)
                .ok()?;
            let back = buffer(&device, readback, 16, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST).ok()?;
            let mut back_data = std::ptr::null_mut();
            back.Map(0, None, Some(&mut back_data)).ok()?;
            let ticks_per_ms = queue.GetTimestampFrequency().ok()? as f64 / 1000.0;
            Some(Card {
                _device: device,
                queue,
                allocator,
                list,
                fence,
                event,
                submitted: 0,
                root,
                pso,
                now,
                hashes,
                differs,
                differs_data: data as usize,
                stamps: stamps?,
                back,
                back_data: back_data as usize,
                ticks_per_ms,
            })
        }
    }

    /// The card's time (ms) of `passes`: each (mode, pieces), a pass per entry as the host
    /// records them; with `table`, the pieces of a pass are named in their result dwords.
    fn time(&mut self, passes: &[(u32, Vec<u32>)], table: bool) -> Option<f64> {
        // SAFETY: our list, open; live buffers; the compare's own root signature; the result
        // dwords hold every pass's pieces (`SLOTS`).
        unsafe {
            let list = &self.list;
            list.SetComputeRootSignature(&self.root);
            list.SetPipelineState(&self.pso);
            list.SetComputeRootShaderResourceView(1, self.now.GetGPUVirtualAddress());
            list.SetComputeRootUnorderedAccessView(2, self.hashes.GetGPUVirtualAddress());
            list.SetComputeRootUnorderedAccessView(3, self.differs.GetGPUVirtualAddress());
            list.EndQuery(&self.stamps, D3D12_QUERY_TYPE_TIMESTAMP, 0);
            let size = (u64::from(COPY_PIECES) * PIECE) as u32;
            let mut slot = 0u32;
            for (mode, pieces) in passes {
                if u64::from(slot) + pieces.len() as u64 > SLOTS {
                    break;
                }
                if table {
                    for (i, &p) in pieces.iter().enumerate() {
                        (self.differs_data as *mut u32).add(slot as usize + i).write_volatile(p);
                    }
                    let constants = [0u32, 0, size, pieces.len() as u32, slot * 4, mode | 4];
                    list.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
                    list.Dispatch(pieces.len() as u32, 1, 1);
                } else {
                    // (a run: its pieces one after the other from the first)
                    let constants = [pieces[0], 0, size, pieces.len() as u32, slot * 4, *mode];
                    list.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
                    list.Dispatch(pieces.len() as u32, 1, 1);
                }
                slot += pieces.len() as u32;
            }
            list.EndQuery(&self.stamps, D3D12_QUERY_TYPE_TIMESTAMP, 1);
            list.ResolveQueryData(&self.stamps, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, &self.back, 0);
            list.Close().ok()?;
            let cmd: ID3D12CommandList = list.cast().ok()?;
            self.queue.ExecuteCommandLists(&[Some(cmd)]);
            self.submitted += 1;
            self.queue.Signal(&self.fence, self.submitted).ok()?;
            self.fence.SetEventOnCompletion(self.submitted, self.event).ok()?;
            WaitForSingleObject(self.event, INFINITE);
            self.allocator.Reset().ok()?;
            self.list.Reset(&self.allocator, None).ok()?;
            let ticks = (self.back_data as *const u64).add(1).read_volatile().saturating_sub((self.back_data as *const u64).read_volatile());
            Some(ticks as f64 / self.ticks_per_ms)
        }
    }

    /// A frame's hash and compare passes as a log counted them (`hash` passes over
    /// `hashed` pieces, `compares` passes over `compared` pieces), with `groups` table passes
    /// of each kind instead: (per run, table) ms on the card. Least of three.
    pub(crate) fn frame(&mut self, hash: f64, hashed: f64, compares: f64, compared: f64, groups: f64) -> Option<(f64, f64)> {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 8) as u32
        };
        // Runs: the pieces cut evenly over the passes, each run at a place of its own.
        let mut runs = |passes: f64, pieces: f64, mode: u32| -> Vec<(u32, Vec<u32>)> {
            let passes = (passes.round() as u32).max(1);
            let pieces = (pieces.round() as u32).max(passes);
            (0..passes)
                .map(|i| {
                    let n = (pieces / passes + u32::from(i < pieces % passes)).clamp(1, COPY_PIECES);
                    let first = next() % (COPY_PIECES - n + 1);
                    (mode, (first..first + n).collect())
                })
                .collect()
        };
        let per_run: Vec<(u32, Vec<u32>)> = runs(hash, hashed, 0).into_iter().chain(runs(compares, compared, 2)).collect();
        // The same pieces as `groups` table passes of each kind.
        let joined = |mode: u32| -> Vec<(u32, Vec<u32>)> {
            let all: Vec<u32> = per_run.iter().filter(|r| r.0 == mode).flat_map(|r| r.1.iter().copied()).collect();
            let n = (groups.round() as usize).max(1);
            all.chunks(all.len().div_ceil(n).max(1)).map(|c| (mode, c.to_vec())).collect()
        };
        let tabled: Vec<(u32, Vec<u32>)> = joined(0).into_iter().chain(joined(2)).collect();
        let mut least = (f64::MAX, f64::MAX);
        // (warm)
        self.time(&per_run, false)?;
        for _ in 0..3 {
            least.0 = least.0.min(self.time(&per_run, false)?);
            least.1 = least.1.min(self.time(&tabled, true)?);
        }
        Some(least)
    }
}
