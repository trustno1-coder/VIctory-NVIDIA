//! Native, asset-free maintenance regressions. Run this ignored test in its own
//! diagnostics process, with one test thread and the D3D12 debug layer enabled.

use super::maintenance::{self, Batch, Job};
use super::*;
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;

const OUTPUT_BYTES: usize = 256;
const HASH_BYTES: usize = 64;
const SENTINEL: u32 = 0xa5a5_5a5a;

struct Case {
    source: ID3D12Resource,
    hashes: ID3D12Resource,
    bytes: Vec<u8>,
    span: crate::shadow::CardSpan,
    out_at: u32,
}

#[derive(Debug, PartialEq, Eq)]
struct Observation {
    output: Vec<u32>,
    hashes: Vec<Vec<u8>>,
}

fn open(host: &Host) -> Prelude {
    let pair = host.make_prelude().expect("fixture command list");
    // SAFETY: a newly created, closed list whose allocator has never executed.
    unsafe {
        pair.0.Reset().unwrap();
        pair.1.Reset(&pair.0, None).unwrap();
    }
    pair
}

fn wait(host: &Host, target: u64) {
    // SAFETY: this fixture exclusively owns the host and its event, and every
    // target was signalled after the lists it covers. Bound a failed GPU wait.
    unsafe {
        host.fence.SetEventOnCompletion(target, host.event).unwrap();
        assert_eq!(
            WaitForSingleObject(host.event, 30_000),
            WAIT_OBJECT_0,
            "native fixture timed out"
        );
        host.device.GetDeviceRemovedReason().unwrap();
        assert!(host.fence.GetCompletedValue() >= target);
    }
}

/// Each entry is a distinct Execute call: buffer decay and producer completion
/// must happen before the checker, including when the producer is a COPY.
fn send(host: &Host, lists: &[&ID3D12GraphicsCommandList], batch: Option<&mut Batch>) -> u64 {
    let mut target = host.submitted.lock().unwrap();
    *target += 1;
    // SAFETY: live closed lists and a freshly reserved native target, signalled
    // after every list; no fixture objects are reset or freed before its wait.
    unsafe {
        if let Some(batch) = batch {
            batch.submitted(&host.fence, *target);
        }
        for list in lists {
            host.queue
                .ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        }
        host.queue.Signal(&host.fence, *target).unwrap();
    }
    *target
}

fn copy_up(
    host: &Host,
    list: &ID3D12GraphicsCommandList,
    dst: &ID3D12Resource,
    at: u64,
    bytes: &[u8],
) -> ID3D12Resource {
    let (upload, data) = host
        .upload_buffer(bytes.len() as u64)
        .expect("fixture upload");
    // SAFETY: the upload mapping has exactly this capacity and remains pinned
    // by the caller until the producer's native submission fence completes.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
        list.CopyBufferRegion(dst, at, &upload, 0, bytes.len() as u64);
    }
    upload
}

fn jobs_of(cases: &[Case], mode: u32) -> Vec<Job> {
    cases
        .iter()
        .map(|c| Job {
            source: c.source.clone(),
            hashes: c.hashes.clone(),
            span: c.span,
            size: c.bytes.len() as u32,
            out_at: c.out_at,
            mode,
        })
        .collect()
}

fn record_legacy(
    work: &CardWork,
    list: &ID3D12GraphicsCommandList,
    jobs: &[Job],
    output: &ID3D12Resource,
) {
    // SAFETY: independent committed buffers, validated spans, open list, and
    // separate Execute calls establish the producer's buffer COMMON state.
    unsafe {
        list.SetComputeRootSignature(&work.root);
        list.SetPipelineState(&work.pso);
        list.SetComputeRootUnorderedAccessView(3, output.GetGPUVirtualAddress());
        for (i, job) in jobs.iter().enumerate() {
            if i != 0 {
                // The duplicate-piece fallback fixture deliberately depends on
                // the preceding mode2 hash update, so order those UAV accesses.
                list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                    Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                            pResource: std::mem::ManuallyDrop::new(None),
                        }),
                    },
                }]);
            }
            list.SetComputeRootShaderResourceView(1, job.source.GetGPUVirtualAddress());
            list.SetComputeRootUnorderedAccessView(2, job.hashes.GetGPUVirtualAddress());
            card_pass(list, &job.span, job.size, job.out_at, job.mode);
        }
    }
}

