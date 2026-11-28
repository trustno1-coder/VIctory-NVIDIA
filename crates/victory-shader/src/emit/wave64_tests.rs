//! A console wave64 compute program on a host of 32-lane waves: a wave reduction inside a
//! per-lane branch, with that branch taken wave-wide ([`crate::structure::structure_wide`])
//! so that the two host waves can exchange their halves inside it. Lowered, validated by
//! `dxil.dll` when present, and run on the host GPU when it takes waves of 32.

use std::collections::HashSet;

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{ShaderError, io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

/// 64 threads in one console wave. Each thread t holds the bit `1 << (t >> 1)`; the threads
/// below s0 (a root constant) OR theirs over the wave inside `if (t < s0) { ... }` and store
/// the result to dword 0 of the raw UAV.
/// `skip`: the branch past the body when no thread is below s0: `s_cbranch_execz`, or
/// `s_cbranch_scc0` on the SCC `s_and_saveexec` sets ("some lane is left").
fn program(skip: u32) -> NativeProgram {
    let mut words = vec![
        0x7E0A_0281, // v_mov_b32 v5, 1
        0x2C0C_0081, // v_lshrrev_b32 v6, 1, v0
        0x340E_0B06, // v_lshlrev_b32 v7, v6, v5
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        skip | 23,   // the branch to the restore of EXEC
    ];
    words.extend_from_slice(&super::wave::WAVE_OR_V7_TO_S7);
    words.extend_from_slice(&[
        0x7E00_0207, // v_mov_b32 v0, s7
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    ]);
    native(&words)
}

/// A wave64 compute program of 64 threads (one console wave; the thread index in v0) with 4
/// root constants in s0..s3 and a raw UAV's descriptor in s[8:11], then `body`.
fn native(body: &[u32]) -> NativeProgram {
    native_of(body, true, 64)
}

/// The same for a console of 32-lane waves when `wave64` is not set, with `threads` threads.
fn native_of(body: &[u32], wave64: bool, threads: u32) -> NativeProgram {
    let mut words = vec![
        0xBE85_0380, // s_mov_b32 s5, 0
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 2,
        125 << 25, // s_load_dwordx4 s[8:11], s[4:5], 0
    ];
    words.extend_from_slice(body);
    words.push(S_ENDPGM);
    NativeProgram {
        stage: HwStage::Cs,
        wave64,
        vgprs: 16,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1, // 5 user SGPRs: s0..s4
            threads: [threads, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// `program()` for a host of 32-lane waves, with the branches closing `wide` taken wave-wide.
fn lowered(native: &NativeProgram, p: &ir::Program, wide: &HashSet<usize>) -> Result<HostShader, ShaderError> {
    lowered_on(native, p, wide, 32)
}

/// [`lowered`] for a host whose waves have at most `host_wave` lanes.
fn lowered_on(
    native: &NativeProgram,
    p: &ir::Program,
    wide: &HashSet<usize>,
    host_wave: u32,
) -> Result<HostShader, ShaderError> {
    lowered_counted(native, p, wide, host_wave, false)
}

fn lowered_counted(
    native: &NativeProgram,
    p: &ir::Program,
    wide: &HashSet<usize>,
    host_wave: u32,
    counted: bool,
) -> Result<HostShader, ShaderError> {
    let st = structure::structure_wide(p, wide).expect("structure");
    let res = resource::track(p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(p, native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(host_wave));
    let lowered = lower::lower_counted(&lower::Parts {
        native,
        p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    }, counted);
    lower::TEST_HOST_WAVE.set(was);
    let lowered = lowered?;
    Ok(HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        root_indexed_views: res.root_indexed_views,
    })
}

#[test]
fn a_sum_of_halves_over_the_wave_is_the_consoles_tree_bit_for_bit() {
    // thread t holds the 16-bit float with bits 0x3C00 + 37 t (1.0 and up, with odd
    // mantissas, so that additions round); all of them store the wave's sum
    let mut words = vec![
        0x340C_0085, // v_lshlrev_b32 v6, 5, v0
        0x340E_0082, // v_lshlrev_b32 v7, 2, v0
        0x4A0E_0F06, // v_add_nc_u32 v7, v6, v7
        0x4A0E_0F00, // v_add_nc_u32 v7, v0, v7
        0x4A0E_0EFF,
        0x0000_3C00, // v_add_nc_u32 v7, 0x3c00, v7
    ];
    // (the wave OR's words with v_add_f16 for v_or_b32: opcode 50 for 28)
    words.extend(super::wave::WAVE_OR_V7_TO_S7.iter().map(|&w| if w >> 25 == 28 { w & 0x01FF_FFFF | 50 << 25 } else { w }));
    words.extend_from_slice(&[
        0x7E00_0207, // v_mov_b32 v0, s7
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ]);
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    assert_eq!(p.blocks.iter().flat_map(|b| &b.insts).filter(|i| i.name() == "v_add_f16").count(), 6);
    let shader = lowered(&native, &p, &HashSet::new()).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    // the console's sum: lanes 2k and 2k+1, then pairs, ... each addition rounded to 16 bits
    // (to nearest, ties to even); values here are normal numbers
    let wide = |h: u16| f32::from_bits((u32::from(h >> 10) + 112) << 23 | u32::from(h & 0x3FF) << 13);
    let narrow = |f: f32| {
        let b = f.to_bits();
        let h = ((b >> 23) - 112) << 10 | (b >> 13 & 0x3FF);
        let rest = b & 0x1FFF;
        (h + u32::from(rest > 0x1000 || rest == 0x1000 && h & 1 == 1)) as u16
    };
    let add = |a: u16, b: u16| narrow(wide(a) + wide(b));
    let terms: Vec<u16> = (0..64u16).map(|t| 0x3C00 + 37 * t).collect();
    let mut tree = terms.clone();
    while tree.len() > 1 {
        tree = tree.chunks(2).map(|c| add(c[0], c[1])).collect();
    }
    let in_a_row = terms.iter().copied().reduce(add).unwrap();
    assert_ne!(tree[0], in_a_row, "these terms tell the tree from another order");
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let (root, pso) = match host.compute_pipeline(&shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("the hardware device does not take this program (waves of 32?): {e}; not run");
            return;
        }
    };
    let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(1)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    // (the high half is lane 0's term's: zero)
    assert_eq!(out[0][0], u32::from(tree[0]), "tree {:#x}, in a row {in_a_row:#x}", tree[0]);
    eprintln!("ran on the hardware device: the sum {:#x} of the console's tree", tree[0]);
}

/// "Some lane is left" of a mask operation as a number: an upscaler's programs wait for
/// other groups in a loop that ends when every lane saw its flag, or after enough rounds:
/// both tests become -1 or 0 and SCC of `timeout | ~waiting` ends it.
#[test]
fn some_lane_of_a_mask_is_a_number_the_whole_wave_agrees_on() {
    let native = native(&[
        0xD765_0001,
        0x0001_00C1, // v_mbcnt_lo_u32_b32 v1, -1, 0
        0xD766_0001,
        0x0002_02C1, // v_mbcnt_hi_u32_b32 v1, -1, v1
        0x7D84_0200, // v_cmp_eq_u32 vcc, s0, v1
        0x8792_7E6A, // s_and_b64 s[18:19], vcc, exec
        0x8592_80C1, // s_cselect_b64 s[18:19], -1, 0
        0xBF09_8501, // s_cmp_ge_u32 s1, 5
        0x8594_80C1, // s_cselect_b64 s[20:21], -1, 0
        0x8B92_1214, // s_orn2_b64 s[18:19], s[20:21], s[18:19]
        0x8506_8081, // s_cselect_b32 s6, 1, 0
        0x7E04_0206, // v_mov_b32 v2, s6
        0xE070_0000,
        0x8002_0200, // buffer_store_dword v2, off, s[8:11], 0
    ]);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let shader = on_64_lanes(&native, &p);
    // (lane s0 waits, if there is one; s1 rounds)
    for (wait, rounds, done) in [(0, 0, 0), (37, 4, 0), (63, 4, 0), (64, 0, 1), (37, 5, 1), (64, 9, 1)] {
        let Some(out) = dispatched(&shader, &[wait, rounds]) else {
            return;
        };
        assert_eq!(out, done, "lane {wait} after {rounds} rounds");
    }
}

/// `native` for a host of 64-lane waves.
fn on_64_lanes(native: &NativeProgram, p: &ir::Program) -> HostShader {
    let st = structure::structure(p).expect("structure");
    let res = resource::track(p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(p, native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(64));
    let lowered = lower::lower(&lower::Parts {
        native,
        p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    });
    lower::TEST_HOST_WAVE.set(was);
    let lowered = lowered.expect("lower");
    assert_eq!(lowered.wave_size, Some(64));
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        root_indexed_views: res.root_indexed_views,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    shader
}

/// Dword 0 of the UAV after `shader` ran with root constants `constants`; none without a
/// device that takes it.
fn dispatched(shader: &HostShader, constants: &[u32]) -> Option<u32> {
    dispatched_words(shader, constants, 1).map(|words| words[0])
}

/// The first `dwords` dwords of the UAV after `shader` ran with root constants `constants`;
/// none without a device that takes it.
fn dispatched_words(shader: &HostShader, constants: &[u32], dwords: u32) -> Option<Vec<u32>> {
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return None;
    };
    let (root, pso) = match host.compute_pipeline(shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("the hardware device does not take this program (waves of 64?): {e}; not run");
            return None;
        }
    };
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        if u.root == RootType::Constants {
            ud[u.index as usize] = constants.get(u.dword as usize).copied().unwrap_or(0);
        }
    }
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(dwords)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0][..dwords as usize].to_vec())
}

