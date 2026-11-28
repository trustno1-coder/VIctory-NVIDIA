//! Observe a stalled display without taking a renderer lock or calling the driver.
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

static SHOWN: AtomicU64 = AtomicU64::new(0);
static PRESENTING: AtomicBool = AtomicBool::new(false);

pub(super) fn begin_present() {
    PRESENTING.store(true, Relaxed);
}

pub(super) fn end_present(shown: bool) {
    if shown {
        SHOWN.fetch_add(1, Relaxed);
    }
    PRESENTING.store(false, Relaxed);
}

pub(super) fn start() {
    static START: Once = Once::new();
    START.call_once(|| {
        if !std::env::var_os("VICTORY_DISPLAY_WATCH").is_some_and(|v| v == "1") {
            return;
        }
        if let Err(error) = std::thread::Builder::new().name("victory-display-watch".into()).spawn(watch) {
            tracing::warn!(%error, "display monitoring could not start");
        }
    });
}

fn watch() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{IsHungAppWindow, IsIconic, IsWindow, IsWindowVisible};
    let mut shown = SHOWN.load(Relaxed);
    let mut last_change = Instant::now();
    let mut reported = false;
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let next = SHOWN.load(Relaxed);
        if next != shown {
            if reported {
                tracing::info!(seconds = last_change.elapsed().as_secs(), "display presentation resumed");
            }
            shown = next;
            last_change = Instant::now();
            reported = false;
        } else if !reported && last_change.elapsed() >= Duration::from_secs(20) {
            let hwnd = super::DISPLAY_WINDOW.load(Relaxed) as windows_sys::Win32::Foundation::HWND;
            // These are status queries; none sends a message to the window thread.
            let (exists, visible, minimized, hung) = unsafe {
                (IsWindow(hwnd) != 0, IsWindowVisible(hwnd) != 0, IsIconic(hwnd) != 0, IsHungAppWindow(hwnd) != 0)
            };
            let (submitted, completed) = super::submissions();
            tracing::warn!(
                frames_shown = shown, title_frame = crate::frame::current(), inside_present = PRESENTING.load(Relaxed),
                submitted, completed, window_exists = exists, visible, minimized, hung,
                "no frame displayed for 20 seconds"
            );
            reported = true;
        }
    }
}
