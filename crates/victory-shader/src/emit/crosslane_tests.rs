//! End-to-end RTX/D3D12 hardware tests of RDNA2 cross-lane instructions
//! (`DPP16`, `DPP8`/`DPP8FI`, and `ds_swizzle_b32`) under divergent `EXEC`
//! masks, verified against an exact CPU reference model of the AMD RDNA2
//! (GFX10.3) ISA.
//!
//! Covers all control-flow forms produced by [`crate::structure`]:
//! - straight-line `EXEC` narrowing (`v_cmpx_*` without a branch, lowered with
//!   a per-lane `guard`),
//! - branch-structurised divergence (`v_cmp_*` + `s_and_saveexec_b32` +
//!   `s_cbranch_execz`, lowered to a DXIL branch with `guard == None` inside
//!   the taken arm), and
//! - nested divergence (a DXIL branch further narrowed by a per-lane `guard`).

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;

use super::container::SigElement;
use super::host::{Dispatch, Draw, Host, Root, View};
use super::{HostShader, container, emit, lower, validator};
use crate::dxbc::{Container, FourCC};
use crate::gfx10::{self, Dpp};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;
const EXEC_LO: u8 = 0x7E;
const VCC_LO: u8 = 0x6A;
const LITERAL: u32 = 0xFF;

const fn s_mov_b32(sdst: u8, src0: u8) -> u32 {
    0xBE80_0000 | (sdst as u32) << 16 | 0x03 << 8 | src0 as u32
}

const fn s_and_saveexec_b32(sdst: u8, src0: u8) -> u32 {
    0xBE80_0000 | (sdst as u32) << 16 | 0x3C << 8 | src0 as u32
}

const fn s_or_b32(sdst: u8, src0: u8, src1: u8) -> u32 {
    0x8000_0000 | (0x10u32 << 23) | (sdst as u32) << 16 | (src1 as u32) << 8 | src0 as u32
}

const fn s_cbranch_execz(simm16: i16) -> u32 {
    0xBF88_0000 | (simm16 as u16 as u32)
}

const fn s_load_dwordx4(dst: u8, base: u8) -> [u32; 2] {
    [
        0x3D << 26 | 0x02 << 18 | (dst as u32) << 6 | (base as u32 >> 1),
        125 << 25,
    ]
}

const fn v_add_nc_u32_lit(vdst: u8, lit: u32, vsrc1: u8) -> [u32; 2] {
    [
        (0x25 << 25) | (vdst as u32) << 17 | (vsrc1 as u32) << 9 | LITERAL,
        lit,
    ]
}

const fn v_and_b32_imm(vdst: u8, imm: u8, vsrc1: u8) -> u32 {
    (0x1B << 25) | (vdst as u32) << 17 | (vsrc1 as u32) << 9 | (0x80 + imm as u32)
}

const fn v_lshlrev_b32_imm(vdst: u8, imm: u8, vsrc1: u8) -> u32 {
    (0x1A << 25) | (vdst as u32) << 17 | (vsrc1 as u32) << 9 | (0x80 + imm as u32)
}

const fn v_cmp_eq_u32_imm(imm: u8, vsrc1: u8) -> u32 {
    0x7C00_0000 | (0xC2 << 17) | (vsrc1 as u32) << 9 | (0x80 + imm as u32)
}

const fn v_cmpx_eq_u32_imm(imm: u8, vsrc1: u8) -> u32 {
    0x7C00_0000 | (0xD2 << 17) | (vsrc1 as u32) << 9 | (0x80 + imm as u32)
}

const fn v_cmp_ne_u32_imm(imm: u8, vsrc1: u8) -> u32 {
    0x7C00_0000 | (0xC5 << 17) | (vsrc1 as u32) << 9 | (0x80 + imm as u32)
}

const fn v_cmpx_ne_u32_imm(imm: u8, vsrc1: u8) -> u32 {
    0x7C00_0000 | (0xD5 << 17) | (vsrc1 as u32) << 9 | (0x80 + imm as u32)
}

const fn dpp16_word(
    vsrc0: u8,
    ctrl: u16,
    row_mask: u8,
    bank_mask: u8,
    bound_ctrl: bool,
    fi: bool,
) -> u32 {
    ((row_mask as u32 & 0xF) << 28)
        | ((bank_mask as u32 & 0xF) << 24)
        | ((bound_ctrl as u32) << 19)
        | ((fi as u32) << 18)
        | ((ctrl as u32 & 0x1FF) << 8)
        | vsrc0 as u32
}

const fn dpp8_word(vsrc0: u8, sel: [u8; 8]) -> u32 {
    let mut s = 0u32;
    let mut i = 0;
    while i < 8 {
        s |= (sel[i] as u32 & 7) << (3 * i);
        i += 1;
    }
    (s << 8) | vsrc0 as u32
}

fn encode_dpp16(vdst: u8, vsrc0: u8, vsrc1: Option<u8>, dpp: Dpp) -> [u32; 2] {
    let Dpp::Row {
        ctrl,
        row_mask,
        bank_mask,
        bound_ctrl,
        fi,
    } = dpp
    else {
        panic!("expected Dpp::Row");
    };
    let w0 = match vsrc1 {
        None => 0x7E00_0000 | (vdst as u32) << 17 | (0x01 << 9) | 0xFA,
        Some(v1) => (0x25 << 25) | (vdst as u32) << 17 | (v1 as u32) << 9 | 0xFA,
    };
    [
        w0,
        dpp16_word(vsrc0, ctrl, row_mask, bank_mask, bound_ctrl, fi),
    ]
}

const fn v_mov_b32_dpp8(vdst: u8, vsrc0: u8, sel: [u8; 8], fi: bool) -> [u32; 2] {
    let tag = if fi { 0xEA } else { 0xE9 };
    [
        0x7E00_0000 | (vdst as u32) << 17 | (0x01 << 9) | tag,
        dpp8_word(vsrc0, sel),
    ]
}

const fn v_add_nc_u32_dpp8(vdst: u8, vsrc0: u8, vsrc1: u8, sel: [u8; 8], fi: bool) -> [u32; 2] {
    let tag = if fi { 0xEA } else { 0xE9 };
    [
        (0x25 << 25) | (vdst as u32) << 17 | (vsrc1 as u32) << 9 | tag,
        dpp8_word(vsrc0, sel),
    ]
}

const fn ds_swizzle_b32(vdst: u8, vsrc: u8, offset: u16) -> [u32; 2] {
    [
        (0x36 << 26) | (0x35 << 18) | offset as u32,
        (vdst as u32) << 24 | vsrc as u32,
    ]
}

