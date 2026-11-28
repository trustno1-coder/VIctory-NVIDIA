//! The console's maximum, minimum and median written as the host's own
//! FMax and FMin ([`super::nvidia_float`]): what an NVIDIA card gives that
//! way against the forms spelled out for every host, bit for bit, where a
//! constant -0 keeps the spelled-out form, and what the two cost on the card
//! (ignored: `cargo test --release -p victory-shader --lib native_float_costs
//! -- --ignored --nocapture`).

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

use super::host::Host;
use super::valu_tests::{
    ONE_AT_A_TIME, build_cs, s_mov_b32, sgpr, sop2, vgpr, vop1, vop3, vop3_mod,
};
use super::{HostShader, lower, rdna};
use crate::native::RootType;

const MAX: u16 = 0x110;
const MIN: u16 = 0x10F;
const MAX3: u16 = 0x154;
const MIN3: u16 = 0x151;
const MED3: u16 = 0x157;
const ADD: u16 = 0x103;
const MUL: u16 = 0x108;
const FMA: u16 = 0x14B;
const MAX_F16: u16 = 0x139;
const MIN_F16: u16 = 0x13A;
const MAX3_F16: u16 = 0x354;
const MIN3_F16: u16 = 0x351;
const MED3_F16: u16 = 0x357;
/// Inline constants 0, 1.0 and -1.0, and the literal that follows.
const ZERO: u16 = 128;
const ONE: u16 = 242;
const MINUS_ONE: u16 = 243;
const LITERAL: u16 = 255;
const NEG_ZERO: u32 = 0x8000_0000;

fn op(code: u16, dst: u8, a: u16, b: u16, c: u16) -> Vec<u32> {
    vop3(code, dst, a, b, c).to_vec()
}

/// [`op`] with the literal `k` for the source that names it.
fn with_literal(mut words: Vec<u32>, k: u32) -> Vec<u32> {
    words.push(k);
    words
}

/// A compute shader of `body` with the float forms of `bits`.
fn shader(body: &[u32], bits: u32, threads: u32) -> HostShader {
    lower::TEST_NATIVE_FLOAT.set(Some(bits));
    let shader = build_cs(body, threads, 0);
    lower::TEST_NATIVE_FLOAT.set(None);
    shader
}

#[test]
fn a_constant_negative_zero_keeps_the_spelled_out_form() {
    let (a, b) = (vgpr(1), vgpr(2));
    let negated_zero = |code| vop3_mod(code, 0, a, ZERO, 0, 0, 2, 0, false).to_vec();
    let moved = |then: Vec<u32>| [vec![vop1(0x001, 6, LITERAL), NEG_ZERO], then].concat();
    let spelled = [
        (
            "max with a literal -0",
            with_literal(op(MAX, 0, a, LITERAL, 0), NEG_ZERO),
        ),
        (
            "min with a literal -0",
            with_literal(op(MIN, 0, LITERAL, a, 0), NEG_ZERO),
        ),
        ("max with a negated 0", negated_zero(MAX)),
        ("min with a negated 0", negated_zero(MIN)),
        (
            "max with a -0 moved to a register",
            moved(op(MAX, 0, a, vgpr(6), 0)),
        ),
        (
            "med3 from -0",
            with_literal(op(MED3, 0, a, LITERAL, ONE), NEG_ZERO),
        ),
        (
            "med3 up to -0",
            with_literal(op(MED3, 0, a, MINUS_ONE, LITERAL), NEG_ZERO),
        ),
    ];
    for (name, body) in spelled {
        assert_eq!(
            shader(&body, 0, 1).dxil,
            shader(&body, rdna::NVIDIA, 1).dxil,
            "{name}: not spelled out"
        );
    }
    let native = [
        ("max", op(MAX, 0, a, b, 0)),
        ("min with 0", op(MIN, 0, a, ZERO, 0)),
        ("med3 0 1", op(MED3, 0, a, ZERO, ONE)),
    ];
    for (name, body) in native {
        assert_ne!(
            shader(&body, 0, 1).dxil,
            shader(&body, rdna::NVIDIA, 1).dxil,
            "{name}: spelled out"
        );
    }
}

