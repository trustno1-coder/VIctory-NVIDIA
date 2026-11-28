//! The tile split of `water_shading:CS_TransmissionVolumetrics_GenerateTiles` (the game,
//! `1aa247ed3653b616`) as a small wave64 program, translated by the production path for a
//! host of 32-lane waves and run on the card: each lane is a tile; tiles without water leave
//! at once; the others look for a light in a loop that each lane leaves on its own; then an
//! if/else appends the tile to one of two lists in one buffer, through one atomic per list
//! (`s_ff1`/`s_bcnt1` of EXEC, `v_readfirstlane`, `v_mbcnt`), the second list growing down
//! from the end. Every tile with water must be in exactly one list, the right one.
//!
//! `cargo test --release -p victory-shader --lib water_tiles -- --ignored --nocapture`

use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32_UINT;

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, lower};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{ShaderError, dxbc, io, resource, spill, structure};

const VCC: u32 = 0x6A;
const EXEC: u32 = 0x7E;
const EXEC_LO: u32 = 0x7E;
const EXEC_HI: u32 = 0x7F;
const ZERO: u32 = 0x80;
const ONE: u32 = 0x81;
const MINUS_ONE: u32 = 0xC1;
const fn v(n: u32) -> u32 {
    256 + n
}
const fn int(n: u32) -> u32 {
    0x80 + n
}

/// A tiny assembler with branch labels.
#[derive(Default)]
struct Asm {
    words: Vec<u32>,
    labels: Vec<Option<usize>>,
    fixups: Vec<(usize, usize)>,
}

impl Asm {
    fn label(&mut self) -> usize {
        self.labels.push(None);
        self.labels.len() - 1
    }
    fn here(&mut self, l: usize) {
        self.labels[l] = Some(self.words.len());
    }
    fn w(&mut self, words: &[u32]) {
        self.words.extend_from_slice(words);
    }
    fn sop1(&mut self, op: u32, sdst: u32, src: u32) {
        self.w(&[0xBE80_0000 | sdst << 16 | op << 8 | src]);
    }
    fn sop2(&mut self, op: u32, sdst: u32, src0: u32, src1: u32) {
        self.w(&[0x8000_0000 | op << 23 | sdst << 16 | src1 << 8 | src0]);
    }
    fn sopp(&mut self, op: u32, target: usize) {
        self.fixups.push((self.words.len(), target));
        self.w(&[0xBF80_0000 | op << 16]);
    }
    fn vop1(&mut self, op: u32, vdst: u32, src: u32) {
        self.w(&[0x7E00_0000 | vdst << 17 | op << 9 | src]);
    }
    fn vop2(&mut self, op: u32, vdst: u32, src0: u32, vsrc1: u32) {
        self.w(&[op << 25 | vdst << 17 | vsrc1 << 9 | src0]);
    }
    fn vopc(&mut self, op: u32, src0: u32, vsrc1: u32) {
        self.w(&[0x7C00_0000 | op << 17 | vsrc1 << 9 | src0]);
    }
    fn vop3(&mut self, op: u32, vdst: u32, src0: u32, src1: u32, src2: u32) {
        self.w(&[0x35 << 26 | op << 16 | vdst, src0 | src1 << 9 | src2 << 18]);
    }
    /// MUBUF; `flags`: bit 12 offen, 13 idxen, 14 glc.
    fn mubuf(&mut self, op: u32, vdata: u32, vaddr: u32, srsrc: u32, flags: u32, offset: u32) {
        self.w(&[
            0x38 << 26 | op << 18 | flags | offset,
            0x80 << 24 | (srsrc >> 2) << 16 | vdata << 8 | vaddr,
        ]);
    }
    fn waitcnt_all(&mut self) {
        self.w(&[0xBF8C_0070]);
    }
    fn finish(mut self) -> Vec<u32> {
        for &(at, l) in &self.fixups {
            let target = self.labels[l].expect("label placed") as i64;
            let delta = target - (at as i64 + 1);
            self.words[at] |= (delta as i16 as u16) as u32;
        }
        self.words
    }
}

