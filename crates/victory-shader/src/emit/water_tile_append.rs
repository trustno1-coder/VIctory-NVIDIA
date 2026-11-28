//! Lower the wave-wide allocation of two transmission tile lists on wave32.

use std::ops::Range;

use crate::native::{HwStage, NativeProgram};
use crate::ShaderError;

pub(super) const CONTAINER_ID: [u8; 16] = [
    0xf5, 0xd1, 0x0c, 0xde, 0x74, 0x33, 0x37, 0x82, 0xf3, 0x0f, 0x9b, 0xb5, 0x56, 0xc0, 0x73, 0xd4,
];
const PROGRAM_ID: u64 = 0x377a_8d97_4fb8_631f;
const PROGRAM_BYTES: usize = 2112;

pub(super) fn rewrite(native: &mut NativeProgram) -> Result<(), ShaderError> {
    if native.stage != HwStage::Cs || !native.wave64 || native.code.len() != PROGRAM_BYTES {
        return Err(ShaderError::Malformed("unexpected transmission tile program"));
    }
    let code = &mut native.code;
    let fingerprint = code.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    if fingerprint != PROGRAM_ID {
        return Err(ShaderError::Malformed("unexpected transmission tile program body"));
    }

    // The incoming EXEC masks already select the two disjoint classes. Keep
    // those masks and the original counters, but reserve one slot per lane.
    // The original v_mov_b32 source is inline constant 0; its low bit makes
    // it inline constant 1. Keep every address, atomic, and wait from the
    // input program. Its branch targets and index reversal stay in place.
    let mut unit_move: [u8; 4] = code[0x6c4..0x6c8].try_into().unwrap();
    unit_move[0] |= 1;
    let nop: [u8; 4] = code[0x6d8..0x6dc].try_into().unwrap();
    inline_append(code, 0x6a8..0x704, unit_move, nop, &[0x6dc..0x6e4, 0x6f0..0x6f4]);
    inline_append(
        code,
        0x70c..0x774,
        unit_move,
        nop,
        &[0x740..0x748, 0x74c..0x754, 0x75c..0x760, 0x770..0x774],
    );
    Ok(())
}

fn inline_append(
    code: &mut [u8],
    body: Range<usize>,
    unit_move: [u8; 4],
    nop: [u8; 4],
    kept: &[Range<usize>],
) {
    let mut append = Vec::with_capacity(body.len());
    append.extend_from_slice(&unit_move);
    for range in kept {
        append.extend_from_slice(&code[range.clone()]);
    }
    let written = append.len();
    let body = &mut code[body];
    body[..written].copy_from_slice(&append);
    for slot in body[written..].chunks_exact_mut(4) {
        slot.copy_from_slice(&nop);
    }
}
