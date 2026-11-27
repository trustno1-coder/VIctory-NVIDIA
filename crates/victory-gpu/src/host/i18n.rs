//! A single language preference for the F1 and F3 overlays.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use windows_sys::Win32::Globalization::{GetUserPreferredUILanguages, MUI_LANGUAGE_NAME};

use super::font::Script;

pub(super) const SETTING: &str = "VICTORY_UI_LANGUAGE";

/// English, Simplified Chinese, Traditional Chinese, in that order.
pub(super) type Text = [&'static str; 3];

pub(super) const fn plain(s: &'static str) -> Text {
    [s; 3]
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(super) enum Language {
    English,
    Simplified,
    Traditional,
}

impl Language {
    pub(super) fn text(self, text: Text) -> &'static str {
        text[self as usize]
    }

    pub(super) fn script(self) -> Script {
        match self {
            Self::English | Self::Simplified => Script::Simplified,
            Self::Traditional => Script::Traditional,
        }
    }
}

fn from_ui_language(name: &str) -> Language {
    let mut parts = name.split('-');
    if !parts.next().is_some_and(|p| p.eq_ignore_ascii_case("zh")) {
        return Language::English;
    }
    match parts.next().unwrap_or_default().to_ascii_lowercase().as_str() {
        "hant" | "cht" | "tw" | "hk" | "mo" => Language::Traditional,
        _ => Language::Simplified,
    }
}

fn windows_language() -> Language {
    static DETECTED: OnceLock<Language> = OnceLock::new();
    *DETECTED.get_or_init(|| {
        let mut count = 0;
        let mut len = 0;
        // SAFETY: query the size first, then give Windows a UTF-16 buffer of that size.
        let names = unsafe {
            if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, std::ptr::null_mut(), &mut len) == 0 || len == 0 {
                return Language::English;
            }
            let mut names = vec![0; len as usize];
            if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, names.as_mut_ptr(), &mut len) == 0 {
                return Language::English;
            }
            names
        };
        let first = names.split(|&c| c == 0).next().unwrap_or_default();
        from_ui_language(&String::from_utf16_lossy(first))
    })
}

pub(super) fn choice(saved: &str) -> &'static str {
    match saved {
        "en" => "en",
        "zh-Hans" => "zh-Hans",
        "zh-Hant" => "zh-Hant",
        _ => "auto",
    }
}

fn resolve(saved: &str, auto: impl FnOnce() -> Language) -> Language {
    match choice(saved) {
        "en" => Language::English,
        "zh-Hans" => Language::Simplified,
        "zh-Hant" => Language::Traditional,
        _ => auto(),
    }
}

fn state() -> &'static AtomicU8 {
    static ACTIVE: OnceLock<AtomicU8> = OnceLock::new();
    ACTIVE.get_or_init(|| {
        let saved = std::env::var(SETTING).unwrap_or_default();
        AtomicU8::new(resolve(&saved, windows_language) as u8)
    })
}

pub(super) fn current() -> Language {
    match state().load(Ordering::Relaxed) {
        value if value == Language::Simplified as u8 => Language::Simplified,
        value if value == Language::Traditional as u8 => Language::Traditional,
        _ => Language::English,
    }
}

pub(super) fn select(saved: &str) {
    state().store(resolve(saved, windows_language) as u8, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use Language::*;

    #[test]
    fn identifies_windows_display_language() {
        for tag in ["zh-Hans", "zh-CN", "zh-SG", "zh-Hans-TW"] {
            assert_eq!(from_ui_language(tag), Simplified, "{tag}");
        }
        for tag in ["zh-Hant", "zh-TW", "zh-HK", "ZH-mo", "zh-Hant-CN"] {
            assert_eq!(from_ui_language(tag), Traditional, "{tag}");
        }
        for tag in ["en-US", "ja-JP", "fr-FR", ""] {
            assert_eq!(from_ui_language(tag), English, "{tag}");
        }
    }

    #[test]
    fn saved_choice_overrides_windows_language() {
        assert_eq!(resolve("zh-Hant", || English), Traditional);
        assert_eq!(resolve("zh-Hans", || Traditional), Simplified);
        assert_eq!(resolve("en", || Simplified), English);
        for setting in ["auto", "", "invalid"] {
            assert_eq!(resolve(setting, || Traditional), Traditional);
        }
    }
}
