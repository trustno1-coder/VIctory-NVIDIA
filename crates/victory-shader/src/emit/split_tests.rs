//! NVIctory's split-wave fallback (`split`): the LDS accesses of a console wave64 program of one
//! wave, read and written in one lane-branch region, on a host of 32-lane waves. Ported from
//! NVIctory's `split_tests.rs` (61db46f) for the proofs `split::admit` runs; here a refusal is
//! no error but a program still needing 64-lane waves, with `split::REFUSED_AT` set.

use super::{lower, split};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;
const VCC: u8 = 106;

/// SOP1 `op sdst, src0`.
const fn sop1(op: u8, sdst: u8, src0: u8) -> u32 {
    0xBE80_0000 | (sdst as u32) << 16 | (op as u32) << 8 | src0 as u32
}

/// SMEM `s_load_dwordx4 s[dst:dst+3], s[base:base+1], 0`.
const fn s_load_dwordx4(dst: u8, base: u8) -> [u32; 2] {
    [0x3D << 26 | 0x02 << 18 | (dst as u32) << 6 | (base as u32 >> 1), 125 << 25]
}

/// VOP1 `op vdst, src0`.
const fn vop1(op: u16, vdst: u8, src0: u16) -> u32 {
    0x7E00_0000 | (vdst as u32) << 17 | (op as u32) << 9 | (src0 as u32 & 0x1FF)
}

/// VOP2 `op vdst, src0, vsrc1`.
const fn vop2(op: u8, vdst: u8, src0: u16, vsrc1: u8) -> u32 {
    (op as u32) << 25 | (vdst as u32) << 17 | (vsrc1 as u32) << 9 | (src0 as u32 & 0x1FF)
}

/// VOPC `op vcc, src0, vsrc1`.
const fn vopc(op: u8, src0: u16, vsrc1: u8) -> u32 {
    0x7C00_0000 | (op as u32) << 17 | (vsrc1 as u32) << 9 | (src0 as u32 & 0x1FF)
}

/// MUBUF `buffer_store_dword vdata, vaddr, s[srsrc:srsrc+3], 0 offen offset:byte_off`.
const fn buffer_store_dword_offen(vdata: u8, vaddr: u8, srsrc: u8, byte_off: u16) -> [u32; 2] {
    [
        0x38 << 26 | 0x1C << 18 | 1 << 12 | (byte_off as u32 & 0xFFF),
        0x80 << 24 | ((srsrc as u32 >> 2) & 0x1F) << 16 | (vdata as u32) << 8 | vaddr as u32,
    ]
}

const fn int(n: u8) -> u16 {
    128 + n as u16
}

