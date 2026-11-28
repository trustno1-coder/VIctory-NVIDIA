use super::{Duration, Instant, MenuPad, SettingsPadMode};
use super::super::{A, B, DOWN, LEFT, LEFT_STICK, PadState, RIGHT, RIGHT_STICK, UP, X};

const BOTH: u32 = LEFT_STICK | RIGHT_STICK;
const F1: u16 = 0x70;
const FOCUSED: (bool, bool, bool) = (true, false, false);
const OPEN: (bool, bool, bool) = (true, true, false);

fn reading(buttons: u32) -> PadState {
    PadState {
        buttons,
        left_trigger: 0.25,
        right_trigger: 0.75,
        left_x: -0.5,
        left_y: 0.5,
        right_x: 0.125,
        right_y: -0.875,
    }
}

struct Probe {
    mode: SettingsPadMode,
    pad: MenuPad,
    origin: Instant,
    actions: Vec<Option<u16>>,
}

impl Probe {
    fn new(mode: SettingsPadMode) -> Self {
        Self { mode, pad: MenuPad::default(), origin: Instant::now(), actions: Vec::new() }
    }

    // The full guest reading (including every analog field) is compared on each poll.
    fn sample(&mut self, ms: u64, raw: u32, context: (bool, bool, bool), expected: u32) {
        let mut pad = Some(reading(raw));
        self.pad.filter(&mut pad, self.mode, || context, || self.origin + Duration::from_millis(ms), |key| self.actions.push(key));
        assert_eq!(pad, Some(reading(expected)), "t={ms}ms raw={raw:#x}");
    }
}

#[test]
fn disabled_does_not_query_focus_time_or_send_events() {
    let mut filter = MenuPad::default();
    for original in [None, Some(reading(BOTH | UP | A | X))] {
        let mut pad = original;
        filter.filter(&mut pad, SettingsPadMode::Off, || panic!("focus"), || panic!("clock"), |_| panic!("event"));
        assert_eq!(pad, original);
    }
}

#[test]
fn single_clicks_in_both_orders_reach_the_guest_on_the_same_poll() {
    for first in [LEFT_STICK, RIGHT_STICK] {
        let mut p = Probe::new(SettingsPadMode::StickShortcut);
        for (ms, raw) in [(0, 0), (1, first), (2, 0), (3, BOTH ^ first), (500, BOTH ^ first), (501, 0)] {
            p.sample(ms, raw | X, FOCUSED, raw | X);
        }
        assert!(p.actions.is_empty());
    }
}

#[test]
fn overlapping_clicks_pass_through_and_only_the_shortcut_has_a_deadline() {
    for first in [LEFT_STICK, RIGHT_STICK] {
        for delta in [0, 1, 249, 250, 251, 5_000] {
            let mut p = Probe::new(SettingsPadMode::StickShortcut);
            for (ms, raw) in [(0, 0), (10, first), (10 + delta, BOTH), (20 + delta, BOTH), (30 + delta, first), (40 + delta, BOTH)] {
                p.sample(ms, raw | A, FOCUSED, raw | A);
            }
            let expected: Vec<_> = (delta <= 250).then_some(Some(F1)).into_iter().collect();
            assert_eq!(p.actions, expected, "gap={delta}ms, first={first:#x}");
        }
    }
}

#[test]
fn same_poll_chord_rearms_only_after_both_sticks_are_up() {
    let mut p = Probe::new(SettingsPadMode::StickShortcut);
    for (ms, raw) in [(0, 0), (2, BOTH), (3, LEFT_STICK), (4, BOTH), (5, RIGHT_STICK), (6, BOTH), (7, 0), (8, BOTH)] {
        p.sample(ms, raw, FOCUSED, raw);
    }
    assert_eq!(p.actions, [Some(F1), Some(F1)]);
}

#[test]
fn swapping_single_sticks_starts_a_new_observation_window() {
    for first in [LEFT_STICK, RIGHT_STICK] {
        let mut p = Probe::new(SettingsPadMode::StickShortcut);
        for (ms, raw) in [(0, 0), (2, first), (500, BOTH ^ first), (550, BOTH), (551, first), (552, BOTH), (553, 0), (555, BOTH)] {
            p.sample(ms, raw, FOCUSED, raw);
        }
        assert_eq!(p.actions, [Some(F1), Some(F1)]);
    }
}

#[test]
fn menu_only_does_not_use_the_chord_or_clock() {
    let mut state = MenuPad::default();
    for open in [false, true] {
        for raw in [0, LEFT_STICK, BOTH, RIGHT_STICK, 0, BOTH, 0] {
            let mut pad = Some(reading(raw | X));
            state.filter(&mut pad, SettingsPadMode::MenuOnly, || (true, open, false), || panic!("clock"), |_| panic!("shortcut"));
            assert_eq!(pad, Some(reading(raw | X)));
        }
    }
}