const fn buffer_store_dword_offen(vdata: u8, vaddr: u8, srsrc: u8) -> [u32; 2] {
    [
        (0x38 << 26) | (0x1C << 18) | (1 << 12),
        (0x80 << 24) | ((srsrc as u32 >> 2) & 0x1F) << 16 | (vdata as u32) << 8 | vaddr as u32,
    ]
}

#[derive(Clone, Copy, Debug)]
enum Divergence {
    /// Full `EXEC = 0xFFFF_FFFF` (no narrowing).
    Full,
    /// Straight-line `v_cmpx_*` narrowing (lowers with a per-lane `guard`).
    Guarded,
    /// `v_cmp_*` + `s_and_saveexec_b32` + `s_cbranch_execz` (lowers to a DXIL
    /// branch with `guard == None` inside the active arm).
    Branched,
    /// Branch on `(lane & 4) == 0` (`0x0F0F_0F0F`), then inside the branch
    /// narrow with `v_cmpx_ne_u32` on `(lane & 3) != 3` (`0x0707_0707`),
    /// exercising active, guard-inactive, branch-inactive, and row-OOB lanes
    /// in a single wave.
    NestedBranchAndGuard,
}

#[derive(Clone, Copy, Debug)]
enum MaskPattern {
    All,
    Even,
    ThreeOfFour,
    Nested0707,
}

impl MaskPattern {
    fn mask(self) -> u32 {
        match self {
            MaskPattern::All => 0xFFFF_FFFF,
            MaskPattern::Even => 0x5555_5555,
            MaskPattern::ThreeOfFour => 0x7777_7777,
            MaskPattern::Nested0707 => 0x0707_0707,
        }
    }
}

fn lane_v1(lane: u32) -> u32 {
    0x1000_0001u32.wrapping_add(lane)
}
fn lane_v2_old(lane: u32) -> u32 {
    0xDEAD_0000u32.wrapping_add(lane)
}
fn lane_v3_rhs(lane: u32) -> u32 {
    0x0001_0000u32.wrapping_add(lane)
}

/// Exact CPU reference model for AMD RDNA2 (GFX10.3) `DPP16` and `DPP8`/`DPP8FI`.
fn ref_dpp(dpp: Dpp, add_rhs: bool, exec: u32) -> [u32; 32] {
    let mut out = [0u32; 32];
    for lane in 0..32u32 {
        let old = lane_v2_old(lane);
        if (exec >> lane) & 1 == 0 {
            out[lane as usize] = old;
            continue;
        }
        let x = match dpp {
            Dpp::Lanes { sel, fi } => {
                let src = (lane & !7) | ((sel >> (3 * (lane & 7))) & 7);
                let src_active = (exec >> src) & 1 != 0;
                if src_active || fi { lane_v1(src) } else { 0 }
            }
            Dpp::Row {
                ctrl,
                row_mask,
                bank_mask,
                bound_ctrl,
                fi,
            } => {
                let row_ok = (row_mask >> (lane >> 4 & 3)) & 1 != 0;
                let bank_ok = (bank_mask >> (lane >> 2 & 3)) & 1 != 0;
                if !(row_ok && bank_ok) {
                    out[lane as usize] = old;
                    continue;
                }
                let row = lane & !15;
                let pos = lane & 15;
                let n = u32::from(ctrl & 15);
                let (src, inside) = match ctrl {
                    0..=0xFF => (
                        (lane & !3) | (u32::from(ctrl) >> (2 * (lane & 3)) & 3),
                        true,
                    ),
                    0x101..=0x10F => (lane.wrapping_add(n), pos + n < 16),
                    0x111..=0x11F => (lane.wrapping_sub(n), pos >= n),
                    0x121..=0x12F => (row | (pos.wrapping_sub(n) & 15), true),
                    0x140 => (lane ^ 15, true),
                    0x141 => (lane ^ 7, true),
                    0x150..=0x15F => (row | n, true),
                    0x160..=0x16F => (lane ^ n, true),
                    _ => panic!("unhandled ctrl {ctrl:#x}"),
                };
                let src_active = inside && ((exec >> src) & 1 != 0);
                if inside && (src_active || fi) {
                    lane_v1(src)
                } else if bound_ctrl {
                    0
                } else {
                    out[lane as usize] = old;
                    continue;
                }
            }
        };
        out[lane as usize] = if add_rhs {
            x.wrapping_add(lane_v3_rhs(lane))
        } else {
            x
        };
    }
    out
}

/// Exact CPU reference model for AMD RDNA2 `ds_swizzle_b32`.
fn ref_swizzle(offset: u16, exec: u32) -> [u32; 32] {
    let mut out = [0u32; 32];
    for lane in 0..32u32 {
        let old = lane_v2_old(lane);
        if (exec >> lane) & 1 == 0 {
            out[lane as usize] = old;
            continue;
        }
        let off = u32::from(offset);
        let src = if off & 0x8000 != 0 {
            let pick = (off >> (2 * (lane & 3))) & 3;
            (lane & !3) | pick
        } else {
            let and = (off & 0x1F) | !0x1F;
            let or = (off >> 5) & 0x1F;
            let xor = (off >> 10) & 0x1F;
            ((lane & and) | or) ^ xor
        };
        out[lane as usize] = if (exec >> src) & 1 != 0 {
            lane_v1(src)
        } else {
            0
        };
    }
    out
}

