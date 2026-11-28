//! The pipelines' line, every 5 seconds with the frame-rate line: what
//! lists bound for the first time, what was translated and what host
//! pipelines the workers made, with the slowest of each by name.
//!
//! It stands in for a line per item ("first bind" for each shader of each
//! pipeline, "shader translated", "host pipeline", "host mesh pipeline"):
//! some 9,000 lines in a 3-minute run (half its lines), 50,000 in a 9-minute
//! one, each formatted by the thread that made or bound the pipeline. Those
//! lines are `debug` now: a `diagnostics` build brings them back with
//! `VICTORY_LOG=victory_gpu::pipeline=debug,info`.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// What happened since the last [`Tally::line`].
#[derive(Default)]
struct Window {
    /// When it began: at the last line, else at the run's first count.
    began: Option<Instant>,
    /// Pipelines bound by a list for the first time, and their shaders.
    bound: u64,
    bound_shaders: u64,
    /// Shaders translated, and those that did not translate.
    translated: u64,
    untranslated: u64,
    translate_us: u64,
    /// The slowest translation: microseconds, "stage hash".
    slowest_translation: Option<(u64, String)>,
    /// Host pipelines made, by kind ([`Kind`]), and those the host refused.
    made: [u64; 3],
    refused: u64,
    make_us: u64,
    /// The slowest pipeline made or refused: microseconds, its shaders.
    slowest_made: Option<(u64, String)>,
}

/// What a worker made of a pipeline.
#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Graphics,
    Mesh,
    Compute,
    /// The host refused it, or a shader did not translate.
    Refused,
}

/// Counts for [`Tally::line`]; one for the run, [`TALLY`].
pub(crate) struct Tally(Mutex<Window>);

/// The run's pipelines' counts.
pub(crate) static TALLY: Tally = Tally::new();

/// Keeps `(us, name)` as the slowest if it is slower than `slowest`; the
/// name is made only then.
fn keep_slowest(slowest: &mut Option<(u64, String)>, us: u64, name: impl FnOnce() -> String) {
    if slowest.as_ref().is_none_or(|(most, _)| us > *most) {
        *slowest = Some((us, name()));
    }
}

impl Tally {
    pub(crate) const fn new() -> Self {
        Tally(Mutex::new(Window {
            began: None,
            bound: 0,
            bound_shaders: 0,
            translated: 0,
            untranslated: 0,
            translate_us: 0,
            slowest_translation: None,
            made: [0; 3],
            refused: 0,
            make_us: 0,
            slowest_made: None,
        }))
    }

    fn with<T>(&self, f: impl FnOnce(&mut Window) -> T) -> T {
        let mut w = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if w.began.is_none() {
            w.began = Some(Instant::now());
        }
        f(&mut w)
    }

    /// A list bound a pipeline of `shaders` shaders for the first time.
    pub(crate) fn bound(&self, shaders: usize) {
        self.with(|w| {
            w.bound += 1;
            w.bound_shaders += shaders as u64;
        });
    }

    /// A shader was translated (or did not translate) in `us`
    /// microseconds; `name` is its stage and hash.
    pub(crate) fn translated(&self, ok: bool, us: u64, name: impl FnOnce() -> String) {
        self.with(|w| {
            if ok {
                w.translated += 1;
            } else {
                w.untranslated += 1;
            }
            w.translate_us += us;
            keep_slowest(&mut w.slowest_translation, us, name);
        });
    }

    /// A worker made a host pipeline (or had it refused) in `us`
    /// microseconds; `name` is its shaders.
    pub(crate) fn made(&self, kind: Kind, us: u64, name: impl FnOnce() -> String) {
        self.with(|w| {
            match kind {
                Kind::Graphics => w.made[0] += 1,
                Kind::Mesh => w.made[1] += 1,
                Kind::Compute => w.made[2] += 1,
                Kind::Refused => w.refused += 1,
            }
            w.make_us += us;
            keep_slowest(&mut w.slowest_made, us, name);
        });
    }

