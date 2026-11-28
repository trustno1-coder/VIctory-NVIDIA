//! PlayStation button prompts for the title's prompt-icon movie (a Scaleform
//! movie of vector button symbols in the user's `data.rpf`). Only shapes are
//! replaced, so every symbol, name and script of the title's movie stays as
//! it is: the face-button letters become Kenney's cross, circle, square and
//! triangle, the bumper and trigger labels L1, L2, R1, R2, and View the
//! touchpad, all inside the title's own button frames. The new movie keeps
//! the old one's length, and its zlib stream the old stream's length, so the
//! archive entry is served in place (`VICTORY_FILE_PATCH`).
//!
//! Nothing of the title is stored here: the movie comes from the user's
//! copy. What is ours: which button number is which button, and where in a
//! frame a symbol goes. The symbols are Kenney's (CC0), below.

//! Ported from Marvisak/VIctory PR #3399 by anoyarou (GPL-3.0-or-later).

use std::collections::HashMap;
use victory_formats::{swf, zlib_exact};

/// The suffix of the names the title's prompt symbols are exported under,
/// followed by the button number.
pub const SYMBOL_SUFFIX: &str = ".input_icons.Main_Button_";

type P = (f64, f64);
/// A segment: start, control point of a quadratic curve (or none), end.
type Seg = (P, Option<P>, P);
type Contour = Vec<Seg>;

/// Contours of an SVG path of absolute `M`, `L`, `Q` (and `Z`) commands.
fn contours(d: &str) -> Vec<Contour> {
    let mut out: Vec<Contour> = Vec::new();
    let (mut cmd, mut pos, mut start) = ('M', (0.0, 0.0), (0.0, 0.0));
    let mut nums: Vec<f64> = Vec::new();
    let flush =
        |cmd: char, nums: &mut Vec<f64>, pos: &mut P, start: &mut P, out: &mut Vec<Contour>| {
            let mut it = nums.chunks_exact(if cmd == 'Q' { 4 } else { 2 });
            let mut first = cmd == 'M';
            for c in &mut it {
                match cmd {
                    'M' if first => {
                        out.push(Vec::new());
                        *pos = (c[0], c[1]);
                        *start = *pos;
                        first = false;
                    }
                    'M' | 'L' => {
                        let p = (c[0], c[1]);
                        out.last_mut()
                            .expect("a path starts with M")
                            .push((*pos, None, p));
                        *pos = p;
                    }
                    _ => {
                        let p = (c[2], c[3]);
                        out.last_mut().expect("a path starts with M").push((
                            *pos,
                            Some((c[0], c[1])),
                            p,
                        ));
                        *pos = p;
                    }
                }
            }
            nums.clear();
        };
    for tok in d.split([' ', ',']).filter(|t| !t.is_empty()) {
        let (head, rest) = match tok.chars().next() {
            Some(c) if c.is_ascii_alphabetic() => (Some(c), &tok[1..]),
            _ => (None, tok),
        };
        if let Some(c) = head {
            flush(cmd, &mut nums, &mut pos, &mut start, &mut out);
            assert!(matches!(c, 'M' | 'L' | 'Q' | 'Z' | 'z'), "path command {c}");
            if c == 'Z' || c == 'z' {
                if pos != start {
                    out.last_mut()
                        .expect("a path starts with M")
                        .push((pos, None, start));
                }
                pos = start;
            }
            cmd = c.to_ascii_uppercase();
        }
        if !rest.is_empty() {
            nums.push(rest.parse().expect("a number"));
        }
    }
    flush(cmd, &mut nums, &mut pos, &mut start, &mut out);
    for c in &mut out {
        if let (Some(first), Some(last)) = (c.first().copied(), c.last().copied())
            && last.2 != first.0
        {
            c.push((last.2, None, first.0));
        }
    }
    out
}

/// `[x0, x1, y0, y1]` of contours (control points included).
fn bbox(cs: &[Contour]) -> [f64; 4] {
    let mut b = [f64::MAX, f64::MIN, f64::MAX, f64::MIN];
    for (a, c, e) in cs.iter().flatten() {
        for p in [Some(*a), *c, Some(*e)].into_iter().flatten() {
            b = [b[0].min(p.0), b[1].max(p.0), b[2].min(p.1), b[3].max(p.1)];
        }
    }
    b
}

