use super::{
    A, B, DOWN, LEFT, LEFT_SHOULDER, LEFT_STICK, MENU, PadState, RIGHT, RIGHT_SHOULDER,
    RIGHT_STICK, UP, VIEW, X, Y,
};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows_sys::Win32::{
    Foundation::{HWND, POINT, RECT},
    UI::{Input::KeyboardAndMouse::GetAsyncKeyState, WindowsAndMessaging as wm},
};

const BUTTONS: &[(&str, u32)] = &[
    ("A", A),
    ("B", B),
    ("X", X),
    ("Y", Y),
    ("LB", LEFT_SHOULDER),
    ("RB", RIGHT_SHOULDER),
    ("LS", LEFT_STICK),
    ("RS", RIGHT_STICK),
    ("Menu", MENU),
    ("View", VIEW),
    ("DpadUp", UP),
    ("DpadDown", DOWN),
    ("DpadLeft", LEFT),
    ("DpadRight", RIGHT),
];
const AXES: [&str; 6] = ["LT", "RT", "MoveLeft", "MoveRight", "MoveDown", "MoveUp"];

struct Bindings {
    buttons: Vec<(u8, u32)>,
    axes: [Vec<u8>; 6],
    sensitivity: f32,
}

fn key(name: &str) -> Option<u8> {
    let name = name.trim().to_ascii_uppercase();
    if name.len() == 1 && name.as_bytes()[0].is_ascii_alphanumeric() {
        return Some(name.as_bytes()[0]);
    }
    if let Some(hex) = name.strip_prefix("0X") {
        return u8::from_str_radix(hex, 16)
            .ok()
            .filter(|v| *v > 0 && *v < 255);
    }
    Some(match name.as_str() {
        "MOUSE1" => 1,
        "MOUSE2" => 2,
        "MOUSE3" => 4,
        "MOUSE4" => 5,
        "MOUSE5" => 6,
        "BACKSPACE" => 8,
        "TAB" => 9,
        "ENTER" => 13,
        "SHIFT" => 16,
        "CTRL" => 17,
        "ALT" => 18,
        "ESC" => 27,
        "SPACE" => 32,
        "LEFT" => 37,
        "UP" => 38,
        "RIGHT" => 39,
        "DOWN" => 40,
        _ => return None,
    })
}

fn parse(file: &str) -> Result<Bindings, String> {
    let mut b = Bindings {
        buttons: Vec::new(),
        axes: Default::default(),
        sensitivity: 0.045,
    };
    for (i, line) in file.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let err = || format!("Invalid binding on line {}: {}", i + 1, line);
        let (out, input) = line.split_once('=').ok_or_else(err)?;
        let out = out.trim();
        if out.eq_ignore_ascii_case("Sensitivity") {
            b.sensitivity = input
                .trim()
                .parse::<f32>()
                .ok()
                .filter(|s| s.is_finite() && *s > 0.0)
                .ok_or_else(err)?;
            continue;
        }
        let keys = input
            .split(',')
            .map(|k| key(k).ok_or_else(err))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some((_, bit)) = BUTTONS
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(out))
        {
            b.buttons.extend(keys.into_iter().map(|k| (k, *bit)));
        } else {
            let index = AXES
                .iter()
                .position(|s| s.eq_ignore_ascii_case(out))
                .ok_or_else(err)?;
            b.axes[index].extend(keys);
        }
    }
    Ok(b)
}

fn config() -> Option<&'static Bindings> {
    static CONFIG: OnceLock<Option<Bindings>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let file = std::env::var_os("VICTORY_MNK_BINDINGS")?;
            match std::fs::read_to_string(file)
                .map_err(|e| e.to_string())
                .and_then(|s| parse(&s))
            {
                Ok(b) => Some(b),
                Err(e) => {
                    eprintln!("Mouse and keyboard disabled: {e}");
                    None
                }
            }
        })
        .as_ref()
}

pub(crate) fn enabled() -> bool {
    config().is_some()
}

#[derive(Default)]
struct Mouse {
    window: usize,
    velocity: [f32; 2],
    stick: [f32; 2],
    last: Option<Instant>,
    last_motion: Option<Instant>,
    centered: bool,
}
fn mouse() -> std::sync::MutexGuard<'static, Mouse> {
    static STATE: OnceLock<Mutex<Mouse>> = OnceLock::new();
    STATE.get_or_init(Default::default).lock().unwrap()
}

