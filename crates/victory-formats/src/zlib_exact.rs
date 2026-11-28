//! zlib streams of an exact length, for archive entries rebuilt in place: an
//! entry keeps its offset and its stored length (an archive's table, often
//! encrypted, is not rewritten), so a new payload must compress to exactly
//! that many bytes. A small inflater of our own also finds where a stream
//! ends and where its last block starts.

//! Ported from Marvisak/VIctory PR #3399 by anoyarou (GPL-3.0-or-later).

/// Adler-32 (RFC 1950).
pub fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += u32::from(x);
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    b << 16 | a
}

struct Bits<'a> {
    b: &'a [u8],
    pos: usize, // in bits
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let v = (self.b.get(self.pos >> 3)? >> (self.pos & 7)) & 1;
        self.pos += 1;
        Some(u32::from(v))
    }
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for i in 0..n {
            v |= self.bit()? << i;
        }
        Some(v)
    }
}

/// A canonical Huffman code (as in zlib's puff).
struct Huffman {
    count: [u16; 16],
    symbol: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Self {
        let mut count = [0u16; 16];
        for &l in lengths {
            count[usize::from(l)] += 1;
        }
        count[0] = 0;
        let mut offs = [0u16; 16];
        for i in 1..16 {
            offs[i] = offs[i - 1] + count[i - 1];
        }
        let mut symbol = vec![0; lengths.len()];
        for (s, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbol[usize::from(offs[usize::from(l)])] = s as u16;
                offs[usize::from(l)] += 1;
            }
        }
        Huffman { count, symbol }
    }
    fn decode(&self, b: &mut Bits) -> Option<u16> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= b.bit()? as i32;
            let count = i32::from(self.count[len]);
            if code - count < first {
                return self.symbol.get((index + code - first) as usize).copied();
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        None
    }
}

const LBASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEXT: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DBASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DEXT: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// Where a raw deflate stream's last block header is and where the stream
/// ends, in bits.
struct Walk {
    last_block: usize,
    end: usize,
}

/// Inflates a raw deflate stream from bit `start` into `out`, stopping with
/// `Err(true)` once `out` would pass `limit` bytes; `Err(false)` if the
/// stream is broken.
fn inflate_raw(stream: &[u8], start: usize, out: &mut Vec<u8>, limit: usize) -> Result<Walk, bool> {
    let mut b = Bits {
        b: stream,
        pos: start,
    };
    loop {
        let header = b.pos;
        let last = b.bit().ok_or(false)? == 1;
        match b.bits(2).ok_or(false)? {
            0 => {
                b.pos = (b.pos + 7) & !7;
                let len = b.bits(16).ok_or(false)? as usize;
                if b.bits(16).ok_or(false)? as usize != len ^ 0xffff {
                    return Err(false);
                }
                let at = b.pos >> 3;
                let bytes = stream.get(at..at + len).ok_or(false)?;
                if out.len() + len > limit {
                    return Err(true);
                }
                out.extend_from_slice(bytes);
                b.pos += 8 * len;
            }
            kind @ (1 | 2) => {
                let (lit, dist) = if kind == 1 {
                    let mut l = [8u8; 288];
                    l[144..256].fill(9);
                    l[256..280].fill(7);
                    (Huffman::new(&l), Huffman::new(&[5; 30]))
                } else {
                    let nlen = b.bits(5).ok_or(false)? as usize + 257;
                    let ndist = b.bits(5).ok_or(false)? as usize + 1;
                    let ncode = b.bits(4).ok_or(false)? as usize + 4;
                    const ORDER: [usize; 19] = [
                        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
                    ];
                    let mut cl = [0u8; 19];
                    for &o in &ORDER[..ncode] {
                        cl[o] = b.bits(3).ok_or(false)? as u8;
                    }
                    let code = Huffman::new(&cl);
                    let mut lengths = Vec::with_capacity(nlen + ndist);
                    while lengths.len() < nlen + ndist {
                        match code.decode(&mut b).ok_or(false)? {
                            s @ 0..=15 => lengths.push(s as u8),
                            16 => {
                                let prev = *lengths.last().ok_or(false)?;
                                let n = 3 + b.bits(2).ok_or(false)?;
                                lengths.extend(std::iter::repeat_n(prev, n as usize));
                            }
                            17 => lengths.extend(std::iter::repeat_n(
                                0,
                                3 + b.bits(3).ok_or(false)? as usize,
                            )),
                            _ => lengths.extend(std::iter::repeat_n(
                                0,
                                11 + b.bits(7).ok_or(false)? as usize,
                            )),
                        }
                    }
                    if lengths.len() != nlen + ndist {
                        return Err(false);
                    }
                    (
                        Huffman::new(&lengths[..nlen]),
                        Huffman::new(&lengths[nlen..]),
                    )
                };
                loop {
                    let s = usize::from(lit.decode(&mut b).ok_or(false)?);
                    if s < 256 {
                        if out.len() == limit {
                            return Err(true);
                        }
                        out.push(s as u8);
                    } else if s == 256 {
                        break;
                    } else {
                        let i = s - 257;
                        let len = usize::from(*LBASE.get(i).ok_or(false)?)
                            + b.bits(u32::from(LEXT[i])).ok_or(false)? as usize;
                        let d = usize::from(dist.decode(&mut b).ok_or(false)?);
                        let back = usize::from(*DBASE.get(d).ok_or(false)?)
                            + b.bits(u32::from(DEXT[d])).ok_or(false)? as usize;
                        if back > out.len() {
                            return Err(false);
                        }
                        if out.len() + len > limit {
                            return Err(true);
                        }
                        for _ in 0..len {
                            out.push(out[out.len() - back]);
                        }
                    }
                }
            }
            _ => return Err(false),
        }
        if last {
            return Ok(Walk {
                last_block: header,
                end: b.pos,
            });
        }
    }
}