/// The symbol cut out of a button: every contour but the outermost.
fn symbol(mut cs: Vec<Contour>) -> Vec<Contour> {
    let area = |c: &Contour| {
        let b = bbox(std::slice::from_ref(c));
        (b[1] - b[0]) * (b[3] - b[2])
    };
    let outer = (0..cs.len())
        .max_by(|&a, &b| area(&cs[a]).total_cmp(&area(&cs[b])))
        .expect("contours");
    cs.remove(outer);
    cs
}

/// Contours grouped by overlapping x ranges, left to right (the `L` and the
/// `2` of `L2`).
fn columns(mut cs: Vec<Contour>) -> Vec<Vec<Contour>> {
    cs.sort_by(|a, b| {
        bbox(std::slice::from_ref(a))[0].total_cmp(&bbox(std::slice::from_ref(b))[0])
    });
    let mut groups: Vec<(Vec<Contour>, f64)> = Vec::new();
    for c in cs {
        let [x0, x1, ..] = bbox(std::slice::from_ref(&c));
        match groups.last_mut() {
            Some((g, right)) if x0 <= *right => {
                *right = right.max(x1);
                g.push(c);
            }
            _ => groups.push((vec![c], x1)),
        }
    }
    groups.into_iter().map(|g| g.0).collect()
}

/// Scales by `s` about `from` and moves that point to `to`.
fn transform(cs: &[Contour], s: f64, from: P, to: P) -> Vec<Contour> {
    let f = |p: P| (to.0 + (p.0 - from.0) * s, to.1 + (p.1 - from.1) * s);
    cs.iter()
        .map(|c| c.iter().map(|&(a, k, e)| (f(a), k.map(f), f(e))).collect())
        .collect()
}

/// Scales to at most `height` and `width`, centred in `bounds`.
fn fit(cs: &[Contour], bounds: [f64; 4], height: f64, width: f64) -> Vec<Contour> {
    let b = bbox(cs);
    let s = (height / (b[3] - b[2])).min(width / (b[1] - b[0]));
    transform(
        cs,
        s,
        ((b[0] + b[1]) / 2.0, (b[2] + b[3]) / 2.0),
        ((bounds[0] + bounds[1]) / 2.0, (bounds[2] + bounds[3]) / 2.0),
    )
}

/// Pixel contours to shape edges in twips; points are rounded once, so
/// contours close exactly.
fn edges(cs: &[Contour]) -> Vec<swf::Edge> {
    let tw = |p: P| ((p.0 * 20.0).round() as i32, (p.1 * 20.0).round() as i32);
    let mut out = Vec::new();
    for c in cs {
        for (i, &(a, k, e)) in c.iter().enumerate() {
            out.push(swf::Edge {
                from: tw(a),
                ctrl: k.map(tw),
                to: tw(e),
                fill0: 1,
                fill1: 0,
                line: 0,
                moved: i == 0,
            });
        }
    }
    out
}

fn px(bounds: [i32; 4]) -> [f64; 4] {
    bounds.map(|v| f64::from(v) / 20.0)
}

