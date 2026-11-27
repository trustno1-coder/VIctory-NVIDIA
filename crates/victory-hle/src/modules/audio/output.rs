//! Whether Windows has an audio output for the title to play through.
//!
//! The title asks Windows for the default playback device
//! (`IMMDeviceEnumerator::GetDefaultAudioEndpoint(eRender, eConsole)`). With
//! none (every playback device unplugged, disabled or missing) it gets
//! `E_NOTFOUND`, and the title's own audio code then keeps a lock it never
//! lets go: the story's loading screen waits forever, and on the first screen
//! the title stops on its own check after about 15 seconds (issue #305). Any
//! playback device, even a virtual one, avoids it, so the start says so.

use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::core::HRESULT;

/// `HRESULT_FROM_WIN32(ERROR_NOT_FOUND)`: no device for that role.
const E_NOTFOUND: HRESULT = HRESULT(0x8007_0490_u32 as i32);

/// What the start says when there is no playback device: a few lines of at
/// most 52 characters, in what the picture's font draws.
pub const NO_OUTPUT_NOTICE: [&str; 3] = [
    "NO AUDIO OUTPUT: THE GAME FREEZES WHEN LOADING",
    "TURN ON A PLAYBACK DEVICE (EVEN A VIRTUAL ONE)",
    "IN WINDOWS SOUND SETTINGS, THEN START AGAIN",
];

/// `Some(true)` if Windows has a default playback device, `Some(false)` if
/// it has none, `None` if it could not be asked.
///
/// Asked on a thread of its own, so that the calling thread's COM apartment
/// is left for the title to choose.
pub fn has_playback_device() -> Option<bool> {
    std::thread::Builder::new()
        .name("victory audio output check".into())
        .spawn(ask)
        .ok()?
        .join()
        .ok()
        .flatten()
}

fn ask() -> Option<bool> {
    // SAFETY: COM is set up on this thread for the call and taken down after.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok().ok()?;
        let answer = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .ok()
            .and_then(|devices| match devices.GetDefaultAudioEndpoint(eRender, eConsole) {
                Ok(_) => Some(true),
                Err(e) if e.code() == E_NOTFOUND => Some(false),
                Err(_) => None,
            });
        CoUninitialize();
        answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_fits_the_picture() {
        for line in NO_OUTPUT_NOTICE {
            assert!(line.len() <= 52, "{line}");
            assert!(
                line.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || " .,:()/-+".contains(c)),
                "{line}"
            );
        }
    }

    #[test]
    fn asking_does_not_fail() {
        // Whatever this machine has, the question gets an answer or none, without a panic.
        let _ = has_playback_device();
    }
}