/// The same sum inside `if (t >= s0) { ... }` on a host of 64-lane waves: the lanes below s0
/// are elsewhere, and count as +0 in the console's tree.
#[test]
fn a_sum_of_halves_where_not_every_lane_runs_on_a_wave_of_64() {
    let mut words = vec![
        0x340C_0085, // v_lshlrev_b32 v6, 5, v0
        0x340E_0082, // v_lshlrev_b32 v7, 2, v0
        0x4A0E_0F06, // v_add_nc_u32 v7, v6, v7
        0x4A0E_0F00, // v_add_nc_u32 v7, v0, v7
        0x4A0E_0EFF,
        0x0000_3C00, // v_add_nc_u32 v7, 0x3c00, v7
        0x7D86_0000, // v_cmp_le_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0000 | 23, // s_cbranch_execz to the restore of EXEC
    ];
    words.extend(super::wave::WAVE_OR_V7_TO_S7.iter().map(|&w| if w >> 25 == 28 { w & 0x01FF_FFFF | 50 << 25 } else { w }));
    words.extend_from_slice(&[
        0x7E00_0207, // v_mov_b32 v0, s7
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    ]);
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let shader = on_64_lanes(&native, &p);
    let wide = |h: u16| match h {
        0 => 0.0,
        h => f32::from_bits((u32::from(h >> 10) + 112) << 23 | u32::from(h & 0x3FF) << 13),
    };
    let narrow = |f: f32| {
        if f == 0.0 {
            return 0;
        }
        let b = f.to_bits();
        let h = ((b >> 23) - 112) << 10 | (b >> 13 & 0x3FF);
        let rest = b & 0x1FFF;
        (h + u32::from(rest > 0x1000 || rest == 0x1000 && h & 1 == 1)) as u16
    };
    for from in [0u32, 1, 5, 31, 32, 37, 63] {
        let mut tree: Vec<u16> = (0..64u16).map(|t| if u32::from(t) < from { 0 } else { 0x3C00 + 37 * t }).collect();
        while tree.len() > 1 {
            tree = tree.chunks(2).map(|c| narrow(wide(c[0]) + wide(c[1]))).collect();
        }
        let Some(out) = dispatched(&shader, &[from]) else {
            return;
        };
        assert_eq!(out, u32::from(tree[0]), "lanes from {from}");
    }
}

/// The console's 16-bit wave sum where the lanes at or past s0 have left the program (a
/// thread past the end of the data): on the console they add +0, and after each step of the
/// tree they hold their run's partial sum all the same. On a host of 64-lane waves (AMD) the
/// lanes that are here read each partner run's sum from a lane of it that is here.
#[test]
fn a_sum_of_halves_where_lanes_have_left_is_the_consoles_tree_bit_for_bit() {
    let mut words = vec![
        0x7DA8_0000, // v_cmpx_gt_u32 exec, s0, v0: the lanes below s0 stay
        0, // s_cbranch_execz to the end (filled in below)
        0x340C_0085, // v_lshlrev_b32 v6, 5, v0
        0x340E_0082, // v_lshlrev_b32 v7, 2, v0
        0x4A0E_0F06, // v_add_nc_u32 v7, v6, v7
        0x4A0E_0F00, // v_add_nc_u32 v7, v0, v7
        0x4A0E_0EFF,
        0x0000_3C00, // v_add_nc_u32 v7, 0x3c00, v7
    ];
    words.extend(super::wave::WAVE_OR_V7_TO_S7.iter().map(|&w| if w >> 25 == 28 { w & 0x01FF_FFFF | 50 << 25 } else { w }));
    words.extend_from_slice(&[
        0x7E00_0207, // v_mov_b32 v0, s7
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ]);
    // past every word after it, to the program's end
    words[1] = 0xBF88_0000 | (words.len() as u32 - 2);
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let shader = lowered_on(&native, &p, &HashSet::new(), 64).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let wide = |h: u16| {
        if h == 0 {
            0.0
        } else {
            f32::from_bits((u32::from(h >> 10) + 112) << 23 | u32::from(h & 0x3FF) << 13)
        }
    };
    let narrow = |f: f32| {
        if f == 0.0 {
            return 0;
        }
        let b = f.to_bits();
        let h = ((b >> 23) - 112) << 10 | (b >> 13 & 0x3FF);
        let rest = b & 0x1FFF;
        (h + u32::from(rest > 0x1000 || rest == 0x1000 && h & 1 == 1)) as u16
    };
    let add = |a: u16, b: u16| narrow(wide(a) + wide(b));
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut ran = false;
    for stay in [64u32, 63, 40, 33, 32, 17, 5, 1] {
        let terms: Vec<u16> = (0..64u32)
            .map(|t| if t < stay { (0x3C00 + 37 * t) as u16 } else { 0 })
            .collect();
        let mut tree = terms.clone();
        while tree.len() > 1 {
            tree = tree.chunks(2).map(|c| add(c[0], c[1])).collect();
        }
        let Some(got) = on_the_card(&shader, [stay, 0]) else {
            return;
        };
        ran = true;
        // (the high half is lane 0's term's: zero)
        assert_eq!(got, u32::from(tree[0]), "{stay} lanes stay: got {got:#x}, the console's tree {:#x}", tree[0]);
    }
    if ran {
        eprintln!("ran on the hardware device: the console's tree with lanes gone, in every case");
    }
}