// Sample cursor displacement on the input polling thread, independently of frame delivery.
fn recenter(hwnd: HWND) -> Option<[i32; 2]> {
    let mut rect = RECT::default();
    let mut cursor = POINT::default();
    unsafe {
        if wm::GetClientRect(hwnd, &mut rect) == 0 || rect.right <= 0 || rect.bottom <= 0 {
            return None;
        }
        let mut center = POINT {
            x: rect.right / 2,
            y: rect.bottom / 2,
        };
        if windows_sys::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut center) == 0
            || wm::GetCursorPos(&mut cursor) == 0
            || wm::GetForegroundWindow() != hwnd
        {
            return None;
        }
        let delta = [cursor.x - center.x, cursor.y - center.y];
        if delta != [0, 0] && wm::SetCursorPos(center.x, center.y) == 0 {
            return None;
        }
        Some(delta)
    }
}

impl Mouse {
    fn update_look(&mut self, delta: [i32; 2], now: Instant, elapsed: Duration, sensitivity: f32) {
        if delta != [0, 0] {
            self.last_motion = Some(now);
        }
        if self
            .last_motion
            .is_none_or(|t| now.duration_since(t) >= Duration::from_millis(40))
        {
            self.velocity = [0.0; 2];
            self.stick = [0.0; 2];
            return;
        }
        let seconds = elapsed.as_secs_f32();
        let weight = 1.0 - (-seconds / 0.020).exp();
        // Filter velocity before limiting it. Clamping bursts first loses motion at low FPS.
        for (v, d) in self
            .velocity
            .iter_mut()
            .zip([delta[0] as f32, -(delta[1] as f32)])
        {
            let target = d * sensitivity * 0.008 / seconds;
            *v += weight * (target - *v);
        }
        let length = self.velocity[0].hypot(self.velocity[1]);
        if length <= 0.001 {
            self.stick = [0.0; 2];
        } else {
            // Map small movements beyond the controller dead zone, retaining direction.
            let gain = (0.24 + 0.76 * length.min(1.0)) / length;
            self.stick = self.velocity.map(|v| v * gain);
        }
    }
}
pub(super) fn sample() -> Option<PadState> {
    let b = config()?;
    let mut m = mouse();
    let hwnd = unsafe { wm::GetForegroundWindow() };
    let mut pid = 0;
    unsafe { wm::GetWindowThreadProcessId(hwnd, &mut pid) };
    if hwnd.is_null() || pid != std::process::id() {
        *m = Mouse::default();
        return Some(PadState::default());
    }
    if m.window != hwnd as usize {
        *m = Mouse {
            window: hwnd as usize,
            ..Default::default()
        };
    }
    let now = Instant::now();
    let elapsed = m
        .last
        .map_or(Duration::from_millis(8), |last| now.duration_since(last));
    if elapsed >= Duration::from_millis(8) {
        let displacement = recenter(hwnd);
        let delta = if m.centered {
            displacement.unwrap_or_default()
        } else {
            [0, 0]
        };
        m.centered = displacement.is_some();
        m.update_look(delta, now, elapsed, b.sensitivity);
        m.last = Some(now);
    }
    let held = |vk: u8| (unsafe { GetAsyncKeyState(vk.into()) }) < 0;
    let axes = b
        .axes
        .each_ref()
        .map(|keys| u8::from(keys.iter().copied().any(held)) as f32);
    Some(PadState {
        buttons: b
            .buttons
            .iter()
            .filter(|(vk, _)| held(*vk))
            .fold(0, |v, (_, bit)| v | bit),
        left_trigger: axes[0],
        right_trigger: axes[1],
        left_x: axes[3] - axes[2],
        left_y: axes[5] - axes[4],
        right_x: m.stick[0],
        right_y: m.stick[1],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn continuous_motion_and_release() {
        let mut m = Mouse::default();
        let start = Instant::now();
        let dt = Duration::from_millis(8);
        m.update_look([1, 0], start, dt, 0.135);
        assert!(m.stick[0] > 0.24);
        for i in 1..12 {
            m.update_look([20, -20], start + dt * i, dt, 0.135);
        }
        assert!((m.stick[0] - m.stick[1]).abs() < 0.001);
        assert!((m.stick[0].hypot(m.stick[1]) - 1.0).abs() < 0.001);
        m.update_look([0, 0], start + dt * 12, dt, 0.135);
        assert!(m.stick[0] > 0.0);
        m.update_look([0, 0], start + dt * 17, dt, 0.135);
        assert_eq!(m.stick, [0.0; 2]);
    }

    #[test]
    fn shipped_profile_maps_triggers_and_vehicle() {
        let b = parse(include_str!("../../../../bindings.ini")).unwrap();
        assert_eq!(b.axes[1], [1]);
        assert_eq!(b.axes[0], [2]);
        assert!(b.buttons.contains(&(b'F', Y)));
        assert!(parse("RT=NotAKey").is_err());
    }
}