/// A wave64 program of 64 threads (root constants in s0-s3, the UAV table in s4) with
/// `lds_blocks` blocks of LDS.
fn native(words: &[u32], lds_blocks: u32) -> NativeProgram {
    NativeProgram {
        stage: HwStage::Cs,
        wave64: true,
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
            rsrc2: 5 << 1 | lds_blocks << 15,
            threads: [64, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// How `words` lowers for a host of 32-lane waves, with the fallback on or not: the wave size
/// it needs, and the access the fallback refused.
fn lowered(words: &[u32], fallback: bool) -> (Option<u32>, Option<usize>) {
    let native = native(words, 1);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, &native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    split::REFUSED_AT.set(None);
    let lower = || {
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
    };
    let l = if fallback { split::with_fallback(lower) } else { lower() };
    lower::TEST_HOST_WAVE.set(was);
    let l = l.expect("lower");
    (l.wave_size, split::REFUSED_AT.take())
}

/// Translated in waves of 32 by the fallback only.
fn admitted(words: &[u32]) -> bool {
    lowered(words, true) == (Some(32), None)
}

/// Refused by the fallback: still 64-lane waves, with the refused access named.
fn refused(words: &[u32]) -> bool {
    matches!(lowered(words, true), (Some(64), Some(_)))
}

#[test]
fn the_fallback_refuses_an_lds_read_after_a_write_in_a_divergent_region() {
    let mut words = vec![
        vop2(0x1A, 1, int(2), 0),
        vopc(0xC4, int(40), 0),
        sop1(0x24, 20, VCC),
        0xBF88_0000 | 4,          // s_cbranch_execz over the write and the read
        0xD800_0000 | 0x0D << 18, // ds_write_b32 v1, v0
        1,
        0xD800_0000 | 0x36 << 18 | 4, // ds_read_b32 v3, v1 offset:4
        3 << 24 | 1,
        S_ENDPGM,
    ];
    // (the forms before the fallback keep 64-lane waves, and so it does)
    assert_eq!(lowered(&words, false).0, Some(64));
    assert!(refused(&words));
    // The refusal names the region: its root's branch first.
    assert_eq!(split::REFUSED.take().first(), Some(&12));
    words[3] = 0xBF80_0000; // s_nop 0: EXEC alone masks the accesses.
    assert!(admitted(&words), "guarded accesses in a block the group runs together meet");
}

#[test]
fn an_execz_skip_over_an_access_falls_through_only_when_it_skips_forward() {
    let words = vec![
        vop2(0x1A, 1, int(2), 0),
        vopc(0xC4, int(40), 0),
        sop1(0x24, 20, VCC),
        0xBF88_0004,              // s_cbranch_execz over both LDS writes
        0xD800_0000 | 0x0D << 18, // ds_write_b32 v1, v0
        1,
        0xD800_0000 | 0x0D << 18 | 4, // ds_write_b32 v1, v0 offset:4
        1,
        S_ENDPGM,
    ];
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let p = ir::build(&code, true, 256).unwrap();
    assert_eq!(split::execz_skip_over(&p, 24), Some(12));
    assert_eq!(split::execz_skip_over(&p, 32), None, "the skip's target is past it");
    assert!(split::skips_forward(&p, 12));
}

#[test]
fn the_fallback_allows_disjoint_fixed_lds_dwords_in_one_arm() {
    let mut words = vec![
        vopc(0xD2, int(0), 0), // Only thread x == 0.
        0xBF88_0005,
        vop1(0x01, 2, int(0)),
        0xD800_0000 | 0x36 << 18 | 4, // Read dword 1.
        3 << 24 | 2,
        0xD800_0000 | 0x0D << 18, // Write dword 0.
        3 << 8 | 2,
        S_ENDPGM,
    ];
    assert!(admitted(&words));
    words[0] = vopc(0xD4, int(40), 0);
    assert!(refused(&words), "fixed writes by lanes in both halves are not proved independent");
    words[0] = vopc(0xD2, int(0), 0);
    words[3] &= !4;
    assert!(admitted(&words), "thread zero alone cannot race itself");
    words[3] |= 4;
    words[1] = 0xBF88_0007;
    words.splice(7..7, [0xD800_0000 | 0x36 << 18 | 8, 4 << 24 | 2]);
    assert!(admitted(&words), "thread zero alone cannot race itself");
    words.drain(7..9);
    words[1] = 0xBF88_0005;
    words[5] = 0xD800_0000 | 0x20 << 18; // Atomics are not plain stores.
    assert!(admitted(&words), "thread zero alone cannot race itself");
}

#[test]
fn the_fallback_recognizes_a_saved_singleton_lds_predicate() {
    let mut words = vec![
        0xD4C2_0000,
        0x0002_0080, // v_cmp_eq_u32 s[0:1], 0, v0
        0xBF82_0001, // Skip a block before using the saved predicate.
        vop1(0x01, 3, int(0)),
        0xBE84_2400, // s_and_saveexec_b64 s[4:5], s[0:1]
        0xBF88_0005,
        vop1(0x01, 2, int(0)),
        0xD800_0000 | 0x36 << 18 | 4,
        3 << 24 | 2,
        0xD800_0000 | 0x0D << 18,
        3 << 8 | 2,
        S_ENDPGM,
    ];
    assert!(admitted(&words));
    words[1] = 0x0002_0081; // Compare thread x with 1 instead of zero.
    assert!(refused(&words));
}

#[test]
fn the_fallback_orders_lds_accesses_for_a_derived_singleton() {
    let mut words = vec![sop1(0x03, 5, 0x80)];
    words.extend(s_load_dwordx4(8, 4));
    words.extend([
        sop1(0x04, 20, 126), // Save EXEC.
        vop1(0x01, 2, int(0)),
        vopc(0xD2, int(0), 0), // Only thread zero sets the flag.
    ]);
    let first_branch = words.len();
    words.push(0);
    words.extend([
        vop1(0x01, 7, int(0)),
        vop1(0x01, 4, int(5)),
        0xD800_0000 | 0x0D << 18,
        4 << 8 | 7,
        vop1(0x01, 2, 193),
    ]);
    words[first_branch] = 0xBF88_0000 | (words.len() - first_branch - 1) as u32;
    words.extend([sop1(0x04, 126, 20), vopc(0xD2, 193, 2)]);
    let second_branch = words.len();
    words.push(0);
    words.extend([
        vop1(0x01, 7, int(0)),
        0xD800_0000 | 0x36 << 18,
        3 << 24 | 7,
        vop1(0x01, 4, int(9)),
        0xD800_0000 | 0x0D << 18,
        4 << 8 | 7,
        0xD800_0000 | 0x0D << 18 | 4,
        4 << 8 | 7,
    ]);
    words[second_branch] = 0xBF88_0000 | (words.len() - second_branch - 1) as u32;
    words.extend([
        sop1(0x04, 126, 20),
        vop1(0x01, 7, int(0)),
        0xD800_0000 | 0x36 << 18,
        3 << 24 | 7,
        vop2(0x1A, 1, int(2), 0),
    ]);
    words.extend(buffer_store_dword_offen(3, 1, 8, 0));
    words.push(S_ENDPGM);
    assert!(admitted(&words));
    words[4] = vop1(0x01, 2, 193); // The flag no longer excludes other threads.
    assert!(refused(&words));
    words[4] = vop1(0x01, 2, int(0));
    words[5] = vopc(0xD4, int(40), 0); // Multiple threads set the flag.
    assert!(refused(&words));
}