/// One `v_add_f16` whose exact result lies between two halves: 1 + 3/1024 (0x3C03) plus 2
/// (0x4000) is 3 + 1.5 steps of that range, which the console rounds up to 0x4202. The
/// host's `f32tof16` may give 0x4201 (toward zero); with `VICTORY_F16_NEAREST=1` the
/// translation gives the console's whatever the host does.
#[test]
fn a_16_bit_sum_rounds_to_nearest_when_asked_to() {
    let words = [
        0x7E0E_02FF,
        0x0000_3C03, // v_mov_b32 v7, 0x3c03
        0x7E0C_02FF,
        0x0000_4000, // v_mov_b32 v6, 0x4000
        0x6400_0D07, // v_add_f16 v0, v7, v6
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ];
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    assert!(p.blocks.iter().flat_map(|b| &b.insts).any(|i| i.name() == "v_add_f16"));
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for nearest in [false, true] {
        let was = lower::TEST_F16_NEAREST.replace(Some(nearest));
        let shader = lowered(&native, &p, &HashSet::new());
        lower::TEST_F16_NEAREST.set(was);
        let shader = shader.expect("lower");
        if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
            let mut copy = shader.dxil.clone();
            v.validate(&mut copy).expect("DXIL validation failed");
        }
        let Ok(host) = Host::new(false) else {
            eprintln!("no SM 6.6 hardware device; not run");
            return;
        };
        let Ok((root, pso)) = host.compute_pipeline(&shader) else {
            eprintln!("the hardware device does not take this program; not run");
            return;
        };
        let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(1)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        let got = out[0][0] & 0xFFFF;
        eprintln!("v_add_f16 0x3c03 + 0x4000 on the hardware device, nearest {nearest}: {got:#x}");
        if nearest {
            assert_eq!(got, 0x4202);
        } else {
            assert!(got == 0x4201 || got == 0x4202, "{got:#x}");
        }
    }
}

#[test]
fn a_wave_or_inside_a_lane_branch_needs_the_branch_wave_wide() {
    for (skip, name) in [(0xBF88_0000, "s_cbranch_execz"), (0xBF84_0000, "s_cbranch_scc0")] {
        a_wave_or_inside(skip, name);
    }
}

fn a_wave_or_inside(skip: u32, name: &str) {
    let native = program(skip);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let branch = (0..p.blocks.len())
        .find(|&b| p.blocks[b].insts.last().is_some_and(|i| i.name() == name))
        .expect("the branch");
    // per lane, the two host waves cannot meet inside the branch
    lower::take_needs();
    let per_lane = lowered(&native, &p, &HashSet::new());
    assert!(
        matches!(&per_lane, Err(ShaderError::Unsupported { what, .. }) if *what == "a wave reduction where not every lane runs"),
        "{:?}",
        per_lane.err()
    );
    // ... and the lowering names the block, around which that branch is the one to widen
    let needs = lower::take_needs();
    let st = structure::structure(&p).expect("structure");
    assert_eq!(lower::branches_around(&st, &needs), Some(vec![branch]));
    // (taken, past the body, when every lane's bit is clear)
    assert_eq!(st.wave_any[branch], Some(false));
    // wave-wide, every lane runs the reduction (EXEC stays a guard on what it does there)
    let wide = HashSet::from([branch]);
    let st = structure::structure_wide(&p, &wide).expect("structure");
    let inside = match p.blocks[branch].exit {
        ir::Exit::Branch { not_taken, .. } => not_taken,
        _ => unreachable!(),
    };
    assert!(st.guards[inside].iter().any(Option::is_some));
    let shader = lowered(&native, &p, &wide).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    // On a GPU that takes waves of 32 (two host waves for the console's one): the OR is of
    // both halves, whichever half's lanes take the branch.
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let (root, pso) = match host.compute_pipeline(&shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("the hardware device does not take this program (waves of 32?): {e}; not run");
            return;
        }
    };
    // (threads below s0 take the branch; thread t gives bit t >> 1)
    for (below, expected) in [(64u32, u32::MAX), (40, 0x000F_FFFF), (33, 0x0001_FFFF), (10, 0x1F), (1, 1)] {
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            if let (RootType::Constants, 0, 0) = (u.root, u.slot, u.dword) {
                ud[u.index as usize] = below;
            }
        }
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(1)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        assert_eq!(out[0][0], expected, "{name}: {below} threads in the branch");
    }
    eprintln!("ran on the hardware device ({name}): the OR of both halves in every case");
}

/// Runs `shader` on the hardware device with the root constants `s0`, `s1` and gives dword 0
/// of the UAV; None (and a line) without a device that takes it.
fn on_the_card(shader: &HostShader, constants: [u32; 2]) -> Option<u32> {
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return None;
    };
    let Ok((root, pso)) = host.compute_pipeline(shader) else {
        eprintln!("the hardware device does not take this program; not run");
        return None;
    };
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        if let (RootType::Constants, 0, d @ 0..=1) = (u.root, u.slot, u.dword) {
            ud[u.index as usize] = constants[d as usize];
        }
    }
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(1)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0][0])
}

/// A 64-bit mask the program does not track, read as a number by a select, in a console wave of
/// 64 that runs as two host waves of 32 (the DPP move makes it take the console's lanes, so the
/// halves form): `v_cndmask_b32` reads s[2:3] (root constants), so each lane takes its own bit of
/// it, lanes 32-63 the high dword's. Their host lane numbers are 0-31 in the second host wave, so
/// they must be numbered by their console lane, not by the host lane.
#[test]
fn lanes_past_32_take_the_high_dword_of_a_mask_held_as_a_number() {
    let words = [
        0x7E0E_02FA,
        0xFFFF_B100, // v_mov_b32_dpp v7, v0 quad_perm:[1,0,3,2] row_mask:0xf bank_mask:0xf
        0xD501_0005,
        0x0009_0280, // v_cndmask_b32 v5, 0, 1, s[2:3]
        0x340C_0082, // v_lshlrev_b32 v6, 2, v0
        0xE070_1000,
        0x8002_0506, // buffer_store_dword v5, v6, s[8:11], 0 offen
    ];
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let shader = lowered_on(&native, &p, &HashSet::new(), 32).expect("lower");
    for (lo, hi) in [(0x5555_0F0F, 0xA0F0_1234), (u32::MAX, 0), (0, u32::MAX)] {
        let Some(out) = dispatched_words(&shader, &[0, 0, lo, hi], 64) else {
            return;
        };
        for lane in 0..64u32 {
            let bit = if lane < 32 {
                (lo >> lane) & 1
            } else {
                (hi >> (lane - 32)) & 1
            };
            assert_eq!(
                out[lane as usize], bit,
                "lane {lane} of the mask {hi:08x}{lo:08x}"
            );
        }
    }
}

