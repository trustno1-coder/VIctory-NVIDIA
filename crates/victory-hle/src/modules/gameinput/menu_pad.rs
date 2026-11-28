//! Observe the focused slot-zero pad for F1 and route button edges to its open menu.
use std::time::{Duration, Instant};
use victory_gpu::SettingsPadMode;

use super::{A, B, DOWN, LEFT, LEFT_STICK, PadState, RIGHT, RIGHT_STICK, UP};

const STICKS: u32 = LEFT_STICK | RIGHT_STICK;
const NAVIGATION: u32 = UP | DOWN | LEFT | RIGHT | A | B;
const CHORD_WINDOW: Duration = Duration::from_millis(250);
const TOGGLE: u16 = 0x70; // VK_F1

#[derive(Default)]
pub(super) struct MenuPad {
    previous: Option<u32>,
    chord_armed: bool,
    first_click: Option<Instant>,
    captured: u32,
}

impl MenuPad {
    /// Cancel outstanding actions and forget edges on disconnect, owner change or focus loss.
    pub(super) fn disconnected(&mut self, notify: impl FnOnce(Option<u16>)) {
        if self.previous.take().is_some() {
            notify(None);
        }
        self.captured = 0;
    }

    // Time qualifies only the overlay shortcut. Each stick bit passes through on every sample.
    fn shortcut(&mut self, sticks: u32, previous: u32, clock: impl FnOnce() -> Instant) -> bool {
        if sticks == 0 {
            self.chord_armed = true;
            self.first_click = None;
        } else if self.chord_armed && sticks != previous {
            if sticks != STICKS {
                self.first_click = Some(clock());
            } else {
                self.chord_armed = false;
                return self.first_click.is_none_or(|first| {
                    clock().saturating_duration_since(first) <= CHORD_WINDOW
                });
            }
        }
        false
    }

    pub(super) fn filter(
        &mut self,
        pad: &mut Option<PadState>,
        mode: SettingsPadMode,
        context: impl FnOnce() -> (bool, bool, bool),
        clock: impl FnOnce() -> Instant,
        notify: impl FnOnce(Option<u16>),
    ) {
        if mode == SettingsPadMode::Off {
            return;
        }
        let Some(pad) = pad else {
            self.disconnected(notify);
            return;
        };
        let (focused, open, pending) = context();
        if !focused {
            self.disconnected(notify);
            return;
        }
        let buttons = pad.buttons;
        let sticks = buttons & STICKS;
        let Some(previous) = self.previous.replace(buttons) else {
            self.first_click = None;
            self.chord_armed = sticks == 0;
            return;
        };
        let pressed = buttons & !previous & NAVIGATION;
        self.captured &= buttons;
        if mode == SettingsPadMode::StickShortcut && self.shortcut(sticks, previous & STICKS, clock) {
            notify(Some(TOGGLE));
        } else if open && !pending && pressed != 0 {
            self.captured |= pressed;
            // Settings can be saved immediately. Require a single navigation button.
            let key = match buttons & NAVIGATION {
                UP => Some(0x26),    // VK_UP
                DOWN => Some(0x28),  // VK_DOWN
                LEFT => Some(0x25),  // VK_LEFT
                RIGHT => Some(0x27), // VK_RIGHT
                A => Some(0x0d),     // VK_RETURN
                B => Some(0x1b),     // VK_ESCAPE
                _ => None,
            };
            if let Some(key) = key {
                notify(Some(key));
            }
        }
        pad.buttons &= !self.captured;
    }
}

#[cfg(test)]
mod tests;