fn build_wave32_cs(pattern: MaskPattern, div: Divergence, op_words: &[u32]) -> HostShader {
    let mut w = Vec::new();
    w.push(s_mov_b32(1, 0x80)); // s1 = 0
    w.extend_from_slice(&s_load_dwordx4(4, 0));
    w.extend_from_slice(&v_add_nc_u32_lit(1, 0x1000_0001, 0));
    w.extend_from_slice(&v_add_nc_u32_lit(2, 0xDEAD_0000, 0));
    w.extend_from_slice(&v_add_nc_u32_lit(3, 0x0001_0000, 0));
    match (pattern, div) {
        (MaskPattern::All, Divergence::Full) => {
            w.extend_from_slice(op_words);
        }
        (MaskPattern::Even, Divergence::Guarded) => {
            w.push(v_and_b32_imm(4, 1, 0));
            w.push(s_mov_b32(12, EXEC_LO));
            w.push(v_cmpx_eq_u32_imm(0, 4));
            w.extend_from_slice(op_words);
            w.push(s_mov_b32(EXEC_LO, 12));
        }
        (MaskPattern::ThreeOfFour, Divergence::Guarded) => {
            w.push(v_and_b32_imm(4, 3, 0));
            w.push(s_mov_b32(12, EXEC_LO));
            w.push(v_cmpx_ne_u32_imm(3, 4));
            w.extend_from_slice(op_words);
            w.push(s_mov_b32(EXEC_LO, 12));
        }
        (MaskPattern::Even, Divergence::Branched) => {
            w.push(v_and_b32_imm(4, 1, 0));
            w.push(v_cmp_eq_u32_imm(0, 4));
            w.push(s_and_saveexec_b32(12, VCC_LO));
            w.push(s_cbranch_execz(op_words.len() as i16));
            w.extend_from_slice(op_words);
            w.push(s_or_b32(EXEC_LO, EXEC_LO, 12));
        }
        (MaskPattern::ThreeOfFour, Divergence::Branched) => {
            w.push(v_and_b32_imm(4, 3, 0));
            w.push(v_cmp_ne_u32_imm(3, 4));
            w.push(s_and_saveexec_b32(12, VCC_LO));
            w.push(s_cbranch_execz(op_words.len() as i16));
            w.extend_from_slice(op_words);
            w.push(s_or_b32(EXEC_LO, EXEC_LO, 12));
        }
        (MaskPattern::Nested0707, Divergence::NestedBranchAndGuard) => {
            // Outer branch: (lane & 4) == 0 -> 0x0F0F_0F0F.
            w.push(v_and_b32_imm(4, 4, 0));
            w.push(v_cmp_eq_u32_imm(0, 4));
            w.push(s_and_saveexec_b32(12, VCC_LO));
            // Body inside branch: 4 scalar/vector 1-dword words + op_words.
            let body_words = 4 + op_words.len() as i16;
            w.push(s_cbranch_execz(body_words));
            // Inner straight-line guard: (lane & 3) != 3 -> 0x0707_0707.
            w.push(v_and_b32_imm(6, 3, 0));
            w.push(s_mov_b32(13, EXEC_LO));
            w.push(v_cmpx_ne_u32_imm(3, 6));
            w.extend_from_slice(op_words);
            w.push(s_mov_b32(EXEC_LO, 13));
            // Restore outer EXEC.
            w.push(s_or_b32(EXEC_LO, EXEC_LO, 12));
        }
        other => panic!("unsupported (pattern, divergence) pair: {other:?}"),
    }
    w.push(v_lshlrev_b32_imm(5, 2, 0));
    w.extend_from_slice(&buffer_store_dword_offen(2, 5, 4));
    w.push(S_ENDPGM);

    let code: Vec<u8> = w.iter().flat_map(|x| x.to_le_bytes()).collect();
    let decoded = gfx10::decode(&code).expect("decode synthetic RDNA2 program");
    assert_eq!(decoded.last().and_then(|i| i.name()), Some("s_endpgm"));

    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
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
            stride: 0,
            dwords: 4,
            user_data: 0,
            slot: 0,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 1 << 1,
            threads: [32, 1, 1],
        }),
        code,
    };

    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res =
        resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, &native).expect("io::raise");
    let lowered = lower::lower(&lower::Parts {
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
        let ours = copy[4..20].to_vec();
        v.validate(&mut copy).expect("DXIL validation failed");
        assert_eq!(copy[4..20], ours[..], "container hash mismatch");
    } else {
        eprintln!("no dxil.dll; validation skipped");
    }
    shader
}

fn wave32_hardware_info(host: &Host) -> Option<String> {
    // SAFETY: plain D3D12/DXGI capability queries on a live device.
    unsafe {
        let mut opts1 = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
        host.device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS1,
                (&raw mut opts1).cast(),
                size_of_val(&opts1) as u32,
            )
            .ok()?;
        if !opts1.WaveOps.as_bool() || opts1.WaveLaneCountMin > 32 || opts1.WaveLaneCountMax < 32 {
            return None;
        }
        let mut opts5 = D3D12_FEATURE_DATA_D3D12_OPTIONS5::default();
        let _ = host.device.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS5,
            (&raw mut opts5).cast(),
            size_of_val(&opts5) as u32,
        );
        let mut opts7 = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
        let _ = host.device.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS7,
            (&raw mut opts7).cast(),
            size_of_val(&opts7) as u32,
        );
        let mut sm = D3D12_FEATURE_DATA_SHADER_MODEL {
            HighestShaderModel: D3D_SHADER_MODEL_6_8,
        };
        let _ = host.device.CheckFeatureSupport(
            D3D12_FEATURE_SHADER_MODEL,
            (&raw mut sm).cast(),
            size_of_val(&sm) as u32,
        );
        let luid = host.device.GetAdapterLuid();
        let mut name = String::from("unknown");
        if let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            && let Ok(adapter) = factory.EnumAdapterByLuid::<IDXGIAdapter1>(luid)
            && let Ok(desc) = adapter.GetDesc1()
        {
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0
                || desc.DedicatedVideoMemory == 0
            {
                return None;
            }
            let end = desc
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.Description.len());
            name = String::from_utf16_lossy(&desc.Description[..end]);
        }
        Some(format!(
            "{name} | SM={:#x} | Wave={}-{} | RT={:?} | Mesh={:?}",
            sm.HighestShaderModel.0,
            opts1.WaveLaneCountMin,
            opts1.WaveLaneCountMax,
            opts5.RaytracingTier,
            opts7.MeshShaderTier
        ))
    }
}

fn dispatch_wave32(shader: &HostShader) -> Option<(String, [u32; 32])> {
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; skipped");
        return None;
    };
    let Some(info) = wave32_hardware_info(&host) else {
        eprintln!("hardware device does not support 32-lane waves; skipped");
        return None;
    };
    let (root, pso) = host
        .compute_pipeline(shader)
        .unwrap_or_else(|e| panic!("compute_pipeline failed on {info}: {e}"));
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        assert_eq!(u.root, RootType::Table);
        ud[u.index as usize] = 0;
    }
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(32)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed on {info}: {e}"));
    let mut got = [0u32; 32];
    got.copy_from_slice(&out[0][..32]);
    Some((info, got))
}

