//! SWF / Scaleform GFx movies, uncompressed body (`FWS` / `GFX`): tags,
//! vector shapes, the first frame of sprites, symbol names, and a
//! `DefineShape3` writer. Enough to replace shapes in a movie and keep every
//! other byte as it was.

//! Ported from Marvisak/VIctory PR #3399 by anoyarou (GPL-3.0-or-later).

use std::collections::HashMap;
use std::ops::Range;

pub const END: u16 = 0;
pub const SHOW_FRAME: u16 = 1;
pub const PLACE_OBJECT2: u16 = 26;
pub const DEFINE_SPRITE: u16 = 39;
pub const EXPORT_ASSETS: u16 = 56;
pub const PLACE_OBJECT3: u16 = 70;
pub const SYMBOL_CLASS: u16 = 76;
pub const DEFINE_SHAPE3: u16 = 32;

/// Whether `code` defines a shape (DefineShape 1 to 4).
pub fn is_shape(code: u16) -> bool {
    matches!(code, 2 | 22 | 32 | 83)
}

/// A tag: its code, where its header starts and where its body lies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    pub code: u16,
    pub start: usize,
    pub body: Range<usize>,
}

/// Whether `movie` starts as an uncompressed SWF or GFx movie.
pub fn is_movie(movie: &[u8]) -> bool {
    movie.len() >= 9 && (movie.starts_with(b"FWS") || movie.starts_with(b"GFX"))
}

/// The length the header gives the movie.
pub fn declared_len(movie: &[u8]) -> Option<usize> {
    Some(u32::from_le_bytes(movie.get(4..8)?.try_into().ok()?) as usize)
}

/// Where the first tag starts: after the signature, version, length, frame
/// rectangle, frame rate and frame count.
pub fn first_tag(movie: &[u8]) -> Option<usize> {
    let mut b = Bits::new(movie, 8);
    b.rect()?;
    Some(b.byte_pos() + 4)
}

/// The tags from `at` up to and including `End` (or `end`).
pub fn tags(movie: &[u8], mut at: usize, end: usize) -> Option<Vec<Tag>> {
    let mut out = Vec::new();
    while at < end {
        let start = at;
        let head = u16::from_le_bytes(movie.get(at..at + 2)?.try_into().ok()?);
        at += 2;
        let mut len = usize::from(head & 63);
        if len == 63 {
            len = u32::from_le_bytes(movie.get(at..at + 4)?.try_into().ok()?) as usize;
            at += 4;
        }
        let body = at..at.checked_add(len).filter(|&e| e <= movie.len())?;
        at = body.end;
        let code = head >> 6;
        out.push(Tag { code, start, body });
        if code == END {
            break;
        }
    }
    Some(out)
}

/// The top-level tags of a movie.
pub fn movie_tags(movie: &[u8]) -> Option<Vec<Tag>> {
    tags(movie, first_tag(movie)?, movie.len())
}

/// Reads bits most significant first, as SWF stores them.
struct Bits<'a> {
    b: &'a [u8],
    pos: usize,
    bit: u32,
}

