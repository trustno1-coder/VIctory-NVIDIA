//! LDS read and written in lane-branch regions of a console wave64 compute program of several
//! console waves, on a host of 32-lane waves ([`super::lds_exact`]): hand-written fixtures of a
//! group of 128 threads in one dimension (two console waves) with the constructs that kept such
//! programs at 64 lanes. Thread 0's read-modify-writes of an LDS dword; the same by threads 0
//! and 1 (no one thread: refused by the module); the odd threads' allocation of slots through
//! `ds_add_rtn_u32` on an LDS counter (the wave's first lane reserves the count, the lanes take
//! base plus rank), alone, with all of D3D12's 32 KiB of LDS, and in a loop the lanes leave one
//! by one. Translated by `emit` (refused with the switch off: 64-lane waves only), validated by
//! `dxil.dll` when present, and run on the host GPU when it takes waves of 32 (Windows).

use super::{HostShader, lds_exact, lower};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{dxbc, io, ir, resource, structure};

const fn vop1(op: u32, vdst: u32, src0: u32) -> u32 {
    0x7E00_0000 | vdst << 17 | op << 9 | src0
}

const fn vop2(op: u32, vdst: u32, src0: u32, vsrc1: u32) -> u32 {
    op << 25 | vdst << 17 | vsrc1 << 9 | src0
}

const fn vopc(op: u32, src0: u32, vsrc1: u32) -> u32 {
    0x7C00_0000 | op << 17 | vsrc1 << 9 | src0
}

const fn sop1(op: u32, sdst: u32, ssrc0: u32) -> u32 {
    0xBE80_0000 | sdst << 16 | op << 8 | ssrc0
}

const fn sop2(op: u32, sdst: u32, ssrc0: u32, ssrc1: u32) -> u32 {
    0x8000_0000 | op << 23 | sdst << 16 | ssrc1 << 8 | ssrc0
}

/// VOP3 `v_mbcnt_lo_u32_b32` (`hi: false`) or `_hi_` of the two sources (operand codes).
const fn mbcnt(hi: bool, vdst: u32, src0: u32, src1: u32) -> [u32; 2] {
    [(0xD765_0000 + ((hi as u32) << 16)) | vdst, src0 | src1 << 9]
}

/// DS `op` with its data and address VGPRs (`vdst` for a read), the two byte offsets.
const fn ds(
    op: u32,
    vdst: u32,
    addr: u32,
    data0: u32,
    data1: u32,
    offset0: u32,
    offset1: u32,
) -> [u32; 2] {
    [
        0x36 << 26 | op << 18 | offset1 << 8 | offset0,
        vdst << 24 | data1 << 16 | data0 << 8 | addr,
    ]
}

/// `buffer_store_dword v<data>, v<addr>, s[8:11], 0 offen offset:<offset>`.
const fn store(data: u32, addr: u32, offset: u32) -> [u32; 2] {
    [
        0x38 << 26 | 0x1C << 18 | 1 << 12 | offset,
        0x80 << 24 | 2 << 16 | data << 8 | addr,
    ]
}

const V: u32 = 256;
const fn k(n: u32) -> u32 {
    128 + n
}
const K_M1: u32 = 193;
const VCC: u32 = 106;
const EXEC: u32 = 126;
const S_ENDPGM: u32 = 0xBF81_0000;
const S_BARRIER: u32 = 0xBF8A_0000;
const S_WAITCNT_LGKM0: u32 = 0xBF8C_C07F;
const S_AND_SAVEEXEC_VCC: u32 = 0xBE8C_246A; // s_and_saveexec_b64 s[12:13], vcc
const S_RESTORE_EXEC: u32 = 0xBEFE_040C; // s_mov_b64 exec, s[12:13]
const DS_WRITE_B32: u32 = 0x0D;
const DS_ADD_RTN_U32: u32 = 0x20;
const DS_READ_B32: u32 = 0x36;

/// The dwords of the UAV a run reads back.
#[cfg(windows)]
const UAV_DWORDS: u32 = 320;

/// D3D12's group-shared memory, in the 512-byte units of the console's LDS size.
const FULL_LDS: u32 = 64;

