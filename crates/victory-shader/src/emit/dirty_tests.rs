//! End-to-end tests of GPU dirty-page bitmap (`u<SLOT_DIRTY>`) lowering,
//! `dxil.dll` validation, and execution on the hardware GPU and WARP.

use super::host::{Dispatch, Host, Root, View};
use super::lower::dirty::with_dirty_bitmap;
use super::{
    HostShader, MEMORY_READ_ONLY, SLOT_DIRTY, SLOT_FAULTS, SLOT_MEMORY, USER_MEMORY_BASE,
    USER_ROOT_ADDRESS, USER_VIEW_ADDRESS, USER_VIEW_BIAS, lower, validator,
};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

const fn s_mov_b32(sdst: u8, src0: u8) -> u32 {
    0xBE80_0000 | (sdst as u32) << 16 | 0x03 << 8 | src0 as u32
}

const fn s_mov_lit(sdst: u8, lit: u32) -> [u32; 2] {
    [s_mov_b32(sdst, 0xFF), lit]
}

const fn v_mov_sgpr(vdst: u8, sgpr: u8) -> u32 {
    0x7E00_0200 | (vdst as u32) << 17 | sgpr as u32
}

const fn v_mov_lit(vdst: u8, lit: u32) -> [u32; 2] {
    [0x7E00_02FF | (vdst as u32) << 17, lit]
}

const fn s_load_dwordx4_off(dst: u8, base: u8, byte_off: u32) -> [u32; 2] {
    [
        0xF408_0000 | (dst as u32) << 6 | (base as u32 >> 1),
        0xFA00_0000 | (byte_off & 0x1F_FFFF),
    ]
}

/// MUBUF with `offen` (VGPR byte offset in `vaddr`, plus 12-bit `inst_off`).
const fn mubuf_offen(op: u8, vdata: u8, vaddr: u8, srsrc: u8, inst_off: u16, glc: bool) -> [u32; 2] {
    [
        0x38 << 26 | (op as u32) << 18 | ((glc as u32) << 14) | 1 << 12 | (inst_off as u32 & 0xFFF),
        0x80 << 24 | ((srsrc as u32 >> 2) & 0x1F) << 16 | (vdata as u32) << 8 | vaddr as u32,
    ]
}

/// GLOBAL instruction (`saddr = NULL`, 64-bit VGPR address in `v[vaddr:vaddr+1]`).
const fn global_op(op: u8, vdst: u8, vdata: u8, vaddr: u8, offset: i16, glc: bool) -> [u32; 2] {
    [
        0x37 << 26 | (op as u32) << 18 | ((glc as u32) << 16) | 2 << 14 | (offset as u16 as u32 & 0xFFF),
        (vdst as u32) << 24 | 0x7D << 16 | (vdata as u32) << 8 | vaddr as u32,
    ]
}

fn compile_cs(native: &NativeProgram) -> HostShader {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res =
        resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, native).expect("io::raise");
    let lowered = lower::lower(&lower::Parts {
        native,
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
        dirty_tracked: lowered.dirty_tracked,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        let ours = copy[4..20].to_vec();
        v.validate(&mut copy).expect("DXIL validation failed");
        assert_eq!(copy[4..20], ours[..], "container hash mismatch");
    }
    shader
}

/// Populates user-data root constants for `shader` given `(slot, dword) -> value`
/// for constants, table base byte offsets, and 64-bit view/root addresses.
fn fill_user_data(
    shader: &HostShader,
    constants: &[u32],
    table_bytes: impl Fn(u8) -> u32,
    view_addr: impl Fn(u8, u32) -> u64,
    root_addr: impl Fn(u8) -> u64,
    memory_base: u32,
) -> Vec<u32> {
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        let val = match u.root {
            RootType::Constants => constants.get(u.dword as usize).copied().unwrap_or(0),
            RootType::Table | RootType::ResourceHeap => table_bytes(u.slot),
            RootType::Other(k) if k == USER_VIEW_ADDRESS => view_addr(u.slot, u.dword) as u32,
            RootType::Other(k) if k == USER_VIEW_ADDRESS + 1 => {
                (view_addr(u.slot, u.dword) >> 32) as u32
            }
            RootType::Other(k) if k == USER_ROOT_ADDRESS => root_addr(u.slot) as u32,
            RootType::Other(k) if k == USER_ROOT_ADDRESS + 1 => (root_addr(u.slot) >> 32) as u32,
            RootType::Other(k) if k == USER_VIEW_BIAS => (root_addr(u.slot) & 15) as u32,
            RootType::Other(k) if k == USER_MEMORY_BASE => memory_base,
            other => panic!("unexpected user_data entry: {other:?}"),
        };
        ud[u.index as usize] = val;
    }
    ud
}