/// How a button's label changes.
enum Label {
    /// A face button: the symbol in the title's disc (Kenney's place in its
    /// own disc, scaled to the height of the title's letters).
    Face(&'static str),
    /// The touchpad, a filled shape, 34 px wide in the disc.
    Pad(&'static str),
    /// A bumper: the lettering fitted to the title's label.
    Bumper(&'static str),
    /// A trigger: the title draws its label as two shapes; the lettering is
    /// fitted to both and split between them.
    Trigger(&'static str),
}

/// The buttons that change, by the number in their symbol name.
fn labels() -> [(u32, Label); 9] {
    [
        (30, Label::Face(kenney::CROSS)),
        (31, Label::Face(kenney::CIRCLE)),
        (32, Label::Face(kenney::SQUARE)),
        (33, Label::Face(kenney::TRIANGLE)),
        (34, Label::Bumper(kenney::L1)),
        (35, Label::Trigger(kenney::L2)),
        (36, Label::Bumper(kenney::R1)),
        (37, Label::Trigger(kenney::R2)),
        (39, Label::Pad(kenney::TOUCHPAD)),
    ]
}

/// Whether `movie` is the title's prompt-icon movie.
pub fn is_prompt_movie(movie: &[u8]) -> bool {
    symbols(movie).is_some_and(|s| labels().iter().all(|(n, _)| s.values().any(|v| v == n)))
}

/// Character id -> button number, from the movie's symbol names.
fn symbols(movie: &[u8]) -> Option<HashMap<u16, u32>> {
    if !swf::is_movie(movie) {
        return None;
    }
    let mut out = HashMap::new();
    for tag in swf::movie_tags(movie)? {
        if tag.code == swf::SYMBOL_CLASS || tag.code == swf::EXPORT_ASSETS {
            for (id, name) in swf::symbol_names(&movie[tag.body.clone()])? {
                if let Some((_, n)) = name.rsplit_once(SYMBOL_SUFFIX)
                    && let Ok(n) = n.parse()
                {
                    out.insert(id, n);
                }
            }
        }
    }
    Some(out)
}

/// The prompt movie with PlayStation labels, as long as the original.
pub fn rebuild(movie: &[u8]) -> Result<Vec<u8>, String> {
    let names = symbols(movie).ok_or("not a movie")?;
    let tags = swf::movie_tags(movie).ok_or("the movie's tags do not read")?;
    let mut shapes = HashMap::new();
    let mut sprites = HashMap::new();
    for tag in &tags {
        let body = &movie[tag.body.clone()];
        if swf::is_shape(tag.code) {
            let s = swf::parse_shape(tag.code, body).ok_or("a shape does not read")?;
            shapes.insert(s.id, s);
        } else if tag.code == swf::DEFINE_SPRITE
            && let Some((id, places)) = swf::sprite_places(movie, tag.body.clone())
        {
            sprites.insert(id, places);
        }
    }
    let shape_at = |sprite: u16, from_top: usize| -> Result<&swf::Shape, String> {
        let places = sprites
            .get(&sprite)
            .ok_or("a button symbol is not a sprite")?;
        let place = places
            .iter()
            .rev()
            .nth(from_top)
            .ok_or("a button has too few parts")?;
        shapes
            .get(&place.character.ok_or("a part without a character")?)
            .ok_or_else(|| "a part is not a shape".into())
    };
    let mut new = HashMap::new();
    for (number, label) in labels() {
        let sprite = *names
            .iter()
            .find(|&(_, &n)| n == number)
            .ok_or(format!("no button {number}"))?
            .0;
        let top = shape_at(sprite, 0)?;
        let color = match top.fills.first() {
            Some(swf::Fill::Solid(c)) => *c,
            _ => [0, 0, 0, 255],
        };
        let mut put = |shape: &swf::Shape, cs: &[Contour]| {
            new.insert(shape.id, swf::define_shape3(shape.id, color, &edges(cs)));
        };
        match label {
            Label::Face(d) => {
                let disc = px(shape_at(sprite, sprites[&sprite].len() - 1)?.bounds);
                let centre = ((disc[0] + disc[1]) / 2.0, (disc[2] + disc[3]) / 2.0);
                put(
                    top,
                    &transform(&symbol(contours(d)), 25.0 / 24.0, (32.0, 32.0), centre),
                );
            }
            Label::Pad(d) => {
                let disc = px(shape_at(sprite, sprites[&sprite].len() - 1)?.bounds);
                put(top, &fit(&contours(d), disc, f64::MAX, 34.0));
            }
            Label::Bumper(d) => {
                let b = px(top.bounds);
                put(top, &fit(&symbol(contours(d)), b, b[3] - b[2], b[1] - b[0]));
            }
            Label::Trigger(d) => {
                let left = shape_at(sprite, 1)?;
                let (a, b) = (px(left.bounds), px(top.bounds));
                let both = [
                    a[0].min(b[0]),
                    a[1].max(b[1]),
                    a[2].min(b[2]),
                    a[3].max(b[3]),
                ];
                let placed = fit(
                    &symbol(contours(d)),
                    both,
                    both[3] - both[2],
                    both[1] - both[0],
                );
                let [l, r]: [Vec<Contour>; 2] = columns(placed)
                    .try_into()
                    .map_err(|_| format!("button {number}: the lettering is not two columns"))?;
                put(left, &l);
                put(top, &r);
            }
        }
    }
    let body = swf::replace_shapes(movie, &new).ok_or("the movie could not be rebuilt")?;
    swf::with_length(body, movie.len())
        .ok_or_else(|| "the new movie is longer than the title's".into())
}

/// The zlib stream for the rebuilt movie, `stream_len` bytes long (the
/// title's stream length).
pub fn pack(movie: &[u8], stream_len: usize) -> Result<Vec<u8>, String> {
    zlib_exact::exact(movie, stream_len, zlib_exact::zopfli)
        .ok_or_else(|| format!("the rebuilt movie does not compress to {stream_len} bytes"))
}

/// Path data of Kenney's "Input Prompts" 1.5A (kenney.nl, CC0 1.0: public domain; credit appreciated,
/// not required): `PlayStation Series/Vector/<name>.svg`, a 64x64 grid, absolute M/L/Q commands.
mod kenney {
    /// `playstation_button_cross.svg`
    pub(super) const CROSS: &str = concat!(
        "M56 32 Q56 42 48.95 48.95 42 56 32 56 22.05 56 15 48.95 8 42 8 32 8 22.05 15 15 22.05 8 32 8 42 ",
        "8 48.95 15 56 22.05 56 32 M22.8 20 L20 22.8 29.2 32 20 41.2 22.8 44 32 34.8 41.2 44 44 41.2 34.8 ",
        "32 44 22.8 41.2 20 32 29.2 22.8 20",
    );
    /// `playstation_button_circle.svg`
    pub(super) const CIRCLE: &str = concat!(
        "M37.7 26.35 Q40 28.65 40 32 40 35.35 37.7 37.7 35.35 40 32 40 28.7 40 26.4 37.7 L26.35 37.65 Q24 ",
        "35.3 24 32 24 28.7 26.35 26.35 L26.4 26.35 Q28.7 24 32 24 35.35 24 37.7 26.35 M44 32 Q44 27 40.5 ",
        "23.5 37 20 32 20 27.05 20 23.55 23.5 20 27 20 32 20 37 23.55 40.5 27.05 44 32 44 37 44 40.5 40.5 ",
        "44 37 44 32 M56 32 Q56 42 48.95 48.95 42 56 32 56 22.05 56 15 48.95 8 42 8 32 8 22.05 15 15 ",
        "22.05 8 32 8 42 8 48.95 15 56 22.05 56 32",
    );
    /// `playstation_button_square.svg`
    pub(super) const SQUARE: &str = concat!(
        "M56 32 Q56 42 48.95 48.95 42 56 32 56 22.05 56 15 48.95 8 42 8 32 8 22.05 15 15 22.05 8 32 8 42 ",
        "8 48.95 15 56 22.05 56 32 M24 40 L24 24 40 24 40 40 24 40 M20 20 L20 44 44 44 44 20 20 20",
    );
    /// `playstation_button_triangle.svg`
    pub(super) const TRIANGLE: &str = concat!(
        "M56 32 Q56 42 48.95 48.95 42 56 32 56 22.05 56 15 48.95 8 42 8 32 8 22.05 15 15 22.05 8 32 8 42 ",
        "8 48.95 15 56 22.05 56 32 M32 25.95 L39.05 38 25 38 32 25.95 M46 42 L32 18 18 42 46 42",
    );
    /// `playstation_trigger_l1.svg`
    pub(super) const L1: &str = concat!(
        "M42 25 L38 25 35 28 35 29.95 38 29.95 38 34.85 38 34.95 38 42 42 42 42 25 M12 30 Q12 21.7 17.8 ",
        "15.8 20.2 13.45 22.95 12.1 27.05 10 32 10 40.25 10 46.1 15.8 49.85 19.55 51.25 24.3 L51.8 26.9 ",
        "52 29.6 52 46 52 46.45 Q51.85 49.15 50.1 51.2 L49.65 51.65 49.45 51.85 Q48 53.2 46.25 53.7 L44 ",
        "54 20 54 Q16.7 54 14.35 51.65 12 49.3 12 46 L12 30 M22 25 L22 42 34 42 34 38 26 38 26 25 22 25",
    );
    /// `playstation_trigger_l2.svg`
    pub(super) const L2: &str = concat!(
        "M23 23 L23 40 35 40 35 36 27 36 27 23 23 23 M16 30 L16 16 Q16 12.7 18.35 10.35 20.7 8 24 8 L48 8 ",
        "Q51.3 8 53.65 10.35 55.75 12.45 56 15.25 L56 16 56 40 Q56 46.6 51.3 51.3 46.6 56 40 56 L12.8 56 ",
        "Q10.75 56 9.45 54.75 8 53.45 8 51.6 8 50.15 12 43.6 15.75 37.4 16 30 M44.5 27.4 Q45.25 28.15 44 ",
        "29.45 L37 36 37 40 41.85 40 41.95 40 49 40 49 36 43 36 47.45 31.55 Q49.65 29.35 48.4 26 L47.4 ",
        "24.65 47.35 24.6 Q45.75 23 43.5 23 L41.85 23 Q39.9 23.05 38.5 24.4 L38.4 24.5 Q37.35 25.6 37.1 ",
        "27 L37 28 41 28 Q41 27.6 41.3 27.3 41.55 27.05 41.85 27 L41.95 27 43.5 27 Q44.1 27 44.5 27.4",
    );
    /// `playstation_trigger_r1.svg`
    pub(super) const R1: &str = concat!(
        "M21 25 L21 42 25 42 25 37 28 37 30.5 42 34.5 42 31.45 35.9 32.2 35.25 32.25 35.2 33.2 34 Q34 ",
        "32.65 34 31 34 28.5 32.25 26.75 L31.35 26 Q29.9 25 28 25 L21 25 M43 25 L39 25 36 28 36 29.95 39 ",
        "29.95 39 34.85 39 34.95 39 42 43 42 43 25 M12 30 Q12 21.7 17.8 15.8 23.7 10 32 10 40.25 10 46.1 ",
        "15.8 49.85 19.55 51.25 24.3 L51.8 26.9 52 29.6 52 46 52 46.45 Q51.85 49.15 50.1 51.2 L49.65 ",
        "51.65 49.45 51.85 Q48 53.2 46.25 53.7 L44 54 20 54 Q16.85 54 14.55 51.85 L14.35 51.65 Q12 49.3 ",
        "12 46 L12 30 M28 29 Q28.8 29 29.4 29.6 30 30.2 30 31 30 31.8 29.45 32.35 L29.35 32.45 Q28.8 33 ",
        "28 33 L25 33 25 29 28 29",
    );
    /// `playstation_trigger_r2.svg`
    pub(super) const R2: &str = concat!(
        "M48 30 Q48.25 37.4 52 43.6 56 50.15 56 51.6 56 53.45 54.55 54.75 53.25 56 51.2 56 L24 56 Q17.4 ",
        "56 12.7 51.3 8 46.6 8 40 L8 16 Q8 12.7 10.35 10.35 12.7 8 16 8 L40 8 Q43.3 8 45.65 10.35 48 12.7 ",
        "48 16 L48 30 M37.5 27.4 Q38.25 28.15 37 29.45 L30 36 30 40 34.85 40 34.95 40 42 40 42 36 36 36 ",
        "40.45 31.55 Q42.65 29.35 41.4 26 L40.4 24.65 40.35 24.6 Q38.75 23 36.5 23 L34.85 23 Q32.9 23.05 ",
        "31.5 24.4 L31.4 24.5 Q30.35 25.6 30.1 27 L30 28 34 28 Q34 27.6 34.3 27.3 34.55 27.05 34.85 27 ",
        "L34.95 27 36.5 27 Q37.1 27 37.5 27.4 M22 27 Q22.8 27 23.4 27.6 24 28.2 24 29 24 29.8 23.45 30.35 ",
        "L23.35 30.45 Q22.8 31 22 31 L19 31 19 27 22 27 M15 23 L15 40 19 40 19 35 22 35 24.5 40 28.5 40 ",
        "25.45 33.9 26.2 33.25 26.25 33.2 Q28 31.45 28 29 28 26.5 26.25 24.75 24.5 23 22 23 L15 23",
    );
    /// `playstation5_touchpad.svg`
    pub(super) const TOUCHPAD: &str = concat!(
        "M9.95 20.2 Q20.95 16 32 16 43.05 16 54.05 20.2 55.1 20.6 55.6 21.6 56.2 22.55 55.9 23.65 L51.25 ",
        "44 Q50.85 45.2 49.85 46.25 48.1 47.95 45.75 48 L18.25 48 Q15.9 47.95 14.15 46.25 13.15 45.2 ",
        "12.75 44 L8.1 23.65 Q7.8 22.55 8.4 21.6 8.9 20.6 9.95 20.2",
    );
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

    fn square(x0: f64, y0: f64, x1: f64, y1: f64, notches: usize) -> Vec<Contour> {
        // A rectangle with many small steps on top, so that labels cost bytes like lettering does.
        let mut pts = vec![(x0, y0)];
        for i in 1..=notches {
            let x = x0 + (x1 - x0) * i as f64 / (notches + 1) as f64;
            pts.extend([(x, y0), (x, y0 + 0.3), (x + 0.2, y0 + 0.3), (x + 0.2, y0)]);
        }
        pts.extend([(x1, y0), (x1, y1), (x0, y1)]);
        vec![
            (0..pts.len())
                .map(|i| (pts[i], None, pts[(i + 1) % pts.len()]))
                .collect(),
        ]
    }

    /// A movie of our own shaped like the title's: per button a sprite with a disc and one or two labels.
    fn movie() -> Vec<u8> {
        // Header: signature, version, length (set below), an empty frame rectangle, frame rate, frame count.
        let mut m = vec![b'G', b'F', b'X', 8, 0, 0, 0, 0, 0, 0, 30, 1, 0];
        let (mut next, mut names) = (1u16, Vec::new());
        for n in [4u32, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39] {
            let mut parts = vec![square(1.5, 1.5, 62.5, 62.5, 4)];
            parts.push(square(16.0, 19.0, 33.0, 45.0, 30));
            if matches!(n, 35 | 37) {
                parts.push(square(32.0, 17.0, 53.0, 45.0, 30));
            }
            let mut sprite = Vec::new();
            for (depth, cs) in parts.iter().enumerate() {
                m.extend(swf::define_shape3(next, [0, 0, 0, 255], &edges(cs)));
                let mut p = vec![2];
                p.extend_from_slice(&(depth as u16 + 1).to_le_bytes());
                p.extend_from_slice(&next.to_le_bytes());
                sprite.extend(tag(swf::PLACE_OBJECT2, &p));
                next += 1;
            }
            sprite.extend(tag(swf::SHOW_FRAME, &[]));
            sprite.extend(tag(swf::END, &[]));
            let mut body = next.to_le_bytes().to_vec();
            body.extend_from_slice(&[1, 0]);
            body.extend(sprite);
            m.extend(tag(swf::DEFINE_SPRITE, &body));
            names.push((next, format!("our_pkg{SYMBOL_SUFFIX}{n}")));
            next += 1;
        }
        let mut sc = (names.len() as u16).to_le_bytes().to_vec();
        for (id, name) in &names {
            sc.extend_from_slice(&id.to_le_bytes());
            sc.extend_from_slice(name.as_bytes());
            sc.push(0);
        }
        m.extend(tag(swf::SYMBOL_CLASS, &sc));
        m.extend(tag(swf::SHOW_FRAME, &[]));
        m.extend(tag(swf::END, &[]));
        let len = m.len() as u32;
        m[4..8].copy_from_slice(&len.to_le_bytes());
        m
    }

    #[test]
    fn kenney_paths_read() {
        for d in [
            kenney::CROSS,
            kenney::CIRCLE,
            kenney::SQUARE,
            kenney::TRIANGLE,
            kenney::L1,
            kenney::L2,
            kenney::R1,
            kenney::R2,
            kenney::TOUCHPAD,
        ] {
            let cs = contours(d);
            assert!(!cs.is_empty());
            assert!(
                cs.iter()
                    .all(|c| c.first().unwrap().0 == c.last().unwrap().2),
                "closed"
            );
            let b = bbox(&cs);
            assert!(b[0] >= 0.0 && b[1] <= 64.0 && b[2] >= 0.0 && b[3] <= 64.0);
        }
        assert_eq!(columns(symbol(contours(kenney::L2))).len(), 2);
        assert_eq!(columns(symbol(contours(kenney::R2))).len(), 2);
    }

    #[test]
    fn labels_change_and_the_rest_stays() {
        let m = movie();
        assert!(is_prompt_movie(&m));
        let out = rebuild(&m).unwrap();
        assert_eq!(out.len(), m.len());
        let (a, b) = (swf::movie_tags(&m).unwrap(), swf::movie_tags(&out).unwrap());
        assert_eq!(a.len(), b.len());
        let changed = a
            .iter()
            .zip(&b)
            .filter(|(x, y)| m[x.start..x.body.end] != out[y.start..y.body.end])
            .count();
        // 7 one-label buttons + 2 triggers with two labels each; the discs and buttons 4 and 38 stay.
        assert_eq!(changed, 11);
        assert_eq!(symbols(&out), symbols(&m));
        let target = zlib_exact::exact(&m, usize::MAX, zlib_exact::zopfli)
            .map_or(0, |s| s.len())
            .max(
                (1..)
                    .map(|extra| m.len() / 2 + extra * 100)
                    .find(|&t| zlib_exact::exact(&m, t, zlib_exact::zopfli).is_some())
                    .unwrap(),
            );
        let stream = pack(&out, target).unwrap();
        assert_eq!(stream.len(), target);
        assert_eq!(zlib_exact::inflate(&stream, out.len()).unwrap().data, out);
    }
}
