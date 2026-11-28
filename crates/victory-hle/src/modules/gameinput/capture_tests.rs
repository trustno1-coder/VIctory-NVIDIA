//! Run on a private desktop with `--ignored --test-threads=1`. Set
//! VICTORY_MOUSE_CAPTURE, VICTORY_FULLSCREEN and VICTORY_MOUSE_WARP before
//! starting the test process, as the launcher does after an F1 restart.
//! Set VICTORY_TEST_ALT_TAB=1 to drive each requested switch with the desktop's
//! Alt-Tab shortcut. Otherwise the fixture changes foreground windows itself.
use super::{Source, host::Host};
use crate::modules::lifecycle;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
use windows_sys::Win32::UI::{Input::KeyboardAndMouse as kb, WindowsAndMessaging as wm};
use windows_sys::core::w;

struct Windows {
    title: HWND,
    other: HWND,
}

impl Drop for Windows {
    fn drop(&mut self) {
        // SAFETY: both windows belong to this test thread.
        unsafe {
            wm::DestroyWindow(self.title);
            wm::DestroyWindow(self.other);
        }
    }
}

fn clip() -> [i32; 4] {
    let mut r = RECT::default();
    // SAFETY: writable screen rectangle.
    assert_ne!(unsafe { wm::GetClipCursor(&mut r) }, 0);
    [r.left, r.top, r.right, r.bottom]
}

fn switch_to(to: HWND, title: HWND) {
    // SAFETY: both fixture windows and their message queue belong to this thread.
    unsafe {
        let desktop_shortcut = lifecycle::enabled("VICTORY_TEST_ALT_TAB");
        if desktop_shortcut {
            println!(
                "Press Alt-Tab now (foreground {:?}, target {to:?}, title {title:?})",
                wm::GetForegroundWindow()
            );
        }
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            let mut msg = wm::MSG::default();
            while wm::PeekMessageW(&mut msg, null_mut(), 0, 0, wm::PM_REMOVE) != 0 {
                wm::TranslateMessage(&msg);
                wm::DispatchMessageW(&msg);
            }
            if !desktop_shortcut {
                // Process pending activation messages before requesting focus.
                wm::SetForegroundWindow(to);
            }
            let fg = wm::GetForegroundWindow();
            if fg == to || (desktop_shortcut && to != title && !fg.is_null() && fg != title) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(wm::GetForegroundWindow(), to, "foreground switch timed out");
    }
}