impl<'a> Bits<'a> {
    fn new(b: &'a [u8], pos: usize) -> Self {
        Bits { b, pos, bit: 0 }
    }
    fn byte_pos(&self) -> usize {
        self.pos + usize::from(self.bit != 0)
    }
    fn align(&mut self) {
        if self.bit != 0 {
            self.bit = 0;
            self.pos += 1;
        }
    }
    fn ub(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.b.get(self.pos)?;
            v = (v << 1) | u32::from((byte >> (7 - self.bit)) & 1);
            self.bit += 1;
            if self.bit == 8 {
                self.bit = 0;
                self.pos += 1;
            }
        }
        Some(v)
    }
    fn sb(&mut self, n: u32) -> Option<i32> {
        if n == 0 {
            return Some(0);
        }
        let v = self.ub(n)?;
        Some(if n < 32 && v & (1 << (n - 1)) != 0 {
            (i64::from(v) - (1i64 << n)) as i32
        } else {
            v as i32
        })
    }
    fn u8(&mut self) -> Option<u8> {
        self.align();
        let v = *self.b.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from(self.u8()?) | u16::from(self.u8()?) << 8)
    }
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        self.align();
        let s = self.b.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(s)
    }
    fn rect(&mut self) -> Option<[i32; 4]> {
        self.align();
        let n = self.ub(5)?;
        let r = [self.sb(n)?, self.sb(n)?, self.sb(n)?, self.sb(n)?];
        self.align();
        Some(r)
    }
    fn matrix(&mut self) -> Option<()> {
        self.align();
        if self.ub(1)? == 1 {
            let n = self.ub(5)?;
            self.ub(2 * n)?;
        }
        if self.ub(1)? == 1 {
            let n = self.ub(5)?;
            self.ub(2 * n)?;
        }
        let n = self.ub(5)?;
        self.ub(2 * n)?;
        self.align();
        Some(())
    }
    fn string(&mut self) -> Option<&'a [u8]> {
        self.align();
        let rest = self.b.get(self.pos..)?;
        let len = rest.iter().position(|&c| c == 0)?;
        self.pos += len + 1;
        Some(&rest[..len])
    }
}

/// A point in twips (1/20 pixel).
pub type Point = (i32, i32);

/// One edge of a shape, with the styles in force on it. `moved`: a move
/// came before it (it starts a new sub-path).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    pub from: Point,
    pub ctrl: Option<Point>,
    pub to: Point,
    pub fill0: u16,
    pub fill1: u16,
    pub line: u16,
    pub moved: bool,
}

/// A fill style; only solid colours are told apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    Solid([u8; 4]),
    Other,
}

/// A shape: its id, bounds (`[x_min, x_max, y_min, y_max]`, twips), fill
/// styles (all of them, also those of later style groups; edges index them
/// from 1) and edges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shape {
    pub id: u16,
    pub bounds: [i32; 4],
    pub fills: Vec<Fill>,
    pub edges: Vec<Edge>,
}

fn read_fills(b: &mut Bits, version: u8) -> Option<Vec<Fill>> {
    let mut n = usize::from(b.u8()?);
    if n == 0xff {
        n = usize::from(b.u16()?);
    }
    let color = |b: &mut Bits| -> Option<[u8; 4]> {
        let c = b.bytes(if version >= 3 { 4 } else { 3 })?;
        Some([c[0], c[1], c[2], c.get(3).copied().unwrap_or(255)])
    };
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        match b.u8()? {
            0x00 => out.push(Fill::Solid(color(b)?)),
            kind @ (0x10 | 0x12 | 0x13) => {
                b.matrix()?;
                let count = b.u8()? & 15;
                for _ in 0..count {
                    b.u8()?;
                    color(b)?;
                }
                if kind == 0x13 {
                    b.u16()?;
                }
                out.push(Fill::Other);
            }
            0x40..=0x43 => {
                b.u16()?;
                b.matrix()?;
                out.push(Fill::Other);
            }
            _ => return None,
        }
    }
    Some(out)
}

fn skip_lines(b: &mut Bits, version: u8) -> Option<usize> {
    let mut n = usize::from(b.u8()?);
    if n == 0xff {
        n = usize::from(b.u16()?);
    }
    for _ in 0..n {
        b.u16()?;
        if version == 4 {
            let flags = b.u16()?;
            let join = (flags >> 12) & 3;
            let has_fill = flags & (1 << 11) != 0;
            if join == 2 {
                b.u16()?;
            }
            if has_fill {
                read_fills(
                    &mut Bits {
                        b: b.b,
                        pos: b.pos - 1,
                        bit: 0,
                    },
                    4,
                )?;
                return None; // a line drawn with a fill style: not needed here
            }
            b.bytes(4)?;
        } else {
            b.bytes(if version >= 3 { 4 } else { 3 })?;
        }
    }
    Some(n)
}