fn check_cases(suite: &str, items: &[(&str, HostShader, [u32; 32])]) {
    let mut failures = Vec::new();
    let mut passed = 0usize;
    let mut skipped = 0usize;
    for (label, shader, expected) in items {
        let Some((info, got)) = dispatch_wave32(shader) else {
            skipped += 1;
            continue;
        };
        let mut bad = Vec::new();
        for lane in 0..32 {
            if got[lane] != expected[lane] {
                bad.push((lane, got[lane], expected[lane]));
            }
        }
        if bad.is_empty() {
            passed += 1;
            eprintln!("[PASS] {label}");
        } else {
            let (l0, g0, e0) = bad[0];
            eprintln!(
                "[FAIL] {label} ({}/32 lanes wrong, first lane {l0}: got {g0:#010x} want {e0:#010x}) on {info}",
                bad.len()
            );
            failures.push(format!(
                "{label}: {}/32 lanes mismatched (lane {l0}: got {g0:#010x}, expected {e0:#010x})",
                bad.len()
            ));
        }
    }
    eprintln!(
        "[SUMMARY] {suite}: {passed} passed, {} failed, {skipped} skipped",
        failures.len()
    );
    assert!(
        failures.is_empty(),
        "{suite} had {} failing GPU case(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn cpu_reference_oracle_boundary_checks() {
    // Bank mask disables writes to bank 1 (lanes 4..7), NOT EXEC of lane 4.
    let bm = ref_dpp(
        Dpp::Row {
            ctrl: 0x101,
            row_mask: 0xF,
            bank_mask: 0x5,
            bound_ctrl: false,
            fi: false,
        },
        false,
        0xFFFF_FFFF,
    );
    assert_eq!(
        bm[3],
        lane_v1(4),
        "lane 3 reads active lane 4 in masked bank 1"
    );
    assert_eq!(
        bm[4],
        lane_v2_old(4),
        "bank 1 destination preserves old vdst"
    );
    assert_eq!(
        bm[15],
        lane_v2_old(15),
        "row-OOB with bound_ctrl:0 preserves old vdst"
    );

    // DPP8 (fi:0) substitutes 0 for inactive source; DPP8FI (fi:1) reads it.
    let sel_bits = dpp8_word(0, [1, 0, 3, 2, 5, 4, 7, 6]) >> 8;
    let d8_0 = ref_dpp(
        Dpp::Lanes {
            sel: sel_bits,
            fi: false,
        },
        false,
        0x7777_7777,
    );
    let d8_1 = ref_dpp(
        Dpp::Lanes {
            sel: sel_bits,
            fi: true,
        },
        false,
        0x7777_7777,
    );
    assert_eq!(d8_0[2], 0);
    assert_eq!(d8_1[2], lane_v1(3));
    assert_eq!(d8_0[3], lane_v2_old(3));
    assert_eq!(d8_1[3], lane_v2_old(3));

    // ds_swizzle_b32 returns 0 for inactive source, keeps old vdst on inactive dest.
    let sw = ref_swizzle(0x041F, 0x7777_7777);
    assert_eq!(sw[0], lane_v1(1));
    assert_eq!(sw[2], 0);
    assert_eq!(sw[3], lane_v2_old(3));
}

struct Dpp16Case {
    label: &'static str,
    pattern: MaskPattern,
    div: Divergence,
    dpp: Dpp,
    add_rhs: bool,
}

#[test]
fn dpp16_inactive_source_and_bound_ctrl_match_rdna2_on_gpu() {
    let row = |ctrl, row_mask, bank_mask, bound_ctrl, fi| Dpp::Row {
        ctrl,
        row_mask,
        bank_mask,
        bound_ctrl,
        fi,
    };
    let specs = [
        Dpp16Case {
            label: "dpp16 row_shr:1 bc:1 fi:0 (guarded, 0x55555555)",
            pattern: MaskPattern::Even,
            div: Divergence::Guarded,
            dpp: row(0x111, 0xF, 0xF, true, false),
            add_rhs: true,
        },
        Dpp16Case {
            label: "dpp16 row_shr:1 bc:1 fi:0 (branched, 0x55555555)",
            pattern: MaskPattern::Even,
            div: Divergence::Branched,
            dpp: row(0x111, 0xF, 0xF, true, false),
            add_rhs: true,
        },
        Dpp16Case {
            label: "dpp16 row_shr:1 bc:0 fi:0 (guarded, 0x55555555)",
            pattern: MaskPattern::Even,
            div: Divergence::Guarded,
            dpp: row(0x111, 0xF, 0xF, false, false),
            add_rhs: true,
        },
        Dpp16Case {
            label: "dpp16 row_shr:1 bc:0 fi:0 (branched, 0x55555555)",
            pattern: MaskPattern::Even,
            div: Divergence::Branched,
            dpp: row(0x111, 0xF, 0xF, false, false),
            add_rhs: true,
        },
        Dpp16Case {
            label: "dpp16 row_shr:1 bc:0 fi:1 (guarded, 0x55555555)",
            pattern: MaskPattern::Even,
            div: Divergence::Guarded,
            dpp: row(0x111, 0xF, 0xF, false, true),
            add_rhs: true,
        },
        Dpp16Case {
            label: "dpp16 row_shr:1 bc:1 fi:1 (guarded, 0x55555555)",
            pattern: MaskPattern::Even,
            div: Divergence::Guarded,
            dpp: row(0x111, 0xF, 0xF, true, true),
            add_rhs: true,
        },
        Dpp16Case {
            label: "dpp16 quad_perm:[1,0,3,2] bc:0 fi:0 (guarded, 0x77777777)",
            pattern: MaskPattern::ThreeOfFour,
            div: Divergence::Guarded,
            dpp: row(0xB1, 0xF, 0xF, false, false),
            add_rhs: false,
        },
        Dpp16Case {
            label: "dpp16 quad_perm:[1,0,3,2] bc:1 fi:0 (branched, 0x77777777)",
            pattern: MaskPattern::ThreeOfFour,
            div: Divergence::Branched,
            dpp: row(0xB1, 0xF, 0xF, true, false),
            add_rhs: false,
        },
        Dpp16Case {
            label: "dpp16 quad_perm:[1,0,3,2] bc:0 fi:1 (guarded, 0x77777777)",
            pattern: MaskPattern::ThreeOfFour,
            div: Divergence::Guarded,
            dpp: row(0xB1, 0xF, 0xF, false, true),
            add_rhs: false,
        },
        Dpp16Case {
            label: "dpp16 row_shl:1 rm:0x1 bm:0x5 bc:1 fi:0 (guarded, 0x77777777)",
            pattern: MaskPattern::ThreeOfFour,
            div: Divergence::Guarded,
            dpp: row(0x101, 0x1, 0x5, true, false),
            add_rhs: true,
        },
        Dpp16Case {
            label: "dpp16 row_shl:1 rm:0xF bm:0x5 bc:0 fi:0 (full EXEC: bank_mask does not deactivate source)",
            pattern: MaskPattern::All,
            div: Divergence::Full,
            dpp: row(0x101, 0xF, 0x5, false, false),
            add_rhs: false,
        },
        Dpp16Case {
            label: "dpp16 row_ror:1 bc:0 fi:0 (guarded, 0x55555555)",
            pattern: MaskPattern::Even,
            div: Divergence::Guarded,
            dpp: row(0x121, 0xF, 0xF, false, false),
            add_rhs: false,
        },
        Dpp16Case {
            label: "dpp16 row_share:3 bc:1 fi:0 (guarded, 0x77777777)",
            pattern: MaskPattern::ThreeOfFour,
            div: Divergence::Guarded,
            dpp: row(0x153, 0xF, 0xF, true, false),
            add_rhs: false,
        },
        Dpp16Case {
            label: "dpp16 row_xmask:1 bc:0 fi:0 (branched, 0x55555555)",
            pattern: MaskPattern::Even,
            div: Divergence::Branched,
            dpp: row(0x161, 0xF, 0xF, false, false),
            add_rhs: false,
        },
        Dpp16Case {
            label: "dpp16 row_mirror bc:1 fi:0 (guarded, 0x55555555)",
            pattern: MaskPattern::Even,
            div: Divergence::Guarded,
            dpp: row(0x140, 0xF, 0xF, true, false),
            add_rhs: false,
        },
        Dpp16Case {
            label: "dpp16 row_half_mirror bc:0 fi:0 (guarded, 0x55555555)",
            pattern: MaskPattern::Even,
            div: Divergence::Guarded,
            dpp: row(0x141, 0xF, 0xF, false, false),
            add_rhs: false,
        },
        Dpp16Case {
            label: "dpp16 row_shr:1 bc:0 fi:0 (nested branch+guard, 0x07070707)",
            pattern: MaskPattern::Nested0707,
            div: Divergence::NestedBranchAndGuard,
            dpp: row(0x111, 0xF, 0xF, false, false),
            add_rhs: true,
        },
        Dpp16Case {
            label: "dpp16 row_shl:1 bc:1 fi:0 (nested branch+guard, 0x07070707)",
            pattern: MaskPattern::Nested0707,
            div: Divergence::NestedBranchAndGuard,
            dpp: row(0x101, 0xF, 0xF, true, false),
            add_rhs: true,
        },
    ];

    let items: Vec<_> = specs
        .iter()
        .map(|s| {
            let words = encode_dpp16(2, 1, s.add_rhs.then_some(3), s.dpp);
            (
                s.label,
                build_wave32_cs(s.pattern, s.div, &words),
                ref_dpp(s.dpp, s.add_rhs, s.pattern.mask()),
            )
        })
        .collect();
    check_cases("DPP16", &items);
}

struct Dpp8Case {
    label: &'static str,
    pattern: MaskPattern,
    div: Divergence,
    fi: bool,
    add_rhs: bool,
}

#[test]
fn dpp8_and_dpp8fi_inactive_source_match_rdna2_on_gpu() {
    let sel = [1, 0, 3, 2, 5, 4, 7, 6];
    let sel_bits = dpp8_word(0, sel) >> 8;
    let specs = [
        Dpp8Case {
            label: "dpp8 (0xE9, fi:0) mov (guarded, 0x77777777)",
            pattern: MaskPattern::ThreeOfFour,
            div: Divergence::Guarded,
            fi: false,
            add_rhs: false,
        },
        Dpp8Case {
            label: "dpp8 (0xE9, fi:0) add (branched, 0x77777777)",
            pattern: MaskPattern::ThreeOfFour,
            div: Divergence::Branched,
            fi: false,
            add_rhs: true,
        },
        Dpp8Case {
            label: "dpp8 (0xE9, fi:0) add (nested branch+guard, 0x07070707)",
            pattern: MaskPattern::Nested0707,
            div: Divergence::NestedBranchAndGuard,
            fi: false,
            add_rhs: true,
        },
        Dpp8Case {
            label: "dpp8fi (0xEA, fi:1) mov (guarded, 0x77777777)",
            pattern: MaskPattern::ThreeOfFour,
            div: Divergence::Guarded,
            fi: true,
            add_rhs: false,
        },
        Dpp8Case {
            label: "dpp8fi (0xEA, fi:1) add (guarded, 0x55555555)",
            pattern: MaskPattern::Even,
            div: Divergence::Guarded,
            fi: true,
            add_rhs: true,
        },
    ];

    let items: Vec<_> = specs
        .iter()
        .map(|s| {
            let words = if s.add_rhs {
                v_add_nc_u32_dpp8(2, 1, 3, sel, s.fi)
            } else {
                v_mov_b32_dpp8(2, 1, sel, s.fi)
            };
            (
                s.label,
                build_wave32_cs(s.pattern, s.div, &words),
                ref_dpp(
                    Dpp::Lanes {
                        sel: sel_bits,
                        fi: s.fi,
                    },
                    s.add_rhs,
                    s.pattern.mask(),
                ),
            )
        })
        .collect();
    check_cases("DPP8/DPP8FI", &items);
}

#[test]
fn ds_swizzle_b32_inactive_source_returns_zero_on_gpu() {
    let specs: &[(&str, MaskPattern, Divergence, u16)] = &[
        (
            "ds_swizzle_b32 bitmode SWAP(1) (guarded, 0x55555555)",
            MaskPattern::Even,
            Divergence::Guarded,
            0x041F,
        ),
        (
            "ds_swizzle_b32 bitmode SWAP(1) (branched, 0x55555555)",
            MaskPattern::Even,
            Divergence::Branched,
            0x041F,
        ),
        (
            "ds_swizzle_b32 bitmode SWAP(1) (guarded, 0x77777777)",
            MaskPattern::ThreeOfFour,
            Divergence::Guarded,
            0x041F,
        ),
        (
            "ds_swizzle_b32 quad_perm [3,2,1,0] (guarded, 0x77777777)",
            MaskPattern::ThreeOfFour,
            Divergence::Guarded,
            0x801B,
        ),
        (
            "ds_swizzle_b32 quad_perm [3,2,1,0] (branched, 0x77777777)",
            MaskPattern::ThreeOfFour,
            Divergence::Branched,
            0x801B,
        ),
        (
            "ds_swizzle_b32 bitmode SWAP(1) (nested branch+guard, 0x07070707)",
            MaskPattern::Nested0707,
            Divergence::NestedBranchAndGuard,
            0x041F,
        ),
    ];

    let items: Vec<_> = specs
        .iter()
        .map(|&(label, pattern, div, offset)| {
            (
                label,
                build_wave32_cs(pattern, div, &ds_swizzle_b32(2, 1, offset)),
                ref_swizzle(offset, pattern.mask()),
            )
        })
        .collect();
    check_cases("ds_swizzle_b32", &items);
}

#[test]
fn pixel_quad_swizzle_and_dpp_validate_and_create_pipeline() {
    // Pixel shader with both unguarded WQM quad_perm (must not depend on
    // waveActiveBallot) and guarded quad ds_swizzle_b32.
    let exp_mrt0 = [
        0xF800_180F, // exp mrt0 v1, v2, v2, v1 done vm (uncompressed)
        0x0102_0201,
    ];
    let mut w = Vec::new();
    // v1 = 1.0 (0x3F80_0000), v2 = 0.5 (0x3F00_0000)
    w.extend_from_slice(&[0x7E02_02FF, 0x3F80_0000]);
    w.extend_from_slice(&[0x7E04_02FF, 0x3F00_0000]);
    // Unguarded WQM quad_perm:[1,0,3,2] (ddx-style quad read).
    w.extend_from_slice(&encode_dpp16(
        1,
        1,
        None,
        Dpp::Row {
            ctrl: 0xB1,
            row_mask: 0xF,
            bank_mask: 0xF,
            bound_ctrl: true,
            fi: false,
        },
    ));
    // Guarded quad swizzle under narrowed EXEC.
    w.push(s_mov_b32(12, EXEC_LO));
    w.push(v_cmpx_eq_u32_imm(0, 0));
    w.extend_from_slice(&ds_swizzle_b32(2, 1, 0x80B1));
    w.push(s_mov_b32(EXEC_LO, 12));
    w.extend_from_slice(&exp_mrt0);
    w.push(S_ENDPGM);

    let native = NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 4,
        layout: vec![],
        used: vec![],
        io: StageIo::Ps {
            inputs: vec![],
            input_addr: 0,
            db_shader_control: 0,
            patches: vec![],
        },
        code: w.iter().flat_map(|x| x.to_le_bytes()).collect(),
    };
    let osg1 = container::signature(
        &[SigElement {
            name: "SV_Target".into(),
            index: 0,
            kind: 16,
            comp: 9,
            interp: 0,
            row: Some(0),
            col: 0,
            cols: 4,
            usage: 15,
            half: false,
        }],
        false,
    );
    let bytes = crate::dxbc::build(&[(FourCC::OSG1, &osg1)]);
    let c = Container::parse(&bytes).unwrap();
    let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    let ps = emit(&c, &native, &p, &st, &io, &res).unwrap();

    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut ps.dxil.clone())
            .expect("pixel shader DXIL validation failed");
    }

    // Verify graphics pipeline creation on both hardware and WARP.
    let vs = super::host::stub_vs(&ps).expect("stub_vs");
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let _ = host
            .pipeline(&vs, &ps)
            .unwrap_or_else(|e| panic!("{label}: graphics pipeline creation failed: {e}"));
    }
}