fn check(
    host: &Host,
    work: &CardWork,
    cases: &[Case],
    output: &ID3D12Resource,
    producer: Option<&ID3D12GraphicsCommandList>,
    mode: u32,
    batched: bool,
    gated: bool,
) {
    let jobs = jobs_of(cases, mode);
    let checker = open(host);
    let mut batch =
        batched.then(|| maintenance::prepare(host, &jobs, output).expect("batch accepted"));
    if let Some(batch) = &mut batch {
        assert_eq!(
            batch.dispatches(),
            1,
            "six independent pieces fit one dispatch"
        );
        // SAFETY: submission below places the producer in an earlier Execute;
        // fixture buffers are independent and retained through the native fence.
        unsafe { batch.record(&checker.1) };
    } else {
        record_legacy(work, &checker.1, &jobs, output);
    }
    // SAFETY: all recording is complete.
    unsafe { checker.1.Close().unwrap() };
    let gate: Option<ID3D12Fence> =
        gated.then(|| unsafe { host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap() });
    if let Some(gate) = &gate {
        // SAFETY: a fixture-only fence released below; the queued batch cannot
        // finish while another preparation tries to acquire reusable storage.
        unsafe { host.queue.Wait(gate, 1).unwrap() };
    }
    let lists: Vec<&ID3D12GraphicsCommandList> = producer.into_iter().chain([&checker.1]).collect();
    let target = send(host, &lists, batch.as_mut());
    drop(batch);
    let disturbance = if gated {
        let mut different = jobs_of(cases, 1);
        different.reverse();
        for job in &mut different {
            job.out_at += 32;
        }
        Some(maintenance::prepare(host, &different, output).expect("second immutable batch"))
    } else {
        None
    };
    if let Some(gate) = &gate {
        // SAFETY: CPU release of the fixture gate, not a completion signal for
        // any work. The existing native queue fence still proves completion.
        unsafe { gate.Signal(1).unwrap() };
    }
    wait(host, target);
    drop(disturbance);
}

fn observe(host: &Host, cases: &[Case], output: &ID3D12Resource) -> Observation {
    let bytes = OUTPUT_BYTES
        + cases.len() * HASH_BYTES
        + cases.iter().map(|c| c.bytes.len()).sum::<usize>();
    let back = host
        .read_back_buffer(bytes as u64)
        .expect("fixture readback");
    let list = open(host);
    // SAFETY: all sources completed and decayed in earlier Execute calls;
    // destination offsets are disjoint and lie inside this readback buffer.
    unsafe {
        list.1
            .CopyBufferRegion(&back, 0, output, 0, OUTPUT_BYTES as u64);
        let mut at = OUTPUT_BYTES;
        for c in cases {
            list.1
                .CopyBufferRegion(&back, at as u64, &c.hashes, 0, HASH_BYTES as u64);
            at += HASH_BYTES;
        }
        for c in cases {
            list.1
                .CopyBufferRegion(&back, at as u64, &c.source, 0, c.bytes.len() as u64);
            at += c.bytes.len();
        }
        list.1.Close().unwrap();
    }
    wait(host, send(host, &[&list.1], None));
    let mut data = std::ptr::null_mut();
    // SAFETY: completed native copy, bounded mapping, CPU read only. Copy all
    // observations out before unmapping and releasing the backing resource.
    unsafe {
        back.Map(0, None, Some(&mut data)).unwrap();
        let data = std::slice::from_raw_parts(data.cast::<u8>(), bytes);
        let output = data[..OUTPUT_BYTES]
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        let hashes = data[OUTPUT_BYTES..OUTPUT_BYTES + cases.len() * HASH_BYTES]
            .chunks_exact(HASH_BYTES)
            .map(<[u8]>::to_vec)
            .collect();
        let mut at = OUTPUT_BYTES + cases.len() * HASH_BYTES;
        for c in cases {
            assert_eq!(
                &data[at..at + c.bytes.len()],
                &c.bytes,
                "producer bytes, including untouched holes"
            );
            at += c.bytes.len();
        }
        let observed = Observation { output, hashes };
        back.Unmap(0, None);
        observed
    }
}

fn writer(host: &Host) -> (ID3D12RootSignature, ID3D12PipelineState) {
    const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=2,b0),UAV(u0)"
cbuffer Place : register(b0) { uint at; uint value; };
RWByteAddressBuffer destination : register(u0);
[RootSignature(RS)] [numthreads(1,1,1)]
void main() { destination.Store(at, value); }
"#;
    let mut code: Option<ID3DBlob> = None;
    // SAFETY: immutable source and valid object descriptions live through each
    // synchronous compile/create call; root signature is embedded in the blob.
    unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            windows::core::s!("maintenance fixture writer"),
            None,
            None,
            windows::core::s!("main"),
            windows::core::s!("cs_5_1"),
            0,
            0,
            &mut code,
            None,
        )
        .unwrap();
        let code = code.unwrap();
        let bytes =
            std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
        let root: ID3D12RootSignature = host.device.CreateRootSignature(0, bytes).unwrap();
        let pso = host
            .device
            .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: bytes.as_ptr().cast(),
                    BytecodeLength: bytes.len(),
                },
                ..Default::default()
            })
            .unwrap();
        (root, pso)
    }
}