/// Parses a DefineShape tag body (`code` 2, 22, 32 or 83).
pub fn parse_shape(code: u16, body: &[u8]) -> Option<Shape> {
    let version = match code {
        2 => 1,
        22 => 2,
        32 => 3,
        83 => 4,
        _ => return None,
    };
    let mut b = Bits::new(body, 0);
    let id = b.u16()?;
    let bounds = b.rect()?;
    if version == 4 {
        b.rect()?;
        b.u8()?;
    }
    let mut fills = read_fills(&mut b, version)?;
    let mut lines = skip_lines(&mut b, version)?;
    b.align();
    let (mut nf, mut nl) = (b.ub(4)?, b.ub(4)?);
    let (mut x, mut y) = (0i32, 0i32);
    let (mut f0, mut f1, mut ln) = (0u16, 0u16, 0u16);
    let (mut fill_base, mut line_base) = (0u16, 0u16);
    let mut moved = false;
    let mut edges = Vec::new();
    loop {
        if b.ub(1)? == 0 {
            let flags = b.ub(5)?;
            if flags == 0 {
                break;
            }
            if flags & 1 != 0 {
                let n = b.ub(5)?;
                x = b.sb(n)?;
                y = b.sb(n)?;
                moved = true;
            }
            let style = |v: u32, base: u16| if v == 0 { 0 } else { v as u16 + base };
            if flags & 2 != 0 {
                f0 = style(b.ub(nf)?, fill_base);
            }
            if flags & 4 != 0 {
                f1 = style(b.ub(nf)?, fill_base);
            }
            if flags & 8 != 0 {
                ln = style(b.ub(nl)?, line_base);
            }
            if flags & 16 != 0 {
                fill_base = fills.len() as u16;
                line_base = lines as u16;
                fills.extend(read_fills(&mut b, version)?);
                lines += skip_lines(&mut b, version)?;
                b.align();
                nf = b.ub(4)?;
                nl = b.ub(4)?;
            }
        } else if b.ub(1)? == 1 {
            let n = b.ub(4)? + 2;
            let (dx, dy) = if b.ub(1)? == 1 {
                (b.sb(n)?, b.sb(n)?)
            } else if b.ub(1)? == 1 {
                (0, b.sb(n)?)
            } else {
                (b.sb(n)?, 0)
            };
            let to = (x + dx, y + dy);
            edges.push(Edge {
                from: (x, y),
                ctrl: None,
                to,
                fill0: f0,
                fill1: f1,
                line: ln,
                moved,
            });
            (x, y) = to;
            moved = false;
        } else {
            let n = b.ub(4)? + 2;
            let c = (x + b.sb(n)?, y + b.sb(n)?);
            let to = (c.0 + b.sb(n)?, c.1 + b.sb(n)?);
            edges.push(Edge {
                from: (x, y),
                ctrl: Some(c),
                to,
                fill0: f0,
                fill1: f1,
                line: ln,
                moved,
            });
            (x, y) = to;
            moved = false;
        }
    }
    Some(Shape {
        id,
        bounds,
        fills,
        edges,
    })
}

/// One object placed by a sprite's first frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub depth: u16,
    pub character: Option<u16>,
}