/// Overlapping sub-views into a 174 MiB heap (`0x1160_0000`), unaligned view
/// starts, cross-64KiB-page-boundary `buffer_store_dwordx4` (`0xFFFC`), and
/// cross-2MiB-word-boundary `buffer_store_dwordx4` (`0x1F_FFFC`).
#[test]
fn overlapping_subviews_and_page_word_boundaries_set_exact_dirty_bits() {
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut words = vec![s_mov_b32(5, 0x80)];
    // Two table UAVs: UAV 0 at table dword 0, UAV 1 at table dword 8.
    words.extend_from_slice(&s_load_dwordx4_off(8, 4, 0));
    words.extend_from_slice(&s_load_dwordx4_off(12, 4, 32));
    // v1 = s0 (offset in UAV 0), v2 = s1 (offset in UAV 1)
    words.push(v_mov_sgpr(1, 0));
    words.push(v_mov_sgpr(2, 1));
    // v[4:7] = payload
    for (k, val) in [0xAA00_0001u32, 0xAA00_0002, 0xAA00_0003, 0xAA00_0004].into_iter().enumerate() {
        words.extend_from_slice(&v_mov_lit(4 + k as u8, val));
    }
    // buffer_store_dword v4, v1, s[8:11], 0 offen
    words.extend_from_slice(&mubuf_offen(0x1C, 4, 1, 8, 0, false));
    // buffer_store_dwordx4 v[4:7], v2, s[12:15], 0 offen
    words.extend_from_slice(&mubuf_offen(0x1E, 4, 2, 12, 0, false));
    words.push(S_ENDPGM);

    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 10,
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
        used: vec![
            UsedResource {
                kind: ResourceType::Uav,
                root: RootType::Table,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 0,
                dwords: 4,
                user_data: 4,
                slot: 1,
            },
            UsedResource {
                kind: ResourceType::Uav,
                root: RootType::Table,
                table_offset_dwords: 8,
                lower_bound: 1,
                stride: 0,
                dwords: 4,
                user_data: 4,
                slot: 1,
            },
        ],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1,
            threads: [1, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };

    let shader = compile_cs(&native);
    assert!(shader.dirty_tracked, "static table UAV stores must be dirty_tracked");
    assert!(shader.root_uavs.contains(&SLOT_DIRTY));

    // Sub-view 0 starts at unaligned offset 0x1163_4580; sub-view 1 starts at 0x117F_FF00.
    // View sizes: 64 dwords (256 bytes) and 128 dwords (512 bytes).
    let addr0 = 0x1_1163_4580u64; // > 4 GiB 40-bit title address: page = 0x11_1163
    let addr1 = 0x1_117F_FF00u64; // page 0x11_117F (bit 31 of word!), +0xFC..+0x10B crosses into 0x11_1180 (next word!)

    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host.compute_pipeline(&shader).expect("compute_pipeline");

        // Case A: UAV 0 writes offset 0x10 (-> 0x1_1163_4590, page 0x11_1163);
        // UAV 1 writes 16 bytes at offset 0xFC (-> 0x1_117F_FFFC..0x1_1180_000B, crossing BOTH a
        // 64 KiB page boundary AND a 32-page / 2 MiB word boundary!).
        let ud = fill_user_data(
            &shader,
            &[0x10, 0xFC, 0, 0],
            |_| 0,
            |_, dword| if dword == 0 { addr0 } else { addr1 },
            |_| 0,
            0,
        );
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(64), View::RawUav(128)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("{label}: dispatch A: {e}"));
        assert_eq!(out[0][4], 0xAA00_0001);
        assert_eq!(&out[1][63..67], &[0xAA00_0001, 0xAA00_0002, 0xAA00_0003, 0xAA00_0004]);
        assert_eq!(
            host.dirty_pages(),
            vec![0x1_1163, 0x1_117F, 0x1_1180],
            "{label}: expected exact pages across sub-views and 2 MiB word boundary"
        );

        // Case B: OOB offset on UAV 0 (0x4_0000 >= 256 bytes) dropped by hardware -> must NOT
        // mark page 0x1_1167; UAV 1 writes within a single page at 0xEC (0x1_117F_FFEC..FFFB).
        let ud_oob = fill_user_data(
            &shader,
            &[0x4_0000, 0xEC, 0, 0],
            |_| 0,
            |_, dword| if dword == 0 { addr0 } else { addr1 },
            |_| 0,
            0,
        );
        let out_oob = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud_oob))],
                    views: &[View::RawUav(64), View::RawUav(128)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("{label}: dispatch B: {e}"));
        assert!(out_oob[0].iter().all(|&w| w == 0), "{label}: OOB store should be dropped");
        assert_eq!(
            host.dirty_pages(),
            vec![0x1_117F],
            "{label}: OOB store must not set dirty bit, and 0xFFEC..0xFFFB stays in one page"
        );
    }
}