#[test]
#[ignore = "takes over the foreground and moves the pointer on a private desktop"]
fn capture_on_a_private_desktop() {
    // SAFETY: the private desktop belongs to this fixture. Start with no prior clip.
    assert_ne!(unsafe { wm::ClipCursor(null()) }, 0);
    let desktop = clip();
    let style = wm::WS_VISIBLE | wm::WS_OVERLAPPEDWINDOW;
    // SAFETY: built-in class, top-level windows, all calls on their owner thread.
    unsafe {
        let other = wm::CreateWindowExW(
            0,
            w!("STATIC"),
            w!("Other"),
            style,
            300,
            200,
            360,
            240,
            null_mut(),
            null_mut(),
            null_mut(),
            null(),
        );
        let title = lifecycle::CreateWindowExW(
            0,
            w!("STATIC"),
            w!("Title"),
            style,
            40,
            50,
            400,
            300,
            null_mut(),
            null_mut(),
            null_mut(),
            null(),
        );
        let windows = Windows { title, other };
        assert!(!title.is_null() && !other.is_null());
        wm::SetForegroundWindow(title);
        kb::SetFocus(title);
        assert_eq!(wm::GetForegroundWindow(), title);
        let mut host = Host::default();
        let capture = lifecycle::enabled("VICTORY_MOUSE_CAPTURE");
        let fullscreen = lifecycle::enabled("VICTORY_FULLSCREEN");
        let warp = lifecycle::enabled("VICTORY_MOUSE_WARP");
        assert_eq!(
            wm::GetWindowLongW(title, wm::GWL_STYLE) as u32 & wm::WS_POPUP != 0,
            fullscreen
        );
        let mut area = RECT::default();
        let mut origin = POINT::default();
        assert_ne!(wm::GetClientRect(title, &mut area), 0);
        assert_ne!(ClientToScreen(title, &mut origin), 0);
        let client = [
            origin.x,
            origin.y,
            origin.x + area.right,
            origin.y + area.bottom,
        ];
        assert_eq!(clip(), if capture { client } else { desktop });
        assert_eq!(host.mouse().is_some(), capture);
        if capture {
            let center = (origin.x + area.right / 2, origin.y + area.bottom / 2);
            assert_ne!(wm::SetCursorPos(center.0, center.1), 0);
            let start = host.mouse().unwrap();
            // Eight milliseconds of synthetic pointer reports between guest polls.
            // The host samples the final screen position at each poll.
            for hz in [125, 500, 1000] {
                let before = host.mouse().unwrap();
                let mut point = POINT::default();
                assert_ne!(wm::GetCursorPos(&mut point), 0);
                let reports = hz / 125;
                let period = Duration::from_micros(1_000_000 / hz as u64);
                let began = Instant::now();
                for step in 1..=reports {
                    // A desktop sleep may round up to longer than the entire
                    // 8 ms poll. Pace this short, opt-in fixture with the clock.
                    while began.elapsed() < period * step as u32 {
                        std::hint::spin_loop();
                    }
                    assert_ne!(wm::SetCursorPos(point.x + step, point.y - step), 0);
                }
                let after = host.mouse().unwrap();
                println!("{hz} Hz: {reports} cursor updates in {:?}", began.elapsed());
                assert_eq!(after.position_x - before.position_x, i64::from(reports));
                assert_eq!(after.position_y - before.position_y, -i64::from(reports));
            }
            let last = host.mouse().unwrap();
            assert_eq!(last.position_x - start.position_x, 13);
            let mut p = POINT::default();
            assert_ne!(wm::GetCursorPos(&mut p), 0);
            if warp {
                assert_eq!((p.x, p.y), center);
            }

            let before = host.mouse().unwrap();
            victory_gpu::set_settings_menu_open(true);
            assert_eq!(host.mouse().unwrap().position_x, before.position_x);
            let mut point = POINT::default();
            assert_ne!(wm::GetCursorPos(&mut point), 0);
            assert_ne!(wm::SetCursorPos(point.x + 20, point.y), 0);
            victory_gpu::set_settings_menu_open(false);
            assert_eq!(
                host.mouse().unwrap().position_x,
                before.position_x,
                "motion made in F1 reached the first reading after close"
            );

            // Without a warp, two attempts past the same edge have the same
            // screen coordinate. Recentring supplies a fresh origin each poll.
            assert_ne!(wm::SetCursorPos(client[2] + 10, center.1), 0);
            let edge = host.mouse().unwrap();
            assert_ne!(wm::SetCursorPos(client[2] + 10, center.1), 0);
            assert_eq!(host.mouse().unwrap().position_x > edge.position_x, warp);
        }

        // Move/resize is a modal window operation. WM_MOVE and WM_SIZE can
        // recur inside it while the title is still the foreground window.
        wm::SendMessageW(title, wm::WM_ENTERSIZEMOVE, 0, 0);
        wm::SendMessageW(title, wm::WM_MOVE, 0, 0);
        wm::SendMessageW(title, wm::WM_SIZE, 0, 0);
        assert!(
            host.mouse().is_none(),
            "the host sampled the pointer during move/resize"
        );
        assert_eq!(clip(), desktop);
        wm::SendMessageW(title, wm::WM_EXITSIZEMOVE, 0, 0);
        assert_eq!(host.mouse().is_some(), capture);

        kb::SetFocus(null_mut());
        wm::SendMessageW(title, wm::WM_SIZE, 0, 0);
        assert!(
            host.mouse().is_none(),
            "geometry reclaimed capture after focus loss"
        );
        assert_eq!(clip(), desktop);
        kb::SetFocus(title);
        assert_eq!(host.mouse().is_some(), capture);

        // Switch away and back through user32, including a geometry update
        // on the background title while the other window owns the pointer.
        switch_to(other, title);
        assert_ne!(wm::GetForegroundWindow(), title);
        wm::SendMessageW(title, wm::WM_MOVE, 0, 0);
        assert!(host.mouse().is_none());
        assert_eq!(clip(), desktop);
        assert_ne!(wm::SetCursorPos(5, 5), 0);
        switch_to(title, title);
        assert_eq!(wm::GetForegroundWindow(), title);
        assert_eq!(lifecycle::captured_mouse_delta(), capture.then_some((0, 0)));
        assert_eq!(clip(), if capture { client } else { desktop });
        drop(windows);
        assert_eq!(clip(), desktop);
    }
}
