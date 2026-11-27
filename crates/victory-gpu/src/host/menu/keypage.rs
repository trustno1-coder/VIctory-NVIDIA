//! The F1 menu's keyboard and mouse page (F2): three tabs (on foot, vehicle,
//! mouse and options), every action of a tab with its two keys in columns, the
//! cell picked shown inverted. Enter sets the key picked (the next key or
//! mouse button pressed; Esc cancels), Delete or Backspace clears it,
//! Left/Right picks the column or changes an option, Tab the next tab. Every
//! change is in use at once and saved at once (`Done::Save`).

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_BACK, VK_DELETE, VK_DOWN, VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT, VK_TAB, VK_UP};

use super::{COLUMNS, LINES, Mark, REPO, ROWS};
use crate::keymap;

const TABS: [&str; 3] = ["ON FOOT", "VEHICLE", "MOUSE AND OPTIONS"];
/// Where the columns begin: the action, key 1, key 2 (or an option's value).
const ACTION: usize = 2;
const KEY1: usize = 31;
const KEY2: usize = 46;
const WIDTH: usize = 14;
/// The list's first line in the box (after the title, the tabs, the column names and a rule).
const TOP: usize = 4;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Row {
    Header(&'static str),
    Blank,
    Bind(usize, &'static str),
    Sensitivity,
    Invert,
    Auto,
    SwitchKey,
    Reset,
    Back,
}

impl Row {
    fn pickable(self) -> bool {
        !matches!(self, Row::Header(_) | Row::Blank)
    }
}

fn rows(tab: usize) -> Vec<Row> {
    let mut out = Vec::new();
    if tab < 2 {
        for (i, (group, actions)) in keymap::PAGES[tab].iter().enumerate() {
            if i > 0 {
                out.push(Row::Blank);
            }
            out.push(Row::Header(group));
            out.extend(actions.iter().map(|(c, name)| Row::Bind(*c as usize, name)));
        }
    } else {
        out.extend([Row::Header("MOUSE"), Row::Sensitivity, Row::Invert, Row::Blank]);
        out.extend([Row::Header("ON FOOT AND VEHICLE CONTROLS"), Row::Auto, Row::SwitchKey]);
    }
    out.extend([Row::Blank, Row::Reset, Row::Back]);
    out
}

/// What the menu does after a key on the page.
#[derive(PartialEq, Debug)]
pub(super) enum Done {
    Stay,
    /// A setting changed: save the keys.
    Save,
    /// Back to the settings.
    Back,
}

pub(super) struct KeyPage {
    tab: usize,
    /// The row picked, in [`rows`].
    at: usize,
    /// The key column picked: 0 or 1.
    slot: usize,
    /// Waiting for the key to set.
    pub(super) capture: bool,
    pub(super) note: String,
}

const MAX_SENSITIVITY: f32 = 2.0;

/// The next mouse sensitivity up or down: steps of 0.01 to 0.20, of 0.05 to
/// 1.00, then of 0.10, from 0.01 to [`MAX_SENSITIVITY`].
fn sensitivity_step(now: f32, up: bool) -> f32 {
    let hundredths = (now * 100.0).round() as i32;
    let step = |at: i32| if at < 20 { 1 } else if at < 100 { 5 } else { 10 };
    let next = if up { hundredths + step(hundredths) } else { hundredths - step(hundredths - 1) };
    // Back on the grid of the steps (a value saved off it, such as 0.08, stays reachable).
    let next = if next >= 20 { next / step(next) * step(next) } else { next };
    next.clamp(1, (MAX_SENSITIVITY * 100.0) as i32) as f32 / 100.0
}

fn on_off(on: bool) -> &'static str {
    if on { "ON" } else { "OFF" }
}

/// `text` written into `line` from column `at` (the line padded with spaces).
fn put(line: &mut String, at: usize, text: &str) {
    while line.len() < at {
        line.push(' ');
    }
    line.truncate(at);
    line.push_str(text);
}

impl KeyPage {
    /// On the tab of the profile in use.
    pub(super) fn new() -> KeyPage {
        let mut page = KeyPage { tab: keymap::profile(), at: 0, slot: 0, capture: false, note: String::new() };
        page.first();
        page
    }

    fn first(&mut self) {
        self.at = rows(self.tab).iter().position(|r| r.pickable()).unwrap_or(0);
        self.slot = 0;
    }

    fn row(&self) -> Row {
        rows(self.tab)[self.at]
    }

    /// The box: its lines, and the cells shown inverted.
    pub(super) fn view(&self) -> (Vec<String>, Vec<Mark>) {
        let mut marks = Vec::new();
        let mut title = "KEYBOARD AND MOUSE".to_owned();
        put(&mut title, COLUMNS as usize - "F1 CLOSES".len(), "F1 CLOSES");
        let mut tabs = String::new();
        let mut at = ACTION;
        for (i, name) in TABS.iter().enumerate() {
            put(&mut tabs, at, &format!(" {name} "));
            if i == self.tab {
                marks.push((1, at, at + name.len() + 2));
            }
            at += name.len() + 4;
        }
        put(&mut tabs, COLUMNS as usize - "TAB: NEXT".len(), "TAB: NEXT");
        let mut names = String::new();
        if self.tab < 2 {
            put(&mut names, ACTION, "ACTION");
            put(&mut names, KEY1, "KEY 1");
            put(&mut names, KEY2, "KEY 2");
        } else {
            put(&mut names, ACTION, "OPTION");
            put(&mut names, KEY1, "SETTING");
        }
        let mut out = vec![title, tabs, names, "-".repeat(COLUMNS as usize)];

        let rows = rows(self.tab);
        let first = self.at.saturating_sub(ROWS / 2).min(rows.len().saturating_sub(ROWS));
        for (n, i) in (first..(first + ROWS).min(rows.len())).enumerate() {
            let picked = i == self.at;
            let line_no = TOP + n;
            let mut line = String::from(if picked { ">" } else { " " });
            let value = |line: &mut String, marks: &mut Vec<Mark>, text: &str| {
                put(line, KEY1, text);
                if picked {
                    marks.push((line_no, KEY1 - 1, KEY1 + text.len() + 1));
                }
            };
            match rows[i] {
                Row::Header(name) => put(&mut line, 0, name),
                Row::Blank => {}
                Row::Bind(control, name) => {
                    put(&mut line, ACTION, name);
                    for (slot, column) in [KEY1, KEY2].into_iter().enumerate() {
                        let vk = keymap::keys(self.tab, control)[slot];
                        let text = if picked && self.capture && slot == self.slot { "PRESS A KEY".to_owned() } else { keymap::name(vk) };
                        put(&mut line, column, &text);
                        if picked && slot == self.slot {
                            marks.push((line_no, column - 1, column + WIDTH - 1));
                        }
                    }
                }
                Row::Sensitivity => {
                    put(&mut line, ACTION, "MOUSE SENSITIVITY");
                    let scale = keymap::mouse_scale();
                    let bar = ((scale / MAX_SENSITIVITY * 12.0).round() as usize).min(12);
                    value(&mut line, &mut marks, &format!("< {:.2} > {}{}", scale, "=".repeat(bar), "-".repeat(12 - bar)));
                }
                Row::Invert => {
                    put(&mut line, ACTION, "INVERT MOUSE Y");
                    value(&mut line, &mut marks, &format!("< {} >", on_off(keymap::mouse_inverted())));
                }
                Row::Auto => {
                    put(&mut line, ACTION, "SWITCH WITH INTERACT (F)");
                    value(&mut line, &mut marks, &format!("< {} >", on_off(keymap::auto_switch())));
                }
                Row::SwitchKey => {
                    put(&mut line, ACTION, "SWITCH KEY");
                    let text = if picked && self.capture { "PRESS A KEY".to_owned() } else { keymap::name(keymap::switch_key()) };
                    value(&mut line, &mut marks, &text);
                }
                Row::Reset | Row::Back => {
                    let text = if rows[i] == Row::Back {
                        "BACK TO SETTINGS".to_owned()
                    } else {
                        format!("RESET {} TO DEFAULTS", TABS[self.tab])
                    };
                    put(&mut line, ACTION, &text);
                    if picked {
                        marks.push((line_no, ACTION - 1, ACTION + text.len() + 1));
                    }
                }
            }
            out.push(line.chars().take(COLUMNS as usize).collect());
        }
        while out.len() < TOP + ROWS {
            out.push(String::new());
        }
        out.push(String::new());
        let help: [&str; 2] = if self.capture {
            ["PRESS THE NEW KEY OR MOUSE BUTTON", "ESC CANCELS"]
        } else if self.tab < 2 {
            ["UP/DOWN ACTION  LEFT/RIGHT KEY 1 OR 2  ENTER CHANGE", "DELETE CLEARS  TAB NEXT PAGE  ESC BACK"]
        } else {
            ["UP/DOWN OPTION  LEFT/RIGHT CHANGE  ENTER SET KEY", "TAB NEXT PAGE  ESC BACK"]
        };
        out.extend(help.iter().map(|l| (*l).to_owned()));
        let note = if self.note.is_empty() { "EVERY CHANGE IS IN USE AT ONCE AND SAVED" } else { &self.note };
        out.push(note.chars().take(COLUMNS as usize).collect());
        out.push(REPO.to_owned());
        debug_assert_eq!(out.len(), LINES as usize);
        (out, marks)
    }

    /// A key pressed while the page is shown.
    pub(super) fn key(&mut self, vk: u16) -> Done {
        let row = self.row();
        if self.capture {
            self.capture = false;
            if vk == VK_ESCAPE {
                self.note.clear();
                return Done::Stay;
            }
            let vk = vk as u8;
            match row {
                Row::SwitchKey => {
                    keymap::set_switch_key(vk);
                    self.note = format!("SWITCH KEY: {}", keymap::name(vk));
                }
                Row::Bind(control, name) => {
                    keymap::set_key(self.tab, control, self.slot, vk);
                    let also = keymap::also_on(self.tab, vk, control);
                    self.note = if also.is_empty() {
                        format!("{name}: {}", keymap::name(vk))
                    } else {
                        format!("{} ALSO DOES: {}", keymap::name(vk), also.join(", "))
                    };
                }
                _ => return Done::Stay,
            }
            return Done::Save;
        }
        let rows = rows(self.tab);
        let step = |from: usize, by: usize| {
            let mut at = from;
            loop {
                at = (at + by) % rows.len();
                if rows[at].pickable() {
                    return at;
                }
            }
        };
        let change = match vk {
            VK_UP => {
                self.at = step(self.at, rows.len() - 1);
                false
            }
            VK_DOWN => {
                self.at = step(self.at, 1);
                false
            }
            VK_TAB => {
                self.tab = (self.tab + 1) % TABS.len();
                self.first();
                self.note.clear();
                false
            }
            VK_ESCAPE => return Done::Back,
            _ => match (row, vk) {
                (Row::Bind(..), VK_LEFT | VK_RIGHT) => {
                    self.slot ^= 1;
                    false
                }
                (Row::Bind(..) | Row::SwitchKey, VK_RETURN) => {
                    self.capture = true;
                    self.note.clear();
                    false
                }
                (Row::Bind(control, name), VK_DELETE | VK_BACK) => {
                    keymap::set_key(self.tab, control, self.slot, 0);
                    self.note = format!("{name}: KEY {} CLEARED", self.slot + 1);
                    true
                }
                (Row::Sensitivity, VK_LEFT | VK_RIGHT) => {
                    keymap::set_mouse_scale(sensitivity_step(keymap::mouse_scale(), vk == VK_RIGHT));
                    true
                }
                (Row::Invert, VK_LEFT | VK_RIGHT | VK_RETURN) => {
                    keymap::set_mouse_inverted(!keymap::mouse_inverted());
                    true
                }
                (Row::Auto, VK_LEFT | VK_RIGHT | VK_RETURN) => {
                    keymap::set_auto_switch(!keymap::auto_switch());
                    true
                }
                (Row::Reset, VK_RETURN) => {
                    if self.tab < 2 {
                        keymap::reset_profile(self.tab);
                    } else {
                        keymap::reset_options();
                    }
                    self.note = format!("{} BACK TO DEFAULTS", TABS[self.tab]);
                    true
                }
                (Row::Back, VK_RETURN) => return Done::Back,
                _ => false,
            },
        };
        if change { Done::Save } else { Done::Stay }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::font::glyph;

    #[test]
    fn every_line_fits_and_every_character_has_a_picture() {
        for tab in 0..TABS.len() {
            for at in 0..rows(tab).len() {
                for capture in [false, true] {
                    let page = KeyPage { tab, at, slot: at % 2, capture, note: String::new() };
                    let (lines, marks) = page.view();
                    assert_eq!(lines.len(), LINES as usize);
                    for line in &lines {
                        assert!(line.len() <= COLUMNS as usize, "{line}");
                        assert!(line.bytes().all(|c| glyph(c.to_ascii_uppercase()) != glyph(b'#')), "{line}");
                    }
                    assert!(marks.iter().all(|&(r, a, b)| r < lines.len() && a < b && b <= COLUMNS as usize), "{marks:?}");
                }
            }
        }
    }

    #[test]
    fn sensitivity_steps_both_ways_to_the_top_and_back() {
        let mut at = 0.08;
        let mut ups = vec![at];
        while at < MAX_SENSITIVITY {
            at = sensitivity_step(at, true);
            ups.push(at);
        }
        assert_eq!(at, MAX_SENSITIVITY);
        assert!(ups.windows(2).all(|w| w[1] > w[0]), "{ups:?}");
        assert!(ups.contains(&0.2) && ups.contains(&0.25) && ups.contains(&1.1), "{ups:?}");
        for &want in ups.iter().rev().skip(1) {
            at = sensitivity_step(at, false);
            assert!((at - want).abs() < 1e-6, "{at} {want}");
        }
        assert_eq!(sensitivity_step(0.01, false), 0.01);
    }

    #[test]
    fn keys_move_over_actions_only_and_set_one() {
        let mut page = KeyPage { tab: 1, at: 0, slot: 0, capture: false, note: String::new() };
        page.first();
        assert!(matches!(page.row(), Row::Bind(..)));
        // Down skips the headers and blanks; Up from the first wraps to Back.
        for _ in 0..40 {
            page.key(VK_DOWN);
            assert!(page.row().pickable());
        }
        page.first();
        page.key(VK_UP);
        assert_eq!(page.row(), Row::Back);
        // Look up in the vehicle (no key by default), key 2: set to F11, then cleared.
        let look_up = rows(1).iter().position(|r| matches!(r, Row::Bind(c, _) if *c == keymap::Control::LookUp as usize)).unwrap();
        page.at = look_up;
        page.key(VK_RIGHT);
        assert_eq!(page.key(VK_RETURN), Done::Stay);
        assert!(page.capture);
        assert_eq!(page.key(0x7A), Done::Save);
        assert_eq!(keymap::keys(1, keymap::Control::LookUp as usize)[1], 0x7A);
        assert_eq!(page.key(VK_DELETE), Done::Save);
        assert_eq!(keymap::keys(1, keymap::Control::LookUp as usize)[1], 0);
        // Tab goes round the three pages; Esc goes back.
        for tab in [2, 0, 1] {
            page.key(VK_TAB);
            assert_eq!(page.tab, tab);
        }
        assert_eq!(page.key(VK_ESCAPE), Done::Back);
    }
}
