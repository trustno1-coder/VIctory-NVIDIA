//! Differential fuzzing of the translator against an independent interpreter of
//! the RDNA2 (GFX10.3) ISA, "the oracle".
//!
//! Random compute programs – integer VALU and SALU arithmetic, lane compares,
//! lane-divergent if/else under EXEC, uniform ifs and loops on SCC, cross-lane
//! reads and writes – are described as [`Item`]s. The description is encoded to
//! GFX10 machine code, translated by the production path (`spill::build`, `structure`,
//! `resource::track`, `io::raise`, then [`super::emit`] retried as [`super::translate`] does), run on
//! the host GPU, and compared lane by lane with what [`Machine`] computes from the
//! same description. The interpreter shares nothing with the translator: not the
//! decoder, not the IR, not the lowering. The encoder is checked against the
//! decoder's instruction names (Mesa's ACO tables) so that a mismatch means the
//! translation, not a wrong encoding.
//!
//! Wave64 programs run on a host of 32-lane waves (NVIDIA; `TEST_HOST_WAVE`), twice:
//! with only exact translations (`VICTORY_WAVE_LOCAL=idiom`) and as the production
//! default, which may fall back to approximate forms (each half-wave on its own).
//!
//! `cargo test -p victory-shader --lib oracle -- --ignored --nocapture --test-threads=1`
//! with `VICTORY_ORACLE_RUNS=<programs per wave size>` (200) and
//! `VICTORY_ORACLE_SEED=<n>` (1).

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, lower};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{ShaderError, dxbc, gfx10, io, resource, spill, structure};

// ---------------------------------------------------------------------------
// Program description
// ---------------------------------------------------------------------------