/// A DefineSprite body: its id and what its first frame places, by depth.
pub fn sprite_places(movie: &[u8], body: Range<usize>) -> Option<(u16, Vec<Place>)> {
    let id = u16::from_le_bytes(movie.get(body.start..body.start + 2)?.try_into().ok()?);
    let mut places: Vec<Place> = Vec::new();
    for tag in tags(movie, body.start + 4, body.end)? {
        match tag.code {
            SHOW_FRAME | END => break,
            PLACE_OBJECT2 | PLACE_OBJECT3 => {
                let mut b = Bits::new(&movie[tag.body.clone()], 0);
                let flags = b.u8()?;
                let flags2 = if tag.code == PLACE_OBJECT3 {
                    b.u8()?
                } else {
                    0
                };
                let depth = b.u16()?;
                if tag.code == PLACE_OBJECT3
                    && (flags2 & 8 != 0 || (flags2 & 16 != 0 && flags & 2 != 0))
                {
                    b.string()?;
                }
                let character = if flags & 2 != 0 { Some(b.u16()?) } else { None };
                match places.iter_mut().find(|p| p.depth == depth) {
                    Some(p) if character.is_some() => p.character = character,
                    Some(_) => {}
                    None => places.push(Place { depth, character }),
                }
            }
            _ => {}
        }
    }
    places.sort_by_key(|p| p.depth);
    Some((id, places))
}

/// The names an `ExportAssets` or `SymbolClass` body gives characters.
pub fn symbol_names(body: &[u8]) -> Option<Vec<(u16, String)>> {
    let mut b = Bits::new(body, 0);
    let n = b.u16()?;
    let mut out = Vec::with_capacity(usize::from(n));
    for _ in 0..n {
        let id = b.u16()?;
        out.push((id, String::from_utf8_lossy(b.string()?).into_owned()));
    }
    Some(out)
}

/// Writes bits most significant first.
#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    acc: u8,
    n: u32,
}

impl BitWriter {
    fn ub(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.acc = (self.acc << 1) | ((v >> i) & 1) as u8;
            self.n += 1;
            if self.n == 8 {
                self.out.push(self.acc);
                self.acc = 0;
                self.n = 0;
            }
        }
    }
    fn sb(&mut self, v: i32, n: u32) {
        self.ub(v as u32 & ((1u64 << n) - 1) as u32, n);
    }
    fn align(&mut self) {
        if self.n != 0 {
            self.ub(0, 8 - self.n);
        }
    }
    fn bytes(&mut self, b: &[u8]) {
        self.align();
        self.out.extend_from_slice(b);
    }
}

/// Bits a signed field needs for all of `values`.
fn signed_bits(values: &[i32]) -> u32 {
    values
        .iter()
        .map(|&v| {
            33 - if v < 0 {
                (!v).leading_zeros()
            } else {
                v.leading_zeros()
            }
        })
        .max()
        .unwrap_or(1)
        .max(1)
}

