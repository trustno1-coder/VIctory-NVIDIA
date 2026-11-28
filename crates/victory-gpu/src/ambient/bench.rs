//! The stand-in's passes on the card without the title: the time of each
//! pass, plain and lean (`VICTORY_AMBIENT_LEAN`), and that the lean gather
//! writes the very bytes of the plain one and the lean mean the same value
//! but for its last bits. A depth of a room is made by a pass of its own;
//! the cells hold made-up light. Ignored by default:
//! `cargo test -p victory-gpu --release --features diagnostics --lib ambient::bench -- --ignored --nocapture --test-threads=1`.

use super::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_R32_FLOAT, DXGI_FORMAT_R32_UINT, DXGI_SAMPLE_DESC};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::Interface;

/// Loops of each pass timed; the median is reported.
const LOOPS: usize = 21;

/// A room 2 to 14 m deep with a step, as the depth the passes read.
const FILL: &str = r#"
RWTexture2D<float> depth : register(u0);
[RootSignature("DescriptorTable(UAV(u0))")]
[numthreads(8, 8, 1)]
void main(uint3 t : SV_DispatchThreadID) {
    float z = 2.0 + 10.0 * t.x / 1280.0 + (t.y > 400 ? 2.0 : 0.0) + 0.5 * sin(t.y * 0.05);
    depth[t.xy] = 0.1 / z;
}
"#;

struct Rig {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    event: windows::Win32::Foundation::HANDLE,
    sent: u64,
}

impl Rig {
    /// Records with `record`, runs it and waits.
    unsafe fn run(&mut self, record: impl FnOnce(&ID3D12GraphicsCommandList)) {
        // SAFETY: the caller's; the list's last run was waited for.
        unsafe {
            self.allocator.Reset().unwrap();
            self.list.Reset(&self.allocator, None).unwrap();
            record(&self.list);
            self.list.Close().unwrap();
            self.queue.ExecuteCommandLists(&[Some(self.list.cast().unwrap())]);
            self.sent += 1;
            self.queue.Signal(&self.fence, self.sent).unwrap();
            self.fence.SetEventOnCompletion(self.sent, self.event).unwrap();
            WaitForSingleObject(self.event, INFINITE);
        }
    }
}

unsafe fn texture(device: &ID3D12Device, size: [u32; 2], format: DXGI_FORMAT) -> ID3D12Resource {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: u64::from(size[0]),
        Height: size[1],
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: a plain creation.
    unsafe {
        device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                None,
                &mut made,
            )
            .unwrap();
    }
    made.unwrap()
}

