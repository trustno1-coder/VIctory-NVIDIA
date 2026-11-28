use std::io::{BufRead, Write};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    VK_DOWN, VK_LEFT, VK_RETURN, VK_RIGHT, VK_UP,
};

const STREAM: &str = "VICTORY_GAME_STREAM_MS";
const ACTIONS: [&str; 5] = [
    "MEASURE 20 SECONDS OF GAMEPLAY",
    "SAVE FOR NEXT START",
    "SAVE AND RESTART",
    "SELECT ORIGINAL GRAPHICS",
    "CLOSE",
];
const CONFIRM: &str = "SAVE GAME PROGRESS FIRST. ENTER AGAIN TO RESTART.";

struct Setting {
    key: String,
    label: String,
    original: String,
    choices: Vec<String>, // The empty first choice follows the profile's original value.
    picked: usize,
    saved: usize,
}

impl Setting {
    fn new(key: &str, label: &str, original: &str, choices: &str, selected: &str) -> Self {
        let choices: Vec<_> = std::iter::once("")
            .chain(choices.split(','))
            .map(str::to_owned)
            .collect();
        let picked = choices.iter().position(|s| s == selected).unwrap_or(0);
        Self {
            key: key.into(),
            label: label.into(),
            original: original.into(),
            choices,
            picked,
            saved: picked,
        }
    }

    fn effective(&self) -> &str {
        if self.picked == 0 {
            &self.original
        } else {
            &self.choices[self.picked]
        }
    }

    fn apply_live(&self) {
        if self.key == STREAM
            && let Ok(ms) = self.effective().parse()
        {
            crate::draw::texture_stage_ms().store(ms, Ordering::Relaxed);
            tracing::info!(target: "victory_gpu::host", ms, "F1 graphics: texture streaming budget changed");
        }
    }
}

pub(super) enum Action {
    None,
    Save,
    Restart,
    Close,
}

pub(super) struct Graphics {
    rows: Vec<Setting>,
    at: usize,
    sample: Option<Sample>,
    recent: Vec<String>,
    results_file: Option<std::path::PathBuf>,
    // The native graphics in use for this run, including after saving pending changes.
    active: String,
}

#[derive(Default)]
struct Sample {
    first: Option<Instant>,
    last: Option<Instant>,
    frames: Vec<Duration>,
}

impl Sample {
    fn frame(&mut self, now: Instant) -> Option<String> {
        let first = *self.first.get_or_insert(now);
        let last = self.last.replace(now)?;
        self.frames.push(now - last);
        let elapsed = now - first;
        if elapsed < Duration::from_secs(20) {
            return None;
        }
        let n = self.frames.len();
        let fps = n as f64 / elapsed.as_secs_f64();
        let (_, p95, _) = self.frames.select_nth_unstable((95 * n).div_ceil(100) - 1);
        Some(format!(
            "{fps:.1} FPS  {:.1} MS AVG  {:.1} MS P95",
            1000.0 / fps,
            p95.as_secs_f64() * 1000.0
        ))
    }
}