fn zlib_header_ok(stream: &[u8]) -> bool {
    matches!(stream, [cmf, flg, ..] if cmf & 0x0f == 8 && cmf >> 4 <= 7 && flg & 0x20 == 0
        && (u16::from(*cmf) << 8 | u16::from(*flg)) % 31 == 0)
}

/// A whole zlib stream inflated, and its length in bytes.
#[derive(Debug, PartialEq, Eq)]
pub struct Inflated {
    pub data: Vec<u8>,
    pub stream_len: usize,
}

/// Inflates a zlib stream of at most `limit` bytes of output, checking its
/// Adler-32; `None` if it is not one, is broken or is longer.
pub fn inflate(stream: &[u8], limit: usize) -> Option<Inflated> {
    if !zlib_header_ok(stream) {
        return None;
    }
    let mut data = Vec::new();
    let walk = inflate_raw(stream, 16, &mut data, limit).ok()?;
    let at = walk.end.div_ceil(8);
    let check = u32::from_be_bytes(stream.get(at..at + 4)?.try_into().ok()?);
    (check == adler32(&data)).then_some(Inflated {
        data,
        stream_len: at + 4,
    })
}

/// The first bytes (at most `n`) a zlib stream inflates to, or `None` if it
/// does not start as one.
pub fn inflate_prefix(stream: &[u8], n: usize) -> Option<Vec<u8>> {
    if !zlib_header_ok(stream) {
        return None;
    }
    let mut data = Vec::new();
    match inflate_raw(stream, 16, &mut data, n) {
        Ok(_) | Err(true) => Some(data),
        Err(false) => None,
    }
}

/// A raw deflate encoder for [`exact`].
pub type Deflater = fn(&[u8]) -> Vec<u8>;

/// Zopfli: small output, slow; for small entries with little room.
pub fn zopfli(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    zopfli::compress(
        zopfli::Options::default(),
        zopfli::Format::Deflate,
        data,
        &mut out,
    )
    .expect("in memory");
    out
}

/// zlib-rs at level 9: quick; for large entries with room to spare. Raw
/// deflate, as [`zopfli`] gives: `split_stream` adds the zlib wrapper.
pub fn fast(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; zlib_rs::compress_bound(data.len())];
    let config = zlib_rs::DeflateConfig {
        window_bits: -15,
        ..zlib_rs::DeflateConfig::new(9)
    };
    let (packed, rc) = zlib_rs::compress_slice(&mut out, data, config);
    assert_eq!(rc, zlib_rs::ReturnCode::Ok);
    let len = packed.len();
    out.truncate(len);
    out
}