/// A 32-lane console wave. A register holds a lane mask on one path (the lanes below s0)
/// and a number on the other, and is read as a number after the join, as the trace programs
/// keep their lanes' masks and their node numbers on one stack
/// (`deferred_tiled:CS_LaserLightShadowRaytracing`, s46 at 0x104, 0x75c and 0x188). The
/// mask arrives as the ballot of its lanes.
#[test]
fn a_lane_mask_joined_with_a_number_is_read_as_the_number_it_is() {
    let words = [
        0x7D88_0000, // v_cmp_gt_u32 vcc_lo, s0, v0
        0xBE94_036A, // s_mov_b32 s20, vcc_lo
        0xBF06_8001, // s_cmp_eq_u32 s1, 0
        0xBF85_0001, // s_cbranch_scc1 over the next
        0xBE94_0385, // s_mov_b32 s20, 5
        0x7E00_0214, // v_mov_b32 v0, s20
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ];
    let native = native_of(&words, false, 32);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let shader = lowered(&native, &p, &HashSet::new()).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for (below, other, expected) in [(32u32, 0u32, u32::MAX), (10, 0, 0x3FF), (0, 0, 0), (10, 1, 5)] {
        let Some(got) = on_the_card(&shader, [below, other]) else { return };
        assert_eq!(got, expected, "{below} lanes in the mask, s1 = {other}");
    }
    eprintln!("ran on the hardware device: the mask's bits, or the number");
}

/// A 32-lane console wave. `if (t < s0) { ... SCC = (s1 == 7) }` and a branch on SCC after
/// the join: the skip brings the SCC of `s_and_saveexec` ("some lane is left") along
/// (`rtdirectionalshadows:CS_RTShadowTrace32HQ_TiledFarOnly`, 0x360-0x39c). On the console
/// the skip is taken by the wave, so with some lane inside every lane sees the inner test.
/// A lane that skipped on its own would see "some lane is left" instead: refused per lane,
/// right with the branch taken wave-wide. Lane 31 stores what it saw.
#[test]
fn scc_of_a_mask_operation_joined_with_a_scalar_test_needs_the_skip_wave_wide() {
    let words = [
        0x7D88_0000, // v_cmp_gt_u32 vcc_lo, s0, v0
        0xBE8D_3C6A, // s_and_saveexec_b32 s13, vcc_lo
        0xBF88_0001, // s_cbranch_execz over the next
        0xBF06_8701, // s_cmp_eq_u32 s1, 7
        0xBEFE_030D, // s_mov_b32 exec_lo, s13
        0xBF85_0002, // s_cbranch_scc1 to "seven"
        0x7E0A_0281, // v_mov_b32 v5, 1
        0xBF82_0001, // s_branch over the next
        0x7E0A_0282, // seven: v_mov_b32 v5, 2
        0x7DA4_009F, // v_cmpx_eq_u32 exec_lo, 31, v0
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16 | 5 << 8, // buffer_store_dword v5, off, s[8:11], 0
    ];
    let native = native_of(&words, false, 32);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let branch = (0..p.blocks.len())
        .find(|&b| p.blocks[b].insts.last().is_some_and(|i| i.name() == "s_cbranch_execz"))
        .expect("the skip");
    lower::take_needs();
    let per_lane = lowered(&native, &p, &HashSet::new());
    assert!(
        matches!(&per_lane, Err(ShaderError::Unsupported { what, .. }) if *what == "a lane mask read as data"),
        "{:?}",
        per_lane.err()
    );
    let st = structure::structure(&p).expect("structure");
    assert_eq!(lower::branches_around(&st, &lower::take_needs()), Some(vec![branch]));
    let shader = lowered(&native, &p, &HashSet::from([branch])).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    // (lanes below s0 inside, s1) -> what lane 31 stores: 2 if SCC was set at the branch
    for (below, s1, expected) in [(0u32, 7u32, 1u32), (10, 7, 2), (10, 3, 1), (32, 3, 1), (32, 7, 2)] {
        let Some(got) = on_the_card(&shader, [below, s1]) else { return };
        assert_eq!(got, expected, "{below} lanes inside, s1 = {s1}");
    }
    eprintln!("ran on the hardware device: the inner test's SCC for every lane once some lane is inside");
}

/// A dispatch counted in threads: of the last group only the lanes below the
/// count store (after DexNom/VIctory bc3cd4b0).
#[test]
fn the_lanes_past_a_count_of_threads_do_not_run() {
    // Each lane that runs adds 1 to counter[local x].
    let native = native(&[
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0
        0x7e04_0281, // v_mov_b32 v2, 1
        0xe0c8_1000, 0x8002_0201, // buffer_atomic_add v2, v1, s[8:11], 0 offen
    ]);
    let p = ir::build(&native.code, native.wave64, native.vgprs).unwrap();
    let shader = lowered_counted(&native, &p, &HashSet::new(), 64, true).unwrap();
    let limit = shader
        .user_data
        .iter()
        .find(|u| u.root == RootType::Other(super::USER_DISPATCH_THREADS_X))
        .unwrap();
    assert_eq!((limit.slot, limit.dword), (0, 64));
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(true) else { return };
    let Ok((root, pso)) = host.compute_pipeline(&shader) else { return };
    for count in [0u32, 1, 7, 63, 64, 65, 74, 77, 127, 128] {
        let mut constants = vec![0; shader.user_data_dwords() as usize];
        constants[limit.index as usize] = count;
        let result = host
            .dispatch(&root, &pso, &Dispatch {
                roots: vec![(0, Root::Constants(constants))],
                views: &[View::RawUav(68)],
                samplers: &[],
                groups: [2, 1, 1],
            })
            .unwrap();
        let active = if count == 0 { 128 } else { count };
        for (i, &word) in result[0].iter().enumerate().take(64) {
            let i = i as u32;
            assert_eq!(word, u32::from(i < active) + u32::from(i + 64 < active), "count {count}, word {i}");
        }
    }
}

