//! The choices the F1 menu saves (`VICTORY_MENU_*` in `victory-settings.txt`).
//! The play script reads them over its own options, so the launcher writes
//! its choices there: the menu and the launcher always agree.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::settings_file::{self, entries};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GameOptions {
    pub console: String,
    pub res: String,
    pub dlss: String,
    /// 0: off; 2 to 6: frames shown for each one rendered.
    pub framegen: u8,
    pub fg_type: String,
    /// 1000: no cap.
    pub fps: u32,
}

const CONSOLES: &[&str] = &["series-x", "series-s"];
const RESOLUTIONS: &[&str] = &["1440", "1080to1440", "4k"];
const DLSS_MODES: &[&str] = &["off", "dlaa", "quality", "balanced", "performance"];
const FG_TYPES: &[&str] = &["auto", "dlss", "fsr"];

impl Default for GameOptions {
    /// The play script's own defaults.
    fn default() -> Self {
        Self {
            console: "series-x".into(),
            res: "1440".into(),
            dlss: "quality".into(),
            framegen: 0,
            fg_type: "auto".into(),
            fps: 1000,
        }
    }
}

impl GameOptions {
    /// What the menu file holds, with anything missing or unknown at its default.
    pub(crate) fn from_menu(text: &str) -> Self {
        let mut o = Self::default();
        for (key, value) in entries(text) {
            let pick = |allowed: &[&str], slot: &mut String| {
                if allowed.contains(&value) {
                    *slot = value.to_owned();
                }
            };
            match key {
                "VICTORY_MENU_CONSOLE" => pick(CONSOLES, &mut o.console),
                "VICTORY_MENU_RES" => pick(RESOLUTIONS, &mut o.res),
                "VICTORY_MENU_DLSS" => pick(DLSS_MODES, &mut o.dlss),
                "VICTORY_MENU_FG_TYPE" => pick(FG_TYPES, &mut o.fg_type),
                // 1, from an older file, means 2.
                "VICTORY_MENU_FRAMEGEN" => match value.parse::<u8>() {
                    Ok(0) => o.framegen = 0,
                    Ok(1) => o.framegen = 2,
                    Ok(n @ 2..=6) => o.framegen = n,
                    _ => {}
                },
                "VICTORY_MENU_FPS" => {
                    if let Ok(n @ 1..=1000) = value.parse() {
                        o.fps = n;
                    }
                }
                _ => {}
            }
        }
        o
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        let check = |allowed: &[&str], v: &str, what: &str| {
            if allowed.contains(&v) {
                Ok(())
            } else {
                Err(format!("Unknown {what}: {v}"))
            }
        };
        check(CONSOLES, &self.console, "console")?;
        check(RESOLUTIONS, &self.res, "resolution")?;
        check(DLSS_MODES, &self.dlss, "DLSS mode")?;
        check(FG_TYPES, &self.fg_type, "frame generation type")?;
        if !matches!(self.framegen, 0 | 2..=6) {
            return Err(format!(
                "Frame generation must be off or 2 to 6, not {}",
                self.framegen
            ));
        }
        if !(1..=1000).contains(&self.fps) {
            return Err(format!(
                "Frame rate cap must be 1 to 1000, not {}",
                self.fps
            ));
        }
        Ok(())
    }

    fn menu_values(&self) -> [(&'static str, String); 6] {
        [
            ("VICTORY_MENU_CONSOLE", self.console.clone()),
            ("VICTORY_MENU_RES", self.res.clone()),
            ("VICTORY_MENU_DLSS", self.dlss.clone()),
            ("VICTORY_MENU_FRAMEGEN", self.framegen.to_string()),
            ("VICTORY_MENU_FG_TYPE", self.fg_type.clone()),
            ("VICTORY_MENU_FPS", self.fps.to_string()),
        ]
    }

    pub(crate) fn load(menu_file: &Path) -> Self {
        Self::from_menu(&settings_file::read(menu_file))
    }

    pub(crate) fn save(&self, menu_file: &Path) -> Result<()> {
        settings_file::set(menu_file, &self.menu_values())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_gives_the_script_defaults() {
        assert_eq!(GameOptions::from_menu(""), GameOptions::default());
    }

    #[test]
    fn the_menu_choices_are_read() {
        let text = "VICTORY_MENU_CONSOLE=series-s\nVICTORY_MENU_DLSS = balanced\n\
                    VICTORY_MENU_FRAMEGEN=1\nVICTORY_MENU_FPS=60\nVICTORY_MENU_RES=nonsense\n";
        let o = GameOptions::from_menu(text);
        assert_eq!(o.console, "series-s");
        assert_eq!(o.dlss, "balanced");
        assert_eq!(o.framegen, 2);
        assert_eq!(o.fps, 60);
        assert_eq!(o.res, "1440");
    }

    #[test]
    fn saving_keeps_the_menu_switches_and_replaces_the_choices() {
        let old = "VICTORY_AMBIENT=0.5\nVICTORY_MENU_DLSS=off\nVICTORY_MENU_DLSS=dlaa\nVICTORY_WATER_LIGHT=\n";
        let o = GameOptions {
            dlss: "performance".into(),
            framegen: 4,
            ..Default::default()
        };
        let new = settings_file::with_values(old, &o.menu_values());
        assert!(new.starts_with(
            "VICTORY_AMBIENT=0.5\nVICTORY_MENU_DLSS=performance\nVICTORY_WATER_LIGHT=\n"
        ));
        assert_eq!(new.matches("VICTORY_MENU_DLSS").count(), 1);
        assert!(new.contains("VICTORY_MENU_FRAMEGEN=4\n"));
        assert_eq!(GameOptions::from_menu(&new), o);
    }

    #[test]
    fn unknown_choices_are_refused() {
        let bad = GameOptions {
            framegen: 1,
            ..Default::default()
        };
        assert!(bad.validate().is_err());
        let bad = GameOptions {
            console: "one".into(),
            ..Default::default()
        };
        assert!(bad.validate().is_err());
        assert!(GameOptions::default().validate().is_ok());
    }
}