/// Builds a 64-thread console wave64 compute shader lowered for a 32-lane host
/// (`halves` mode), setting asymmetric 64-bit `EXEC` from root constants
/// `s[2:3]` (`EXEC_LO` for half 0, `EXEC_HI` for half 1).
fn build_wave64_halves_cs(op_words: &[u32]) -> HostShader {
    let mut w = Vec::new();
    w.push(s_mov_b32(5, 0x80)); // s5 = 0 (table descriptor addr high)
    w.extend_from_slice(&[
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 2,
        125 << 25, // s_load_dwordx4 s[8:11], s[4:5], 0
    ]);
    w.extend_from_slice(&v_add_nc_u32_lit(1, 0x1000_0001, 0));
    w.extend_from_slice(&v_add_nc_u32_lit(2, 0xDEAD_0000, 0));
    w.extend_from_slice(&v_add_nc_u32_lit(3, 0x0001_0000, 0));
    w.push(0xBEFE_0402); // s_mov_b64 exec, s[2:3]
    w.extend_from_slice(op_words);
    w.push(0xBEFE_04C1); // s_mov_b64 exec, -1
    w.push(v_lshlrev_b32_imm(5, 2, 0));
    w.extend_from_slice(&buffer_store_dword_offen(2, 5, 8));
    w.push(S_ENDPGM);

    let code: Vec<u8> = w.iter().flat_map(|x| x.to_le_bytes()).collect();
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: true,
        vgprs: 8,
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
            threads: [64, 1, 1],
        }),
        code,
    };

    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res =
        resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, &native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let lowered = lower::lower(&lower::Parts {
        native: &native,
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
    let lowered = lowered.expect("lower wave64 halves");

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
        let ours = copy[4..20].to_vec();
        v.validate(&mut copy).expect("wave64 halves DXIL validation failed");
        assert_eq!(copy[4..20], ours[..], "container hash mismatch");
    }
    shader
}