/// The wave size a lowering of `native` at a host of 32-lane waves asks for, and the shader.
fn lowered_with_wave(
    native: &NativeProgram,
    p: &ir::Program,
    wide: &HashSet<usize>,
) -> (Option<u32>, HostShader) {
    let st = structure::structure_wide(p, wide).expect("structure");
    let res = resource::track(p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(p, native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let lowered = lower::lower(&lower::Parts {
        native,
        p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    });
    lower::TEST_HOST_WAVE.set(was);
    let lowered = lowered.expect("lower");
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        root_indexed_views: res.root_indexed_views,
    };
    (lowered.wave_size, shader)
}

/// Two console waves in a group of 128 threads, each run as two host waves of 32. Lane t goes
/// round a per-lane loop (t >> 4) + 1 times: it writes its sum to LDS and adds that of lane
/// t ^ 63 (in the other host wave of its console wave) with no barrier between, as one console
/// wave may; after the loop every lane adds lane 63's sum (`v_readlane_b32`). The loop's back
/// edge is taken as the console takes it, by each console wave as a whole, and the two console
/// waves go round 4 and 8 times: their host waves meet per console wave
/// ([`lower::with_pair_meetings`]). A group barrier in the loop would wait for the threads of
/// the console wave that left it (`CS_DefragHeap_ComputeRelocations`, groups of 128, whose
/// console waves walk a heap each its own way).
#[test]
fn console_waves_of_one_group_meet_on_their_own_in_loops_of_their_own() {
    let words = [
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0: this lane's LDS dword
        0x3A06_00BF, // v_xor_b32 v3, 63, v0
        0x3406_0682, // v_lshlrev_b32 v3, 2, v3: the partner lane's (other half, same wave)
        0x4A04_0000, // v_add_nc_u32 v2, s0, v0: the sum starts at s0 + t
        0x2C0E_0084, // v_lshrrev_b32 v7, 4, v0
        0x4A0E_0E81, // v_add_nc_u32 v7, 1, v7: rounds (t >> 4) + 1
        0x7E0A_0280, // v_mov_b32 v5, 0
        0xBE8E_047E, // s_mov_b64 s[14:15], exec
        0xD834_0000,
        0x0000_0201, // loop: ds_write_b32 v1, v2
        0xD8D8_0000,
        0x0400_0003, // ds_read_b32 v4, v3
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0x4A04_0902, // v_add_nc_u32 v2, v2, v4
        0x4A0A_0A81, // v_add_nc_u32 v5, 1, v5
        0x7DA8_0B07, // v_cmpx_gt_u32 exec, v7, v5: the lanes with rounds left
        0xBF89_FFF7, // s_cbranch_execnz loop
        0xBEFE_040E, // s_mov_b64 exec, s[14:15]
        0xD760_0007,
        0x0001_7F02, // v_readlane_b32 s7, v2, 63
        0x4A04_0407, // v_add_nc_u32 v2, s7, v2
        0xE070_1000,
        0x8002_0201, // buffer_store_dword v2, v1, s[8:11], 0 offen
    ];
    let mut native = native_of(&words, true, 128);
    if let StageIo::Cs(cs) = &mut native.io {
        cs.rsrc2 |= 1 << 15; // 512 bytes of LDS
    }
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let back = (0..p.blocks.len())
        .find(|&b| p.blocks[b].insts.last().is_some_and(|i| i.name() == "s_cbranch_execnz"))
        .expect("the back edge");
    // per lane, the loop's LDS write and read stand where not every lane runs: still 64 lanes,
    // and the lowering names the loop
    lower::take_needs();
    let (wave, _) = lower::with_pair_meetings(|| lowered_with_wave(&native, &p, &HashSet::new()));
    assert_eq!(wave, Some(64));
    let needs = lower::take_needs();
    let st = structure::structure(&p).expect("structure");
    assert_eq!(lower::with_pair_meetings(|| lower::branches_around(&st, &needs)), Some(vec![back]));
    // the back edge taken per console wave: waves of 32, meeting per console wave
    let wide = HashSet::from([back]);
    let (wave, shader) = lower::with_pair_meetings(|| lowered_with_wave(&native, &p, &wide));
    assert_eq!(wave, Some(32));
    assert!(lower::take_needs().is_empty());
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    // the console: each wave's lanes in step, writes before reads
    let console = |s0: u32| {
        let mut out = [0u32; 128];
        let mut lds = [0u32; 128];
        for w in 0..2u32 {
            let lanes = 64 * w..64 * w + 64;
            let mut sum: Vec<u32> = lanes.clone().map(|t| s0.wrapping_add(t)).collect();
            for round in 0..8 {
                let on: Vec<u32> = lanes.clone().filter(|t| round < (t >> 4) + 1).collect();
                if on.is_empty() {
                    break;
                }
                for &t in &on {
                    lds[t as usize] = sum[(t - 64 * w) as usize];
                }
                for &t in &on {
                    let k = (t - 64 * w) as usize;
                    sum[k] = sum[k].wrapping_add(lds[(t ^ 63) as usize]);
                }
            }
            let last = sum[63];
            for t in lanes {
                out[t as usize] = sum[(t - 64 * w) as usize].wrapping_add(last);
            }
        }
        out
    };
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let (root, pso) = match host.compute_pipeline(&shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("the hardware device does not take this program: {e}; not run");
            return;
        }
    };
    for s0 in [0u32, 1000, 0xFFFF_FF00] {
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            if let (RootType::Constants, 0, 0) = (u.root, u.slot, u.dword) {
                ud[u.index as usize] = s0;
            }
        }
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(128)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        let want = console(s0);
        assert_eq!(&out[0][..128], &want[..], "s0 = {s0:#x}");
    }
    eprintln!("ran on the hardware device: both console waves' loops, as the console runs them");
}

/// Which LDS accesses need the host waves of their console wave to meet
/// ([`lower::LdsOrder`]): before a barrier a lane writes its dword and reads it back (the
/// order counts); after it the lanes read and add with atomics only (it does not: a read
/// there may see another lane's atomic, so it is not taken as alike in every lane).
#[test]
fn lds_accesses_after_the_last_plain_write_need_no_meeting() {
    let words = [
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0
        0xD834_0000,
        0x0000_0001, // ds_write_b32 v1, v0
        0xD8D8_0000,
        0x0400_0001, // ds_read_b32 v4, v1
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xBF8A_0000, // s_barrier
        0xD8D8_0000,
        0x0500_0001, // ds_read_b32 v5, v1
        0xD800_0000,
        0x0000_0001, // ds_add_u32 v1, v0
    ];
    let mut native = native_of(&words, true, 128);
    if let StageIo::Cs(cs) = &mut native.io {
        cs.rsrc2 |= 1 << 15;
    }
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let order = lower::lds_order(&p, &st);
    let name = |&(b, i): &(usize, usize)| (p.blocks[b].insts[i].name(), p.blocks[b].insts[i].machine.offset);
    let mut unordered: Vec<_> = order.unordered.iter().map(name).collect();
    unordered.sort_unstable_by_key(|x| x.1);
    let unstable: Vec<_> = order.unstable.iter().map(name).collect();
    assert_eq!(unordered.iter().map(|x| x.0).collect::<Vec<_>>(), ["ds_read_b32", "ds_add_u32"]);
    assert_eq!(unstable.iter().map(|x| x.0).collect::<Vec<_>>(), ["ds_read_b32"]);
    assert!(unordered[0].1 > 0x10, "the read after the barrier");
}

/// A console wave32 program with console lanes (`v_mbcnt`) in a 32 x 2 group: thread (x, y)
/// stores 1 at dword 32y + x. Every dword must be written once: the host's relabelled thread
/// ids are a bijection of the group. A 32-lane host need not give a host wave a 32-aligned run
/// of flat ids (2-D groups are filled from 16 x 2 tiles on NVIDIA and RDNA), so the first flat
/// id of a wave rounded down gave two waves the same ids (issue #1243); the waves now take
/// their threads from a group counter. With `VICTORY_THREAD_TICKET=0` (the old rounding) the
/// count of dwords written is printed, not checked: it depends on the card.
#[test]
fn the_console_threads_of_a_two_dimensional_group_are_each_given_once() {
    let words = [
        0xD765_0003, // v_mbcnt_lo_u32_b32 v3, exec_lo, 0
        0x0001_007E,
        0x3408_0285, // v_lshlrev_b32 v4, 5, v1
        0x4A08_0104, // v_add_nc_u32 v4, v4, v0
        0x3408_0882, // v_lshlrev_b32 v4, 2, v4
        0x7E0A_0281, // v_mov_b32 v5, 1
        0x38 << 26 | 0x1C << 18 | 1 << 12,
        0x80 << 24 | 2 << 16 | 5 << 8 | 4, // buffer_store_dword v5, v4, s[8:11], 0 offen
    ];
    let mut native = native_of(&words, false, 64);
    native.io = StageIo::Cs(ComputeIo {
        rsrc2: 5 << 1 | 1 << 11, // 5 user SGPRs, thread ids x and y in v0, v1
        threads: [32, 2, 1],
    });
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let lower_with = |ticket: bool| {
        // SAFETY: tests that read this switch hold ONE_AT_A_TIME
        unsafe { std::env::set_var("VICTORY_THREAD_TICKET", if ticket { "1" } else { "0" }) };
        let shader = lowered_on(&native, &p, &HashSet::new(), 32);
        unsafe { std::env::remove_var("VICTORY_THREAD_TICKET") };
        shader.expect("lower")
    };
    let shader = lower_with(true);
    let old = lower_with(false);
    assert_ne!(shader.dxil, old.dxil, "the counter is used for a group of two host waves");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let run = |shader: &HostShader| -> Option<Vec<u32>> {
        let (root, pso) = match host.compute_pipeline(shader) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("the hardware device does not take this program: {e}; not run");
                return None;
            }
        };
        let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(64)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        Some(out[0][..64].to_vec())
    };
    let Some(got) = run(&shader) else { return };
    let missing: Vec<usize> = (0..64).filter(|&i| got[i] != 1).collect();
    assert!(missing.is_empty(), "dwords not written once: {missing:?}");
    if let Some(before) = run(&old) {
        let written = before.iter().filter(|&&d| d == 1).count();
        eprintln!("the old rounding on this device: {written} of 64 dwords written");
    }
    eprintln!("ran on the hardware device: 64 of 64 console threads, each once");
}