/// A whole `DefineShape3` tag: one solid fill `color` and no lines. Edges
/// use fill style 1 or 0 on each side and must run as contiguous paths (a
/// move is written wherever an edge does not start where the last ended).
pub fn define_shape3(id: u16, color: [u8; 4], edges: &[Edge]) -> Vec<u8> {
    let mut w = BitWriter::default();
    w.bytes(&id.to_le_bytes());
    let pts = edges
        .iter()
        .flat_map(|e| [Some(e.from), e.ctrl, Some(e.to)])
        .flatten();
    let (mut x0, mut x1, mut y0, mut y1) = (i32::MAX, i32::MIN, i32::MAX, i32::MIN);
    for (x, y) in pts {
        (x0, x1, y0, y1) = (x0.min(x), x1.max(x), y0.min(y), y1.max(y));
    }
    if edges.is_empty() {
        (x0, x1, y0, y1) = (0, 0, 0, 0);
    }
    let n = signed_bits(&[x0, x1, y0, y1]);
    w.ub(n, 5);
    for v in [x0, x1, y0, y1] {
        w.sb(v, n);
    }
    w.align();
    w.bytes(&[1, 0x00]);
    w.bytes(&color);
    w.bytes(&[0]);
    w.ub(1, 4);
    w.ub(0, 4);
    let (mut at, mut f0, mut f1) = (None, 0u16, 0u16);
    for e in edges {
        let move_to = at != Some(e.from);
        let (set0, set1) = (e.fill0 != f0, e.fill1 != f1);
        if move_to || set0 || set1 {
            w.ub(0, 1);
            w.ub(
                u32::from(set1) << 2 | u32::from(set0) << 1 | u32::from(move_to),
                5,
            );
            if move_to {
                let n = signed_bits(&[e.from.0, e.from.1]);
                w.ub(n, 5);
                w.sb(e.from.0, n);
                w.sb(e.from.1, n);
            }
            if set0 {
                w.ub(u32::from(e.fill0 != 0), 1);
            }
            if set1 {
                w.ub(u32::from(e.fill1 != 0), 1);
            }
            (f0, f1) = (e.fill0, e.fill1);
        }
        match e.ctrl {
            None => {
                let (dx, dy) = (e.to.0 - e.from.0, e.to.1 - e.from.1);
                if dx == 0 && dy == 0 {
                    at = Some(e.to);
                    continue;
                }
                let n = signed_bits(&[dx, dy]).max(2);
                w.ub(0b11, 2);
                w.ub(n - 2, 4);
                if dx != 0 && dy != 0 {
                    w.ub(1, 1);
                    w.sb(dx, n);
                    w.sb(dy, n);
                } else if dx != 0 {
                    w.ub(0b00, 2);
                    w.sb(dx, n);
                } else {
                    w.ub(0b01, 2);
                    w.sb(dy, n);
                }
            }
            Some(c) => {
                let v = [c.0 - e.from.0, c.1 - e.from.1, e.to.0 - c.0, e.to.1 - c.1];
                let n = signed_bits(&v).max(2);
                w.ub(0b10, 2);
                w.ub(n - 2, 4);
                for d in v {
                    w.sb(d, n);
                }
            }
        }
        at = Some(e.to);
    }
    w.ub(0, 6);
    w.align();
    let body = w.out;
    let mut tag = Vec::with_capacity(body.len() + 6);
    tag.extend_from_slice(&(DEFINE_SHAPE3 << 6 | 63).to_le_bytes());
    tag.extend_from_slice(&(body.len() as u32).to_le_bytes());
    tag.extend_from_slice(&body);
    tag
}

/// The movie with the shape tags whose ids are in `new` replaced by those
/// whole tags; everything else is copied. Ends after `End`.
pub fn replace_shapes(movie: &[u8], new: &HashMap<u16, Vec<u8>>) -> Option<Vec<u8>> {
    let first = first_tag(movie)?;
    let mut out = movie.get(..first)?.to_vec();
    for tag in tags(movie, first, movie.len())? {
        let id = movie
            .get(tag.body.start..tag.body.start + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]));
        match id
            .filter(|_| is_shape(tag.code))
            .and_then(|id| new.get(&id))
        {
            Some(bytes) => out.extend_from_slice(bytes),
            None => out.extend_from_slice(&movie[tag.start..tag.body.end]),
        }
    }
    Some(out)
}

