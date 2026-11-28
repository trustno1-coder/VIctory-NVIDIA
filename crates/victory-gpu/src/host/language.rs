use std::sync::OnceLock;

use windows_sys::Win32::Globalization::{GetUserPreferredUILanguages, MUI_LANGUAGE_NAME};

pub(crate) type Labels = [&'static str; 4];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Language {
    English,
    SimplifiedChinese,
    TraditionalChinese,
    Spanish,
}

impl Language {
    pub(crate) const ALL: [Self; 4] = [Self::English, Self::SimplifiedChinese, Self::TraditionalChinese, Self::Spanish];

    pub(crate) const fn index(self) -> usize {
        self as usize
    }

    pub(crate) fn text(self, labels: Labels) -> &'static str {
        labels[self.index()]
    }

    pub(crate) const fn unicode(self) -> bool {
        !matches!(self, Self::English)
    }

    pub(crate) fn from_setting(value: &str) -> Self {
        if value.eq_ignore_ascii_case("auto") {
            static DISPLAY: OnceLock<Language> = OnceLock::new();
            return *DISPLAY.get_or_init(windows_display_language);
        }
        Self::from_locale(value)
    }

    fn from_locale(locale: &str) -> Self {
        let mut parts = locale.split('-');
        let language = parts.next().unwrap_or_default();
        if language.eq_ignore_ascii_case("es") {
            return Self::Spanish;
        }
        if language.eq_ignore_ascii_case("zh") {
            return match parts.next().unwrap_or_default().to_ascii_lowercase().as_str() {
                "hant" | "tw" | "hk" | "mo" => Self::TraditionalChinese,
                _ => Self::SimplifiedChinese,
            };
        }
        Self::English
    }
}

fn windows_display_language() -> Language {
    let mut count = 0;
    let mut length = 0;
    // SAFETY: Windows fills the two size outputs when the language buffer is null.
    unsafe { GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, std::ptr::null_mut(), &mut length) };
    if length == 0 {
        return Language::English;
    }
    let mut names = vec![0; length as usize];
    // SAFETY: names provides length writable UTF-16 code units for the returned list.
    let read = unsafe { GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, names.as_mut_ptr(), &mut length) };
    if read == 0 {
        return Language::English;
    }
    let first = names.split(|&c| c == 0).next().unwrap_or_default();
    Language::from_locale(&String::from_utf16_lossy(first))
}