/// A compute pipeline recorded once, to run any number of times: for each of
/// its cases the root constants, a dispatch of `groups` groups and a copy of
/// the UAV's first dword, with a timestamp before the first and after the last.
struct Recorded {
    queue: ID3D12CommandQueue,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    /// One dword per case, then (8-byte aligned) the two timestamps.
    back: ID3D12Resource,
    cases: usize,
    runs: std::cell::Cell<u64>,
    /// What the list names, alive as long as it is.
    _kept: Vec<windows::core::IUnknown>,
}

impl Recorded {
    fn new(host: &Host, shader: &HostShader, cases: &[[u32; 4]], groups: u32) -> Self {
        let device = &host.device;
        let (root, pso) = host.compute_pipeline(shader).expect("a compute pipeline");
        let ticks_at = (cases.len() * 4).next_multiple_of(8);
        // SAFETY: descriptions that outlive the calls; every object the list
        // names is kept with it.
        unsafe {
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .unwrap();
            let allocator: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .unwrap();
            let list: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, &pso)
                .unwrap();
            let buffer = |heap, bytes: u64, flags, state| {
                let mut made: Option<ID3D12Resource> = None;
                device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES {
                            Type: heap,
                            ..Default::default()
                        },
                        D3D12_HEAP_FLAG_NONE,
                        &D3D12_RESOURCE_DESC {
                            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                            Width: bytes,
                            Height: 1,
                            DepthOrArraySize: 1,
                            MipLevels: 1,
                            SampleDesc: DXGI_SAMPLE_DESC {
                                Count: 1,
                                Quality: 0,
                            },
                            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                            Flags: flags,
                            ..Default::default()
                        },
                        state,
                        None,
                        &mut made,
                    )
                    .unwrap();
                made.unwrap()
            };
            let out = buffer(
                D3D12_HEAP_TYPE_DEFAULT,
                256,
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COMMON,
            );
            let back = buffer(
                D3D12_HEAP_TYPE_READBACK,
                ticks_at as u64 + 16,
                D3D12_RESOURCE_FLAG_NONE,
                D3D12_RESOURCE_STATE_COPY_DEST,
            );
            let views: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 1,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .unwrap();
            device.CreateUnorderedAccessView(
                &out,
                None,
                Some(&D3D12_UNORDERED_ACCESS_VIEW_DESC {
                    Format: DXGI_FORMAT_R32_TYPELESS,
                    ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                    Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_UAV {
                            NumElements: 64,
                            Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                            ..Default::default()
                        },
                    },
                }),
                views.GetCPUDescriptorHandleForHeapStart(),
            );
            let mut queries: Option<ID3D12QueryHeap> = None;
            device
                .CreateQueryHeap(
                    &D3D12_QUERY_HEAP_DESC {
                        Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                        Count: 2,
                        NodeMask: 0,
                    },
                    &mut queries,
                )
                .unwrap();
            let queries = queries.unwrap();
            list.SetDescriptorHeaps(&[Some(views.clone())]);
            list.SetComputeRootSignature(&root);
            list.EndQuery(&queries, D3D12_QUERY_TYPE_TIMESTAMP, 0);
            let (write, read) = (
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            );
            for (i, inputs) in cases.iter().enumerate() {
                let mut user = vec![0u32; shader.user_data_dwords().max(1) as usize];
                for u in &shader.user_data {
                    if u.root == RootType::Constants {
                        user[u.index as usize] = inputs[u.dword as usize];
                    }
                }
                list.SetComputeRoot32BitConstants(0, user.len() as u32, user.as_ptr().cast(), 0);
                list.Dispatch(groups, 1, 1);
                list.ResourceBarrier(&[super::host::transition(&out, write, read)]);
                list.CopyBufferRegion(&back, i as u64 * 4, &out, 0, 4);
                list.ResourceBarrier(&[super::host::transition(&out, read, write)]);
            }
            list.EndQuery(&queries, D3D12_QUERY_TYPE_TIMESTAMP, 1);
            list.ResolveQueryData(
                &queries,
                D3D12_QUERY_TYPE_TIMESTAMP,
                0,
                2,
                &back,
                ticks_at as u64,
            );
            list.Close().unwrap();
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let kept = vec![
                root.cast().unwrap(),
                pso.cast().unwrap(),
                allocator.cast().unwrap(),
                out.cast().unwrap(),
                views.cast().unwrap(),
                queries.cast().unwrap(),
            ];
            Recorded {
                queue,
                list,
                fence,
                back,
                cases: cases.len(),
                runs: Default::default(),
                _kept: kept,
            }
        }
    }

    /// Runs the list and waits: the dword after each case, and the card's
    /// milliseconds from the first to the last.
    fn run(&self) -> (Vec<u32>, f64) {
        let ticks_at = (self.cases * 4).next_multiple_of(8);
        self.runs.set(self.runs.get() + 1);
        // SAFETY: a closed list of ours, waited for before it is read or run
        // again; the mapping holds the dwords and the two timestamps.
        unsafe {
            let list: ID3D12CommandList = self.list.cast().unwrap();
            self.queue.ExecuteCommandLists(&[Some(list)]);
            self.queue.Signal(&self.fence, self.runs.get()).unwrap();
            // Without an event: returns once the fence has the value.
            self.fence
                .SetEventOnCompletion(self.runs.get(), Default::default())
                .unwrap();
            let mut mapped = std::ptr::null_mut();
            self.back.Map(0, None, Some(&mut mapped)).unwrap();
            let found = std::slice::from_raw_parts(mapped.cast::<u32>(), self.cases).to_vec();
            let ticks = mapped.cast::<u8>().add(ticks_at).cast::<[u64; 2]>().read();
            self.back.Unmap(0, None);
            let frequency = self.queue.GetTimestampFrequency().unwrap();
            (found, (ticks[1] - ticks[0]) as f64 * 1e3 / frequency as f64)
        }
    }
}

