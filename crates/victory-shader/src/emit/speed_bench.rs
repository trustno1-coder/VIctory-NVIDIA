//! Time of the whole translation (ir -> structure -> resources -> io -> lower)
//! of synthetic compute programs. No title data.
//!
//! `cargo test --release -p victory-shader translation_speed -- --ignored --nocapture`
//! (and `stage_split` for the time of each stage).
//!
//! `dump_random_translations` writes a hash of the DXIL of 600 random
//! programs to the file named by `DUMP`: run it on two builds and compare
//! the files to show that a change to the translator leaves its output as
//! it was.
use super::lower;
use crate::native::{ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource};
use crate::{io, ir, resource, structure};
const S_ENDPGM: u32 = 0xBF81_0000;
/// SOP1 `s_mov_b32 sdst, src0`.
const fn s_mov_b32(sdst: u8, src0: u8) -> u32 {
    0xBE80_0000 | (sdst as u32) << 16 | 0x03 << 8 | src0 as u32
}

/// SMEM `s_load_dwordx4 s[dst:dst+3], s[base:base+1], 0` (soffset = null).
const fn s_load_dwordx4(dst: u8, base: u8) -> [u32; 2] {
    [
        0x3D << 26 | 0x02 << 18 | (dst as u32) << 6 | (base as u32 >> 1),
        125 << 25,
    ]
}

/// VOP1 `op vdst, src0` (`src0`: 0..105 = SGPR, 256..511 = VGPR).
const fn vop1(op: u16, vdst: u8, src0: u16) -> u32 {
    0x7E00_0000 | (vdst as u32) << 17 | (op as u32) << 9 | (src0 as u32 & 0x1FF)
}

/// MUBUF `buffer_store_dword vdata, off, s[srsrc:srsrc+3], 0 offset:byte_off`.
const fn buffer_store_dword(vdata: u8, srsrc: u8, byte_off: u16) -> [u32; 2] {
    [
        0x38 << 26 | 0x1C << 18 | (byte_off as u32 & 0xFFF),
        0x80 << 24 | ((srsrc as u32 >> 2) & 0x1F) << 16 | (vdata as u32) << 8,
    ]
}

const fn sgpr(n: u8) -> u16 {
    n as u16
}

/// A 64-thread compute program of `blocks` stretches, each a compare that
/// narrows EXEC and `run` vector additions, then a store.
fn program(blocks: usize, run: usize) -> NativeProgram {
    let mut words: Vec<u32> = Vec::new();
    words.push(s_mov_b32(5, 0x80));
    words.extend_from_slice(&s_load_dwordx4(8, 4));
    for i in 0..4u8 { words.push(vop1(0x001, i + 1, sgpr(i))); }
    for b in 0..blocks {
        let r = (b % 6) as u32 + 1;
        words.push(0x7DA4_0080 | (r << 9)); // v_cmpx_eq_u32 0, v[r]
        for k in 0..run {
            let d = ((b + k) % 6) as u32 + 1;
            let s = ((b * 3 + k) % 6) as u32 + 1;
            words.push((0x25 << 25) | (d << 17) | (s << 9) | (256 + ((k as u32) % 6 + 1))); // v_add_nc_u32 vd, v(src0), v(s)
        }
    }
    words.extend_from_slice(&buffer_store_dword(0, 8, 0));
    words.push(S_ENDPGM);
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 16,
        layout: vec![
            RootParam { root: RootType::Constants, table_offset_dwords: 0, sgprs: 4, base_register: 0, user_data: 0, slot: 0 },
            RootParam { root: RootType::Table, table_offset_dwords: 0, sgprs: 1, base_register: 0, user_data: 4, slot: 1 },
        ],
        used: vec![UsedResource { kind: ResourceType::Uav, root: RootType::Table, table_offset_dwords: 0, lower_bound: 0, stride: 0, dwords: 4, user_data: 4, slot: 1 }],
        io: StageIo::Cs(ComputeIo { rsrc2: 5 << 1, threads: [64, 1, 1] }),
        code,
    }
}

fn translate_once(native: &NativeProgram) -> usize {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, native).expect("io::raise");
    let lowered = lower::lower(&lower::Parts { native, p: &p, st: &st, io: &io, res: &res, inputs: Vec::new(), outputs: Vec::new(), prims: Vec::new(), mesh_stride: 0 }).expect("lower");
    lowered.dxil.iter().fold(0xcbf29ce484222325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3)) as usize
}