const S_MOV_B32: u32 = 0x03;
const S_MOV_B64: u32 = 0x04;
const S_BCNT1_I32_B64: u32 = 0x10;
const S_FF1_I32_B64: u32 = 0x14;
const S_AND_SAVEEXEC_B64: u32 = 0x24;
const S_ADD_I32: u32 = 0x02;
const S_AND_B64: u32 = 0x0F;
const S_OR_B64: u32 = 0x11;
const S_ANDN2_B64: u32 = 0x15;
const S_BRANCH: u32 = 0x02;
const S_CBRANCH_SCC1: u32 = 0x05;
const S_CBRANCH_EXECZ: u32 = 0x08;
const V_MOV_B32: u32 = 0x01;
const V_READFIRSTLANE_B32: u32 = 0x02;
const V_ADD_NC_U32: u32 = 0x25;
const V_LSHLREV_B32: u32 = 0x1A;
const V_CMP_EQ_U32: u32 = 0xC2;
const V_CMP_NE_U32: u32 = 0xC5;
const V_CMPX_EQ_U32: u32 = 0xD2;
const V_CMPX_NE_U32: u32 = 0xD5;
const V_CMPX_EQ_I32: u32 = 0x92;
const V3_CMP_GT_U32: u32 = 0x0C4;
const V3_CNDMASK_B32: u32 = 0x101;
const V3_MBCNT_LO: u32 = 0x365;
const V3_MBCNT_HI: u32 = 0x366;
const V3_XAD_U32: u32 = 0x345;
const BUFFER_LOAD_FORMAT_X: u32 = 0x00;
const BUFFER_STORE_DWORD: u32 = 0x1C;
const BUFFER_ATOMIC_ADD: u32 = 0x32;
const OFFEN: u32 = 1 << 12;
const IDXEN: u32 = 1 << 13;
const GLC: u32 = 1 << 14;

/// Lanes (tiles), dwords of the output (two counters, then the lists).
const LANES: u32 = 64;
const LIST: u32 = 16;
const OUT_DWORDS: u32 = LIST + LANES;

/// The append of GenerateTiles: `v5` = this lane's place in the list whose counter is at
/// byte `counter` of the output.
fn append(a: &mut Asm, counter: u32) {
    let after = a.label();
    a.vop3(V3_MBCNT_LO, 5, MINUS_ONE, ZERO, 0);
    a.sop1(S_MOV_B64, 24, EXEC);
    a.sop1(S_FF1_I32_B64, 26, EXEC);
    a.sop1(S_MOV_B64, 30, EXEC);
    a.vop3(V3_MBCNT_HI, 6, MINUS_ONE, v(5), 0);
    a.vop1(V_MOV_B32, 5, ZERO);
    a.vopc(V_CMPX_EQ_I32, 26, 6);
    a.sopp(S_CBRANCH_EXECZ, after);
    a.sop1(S_BCNT1_I32_B64, 29, 24);
    a.vop1(V_MOV_B32, 5, 29);
    a.mubuf(BUFFER_ATOMIC_ADD, 5, 0, 12, GLC, counter);
    a.here(after);
    a.sop1(S_MOV_B64, EXEC, 30);
    a.vop3(V3_MBCNT_LO, 6, EXEC_LO, ZERO, 0);
    a.waitcnt_all();
    a.vop1(V_READFIRSTLANE_B32, 29, v(5));
    a.vop3(V3_MBCNT_HI, 5, EXEC_HI, v(6), 0);
    a.vop2(V_ADD_NC_U32, 5, 29, 5);
}