fn mutate(
    host: &Host,
    cases: &mut [Case],
    uav: bool,
    writer: &(ID3D12RootSignature, ID3D12PipelineState),
) -> (Prelude, Vec<ID3D12Resource>) {
    let changes: Vec<(usize, usize, u32)> = if uav {
        vec![
            (0, 80, 0x1122_3344),
            (1, 144, 0x5566_7788),
            (2, 400, 0x99aa_bbcc),
        ]
    } else {
        vec![
            (0, 0, 0x0123_4567),
            (0, 131_088, 0x89ab_cdef),
            (1, 284, 0xfedc_ba98),
            (2, 0, 0x7654_3210),
        ]
    };
    let list = open(host);
    let mut pins = Vec::new();
    for (i, at, value) in changes {
        let c = &mut cases[i];
        c.bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
        if uav {
            // SAFETY: committed COMMON buffer, one disjoint dword per resource
            // in this producer. No hash/check work shares this Execute call.
            unsafe {
                list.1.SetComputeRootSignature(&writer.0);
                list.1.SetPipelineState(&writer.1);
                list.1
                    .SetComputeRootUnorderedAccessView(1, c.source.GetGPUVirtualAddress());
                list.1
                    .SetComputeRoot32BitConstants(0, 2, [at as u32, value].as_ptr().cast(), 0);
                list.1.Dispatch(1, 1, 1);
            }
        } else {
            pins.push(copy_up(
                host,
                &list.1,
                &c.source,
                at as u64,
                &value.to_le_bytes(),
            ));
        }
    }
    // SAFETY: all commands are recorded. The caller pins both objects and
    // uploads until the checker's fence covers their separate Execute calls.
    unsafe { list.1.Close().unwrap() };
    (list, pins)
}

fn flags(observed: &Observation, cases: &[Case]) -> Vec<bool> {
    let mut used = vec![false; OUTPUT_BYTES / 4];
    let mut flags = Vec::new();
    for c in cases {
        for p in 0..c.span.pieces {
            let i = (c.out_at + p) as usize;
            used[i] = true;
            flags.push(torn::flagged(observed.output[i]));
        }
    }
    for (i, &used) in used.iter().enumerate() {
        if !used {
            assert_eq!(observed.output[i], SENTINEL, "output hole {i} overwritten");
        }
    }
    flags
}