    /// The line on what happened since the last call (at `now`), which
    /// starts the next window; nothing if nothing happened and `being_made`
    /// (pipelines queued and not made) is 0. `so_far` is the host pipelines
    /// compiled and loaded from the library since the start.
    pub(crate) fn line(&self, now: Instant, being_made: u64, so_far: [u64; 2]) -> Option<String> {
        let w = self.with(|w| std::mem::replace(w, Window { began: Some(now), ..Window::default() }));
        let seconds = w.began.map_or(Duration::ZERO, |began| now.saturating_duration_since(began)).as_secs_f64();
        let made: u64 = w.made.iter().sum::<u64>() + w.refused;
        if w.bound + w.translated + w.untranslated + made == 0 && being_made == 0 {
            return None;
        }
        let ms = |us: u64| us as f64 / 1e3;
        let mut line = format!(
            "pipelines in the last {seconds:.0} seconds: {} first bound by lists ({} shaders); {} shaders translated",
            w.bound, w.bound_shaders, w.translated
        );
        if w.untranslated > 0 {
            line += &format!(" and {} that do not translate", w.untranslated);
        }
        if let Some((us, name)) = &w.slowest_translation {
            line += &format!(" in {:.0} ms (the slowest {:.1} ms: {name})", ms(w.translate_us), ms(*us));
        }
        let [graphics, mesh, compute] = w.made;
        line += &format!(
            "; {made} host pipelines made by the workers ({graphics} graphics, {mesh} mesh, {compute} compute, {} refused)",
            w.refused
        );
        if let Some((us, name)) = &w.slowest_made {
            line += &format!(
                " in {:.0} ms, their translations included (the slowest {:.1} ms: {name})",
                ms(w.make_us),
                ms(*us)
            );
        }
        line += &format!(
            "; {being_made} being made now; since the start {} compiled, {} loaded from the library \
             (one line each: VICTORY_LOG=victory_gpu::pipeline=debug,info in a diagnostics build)",
            so_far[0], so_far[1]
        );
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Five seconds from now: windows in these tests are 5 seconds long.
    fn later() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    #[test]
    fn the_line_counts_what_happened_since_the_last_one() {
        let t = Tally::new();
        t.bound(2);
        t.bound(1);
        t.translated(true, 1_500, || "vertex aa".into());
        t.translated(true, 40_000, || "compute bb".into());
        t.translated(false, 2_100, || "pixel cc".into());
        t.made(Kind::Graphics, 10_000, || "[vertex aa, pixel dd]".into());
        t.made(Kind::Graphics, 3_000, || "[vertex ee, pixel ff]".into());
        t.made(Kind::Mesh, 1_000, || "[mesh gg, pixel hh]".into());
        t.made(Kind::Compute, 250_000, || "[compute bb]".into());
        t.made(Kind::Refused, 400, || "[compute zz]".into());
        let line = t.line(later(), 970, [120, 3_400]).unwrap();
        assert_eq!(
            line,
            "pipelines in the last 5 seconds: 2 first bound by lists (3 shaders); 2 shaders translated and 1 that \
             do not translate in 44 ms (the slowest 40.0 ms: compute bb); 5 host pipelines made by the workers \
             (2 graphics, 1 mesh, 1 compute, 1 refused) in 264 ms, their translations included (the slowest 250.0 ms: \
             [compute bb]); 970 being made now; since the start 120 compiled, 3400 loaded from the library \
             (one line each: VICTORY_LOG=victory_gpu::pipeline=debug,info in a diagnostics build)"
        );
        // The next window starts empty.
        let line = t.line(later() + Duration::from_secs(5), 3, [121, 3_400]).unwrap();
        assert!(line.starts_with(
            "pipelines in the last 5 seconds: 0 first bound by lists (0 shaders); 0 shaders translated; \
             0 host pipelines made by the workers (0 graphics, 0 mesh, 0 compute, 0 refused); 3 being made now"
        ), "{line}");
    }

    #[test]
    fn a_quiet_window_with_nothing_being_made_says_nothing() {
        let t = Tally::new();
        assert_eq!(t.line(later(), 0, [1, 2]), None);
        t.bound(1);
        assert!(t.line(later(), 0, [1, 2]).is_some());
        assert_eq!(t.line(later(), 0, [1, 2]), None);
    }

    /// The slowest keeps the first of equals, and its name is made only
    /// when it is kept.
    #[test]
    fn only_the_slowest_name_is_made() {
        let t = Tally::new();
        let made = std::cell::Cell::new(0);
        let name = |n: &'static str| {
            made.set(made.get() + 1);
            n.to_owned()
        };
        t.made(Kind::Graphics, 5, || name("first"));
        t.made(Kind::Graphics, 5, || name("equal"));
        t.made(Kind::Graphics, 4, || name("faster"));
        t.made(Kind::Graphics, 9, || name("slowest"));
        assert_eq!(made.get(), 2);
        assert!(t.line(later(), 0, [0, 0]).unwrap().contains("(the slowest 0.0 ms: slowest)"));
    }
}
