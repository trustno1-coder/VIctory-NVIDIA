use super::{tests::lowers_valu, whole_wave_side_effect};

const S_ENDPGM: u32 = 0xBF81_0000;

#[test]
fn whole_wave_sections_reject_side_effects() {
    let save = 0xBE86_3DC1; // s_or_saveexec_b32 s6, -1
    let restore = 0xBEFE_0306; // s_mov_b32 exec_lo, s6
    let barrier = [0xBF8A_0000]; // s_barrier
    let store = [0xE070_0000, 0x8002_0000]; // buffer_store_dword v0, off, s[8:11], 0
    let mov = 0x7E00_0301; // v_mov_b32 v0, v1
    let load_lds = [0xE031_0000, 0x8002_0000]; // buffer_load_dword to LDS

    for effect in [barrier.as_slice(), store.as_slice()] {
        let outside = [&[mov, save, restore], effect].concat();
        lowers_valu(&outside).unwrap();

        let inside = [&[mov, save], effect, &[restore]].concat();
        let error = lowers_valu(&inside).expect_err("the translator cannot preserve this effect");
        assert!(
            matches!(
                error,
                crate::ShaderError::Unsupported {
                    offset: 36,
                    what: "a side effect in a whole-wave section",
                }
            ),
            "{error}"
        );
    }
    assert!(matches!(
        lowers_valu(&[mov, save, load_lds[0], load_lds[1], restore]),
        Err(crate::ShaderError::Unsupported {
            offset: 36,
            what: "a side effect in a whole-wave section",
        })
    ));
}

#[test]
fn global_loads_into_lds_are_side_effects() {
    let words: [u32; 8] = [
        0x7E04_0280, // v_mov_b32 v2, 0
        0xBE84_0380, // s_mov_b32 s4, 0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xBE86_3DC1, // s_or_saveexec_b32 s6, -1
        0xDC30_A400,
        0x0004_0002, // global_load_dword v2, s[4:5] inst_offset:1024 lds
        0xBEFE_0306, // s_mov_b32 exec_lo, s6
        S_ENDPGM,
    ];
    let code: Vec<_> = words.into_iter().flat_map(u32::to_le_bytes).collect();
    let program = crate::ir::build(&code, false, 256).unwrap();
    let load = &program.blocks[0].insts[4];
    assert!(matches!(load.fields, crate::gfx10::Fields::Flat { lds: true, .. }));
    assert!(whole_wave_side_effect(load));
}