fn exercise(
    host: &Host,
    work: &CardWork,
    writer: &(ID3D12RootSignature, ID3D12PipelineState),
    batched: bool,
) -> Vec<Observation> {
    let specs = [
        (131_104usize, 7, 3, 23, 131_099, 2),
        (288, 0, 1, 17, 281, 9),
        (65_552, 65_520, 2, 0, 65_547, 13),
    ];
    let mut cases: Vec<Case> = specs
        .into_iter()
        .enumerate()
        .map(|(i, (size, skew, pieces, from, to, out_at))| {
            let bytes = (0..size / 4)
                .flat_map(|w| {
                    (0x3456_7890u32
                        .wrapping_add(i as u32 * 17)
                        .wrapping_add(w as u32 * 31))
                    .to_le_bytes()
                })
                .collect();
            Case {
                source: host.uav_buffer(size as u64).unwrap(),
                hashes: host.uav_buffer(HASH_BYTES as u64).unwrap(),
                bytes,
                span: crate::shadow::CardSpan {
                    from,
                    to,
                    first: 0,
                    pieces,
                    skew,
                },
                out_at,
            }
        })
        .collect();
    let output = host.uav_buffer(OUTPUT_BYTES as u64).unwrap();
    let initial = open(host);
    let mut pins = Vec::new();
    for c in &cases {
        pins.push(copy_up(host, &initial.1, &c.source, 0, &c.bytes));
        pins.push(copy_up(host, &initial.1, &c.hashes, 0, &[0; HASH_BYTES]));
    }
    pins.push(copy_up(
        host,
        &initial.1,
        &output,
        0,
        &SENTINEL.to_le_bytes().repeat(OUTPUT_BYTES / 4),
    ));
    // SAFETY: all initial copies are recorded; their uploads live until wait.
    unsafe { initial.1.Close().unwrap() };
    wait(host, send(host, &[&initial.1], None));
    check(host, work, &cases, &output, None, 0, batched, false);
    let mut observations = vec![observe(host, &cases, &output)];
    assert!(
        observations[0].output.iter().all(|&v| v == SENTINEL),
        "mode0 changed flags"
    );
    for (uav, expected) in [
        (false, vec![true, false, true, true, true, false]),
        (true, vec![true, false, false, true, false, true]),
    ] {
        let (producer, _uploads) = mutate(host, &mut cases, uav, writer);
        let before = observations.last().unwrap().hashes.clone();
        // No CPU wait separates producer and checker: the two native Execute
        // calls and final fence must establish completion and COMMON decay.
        check(
            host,
            work,
            &cases,
            &output,
            Some(&producer.1),
            1,
            batched,
            false,
        );
        let read_only = observe(host, &cases, &output);
        assert_eq!(
            flags(&read_only, &cases),
            expected,
            "mode1 flags, uav={uav}"
        );
        assert_eq!(read_only.hashes, before, "mode1 modified hashes");
        observations.push(read_only);
        check(
            host,
            work,
            &cases,
            &output,
            None,
            2,
            batched,
            batched && !uav,
        );
        let updated = observe(host, &cases, &output);
        assert_eq!(flags(&updated, &cases), expected, "mode2 flags, uav={uav}");
        observations.push(updated);
        check(host, work, &cases, &output, None, 1, batched, false);
        let unchanged = observe(host, &cases, &output);
        assert_eq!(
            flags(&unchanged, &cases),
            vec![false; 6],
            "updated baseline remained stale"
        );
        assert_eq!(unchanged.hashes, observations.last().unwrap().hashes);
        observations.push(unchanged);
    }
    // A mode2 update may suppress another range's flag for the same piece.
    // Batch rejection must preserve the relay's union-and-fanout protocol.
    let mut duplicates = jobs_of(&cases[..1], 2);
    duplicates[0].span = crate::shadow::CardSpan {
        from: 64,
        to: 96,
        first: 0,
        pieces: 1,
        skew: 7,
    };
    duplicates.push(Job {
        source: cases[0].source.clone(),
        hashes: cases[0].hashes.clone(),
        span: crate::shadow::CardSpan {
            from: 40_000,
            to: 40_032,
            first: 0,
            pieces: 1,
            skew: 7,
        },
        size: cases[0].bytes.len() as u32,
        out_at: 9,
        mode: 2,
    });
    assert!(
        maintenance::prepare(host, &duplicates, &output).is_none(),
        "duplicate hash writer must fall back"
    );
    let fetch = flagged_pieces(&[(1, 0, true), (1, 0, false)]);
    let selected: Vec<_> = duplicates
        .iter()
        .filter(|j| fetch.contains(&(1, j.span.first)))
        .map(|j| (j.span.from, j.span.to))
        .collect();
    assert_eq!(
        selected,
        [(64, 96), (40_000, 40_032)],
        "both clipped portions remain landable"
    );
    observations
}

#[test]
#[ignore = "native D3D12 fixture; own diagnostics process, VICTORY_D3D12_DEBUG=1, one test thread"]
fn maintenance_native_copy_uav_equivalence() {
    assert!(
        crate::switch::on("VICTORY_D3D12_DEBUG"),
        "enable the D3D12 debug layer"
    );
    let host = create().expect("native fixture must execute, never silently skip");
    assert!(host.messages.is_some(), "D3D12 debug interface is required");
    // SAFETY: the host retained its adapter for this device's lifetime.
    let desc = unsafe {
        host.adapter
            .as_ref()
            .expect("fixture adapter")
            .GetDesc1()
            .unwrap()
    };
    let end = desc
        .Description
        .iter()
        .position(|&v| v == 0)
        .unwrap_or(desc.Description.len());
    let adapter = String::from_utf16_lossy(&desc.Description[..end]);
    let software = desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0;
    assert_eq!(
        software,
        crate::switch::on("VICTORY_D3D12_WARP"),
        "actual adapter must match requested backend"
    );
    if let Ok(expected) = std::env::var("VICTORY_EXPECT_ADAPTER") {
        assert!(
            adapter.contains(&expected),
            "actual adapter {adapter:?} must contain {expected:?}"
        );
    }
    println!("MAINTENANCE_NATIVE_START adapter={adapter:?} software={software} debug=true");
    let work = host.make_card_work().expect("legacy compare pipeline");
    let writer = writer(&host);
    let legacy = exercise(&host, &work, &writer, false);
    let batched = exercise(&host, &work, &writer, true);
    assert_eq!(
        batched, legacy,
        "all output DWORDs and complete128bit hash buffers must match"
    );
    assert_eq!(host.log_messages(), None, "D3D12 debug layer error");
    println!(
        "MAINTENANCE_NATIVE_OK adapter={adapter:?} observations={} producers=COPY,UAV modes=0,1,2 pending_fence=true duplicate_fallback=true",
        batched.len()
    );
    println!("maintenance native parity PASS");
}