/// Narrow `buffer_store_byte` and `buffer_store_short` (including a 16-bit
/// store straddling a 64 KiB boundary at `0xFFFF`), plus `buffer_atomic_add`
/// and `buffer_atomic_cmpswap`.
#[test]
fn narrow_byte_short_and_buffer_atomics_set_exact_dirty_pages() {
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut words = vec![s_mov_b32(5, 0x80)];
    words.extend_from_slice(&s_load_dwordx4_off(8, 4, 0));
    // v1 = s0 (byte store offset), v2 = s1 (short store offset), v3 = s2 (atomic offset)
    words.push(v_mov_sgpr(1, 0));
    words.push(v_mov_sgpr(2, 1));
    words.push(v_mov_sgpr(3, 2));
    words.extend_from_slice(&v_mov_lit(4, 0xAB));
    words.extend_from_slice(&v_mov_lit(5, 0xBEEF));
    words.extend_from_slice(&v_mov_lit(6, 7));
    words.extend_from_slice(&v_mov_lit(7, 99)); // cmpswap new
    words.extend_from_slice(&v_mov_lit(8, 7)); // cmpswap compare
    // buffer_store_byte v4, v1, s[8:11], 0 offen (op 0x18)
    words.extend_from_slice(&mubuf_offen(0x18, 4, 1, 8, 0, false));
    // buffer_store_short v5, v2, s[8:11], 0 offen (op 0x1A)
    words.extend_from_slice(&mubuf_offen(0x1A, 5, 2, 8, 0, false));
    // buffer_atomic_add v6, v3, s[8:11], 0 offen (op 0x32)
    words.extend_from_slice(&mubuf_offen(0x32, 6, 3, 8, 0, false));
    // buffer_atomic_cmpswap v[7:8], v3, s[8:11], 0 offen (op 0x31)
    words.extend_from_slice(&mubuf_offen(0x31, 7, 3, 8, 0, false));
    words.push(S_ENDPGM);

    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 10,
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
            rsrc2: 5 << 1,
            threads: [1, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };

    let shader = compile_cs(&native);
    assert!(shader.dirty_tracked);

    // UAV base at 0x2000_FF00 (512 bytes = 128 dwords, spanning 0x2000_FF00..0x2001_0100).
    let base_addr = 0x2000_FF00u64;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else { continue };
        let (root, pso) = host.compute_pipeline(&shader).expect("compute_pipeline");

        // Subcase 1: byte at +0xFF (0x2000_FFFF, last byte of page 0x2000!), short at +0x10 (page
        // 0x2000), atomics OOB at 0x1000 -> ONLY page 0x2000 is dirtied!
        let ud1 = fill_user_data(&shader, &[0xFF, 0x10, 0x1000, 0], |_| 0, |_, _| base_addr, |_| 0, 0);
        let out1 = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud1))],
                    views: &[View::RawUav(128)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap();
        assert_eq!(out1[0][0xFF / 4] >> 24, 0xAB);
        assert_eq!(out1[0][0x10 / 4] & 0xFFFF, 0xBEEF);
        assert_eq!(host.dirty_pages(), vec![0x2000], "{label}: byte at 0xFFFF stays in page 0x2000");

        // Subcase 2: short at +0xFF (0x2000_FFFF..0x2001_0000 -> straddles BOTH 0x2000 and 0x2001!),
        // atomics in-bounds at +0x104 (0x2001_0004, page 0x2001).
        let ud2 = fill_user_data(&shader, &[0x04, 0xFF, 0x104, 0], |_| 0, |_, _| base_addr, |_| 0, 0);
        let out2 = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud2))],
                    views: &[View::RawUav(128)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap();
        assert_eq!(out2[0][0xFF / 4] >> 24, 0xEF);
        assert_eq!(out2[0][0x100 / 4] & 0xFF, 0xBE);
        assert_eq!(out2[0][0x104 / 4], 99, "{label}: atomic_add(7) + cmpswap(7 -> 99)");
        assert_eq!(
            host.dirty_pages(),
            vec![0x2000, 0x2001],
            "{label}: straddling buffer_store_short at 0xFFFF must mark both pages"
        );
    }
}