/// `movie` padded with zeros after `End` to `len` bytes, with that length in
/// its header; `None` if it is longer.
pub fn with_length(mut movie: Vec<u8>, len: usize) -> Option<Vec<u8>> {
    if movie.len() > len || movie.len() < 8 {
        return None;
    }
    movie.resize(len, 0);
    movie[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    Some(movie)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(code: u16, body: &[u8]) -> Vec<u8> {
        let mut t = (code << 6 | 63).to_le_bytes().to_vec();
        t.extend_from_slice(&(body.len() as u32).to_le_bytes());
        t.extend_from_slice(body);
        t
    }

    fn edges(points: &[Point], curved: bool) -> Vec<Edge> {
        (0..points.len())
            .map(|i| {
                let (a, b) = (points[i], points[(i + 1) % points.len()]);
                let ctrl =
                    (curved && i % 2 == 0).then_some(((a.0 + b.0) / 2 + 40, (a.1 + b.1) / 2 - 40));
                Edge {
                    from: a,
                    ctrl,
                    to: b,
                    fill0: 1,
                    fill1: 0,
                    line: 0,
                    moved: i == 0,
                }
            })
            .collect()
    }

    /// A small movie of our own: two shapes, a sprite placing them, names.
    pub(crate) fn movie() -> Vec<u8> {
        let mut m = b"GFX\x08\0\0\0\0".to_vec();
        let mut w = BitWriter::default();
        w.ub(16, 5);
        for v in [0, 12800, 0, 9600] {
            w.sb(v, 16);
        }
        w.align();
        m.extend_from_slice(&w.out);
        m.extend_from_slice(&[0, 30, 1, 0]);
        m.extend(define_shape3(
            1,
            [255, 255, 255, 255],
            &edges(&[(0, 0), (1280, 0), (1280, 1280), (0, 1280)], true),
        ));
        m.extend(define_shape3(
            2,
            [0, 0, 0, 255],
            &edges(&[(300, 300), (900, 310), (600, 1000)], false),
        ));
        let mut sprite = vec![3, 0, 1, 0];
        for (depth, ch) in [(1u16, 1u16), (2, 2)] {
            let mut p = vec![2];
            p.extend_from_slice(&depth.to_le_bytes());
            p.extend_from_slice(&ch.to_le_bytes());
            sprite.extend(tag(PLACE_OBJECT2, &p));
        }
        sprite.extend(tag(SHOW_FRAME, &[]));
        sprite.extend(tag(END, &[]));
        m.extend(tag(DEFINE_SPRITE, &sprite));
        m.extend(tag(SYMBOL_CLASS, b"\x01\x00\x03\x00pkg.thing_7\0"));
        m.extend(tag(SHOW_FRAME, &[]));
        m.extend(tag(END, &[]));
        let len = m.len() as u32;
        m[4..8].copy_from_slice(&len.to_le_bytes());
        m
    }

    #[test]
    fn shapes_written_read_back() {
        let e = edges(&[(-500, 20), (700, -40), (1900, 2000), (3, 77)], true);
        let t = define_shape3(9, [1, 2, 3, 4], &e);
        let tags = tags(&t, 0, t.len()).unwrap();
        let s = parse_shape(tags[0].code, &t[tags[0].body.clone()]).unwrap();
        assert_eq!(s.id, 9);
        assert_eq!(s.fills, vec![Fill::Solid([1, 2, 3, 4])]);
        assert_eq!(s.edges, e);
    }

    #[test]
    fn movie_parts_found_and_shapes_replaced() {
        let m = movie();
        assert!(is_movie(&m));
        let tags = movie_tags(&m).unwrap();
        let sprite = tags.iter().find(|t| t.code == DEFINE_SPRITE).unwrap();
        let (id, places) = sprite_places(&m, sprite.body.clone()).unwrap();
        assert_eq!((id, places.len(), places[1].character), (3, 2, Some(2)));
        let names = tags.iter().find(|t| t.code == SYMBOL_CLASS).unwrap();
        assert_eq!(
            symbol_names(&m[names.body.clone()]).unwrap(),
            vec![(3, "pkg.thing_7".to_string())]
        );
        let new = define_shape3(
            2,
            [0, 0, 0, 255],
            &edges(&[(400, 400), (800, 400), (800, 800)], false),
        );
        let out = replace_shapes(&m, &HashMap::from([(2, new.clone())])).unwrap();
        let out = with_length(out, m.len() + 64).unwrap();
        assert_eq!(declared_len(&out), Some(m.len() + 64));
        let after = movie_tags(&out).unwrap();
        assert_eq!(after.len(), tags.len());
        for (a, b) in tags.iter().zip(&after) {
            let same = m[a.start..a.body.end] == out[b.start..b.body.end];
            let replaced = out[b.start..b.body.end] == new[..];
            assert!(same || replaced);
        }
    }
}
