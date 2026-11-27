//! `VICTORY_WAVE_WIDE_APPROX`: a console wave64 compute program on a host of 32-lane
//! waves whose cross-lane code sits inside a per-lane branch takes the exact wave-wide form
//! before the approximate local-wave ones, unless that form puts a group barrier into a loop.
//! The programs are built here (no title code); the dispatch, where there is one, runs on WARP.

use super::host::{Dispatch, Host, Root, View};
use super::{lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

/// 64 threads in one console wave (thread index t in v0), root constants in s0..s3 and a raw
/// UAV's descriptor loaded into s[8:11]:
/// `s20 = 0; if (t < s0) { n = WaveActiveCountBits(t == s2); s20 = 1; uav[t] = n }
/// uav[64 + t] = s20`. The scalar written inside keeps the skip (left out, s20 would be 1
/// with no lane inside), so no exact form but the wave-wide one exchanges the count's
/// halves. `looped`: all of it inside a loop on the uniform s3 (`while (s3 == 0)`).
fn count_in_a_kept_branch(looped: bool) -> NativeProgram {
    let mut body = vec![
        0xBE94_0380, // s_mov_b32 s20, 0
        0x3404_0082, // v_lshlrev_b32 v2, 2, v0
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0006, // s_cbranch_execz to the restore of EXEC
        0x7D84_0002, // v_cmp_eq_u32 vcc, s2, v0
        0xBE90_106A, // s_bcnt1_i32_b64 s16, vcc
        0xBE94_0381, // s_mov_b32 s20, 1
        0x7E02_0210, // v_mov_b32 v1, s16
        0x38 << 26 | 0x1C << 18 | 1 << 12,
        0x80 << 24 | 2 << 16 | 1 << 8 | 2, // buffer_store_dword v1, v2, s[8:11], 0 offen
        0xBEFE_040C,                       // s_mov_b64 exec, s[12:13]
        0x7E06_0214,                       // v_mov_b32 v3, s20
        0x38 << 26 | 0x1C << 18 | 1 << 12 | 256,
        0x80 << 24 | 2 << 16 | 3 << 8 | 2, // buffer_store_dword v3, v2, s[8:11], 0 offen offset:256
    ];
    if looped {
        body.push(0xBF06_8003); // s_cmp_eq_u32 s3, 0
        let back = -(body.len() as i32 + 1);
        body.push(0xBF85_0000 | (back as u32 & 0xFFFF)); // s_cbranch_scc1 to the start
    }
    let mut words = vec![
        0xBE85_0380, // s_mov_b32 s5, 0
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 2,
        125 << 25, // s_load_dwordx4 s[8:11], s[4:5], 0
    ];
    words.extend_from_slice(&body);
    words.push(S_ENDPGM);
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
            rsrc2: 5 << 1, // 5 user SGPRs: s0..s4
            threads: [64, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// What [`super::emit`] gives `native` on a host of 32-lane waves with the switch `wide_approx`
/// (validated by `dxil.dll` where there is one): the form, and the shader.
fn emitted(native: &NativeProgram, wide_approx: bool) -> (String, super::HostShader) {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, native).expect("io::raise");
    let bytes = crate::dxbc::build(&[]);
    let c = super::Container::parse(&bytes).expect("container");
    let wave = lower::TEST_HOST_WAVE.replace(Some(32));
    let was = super::TEST_WIDE_APPROX.replace(Some(wide_approx));
    let r = super::emit(&c, native, &p, &st, &io, &res);
    super::TEST_WIDE_APPROX.set(was);
    lower::TEST_HOST_WAVE.set(wave);
    let shader = r.expect("translates");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    (super::last_form(), shader)
}

#[test]
fn a_count_in_a_kept_lane_branch_takes_the_wave_wide_form_before_an_approximate_one() {
    let native = count_in_a_kept_branch(false);
    // Without the switch: each host wave counts its own half (approximate).
    let (off, _) = emitted(&native, false);
    assert!(off.contains("APPROXIMATE"), "{off}");
    // With it: the branch taken wave-wide, the count of both halves (exact).
    let (on, shader) = emitted(&native, true);
    assert_eq!(on, "wave-wide branches");

    // On WARP (never the card), where it takes waves of 32: the console's count whichever half
    // the counted lane is in, and s20 as the console leaves it.
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(true) else {
        eprintln!("no SM 6.6 WARP device; the dispatch is not run");
        return;
    };
    let (root, pso) = match host.compute_pipeline(&shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("WARP does not take this program (not waves of 32?): {e}; the dispatch is not run");
            return;
        }
    };
    // (s0: threads below it take the branch; s2: the thread counted)
    for (below, counted) in [(64u32, 40u32), (64, 3), (40, 35), (40, 50), (10, 5), (0, 5)] {
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            if let (RootType::Constants, 0, d) = (u.root, u.slot, u.dword) {
                ud[u.index as usize] = [below, 0, counted, 1][d as usize % 4];
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
        let n = u32::from(counted < below);
        for t in 0..64u32 {
            let inside = t < below;
            assert_eq!(out[0][t as usize], if inside { n } else { 0 }, "count, thread {t}, s0 {below}, s2 {counted}");
            assert_eq!(out[0][64 + t as usize], u32::from(below > 0), "s20, thread {t}, s0 {below}");
        }
    }
    eprintln!("ran on WARP: the count of both halves in every case");
}

/// In a loop the wave-wide form would put its group barriers into the loop:
/// the switch leaves such a program on the approximate form it had.
#[test]
fn a_count_in_a_kept_lane_branch_in_a_loop_keeps_its_form() {
    let native = count_in_a_kept_branch(true);
    let (off, _) = emitted(&native, false);
    let (on, _) = emitted(&native, true);
    assert!(off.contains("APPROXIMATE"), "{off}");
    assert_eq!(on, off);
}