/// Root UAV (`RootType::Descriptor`) starting at an unaligned address
/// (`0x1165_FFF8`, `USER_VIEW_BIAS = 8`): verifies `USER_VIEW_BIAS` is not
/// double-counted in the dirty page address.
#[test]
fn root_uav_subtracts_view_bias_for_exact_page_boundaries() {
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut words = Vec::new();
    // Build synthetic descriptor in s[4:7] around root UAV in s[0:1].
    words.push(s_mov_b32(4, 0));
    words.push(s_mov_b32(5, 1));
    words.push(s_mov_b32(6, 0xC1)); // -1
    words.extend_from_slice(&s_mov_lit(7, 0x1101_4FAC));
    // v1 = s2 (runtime byte offset), v[4:5] = 2 dwords
    words.push(v_mov_sgpr(1, 2));
    words.extend_from_slice(&v_mov_lit(4, 0x1234_5678));
    words.extend_from_slice(&v_mov_lit(5, 0x9ABC_DEF0));
    // buffer_store_dwordx2 v[4:5], v1, s[4:7], 0 offen (op 0x1D, 8 bytes)
    words.extend_from_slice(&mubuf_offen(0x1D, 4, 1, 4, 0, false));
    words.push(S_ENDPGM);

    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
        layout: vec![
            RootParam {
                root: RootType::Descriptor,
                table_offset_dwords: 0,
                sgprs: 2,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 2,
                base_register: 0,
                user_data: 2,
                slot: 1,
            },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Descriptor,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 1,
            dwords: 2,
            user_data: 0,
            slot: 0,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 4 << 1,
            threads: [1, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };

    let shader = compile_cs(&native);
    assert!(shader.dirty_tracked);
    assert_eq!(shader.root_uavs, vec![0, SLOT_DIRTY]);

    // Root UAV starts at 0x1165_FFF8 (bias = 8).
    // - At offset 0: 8-byte store covers 0x1165_FFF8..=0x1165_FFFF -> ONLY page 0x1165!
    //   (If `USER_VIEW_BIAS = 8` had been included in the dirty address, it would have
    //   wrongly computed 0x1166_0000 and marked page 0x1166!)
    // - At offset 4: 8-byte store covers 0x1165_FFFC..=0x1166_0003 -> BOTH 0x1165 and 0x1166!
    let root_va = 0x1165_FFF8u64;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else { continue };
        let (root, pso) = host.compute_pipeline(&shader).expect("compute_pipeline");

        let ud_in_page = fill_user_data(&shader, &[0, 0], |_| 0, |_, _| 0, |_| root_va, 0);
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![
                        (0, Root::Constants(ud_in_page)),
                        (1, Root::Uav(vec![0u32; 16])),
                    ],
                    views: &[],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap();
        // Host raw UAV begins 8 bytes before `root_va`, so offset 0 lands at dword 2..4!
        assert_eq!(&out[0][2..4], &[0x1234_5678, 0x9ABC_DEF0]);
        assert_eq!(host.dirty_pages(), vec![0x1165], "{label}: 0xFFF8..0xFFFF stays in 0x1165");

        let ud_cross = fill_user_data(&shader, &[4, 0], |_| 0, |_, _| 0, |_| root_va, 0);
        let out_cross = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![
                        (0, Root::Constants(ud_cross)),
                        (1, Root::Uav(vec![0u32; 16])),
                    ],
                    views: &[],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap();
        assert_eq!(&out_cross[0][3..5], &[0x1234_5678, 0x9ABC_DEF0]);
        assert_eq!(
            host.dirty_pages(),
            vec![0x1165, 0x1166],
            "{label}: 0xFFFC..0x10003 crosses into 0x1166"
        );
    }
}

