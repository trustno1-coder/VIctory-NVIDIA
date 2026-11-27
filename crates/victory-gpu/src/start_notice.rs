//! Lines the start adds to the notice at the top of the picture
//! (`host::panel::Notice`), above the warm-up and frame generation lines.

use std::sync::Mutex;

static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Adds `lines` to the start's notice. Call before the title makes its swap
/// chain: the notice is put together once, when the first frame is shown.
pub fn add(lines: &[&str]) {
    LINES.lock().unwrap_or_else(|e| e.into_inner()).extend(lines.iter().map(|l| l.to_string()));
}

/// What [`add`] was given, in order.
pub(crate) fn lines() -> Vec<String> {
    LINES.lock().unwrap_or_else(|e| e.into_inner()).clone()
}