#[test]
fn wave64_halves_dpp_and_swizzle_match_rdna2_on_gpu() {
    let exec_lo: u32 = 0x5555_5555;
    let exec_hi: u32 = 0x7777_7777;
    let sel = [1, 0, 3, 2, 5, 4, 7, 6];
    let sel_bits = dpp8_word(0, sel) >> 8;

    let ref_half = |dpp_or_sw: Result<(Dpp, bool), u16>, exec: u32, base: u32| -> [u32; 32] {
        let mut out = [0u32; 32];
        for lane in 0..32u32 {
            let clane = base + lane;
            let old = lane_v2_old(clane);
            if (exec >> lane) & 1 == 0 {
                out[lane as usize] = old;
                continue;
            }
            match dpp_or_sw {
                Ok((dpp, add_rhs)) => {
                    let x = match dpp {
                        Dpp::Lanes { sel, fi } => {
                            let src = (lane & !7) | ((sel >> (3 * (lane & 7))) & 7);
                            if ((exec >> src) & 1 != 0) || fi {
                                lane_v1(base + src)
                            } else {
                                0
                            }
                        }
                        Dpp::Row {
                            ctrl,
                            row_mask,
                            bank_mask,
                            bound_ctrl,
                            fi,
                        } => {
                            let row = (clane >> 4) & 3;
                            let bank = (clane >> 2) & 3;
                            if ((u32::from(row_mask) >> row) & 1 == 0)
                                || ((u32::from(bank_mask) >> bank) & 1 == 0)
                            {
                                out[lane as usize] = old;
                                continue;
                            }
                            let n = u32::from(ctrl & 15);
                            let (src, inside) = (lane.wrapping_sub(n), (lane & 15) >= n);
                            let src_active = inside && ((exec >> src) & 1 != 0);
                            if inside && (src_active || fi) {
                                lane_v1(base + src)
                            } else if bound_ctrl {
                                0
                            } else {
                                out[lane as usize] = old;
                                continue;
                            }
                        }
                    };
                    out[lane as usize] = if add_rhs {
                        x.wrapping_add(lane_v3_rhs(clane))
                    } else {
                        x
                    };
                }
                Err(offset) => {
                    let off = u32::from(offset);
                    let and = (off & 0x1F) | !0x1F;
                    let or = (off >> 5) & 0x1F;
                    let xor = (off >> 10) & 0x1F;
                    let src = ((lane & and) | or) ^ xor;
                    out[lane as usize] = if (exec >> src) & 1 != 0 {
                        lane_v1(base + src)
                    } else {
                        0
                    };
                }
            }
        }
        out
    };

    let dpp_shr_bc0 = Dpp::Row {
        ctrl: 0x111,
        row_mask: 0xF,
        bank_mask: 0xF,
        bound_ctrl: false,
        fi: false,
    };
    let dpp_shr_bc1 = Dpp::Row {
        ctrl: 0x111,
        row_mask: 0xF,
        bank_mask: 0xF,
        bound_ctrl: true,
        fi: false,
    };
    let dpp_shr_row2 = Dpp::Row {
        ctrl: 0x111,
        row_mask: 0x4,
        bank_mask: 0xF,
        bound_ctrl: true,
        fi: false,
    };
    let dpp_shr_row3 = Dpp::Row {
        ctrl: 0x111,
        row_mask: 0x8,
        bank_mask: 0xF,
        bound_ctrl: true,
        fi: false,
    };
    let cases: Vec<(&str, Vec<u32>, Result<(Dpp, bool), u16>)> = vec![
        (
            "wave64 halves dpp16 row_shr:1 bc:0 fi:0",
            encode_dpp16(2, 1, Some(3), dpp_shr_bc0).to_vec(),
            Ok((dpp_shr_bc0, true)),
        ),
        (
            "wave64 halves dpp16 row_shr:1 bc:1 fi:0",
            encode_dpp16(2, 1, Some(3), dpp_shr_bc1).to_vec(),
            Ok((dpp_shr_bc1, true)),
        ),
        (
            "wave64 halves dpp16 row_shr:1 row_mask:0x4 bc:1 fi:0",
            encode_dpp16(2, 1, Some(3), dpp_shr_row2).to_vec(),
            Ok((dpp_shr_row2, true)),
        ),
        (
            "wave64 halves dpp16 row_shr:1 row_mask:0x8 bc:1 fi:0",
            encode_dpp16(2, 1, Some(3), dpp_shr_row3).to_vec(),
            Ok((dpp_shr_row3, true)),
        ),
        (
            "wave64 halves dpp8 (0xE9, fi:0) mov",
            v_mov_b32_dpp8(2, 1, sel, false).to_vec(),
            Ok((Dpp::Lanes { sel: sel_bits, fi: false }, false)),
        ),
        (
            "wave64 halves dpp8fi (0xEA, fi:1) mov",
            v_mov_b32_dpp8(2, 1, sel, true).to_vec(),
            Ok((Dpp::Lanes { sel: sel_bits, fi: true }, false)),
        ),
        (
            "wave64 halves ds_swizzle_b32 SWAP(1)",
            ds_swizzle_b32(2, 1, 0x041F).to_vec(),
            Err(0x041F),
        ),
    ];

    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; wave64 halves skipped");
        return;
    };
    let Some(info) = wave32_hardware_info(&host) else {
        eprintln!("hardware device does not support 32-lane waves; wave64 halves skipped");
        return;
    };
    for (label, words, spec) in cases {
        let shader = build_wave64_halves_cs(&words);
        let (root, pso) = host
            .compute_pipeline(&shader)
            .unwrap_or_else(|e| panic!("{label}: compute_pipeline failed on {info}: {e}"));
        let mut ud = vec![0u32; shader.user_data_dwords().max(5) as usize];
        for u in &shader.user_data {
            ud[u.index as usize] = match (u.root, u.dword) {
                (RootType::Constants, 2) => exec_lo,
                (RootType::Constants, 3) => exec_hi,
                _ => 0,
            };
        }
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
            .unwrap_or_else(|e| panic!("{label}: dispatch failed on {info}: {e}"));
        let mut expected = [0u32; 64];
        expected[..32].copy_from_slice(&ref_half(spec, exec_lo, 0));
        expected[32..].copy_from_slice(&ref_half(spec, exec_hi, 32));
        for lane in 0..64 {
            assert_eq!(
                out[0][lane], expected[lane],
                "{label}: lane {lane} got {:#010x} want {:#010x} on {info}",
                out[0][lane], expected[lane]
            );
        }
        eprintln!("[PASS] {label} (64/64 lanes) on {info}");
    }
}