/// `global_store_dwordx4` and `global_atomic_add` through the `SLOT_MEMORY`
/// directory: mapped writable pages mark `SLOT_DIRTY`, while unmapped (`0`)
/// and `MEMORY_READ_ONLY` pages are dropped without setting `SLOT_DIRTY`.
#[test]
fn global_stores_and_atomics_mark_writable_pages_only() {
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    // v[2:3] = s[0:1] (64-bit address 1), v[4:5] = s[2:3] (64-bit address 2)
    let mut words = vec![
        v_mov_sgpr(2, 0),
        v_mov_sgpr(3, 1),
        v_mov_sgpr(4, 2),
        v_mov_sgpr(5, 3),
    ];
    for (k, val) in [0x11u32, 0x22, 0x33, 0x44].into_iter().enumerate() {
        words.extend_from_slice(&v_mov_lit(6 + k as u8, val));
    }
    words.extend_from_slice(&v_mov_lit(10, 5));
    // global_store_dwordx4 v[2:3], v[6:9], off (op 0x1E)
    words.extend_from_slice(&global_op(0x1E, 0, 6, 2, 0, false));
    // global_atomic_add v[4:5], v10, off (op 0x32)
    words.extend_from_slice(&global_op(0x32, 0, 10, 4, 0, false));
    words.push(S_ENDPGM);

    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 12,
        layout: vec![RootParam {
            root: RootType::Constants,
            table_offset_dwords: 0,
            sgprs: 4,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: Vec::new(),
        io: StageIo::Cs(ComputeIo {
            rsrc2: 4 << 1,
            threads: [1, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };

    let shader = compile_cs(&native);
    assert!(shader.dirty_tracked);
    assert!(shader.root_srvs.contains(&SLOT_MEMORY));
    assert!(shader.root_uavs.contains(&SLOT_FAULTS));
    assert!(shader.root_uavs.contains(&SLOT_DIRTY));

    // Directory covers pages 0..=8 (16 dwords):
    // - Page 3: copy 1, start 0 (writable, 0x0000..0x1_0000)
    // - Page 4: copy 1, start 0x1_0000 (writable, 0x1_0000..0x2_0000)
    // - Page 5: copy 1 | MEMORY_READ_ONLY, start 0x2_0000 (read-only!)
    // - Page 6: 0 (unmapped!)
    let mut dir = vec![0u32; 16];
    dir[3 * 2] = 1;
    dir[3 * 2 + 1] = 0;
    dir[4 * 2] = 1;
    dir[4 * 2 + 1] = 0x1_0000;
    dir[5 * 2] = 1 | MEMORY_READ_ONLY;
    dir[5 * 2 + 1] = 0x2_0000;

    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else { continue };
        let (root, pso) = host.compute_pipeline(&shader).expect("compute_pipeline");

        // Address 1 = 0x3_FFFC (straddles writable pages 3 and 4);
        // Address 2 = 0x5_0000 (read-only page 5 -> dropped to null slot 0, NOT marked dirty!).
        let ud = fill_user_data(
            &shader,
            &[0x0003_FFFC, 0, 0x0005_0000, 0],
            |_| 0,
            |_, _| 0,
            |_| 0,
            0,
        );
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![
                        (0, Root::Constants(ud)),
                        (1, Root::View { srv: true, data: dir.clone() }),
                    ],
                    // View 0 = null/sink slot, View 1 = backing copy 1 (3 * 64 KiB = 49,152 dwords).
                    views: &[View::RawUav(64), View::RawUav(3 * 16_384)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap();
        assert_eq!(
            host.dirty_pages(),
            vec![3, 4],
            "{label}: writable pages 3 & 4 marked; read-only page 5 excluded"
        );
        assert_eq!(
            &out[1][0xFFFC / 4..0xFFFC / 4 + 4],
            &[0x11, 0x22, 0x33, 0x44],
            "{label}: cross-page global_store"
        );
        assert_eq!(out[1][0x2_0000 / 4], 0, "{label}: read-only page 5 in copy 1 untouched");
        // Unmapped page 6 -> also excluded from SLOT_DIRTY.
        let ud_missing = fill_user_data(
            &shader,
            &[0x0006_0010, 0, 0x0006_0020, 0],
            |_| 0,
            |_, _| 0,
            |_| 0,
            0,
        );
        let _ = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![
                        (0, Root::Constants(ud_missing)),
                        (1, Root::View { srv: true, data: dir.clone() }),
                    ],
                    views: &[View::RawUav(64), View::RawUav(3 * 16_384)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap();
        assert!(host.dirty_pages().is_empty(), "{label}: unmapped page must not set SLOT_DIRTY");
    }
}

/// Root signature 64-dword budget exhaustion, dynamic `ResourceHeap` buffer
/// stores, and `VICTORY_DIRTY_BITMAP=0` all fall back cleanly with
/// `dirty_tracked = false` and no `SLOT_DIRTY` binding.
#[test]
fn root_budget_overflow_and_dynamic_uav_degrade_cleanly_to_untracked() {
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());

    // 1. Compute shader reading s59 (`user_data_dwords = 60`) + 1 table SGPR (`s60`, making
    // `user_data_dwords = 64`!): adding `SLOT_DIRTY` (+2 root dwords) + `USER_VIEW_ADDRESS`
    // would exceed D3D12's 64-dword root signature limit, so lowering retries without dirty
    // tracking (`dirty_tracked = false`) and still creates a valid D3D12 PSO!
    let mut words = vec![
        s_mov_b32(61, 0x80),
        v_mov_sgpr(1, 58), // touches constant dword 58 -> index 58 -> 60 dwords rounded to 60/64
    ];
    words.extend_from_slice(&s_load_dwordx4_off(64, 60, 0));
    words.extend_from_slice(&mubuf_offen(0x1C, 1, 0, 64, 0, false));
    words.push(S_ENDPGM);

    let tight = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 4,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 60,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 60,
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
            user_data: 60,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 61 << 1,
            threads: [1, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let tight_shader = compile_cs(&tight);
    assert!(!tight_shader.dirty_tracked, "64-dword root budget overflow must set dirty_tracked=false");
    assert!(!tight_shader.root_uavs.contains(&SLOT_DIRTY));
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else { continue };
        host.compute_pipeline(&tight_shader)
            .unwrap_or_else(|e| panic!("{label}: tight 64-dword PSO failed: {e}"));
    }

    // 2. Dynamic `ResourceHeap` buffer store (`s_load_dwordx4` at computed SGPR offset `s4`).
    let dyn_words = [
        s_mov_b32(2, 1), // s2 = heap base (s1)
        s_mov_b32(3, 0x80),
        0x3404_0085, // v_lshlrev_b32 v2, 5, v0
        0x7E08_0502, // v_readfirstlane_b32 s4, v2
        0xF408_0201, // s_load_dwordx4 s[8:11], s[2:3], s4
        0x0800_0000,
        0x7E02_0287, // v_mov_b32 v1, 7
        0xE070_0000, // buffer_store_dword v1, off, s[8:11], 0
        0x8002_0100,
        S_ENDPGM,
    ];
    let dyn_prog = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 4,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::ResourceHeap,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 1,
                slot: 1,
            },
        ],
        used: Vec::new(),
        io: StageIo::Cs(ComputeIo {
            rsrc2: 2 << 1,
            threads: [1, 1, 1],
        }),
        code: dyn_words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let dyn_shader = compile_cs(&dyn_prog);
    assert!(!dyn_shader.dirty_tracked, "bindless heap UAV store must fall back to dirty_tracked=false");
    assert!(!dyn_shader.root_uavs.contains(&SLOT_DIRTY));

    // 3. `VICTORY_DIRTY_BITMAP=0` (`with_dirty_bitmap(false, ...)`).
    let simple_words = [
        s_mov_b32(1, 0x80),
        0xF408_0100,
        0xFA00_0000,
        0x7E02_0281,
        0xE070_0000,
        0x8001_0100,
        S_ENDPGM,
    ];
    let simple_prog = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 4,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 0,
            slot: 0,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 1 << 1,
            threads: [1, 1, 1],
        }),
        code: simple_words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let disabled = with_dirty_bitmap(false, || compile_cs(&simple_prog));
    assert!(!disabled.dirty_tracked);
    assert!(!disabled.root_uavs.contains(&SLOT_DIRTY));
}