impl Graphics {
    pub(super) fn load() -> Option<Self> {
        let spec = std::env::var("VICTORY_MENU_GAME_FIELDS").ok()?;
        let mut page = Self::from_spec(&spec, &super::get("VICTORY_TEXTURE_STAGE_MS"), super::get)?;
        page.results_file = std::env::var_os("VICTORY_MENU_SAMPLE_FILE").map(Into::into);
        if let Some(file) = page
            .results_file
            .as_ref()
            .and_then(|p| std::fs::File::open(p).ok())
        {
            for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
                if let Some(summary) = line.split('\t').nth(1) {
                    page.remember(summary.to_owned());
                }
            }
        }
        Some(page)
    }

    pub(super) fn from_spec(spec: &str, budget: &str, get: impl Fn(&str) -> String) -> Option<Self> {
        let mut rows = Vec::new();
        for line in spec.lines() {
            let parts: Vec<_> = line.split('|').collect();
            if let [key, label, original, choices] = parts.as_slice()
                && key.starts_with("VICTORY_GAME_")
                && !choices.is_empty()
            {
                rows.push(Setting::new(key, label, original, choices, &get(key)));
            }
        }
        if rows.is_empty() {
            return None;
        }
        let mut active = rows
            .iter()
            .map(|r| format!("{}={}", r.key, r.effective()))
            .collect::<Vec<_>>()
            .join(" ");
        for key in ["VICTORY_MENU_CONSOLE", "VICTORY_MENU_DLSS", "VICTORY_BVH_BUILDER", "VICTORY_FULLSCREEN"] {
            let value = get(key);
            if !value.is_empty() {
                active.push_str(&format!(" {key}={value}"));
            }
        }
        let budget = budget.trim().parse::<u64>().unwrap_or(250).to_string();
        rows.push(Setting::new(
            STREAM,
            "Texture streaming budget",
            &budget,
            "4,8,12,16,24",
            &get(STREAM),
        ));
        Some(Self {
            rows,
            at: 0,
            sample: None,
            recent: Vec::new(),
            results_file: None,
            active,
        })
    }

    fn remember(&mut self, summary: String) {
        self.recent.push(summary);
        if self.recent.len() > 2 {
            self.recent.remove(0);
        }
    }

    pub(super) fn frame(&mut self) {
        let Some(sample) = &mut self.sample else {
            return;
        };
        let Some(result) = sample.frame(Instant::now()) else {
            return;
        };
        self.sample = None;
        let ms = crate::draw::texture_stage_ms().load(Ordering::Relaxed);
        let fxaa = super::fxaa_on().load(Ordering::Relaxed);
        tracing::info!(target: "victory_gpu::host", %result, streaming_ms = ms, fxaa, graphics = %self.active, "F1 gameplay sample");
        if let Some(path) = &self.results_file {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if let Err(e) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut f| writeln!(f, "{stamp}\t{result}\tstream_ms={ms} fxaa={fxaa} {}", self.active))
            {
                tracing::warn!(error = %e, "could not save gameplay sample");
            }
        }
        self.remember(result);
    }

    pub(super) fn cancel_sample(&mut self) -> bool {
        self.sample.take().is_some()
    }

    pub(super) fn seconds_left(&self) -> Option<u64> {
        let sample = self.sample.as_ref()?;
        Some(20u64.saturating_sub(sample.first.map_or(0, |t| t.elapsed().as_secs())))
    }

    pub(super) fn edits(&self) -> impl Iterator<Item = (&str, &str)> {
        self.rows
            .iter()
            .filter(|r| r.picked != r.saved)
            .map(|r| (r.key.as_str(), r.choices[r.picked].as_str()))
    }

    pub(super) fn mark_saved(&mut self) {
        for row in &mut self.rows {
            row.saved = row.picked
        }
    }

    pub(super) fn lines(&self, note: &str) -> Vec<String> {
        let mut lines = vec![
            "GAME GRAPHICS                  TAB: VICTORY SETTINGS".into(),
            "UP/DOWN PICK   LEFT/RIGHT CHANGE   F1 OR ESC CLOSE".into(),
            String::new(),
        ];
        for (i, row) in self.rows.iter().enumerate() {
            let raw = row.effective();
            let value = match raw {
                "true" => "ON",
                "false" => "OFF",
                v if v.contains('.') => v.trim_end_matches('0').trim_end_matches('.'),
                v => v,
            };
            let unit = if row.key == STREAM { " MS" } else { "" };
            let original = if row.picked == 0 { "ORIGINAL " } else { "" };
            let mark = if row.picked != row.saved { " *" } else { "" };
            lines.push(format!(
                "{} {:<26} < {original}{value}{unit} >{mark}",
                if i == self.at { '>' } else { ' ' },
                row.label.to_ascii_uppercase()
            ));
        }
        lines.push(String::new());
        for (i, action) in ACTIONS.iter().enumerate() {
            lines.push(format!(
                "{} {action}",
                if self.at == self.rows.len() + i {
                    '>'
                } else {
                    ' '
                }
            ));
        }
        lines.push(String::new());
        if self.rows.get(self.at).is_some_and(|row| row.key == STREAM) {
            lines.push("LIVE. LOWER VALUES CAN DELAY TEXTURE DETAIL.".into());
        } else {
            lines.push("GAME GRAPHICS LOAD AT THE NEXT START.".into());
        }
        lines.push("F3: STATS   *: UNSAVED CHANGE".into());
        for (label, sample) in ["LAST", "PREV"].into_iter().zip(self.recent.iter().rev()) {
            lines.push(format!("{label}  {sample}"));
        }
        lines.push(note.into());
        lines
    }

    pub(super) fn key(&mut self, vk: u16, note: &mut String) -> Action {
        let confirmed = note == CONFIRM;
        note.clear();
        let count = self.rows.len();
        let total = count + ACTIONS.len();
        match vk {
            VK_UP => self.at = (self.at + total - 1) % total,
            VK_DOWN => self.at = (self.at + 1) % total,
            VK_LEFT | VK_RIGHT | VK_RETURN if self.at < count => {
                let row = &mut self.rows[self.at];
                let n = row.choices.len();
                row.picked = (row.picked + if vk == VK_LEFT { n - 1 } else { 1 }) % n;
                row.apply_live();
            }
            VK_RETURN => match self.at - count {
                0 => {
                    self.sample = Some(Sample::default());
                    return Action::Close;
                }
                1 => return Action::Save,
                2 if confirmed => return Action::Restart,
                2 => *note = CONFIRM.into(),
                3 => {
                    for row in &mut self.rows {
                        if row.picked != 0 {
                            row.picked = 0;
                            row.apply_live();
                        }
                    }
                    *note = "ORIGINALS SELECTED. SAVE TO KEEP.".into();
                }
                _ => return Action::Close,
            },
            _ => {}
        }
        Action::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edits_save_restore_and_restart_confirmation() {
        let mut g = Graphics::from_spec(
            "VICTORY_GAME_GRASSLOD|Grass detail|0.875000|0.50,0.75,1.00",
            "16",
            |_| String::new(),
        )
        .unwrap();
        let mut note = String::new();
        assert!(g.lines("").iter().any(|s| s.contains("ORIGINAL 0.875")));
        g.key(VK_RIGHT, &mut note);
        assert_eq!(
            g.edits().collect::<Vec<_>>(),
            [("VICTORY_GAME_GRASSLOD", "0.50")]
        );
        g.mark_saved();
        assert_eq!(g.edits().count(), 0);
        g.at = g.rows.len() + 3;
        g.key(VK_RETURN, &mut note);
        assert_eq!(
            g.edits().collect::<Vec<_>>(),
            [("VICTORY_GAME_GRASSLOD", "")]
        );
        g.key(VK_UP, &mut note);
        assert!(matches!(g.key(VK_RETURN, &mut note), Action::None));
        assert!(matches!(g.key(VK_RETURN, &mut note), Action::Restart));
        assert!(
            g.lines(&note)
                .iter()
                .all(|line| line.len() <= super::super::COLUMNS as usize)
        );
    }

    #[test]
    fn sample_counts_present_intervals_and_reports_slow_frames() {
        let start = Instant::now();
        let mut sample = Sample::default();
        for i in 0..400 {
            assert!(
                sample
                    .frame(start + Duration::from_millis(i * 50))
                    .is_none()
            );
        }
        assert_eq!(
            sample.frame(start + Duration::from_secs(20)).as_deref(),
            Some("20.0 FPS  50.0 MS AVG  50.0 MS P95")
        );
    }
}