/// Two console waves (128 threads in one dimension; the thread index in v0) with 4 root
/// constants in s0..s3, a raw UAV's descriptor in s[8:11] and `lds` 512-byte units of LDS:
/// `head`, `cmp` (a VOPC into vcc against v0), `s_and_saveexec_b64 s[12:13], vcc`, `region` up
/// to the restore of EXEC (skipped when no lane is left), then `tail`.
///
/// The region also sets s30 (0 before it), which every thread reads after it into a value it
/// stores, masked to 0, at `out[300]` (past what the tests read as data): the code after the
/// region can see whether the console wave skipped it, so no form keeps the region's blocks as
/// ones every lane runs (`ir::without_skips`), as in programs whose regions hold scalar work.
fn two_waves(head: &[u32], cmp: u32, region: &[u32], tail: &[u32], lds: u32) -> NativeProgram {
    let mut words = vec![
        0xBE85_0380, // s_mov_b32 s5, 0
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 2,
        125 << 25,   // s_load_dwordx4 s[8:11], s[4:5], 0
        0xBE9E_0380, // s_mov_b32 s30, 0
    ];
    words.extend(head);
    words.extend([
        cmp,
        S_AND_SAVEEXEC_VCC,
        0xBF88_0000 | (region.len() as u32 + 1),
        0xBE9E_0381, // s_mov_b32 s30, 1
    ]);
    words.extend(region);
    words.push(S_RESTORE_EXEC);
    words.extend(tail);
    words.extend([
        vop1(1, 30, 30),          // v_mov_b32 v30, s30
        vop2(0x1B, 30, k(0), 30), // v_and_b32 v30, 0, v30
        vop1(1, 31, 0xFF),        // v_mov_b32 v31, 1200
        1200,
    ]);
    words.extend(store(30, 31, 0));
    words.push(S_ENDPGM);
    NativeProgram {
        stage: HwStage::Cs,
        wave64: true,
        vgprs: 32,
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
            rsrc2: 5 << 1 | lds << 15, // 5 user SGPRs: s0..s4
            threads: [128, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// `native` translated by `emit` for a host of 32-lane waves, with `exact` for
/// `VICTORY_LDS_EXACT`, and the group-shared dwords of the program it made last.
fn translated(native: &NativeProgram, exact: bool) -> (HostShader, u32) {
    let shader = emitted(native, exact).expect("emits");
    (shader, super::TEST_LAST_LDS_DWORDS.get())
}

/// What `emit` makes of `native` for a host of 32-lane waves, with `exact` for
/// `VICTORY_LDS_EXACT`; a shader it gives is validated by `dxil.dll` when present (Windows).
fn emitted(native: &NativeProgram, exact: bool) -> Result<HostShader, crate::ShaderError> {
    let p = crate::spill::build(native).expect("build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, native).expect("io::raise");
    let bytes = dxbc::build(&[]);
    let c = dxbc::Container::parse(&bytes).expect("container");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let was_exact = lds_exact::TEST_EXACT.replace(Some(exact));
    let shader = super::emit(&c, native, &p, &st, &io, &res);
    lds_exact::TEST_EXACT.set(was_exact);
    lower::TEST_HOST_WAVE.set(was);
    #[cfg(windows)]
    if let Ok(shader) = &shader
        && let Ok(mut v) = super::validator::Validator::load(
            super::validator::Validator::default_path().as_deref(),
        )
    {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    shader
}

/// Whether `emit` refuses `native` with the switch off: a program of 64-lane waves, which a
/// host of 32-lane waves does not run.
fn refused_without_the_switch(native: &NativeProgram) -> bool {
    matches!(
        emitted(native, false),
        Err(crate::ShaderError::Unsupported { what, .. })
            if what == "no wave32 lowering for this wave64 program"
    )
}

/// The wave size a translated program asks of the host (PSV0), if it asks one.
fn wave_of(shader: &HostShader) -> Option<u32> {
    let c = dxbc::Container::parse(&shader.dxil).expect("the shader's container");
    let psv = c.part(dxbc::FourCC::PSV0).expect("PSV0").data;
    let min = u32::from_le_bytes(psv[20..24].try_into().expect("4 bytes"));
    (min != 0).then_some(min)
}

/// The threads under `count` (or thread 0 alone: `None`) write LDS dword 2, then read it, add
/// one and write it back three times; after a barrier every thread reads it and stores it with
/// its lane (`v_mbcnt`: what makes the program one of console lanes).
fn read_modify_write_program(count: Option<u32>) -> NativeProgram {
    let mut region = vec![vop1(1, 4, k(0)), vop1(1, 5, 0xFF), 41]; // v4 = 0, v5 = 41
    region.extend(ds(DS_WRITE_B32, 0, 4, 5, 0, 8, 0));
    for _ in 0..3 {
        region.push(S_WAITCNT_LGKM0);
        region.extend(ds(DS_READ_B32, 6, 4, 0, 0, 8, 0));
        region.extend([S_WAITCNT_LGKM0, vop2(0x25, 6, k(1), 6)]);
        region.extend(ds(DS_WRITE_B32, 0, 4, 6, 0, 8, 0));
    }
    let mut tail = vec![S_WAITCNT_LGKM0, S_BARRIER, vop1(1, 4, k(0))];
    tail.extend(ds(DS_READ_B32, 7, 4, 0, 0, 8, 0));
    tail.push(S_WAITCNT_LGKM0);
    tail.extend(mbcnt(false, 9, K_M1, k(0)));
    tail.extend(mbcnt(true, 10, K_M1, V + 9));
    tail.push(vop2(0x1A, 11, k(3), 0)); // v11 = thread * 8
    tail.extend(store(7, 11, 0));
    tail.extend(store(10, 11, 4));
    let cmp = match count {
        None => vopc(0xC2, k(0), 0),    // v_cmp_eq_u32 vcc, 0, v0
        Some(n) => vopc(0xC4, k(n), 0), // v_cmp_gt_u32 vcc, n, v0
    };
    two_waves(&[], cmp, &region, &tail, 2)
}

/// The head of the allocation programs: every thread sets its word of a bitset at bytes 16 and
/// up, the counter in LDS dword 1 starts at 0, the group meets.
fn allocation_head() -> Vec<u32> {
    let mut head = vec![
        vop2(0x1A, 2, k(2), 0), // v_lshlrev_b32 v2, 2, v0
        vop2(0x1B, 2, 0xFF, 2), // v_and_b32 v2, lit, v2
        0x1FC,
        vop1(1, 3, K_M1), // v_mov_b32 v3, -1
    ];
    head.extend(ds(DS_WRITE_B32, 0, 2, 3, 0, 16, 0));
    head.push(vop1(1, 4, k(0)));
    head.extend(ds(DS_WRITE_B32, 0, 4, 4, 0, 4, 0));
    head.extend([S_WAITCNT_LGKM0, S_BARRIER]);
    head
}

/// Each odd thread whose bit of a word of LDS is set takes a slot in a list by the allocation
/// idiom: its wave's first lane adds the wave's count to the counter in LDS dword 1
/// (`ds_add_rtn_u32`) and the lanes take the base it returned plus their rank among the odd
/// lanes. The region also reads the bitset word at an address of its own whose range does not
/// meet the counter's. Every odd thread stores its number at `out[slot]`. `lds`: 512-byte
/// units of LDS.
fn allocation_program(lds: u32) -> NativeProgram {
    let mut head = allocation_head();
    head.push(vop2(0x1B, 5, k(1), 0)); // v_and_b32 v5, 1, v0: odd threads
    head.extend(ds(DS_READ_B32, 6, 2, 0, 0, 16, 0)); // their bitset word (all lanes)
    head.extend([
        S_WAITCNT_LGKM0,
        vop2(0x1B, 7, V + 6, 5), // v_and_b32 v7, v6, v5
    ]);
    let cmp = vopc(0xC5, k(0), 7); // v_cmp_ne_u32 vcc, 0, v7
    let mut region = vec![
        sop1(0x14, 16, EXEC), // s_ff1_i32_b64 s16, exec
        sop1(4, 18, EXEC),    // s_mov_b64 s[18:19], exec
    ];
    region.extend(ds(DS_READ_B32, 8, 2, 0, 0, 16, 0)); // (in the region: the bitset word again)
    region.push(S_WAITCNT_LGKM0);
    region.extend(mbcnt(false, 9, K_M1, k(0)));
    region.push(sop2(0x0F, 20, VCC, EXEC)); // s_and_b64 s[20:21], vcc, exec
    region.extend(mbcnt(true, 10, K_M1, V + 9));
    region.extend(mbcnt(false, 11, 20, k(0)));
    region.extend(mbcnt(true, 12, 21, V + 11));
    region.extend([
        vop1(1, 13, k(0)),  // v_mov_b32 v13, 0
        vopc(0xD2, 16, 10), // v_cmpx_eq_u32 exec, s16, v10: the first lane
        sop1(0x10, 22, 20), // s_bcnt1_i32_b64 s22, s[20:21]
        vop1(1, 13, k(0)),  // v_mov_b32 v13, 0
        vop1(1, 14, 22),    // v_mov_b32 v14, s22
    ]);
    region.extend(ds(DS_ADD_RTN_U32, 13, 13, 14, 0, 4, 0)); // ds_add_rtn_u32 v13, v13, v14 offset:4
    region.extend([
        sop1(4, EXEC, 18), // s_mov_b64 exec, s[18:19]
        S_WAITCNT_LGKM0,
        vop1(2, 23, V + 13),      // v_readfirstlane_b32 s23, v13
        vop2(0x25, 15, 23, 12),   // v_add_nc_u32 v15, s23, v12: the slot
        vop2(0x1A, 16, k(2), 15), // v_lshlrev_b32 v16, 2, v15
    ]);
    region.extend(store(0, 16, 0)); // out[slot] = the thread's number
    two_waves(&head, cmp, &region, &[], lds)
}

/// The allocation of [`allocation_program`] in a loop the lanes leave one by one (the lanes of
/// the loop's mask walk their items; the wave goes on while some lane has one): the odd lanes
/// allocate once for each of `(thread & 3) + 1` rounds and store `thread * 8 + round`. The loop
/// keeps the region from being cut into blocks every lane runs.
fn allocation_loop_program() -> NativeProgram {
    let mut head = allocation_head();
    head.push(vop2(0x1B, 5, k(1), 0)); // v5 = odd
    let cmp = vopc(0xC5, k(0), 5); // v_cmp_ne_u32 vcc, 0, v5
    let mut region = vec![
        sop1(4, 14, EXEC),        // s_mov_b64 s[14:15], exec: the lanes in the loop
        vop2(0x1B, 20, k(3), 0),  // v_and_b32 v20, 3, v0
        vop2(0x25, 20, k(1), 20), // v_add_nc_u32 v20, 1, v20: its rounds
        vop1(1, 21, k(0)),        // v_mov_b32 v21, 0: its round
        0xBF82_0001,              // s_branch past the top of the loop
    ];
    // (the branch at the bottom comes back here)
    let top_at = region.len();
    region.push(sop2(0x0F, EXEC, 26, 14)); // s_and_b64 exec, s[26:27], s[14:15]
    region.extend([
        sop1(0x14, 16, EXEC), // s_ff1_i32_b64 s16, exec
        sop1(4, 18, EXEC),    // s_mov_b64 s[18:19], exec
    ]);
    region.extend(ds(DS_READ_B32, 8, 2, 0, 0, 16, 0));
    region.push(S_WAITCNT_LGKM0);
    region.extend(mbcnt(false, 9, K_M1, k(0)));
    region.push(sop1(4, 22, EXEC)); // s_mov_b64 s[22:23], exec: the lanes that allocate
    region.extend(mbcnt(true, 10, K_M1, V + 9));
    region.extend(mbcnt(false, 11, 22, k(0)));
    region.extend(mbcnt(true, 12, 23, V + 11));
    region.extend([
        vop1(1, 13, k(0)),
        vopc(0xD2, 16, 10), // v_cmpx_eq_u32 exec, s16, v10: the first lane
        sop1(0x10, 24, 22), // s_bcnt1_i32_b64 s24, s[22:23]
        vop1(1, 13, k(0)),
        vop1(1, 14, 24),
    ]);
    region.extend(ds(DS_ADD_RTN_U32, 13, 13, 14, 0, 4, 0));
    region.extend([
        sop1(4, EXEC, 18),
        S_WAITCNT_LGKM0,
        vop1(2, 25, V + 13),        // v_readfirstlane_b32 s25, v13
        vop2(0x25, 15, 25, 12),     // v_add_nc_u32 v15, s25, v12: the slot
        vop2(0x1A, 16, k(2), 15),   // v_lshlrev_b32 v16, 2, v15
        vop2(0x1A, 17, k(3), 0),    // v_lshlrev_b32 v17, 3, v0
        vop2(0x25, 17, V + 17, 21), // v_add_nc_u32 v17, v17, v21
    ]);
    region.extend(store(17, 16, 0));
    region.extend([
        vop2(0x25, 21, k(1), 21), // v_add_nc_u32 v21, 1, v21
        sop1(4, 26, EXEC),        // s_mov_b64 s[26:27], exec
        vopc(0xD6, V + 21, 20),   // v_cmpx_ge_u32 exec, v21, v20: the lanes that are through
        sop2(0x15, 14, 14, EXEC), // s_andn2_b64 s[14:15], s[14:15], exec
    ]);
    // s_cbranch_scc1 to the top: its offset counts from the dword after it
    let offset = (top_at as i32 - (region.len() as i32 + 1)) as i16 as u16 as u32;
    region.push(0xBF85_0000 | offset);
    two_waves(&head, cmp, &region, &[], 2)
}

/// The IR blocks of `p` that hold an instruction called `name`.
fn blocks_with(p: &ir::Program, name: &str) -> Vec<usize> {
    (0..p.blocks.len())
        .filter(|&b| p.blocks[b].insts.iter().any(|i| i.name() == name))
        .collect()
}

#[test]
fn the_regions_that_need_no_order_between_the_halves_are_told_from_those_that_do() {
    use lds_exact::{single_thread_blocks, with_exact};
    let was = lds_exact::TEST_EXACT.replace(Some(true));
    let exact_region = |p: &ir::Program, region: &[usize], single: &[bool], rtn: bool| {
        with_exact(|| lds_exact::exact_region(p, region, single, rtn))
    };
    // thread 0 alone: its region is one thread's
    let native = read_modify_write_program(None);
    let p = crate::spill::build(&native).unwrap();
    let st = structure::structure(&p).unwrap();
    let single = single_thread_blocks(&p, &st, [128, 1, 1]);
    let region = blocks_with(&p, "ds_write_b32");
    assert!(
        !region.is_empty() && region.iter().all(|&b| single[b]),
        "{region:?} {single:?}"
    );
    assert_eq!(exact_region(&p, &region, &single, false), Ok(()));
    // (not in a group of two dimensions, where `v0 == 0` is a column)
    assert!(
        single_thread_blocks(&p, &st, [8, 16, 1])
            .iter()
            .all(|&s| !s)
    );
    // the threads below 2: no one thread, and one dword read and written
    let native = read_modify_write_program(Some(2));
    let p = crate::spill::build(&native).unwrap();
    let st = structure::structure(&p).unwrap();
    let single = single_thread_blocks(&p, &st, [128, 1, 1]);
    let region = blocks_with(&p, "ds_write_b32");
    assert!(region.iter().all(|&b| !single[b]));
    assert_eq!(
        exact_region(&p, &region, &single, true),
        Err("an LDS range read and written in one region")
    );
    // the allocation: a read from 16 up and an atomic at 4 never meet; the atomic that returns
    // the old value only in the form that takes the idiom
    let native = allocation_program(2);
    let p = crate::spill::build(&native).unwrap();
    let st = structure::structure(&p).unwrap();
    let single = single_thread_blocks(&p, &st, [128, 1, 1]);
    let region = blocks_with(&p, "ds_add_rtn_u32");
    assert_eq!(region.len(), 1);
    assert_eq!(exact_region(&p, &region, &single, true), Ok(()));
    assert_eq!(
        exact_region(&p, &region, &single, false),
        Err("an atomic that returns the old value")
    );
    // (and nothing outside the last forms)
    assert_eq!(
        lds_exact::exact_region(&p, &region, &single, true),
        Err("exact LDS regions are off")
    );
    lds_exact::TEST_EXACT.set(was);
}

#[test]
fn the_allocation_idiom_knows_a_reservation_by_an_lds_atomic_only_in_the_last_forms() {
    let native = allocation_program(2);
    let p = crate::spill::build(&native).unwrap();
    let st = structure::structure(&p).unwrap();
    let was = lds_exact::TEST_EXACT.replace(Some(true));
    assert!(lower::wave_local(&p, &st).is_err());
    assert_eq!(lds_exact::with_exact(|| lower::wave_local(&p, &st)), Ok(()));
    lds_exact::TEST_EXACT.set(was);
}

#[test]
fn a_region_one_thread_runs_may_read_and_write_its_own_lds() {
    let native = read_modify_write_program(None);
    assert!(refused_without_the_switch(&native), "off");
    let (shader, _) = translated(&native, true);
    assert_eq!(wave_of(&shader), Some(32));
    #[cfg(windows)]
    {
        let _one = super::valu_tests::ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(out) = super::wave64_tests::whole_uav(&shader, [0, 0], UAV_DWORDS) else {
            return;
        };
        for t in 0..128usize {
            assert_eq!(out[2 * t], 44, "thread {t}");
            assert_eq!(out[2 * t + 1], (t % 64) as u32, "thread {t}'s lane");
        }
        eprintln!("ran on the hardware device: 128 threads read the 44 thread 0 made of 41");
    }
}

/// Runs an allocation program 8 times on the hardware device (Windows): the numbers stored in
/// the first `want.len()` dwords are `want`, each once, in any order (the order of the waves'
/// atomics), and nothing past them.
#[cfg_attr(not(windows), allow(unused_variables))]
fn allocates_each_once(shader: &HostShader, want: &[u32]) {
    #[cfg(windows)]
    {
        let _one = super::valu_tests::ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for _ in 0..8 {
            let Some(out) = super::wave64_tests::whole_uav(shader, [0, 0], UAV_DWORDS) else {
                return;
            };
            let mut got = out[..want.len()].to_vec();
            got.sort_unstable();
            assert_eq!(got, want, "every number once in the first slots");
            assert!(
                out[want.len()..].iter().all(|&x| x == 0),
                "nothing past the slots"
            );
        }
        eprintln!(
            "ran on the hardware device: {} slots, each once",
            want.len()
        );
    }
}

#[test]
fn a_wave_allocation_through_an_lds_counter_gives_every_thread_a_slot_of_its_own() {
    let native = allocation_program(2);
    assert!(refused_without_the_switch(&native), "off");
    let (shader, _) = translated(&native, true);
    assert_eq!(wave_of(&shader), Some(32));
    let want: Vec<u32> = (0..128).filter(|t| t % 2 == 1).collect();
    allocates_each_once(&shader, &want);
}

#[test]
fn a_program_with_all_32_kib_of_lds_takes_no_dword_past_them() {
    let native = allocation_program(FULL_LDS);
    assert!(refused_without_the_switch(&native), "off");
    let (shader, lds_dwords) = translated(&native, true);
    assert_eq!(wave_of(&shader), Some(32));
    // (no counter for `wave_base`, no exchange dwords for local waves)
    assert_eq!(lds_dwords, 8192);
    let want: Vec<u32> = (0..128).filter(|t| t % 2 == 1).collect();
    allocates_each_once(&shader, &want);
}

#[test]
fn a_wave_allocation_in_a_loop_lanes_leave_one_by_one_gives_every_round_a_slot_of_its_own() {
    let native = allocation_loop_program();
    assert!(refused_without_the_switch(&native), "off");
    let (shader, _) = translated(&native, true);
    assert_eq!(wave_of(&shader), Some(32));
    let mut want: Vec<u32> = (0..128u32)
        .filter(|t| t % 2 == 1)
        .flat_map(|t| (0..(t & 3) + 1).map(move |round| t * 8 + round))
        .collect();
    want.sort_unstable();
    allocates_each_once(&shader, &want);
}