#[test]
fn only_fresh_open_menu_navigation_is_captured_and_held_closing_press_is_not_replayed() {
    for mode in [SettingsPadMode::StickShortcut, SettingsPadMode::MenuOnly] {
        for (button, key) in [(UP, 0x26), (DOWN, 0x28), (LEFT, 0x25), (RIGHT, 0x27), (A, 0x0d), (B, 0x1b)] {
            let mut p = Probe::new(mode);
            p.sample(0, 0, FOCUSED, 0);
            p.sample(1, button | X, (true, false, true), button | X);
            p.sample(2, button | X, OPEN, button | X);
            p.sample(3, 0, OPEN, 0);
            p.sample(4, button | X | LEFT_STICK, OPEN, X | LEFT_STICK);
            p.sample(5, button | X | LEFT_STICK, OPEN, X | LEFT_STICK);
            p.sample(6, button | X, FOCUSED, X);
            p.sample(7, 0, FOCUSED, 0);
            p.sample(8, button | X, FOCUSED, button | X);
            assert_eq!(p.actions, [Some(key)]);
        }
    }
}

#[test]
fn pending_transitions_pass_new_presses_in_either_logical_state() {
    for open in [false, true] {
        let mut p = Probe::new(SettingsPadMode::StickShortcut);
        p.sample(0, 0, OPEN, 0);
        p.sample(1, B | X, (true, open, true), B | X);
        p.sample(2, B | X, OPEN, B | X);
        p.sample(3, 0, OPEN, 0);
        p.sample(4, B | X, OPEN, X);
        assert_eq!(p.actions, [Some(0x1b)]);
    }
}

#[test]
fn a_new_navigation_button_on_the_closing_chord_goes_to_the_guest() {
    let mut p = Probe::new(SettingsPadMode::StickShortcut);
    p.sample(0, 0, OPEN, 0);
    p.sample(1, BOTH | A | X, OPEN, BOTH | A | X);
    p.sample(2, BOTH | A | X, (true, true, true), BOTH | A | X);
    p.sample(3, A | X, FOCUSED, A | X);
    assert_eq!(p.actions, [Some(F1)]);
}

#[test]
fn every_background_reading_is_unchanged_even_when_the_menu_was_capturing() {
    for mode in [SettingsPadMode::StickShortcut, SettingsPadMode::MenuOnly] {
        for pending in [false, true] {
            let mut p = Probe::new(mode);
            p.sample(0, 0, OPEN, 0);
            p.sample(1, B | X, OPEN, X);
            for (ms, raw) in [(2, B | X), (3, B | UP | BOTH | X), (4, 0), (5, BOTH | UP)] {
                p.sample(ms, raw, (false, true, pending), raw);
            }
            p.sample(6, BOTH | UP, OPEN, BOTH | UP);
            p.sample(7, BOTH | UP, OPEN, BOTH | UP);
            p.sample(8, 0, OPEN, 0);
            p.sample(9, UP, OPEN, 0);
            assert_eq!(p.actions, [Some(0x1b), None, Some(0x26)]);
        }
    }
}

#[test]
fn losing_focus_or_replacing_the_pad_between_stick_clicks_abandons_the_chord() {
    for disconnect in [false, true] {
        let mut p = Probe::new(SettingsPadMode::StickShortcut);
        p.sample(0, 0, FOCUSED, 0);
        p.sample(1, LEFT_STICK, FOCUSED, LEFT_STICK);
        if disconnect {
            p.pad.filter(&mut None, p.mode, || panic!("focus"), || panic!("clock"), |key| p.actions.push(key));
        } else {
            p.sample(2, LEFT_STICK, (false, false, false), LEFT_STICK);
        }
        p.sample(3, BOTH, FOCUSED, BOTH);
        p.sample(4, 0, FOCUSED, 0);
        p.sample(5, BOTH, FOCUSED, BOTH);
        assert_eq!(p.actions, [None, Some(F1)]);
    }
}

#[test]
fn menus_ignore_diagonals_and_keep_unmapped_buttons_unchanged() {
    for vertical in [UP, DOWN] {
        for horizontal in [LEFT, RIGHT] {
            let mut p = Probe::new(SettingsPadMode::MenuOnly);
            p.sample(0, 0, OPEN, 0);
            p.sample(1, vertical | horizontal | X, OPEN, X);
            p.sample(2, 0, OPEN, 0);
            p.sample(3, A | B | X, OPEN, X);
            assert!(p.actions.is_empty());
        }
    }
}
