//! One synthetic 8 × 8 tile. Each lane updates its pixel from the pixel in
//! the opposite half of the console wave, until that lane's round count expires.

use super::*;
use crate::native::{ComputeIo, ResourceType, RootParam, StageIo, UsedResource};

fn tile(shared_light: bool) -> NativeProgram {
    let mut words = vec![
        0xBE81_0380u32, // s_mov_b32 s1, 0
        0xF40C_0200,
        0xFA00_0000, // s_load_dwordx8 s[8:15], s[0:1], 0
        0x7E04_0300, // v_mov_b32 v2, v0 (initial intensity)
    ];
    if shared_light {
        words.extend_from_slice(&[
            0x3402_0082, // v_lshlrev_b32 v1, 2, v0 (pixel byte offset)
            0x3A06_00BF, // v_xor_b32 v3, 63, v0 (partner)
            0x3406_0682, // v_lshlrev_b32 v3, 2, v3
            0x2C0E_0084, // v_lshrrev_b32 v7, 4, v0
            0x4A0E_0E81, // v_add_nc_u32 v7, 1, v7 (one to four rounds)
            0x7E0A_0280, // v_mov_b32 v5, 0
            0xBE90_047E, // s_mov_b64 s[16:17], exec
            0x7DA8_0B07, // loop: v_cmpx_gt_u32 exec, v7, v5
            0xBF88_0008, // s_cbranch_execz done
            0xD834_0000,
            0x0000_0201, // ds_write_b32 v1, v2
            0xD8D8_0000,
            0x0400_0003, // ds_read_b32 v4, v3
            0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
            0x4A04_0902, // v_add_nc_u32 v2, v2, v4
            0x4A0A_0A81, // v_add_nc_u32 v5, 1, v5
            0xBF82_FFF6, // s_branch loop
            0xBEFE_0410, // done: s_mov_b64 exec, s[16:17]
        ]);
    }
    words.extend_from_slice(&[
        0x360C_0087, // v_and_b32 v6, 7, v0 (x)
        0x2C0E_0083, // v_lshrrev_b32 v7, 3, v0 (y)
        0x7E04_0D02, // v_cvt_f32_u32 v2, v2
        0x1004_04FF,
        0x3B00_0000, // v_mul_f32 v2, 1/512, v2 (red)
        0x7E06_02F0, // v_mov_b32 v3, 0.5 (green)
        0x7E08_0280, // v_mov_b32 v4, 0 (blue)
        0x7E0A_02F2, // v_mov_b32 v5, 1.0 (alpha)
        0xF020_1F08,
        0x0002_0206, // image_store v[2:5], v[6:7], s[8:15] dmask:0xf 2D
        0xBF81_0000, // s_endpgm
    ]);
    NativeProgram {
        stage: HwStage::Cs,
        wave64: true,
        vgprs: 8,
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
            stride: 4,
            dwords: 8,
            user_data: 0,
            slot: 0,
        }],
        io: StageIo::Cs(ComputeIo {
            threads: [64, 1, 1],
            rsrc2: (u32::from(shared_light) << 15) | (1 << 1),
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

fn translate_tile(shared_light: bool) -> Result<HostShader, ShaderError> {
    let native = tile(shared_light);
    // Exercise the existing widening policy for the 64-thread submerged kernel.
    // Only the selection digest is shared. The instructions and IO above are synthetic.
    let c = Container {
        version: (1, 0),
        parts: vec![],
        digest: [
            0xf1, 0xf1, 0x61, 0xe4, 0x7e, 0xef, 0xa3, 0x64, 0x69, 0x18, 0x39, 0xfd, 0x72, 0x06,
            0xcc, 0xf6,
        ],
    };
    let p = ir::build(&native.code, true, native.vgprs).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    let previous = lower::TEST_HOST_WAVE.replace(Some(32));
    let shader = emit(&c, &native, &p, &st, &io, &res);
    lower::TEST_HOST_WAVE.set(previous);
    shader
}

#[test]
fn split_wave_lds_loop() {
    translate_tile(true).expect("translate the LDS loop on a wave32 host");
}

#[cfg(windows)]
#[test]
#[ignore = "requires a wave32-capable D3D12 device (SM 6.6)"]
fn tile_pixels_match_the_console_wave() {
    use super::host::{Dispatch, Host, Root, View};

    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let host = Host::new(false).expect("SM 6.6 D3D12 device");
    for shared_light in [false, true] {
        let shader = translate_tile(shared_light).unwrap();
        let (root, pipeline) = host.compute_pipeline(&shader).expect("compute pipeline");
        let out = host
            .dispatch(
                &root,
                &pipeline,
                &Dispatch {
                    roots: vec![(
                        0,
                        Root::Constants(vec![0; shader.user_data_dwords().max(1) as usize]),
                    )],
                    views: &[View::TextureUav(8, 8)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .expect("dispatch and copy the texture back");

        // Console lanes run in lockstep. All active lanes store before any reads.
        let mut intensity: [u32; 64] = std::array::from_fn(|t| t as u32);
        let mut lds = [0; 64];
        let rounds = if shared_light { 4 } else { 0 };
        for round in 0..rounds {
            let first = 16 * round;
            lds[first..].copy_from_slice(&intensity[first..]);
            for t in first..64 {
                intensity[t] += lds[t ^ 63];
            }
        }
        assert_eq!(out[0].len(), 64 * 4);
        for (t, rgba) in out[0].chunks_exact(4).enumerate() {
            let expected = [intensity[t] as f32 / 512.0, 0.5, 0.0, 1.0];
            assert_eq!(
                rgba,
                expected.map(f32::to_bits),
                "pixel {t}, shared light {shared_light}"
            );
        }
        eprintln!("64 RGBA32F pixels matched (shared light {shared_light})");
    }
}