#[test]
fn pixel_quad_edge_dpp_and_guarded_swizzle_match_on_gpu() {
    run_pixel_quad_edge(None);
}

#[test]
fn divergent_pixel_quad_reads_are_reported_as_unsupported() {
    let dpp = Dpp::Row {
        ctrl: 0xB1,
        row_mask: 0xF,
        bank_mask: 0xF,
        bound_ctrl: true,
        fi: false,
    };
    for op in [encode_dpp16(5, 2, None, dpp), ds_swizzle_b32(5, 2, 0x80B1)] {
        run_pixel_quad_edge(Some(op));
    }
}

fn run_pixel_quad_edge(divergent_op: Option<[u32; 2]>) {
    use super::bitcode::{Attr, Term};
    use super::shader::Shader;

    // v2 = SV_Position.x, v3 = SV_Position.y (input_addr: POS_X | POS_Y after
    // perspective barycentrics).
    // R (v4): unguarded WQM DPP16 quad_perm:[1,0,3,2](v2) - v2 -> +1 (even x) / -1 (odd x),
    //         even on diagonal triangle edge pixels whose horizontal neighbour is a helper lane.
    // G (v5): initialized to 99.0; under guard (floor(x) & 1) == 0, ds_swizzle_b32 [1,0,3,2]
    //         from v2 -> 0.0 on even x (inactive odd-x source), 99.0 on odd x (old vdst).
    let mut w = Vec::new();
    w.push(0xBEFC_0380); // s_mov_b32 m0, 0
    w.extend_from_slice(&encode_dpp16(
        4,
        2,
        None,
        Dpp::Row {
            ctrl: 0xB1,
            row_mask: 0xF,
            bank_mask: 0xF,
            bound_ctrl: true,
            fi: false,
        },
    ));
    w.push(0x0808_0504); // v_sub_f32 v4, v4, v2
    w.extend_from_slice(&[0x7E0A_02FF, 99.0f32.to_bits()]); // v_mov_b32 v5, 99.0
    w.push(0x7E0C_0F02); // v_cvt_u32_f32 v6, v2
    w.push(v_and_b32_imm(6, 1, 6));
    if let Some(op) = divergent_op {
        w.push(v_cmp_eq_u32_imm(0, 6));
        w.push(s_and_saveexec_b32(12, VCC_LO));
        w.push(s_cbranch_execz(op.len() as i16));
        w.extend_from_slice(&op);
        w.push(s_or_b32(EXEC_LO, EXEC_LO, 12));
    } else {
        w.push(s_mov_b32(12, EXEC_LO));
        w.push(v_cmpx_eq_u32_imm(0, 6));
        w.extend_from_slice(&ds_swizzle_b32(5, 2, 0x80B1));
        w.push(s_mov_b32(EXEC_LO, 12));
    }
    // `lower` defers failures of value-producing ops until a live consumer reads the value (`exp`).
    let exp_offset = w.len() * 4;
    w.extend_from_slice(&[0xF800_180F, 0x0302_0504]); // exp mrt0 v4, v5, v2, v3 done vm
    w.push(S_ENDPGM);

    let native = NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 8,
        layout: vec![],
        used: vec![],
        io: StageIo::Ps {
            inputs: vec![],
            input_addr: 1 << 1 | 1 << 8 | 1 << 9,
            db_shader_control: 0,
            patches: vec![],
        },
        code: w.iter().flat_map(|x| x.to_le_bytes()).collect(),
    };
    let pos_sig = SigElement {
        name: "SV_Position".into(),
        index: 0,
        kind: 3,
        comp: 9,
        interp: 4,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 3,
        half: false,
    };
    let out_sig = SigElement {
        name: "SV_Target".into(),
        index: 0,
        kind: 16,
        comp: 9,
        interp: 0,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 15,
        half: false,
    };
    let isg1 = container::signature(std::slice::from_ref(&pos_sig), true);
    let osg1 = container::signature(&[out_sig], false);
    let bytes = crate::dxbc::build(&[(FourCC::ISG1, &isg1), (FourCC::OSG1, &osg1)]);
    let c = Container::parse(&bytes).unwrap();
    let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    let emitted = emit(&c, &native, &p, &st, &io, &res);
    if divergent_op.is_some() {
        assert!(
            matches!(
                emitted,
                Err(crate::ShaderError::Unsupported {
                    offset,
                    what: "a pixel quad read inside divergent control flow",
                }) if offset == exp_offset
            ),
            "expected Unsupported at offset {exp_offset}, got {emitted:?}"
        );
        return;
    }
    let ps = emitted.unwrap();

    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut ps.dxil.clone())
            .expect("pixel edge DXIL validation failed");
    }

    // Minimal passthrough vertex shader (POSITION -> SV_Position).
    let mut s = Shader::new(false);
    s.inputs = vec![SigElement {
        name: "POSITION".into(),
        index: 0,
        kind: 0,
        comp: 9,
        interp: 0,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 15,
        half: false,
    }];
    s.outputs = vec![SigElement {
        usage: 15,
        ..pos_sig
    }];
    for col in 0..4 {
        let args = [s.m.i32(0), s.m.i32(0), s.m.i8(col), s.m.undef(s.t.i32)];
        let v = s.op(s.entry, "loadInput.f32", Attr::ReadNone, s.t.f32, 4, &args);
        let args = [s.m.i32(0), s.m.i32(0), s.m.i8(col), v];
        s.op(s.entry, "storeOutput.f32", Attr::NoUnwind, s.t.void, 5, &args);
    }
    s.m.terminate(s.entry, Term::Ret);
    let vs = HostShader {
        stage: HwStage::Vs,
        dxil: s.finish(),
        user_data: Vec::new(),
        samplers: Vec::new(),
        static_samplers: Vec::new(),
        root_cbvs: Vec::new(),
        root_srvs: Vec::new(),
        root_uavs: Vec::new(),
        root_indexed_views: Vec::new(),
    };
    // Diagonal triangle partially covering a 16x16 target so edge quads contain helper lanes.
    let vertices: [f32; 12] = [
        -1.0, 1.0, 0.5, 1.0,
        -1.0, -1.0, 0.5, 1.0,
         0.5, 1.0, 0.5, 1.0,
    ];

    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; pixel edge test skipped");
            continue;
        };
        let (root, pso) = host
            .pipeline(&vs, &ps)
            .unwrap_or_else(|e| panic!("{label}: pixel edge pipeline failed: {e}"));
        let texels = host
            .draw(
                &root,
                &pso,
                &Draw {
                    roots: Vec::new(),
                    textures: &[],
                    samplers: &[],
                    vertices: &vertices,
                    stride: 16,
                    size: (16, 16),
                    mesh_groups: 0,
                },
            )
            .unwrap_or_else(|e| panic!("{label}: pixel edge draw failed: {e}"));
        let lit: Vec<(usize, &[f32; 4])> = texels.iter().enumerate().filter(|t| t.1[2] != 0.0).collect();
        assert!(
            (40..200).contains(&lit.len()),
            "{label}: expected partial diagonal coverage, got {} lit texels",
            lit.len()
        );
        for (idx, t) in &lit {
            let x = idx % 16;
            let want_dx = if x % 2 == 0 { 1.0 } else { -1.0 };
            let want_guarded = if x % 2 == 0 { 0.0 } else { 99.0 };
            assert!(
                (t[0] - want_dx).abs() < 1e-4,
                "{label}: texel ({x}, {}) unguarded WQM DPP16 dx got {}, want {want_dx}",
                idx / 16,
                t[0]
            );
            assert!(
                (t[1] - want_guarded).abs() < 1e-4,
                "{label}: texel ({x}, {}) guarded quad swizzle got {}, want {want_guarded}",
                idx / 16,
                t[1]
            );
        }
        eprintln!(
            "[PASS] pixel quad edge WQM DPP16 + guarded swizzle ({} lit texels) on {label}",
            lit.len()
        );
    }
}