fn native_lds64(body: &[u32], lds_blocks: u32) -> NativeProgram {
    let mut prog = native(body);
    if let StageIo::Cs(cs) = &mut prog.io {
        cs.rsrc2 |= lds_blocks << 15;
    }
    prog
}

fn ds_enc_w(op: u8, off0: u8, off1: u8, addr: u8, data0: u8, data1: u8, vdst: u8) -> [u32; 2] {
    let w0 = (0x36u32 << 26) | (u32::from(op) << 18) | (u32::from(off1) << 8) | u32::from(off0);
    let w1 = (u32::from(vdst) << 24)
        | (u32::from(data1) << 16)
        | (u32::from(data0) << 8)
        | u32::from(addr);
    [w0, w1]
}

fn buffer_store_uav(vdata: u8, vaddr_bytes: u8) -> [u32; 2] {
    [
        0x38u32 << 26 | 0x1Cu32 << 18 | 0x1000,
        0x80u32 << 24 | 2u32 << 16 | (u32::from(vdata) << 8) | u32::from(vaddr_bytes),
    ]
}

fn run_wave32_lds_on_hardware(
    label: &str,
    native: &NativeProgram,
    constants: [u32; 2],
    uav_dwords: u32,
) -> Option<Vec<u32>> {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let lowered_res = lower::lower(&lower::Parts {
        native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    });
    lower::TEST_HOST_WAVE.set(was);
    let low = lowered_res.expect("wave32 lower failed");
    assert_eq!(low.wave_size, Some(32), "{label}: expected recovered WaveSize(32)");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = low.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: low.dxil,
        user_data: low.user_data,
        samplers: low.samplers,
        static_samplers: Vec::new(),
        root_cbvs: low.root_cbvs,
        root_srvs: low.root_srvs,
        root_uavs: low.root_uavs,
        root_indexed_views: res.root_indexed_views,
    };
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let host = match Host::new(false) {
        Ok(h) => h,
        Err(e) => {
            if std::env::var_os("VICTORY_REQUIRE_HW_GPU").is_some() {
                panic!("hardware SM 6.6 D3D12 device required (VICTORY_REQUIRE_HW_GPU=1): {e}");
            }
            eprintln!("no SM 6.6 hardware device; not run");
            return None;
        }
    };
    let (root, pso) = match host.compute_pipeline(&shader) {
        Ok(x) => x,
        Err(e)
            if std::env::var_os("VICTORY_REQUIRE_HW_GPU").is_none()
                && (std::env::var("VICTORY_WARP").is_ok_and(|v| v == "1")
                    || e.contains("maximum wave lane count")) =>
        {
            eprintln!("{label}: device does not support WaveSize(32) ({e}); not run");
            return None;
        }
        Err(e) => panic!("{label}: hardware GPU must accept recovered WaveSize(32) pipeline: {e}"),
    };
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        if let (RootType::Constants, 0, d @ 0..=1) = (u.root, u.slot, u.dword) {
            ud[u.index as usize] = constants[d as usize];
        }
    }
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(uav_dwords)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("{label}: hardware dispatch failed: {e}"));
    eprintln!("{label}: ran recovered WaveSize(32) on hardware D3D12 device (no skip)");
    Some(out.into_iter().next().unwrap())
}

/// Refinement A hardware GPU test: inside a per-lane branch across both 32-lane halves of a
/// console Wave64 group, writes `[16, 20)` while reading `[4, 12)`, plus a post-branch read
/// of `[16, 20)`. Executes on hardware D3D12 with bit-exact 64-lane + barrier semantics.
#[test]
fn lds_disjoint_ranges_in_lane_branch_execute_wave32_on_hardware() {
    // Init (all block): lds[tid * 4] = (tid + 1) * 10
    // Inside if (v0 < s0): ds_write_b32(16, s1), r4 = ds_read_b32(4), r8 = ds_read_b32(8),
    //   uav[tid] = r4 + r8 + tid;
    // After branch (all block): uav[64 + tid] = ds_read_b32(16)
    let v_mov_s = |d: u8, s: u8| 0x7E00_0200 | (u32::from(d) << 17) | u32::from(s);
    let v_mov_k = |d: u8, k: u8| 0x7E00_0200 | (u32::from(d) << 17) | u32::from(128 + k);
    let v_lshlrev_k = |d: u8, sh: u8, s: u8| {
        0x3400_0000 | (u32::from(d) << 17) | (u32::from(s) << 9) | u32::from(128 + sh)
    };
    let v_add_k = |d: u8, k: u8, s: u8| {
        0x4A00_0000 | (u32::from(d) << 17) | (u32::from(s) << 9) | u32::from(128 + k)
    };
    let v_add_vv = |d: u8, a: u8, b: u8| {
        0x4A00_0000 | (u32::from(d) << 17) | (u32::from(b) << 9) | (256 + u32::from(a))
    };
    let v_mul24_k = |d: u8, k: u8, s: u8| {
        0x1600_0000 | (u32::from(d) << 17) | (u32::from(s) << 9) | u32::from(128 + k)
    };

    let mut inner = vec![
        v_mov_k(2, 0),
        v_mov_s(3, 1), // v3 = s1
    ];
    inner.extend(ds_enc_w(0x0d, 16, 0, 2, 3, 0, 0)); // ds_write_b32 addr=0+16, data=v3
    inner.extend(ds_enc_w(0x36, 4, 0, 2, 0, 0, 4)); // v4 = ds_read_b32 addr=0+4 (=20)
    inner.extend(ds_enc_w(0x36, 8, 0, 2, 0, 0, 5)); // v5 = ds_read_b32 addr=0+8 (=30)
    inner.push(v_add_vv(6, 4, 5));
    inner.push(v_add_vv(6, 6, 0)); // v6 = 20 + 30 + tid
    inner.push(v_lshlrev_k(7, 2, 0)); // v7 = tid * 4
    inner.extend(buffer_store_uav(6, 7));

    let mut words = vec![
        v_lshlrev_k(2, 2, 0),     // v2 = tid * 4
        v_add_k(3, 1, 0),         // v3 = tid + 1
        v_mul24_k(3, 10, 3),      // v3 = (tid + 1) * 10
    ];
    words.extend(ds_enc_w(0x0d, 0, 0, 2, 3, 0, 0)); // lds[tid * 4] = v3
    words.extend([
        0x7D88_0000,                        // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A,                        // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0000 | (inner.len() as u32), // s_cbranch_execz
    ]);
    words.extend_from_slice(&inner);
    words.push(0xBEFE_040C); // s_mov_b64 exec, s[12:13]
    words.push(v_mov_k(2, 0));
    words.extend(ds_enc_w(0x36, 16, 0, 2, 0, 0, 8)); // v8 = lds[16]
    words.push(v_lshlrev_k(7, 2, 0));
    words.push(0x4A0E_0EFF);
    words.push(256); // v_add_nc_u32 v7, 256, v7
    words.extend(buffer_store_uav(8, 7));

    let native = native_lds64(&words, 1);
    for &(active, secret) in &[(48u32, 0xCAFE_1234u32), (64, 0xABCD_5678), (12, 0x9988_7766)] {
        let Some(out) = run_wave32_lds_on_hardware("lds_disjoint_ranges", &native, [active, secret], 128) else {
            return;
        };
        for tid in 0..64u32 {
            let expected_branch = if tid < active { 50 + tid } else { 0 };
            assert_eq!(out[tid as usize], expected_branch, "tid={tid}, active={active}");
            assert_eq!(out[(64 + tid) as usize], secret, "post-barrier tid={tid}, active={active}");
        }
    }
}