fn program() -> Vec<u32> {
    let mut a = Asm::default();
    let (end, head, body, els, store) = (a.label(), a.label(), a.label(), a.label(), a.label());
    a.sop1(S_MOV_B32, 5, ZERO);
    // s[8:11] the input (typed, R32_UINT: found_at[64], limit[64], water[64]), s[12:15] the output
    a.w(&[0x3D << 26 | 0x02 << 18 | 8 << 6 | 4 >> 1, 125 << 25]);
    a.w(&[0x3D << 26 | 0x02 << 18 | 12 << 6 | 4 >> 1, 125 << 25 | 32]);
    a.w(&[0xBF8C_C07F]);
    // water[lane]: tiles without water leave
    a.sop1(S_MOV_B32, 16, 0xFF);
    a.w(&[128]);
    a.vop2(V_ADD_NC_U32, 1, 16, 0);
    a.mubuf(BUFFER_LOAD_FORMAT_X, 4, 1, 8, IDXEN, 0);
    a.waitcnt_all();
    a.vopc(V_CMPX_NE_U32, ZERO, 4);
    a.sopp(S_CBRANCH_EXECZ, end);
    // found_at in v9, limit in v7
    a.mubuf(BUFFER_LOAD_FORMAT_X, 9, 0, 8, IDXEN, 0);
    a.vop2(V_ADD_NC_U32, 2, int(64), 0);
    a.mubuf(BUFFER_LOAD_FORMAT_X, 7, 2, 8, IDXEN, 0);
    a.waitcnt_all();
    // the loop: each lane leaves when its light is found (v10) or past its limit
    a.sop1(S_MOV_B64, 2, EXEC);
    a.sop1(S_MOV_B32, 22, ZERO);
    a.sop1(S_MOV_B64, 20, EXEC);
    a.sopp(S_BRANCH, body);
    a.here(head);
    a.sop2(S_AND_B64, EXEC, 0, 20);
    a.here(body);
    a.vop1(V_MOV_B32, 10, ZERO);
    a.sop2(S_ADD_I32, 22, 22, ONE);
    // (a load in the body, as the game's loop reads a cluster's mask each turn)
    a.mubuf(BUFFER_LOAD_FORMAT_X, 11, 0, 8, IDXEN, 0);
    a.waitcnt_all();
    a.vopc(V_CMP_EQ_U32, 22, 11);
    a.vop3(V3_CNDMASK_B32, 10, ZERO, ONE, VCC);
    a.vopc(V_CMP_NE_U32, ZERO, 10);
    a.vop3(V3_CMP_GT_U32, 0, 22, v(7), 0);
    a.sop2(S_OR_B64, 0, VCC, 0);
    a.sop1(S_AND_SAVEEXEC_B64, 0, 0);
    a.sop2(S_ANDN2_B64, 20, 20, EXEC);
    a.sopp(S_CBRANCH_SCC1, head);
    a.sop1(S_MOV_B64, EXEC, 2);
    // the split: no light -> list A (counter at byte 0), a light -> list B (byte 32, from the end)
    a.sop1(S_MOV_B64, 0, EXEC);
    a.vopc(V_CMPX_EQ_U32, ZERO, 10);
    a.sopp(S_CBRANCH_EXECZ, els);
    append(&mut a, 0);
    a.here(els);
    a.sop2(S_ANDN2_B64, EXEC, 0, EXEC);
    a.sopp(S_CBRANCH_EXECZ, store);
    append(&mut a, 32);
    a.sop1(S_MOV_B32, 28, int(64));
    a.vop3(V3_XAD_U32, 5, v(5), MINUS_ONE, 28);
    a.here(store);
    a.sop1(S_MOV_B64, EXEC, 0);
    // list[v5] = lane + 1
    a.vop2(V_ADD_NC_U32, 5, int(LIST), 5);
    a.vop2(V_LSHLREV_B32, 5, int(2), 5);
    a.vop2(V_ADD_NC_U32, 8, ONE, 0);
    a.mubuf(BUFFER_STORE_DWORD, 8, 5, 12, OFFEN, 0);
    a.here(end);
    a.w(&[0xBF81_0000]);
    a.finish()
}

