//! A small BC7 encoder: mode 6 only (one RGBA line per block, 7-bit
//! endpoints with a p-bit each, 4-bit indices). Good for clean UI art such
//! as line drawings; not a general-purpose encoder.

//! Ported from Marvisak/VIctory PR #3399 by anoyarou (GPL-3.0-or-later).

const WEIGHTS: [i32; 16] = [0, 4, 9, 13, 17, 21, 26, 30, 34, 38, 43, 47, 51, 55, 60, 64];

fn palette(e0: [i32; 4], e1: [i32; 4]) -> [[i32; 4]; 16] {
    let mut p = [[0; 4]; 16];
    for (i, w) in WEIGHTS.iter().enumerate() {
        for c in 0..4 {
            p[i][c] = ((64 - w) * e0[c] + w * e1[c] + 32) >> 6;
        }
    }
    p
}

fn dist(a: [i32; 4], b: [i32; 4]) -> i32 {
    (0..4).map(|c| (a[c] - b[c]) * (a[c] - b[c])).sum()
}

/// One block of 16 RGBA texels (row by row) to 16 bytes.
pub fn encode_block(px: &[[u8; 4]; 16]) -> [u8; 16] {
    let f: Vec<[f32; 4]> = px.iter().map(|p| p.map(f32::from)).collect();
    let mut mean = [0f32; 4];
    for p in &f {
        for c in 0..4 {
            mean[c] += p[c] / 16.0;
        }
    }
    // Principal axis by power iteration on the covariance, from the
    // widest channel range.
    let mut cov = [[0f32; 4]; 4];
    for p in &f {
        for i in 0..4 {
            for j in 0..4 {
                cov[i][j] += (p[i] - mean[i]) * (p[j] - mean[j]);
            }
        }
    }
    let mut axis = [0f32; 4];
    for c in 0..4 {
        let (lo, hi) = f
            .iter()
            .fold((f32::MAX, f32::MIN), |(l, h), p| (l.min(p[c]), h.max(p[c])));
        axis[c] = hi - lo;
    }
    for _ in 0..8 {
        let mut next = [0f32; 4];
        for i in 0..4 {
            next[i] = (0..4).map(|j| cov[i][j] * axis[j]).sum();
        }
        let len = next.iter().map(|v| v * v).sum::<f32>().sqrt();
        if len < 1e-6 {
            break;
        }
        axis = next.map(|v| v / len);
    }
    let t: Vec<f32> = f
        .iter()
        .map(|p| (0..4).map(|c| (p[c] - mean[c]) * axis[c]).sum())
        .collect();
    let (tmin, tmax) = t
        .iter()
        .fold((f32::MAX, f32::MIN), |(l, h), &v| (l.min(v), h.max(v)));
    let e0: [f32; 4] = std::array::from_fn(|c| mean[c] + axis[c] * tmin);
    let e1: [f32; 4] = std::array::from_fn(|c| mean[c] + axis[c] * tmax);
    let target: Vec<[i32; 4]> = px.iter().map(|p| p.map(i32::from)).collect();
    let mut best = (i32::MAX, [0; 4], [0; 4], 0, 0, [0u8; 16]);
    for p0 in 0..2 {
        for p1 in 0..2 {
            let q = |e: [f32; 4], p: i32| {
                e.map(|v| (((v - p as f32) / 2.0).round() as i32).clamp(0, 127))
            };
            let (q0, q1) = (q(e0, p0), q(e1, p1));
            let pal = palette(q0.map(|v| v * 2 + p0), q1.map(|v| v * 2 + p1));
            let mut idx = [0u8; 16];
            let mut err = 0;
            for (i, t) in target.iter().enumerate() {
                let (k, d) = (0..16)
                    .map(|k| (k, dist(*t, pal[k])))
                    .min_by_key(|x| x.1)
                    .unwrap();
                idx[i] = k as u8;
                err += d;
            }
            if err < best.0 {
                best = (err, q0, q1, p0, p1, idx);
            }
        }
    }
    let (_, mut q0, mut q1, mut p0, mut p1, mut idx) = best;
    if idx[0] >= 8 {
        // The first index is stored without its top bit: swap the ends.
        std::mem::swap(&mut q0, &mut q1);
        std::mem::swap(&mut p0, &mut p1);
        idx = idx.map(|i| 15 - i);
    }
    let mut v: u128 = 1 << 6;
    let mut pos = 7;
    for c in 0..4 {
        for q in [q0[c], q1[c]] {
            v |= (q as u128) << pos;
            pos += 7;
        }
    }
    v |= (p0 as u128) << pos;
    v |= (p1 as u128) << (pos + 1);
    pos += 2;
    v |= u128::from(idx[0]) << pos;
    pos += 3;
    for &i in &idx[1..] {
        v |= u128::from(i) << pos;
        pos += 4;
    }
    v.to_le_bytes()
}

/// An RGBA8 image (rows of `width` texels; width and height multiples of
/// 4) to BC7 blocks, row by row.
pub fn encode(rgba: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height);
    for by in (0..height).step_by(4) {
        for bx in (0..width).step_by(4) {
            let px: [[u8; 4]; 16] = std::array::from_fn(|i| {
                let at = ((by + i / 4) * width + bx + i % 4) * 4;
                rgba[at..at + 4].try_into().unwrap()
            });
            out.extend_from_slice(&encode_block(&px));
        }
    }
    out
}

/// Decodes a mode 6 block (the only mode [`encode`] writes).
pub fn decode_mode6(block: &[u8; 16]) -> Option<[[u8; 4]; 16]> {
    let v = u128::from_le_bytes(*block);
    if v & 0x7f != 1 << 6 {
        return None;
    }
    let field = |pos: u32, n: u32| ((v >> pos) & ((1u128 << n) - 1)) as i32;
    let (p0, p1) = (field(63, 1), field(64, 1));
    let e0: [i32; 4] = std::array::from_fn(|c| field(7 + 14 * c as u32, 7) * 2 + p0);
    let e1: [i32; 4] = std::array::from_fn(|c| field(14 + 14 * c as u32, 7) * 2 + p1);
    let pal = palette(e0, e1);
    Some(std::array::from_fn(|i| {
        let k = if i == 0 {
            field(65, 3)
        } else {
            field(68 + 4 * (i as u32 - 1), 4)
        };
        pal[k as usize].map(|c| c as u8)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_art_survives() {
        // A soft outer edge (one colour, alpha ramp) and a line across the
        // white body (opaque, dark to white).
        let edge: [[u8; 4]; 16] = std::array::from_fn(|i| [40, 40, 46, (i * 17) as u8]);
        let line: [[u8; 4]; 16] = std::array::from_fn(|i| {
            let t = (i % 4) as i32 * 68;
            [
                (40 + (204 * t) / 204) as u8,
                (40 + (204 * t) / 204) as u8,
                (46 + (201 * t) / 204) as u8,
                255,
            ]
        });
        for px in [edge, line] {
            let back = decode_mode6(&encode_block(&px)).unwrap();
            for (a, b) in px.iter().zip(&back) {
                assert!(
                    (0..4).all(|c| (i32::from(a[c]) - i32::from(b[c])).abs() <= 6),
                    "{a:?} {b:?}"
                );
            }
        }
        let flat = [[10, 200, 30, 255]; 16];
        let back = decode_mode6(&encode_block(&flat)).unwrap();
        assert!(
            back.iter()
                .all(|b| (0..4).all(|c| (i32::from(b[c]) - i32::from(flat[0][c])).abs() <= 2))
        );
    }
}