/// Refinement A hardware GPU test with dual-offset `ds_write2_b32`/`ds_read2_b32` and
/// 128-bit `ds_write_b128`/`ds_read_b128` inside a divergent branch across both halves.
#[test]
fn lds_dual_and_b128_disjoint_execute_wave32_on_hardware() {
    let v_mov_k = |d: u8, k: u8| 0x7E00_0200 | (u32::from(d) << 17) | u32::from(128 + k);
    let v_lshlrev_k = |d: u8, sh: u8, s: u8| {
        0x3400_0000 | (u32::from(d) << 17) | (u32::from(s) << 9) | u32::from(128 + sh)
    };
    let v_add_k = |d: u8, k: u8, s: u8| {
        0x4A00_0000 | (u32::from(d) << 17) | (u32::from(s) << 9) | u32::from(128 + k)
    };
    let v_add_vv = |d: u8, a: u8, b: u8| {
        0x4A00_0000 | (u32::from(d) << 17) | (u32::from(b) << 9) | (256 + u32::from(a))
    };

    // Init all: lds[tid * 4] = tid + 10 (fills [0..256)).
    // Branch (v0 < s0):
    //   ds_write2_b32 off0=64 (256), off1=65 (260), data0=v3(=11), data1=v4(=22) -> [256, 264)
    //   ds_read2_b32 v[8:9], addr=0, off0=2 (8), off1=3 (12) -> [8, 16) (= 12, 13)
    //   ds_write_b128 addr=0+272, v[10:13] (= 1, 2, 3, 4) -> [272, 288)
    //   ds_read_b128 v[12:15], addr=0+16 (= 14, 15, 16, 17) -> [16, 32)
    //   uav[tid] = v8 + v9 + v12 + v15 = 12 + 13 + 14 + 17 = 56
    // After branch: uav[64 + tid] = lds[256] + lds[260] + lds[272] + lds[284] = 11 + 22 + 1 + 4 = 38.
    let mut inner = vec![
        v_mov_k(2, 0),
        v_mov_k(3, 11),
        v_mov_k(4, 22),
        v_mov_k(10, 1),
        v_mov_k(11, 2),
        v_mov_k(12, 3),
        v_mov_k(13, 4),
    ];
    inner.extend(ds_enc_w(0x0e, 64, 65, 2, 3, 4, 0)); // ds_write2_b32
    inner.extend(ds_enc_w(0x37, 2, 3, 2, 0, 0, 8)); // ds_read2_b32 -> v[8:9]
    inner.extend(ds_enc_w(0xdf, 272u16 as u8, (272u16 >> 8) as u8, 2, 10, 0, 0)); // ds_write_b128
    inner.extend(ds_enc_w(0xff, 16, 0, 2, 0, 0, 12)); // ds_read_b128 -> v[12:15]
    inner.push(v_add_vv(6, 8, 9));
    inner.push(v_add_vv(6, 6, 12));
    inner.push(v_add_vv(6, 6, 15));
    inner.push(v_lshlrev_k(7, 2, 0));
    inner.extend(buffer_store_uav(6, 7));

    let mut words = vec![v_lshlrev_k(2, 2, 0), v_add_k(3, 10, 0)];
    words.extend(ds_enc_w(0x0d, 0, 0, 2, 3, 0, 0));
    words.extend([
        0x7D88_0000,
        0xBE8C_246A,
        0xBF88_0000 | (inner.len() as u32),
    ]);
    words.extend_from_slice(&inner);
    words.push(0xBEFE_040C);
    words.push(v_mov_k(2, 0));
    words.extend(ds_enc_w(0x37, 64, 65, 2, 0, 0, 8)); // v[8:9] = [11, 22]
    words.extend(ds_enc_w(0xff, 272u16 as u8, (272u16 >> 8) as u8, 2, 0, 0, 12)); // v[12:15] = [1, 2, 3, 4]
    words.push(v_add_vv(6, 8, 9));
    words.push(v_add_vv(6, 6, 12));
    words.push(v_add_vv(6, 6, 15));
    words.push(v_lshlrev_k(7, 2, 0));
    words.push(0x4A0E_0EFF);
    words.push(256);
    words.extend(buffer_store_uav(6, 7));

    let native = native_lds64(&words, 2);
    let Some(out) = run_wave32_lds_on_hardware("lds_dual_and_b128_disjoint", &native, [44, 0], 128) else {
        return;
    };
    for tid in 0..64u32 {
        assert_eq!(out[tid as usize], if tid < 44 { 56 } else { 0 }, "tid={tid}");
        assert_eq!(out[(64 + tid) as usize], 38, "post tid={tid}");
    }
}