#[test]
#[ignore = "benchmark"]
fn translation_speed() {
    for (blocks, run) in [(16usize, 16usize), (64, 24), (200, 24), (400, 30)] {
        let native = program(blocks, run);
        let len = translate_once(&native);
        let mut samples = vec![];
        for _ in 0..9 {
            let s = std::time::Instant::now();
            let n = if blocks > 100 { 3 } else { 20 };
            for _ in 0..n { std::hint::black_box(translate_once(std::hint::black_box(&native))); }
            samples.push(s.elapsed().as_secs_f64() / n as f64);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("BENCH insts {:5}: {:9.1} us (dxil fnv {:016x})", native.code.len() / 4, samples[4] * 1e6, len);
    }
}

#[test]
#[ignore = "benchmark"]
fn stage_split() {
    for (blocks, run) in [(64usize, 24usize), (200, 24), (400, 30)] {
        let native = program(blocks, run);
        let mut t = [0f64; 5];
        let n = 3;
        for _ in 0..n {
            let s = std::time::Instant::now();
            let p = ir::build(&native.code, native.wave64, native.vgprs).unwrap();
            t[0] += s.elapsed().as_secs_f64(); let s = std::time::Instant::now();
            let st = structure::structure(&p).unwrap();
            t[1] += s.elapsed().as_secs_f64(); let s = std::time::Instant::now();
            let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
            t[2] += s.elapsed().as_secs_f64(); let s = std::time::Instant::now();
            let io = io::raise(&p, &native).unwrap();
            t[3] += s.elapsed().as_secs_f64(); let s = std::time::Instant::now();
            let l = lower::lower(&lower::Parts { native: &native, p: &p, st: &st, io: &io, res: &res, inputs: Vec::new(), outputs: Vec::new(), prims: Vec::new(), mesh_stride: 0 }).unwrap();
            t[4] += s.elapsed().as_secs_f64();
            std::hint::black_box(l);
        }
        let ms: Vec<String> = t.iter().map(|x| format!("{:.2}", x / n as f64 * 1e3)).collect();
        println!("SPLIT insts {:5}: ir {} | structure {} | resource {} | io {} | lower {} (ms)", native.code.len()/4, ms[0], ms[1], ms[2], ms[3], ms[4]);
    }
}


fn random_program(seed: u64) -> NativeProgram {
    let mut r = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut rnd = |n: u64| { r ^= r << 13; r ^= r >> 7; r ^= r << 17; r % n };
    let mut words: Vec<u32> = Vec::new();
    words.push(s_mov_b32(5, 0x80));
    words.extend_from_slice(&s_load_dwordx4(8, 4));
    for i in 0..4u8 { words.push(vop1(0x001, i + 1, sgpr(i))); }
    let blocks = 1 + rnd(60) as usize;
    let ops = [0x25u32, 0x26, 0x1b, 0x1c, 0x1d];
    for _ in 0..blocks {
        if rnd(3) != 0 {
            let reg = rnd(6) as u32 + 1;
            words.push(0x7DA4_0080 | (reg << 9)); // v_cmpx_eq_u32 0, v[reg]
        }
        for _ in 0..rnd(20) {
            let op = ops[rnd(ops.len() as u64) as usize];
            let d = rnd(6) as u32 + 1;
            let s1 = rnd(6) as u32 + 1;
            // src0: a VGPR, or an inline constant (0x80..0xC0 integers)
            let s0 = if rnd(4) == 0 { 0x80 + rnd(16) as u32 } else { 256 + rnd(6) as u32 + 1 };
            words.push((op << 25) | (d << 17) | (s1 << 9) | s0);
        }
    }
    words.extend_from_slice(&buffer_store_dword(rnd(6) as u8 + 1, 8, 0));
    words.push(S_ENDPGM);
    let mut p = program(1, 1);
    p.code = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    p
}

fn try_translate(native: &NativeProgram) -> String {
    let Ok(p) = ir::build(&native.code, native.wave64, native.vgprs) else { return "ir-err".into() };
    let st = match structure::structure(&p) { Ok(s) => s, Err(e) => return format!("st-err {e:?}") };
    let res = match resource::track(&p, &native.layout, native.stage, Some(&st)) { Ok(r) => r, Err(e) => return format!("res-err {e:?}") };
    let io = match io::raise(&p, native) { Ok(i) => i, Err(e) => return format!("io-err {e:?}") };
    match lower::lower(&lower::Parts { native, p: &p, st: &st, io: &io, res: &res, inputs: Vec::new(), outputs: Vec::new(), prims: Vec::new(), mesh_stride: 0 }) {
        Ok(l) => format!("{:016x} {}", l.dxil.iter().fold(0xcbf29ce484222325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3)), l.dxil.len()),
        Err(e) => format!("lower-err {e:?}"),
    }
}

#[test]
#[ignore = "differential"]
fn dump_random_translations() {
    let out = std::env::var("DUMP").unwrap();
    let mut s = String::new();
    for seed in 0..600u64 {
        let native = random_program(seed);
        s += &format!("{seed} {}\n", try_translate(&native));
    }
    std::fs::write(out, s).unwrap();
}