/// Copies `bytes` of `from` at `at` out and returns them.
unsafe fn read(rig: &mut Rig, host: &crate::host::Host, from: &ID3D12Resource, at: u64, bytes: u64) -> Vec<u8> {
    let back = host.read_back_buffer(bytes).unwrap();
    // SAFETY: live buffers; the copy is waited for before the mapping is read.
    unsafe {
        rig.run(|l| l.CopyBufferRegion(&back, 0, from, at, bytes));
        let mut data = std::ptr::null_mut();
        back.Map(0, None, Some(&mut data)).unwrap();
        let out = std::slice::from_raw_parts(data.cast::<u8>(), bytes as usize).to_vec();
        back.Unmap(0, None);
        out
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

#[test]
#[ignore = "benchmark on the card; see the module notes"]
fn bench_the_stand_ins_passes() {
    let Some(host) = host() else {
        eprintln!("no host GPU");
        return;
    };
    let pass = make(&host.device, |bytes| host.uav_buffer(bytes), lean::GATHER_WIDE | lean::MEAN_WIDE).expect("the passes");
    let (gather_wide, mean_wide) = (pass.gather_wide.clone().expect("lean gather"), pass.mean_wide.clone().expect("lean mean"));
    // SAFETY: plain D3D12 object creation and recording; every object
    // outlives the lists that use it, each run is waited for.
    unsafe {
        let device = host.device.clone();
        let queue: ID3D12CommandQueue = device
            .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() })
            .unwrap();
        let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
        let list: ID3D12GraphicsCommandList =
            device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
        list.Close().unwrap();
        let mut rig = Rig {
            device: device.clone(),
            queue,
            allocator,
            list,
            fence: device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap(),
            event: CreateEventW(None, false, false, None).unwrap(),
            sent: 0,
        };
        let ticks = rig.queue.GetTimestampFrequency().unwrap() as f64;

        // Views: the depth to read (0), the light to raise (1), the depth to
        // fill (2).
        let heap: ID3D12DescriptorHeap = rig
            .device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 3,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })
            .unwrap();
        let step = u64::from(rig.device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV));
        let cpu = |i: u64| D3D12_CPU_DESCRIPTOR_HANDLE { ptr: heap.GetCPUDescriptorHandleForHeapStart().ptr + (i * step) as usize };
        let gpu = |i: u64| D3D12_GPU_DESCRIPTOR_HANDLE { ptr: heap.GetGPUDescriptorHandleForHeapStart().ptr + i * step };

        for size in [[1280u32, 720], [640, 360]] {
            let depth = texture(&rig.device, size, DXGI_FORMAT_R32_FLOAT);
            let light = texture(&rig.device, size, DXGI_FORMAT_R32_UINT);
            let srv = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R32_FLOAT,
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 { Texture2D: D3D12_TEX2D_SRV { MipLevels: 1, ..Default::default() } },
            };
            rig.device.CreateShaderResourceView(&depth, Some(&srv), cpu(0));
            rig.device.CreateUnorderedAccessView(&light, None::<&ID3D12Resource>, None, cpu(1));
            rig.device.CreateUnorderedAccessView(&depth, None::<&ID3D12Resource>, None, cpu(2));
            let fill_code = crate::host::compile(FILL, s!("main"), s!("cs_5_1")).unwrap();
            let fill_root: ID3D12RootSignature = rig.device.CreateRootSignature(0, &fill_code).unwrap();
            let fill: ID3D12PipelineState = rig
                .device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&fill_root),
                    CS: D3D12_SHADER_BYTECODE { pShaderBytecode: fill_code.as_ptr().cast(), BytecodeLength: fill_code.len() },
                    ..Default::default()
                })
                .unwrap();
            // Made-up light in the cells' first CELLS entries (what the
            // gather sums; the keep would put it there).
            let light_cells: Vec<f32> = (0..CELLS as usize * 4)
                .map(|i| if i % 4 == 3 { 1.0 } else { ((i * 37 % 101) as f32) / 50.0 })
                .collect();
            let (up, up_data) = host.shared_buffer(CELLS * 16).unwrap();
            std::ptr::copy_nonoverlapping(light_cells.as_ptr().cast::<u8>(), up_data, (CELLS * 16) as usize);
            rig.run(|l| {
                l.SetDescriptorHeaps(&[Some(heap.clone())]);
                l.SetComputeRootSignature(&fill_root);
                l.SetPipelineState(&fill);
                l.SetComputeRootDescriptorTable(0, gpu(2));
                l.Dispatch(size[0].div_ceil(8), size[1].div_ceil(8), 1);
                l.ResourceBarrier(&[crate::host::transition(
                    &depth,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                )]);
                l.CopyBufferRegion(&pass.cells, 0, &up, 0, CELLS * 16);
            });

            let [strength, radius] = shade();
            let mut words = [0u32; 12];
            for (to, v) in words.iter_mut().zip([1.25f32, 0.0, strength, radius, 0.1, 0.45]) {
                *to = v.to_bits();
            }
            words[6] = size[0];
            words[7] = size[1];
            words[8] = 0.04f32.to_bits();
            words[9] = 4.0f32.to_bits();
            let bind = |l: &ID3D12GraphicsCommandList| {
                l.SetDescriptorHeaps(&[Some(heap.clone())]);
                l.SetComputeRootSignature(&pass.root);
                l.SetComputeRoot32BitConstants(0, 12, words.as_ptr().cast(), 0);
                l.SetComputeRootDescriptorTable(1, gpu(0));
                l.SetComputeRootDescriptorTable(2, gpu(1));
                l.SetComputeRootUnorderedAccessView(3, pass.cells.GetGPUVirtualAddress());
                l.SetComputeRootUnorderedAccessView(4, pass.points.GetGPUVirtualAddress());
            };
            // (Not the keep: it reads the radiance image as colours, and the
            // view here is the raise's.)
            let cases: [(&str, &ID3D12PipelineState, [u32; 3]); 6] = [
                ("place", &pass.place, [8, 9, 1]),
                ("gather", &pass.gather, [20, 45, 1]),
                ("gather_wide", &gather_wide, [10, 45, 1]),
                ("mean", &pass.mean, [1, 1, 1]),
                ("mean_wide", &mean_wide, [1, 1, 1]),
                ("raise", &pass.raise, [size[0].div_ceil(8), size[1].div_ceil(8), 1]),
            ];
            // Timestamps around every loop of every case.
            let stamps = (cases.len() * LOOPS * 2) as u32;
            let mut qheap: Option<ID3D12QueryHeap> = None;
            rig.device
                .CreateQueryHeap(&D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: stamps, NodeMask: 0 }, &mut qheap)
                .unwrap();
            let qheap = qheap.unwrap();
            let back = host.read_back_buffer(u64::from(stamps) * 8).unwrap();
            rig.run(|l| {
                bind(l);
                let mut q = 0;
                for (_, pso, [x, y, z]) in &cases {
                    l.SetPipelineState(*pso);
                    for _ in 0..LOOPS {
                        uav_barrier(l);
                        l.EndQuery(&qheap, D3D12_QUERY_TYPE_TIMESTAMP, q);
                        l.Dispatch(*x, *y, *z);
                        uav_barrier(l);
                        l.EndQuery(&qheap, D3D12_QUERY_TYPE_TIMESTAMP, q + 1);
                        q += 2;
                    }
                }
                l.ResolveQueryData(&qheap, D3D12_QUERY_TYPE_TIMESTAMP, 0, stamps, &back, 0);
            });
            let mut data = std::ptr::null_mut();
            back.Map(0, None, Some(&mut data)).unwrap();
            let t = std::slice::from_raw_parts(data.cast::<u64>(), stamps as usize).to_vec();
            back.Unmap(0, None);
            for (i, (name, _, _)) in cases.iter().enumerate() {
                let ms: Vec<f64> = (0..LOOPS)
                    .map(|k| {
                        let at = (i * LOOPS + k) * 2;
                        t[at + 1].saturating_sub(t[at]) as f64 * 1000.0 / ticks
                    })
                    .collect();
                eprintln!("ambient bench {}x{}: {name:12} {:.3} ms (median of {LOOPS})", size[0], size[1], median(ms));
            }

            // Same bytes: the points of the plain gather and of the wide one.
            let points_of = |rig: &mut Rig, pso: &ID3D12PipelineState, groups: [u32; 2]| {
                rig.run(|l| {
                    bind(l);
                    l.SetPipelineState(&pass.place);
                    l.Dispatch(8, 9, 1);
                    uav_barrier(l);
                    l.SetPipelineState(pso);
                    l.Dispatch(groups[0], groups[1], 1);
                });
                read(rig, host, &pass.points, 0, POINT_BYTES)
            };
            let plain = points_of(&mut rig, &pass.gather, [20, 45]);
            let wide = points_of(&mut rig, &gather_wide, [10, 45]);
            assert!(plain.iter().any(|&b| b != 0), "the gather wrote points");
            assert_eq!(plain, wide, "the wide gather writes the plain one's bytes");

            // The mean: from a fresh slow mean (its alpha 0), both ways.
            let mean_of = |rig: &mut Rig, pso: &ID3D12PipelineState| {
                let zero = host.shared_buffer(16).unwrap();
                std::ptr::write_bytes(zero.1, 0, 16);
                rig.run(|l| {
                    l.CopyBufferRegion(&pass.cells, CELLS * 16, &zero.0, 0, 16);
                });
                rig.run(|l| {
                    bind(l);
                    l.SetPipelineState(pso);
                    l.Dispatch(1, 1, 1);
                });
                let bytes = read(rig, host, &pass.cells, CELLS * 16, 16);
                let f: Vec<f32> = bytes.chunks(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
                f
            };
            let a = mean_of(&mut rig, &pass.mean);
            let b = mean_of(&mut rig, &mean_wide);
            for c in 0..3 {
                assert!((a[c] - b[c]).abs() <= 1e-5 * a[c].abs().max(1e-3), "mean {c}: {} against {}", a[c], b[c]);
            }
            eprintln!("ambient bench {}x{}: gather bytes equal; mean {a:?} against {b:?}", size[0], size[1]);
        }
    }
}