fn native() -> NativeProgram {
    let words = program();
    let used = (0..2u32)
        .map(|k| UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: k * 8,
            lower_bound: k,
            stride: 4,
            dwords: 4,
            user_data: 4,
            slot: 0,
        })
        .collect();
    NativeProgram {
        stage: HwStage::Cs,
        wave64: true,
        vgprs: 16,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 4,
            slot: 0,
        }],
        used,
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1,
            threads: [LANES, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// As `super::translate` does, on a host of 32-lane waves.
fn translate(native: &NativeProgram) -> Result<HostShader, ShaderError> {
    let container = dxbc::build(&[]);
    let c = dxbc::Container::parse(&container)?;
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let r = (|| {
        let ir = spill::build(native)?;
        let st = structure::structure(&ir);
        let res = resource::track(&ir, &native.layout, native.stage, st.as_ref().ok())?;
        let io = io::raise(&ir, native)?;
        let st = st?;
        match super::emit(&c, native, &ir, &st, &io, &res) {
            Err(ShaderError::Unsupported { what, .. }) if what == lower::FIRST_HALF => {
                lower::with_first_half_pairs(|| super::emit(&c, native, &ir, &st, &io, &res))
            }
            other => other,
        }
    })();
    lower::TEST_HOST_WAVE.set(was);
    r
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 32) as u32
    }
}

#[test]
#[ignore = "on the card: the tile split of the water's GenerateTiles"]
fn water_tiles_split_puts_each_tile_in_one_list() {
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let shader = translate(&native()).expect("translates");
    let host = Host::new(false).expect("an SM 6.6 card");
    let (root, pso) = host.compute_pipeline(&shader).expect("pipeline");
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut bad = 0;
    for case in 0..200 {
        let mut input = vec![0u32; 3 * LANES as usize];
        let water_odds = [100, 90, 50, 10][case % 4];
        for lane in 0..LANES as usize {
            input[lane] = rng.next() % 10; // found_at: 0 none, 1..9
            input[64 + lane] = rng.next() % 9; // limit
            input[128 + lane] = u32::from(rng.next() % 100 < water_odds);
        }
        let views = [
            View::TypedUav {
                data: &input,
                format: DXGI_FORMAT_R32_UINT,
                elem_bytes: 4,
            },
            View::RawUav(OUT_DWORDS),
        ];
        let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &views,
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .expect("dispatch");
        let out = &out[1];
        let (mut want_a, mut want_b) = (Vec::new(), Vec::new());
        for lane in 0..LANES {
            let l = lane as usize;
            if input[128 + l] == 0 {
                continue;
            }
            let found = input[l] >= 1 && input[l] <= input[64 + l] + 1;
            if found {
                want_b.push(lane + 1)
            } else {
                want_a.push(lane + 1)
            }
        }
        let (ca, cb) = (out[0] as usize, out[8] as usize);
        let list = &out[LIST as usize..];
        let mut got_a: Vec<u32> = list[..ca.min(64)].to_vec();
        let mut got_b: Vec<u32> = list[64usize.saturating_sub(cb)..].to_vec();
        got_a.sort_unstable();
        got_b.sort_unstable();
        if ca != want_a.len() || cb != want_b.len() || got_a != want_a || got_b != want_b {
            bad += 1;
            if bad <= 5 {
                eprintln!(
                    "case {case} (water {water_odds} %): counts A {ca}/{} B {cb}/{}\n  A got {got_a:?}\n  A want {want_a:?}\n  B got {got_b:?}\n  B want {want_b:?}",
                    want_a.len(),
                    want_b.len()
                );
            }
        }
    }
    assert_eq!(bad, 0, "{bad} of 200 cases put tiles in the wrong list");
}