/// The hardware adapter, if it is an NVIDIA card's.
fn nvidia_card() -> Option<Host> {
    let host = Host::new(false).ok()?;
    // SAFETY: plain queries.
    let nvidia = unsafe {
        CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            .and_then(|f| f.EnumAdapterByLuid::<IDXGIAdapter1>(host.device.GetAdapterLuid()))
            .and_then(|a| a.GetDesc1())
            .is_ok_and(|d| d.VendorId == 0x10DE)
    };
    nvidia.then_some(host)
}

/// Every pair of `numbers` with a third picked three ways, then 4,000
/// pseudo-random bit patterns, all cut to `mask`.
fn inputs(numbers: &[u32], mask: u32) -> Vec<[u32; 4]> {
    let mut cases = Vec::new();
    for (i, &x) in numbers.iter().enumerate() {
        for (j, &y) in numbers.iter().enumerate() {
            for k in [3 * i + j, 5 * j + i + 1, i + j + 2] {
                cases.push([x, y, numbers[k % numbers.len()], 0]);
            }
        }
    }
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..4000 {
        let (x, y) = (next(), next());
        cases.push([x as u32 & mask, (x >> 32) as u32 & mask, y as u32 & mask, 0]);
    }
    cases
}

#[test]
fn nvidia_forms_give_what_the_spelled_out_ones_give() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(host) = nvidia_card() else {
        eprintln!("no NVIDIA card; skipped");
        return;
    };
    let (a, b, c, t) = (vgpr(1), vgpr(2), vgpr(3), vgpr(5));
    let modified =
        |code, abs, neg, omod, clamp| vop3_mod(code, 0, a, b, 0, abs, neg, omod, clamp).to_vec();
    // `first` into v5, then `then` of v5.
    let chain = |first: Vec<u32>, then: Vec<u32>| [first, then].concat();
    let negative_zero = |words| with_literal(words, NEG_ZERO);
    let f32_bodies = vec![
        ("max", op(MAX, 0, a, b, 0)),
        ("min", op(MIN, 0, a, b, 0)),
        ("max of a value with itself", op(MAX, 0, a, a, 0)),
        ("max with 0", op(MAX, 0, a, ZERO, 0)),
        ("min with 0", op(MIN, 0, ZERO, a, 0)),
        ("max with -0", negative_zero(op(MAX, 0, a, LITERAL, 0))),
        ("min with -0", negative_zero(op(MIN, 0, a, LITERAL, 0))),
        (
            "max of 0 and -0",
            negative_zero(op(MAX, 0, ZERO, LITERAL, 0)),
        ),
        (
            "min of 0 and -0",
            negative_zero(op(MIN, 0, ZERO, LITERAL, 0)),
        ),
        (
            "max with infinity",
            with_literal(op(MAX, 0, a, LITERAL, 0), f32::NEG_INFINITY.to_bits()),
        ),
        (
            "min with a NaN",
            with_literal(op(MIN, 0, a, LITERAL, 0), f32::NAN.to_bits()),
        ),
        ("max negated", modified(MAX, 0, 1, 0, false)),
        ("min of a magnitude", modified(MIN, 1, 2, 0, false)),
        ("max clamped", modified(MAX, 0, 0, 0, true)),
        ("max halved", modified(MAX, 0, 0, 3, false)),
        ("max3", op(MAX3, 0, a, b, c)),
        ("min3", op(MIN3, 0, a, b, c)),
        ("max3 with 0", op(MAX3, 0, a, ZERO, b)),
        ("med3 0 1", op(MED3, 0, a, ZERO, ONE)),
        ("med3 1 0", op(MED3, 0, ONE, a, ZERO)),
        ("med3 -1 1", op(MED3, 0, MINUS_ONE, ONE, a)),
        ("med3 -0 1", negative_zero(op(MED3, 0, a, LITERAL, ONE))),
        (
            "med3 -1 -0",
            negative_zero(op(MED3, 0, a, MINUS_ONE, LITERAL)),
        ),
        ("med3 of a NaN or a number", op(MED3, 0, a, b, c)),
        (
            "max 0, min 1",
            chain(op(MAX, 5, a, ZERO, 0), op(MIN, 0, t, ONE, 0)),
        ),
        (
            "min 1, max 0",
            chain(op(MIN, 5, a, ONE, 0), op(MAX, 0, t, ZERO, 0)),
        ),
        (
            "max -0, min 1",
            chain(
                negative_zero(op(MAX, 5, a, LITERAL, 0)),
                op(MIN, 0, t, ONE, 0),
            ),
        ),
        (
            "min 1, max -0",
            chain(
                op(MIN, 5, a, ONE, 0),
                negative_zero(op(MAX, 0, t, LITERAL, 0)),
            ),
        ),
        (
            "max negated 0, min 1",
            chain(
                vop3_mod(MAX, 5, a, ZERO, 0, 0, 2, 0, false).to_vec(),
                op(MIN, 0, t, ONE, 0),
            ),
        ),
        (
            "max of a moved -0, min 1",
            chain(
                vec![vop1(0x001, 6, LITERAL), NEG_ZERO],
                chain(op(MAX, 5, a, vgpr(6), 0), op(MIN, 0, t, ONE, 0)),
            ),
        ),
        (
            "a product saturated",
            chain(
                op(MUL, 5, a, b, 0),
                chain(op(MAX, 6, t, ZERO, 0), op(MIN, 0, vgpr(6), ONE, 0)),
            ),
        ),
        (
            "a product, max -0, min 1",
            chain(
                op(MUL, 5, a, b, 0),
                chain(
                    negative_zero(op(MAX, 6, t, LITERAL, 0)),
                    op(MIN, 0, vgpr(6), ONE, 0),
                ),
            ),
        ),
        (
            "a sum, med3 0 1",
            chain(op(ADD, 5, a, b, 0), op(MED3, 0, t, ZERO, ONE)),
        ),
    ];
    let f16_bodies = vec![
        ("max_f16", op(MAX_F16, 0, a, b, 0)),
        ("min_f16", op(MIN_F16, 0, a, b, 0)),
        ("max_f16 with 0", op(MAX_F16, 0, a, ZERO, 0)),
        ("max3_f16", op(MAX3_F16, 0, a, b, c)),
        ("min3_f16", op(MIN3_F16, 0, a, b, c)),
        ("med3_f16", op(MED3_F16, 0, a, b, c)),
        ("med3_f16 0 1", op(MED3_F16, 0, a, ZERO, ONE)),
        (
            "max_f16 -0, min_f16 1",
            chain(
                with_literal(op(MAX_F16, 5, a, LITERAL, 0), 0x8000),
                op(MIN_F16, 0, t, ONE, 0),
            ),
        ),
        (
            "max_f16 negated 0, min_f16 1",
            chain(
                vop3_mod(MAX_F16, 5, a, ZERO, 0, 0, 2, 0, false).to_vec(),
                op(MIN_F16, 0, t, ONE, 0),
            ),
        ),
    ];
    let f32_numbers: Vec<u32> = [
        0.0f32,
        -0.0,
        1.0,
        -1.0,
        0.5,
        -0.5,
        0.3,
        -2.5e-7,
        2.0,
        -2.0,
        3e38,
        -3e38,
        1e-30,
        -1e-30,
        f32::MIN_POSITIVE,
        -f32::MIN_POSITIVE,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NAN,
    ]
    .iter()
    .map(|x| x.to_bits())
    // NaNs of both signs and signalling, denormals, the neighbours of 1.
    .chain([
        0xFFC0_0000,
        0x7F80_0001,
        0xFF80_0001,
        1,
        0x8000_0001,
        0x007F_FFFF,
        0x807F_FFFF,
        0x3F7F_FFFF,
        0x3F80_0001,
    ])
    .collect();
    let f16_numbers = [
        0x0000, 0x8000, 0x3C00, 0xBC00, 0x3800, 0xB800, 0x4000, 0xC000, 0x7C00, 0xFC00, 0x7E00,
        0xFE00, 0x7C01, 0x0001, 0x8001, 0x03FF, 0x83FF, 0x0400, 0x8400, 0x7BFF, 0xFBFF, 0x3BFF,
        0x3C01,
    ];
    let mut wrong = Vec::new();
    for (bodies, numbers, mask, infinity) in [
        (f32_bodies, &f32_numbers[..], u32::MAX, 0x7F80_0000u32),
        (f16_bodies, &f16_numbers[..], 0xFFFF, 0x7C00),
    ] {
        let cases = inputs(numbers, mask);
        let nan = |x: u32| x & (mask >> 1) > infinity;
        for (name, body) in bodies {
            let [spelled, native] = [0, rdna::NVIDIA].map(|bits| {
                Recorded::new(&host, &shader(&body, bits, 1), &cases, 1)
                    .run()
                    .0
            });
            let differ: Vec<usize> = (0..cases.len())
                .filter(|&i| spelled[i] != native[i] && !(nan(spelled[i]) && nan(native[i])))
                .collect();
            if let Some(&i) = differ.first() {
                wrong.push(format!(
                    "{name}: {} of {} differ, as {:x?}: {:#x}, natively {:#x}",
                    differ.len(),
                    cases.len(),
                    &cases[i][..3],
                    spelled[i],
                    native[i]
                ));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// What the two forms cost on the card: kernels of arithmetic with a maximum,
/// minimum or median in so many of their instructions, 131,072 threads of
/// 4,096 rounds, the forms turn by turn; medians of 61 runs each.
#[test]
#[ignore = "a measurement on the card: cargo test --release -p victory-shader --lib native_float_costs -- --ignored --nocapture"]
fn native_float_costs() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let host = Host::new(false).expect("a graphics card");
    let v = vgpr;
    let (a, b) = (sgpr(1), sgpr(2));
    // v1..v4 each times s1 plus the next: nothing a driver can fold.
    let arithmetic = |n: usize| -> Vec<u32> {
        (0..n)
            .flat_map(|i| {
                op(
                    FMA,
                    1 + (i % 4) as u8,
                    v(1 + (i % 4) as u8),
                    a,
                    v(1 + ((i + 1) % 4) as u8),
                )
            })
            .collect()
    };
    let kernels = [
        (
            "3 of 6 are max, min or med3",
            [
                op(FMA, 1, v(1), a, v(5)),
                op(MED3, 3, v(1), ZERO, ONE),
                op(MAX, 4, v(3), v(2), 0),
                op(FMA, 2, v(4), b, v(3)),
                op(MIN, 6, v(2), v(1), 0),
                op(ADD, 7, v(7), v(6), 0),
            ]
            .concat(),
        ),
        (
            "2 of 20 (med3, max)",
            [
                arithmetic(9),
                op(MED3, 3, v(1), ZERO, ONE),
                arithmetic(8),
                op(MAX, 4, v(3), v(2), 0),
                op(ADD, 7, v(7), v(4), 0),
            ]
            .concat(),
        ),
        (
            "1 of 10 (max)",
            [
                arithmetic(8),
                op(MAX, 6, v(1), v(2), 0),
                op(ADD, 7, v(7), v(6), 0),
            ]
            .concat(),
        ),
        (
            "none (control)",
            [arithmetic(9), op(ADD, 7, v(7), v(1), 0)].concat(),
        ),
    ];
    for (name, body) in kernels {
        // v5 = the thread's own number * s3; v1..v4 start there, v7 at 0.
        let mut words = vec![vop1(0x006, 5, v(0))];
        words.extend(op(MUL, 5, v(5), sgpr(3), 0));
        words.extend((1..5).map(|d| vop1(0x001, d, v(5))));
        words.push(vop1(0x001, 7, ZERO));
        words.push(s_mov_b32(20, ZERO as u8));
        words.extend_from_slice(&body);
        words.push(sop2(0x00, 20, 20, 129)); // s_add_u32 s20, s20, 1
        words.push(0xBF0A_0014); // s_cmp_lt_u32 s20, s0
        words.push(0xBF85_0000 | u32::from((-(body.len() as i32) - 3) as u16)); // s_cbranch_scc1
        words.push(vop1(0x001, 0, v(7)));
        let constants = [
            4096,
            0.5f32.to_bits(),
            0.37f32.to_bits(),
            (1.0f32 / 131072.0).to_bits(),
        ];
        let forms = [0, rdna::NVIDIA]
            .map(|bits| Recorded::new(&host, &shader(&words, bits, 64), &[constants], 2048));
        // The card's clocks come up.
        for form in &forms {
            for _ in 0..10 {
                form.run();
            }
        }
        let mut ms = [Vec::new(), Vec::new()];
        for round in 0..61 {
            for k in [round % 2, 1 - round % 2] {
                ms[k].push(forms[k].run().1);
            }
        }
        let [spelled, native] = ms.map(|mut m| {
            m.sort_by(f64::total_cmp);
            m[m.len() / 2]
        });
        println!(
            "{name}: spelled out {spelled:.3} ms, the host's own {native:.3} ms, {:+.1} %",
            (native / spelled - 1.0) * 100.0
        );
    }
}