/// The zlib stream for `data[..k]` deflated and `data[k..]` in stored
/// blocks.
fn split_stream(data: &[u8], k: usize, deflate: Deflater) -> Option<Vec<u8>> {
    let mut raw = deflate(&data[..k]);
    let walk = inflate_raw(&raw, 0, &mut Vec::new(), usize::MAX).ok()?;
    raw[walk.last_block >> 3] &= !(1 << (walk.last_block & 7));
    let end = walk.end;
    raw.truncate(end.div_ceil(8));
    if end & 7 != 0 {
        let last = raw.len() - 1;
        raw[last] &= (1 << (end & 7)) - 1;
    }
    let tail = &data[k..];
    let chunks: Vec<&[u8]> = if tail.is_empty() {
        vec![&[][..]]
    } else {
        tail.chunks(65535).collect()
    };
    // The first stored block's 3 header bits follow the deflated blocks.
    raw.resize((end + 3).div_ceil(8), 0);
    if chunks.len() == 1 {
        raw[end >> 3] |= 1 << (end & 7);
    }
    let mut out = vec![0x78, 0xda];
    out.extend_from_slice(&raw);
    for (i, c) in chunks.iter().enumerate() {
        if i > 0 {
            out.push(u8::from(i == chunks.len() - 1));
        }
        out.extend_from_slice(&(c.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(c.len() as u16)).to_le_bytes());
        out.extend_from_slice(c);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    Some(out)
}

/// A zlib stream of `data` exactly `target` bytes long: `data[..k]`
/// deflated, the rest in stored blocks, `k` searched. `None` if `data`
/// does not fit.
pub fn exact(data: &[u8], target: usize, deflate: Deflater) -> Option<Vec<u8>> {
    let n = data.len();
    let mut tried = std::collections::BTreeMap::new();
    let mut k = n;
    for _ in 0..64 {
        if tried.contains_key(&k) {
            break;
        }
        let s = split_stream(data, k, deflate)?;
        tried.insert(k, s.len());
        if s.len() == target {
            return Some(s);
        }
        let next = if s.len() < target {
            k.saturating_sub(target - s.len())
        } else {
            k + ((s.len() - target) / 2).max(1)
        };
        let next = next.min(n);
        if next == k {
            break;
        }
        k = next;
    }
    // Step one byte at a time down from the smallest split that was short.
    let k0 = tried
        .iter()
        .filter(|&(_, &len)| len < target)
        .map(|(&k, _)| k)
        .min()?;
    (k0.saturating_sub(512)..k0)
        .rev()
        .find_map(|k| split_stream(data, k, deflate).filter(|s| s.len() == target))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(n: usize) -> Vec<u8> {
        let mut x = 0x1234_5678u32;
        (0..n)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                if i % 7 < 3 {
                    (x >> 24) as u8
                } else {
                    (i / 50) as u8
                }
            })
            .collect()
    }

    #[test]
    fn our_inflater_reads_zopfli_and_stored() {
        let data = sample(40_000);
        let mut z = vec![0x78, 0xda];
        z.extend(zopfli(&data));
        z.extend(adler32(&data).to_be_bytes());
        let got = inflate(&z, usize::MAX).unwrap();
        assert_eq!((got.data == data, got.stream_len), (true, z.len()));
        assert!(inflate(&z, 39_999).is_none());
        assert_eq!(inflate_prefix(&z, 10).unwrap(), data[..10]);
    }

    #[test]
    fn exact_streams_hit_the_length() {
        let mut data = sample(30_000);
        data.extend(std::iter::repeat_n(0, 2000));
        let base = split_stream(&data, data.len(), zopfli).unwrap().len();
        for target in [base + 1, base + 7, base + 300, base + 10_000] {
            let s = exact(&data, target, zopfli).unwrap();
            assert_eq!(s.len(), target);
            assert_eq!(inflate(&s, usize::MAX).unwrap().data, data);
        }
        assert!(exact(&data, base - 200, zopfli).is_none());
        let s = exact(&data, base + 2000, fast).unwrap();
        assert_eq!(
            (s.len(), inflate(&s, usize::MAX).unwrap().data == data),
            (base + 2000, true)
        );
    }
}