/// Refinement B hardware GPU test: lane-private 8-byte struct (`tid * 8 + 0`, `tid * 8 + 4`)
/// with a returning atomic `ds_add_rtn_u32` and private read/write inside a divergent branch
/// on hardware D3D12, compared against exact Wave64 results.
#[test]
fn lds_lane_private_with_rtn_atomic_executes_wave32_on_hardware() {
    let v_mov_k = |d: u8, k: u8| 0x7E00_0200 | (u32::from(d) << 17) | u32::from(128 + k);
    let v_lshlrev_k = |d: u8, sh: u8, s: u8| {
        0x3400_0000 | (u32::from(d) << 17) | (u32::from(s) << 9) | u32::from(128 + sh)
    };
    let v_add_lit = |d: u8, lit: u32, s: u8| {
        [
            0x4A00_00FF | (u32::from(d) << 17) | (u32::from(s) << 9),
            lit,
        ]
    };
    let v_add_vv = |d: u8, a: u8, b: u8| {
        0x4A00_0000 | (u32::from(d) << 17) | (u32::from(b) << 9) | (256 + u32::from(a))
    };
    let v_mul24_k = |d: u8, k: u8, s: u8| {
        0x1600_0000 | (u32::from(d) << 17) | (u32::from(s) << 9) | u32::from(128 + k)
    };

    // Init all: lds[tid * 8 + 0] = 100 + tid, lds[tid * 8 + 4] = 200 + tid * 3.
    // Inside if (v0 < s0):
    //   old = ds_add_rtn_u32(tid * 8 + 0, 50)
    //   b   = ds_read_b32(tid * 8 + 4)
    //   ds_write_b32(tid * 8 + 4, old + b)
    //   uav[tid] = old
    // After branch (all):
    //   uav[64 + tid]  = lds[tid * 8 + 0]
    //   uav[128 + tid] = lds[tid * 8 + 4]
    let mut inner = vec![v_lshlrev_k(2, 3, 0), v_mov_k(3, 50)];
    inner.extend(ds_enc_w(0x20, 0, 0, 2, 3, 0, 4)); // v4 = ds_add_rtn_u32
    inner.extend(ds_enc_w(0x36, 4, 0, 2, 0, 0, 5)); // v5 = lds[tid*8 + 4]
    inner.push(v_add_vv(6, 4, 5));
    inner.extend(ds_enc_w(0x0d, 4, 0, 2, 6, 0, 0)); // lds[tid*8 + 4] = old + b
    inner.push(v_lshlrev_k(7, 2, 0));
    inner.extend(buffer_store_uav(4, 7));

    let mut words = vec![v_lshlrev_k(2, 3, 0)];
    words.extend(v_add_lit(3, 100, 0));
    words.extend(ds_enc_w(0x0d, 0, 0, 2, 3, 0, 0));
    words.push(v_mul24_k(4, 3, 0));
    words.extend(v_add_lit(4, 200, 4));
    words.extend(ds_enc_w(0x0d, 4, 0, 2, 4, 0, 0));
    words.extend([
        0x7D88_0000,
        0xBE8C_246A,
        0xBF88_0000 | (inner.len() as u32),
    ]);
    words.extend_from_slice(&inner);
    words.push(0xBEFE_040C);
    words.push(v_lshlrev_k(2, 3, 0));
    words.extend(ds_enc_w(0x36, 0, 0, 2, 0, 0, 8));
    words.extend(ds_enc_w(0x36, 4, 0, 2, 0, 0, 9));
    words.push(v_lshlrev_k(7, 2, 0));
    words.extend(v_add_lit(7, 256, 7));
    words.extend(buffer_store_uav(8, 7));
    words.push(v_lshlrev_k(7, 2, 0));
    words.extend(v_add_lit(7, 512, 7));
    words.extend(buffer_store_uav(9, 7));

    let native = native_lds64(&words, 2);
    for &active in &[40u32, 64] {
        let Some(out) = run_wave32_lds_on_hardware("lds_lane_private_rtn_atomic", &native, [active, 0], 192) else {
            return;
        };
        for tid in 0..64u32 {
            let active_lane = tid < active;
            let expected_old = if active_lane { 100 + tid } else { 0 };
            let expected_slot0 = if active_lane { 150 + tid } else { 100 + tid };
            let expected_slot1 = if active_lane {
                300 + 4 * tid
            } else {
                200 + 3 * tid
            };
            assert_eq!(out[tid as usize], expected_old, "old tid={tid}");
            assert_eq!(out[(64 + tid) as usize], expected_slot0, "slot0 tid={tid}");
            assert_eq!(out[(128 + tid) as usize], expected_slot1, "slot1 tid={tid}");
        }
    }
}

/// Negative Wave64 hardware test: when a divergent region contains a returning atomic at a
/// shared uniform address, lowering keeps `WaveSize(64)` and does NOT unsafely lower to 32.
#[test]
fn lds_shared_returning_atomic_stays_wave64() {
    let v_mov_k = |d: u8, k: u8| 0x7E00_0200 | (u32::from(d) << 17) | u32::from(128 + k);
    let mut inner = vec![v_mov_k(2, 0), v_mov_k(3, 1)];
    inner.extend(ds_enc_w(0x20, 0, 0, 2, 3, 0, 4));
    inner.extend([0x38u32 << 26 | 0x1Cu32 << 18, 0x80u32 << 24 | 2u32 << 16 | (4 << 8)]);
    let mut words = vec![
        0x7D88_0000,
        0xBE8C_246A,
        0xBF88_0000 | (inner.len() as u32),
    ];
    words.extend_from_slice(&inner);
    words.push(0xBEFE_040C);
    let native = native_lds64(&words, 1);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, &native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let low = lower::lower(&lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    })
    .expect("lower");
    lower::TEST_HOST_WAVE.set(was);
    assert_eq!(low.wave_size, Some(64));
}

/// Negative + opt-in hardware test for unproven dynamic write-only LDS in a divergent region:
/// keeps `WaveSize(64)` by default, and lowers/executes as `WaveSize(32)` only when
/// `VICTORY_LDS_WRITE_ONLY_OK=1` is enabled on a 32-lane host.
#[test]
fn lds_unresolved_dynamic_waw_stays_wave64_by_default_and_lowers_only_with_opt_in() {
    let v_mov_k = |d: u8, k: u8| 0x7E00_0200 | (u32::from(d) << 17) | u32::from(128 + k);
    let v_lshlrev_k = |d: u8, sh: u8, s: u8| {
        0x3400_0000 | (u32::from(d) << 17) | (u32::from(s) << 9) | u32::from(128 + sh)
    };
    // `v_mul_hi_u32 v2, v0, v0` evaluates to 0 for `tid` in 0..64 while staying
    // unmodelled in `eval_inst`, producing an unresolved dynamic address (`expr == None`).
    let v_mul_hi_u32 = |d: u8, a: u8, b: u8| -> [u32; 2] {
        [
            (0xD4u32 << 24) | (0x16Au32 << 16) | u32::from(d),
            ((256 + u32::from(b)) << 9) | (256 + u32::from(a)),
        ]
    };

    let mut inner = Vec::new();
    inner.extend(v_mul_hi_u32(2, 0, 0)); // runtime 0, statically `expr == None`
    inner.push(v_mov_k(3, 11));
    inner.push(v_mov_k(4, 22));
    inner.extend(ds_enc_w(0x0d, 0, 0, 2, 3, 0, 0)); // ds_write_b32 [v2], 11
    inner.extend(ds_enc_w(0x0d, 0, 0, 2, 4, 0, 0)); // ds_write_b32 [v2], 22

    let mut words = vec![
        0x7D88_0000,                        // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A,                        // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0000 | (inner.len() as u32), // s_cbranch_execz
    ];
    words.extend_from_slice(&inner);
    words.push(0xBEFE_040C); // s_mov_b64 exec, s[12:13]
    words.push(v_mov_k(2, 0));
    words.extend(ds_enc_w(0x36, 0, 0, 2, 0, 0, 5)); // v5 = lds[0]
    words.push(v_lshlrev_k(7, 2, 0));
    words.extend(buffer_store_uav(5, 7));

    let native = native_lds64(&words, 1);

    // 1. Default (`VICTORY_LDS_WRITE_ONLY_OK` off): MUST stay Wave64.
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, &native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let low_default = super::lds::with_test_write_only_ok(false, || {
        lower::lower(&lower::Parts {
            native: &native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: Vec::new(),
            outputs: Vec::new(),
            prims: Vec::new(),
            mesh_stride: 0,
        })
        .expect("lower")
    });
    lower::TEST_HOST_WAVE.set(was);
    assert_eq!(
        low_default.wave_size,
        Some(64),
        "unresolved dynamic write-only LDS must stay Wave64 by default"
    );

    // 2. Opt-in (`VICTORY_LDS_WRITE_ONLY_OK=1`): lowers to Wave32 and runs on hardware D3D12.
    let out = super::lds::with_test_write_only_ok(true, || {
        run_wave32_lds_on_hardware("lds_unresolved_dynamic_waw_opt_in", &native, [64, 0], 64)
    });
    if let Some(out) = out {
        for tid in 0..64usize {
            assert_eq!(out[tid], 22, "tid={tid}");
        }
    }
}