/// An operand. `S(106)`/`S(107)` are VCC's halves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Src {
    V(u8),
    S(u8),
    /// An inline integer, -16..=64.
    Int(i8),
    /// A 32-bit literal (VOP1/VOP2/VOPC/SOP only).
    Lit(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum V1 {
    Mov,
    Not,
    Bfrev,
    FfbhU,
    FfblB,
    FfbhI,
}
const V1S: [V1; 6] = [V1::Mov, V1::Not, V1::Bfrev, V1::FfbhU, V1::FfblB, V1::FfbhI];

impl V1 {
    fn op(self) -> u32 {
        match self {
            V1::Mov => 0x01,
            V1::Not => 0x37,
            V1::Bfrev => 0x38,
            V1::FfbhU => 0x39,
            V1::FfblB => 0x3a,
            V1::FfbhI => 0x3b,
        }
    }
    fn name(self) -> &'static str {
        match self {
            V1::Mov => "v_mov_b32",
            V1::Not => "v_not_b32",
            V1::Bfrev => "v_bfrev_b32",
            V1::FfbhU => "v_ffbh_u32",
            V1::FfblB => "v_ffbl_b32",
            V1::FfbhI => "v_ffbh_i32",
        }
    }
    fn eval(self, a: u32) -> u32 {
        match self {
            V1::Mov => a,
            V1::Not => !a,
            V1::Bfrev => a.reverse_bits(),
            V1::FfbhU => {
                if a == 0 {
                    u32::MAX
                } else {
                    a.leading_zeros()
                }
            }
            V1::FfblB => {
                if a == 0 {
                    u32::MAX
                } else {
                    a.trailing_zeros()
                }
            }
            V1::FfbhI => {
                if a == 0 || a == u32::MAX {
                    u32::MAX
                } else if (a as i32) < 0 {
                    (!a).leading_zeros()
                } else {
                    a.leading_zeros()
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum V2 {
    AddNc,
    SubNc,
    SubrevNc,
    And,
    Or,
    Xor,
    Lshlrev,
    Lshrrev,
    Ashrrev,
    MinI,
    MaxI,
    MinU,
    MaxU,
    MulU24,
    MulHiU24,
    MulI24,
}
const V2S: [V2; 16] = [
    V2::AddNc,
    V2::SubNc,
    V2::SubrevNc,
    V2::And,
    V2::Or,
    V2::Xor,
    V2::Lshlrev,
    V2::Lshrrev,
    V2::Ashrrev,
    V2::MinI,
    V2::MaxI,
    V2::MinU,
    V2::MaxU,
    V2::MulU24,
    V2::MulHiU24,
    V2::MulI24,
];

fn sext24(x: u32) -> i64 {
    i64::from(((x << 8) as i32) >> 8)
}

impl V2 {
    fn op(self) -> u32 {
        match self {
            V2::AddNc => 0x25,
            V2::SubNc => 0x26,
            V2::SubrevNc => 0x27,
            V2::And => 0x1b,
            V2::Or => 0x1c,
            V2::Xor => 0x1d,
            V2::Lshlrev => 0x1a,
            V2::Lshrrev => 0x16,
            V2::Ashrrev => 0x18,
            V2::MinI => 0x11,
            V2::MaxI => 0x12,
            V2::MinU => 0x13,
            V2::MaxU => 0x14,
            V2::MulU24 => 0x0b,
            V2::MulHiU24 => 0x0c,
            V2::MulI24 => 0x09,
        }
    }
    fn name(self) -> &'static str {
        match self {
            V2::AddNc => "v_add_nc_u32",
            V2::SubNc => "v_sub_nc_u32",
            V2::SubrevNc => "v_subrev_nc_u32",
            V2::And => "v_and_b32",
            V2::Or => "v_or_b32",
            V2::Xor => "v_xor_b32",
            V2::Lshlrev => "v_lshlrev_b32",
            V2::Lshrrev => "v_lshrrev_b32",
            V2::Ashrrev => "v_ashrrev_i32",
            V2::MinI => "v_min_i32",
            V2::MaxI => "v_max_i32",
            V2::MinU => "v_min_u32",
            V2::MaxU => "v_max_u32",
            V2::MulU24 => "v_mul_u32_u24",
            V2::MulHiU24 => "v_mul_hi_u32_u24",
            V2::MulI24 => "v_mul_i32_i24",
        }
    }
    fn eval(self, a: u32, b: u32) -> u32 {
        match self {
            V2::AddNc => a.wrapping_add(b),
            V2::SubNc => a.wrapping_sub(b),
            V2::SubrevNc => b.wrapping_sub(a),
            V2::And => a & b,
            V2::Or => a | b,
            V2::Xor => a ^ b,
            V2::Lshlrev => b << (a & 31),
            V2::Lshrrev => b >> (a & 31),
            V2::Ashrrev => ((b as i32) >> (a & 31)) as u32,
            V2::MinI => (a as i32).min(b as i32) as u32,
            V2::MaxI => (a as i32).max(b as i32) as u32,
            V2::MinU => a.min(b),
            V2::MaxU => a.max(b),
            V2::MulU24 => (u64::from(a & 0xFF_FFFF) * u64::from(b & 0xFF_FFFF)) as u32,
            V2::MulHiU24 => ((u64::from(a & 0xFF_FFFF) * u64::from(b & 0xFF_FFFF)) >> 32) as u32,
            V2::MulI24 => (sext24(a) * sext24(b)) as u32,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum V3 {
    MulLo,
    MulHi,
    BfeU,
    BfeI,
    Bfi,
    Alignbit,
    LshlAdd,
    Add3,
    Xad,
    LshlOr,
    AndOr,
    Or3,
    Bcnt,
    MbcntLo,
    MbcntHi,
    Med3I,
    Med3U,
    Min3I,
    Min3U,
    Max3I,
    Max3U,
}
const V3S: [V3; 21] = [
    V3::MulLo,
    V3::MulHi,
    V3::BfeU,
    V3::BfeI,
    V3::Bfi,
    V3::Alignbit,
    V3::LshlAdd,
    V3::Add3,
    V3::Xad,
    V3::LshlOr,
    V3::AndOr,
    V3::Or3,
    V3::Bcnt,
    V3::MbcntLo,
    V3::MbcntHi,
    V3::Med3I,
    V3::Med3U,
    V3::Min3I,
    V3::Min3U,
    V3::Max3I,
    V3::Max3U,
];

impl V3 {
    fn op(self) -> u32 {
        match self {
            V3::MulLo => 0x169,
            V3::MulHi => 0x16a,
            V3::BfeU => 0x148,
            V3::BfeI => 0x149,
            V3::Bfi => 0x14a,
            V3::Alignbit => 0x14e,
            V3::LshlAdd => 0x346,
            V3::Add3 => 0x36d,
            V3::Xad => 0x345,
            V3::LshlOr => 0x36f,
            V3::AndOr => 0x371,
            V3::Or3 => 0x372,
            V3::Bcnt => 0x364,
            V3::MbcntLo => 0x365,
            V3::MbcntHi => 0x366,
            V3::Med3I => 0x158,
            V3::Med3U => 0x159,
            V3::Min3I => 0x152,
            V3::Min3U => 0x153,
            V3::Max3I => 0x155,
            V3::Max3U => 0x156,
        }
    }
    fn name(self) -> &'static str {
        match self {
            V3::MulLo => "v_mul_lo_u32",
            V3::MulHi => "v_mul_hi_u32",
            V3::BfeU => "v_bfe_u32",
            V3::BfeI => "v_bfe_i32",
            V3::Bfi => "v_bfi_b32",
            V3::Alignbit => "v_alignbit_b32",
            V3::LshlAdd => "v_lshl_add_u32",
            V3::Add3 => "v_add3_u32",
            V3::Xad => "v_xad_u32",
            V3::LshlOr => "v_lshl_or_b32",
            V3::AndOr => "v_and_or_b32",
            V3::Or3 => "v_or3_b32",
            V3::Bcnt => "v_bcnt_u32_b32",
            V3::MbcntLo => "v_mbcnt_lo_u32_b32",
            V3::MbcntHi => "v_mbcnt_hi_u32_b32",
            V3::Med3I => "v_med3_i32",
            V3::Med3U => "v_med3_u32",
            V3::Min3I => "v_min3_i32",
            V3::Min3U => "v_min3_u32",
            V3::Max3I => "v_max3_i32",
            V3::Max3U => "v_max3_u32",
        }
    }
    fn arity(self) -> usize {
        match self {
            V3::MulLo | V3::MulHi | V3::Bcnt | V3::MbcntLo | V3::MbcntHi => 2,
            _ => 3,
        }
    }
    fn eval(self, a: u32, b: u32, c: u32, lane: usize) -> u32 {
        let (ia, ib, ic) = (a as i32, b as i32, c as i32);
        let med_i = ia.max(ib).min(ia.min(ib).max(ic)) as u32;
        let med_u = a.max(b).min(a.min(b).max(c));
        match self {
            V3::MulLo => a.wrapping_mul(b),
            V3::MulHi => ((u64::from(a) * u64::from(b)) >> 32) as u32,
            V3::BfeU => {
                let (off, w) = (b & 31, c & 31);
                if w == 0 { 0 } else { (a >> off) & ((1u32 << w) - 1) }
            }
            V3::BfeI => {
                let (off, w) = (b & 31, c & 31);
                if w == 0 {
                    0
                } else if off + w < 32 {
                    (((a << (32 - off - w)) as i32) >> (32 - w)) as u32
                } else {
                    ((a as i32) >> off) as u32
                }
            }
            V3::Bfi => (a & b) | (!a & c),
            V3::Alignbit => (((u64::from(a) << 32) | u64::from(b)) >> (c & 31)) as u32,
            V3::LshlAdd => (a << (b & 31)).wrapping_add(c),
            V3::Add3 => a.wrapping_add(b).wrapping_add(c),
            V3::Xad => (a ^ b).wrapping_add(c),
            V3::LshlOr => (a << (b & 31)) | c,
            V3::AndOr => (a & b) | c,
            V3::Or3 => a | b | c,
            V3::Bcnt => a.count_ones().wrapping_add(b),
            V3::MbcntLo => {
                let mask = if lane < 32 { (1u32 << lane).wrapping_sub(1) } else { u32::MAX };
                (a & mask).count_ones().wrapping_add(b)
            }
            V3::MbcntHi => {
                let mask = if lane < 32 { 0 } else { (1u32 << (lane - 32)).wrapping_sub(1) };
                (a & mask).count_ones().wrapping_add(b)
            }
            V3::Med3I => med_i,
            V3::Med3U => med_u,
            V3::Min3I => ia.min(ib).min(ic) as u32,
            V3::Min3U => a.min(b).min(c),
            V3::Max3I => ia.max(ib).max(ic) as u32,
            V3::Max3U => a.max(b).max(c),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cmp {
    Lt,
    Eq,
    Le,
    Gt,
    Ne,
    Ge,
}
const CMPS: [Cmp; 6] = [Cmp::Lt, Cmp::Eq, Cmp::Le, Cmp::Gt, Cmp::Ne, Cmp::Ge];

impl Cmp {
    fn eval(self, signed: bool, a: u32, b: u32) -> bool {
        let o = if signed { (a as i32).cmp(&(b as i32)) } else { a.cmp(&b) };
        use std::cmp::Ordering::*;
        match self {
            Cmp::Lt => o == Less,
            Cmp::Eq => o == Equal,
            Cmp::Le => o != Greater,
            Cmp::Gt => o == Greater,
            Cmp::Ne => o != Equal,
            Cmp::Ge => o != Less,
        }
    }
    /// VOPC opcode: 0x80 (i32) / 0xc0 (u32) + lt 1, eq 2, le 3, gt 4, ne 5, ge 6.
    fn vopc(self, signed: bool) -> u32 {
        let base = if signed { 0x80 } else { 0xc0 };
        base + match self {
            Cmp::Lt => 1,
            Cmp::Eq => 2,
            Cmp::Le => 3,
            Cmp::Gt => 4,
            Cmp::Ne => 5,
            Cmp::Ge => 6,
        }
    }
    /// SOPC opcode: i32 eq 0, lg 1, gt 2, ge 3, lt 4, le 5; u32 the same + 6.
    fn sopc(self, signed: bool) -> u32 {
        let base = if signed { 0 } else { 6 };
        base + match self {
            Cmp::Eq => 0,
            Cmp::Ne => 1,
            Cmp::Gt => 2,
            Cmp::Ge => 3,
            Cmp::Lt => 4,
            Cmp::Le => 5,
        }
    }
    fn suffix(self) -> &'static str {
        match self {
            Cmp::Lt => "lt",
            Cmp::Eq => "eq",
            Cmp::Le => "le",
            Cmp::Gt => "gt",
            Cmp::Ne => "ne",
            Cmp::Ge => "ge",
        }
    }
    fn v_name(self, signed: bool) -> &'static str {
        const N: [[&str; 2]; 6] = [
            ["v_cmp_lt_u32", "v_cmp_lt_i32"],
            ["v_cmp_eq_u32", "v_cmp_eq_i32"],
            ["v_cmp_le_u32", "v_cmp_le_i32"],
            ["v_cmp_gt_u32", "v_cmp_gt_i32"],
            ["v_cmp_ne_u32", "v_cmp_ne_i32"],
            ["v_cmp_ge_u32", "v_cmp_ge_i32"],
        ];
        N[self as usize][usize::from(signed)]
    }
    fn s_name(self, signed: bool) -> &'static str {
        const N: [[&str; 2]; 6] = [
            ["s_cmp_lt_u32", "s_cmp_lt_i32"],
            ["s_cmp_eq_u32", "s_cmp_eq_i32"],
            ["s_cmp_le_u32", "s_cmp_le_i32"],
            ["s_cmp_gt_u32", "s_cmp_gt_i32"],
            ["s_cmp_lg_u32", "s_cmp_lg_i32"],
            ["s_cmp_ge_u32", "s_cmp_ge_i32"],
        ];
        N[self as usize][usize::from(signed)]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum S1 {
    Mov,
    Not,
    Brev,
    Bcnt1,
    Ff1,
}
const S1S: [S1; 5] = [S1::Mov, S1::Not, S1::Brev, S1::Bcnt1, S1::Ff1];

impl S1 {
    fn op(self) -> u32 {
        match self {
            S1::Mov => 0x03,
            S1::Not => 0x07,
            S1::Brev => 0x0b,
            S1::Bcnt1 => 0x0f,
            S1::Ff1 => 0x13,
        }
    }
    fn name(self) -> &'static str {
        match self {
            S1::Mov => "s_mov_b32",
            S1::Not => "s_not_b32",
            S1::Brev => "s_brev_b32",
            S1::Bcnt1 => "s_bcnt1_i32_b32",
            S1::Ff1 => "s_ff1_i32_b32",
        }
    }
    /// The result and the new SCC (`None`: unchanged).
    fn eval(self, a: u32) -> (u32, Option<bool>) {
        match self {
            S1::Mov => (a, None),
            S1::Not => (!a, Some(!a != 0)),
            S1::Brev => (a.reverse_bits(), None),
            S1::Bcnt1 => (a.count_ones(), Some(a != 0)),
            S1::Ff1 => (if a == 0 { u32::MAX } else { a.trailing_zeros() }, None),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum S2 {
    AddU,
    SubU,
    MinI,
    MinU,
    MaxI,
    MaxU,
    Cselect,
    And,
    Or,
    Xor,
    Andn2,
    Lshl,
    Lshr,
    Ashr,
    Mul,
}
const S2S: [S2; 15] = [
    S2::AddU,
    S2::SubU,
    S2::MinI,
    S2::MinU,
    S2::MaxI,
    S2::MaxU,
    S2::Cselect,
    S2::And,
    S2::Or,
    S2::Xor,
    S2::Andn2,
    S2::Lshl,
    S2::Lshr,
    S2::Ashr,
    S2::Mul,
];

impl S2 {
    fn op(self) -> u32 {
        match self {
            S2::AddU => 0x00,
            S2::SubU => 0x01,
            S2::MinI => 0x06,
            S2::MinU => 0x07,
            S2::MaxI => 0x08,
            S2::MaxU => 0x09,
            S2::Cselect => 0x0a,
            S2::And => 0x0e,
            S2::Or => 0x10,
            S2::Xor => 0x12,
            S2::Andn2 => 0x14,
            S2::Lshl => 0x1e,
            S2::Lshr => 0x20,
            S2::Ashr => 0x22,
            S2::Mul => 0x26,
        }
    }
    fn name(self) -> &'static str {
        match self {
            S2::AddU => "s_add_u32",
            S2::SubU => "s_sub_u32",
            S2::MinI => "s_min_i32",
            S2::MinU => "s_min_u32",
            S2::MaxI => "s_max_i32",
            S2::MaxU => "s_max_u32",
            S2::Cselect => "s_cselect_b32",
            S2::And => "s_and_b32",
            S2::Or => "s_or_b32",
            S2::Xor => "s_xor_b32",
            S2::Andn2 => "s_andn2_b32",
            S2::Lshl => "s_lshl_b32",
            S2::Lshr => "s_lshr_b32",
            S2::Ashr => "s_ashr_i32",
            S2::Mul => "s_mul_i32",
        }
    }
    /// The result and the new SCC (`None`: unchanged).
    fn eval(self, a: u32, b: u32, scc: bool) -> (u32, Option<bool>) {
        let nz = |r: u32| (r, Some(r != 0));
        match self {
            S2::AddU => {
                let (r, c) = a.overflowing_add(b);
                (r, Some(c))
            }
            S2::SubU => (a.wrapping_sub(b), Some(a < b)),
            S2::MinI => {
                let lt = (a as i32) < (b as i32);
                (if lt { a } else { b }, Some(lt))
            }
            S2::MinU => (if a < b { a } else { b }, Some(a < b)),
            S2::MaxI => {
                let gt = (a as i32) > (b as i32);
                (if gt { a } else { b }, Some(gt))
            }
            S2::MaxU => (if a > b { a } else { b }, Some(a > b)),
            S2::Cselect => (if scc { a } else { b }, None),
            S2::And => nz(a & b),
            S2::Or => nz(a | b),
            S2::Xor => nz(a ^ b),
            S2::Andn2 => nz(a & !b),
            S2::Lshl => nz(a << (b & 31)),
            S2::Lshr => nz(a >> (b & 31)),
            S2::Ashr => nz(((a as i32) >> (b & 31)) as u32),
            S2::Mul => (a.wrapping_mul(b), None),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Inst {
    V1 { op: V1, dst: u8, a: Src },
    V2 { op: V2, dst: u8, a: Src, b: u8 },
    /// `v_cndmask_b32 dst, a, b, vcc`: VCC ? b : a.
    Cnd { dst: u8, a: Src, b: u8 },
    V3 { op: V3, dst: u8, src: [Src; 3] },
    /// `v_cmp_* vcc, a, vb`.
    Cmp { cmp: Cmp, signed: bool, a: Src, b: u8 },
    FirstLane { dst: u8, v: u8 },
    ReadLane { dst: u8, v: u8, lane: Src },
    WriteLane { dst: u8, val: Src, lane: Src },
    S1 { op: S1, dst: u8, a: Src },
    S2 { op: S2, dst: u8, a: Src, b: Src },
    SCmp { cmp: Cmp, signed: bool, a: Src, b: Src },
}

#[derive(Clone, Debug, PartialEq)]
enum Item {
    I(Inst),
    /// Lane-divergent: `v_cmp`, `s_and_saveexec`, `s_cbranch_execz`, then; else
    /// under `s_andn2 exec, saved, cond`; `s_mov exec, saved`.
    If { cmp: Cmp, signed: bool, a: Src, b: u8, then: Vec<Item>, els: Option<Vec<Item>> },
    /// Uniform: `s_cmp`, `s_cbranch_scc0` over the body.
    UIf { cmp: Cmp, signed: bool, a: u8, b: Src, body: Vec<Item> },
    /// Uniform loop of `n` turns (`s_sub_u32`, `s_cmp_lg_u32`, `s_cbranch_scc1`).
    Loop { n: u8, body: Vec<Item> },
}

#[derive(Clone, Debug, PartialEq)]
struct Prog {
    wave64: bool,
    seeds: [u32; 4],
    body: Vec<Item>,
    /// VCC also stored as numbers (a lane mask read as data), not only as each lane's bit.
    raw_masks: bool,
}

/// Known kinds of difference the generator can stay clear of
/// (`VICTORY_ORACLE_AVOID=salu_in_branch,minmax_scc,lane_range,writelane_inactive,mask_data`),
/// so that a run finds other kinds.
#[derive(Clone, Copy, Debug, Default)]
struct Avoid {
    /// Scalar writes (SALU, `v_readfirstlane`, `v_readlane`, uniform ifs and loops) inside
    /// lane-divergent branches.
    salu_in_branch: bool,
    /// `s_min_*` / `s_max_*` (their SCC).
    minmax_scc: bool,
    /// Lane selects of `v_readlane` / `v_writelane` outside the wave.
    lane_range: bool,
    /// `v_writelane` inside lane-divergent branches (a lane EXEC leaves out).
    writelane_inactive: bool,
    /// VCC stored as numbers.
    mask_data: bool,
}

impl Avoid {
    fn from_env() -> Avoid {
        let v = std::env::var("VICTORY_ORACLE_AVOID").unwrap_or_default();
        let has = |k: &str| v.split(',').any(|x| x.trim() == k || x.trim() == "all");
        Avoid {
            salu_in_branch: has("salu_in_branch"),
            minmax_scc: has("minmax_scc"),
            lane_range: has("lane_range"),
            writelane_inactive: has("writelane_inactive"),
            mask_data: has("mask_data"),
        }
    }
}

// Register plan. s0..s3 seeds, s4 table address, s5 0, s[8:11] the UAV's descriptor,
// s12/s13 hash constants; s16..s27 work; s29 SCC capture; s30.. loop counters;
// s40.. saved EXEC, s48.. conditions (pairs, by depth). v0 thread id, v1..v12 work,
// v13 the constant 1, v14 store address, v15 store temporary.
const VCC_LO: u8 = 106;
const VCC_HI: u8 = 107;
const EXEC_LO: u8 = 126;
const SLOTS: usize = 32;
/// Per lane: v1..v12, s16..s27, the lane's VCC bit, vcc_lo, vcc_hi, SCC.
const OUT_USED: usize = 28;

fn save_reg(depth: usize) -> u8 {
    40 + 2 * depth as u8
}
fn cond_reg(depth: usize) -> u8 {
    48 + 2 * depth as u8
}
fn loop_reg(depth: usize) -> u8 {
    30 + depth as u8
}

/// The fixed start: inputs per lane from the thread id and the four seeds, defined
/// SGPRs, VCC and SCC (nothing is read undefined).
fn prologue() -> Vec<Inst> {
    use Src::*;
    let mut p = vec![
        Inst::S1 { op: S1::Mov, dst: 12, a: Lit(0x9E37_79B9) },
        Inst::S1 { op: S1::Mov, dst: 13, a: Lit(0x85EB_CA6B) },
        Inst::V2 { op: V2::Lshlrev, dst: 14, a: Int(7), b: 0 },
        Inst::V3 { op: V3::MulLo, dst: 1, src: [V(0), S(12), Int(0)] },
        Inst::V2 { op: V2::Xor, dst: 1, a: S(0), b: 1 },
        Inst::V3 { op: V3::MulLo, dst: 2, src: [V(1), S(13), Int(0)] },
        Inst::V2 { op: V2::AddNc, dst: 2, a: S(1), b: 2 },
        Inst::V3 { op: V3::MulLo, dst: 3, src: [V(2), S(12), Int(0)] },
        Inst::V2 { op: V2::Xor, dst: 3, a: S(2), b: 3 },
        Inst::V3 { op: V3::MulLo, dst: 4, src: [V(3), S(13), Int(0)] },
        Inst::V2 { op: V2::AddNc, dst: 4, a: S(3), b: 4 },
        Inst::V1 { op: V1::Mov, dst: 5, a: V(0) },
        Inst::V2 { op: V2::Xor, dst: 6, a: V(1), b: 4 },
        Inst::V1 { op: V1::Mov, dst: 7, a: Int(-1) },
        Inst::V1 { op: V1::Mov, dst: 8, a: Lit(0x8000_0000) },
        Inst::V2 { op: V2::Lshrrev, dst: 9, a: Int(7), b: 2 },
        Inst::V2 { op: V2::And, dst: 10, a: Lit(0xFF), b: 3 },
        Inst::V2 { op: V2::Ashrrev, dst: 11, a: Int(13), b: 4 },
        Inst::V2 { op: V2::MulU24, dst: 12, a: V(1), b: 2 },
        Inst::V1 { op: V1::Mov, dst: 13, a: Int(1) },
    ];
    let scalars = [S(0), S(1), S(2), S(3), Int(0), Int(-1), Lit(0x8000_0000), Int(31), Int(32), Int(7), Lit(0x7FFF_FFFF), Int(1)];
    for (k, a) in scalars.into_iter().enumerate() {
        p.push(Inst::S1 { op: S1::Mov, dst: 16 + k as u8, a });
    }
    p.push(Inst::Cmp { cmp: Cmp::Gt, signed: false, a: V(1), b: 2 });
    p.push(Inst::SCmp { cmp: Cmp::Lt, signed: false, a: S(0), b: S(1) });
    p
}

// ---------------------------------------------------------------------------
// Generator
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u32) -> u32 {
        (self.next() % u64::from(n.max(1))) as u32
    }
    fn chance(&mut self, percent: u32) -> bool {
        self.below(100) < percent
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len() as u32) as usize]
    }
    fn word(&mut self) -> u32 {
        if self.chance(25) {
            self.pick(&[0, 1, 2, 31, 32, 33, u32::MAX, 0x8000_0000, 0x7FFF_FFFF, 0xFFFF, 0x00FF_FFFF, 0xFF80_0000])
        } else {
            self.next() as u32
        }
    }
    fn int(&mut self) -> i8 {
        if self.chance(30) { -(self.below(16) as i8) - 1 } else { self.below(65) as i8 }
    }
    fn vgpr_read(&mut self) -> u8 {
        self.below(13) as u8
    }
    fn vgpr_write(&mut self) -> u8 {
        1 + self.below(12) as u8
    }
    fn sgpr_read(&mut self) -> u8 {
        if self.chance(20) { self.below(4) as u8 } else { 16 + self.below(12) as u8 }
    }
    fn sgpr_write(&mut self) -> u8 {
        16 + self.below(12) as u8
    }
    /// A VALU operand: a VGPR mostly, else an SGPR, an inline integer or a literal.
    fn vsrc(&mut self, literal: bool) -> Src {
        match self.below(100) {
            0..=54 => Src::V(self.vgpr_read()),
            55..=74 => Src::S(self.sgpr_read()),
            75..=91 => Src::Int(self.int()),
            _ if literal => Src::Lit(self.word()),
            _ => Src::Int(self.int()),
        }
    }
    fn ssrc(&mut self, literal: bool) -> Src {
        match self.below(100) {
            0..=59 => Src::S(self.sgpr_read()),
            60..=84 => Src::Int(self.int()),
            _ if literal => Src::Lit(self.word()),
            _ => Src::Int(self.int()),
        }
    }
}

/// What the generator may produce here.
#[derive(Clone, Copy)]
struct Ctx {
    avoid: Avoid,
    lanes: u32,
    /// Inside a lane-divergent branch.
    lane_branch: bool,
    in_loop: bool,
}

fn lane_select(r: &mut Rng, cx: Ctx) -> Src {
    if cx.avoid.lane_range {
        Src::Int(r.below(cx.lanes) as i8)
    } else if r.chance(50) {
        Src::S(r.sgpr_read())
    } else {
        Src::Int(r.below(64) as i8)
    }
}

fn gen_valu(r: &mut Rng, cx: Ctx) -> Inst {
    let no_scalar_writes = cx.avoid.salu_in_branch && cx.lane_branch;
    let mut k = r.below(100);
    if no_scalar_writes && (86..=94).contains(&k) || cx.avoid.writelane_inactive && cx.lane_branch && k >= 95 {
        k = r.below(70);
    }
    match k {
        0..=29 => Inst::V2 { op: r.pick(&V2S), dst: r.vgpr_write(), a: r.vsrc(true), b: r.vgpr_read() },
        30..=39 => Inst::V1 { op: r.pick(&V1S), dst: r.vgpr_write(), a: r.vsrc(true) },
        40..=69 => {
            let op = r.pick(&V3S);
            let mut src = [r.vsrc(false), r.vsrc(false), r.vsrc(false)];
            // At most two scalar values on the constant bus (GFX10).
            let mut scalars: Vec<Src> = Vec::new();
            for s in src.iter_mut() {
                if let Src::S(_) = s
                    && !scalars.contains(s)
                {
                    if scalars.len() == 2 {
                        *s = Src::V(r.vgpr_read());
                        continue;
                    }
                    scalars.push(*s);
                }
            }
            if op.arity() == 2 {
                src[2] = Src::Int(0);
            }
            Inst::V3 { op, dst: r.vgpr_write(), src }
        }
        70..=77 => Inst::Cmp { cmp: r.pick(&CMPS), signed: r.chance(50), a: r.vsrc(true), b: r.vgpr_read() },
        78..=85 => Inst::Cnd { dst: r.vgpr_write(), a: r.vsrc(true), b: r.vgpr_read() },
        86..=89 => Inst::FirstLane { dst: r.sgpr_write(), v: r.vgpr_read() },
        90..=94 => Inst::ReadLane { dst: r.sgpr_write(), v: r.vgpr_read(), lane: lane_select(r, cx) },
        _ => {
            let lane = lane_select(r, cx);
            let val = if r.chance(60) { Src::S(r.sgpr_read()) } else { Src::Int(r.int()) };
            Inst::WriteLane { dst: r.vgpr_write(), val, lane }
        }
    }
}

fn gen_salu(r: &mut Rng, cx: Ctx) -> Inst {
    match r.below(100) {
        0..=59 => {
            let a = r.ssrc(true);
            let b = r.ssrc(!matches!(a, Src::Lit(_)));
            let mut op = r.pick(&S2S);
            while cx.avoid.minmax_scc && matches!(op, S2::MinI | S2::MinU | S2::MaxI | S2::MaxU) {
                op = r.pick(&S2S);
            }
            Inst::S2 { op, dst: r.sgpr_write(), a, b }
        }
        60..=79 => Inst::S1 { op: r.pick(&S1S), dst: r.sgpr_write(), a: r.ssrc(true) },
        _ => {
            let a = r.ssrc(true);
            let b = r.ssrc(!matches!(a, Src::Lit(_)));
            Inst::SCmp { cmp: r.pick(&CMPS), signed: r.chance(50), a, b }
        }
    }
}

fn gen_items(r: &mut Rng, depth: usize, budget: &mut u32, cx: Ctx) -> Vec<Item> {
    let n = 1 + r.below(if depth == 0 { 16 } else { 6 });
    let scalar_ok = !(cx.avoid.salu_in_branch && cx.lane_branch);
    let mut out = Vec::new();
    for _ in 0..n {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        let k = r.below(100);
        let item = if k < 58 {
            Item::I(gen_valu(r, cx))
        } else if k < 75 && scalar_ok {
            Item::I(gen_salu(r, cx))
        } else if (75..87).contains(&k) && depth < 2 {
            let inner = Ctx { lane_branch: true, ..cx };
            let then = gen_items(r, depth + 1, budget, inner);
            let els = if r.chance(45) { Some(gen_items(r, depth + 1, budget, inner)) } else { None };
            Item::If { cmp: r.pick(&CMPS), signed: r.chance(50), a: r.vsrc(true), b: r.vgpr_read(), then, els }
        } else if (87..94).contains(&k) && depth < 2 && scalar_ok {
            let body = gen_items(r, depth + 1, budget, cx);
            Item::UIf { cmp: r.pick(&CMPS), signed: r.chance(50), a: r.sgpr_read(), b: r.ssrc(true), body }
        } else if k >= 94 && depth < 2 && !cx.in_loop && scalar_ok {
            let body = gen_items(r, depth + 1, budget, Ctx { in_loop: true, ..cx });
            Item::Loop { n: 1 + r.below(3) as u8, body }
        } else {
            Item::I(gen_valu(r, cx))
        };
        out.push(item);
    }
    out
}

fn gen_prog(r: &mut Rng, wave64: bool, avoid: Avoid) -> Prog {
    let seeds = [r.word(), r.word(), r.word(), r.word()];
    let mut budget = 10 + r.below(30);
    let cx = Ctx { avoid, lanes: if wave64 { 64 } else { 32 }, lane_branch: false, in_loop: false };
    Prog { wave64, seeds, body: gen_items(r, 0, &mut budget, cx), raw_masks: !avoid.mask_data }
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

fn int_code(n: i8) -> u32 {
    if n >= 0 { 128 + n as u32 } else { (192 - i32::from(n)) as u32 }
}

/// A 9-bit VALU operand code and its literal, if any.
fn vcode(s: Src) -> (u32, Option<u32>) {
    match s {
        Src::V(n) => (256 + u32::from(n), None),
        Src::S(n) => (u32::from(n), None),
        Src::Int(n) => (int_code(n), None),
        Src::Lit(x) => (255, Some(x)),
    }
}

/// An 8-bit SALU operand code and its literal, if any.
fn scode(s: Src) -> (u32, Option<u32>) {
    match s {
        Src::V(_) => panic!("a VGPR in a scalar operand"),
        other => vcode(other),
    }
}

#[derive(Default)]
struct Asm {
    words: Vec<u32>,
    names: Vec<&'static str>,
    fixups: Vec<(usize, usize)>,
    labels: Vec<usize>,
}

impl Asm {
    fn put(&mut self, name: &'static str, words: &[u32], literal: Option<u32>) {
        self.names.push(name);
        self.words.extend_from_slice(words);
        self.words.extend(literal);
    }
    fn label(&mut self) -> usize {
        self.labels.push(usize::MAX);
        self.labels.len() - 1
    }
    fn bind(&mut self, l: usize) {
        self.labels[l] = self.words.len();
    }
    fn branch(&mut self, name: &'static str, op: u32, l: usize) {
        self.fixups.push((self.words.len(), l));
        self.put(name, &[0xBF80_0000 | op << 16], None);
    }
    fn sop1(&mut self, name: &'static str, op: u32, dst: u8, a: Src) {
        let (a, lit) = scode(a);
        self.put(name, &[0xBE80_0000 | u32::from(dst) << 16 | op << 8 | a], lit);
    }
    fn sop2(&mut self, name: &'static str, op: u32, dst: u8, a: Src, b: Src) {
        let (a, la) = scode(a);
        let (b, lb) = scode(b);
        assert!(la.is_none() || lb.is_none(), "two literals");
        self.put(name, &[0x8000_0000 | op << 23 | u32::from(dst) << 16 | b << 8 | a], la.or(lb));
    }
    fn sopc(&mut self, name: &'static str, op: u32, a: Src, b: Src) {
        let (a, la) = scode(a);
        let (b, lb) = scode(b);
        assert!(la.is_none() || lb.is_none(), "two literals");
        self.put(name, &[0xBF00_0000 | op << 16 | b << 8 | a], la.or(lb));
    }
    fn vop1(&mut self, name: &'static str, op: u32, dst: u8, a: Src) {
        let (a, lit) = vcode(a);
        self.put(name, &[0x7E00_0000 | u32::from(dst) << 17 | op << 9 | a], lit);
    }
    fn vop2(&mut self, name: &'static str, op: u32, dst: u8, a: Src, b: u8) {
        let (a, lit) = vcode(a);
        self.put(name, &[op << 25 | u32::from(dst) << 17 | u32::from(b) << 9 | a], lit);
    }
    fn vopc(&mut self, name: &'static str, op: u32, a: Src, b: u8) {
        let (a, lit) = vcode(a);
        self.put(name, &[0x7C00_0000 | op << 17 | u32::from(b) << 9 | a], lit);
    }
    fn vop3(&mut self, name: &'static str, op: u32, dst: u8, src: [Src; 3]) {
        let [a, b, c] = src.map(|s| {
            let (code, lit) = vcode(s);
            assert!(lit.is_none(), "a literal in VOP3");
            code
        });
        self.put(name, &[0xD400_0000 | op << 16 | u32::from(dst), c << 18 | b << 9 | a], None);
    }
    fn finish(mut self) -> (Vec<u32>, Vec<&'static str>) {
        for &(at, l) in &self.fixups {
            let target = self.labels[l];
            assert_ne!(target, usize::MAX, "unbound label");
            let rel = (target as i64 - at as i64 - 1) as i16 as u16;
            self.words[at] |= u32::from(rel);
        }
        (self.words, self.names)
    }
}

fn encode_inst(a: &mut Asm, i: &Inst) {
    match *i {
        Inst::V1 { op, dst, a: s } => a.vop1(op.name(), op.op(), dst, s),
        Inst::V2 { op, dst, a: s, b } => a.vop2(op.name(), op.op(), dst, s, b),
        Inst::Cnd { dst, a: s, b } => a.vop2("v_cndmask_b32", 0x01, dst, s, b),
        Inst::V3 { op, dst, src } => a.vop3(op.name(), op.op(), dst, src),
        Inst::Cmp { cmp, signed, a: s, b } => a.vopc(cmp.v_name(signed), cmp.vopc(signed), s, b),
        Inst::FirstLane { dst, v } => a.vop1("v_readfirstlane_b32", 0x02, dst, Src::V(v)),
        Inst::ReadLane { dst, v, lane } => a.vop3("v_readlane_b32", 0x360, dst, [Src::V(v), lane, Src::Int(0)]),
        Inst::WriteLane { dst, val, lane } => a.vop3("v_writelane_b32", 0x361, dst, [val, lane, Src::Int(0)]),
        Inst::S1 { op, dst, a: s } => a.sop1(op.name(), op.op(), dst, s),
        Inst::S2 { op, dst, a: s, b } => a.sop2(op.name(), op.op(), dst, s, b),
        Inst::SCmp { cmp, signed, a: s, b } => a.sopc(cmp.s_name(signed), cmp.sopc(signed), s, b),
    }
}

fn encode_items(a: &mut Asm, items: &[Item], depth: usize, wave64: bool) {
    for item in items {
        match item {
            Item::I(i) => encode_inst(a, i),
            Item::If { cmp, signed, a: x, b, then, els } => {
                encode_inst(a, &Inst::Cmp { cmp: *cmp, signed: *signed, a: *x, b: *b });
                let (save, cond) = (save_reg(depth), cond_reg(depth));
                if els.is_some() {
                    if wave64 {
                        a.sop1("s_mov_b64", 0x04, cond, Src::S(VCC_LO));
                    } else {
                        a.sop1("s_mov_b32", 0x03, cond, Src::S(VCC_LO));
                    }
                }
                if wave64 {
                    a.sop1("s_and_saveexec_b64", 0x24, save, Src::S(VCC_LO));
                } else {
                    a.sop1("s_and_saveexec_b32", 0x3c, save, Src::S(VCC_LO));
                }
                let after_then = a.label();
                a.branch("s_cbranch_execz", 0x08, after_then);
                encode_items(a, then, depth + 1, wave64);
                a.bind(after_then);
                if let Some(els) = els {
                    if wave64 {
                        a.sop2("s_andn2_b64", 0x15, EXEC_LO, Src::S(save), Src::S(cond));
                    } else {
                        a.sop2("s_andn2_b32", 0x14, EXEC_LO, Src::S(save), Src::S(cond));
                    }
                    let end = a.label();
                    a.branch("s_cbranch_execz", 0x08, end);
                    encode_items(a, els, depth + 1, wave64);
                    a.bind(end);
                }
                if wave64 {
                    a.sop1("s_mov_b64", 0x04, EXEC_LO, Src::S(save));
                } else {
                    a.sop1("s_mov_b32", 0x03, EXEC_LO, Src::S(save));
                }
            }
            Item::UIf { cmp, signed, a: x, b, body } => {
                encode_inst(a, &Inst::SCmp { cmp: *cmp, signed: *signed, a: Src::S(*x), b: *b });
                let end = a.label();
                a.branch("s_cbranch_scc0", 0x04, end);
                encode_items(a, body, depth + 1, wave64);
                a.bind(end);
            }
            Item::Loop { n, body } => {
                let counter = loop_reg(depth);
                a.sop1("s_mov_b32", 0x03, counter, Src::Int(*n as i8));
                let top = a.label();
                a.bind(top);
                encode_items(a, body, depth + 1, wave64);
                a.sop2("s_sub_u32", 0x01, counter, Src::S(counter), Src::Int(1));
                a.sopc("s_cmp_lg_u32", 0x07, Src::S(counter), Src::Int(0));
                a.branch("s_cbranch_scc1", 0x05, top);
            }
        }
    }
}

fn store(a: &mut Asm, vdata: u8, slot: usize) {
    // buffer_store_dword vdata, v14, s[8:11], 0 offen offset:slot*4
    a.put(
        "buffer_store_dword",
        &[0x38 << 26 | 0x1C << 18 | 1 << 12 | (slot as u32 * 4), 0x80 << 24 | 2 << 16 | u32::from(vdata) << 8 | 14],
        None,
    );
}

fn encode(p: &Prog) -> (Vec<u32>, Vec<&'static str>) {
    let mut a = Asm::default();
    a.sop1("s_mov_b32", 0x03, 5, Src::Int(0));
    a.put("s_load_dwordx4", &[0x3D << 26 | 0x02 << 18 | 8 << 6 | 2, 125 << 25], None);
    for i in prologue() {
        encode_inst(&mut a, &i);
    }
    encode_items(&mut a, &p.body, 0, p.wave64);
    for k in 0..12u8 {
        store(&mut a, 1 + k, usize::from(k));
    }
    for k in 0..12u8 {
        a.vop1("v_mov_b32", 0x01, 15, Src::S(16 + k));
        store(&mut a, 15, 12 + usize::from(k));
    }
    // Each lane's VCC bit (no mask read as data), then VCC as numbers if asked.
    a.vop2("v_cndmask_b32", 0x01, 15, Src::Int(0), 13);
    store(&mut a, 15, 24);
    if p.raw_masks {
        a.vop1("v_mov_b32", 0x01, 15, Src::S(VCC_LO));
        store(&mut a, 15, 25);
        if p.wave64 {
            a.vop1("v_mov_b32", 0x01, 15, Src::S(VCC_HI));
            store(&mut a, 15, 26);
        }
    }
    a.sop2("s_cselect_b32", 0x0a, 29, Src::Int(1), Src::Int(0));
    a.vop1("v_mov_b32", 0x01, 15, Src::S(29));
    store(&mut a, 15, 27);
    a.put("s_endpgm", &[0xBF81_0000], None);
    a.finish()
}

// ---------------------------------------------------------------------------
// Interpreter
// ---------------------------------------------------------------------------

struct Machine {
    lanes: usize,
    exec: u64,
    vcc: u64,
    scc: bool,
    /// SCC came from `s_min`/`s_max` of equal operands, which the ISA documents
    /// differently (RDNA2: "S0 is chosen", RDNA3: `S0 >= S1`): not checked.
    scc_unknown: bool,
    /// Such an SCC was read (`s_cselect`): the program's result is not checked.
    ambiguous: bool,
    s: [u32; 128],
    v: Vec<[u32; 64]>,
}

/// What the oracle expects a program to store.
struct Expected {
    out: Vec<u32>,
    ambiguous: bool,
    scc_known: bool,
}

impl Machine {
    fn new(p: &Prog) -> Machine {
        let lanes = if p.wave64 { 64 } else { 32 };
        let mut m = Machine {
            lanes,
            exec: if lanes == 64 { u64::MAX } else { (1 << lanes) - 1 },
            vcc: 0,
            scc: false,
            scc_unknown: false,
            ambiguous: false,
            s: [0; 128],
            v: vec![[0; 64]; 16],
        };
        m.s[..4].copy_from_slice(&p.seeds);
        for lane in 0..lanes {
            m.v[0][lane] = lane as u32;
        }
        m
    }

    fn full(&self) -> u64 {
        if self.lanes == 64 { u64::MAX } else { (1 << self.lanes) - 1 }
    }

    fn read(&self, s: Src, lane: usize) -> u32 {
        match s {
            Src::V(n) => self.v[usize::from(n)][lane],
            Src::S(VCC_LO) => self.vcc as u32,
            Src::S(VCC_HI) => (self.vcc >> 32) as u32,
            Src::S(n) => self.s[usize::from(n)],
            Src::Int(n) => i32::from(n) as u32,
            Src::Lit(x) => x,
        }
    }

    fn active(&self, lane: usize) -> bool {
        self.exec >> lane & 1 != 0
    }

    fn valu(&mut self, dst: u8, f: impl Fn(&Machine, usize) -> u32) {
        let vals: Vec<(usize, u32)> = (0..self.lanes).filter(|&l| self.active(l)).map(|l| (l, f(self, l))).collect();
        for (l, x) in vals {
            self.v[usize::from(dst)][l] = x;
        }
    }

    fn scalar_result(&mut self, dst: u8, r: (u32, Option<bool>)) {
        self.s[usize::from(dst)] = r.0;
        if let Some(scc) = r.1 {
            self.set_scc(scc);
        }
    }

    fn set_scc(&mut self, scc: bool) {
        self.scc = scc;
        self.scc_unknown = false;
    }

    fn inst(&mut self, i: &Inst) {
        match *i {
            Inst::V1 { op, dst, a } => self.valu(dst, |m, l| op.eval(m.read(a, l))),
            Inst::V2 { op, dst, a, b } => self.valu(dst, |m, l| op.eval(m.read(a, l), m.v[usize::from(b)][l])),
            Inst::Cnd { dst, a, b } => self.valu(dst, |m, l| {
                if m.vcc >> l & 1 != 0 { m.v[usize::from(b)][l] } else { m.read(a, l) }
            }),
            Inst::V3 { op, dst, src } => self.valu(dst, |m, l| {
                op.eval(m.read(src[0], l), m.read(src[1], l), m.read(src[2], l), l)
            }),
            Inst::Cmp { cmp, signed, a, b } => {
                let mut mask = 0u64;
                for l in 0..self.lanes {
                    if self.active(l) && cmp.eval(signed, self.read(a, l), self.v[usize::from(b)][l]) {
                        mask |= 1 << l;
                    }
                }
                self.vcc = if self.lanes == 64 { mask } else { (self.vcc & !0xFFFF_FFFF) | mask };
            }
            Inst::FirstLane { dst, v } => {
                let lane = if self.exec == 0 { 0 } else { self.exec.trailing_zeros() as usize };
                self.s[usize::from(dst)] = self.v[usize::from(v)][lane];
            }
            Inst::ReadLane { dst, v, lane } => {
                let sel = self.read(lane, 0) as usize & (self.lanes - 1);
                self.s[usize::from(dst)] = self.v[usize::from(v)][sel];
            }
            Inst::WriteLane { dst, val, lane } => {
                let sel = self.read(lane, 0) as usize & (self.lanes - 1);
                self.v[usize::from(dst)][sel] = self.read(val, 0);
            }
            Inst::S1 { op, dst, a } => {
                let r = op.eval(self.read(a, 0));
                self.scalar_result(dst, r);
            }
            Inst::S2 { op, dst, a, b } => {
                if op == S2::Cselect && self.scc_unknown {
                    self.ambiguous = true;
                }
                let (x, y) = (self.read(a, 0), self.read(b, 0));
                let r = op.eval(x, y, self.scc);
                self.scalar_result(dst, r);
                if x == y && matches!(op, S2::MinI | S2::MinU | S2::MaxI | S2::MaxU) {
                    self.scc_unknown = true;
                }
            }
            Inst::SCmp { cmp, signed, a, b } => {
                let scc = cmp.eval(signed, self.read(a, 0), self.read(b, 0));
                self.set_scc(scc);
            }
        }
    }

    fn run(&mut self, items: &[Item], depth: usize) {
        for item in items {
            match item {
                Item::I(i) => self.inst(i),
                Item::If { cmp, signed, a, b, then, els } => {
                    self.inst(&Inst::Cmp { cmp: *cmp, signed: *signed, a: *a, b: *b });
                    let cond = self.vcc & self.full();
                    let saved = self.exec;
                    self.s[usize::from(save_reg(depth))] = saved as u32;
                    self.exec = saved & cond;
                    self.set_scc(self.exec != 0);
                    if self.exec != 0 {
                        self.run(then, depth + 1);
                    }
                    if let Some(els) = els {
                        self.exec = saved & !cond & self.full();
                        self.set_scc(self.exec != 0);
                        if self.exec != 0 {
                            self.run(els, depth + 1);
                        }
                    }
                    self.exec = saved;
                }
                Item::UIf { cmp, signed, a, b, body } => {
                    self.inst(&Inst::SCmp { cmp: *cmp, signed: *signed, a: Src::S(*a), b: *b });
                    if self.scc {
                        self.run(body, depth + 1);
                    }
                }
                Item::Loop { n, body } => {
                    let counter = usize::from(loop_reg(depth));
                    self.s[counter] = u32::from(*n);
                    loop {
                        self.run(body, depth + 1);
                        self.s[counter] = self.s[counter].wrapping_sub(1);
                        self.set_scc(self.s[counter] != 0);
                        if !self.scc {
                            break;
                        }
                    }
                }
            }
        }
    }

    /// What the program stores: per lane, `SLOTS` dwords.
    fn output(p: &Prog) -> Expected {
        let mut m = Machine::new(p);
        for i in prologue() {
            m.inst(&i);
        }
        m.run(&p.body, 0);
        let mut out = vec![0u32; m.lanes * SLOTS];
        for l in 0..m.lanes {
            let o = &mut out[l * SLOTS..];
            for k in 0..12 {
                o[k] = m.v[1 + k][l];
                o[12 + k] = m.s[16 + k];
            }
            o[24] = (m.vcc >> l & 1) as u32;
            if p.raw_masks {
                o[25] = m.vcc as u32;
                if m.lanes == 64 {
                    o[26] = (m.vcc >> 32) as u32;
                }
            }
            o[27] = u32::from(m.scc);
        }
        Expected { out, ambiguous: m.ambiguous, scc_known: !m.scc_unknown }
    }
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

fn src_text(s: Src) -> String {
    match s {
        Src::V(n) => format!("v{n}"),
        Src::S(VCC_LO) => "vcc_lo".into(),
        Src::S(VCC_HI) => "vcc_hi".into(),
        Src::S(n) => format!("s{n}"),
        Src::Int(n) => format!("{n}"),
        Src::Lit(x) => format!("{x:#x}"),
    }
}

fn inst_text(i: &Inst) -> String {
    match *i {
        Inst::V1 { op, dst, a } => format!("{} v{dst}, {}", op.name(), src_text(a)),
        Inst::V2 { op, dst, a, b } => format!("{} v{dst}, {}, v{b}", op.name(), src_text(a)),
        Inst::Cnd { dst, a, b } => format!("v_cndmask_b32 v{dst}, {}, v{b}, vcc", src_text(a)),
        Inst::V3 { op, dst, src } => {
            let s: Vec<String> = src[..op.arity()].iter().map(|&x| src_text(x)).collect();
            format!("{} v{dst}, {}", op.name(), s.join(", "))
        }
        Inst::Cmp { cmp, signed, a, b } => format!("{} vcc, {}, v{b}", cmp.v_name(signed), src_text(a)),
        Inst::FirstLane { dst, v } => format!("v_readfirstlane_b32 s{dst}, v{v}"),
        Inst::ReadLane { dst, v, lane } => format!("v_readlane_b32 s{dst}, v{v}, {}", src_text(lane)),
        Inst::WriteLane { dst, val, lane } => format!("v_writelane_b32 v{dst}, {}, {}", src_text(val), src_text(lane)),
        Inst::S1 { op, dst, a } => format!("{} s{dst}, {}", op.name(), src_text(a)),
        Inst::S2 { op, dst, a, b } => format!("{} s{dst}, {}, {}", op.name(), src_text(a), src_text(b)),
        Inst::SCmp { cmp, signed, a, b } => format!("{} {}, {}", cmp.s_name(signed), src_text(a), src_text(b)),
    }
}

fn listing(items: &[Item], indent: usize, out: &mut String) {
    let pad = "  ".repeat(indent);
    for item in items {
        match item {
            Item::I(i) => out.push_str(&format!("{pad}{}\n", inst_text(i))),
            Item::If { cmp, signed, a, b, then, els } => {
                out.push_str(&format!(
                    "{pad}if lane ({} {}{} v{b}) {{\n",
                    src_text(*a),
                    cmp.suffix(),
                    if *signed { " signed" } else { "" }
                ));
                listing(then, indent + 1, out);
                if let Some(els) = els {
                    out.push_str(&format!("{pad}}} else {{\n"));
                    listing(els, indent + 1, out);
                }
                out.push_str(&format!("{pad}}}\n"));
            }
            Item::UIf { cmp, signed, a, b, body } => {
                out.push_str(&format!(
                    "{pad}if uniform (s{a} {}{} {}) {{\n",
                    cmp.suffix(),
                    if *signed { " signed" } else { "" },
                    src_text(*b)
                ));
                listing(body, indent + 1, out);
                out.push_str(&format!("{pad}}}\n"));
            }
            Item::Loop { n, body } => {
                out.push_str(&format!("{pad}repeat {n} {{\n"));
                listing(body, indent + 1, out);
                out.push_str(&format!("{pad}}}\n"));
            }
        }
    }
}

fn prog_text(p: &Prog) -> String {
    let mut s = format!(
        "wave{} seeds [{:#x}, {:#x}, {:#x}, {:#x}]\n",
        if p.wave64 { 64 } else { 32 },
        p.seeds[0],
        p.seeds[1],
        p.seeds[2],
        p.seeds[3]
    );
    listing(&p.body, 1, &mut s);
    s
}

// ---------------------------------------------------------------------------
// Translation and the GPU
// ---------------------------------------------------------------------------

fn native_of(p: &Prog, words: &[u32]) -> NativeProgram {
    NativeProgram {
        stage: HwStage::Cs,
        wave64: p.wave64,
        vgprs: 16,
        layout: vec![
            RootParam { root: RootType::Constants, table_offset_dwords: 0, sgprs: 4, base_register: 0, user_data: 0, slot: 0 },
            RootParam { root: RootType::Table, table_offset_dwords: 0, sgprs: 1, base_register: 0, user_data: 4, slot: 1 },
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
        io: StageIo::Cs(ComputeIo { rsrc2: 5 << 1, threads: [if p.wave64 { 64 } else { 32 }, 1, 1] }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// How the production path translates `p` on a host of 32-lane waves.
fn translate(p: &Prog) -> Result<HostShader, ShaderError> {
    let (words, _) = encode(p);
    let native = native_of(p, &words);
    let container = dxbc::build(&[]);
    let c = dxbc::Container::parse(&container)?;
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let r = (|| {
        let ir = spill::build(&native)?;
        let st = structure::structure(&ir);
        let res = resource::track(&ir, &native.layout, native.stage, st.as_ref().ok())?;
        let io = io::raise(&ir, &native)?;
        let st = st?;
        // as `super::translate` does after the earlier stages
        match super::emit(&c, &native, &ir, &st, &io, &res) {
            Err(ShaderError::Unsupported { what, .. }) if what == lower::FIRST_HALF => {
                lower::with_first_half_pairs(|| super::emit(&c, &native, &ir, &st, &io, &res))
            }
            other => other,
        }
    })();
    lower::TEST_HOST_WAVE.set(was);
    r
}

struct Gpu {
    host: Host,
}

impl Gpu {
    fn run(&self, shader: &HostShader, p: &Prog) -> Result<Vec<u32>, String> {
        let lanes = if p.wave64 { 64 } else { 32 };
        let (root, pso) = self.host.compute_pipeline(shader)?;
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            ud[u.index as usize] = match (u.root, u.slot) {
                (RootType::Constants, 0) => p.seeds[u.dword as usize],
                _ => 0,
            };
        }
        let out = self.host.dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav((lanes * SLOTS) as u32)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )?;
        Ok(out[0].clone())
    }
}

const SLOT_NAMES: [&str; OUT_USED] = [
    "v1", "v2", "v3", "v4", "v5", "v6", "v7", "v8", "v9", "v10", "v11", "v12", "s16", "s17", "s18", "s19", "s20", "s21",
    "s22", "s23", "s24", "s25", "s26", "s27", "vcc_bit", "vcc_lo", "vcc_hi", "scc",
];

/// The (lane, slot, gpu, oracle) differences; SCC left out where the oracle does not know it.
fn differences(gpu: &[u32], want: &Expected, lanes: usize) -> Vec<(usize, usize, u32, u32)> {
    let mut d = Vec::new();
    for l in 0..lanes {
        for k in 0..OUT_USED {
            if k == 27 && !want.scc_known {
                continue;
            }
            let (g, o) = (gpu[l * SLOTS + k], want.out[l * SLOTS + k]);
            if g != o {
                d.push((l, k, g, o));
            }
        }
    }
    d
}

#[derive(Debug)]
enum Outcome {
    Same,
    Refused(String),
    PipelineFailed(String),
    /// An SCC the ISA documents ambiguously decided the result.
    Ambiguous,
    Differs(Vec<(usize, usize, u32, u32)>),
}

/// The reason of a refusal without the code offset.
fn reason(e: &ShaderError) -> String {
    match e {
        ShaderError::Unsupported { what, .. } => format!("{what} is not supported"),
        ShaderError::Isa { reason, .. } => (*reason).to_string(),
        other => other.to_string(),
    }
}

fn check(gpu: &Gpu, p: &Prog) -> Outcome {
    let want = Machine::output(p);
    if want.ambiguous {
        return Outcome::Ambiguous;
    }
    let shader = match translate(p) {
        Ok(s) => s,
        Err(e) => return Outcome::Refused(reason(&e)),
    };
    let got = match gpu.run(&shader, p) {
        Ok(g) => g,
        Err(e) => return Outcome::PipelineFailed(e),
    };
    let lanes = if p.wave64 { 64 } else { 32 };
    let d = differences(&got, &want, lanes);
    if d.is_empty() { Outcome::Same } else { Outcome::Differs(d) }
}

// ---------------------------------------------------------------------------
// Minimizing
// ---------------------------------------------------------------------------

/// Every way to make `items` one step smaller: an item left out, a branch or loop
/// replaced by its body, an else left out.
fn smaller(items: &[Item]) -> Vec<Vec<Item>> {
    let mut out = Vec::new();
    for k in 0..items.len() {
        let mut without = items.to_vec();
        without.remove(k);
        out.push(without);
        let mut flat = |body: &[Item]| {
            let mut v = items[..k].to_vec();
            v.extend_from_slice(body);
            v.extend_from_slice(&items[k + 1..]);
            out.push(v);
        };
        match &items[k] {
            Item::If { then, els, .. } => {
                flat(then);
                if let Some(e) = els {
                    flat(e);
                }
            }
            Item::UIf { body, .. } | Item::Loop { body, .. } => flat(body),
            Item::I(_) => {}
        }
        let nested: Vec<(Vec<Item>, fn(&Item, Vec<Item>) -> Item)> = match &items[k] {
            Item::If { then, .. } => vec![(then.clone(), |i, b| match i {
                Item::If { cmp, signed, a, b: vb, els, .. } => {
                    Item::If { cmp: *cmp, signed: *signed, a: *a, b: *vb, then: b, els: els.clone() }
                }
                _ => unreachable!(),
            })],
            Item::UIf { body, .. } => vec![(body.clone(), |i, b| match i {
                Item::UIf { cmp, signed, a, b: sb, .. } => Item::UIf { cmp: *cmp, signed: *signed, a: *a, b: *sb, body: b },
                _ => unreachable!(),
            })],
            Item::Loop { body, .. } => vec![(body.clone(), |i, b| match i {
                Item::Loop { n, .. } => Item::Loop { n: *n, body: b },
                _ => unreachable!(),
            })],
            Item::I(_) => vec![],
        };
        for (body, rebuild) in nested {
            for smaller_body in smaller(&body) {
                let mut v = items.to_vec();
                v[k] = rebuild(&items[k], smaller_body);
                out.push(v);
            }
        }
        if let Item::If { cmp, signed, a, b, then, els: Some(els) } = &items[k] {
            let mut v = items.to_vec();
            v[k] = Item::If { cmp: *cmp, signed: *signed, a: *a, b: *b, then: then.clone(), els: None };
            out.push(v);
            for smaller_else in smaller(els) {
                let mut v = items.to_vec();
                v[k] = Item::If { cmp: *cmp, signed: *signed, a: *a, b: *b, then: then.clone(), els: Some(smaller_else) };
                out.push(v);
            }
        }
    }
    out
}

fn minimize(gpu: &Gpu, p: &Prog, budget: usize) -> Prog {
    let mut best = p.clone();
    let mut tries = 0;
    'outer: loop {
        for body in smaller(&best.body) {
            if tries >= budget {
                break 'outer;
            }
            tries += 1;
            let candidate = Prog { body, ..best.clone() };
            if matches!(check(gpu, &candidate), Outcome::Differs(_)) {
                best = candidate;
                continue 'outer;
            }
        }
        break;
    }
    best
}

fn describe(d: &[(usize, usize, u32, u32)]) -> String {
    let mut slots: Vec<usize> = d.iter().map(|x| x.1).collect();
    slots.sort_unstable();
    slots.dedup();
    let lanes: std::collections::BTreeSet<usize> = d.iter().map(|x| x.0).collect();
    let mut s = format!(
        "{} differences in {} lanes, registers {:?}\n",
        d.len(),
        lanes.len(),
        slots.iter().map(|&k| SLOT_NAMES[k]).collect::<Vec<_>>()
    );
    for &(l, k, g, o) in d.iter().take(6) {
        s.push_str(&format!("    lane {l:2} {:6}: gpu {g:#010x}  oracle {o:#010x}\n", SLOT_NAMES[k]));
    }
    s
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The encoder writes what the decoder (Mesa's ACO names) reads back, instruction
/// for instruction, for many random programs of both wave sizes.
#[test]
fn oracle_encoding_reads_back_as_the_instructions_meant() {
    let mut r = Rng(0x5EED);
    for n in 0..400 {
        let avoid = Avoid { mask_data: n % 4 >= 2, ..Avoid::default() };
        let p = gen_prog(&mut r, n % 2 == 1, avoid);
        let (words, names) = encode(&p);
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let decoded = gfx10::decode(&bytes).expect("decode");
        let got: Vec<&str> = decoded.iter().map(|i| i.name().unwrap_or("?")).collect();
        assert_eq!(got, names, "program {n}:\n{}", prog_text(&p));
    }
}

/// Hand-checked interpreter results: EXEC under if/else, VCC of inactive lanes,
/// SCC of the scalar operations, cross-lane reads.
#[test]
fn oracle_interpreter_follows_the_isa_on_known_cases() {
    use Src::*;
    // Lanes below 5 take the then-branch (v1 = 1), the others the else (v1 = 2).
    let p = Prog {
        wave64: false,
        seeds: [5, 0, 0, 0],
        raw_masks: true,
        body: vec![Item::If {
            cmp: Cmp::Lt,
            signed: false,
            a: V(0),
            b: 15, // v15 is only written by the stores after the body: 0
            then: vec![],
            els: None,
        }],
    };
    let _ = Machine::output(&p);
    let p = Prog {
        wave64: false,
        seeds: [5, 0, 0, 0],
        raw_masks: true,
        body: vec![
            Item::I(Inst::V1 { op: V1::Mov, dst: 3, a: S(0) }),
            Item::If {
                cmp: Cmp::Gt,
                signed: false,
                a: V(3),
                b: 5, // v5 = thread id: 5 > t for t < 5
                then: vec![Item::I(Inst::V1 { op: V1::Mov, dst: 1, a: Int(1) })],
                els: Some(vec![Item::I(Inst::V1 { op: V1::Mov, dst: 1, a: Int(2) })]),
            },
            Item::I(Inst::Cmp { cmp: Cmp::Eq, signed: false, a: Int(2), b: 1 }),
            Item::I(Inst::FirstLane { dst: 16, v: 1 }),
            Item::I(Inst::ReadLane { dst: 17, v: 5, lane: Int(35) }),
            Item::I(Inst::S2 { op: S2::MinU, dst: 18, a: Int(3), b: Int(9) }),
        ],
    };
    let out = Machine::output(&p).out;
    for l in 0..32 {
        assert_eq!(out[l * SLOTS], if l < 5 { 1 } else { 2 }, "v1 of lane {l}");
    }
    assert_eq!(out[25], !0x1F, "vcc: the lanes that took the else");
    assert_eq!((out[24], out[5 * SLOTS + 24]), (0, 1), "each lane's VCC bit");
    assert_eq!(out[12], 1, "s16: lane 0's v1");
    assert_eq!(out[13], 3, "s17: lane 35 & 31 = 3");
    assert_eq!(out[14], 3, "s18: min");
    assert_eq!(out[27], 1, "scc: 3 < 9");
    // VCC of inactive lanes is 0 after a compare inside a branch.
    let p = Prog {
        wave64: true,
        seeds: [0; 4],
        raw_masks: true,
        body: vec![Item::If {
            cmp: Cmp::Lt,
            signed: false,
            a: V(0),
            b: 15,
            then: vec![],
            els: None,
        }],
    };
    let out = Machine::output(&p).out;
    assert_eq!((out[25], out[26]), (0, 0), "no lane is below 0");
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// The fuzzer. Prints a summary and a minimized program for each kind of
/// difference; fails if an exact translation differs from the oracle.
#[test]
#[ignore = "differential fuzzing on the GPU: VICTORY_ORACLE_RUNS, VICTORY_ORACLE_SEED"]
fn oracle_fuzz() {
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let host = match Host::new(false) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("no SM 6.6 hardware device ({e}); skipped");
            return;
        }
    };
    let gpu = Gpu { host };
    let runs = env_u64("VICTORY_ORACLE_RUNS", 200);
    let seed = env_u64("VICTORY_ORACLE_SEED", 1);
    let avoid = Avoid::from_env();
    eprintln!("avoiding {avoid:?}");
    let mut r = Rng(seed);
    let mut report = String::new();
    let mut exact_failures = 0usize;
    for wave64 in [false, true] {
        let mut counts = std::collections::BTreeMap::<String, usize>::new();
        let mut refusals = std::collections::BTreeMap::<String, usize>::new();
        let mut seen_kinds = std::collections::BTreeSet::<String>::new();
        for n in 0..runs {
            let p = gen_prog(&mut r, wave64, avoid);
            // Wave64: exact forms only first; then the production default.
            let modes: &[(&str, Option<&str>)] =
                if wave64 { &[("exact", Some("idiom")), ("default", None)] } else { &[("default", None)] };
            let mut exact_refused = false;
            for &(mode, local) in modes {
                // SAFETY: this test holds the GPU lock and runs with one test thread.
                unsafe {
                    match local {
                        Some(v) => std::env::set_var("VICTORY_WAVE_LOCAL", v),
                        None => std::env::remove_var("VICTORY_WAVE_LOCAL"),
                    }
                }
                let outcome = check(&gpu, &p);
                let key = match &outcome {
                    Outcome::Same => format!("{mode}: same"),
                    Outcome::Refused(why) => {
                        if mode == "exact" {
                            exact_refused = true;
                        }
                        *refusals.entry(format!("{mode}: {why}")).or_default() += 1;
                        format!("{mode}: refused")
                    }
                    Outcome::PipelineFailed(e) => format!("{mode}: pipeline failed: {e}"),
                    Outcome::Ambiguous => format!("{mode}: not checked (SCC of s_min/s_max with equal operands read)"),
                    Outcome::Differs(d) => {
                        let approximate = mode == "default" && exact_refused;
                        let label = if approximate { "default (approximate form)" } else { mode };
                        if !approximate {
                            exact_failures += 1;
                        }
                        let mut slots: Vec<&str> = d.iter().map(|x| SLOT_NAMES[x.1]).collect();
                        slots.sort_unstable();
                        slots.dedup();
                        let kind = format!("{label}: differs in {}", slots.join(","));
                        if seen_kinds.insert(kind.clone()) && seen_kinds.len() <= 12 {
                            let small = minimize(&gpu, &p, 400);
                            let detail = match check(&gpu, &small) {
                                Outcome::Differs(d) => describe(&d),
                                other => format!("{other:?}\n"),
                            };
                            report.push_str(&format!(
                                "--- wave{} program {n}, {label} ---\noriginal:\n{}minimized:\n{}{}",
                                if wave64 { 64 } else { 32 },
                                prog_text(&p),
                                prog_text(&small),
                                detail
                            ));
                        }
                        format!("{label}: DIFFERS")
                    }
                };
                *counts.entry(key).or_default() += 1;
            }
        }
        // SAFETY: as above.
        unsafe { std::env::remove_var("VICTORY_WAVE_LOCAL") };
        eprintln!("=== wave{} ({runs} programs, seed {seed}) ===", if wave64 { 64 } else { 32 });
        for (k, n) in &counts {
            eprintln!("  {n:5}  {k}");
        }
        for (k, n) in &refusals {
            eprintln!("  refused {n:5}x  {k}");
        }
    }
    eprintln!("{report}");
    if let Ok(path) = std::env::var("VICTORY_ORACLE_OUT") {
        let _ = std::fs::write(path, &report);
    }
    assert_eq!(exact_failures, 0, "exact translations that differ from the oracle (see above)");
}

/// A lane-divergent branch: the lanes whose v5 (the thread id) is above s0 (signed) take
/// `then`. [`by_hash`] turns it into one on v3 (a hash of the thread id, unsigned), as the
/// fuzzer's branches are.
fn lane_split(then: Vec<Item>, els: Option<Vec<Item>>) -> Item {
    Item::If { cmp: Cmp::Lt, signed: true, a: Src::S(0), b: 5, then, els }
}

/// `items` with every branch of [`lane_split`] on v3, unsigned.
fn by_hash(items: &[Item]) -> Vec<Item> {
    items
        .iter()
        .map(|i| match i {
            Item::If { cmp: Cmp::Lt, signed: true, a: Src::S(0), b: 5, then, els } => Item::If {
                cmp: Cmp::Lt,
                signed: false,
                a: Src::S(0),
                b: 3,
                then: by_hash(then),
                els: els.as_ref().map(|e| by_hash(e)),
            },
            Item::Loop { n, body } => Item::Loop { n: *n, body: by_hash(body) },
            other => other.clone(),
        })
        .collect()
}

/// VCC read: through `v_cndmask` into `v{lane_bit}`, and with `data` as numbers too, `s_mov` to
/// s20 (and s21 from vcc_hi in wave64), copied to `v{copy}` (and the next VGPR) where `copy` is
/// given (inside a lane branch, where only the lanes in it keep that copy).
fn read_vcc(lane_bit: u8, data: bool, wave64: bool, copy: Option<u8>) -> Vec<Item> {
    use Src::*;
    let mut out = vec![Item::I(Inst::Cnd { dst: lane_bit, a: Int(7), b: 1 })];
    if data {
        out.push(Item::I(Inst::S1 { op: S1::Mov, dst: 20, a: S(VCC_LO) }));
        if wave64 {
            out.push(Item::I(Inst::S1 { op: S1::Mov, dst: 21, a: S(VCC_HI) }));
        }
        if let Some(v) = copy {
            out.push(Item::I(Inst::V1 { op: V1::Mov, dst: v, a: S(20) }));
            if wave64 {
                out.push(Item::I(Inst::V1 { op: V1::Mov, dst: v + 1, a: S(21) }));
            }
        }
    }
    out
}

/// Class 5 of the oracle's findings: a compare (`v_cmp_*` writing VCC) that runs while a
/// lane-divergent branch narrows EXEC writes 0 for the lanes left out (RDNA2 ISA, VOPC), and
/// after the join every lane has that mask whenever some lane ran the compare. VCC is read
/// after the compare inside the branch and after it, through `v_cndmask` and, with masks as
/// data, as numbers (`s_mov`). A compare in then, in else, in a nested branch, in a uniform loop
/// inside the branch and in a branch inside a uniform loop; for all, some and no lanes, waves of
/// 32 and of 64 (two host waves of 32 with `VICTORY_WAVE_WIDE=1`).
#[test]
fn oracle_compares_in_lane_branches_zero_the_lanes_left_out() {
    use Src::*;
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; skipped");
        return;
    };
    let gpu = Gpu { host };
    // v4 < v6 (two hashes): about half of the lanes that run it.
    let cmp = || Item::I(Inst::Cmp { cmp: Cmp::Lt, signed: false, a: V(4), b: 6 });
    let shapes: Vec<(&str, fn(Item, Vec<Item>, Vec<Item>) -> Vec<Item>)> = vec![
        ("in then", |cmp, inside, after| [vec![lane_split([vec![cmp], inside].concat(), None)], after].concat()),
        ("in else", |cmp, inside, after| [vec![lane_split(vec![], Some([vec![cmp], inside].concat()))], after].concat()),
        ("nested", |cmp, inside, after| {
            // (the inner branch on v1 > v6: about half of the outer branch's lanes; VCC read
            // after the inner join too)
            let then = [vec![cmp], inside.clone()].concat();
            let inner = Item::If { cmp: Cmp::Gt, signed: false, a: V(1), b: 6, then, els: None };
            [vec![lane_split([vec![inner], inside].concat(), None)], after].concat()
        }),
        ("in a uniform loop in the branch", |cmp, inside, after| {
            let step = Item::I(Inst::V2 { op: V2::AddNc, dst: 4, a: Int(9), b: 4 });
            let body = [vec![cmp], inside, vec![step]].concat();
            [vec![lane_split(vec![Item::Loop { n: 2, body }], None)], after].concat()
        }),
        ("in a branch in a uniform loop", |cmp, inside, after| {
            let body = [vec![lane_split([vec![cmp], inside].concat(), None)], after.clone()].concat();
            [vec![Item::Loop { n: 2, body }], after].concat()
        }),
    ];
    let (mut failures, mut refused) = (Vec::new(), 0);
    for wave64 in [false, true] {
        // (on this thread only: other tests translate meanwhile)
        lower::THREAD_WAVE_WIDE.set(true);
        for (name, shape) in &shapes {
            // On the thread id, s0: -1 every lane, 9 some lanes, 40 some (wave64) or none
            // (wave32), 70 none. On the hash, s0: 0 nearly all, 2^31 about half, 2^32-1 none.
            let cases = [(false, u32::MAX), (false, 9), (false, 40), (false, 70), (true, 0), (true, 0x8000_0000), (true, u32::MAX)];
            // (VCC read as numbers too, in the program and stored, or only as each lane's bit)
            for ((hash, s0), raw_masks) in cases.into_iter().flat_map(|c| [(c, false), (c, true)]) {
                let (inside, after) = (read_vcc(9, raw_masks, wave64, Some(11)), read_vcc(10, raw_masks, wave64, None));
                let body = shape(cmp(), inside, after);
                let body = if hash { by_hash(&body) } else { body };
                // (SCC from a scalar compare at the end, which the program stores: without masks as
                // data, SCC of the branch's `s_and_saveexec` read as data is refused, another matter)
                let body = [body, vec![Item::I(Inst::SCmp { cmp: Cmp::Eq, signed: false, a: S(16), b: S(16) })]].concat();
                let name = format!("{name}{}{}", if hash { " (on a hash)" } else { "" }, if raw_masks { " (masks as data)" } else { "" });
                let p = Prog { wave64, seeds: [s0, 3, 0x1234, 0x5678], body, raw_masks };
                match check(&gpu, &p) {
                    Outcome::Same => {}
                    // (In a wave32 the nested shape with masks as data is refused, before the
                    // change and after it: VCC joined in the inner branch and read as data in the
                    // outer one is a lane mask holding lanes elsewhere where not every lane runs.
                    // This fork has no retry that lowers such a mask as the number it is. A
                    // refusal gives no wrong value; in wave64 the shape translates and passes.)
                    Outcome::Refused(why)
                        if !wave64 && raw_masks && name.starts_with("nested") && why.starts_with(lower::MASK_ELSEWHERE) =>
                    {
                        refused += 1;
                    }
                    other => failures.push(format!(
                        "wave{} {name}, s0 = {s0:#x}: {}",
                        if wave64 { 64 } else { 32 },
                        match other {
                            Outcome::Differs(d) => describe(&d),
                            o => format!("{o:?}"),
                        }
                    )),
                }
            }
        }
        lower::THREAD_WAVE_WIDE.set(false);
    }
    eprintln!("{refused} of 140 cases refused as before (wave32, nested, masks as data)");
    assert!(failures.is_empty(), "{} of 140 cases differ:\n{}", failures.len(), failures.join("\n"));
}

fn mov_s(dst: u8, x: i8) -> Item {
    Item::I(Inst::S1 { op: S1::Mov, dst, a: Src::Int(x) })
}

/// Class 1 of the oracle's findings: a scalar a lane-divergent branch writes is the wave's
/// after the join, as on the console (`lower::scalar_splits`). Every shape for all, some and
/// no lanes in the branch, waves of 32 and of 64 (two host waves of 32 with
/// `VICTORY_WAVE_WIDE=1`).
#[test]
fn oracle_scalars_written_in_lane_branches_reach_every_lane() {
    use Src::*;
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; skipped");
        return;
    };
    let gpu = Gpu { host };
    let shapes: Vec<(&str, Vec<Item>)> = vec![
        ("else writes", vec![lane_split(vec![], Some(vec![mov_s(21, -3)]))]),
        ("then writes", vec![lane_split(vec![Item::I(Inst::S2 { op: S2::SubU, dst: 20, a: S(17), b: S(16) })], None)]),
        ("both write", vec![lane_split(vec![mov_s(22, 7)], Some(vec![mov_s(22, 9)]))]),
        // (a copy of a value from before the branch: still a different value at the join)
        ("then copies an older scalar", vec![lane_split(vec![Item::I(Inst::S1 { op: S1::Mov, dst: 27, a: S(19) })], None)]),
        (
            "nested, read after the outer join",
            vec![lane_split(
                vec![],
                Some(vec![Item::If { cmp: Cmp::Gt, signed: false, a: S(1), b: 5, then: vec![mov_s(23, 5)], els: None }]),
            )],
        ),
        (
            "SCC set in the branch, read after it",
            vec![
                lane_split(vec![Item::I(Inst::SCmp { cmp: Cmp::Eq, signed: false, a: S(16), b: S(16) })], None),
                Item::I(Inst::S2 { op: S2::Cselect, dst: 24, a: Int(1), b: Int(2) }),
            ],
        ),
        ("first lane read in the branch", vec![lane_split(vec![Item::I(Inst::FirstLane { dst: 25, v: 5 })], None)]),
        ("lane read in the branch", vec![lane_split(vec![Item::I(Inst::ReadLane { dst: 25, v: 5, lane: Int(0) })], None)]),
        (
            "branch in a uniform loop",
            vec![Item::Loop {
                n: 2,
                body: vec![lane_split(vec![Item::I(Inst::S2 { op: S2::AddU, dst: 27, a: S(27), b: Int(1) })], None)],
            }],
        ),
    ];
    let mut failures = Vec::new();
    for wave64 in [false, true] {
        // (on this thread only: other tests translate meanwhile)
        lower::THREAD_WAVE_WIDE.set(true);
        for (name, body) in &shapes {
            // On the thread id, s0: -1 every lane, 9 some lanes, 40 some (wave64) or none
            // (wave32), 70 none. On the hash, s0: 0 nearly all, 2^31 about half, 2^32-1 none.
            let cases = [(false, u32::MAX), (false, 9), (false, 40), (false, 70), (true, 0), (true, 0x8000_0000), (true, u32::MAX)];
            // (VCC also stored as numbers: a mask read as data, which lowers branches as the
            // console's lanes take them instead of as guarded code)
            for ((hash, s0), raw_masks) in cases.into_iter().flat_map(|c| [(c, false), (c, true)]) {
                let body = if hash { by_hash(body) } else { body.clone() };
                let name = format!("{name}{}{}", if hash { " (on a hash)" } else { "" }, if raw_masks { " (masks as data)" } else { "" });
                let p = Prog { wave64, seeds: [s0, 3, 0x1234, 0x5678], body, raw_masks };
                match check(&gpu, &p) {
                    Outcome::Same => {}
                    other => failures.push(format!(
                        "wave{} {name}, s0 = {s0:#x}: {}",
                        if wave64 { 64 } else { 32 },
                        match other {
                            Outcome::Differs(d) => describe(&d),
                            o => format!("{o:?}"),
                        }
                    )),
                }
            }
        }
        lower::THREAD_WAVE_WIDE.set(false);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// `VICTORY_SCALAR_SPLITS=0` reaches every caller of `widen`, the fallbacks included, which
/// start from no branch: with the switch off a scalar written in a lane branch keeps the
/// per-lane form there, with it on that branch is taken wave-wide. (No GPU: translation only.)
#[test]
fn widen_takes_scalar_splits_only_when_asked() {
    let p = Prog {
        wave64: false,
        seeds: [9, 3, 0x1234, 0x5678],
        body: vec![lane_split(vec![mov_s(21, -3)], None)],
        raw_masks: false,
    };
    let (words, _) = encode(&p);
    let native = native_of(&p, &words);
    let container = dxbc::build(&[]);
    let c = dxbc::Container::parse(&container).unwrap();
    let ir = spill::build(&native).unwrap();
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let off = super::widen(&c, &native, &ir, None, false, false, false).map(|(s, _)| s.dxil);
    let on = super::widen(&c, &native, &ir, Some(&[]), false, false, false).map(|(s, _)| s.dxil);
    let plain = structure::structure(&ir).map(|st| lower::scalar_splits(&ir, &st));
    lower::TEST_HOST_WAVE.set(was);
    assert!(matches!(plain, Ok(Some(ref s)) if !s.is_empty()), "the program has a scalar split: {plain:?}");
    let on = on.expect("with the switch on, the branch is taken wave-wide and translates");
    // (with it off this program stays per lane, which `widen` alone refuses: a lane mask read as
    // data, retried later on the full path)
    assert!(!matches!(&off, Ok(d) if *d == on), "with the switch off the branch is not taken for its scalar: {off:?}");
}
