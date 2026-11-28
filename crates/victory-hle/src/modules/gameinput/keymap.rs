//! Which keyboard key presses which control of the stand-in pad.
//!
//! `VICTORY_KEYMAP` changes any of them: `control=key` pairs separated by
//! commas, the key as a hexadecimal virtual-key code, a letter counting by
//! its place on a US keyboard (see `pad_key`). Example: `a=20,lt=A2` puts A
//! on Space and the left trigger on the left Ctrl. The graphical launcher
//! (`apps/launcher`) writes it; its list of controls is the same.

use std::sync::LazyLock;

use super::{
    A, B, DOWN, LEFT, LEFT_SHOULDER, LEFT_STICK, MENU, PadState, RIGHT, RIGHT_SHOULDER,
    RIGHT_STICK, UP, VIEW, X, Y,
};

/// What a key does on the pad.
#[derive(Clone, Copy)]
enum Effect {
    Button(u32),
    LeftTrigger,
    RightTrigger,
    /// Stick (0 left, 1 right), axis (0 x, 1 y) and direction.
    Stick(usize, usize, f32),
}

/// Every control: its name in `VICTORY_KEYMAP`, what it does, its key.
const CONTROLS: [(&str, Effect, u8); 24] = [
    ("a", Effect::Button(A), 0x0D),
    ("b", Effect::Button(B), 0x08),
    ("x", Effect::Button(X), b'X'),
    ("y", Effect::Button(Y), b'Y'),
    ("menu", Effect::Button(MENU), 0x1B),
    ("view", Effect::Button(VIEW), 0x09),
    ("up", Effect::Button(UP), 0x26),
    ("down", Effect::Button(DOWN), 0x28),
    ("left", Effect::Button(LEFT), 0x25),
    ("right", Effect::Button(RIGHT), 0x27),
    ("lb", Effect::Button(LEFT_SHOULDER), b'Q'),
    ("rb", Effect::Button(RIGHT_SHOULDER), b'E'),
    ("ls", Effect::Button(LEFT_STICK), b'F'),
    ("rs", Effect::Button(RIGHT_STICK), b'H'),
    ("lt", Effect::LeftTrigger, b'Z'),
    ("rt", Effect::RightTrigger, b'C'),
    ("ls_up", Effect::Stick(0, 1, 1.0), b'W'),
    ("ls_down", Effect::Stick(0, 1, -1.0), b'S'),
    ("ls_left", Effect::Stick(0, 0, -1.0), b'A'),
    ("ls_right", Effect::Stick(0, 0, 1.0), b'D'),
    ("rs_up", Effect::Stick(1, 1, 1.0), b'I'),
    ("rs_down", Effect::Stick(1, 1, -1.0), b'K'),
    ("rs_left", Effect::Stick(1, 0, -1.0), b'J'),
    ("rs_right", Effect::Stick(1, 0, 1.0), b'L'),
];

pub(super) struct Keymap {
    keys: [u8; CONTROLS.len()],
}

impl Keymap {
    /// The defaults with `spec`'s changes; pairs that do not parse are skipped.
    fn parse(spec: &str) -> Self {
        let mut keys = CONTROLS.map(|(_, _, key)| key);
        for pair in spec.split(',') {
            let Some((name, key)) = pair.split_once('=') else {
                continue;
            };
            let at = CONTROLS.iter().position(|(n, _, _)| *n == name.trim());
            let key = u8::from_str_radix(key.trim(), 16).ok().filter(|&k| k != 0);
            match (at, key) {
                (Some(at), Some(key)) => keys[at] = key,
                _ => tracing::warn!(pair, "VICTORY_KEYMAP: not a control=key pair"),
            }
        }
        Self { keys }
    }

    /// The pad that the keys for which `held` is true make.
    pub(super) fn pad(&self, held: impl Fn(u8) -> bool) -> PadState {
        let mut pad = PadState::default();
        let mut sticks = [[0.0f32; 2]; 2];
        for ((_, effect, _), &key) in CONTROLS.iter().zip(&self.keys) {
            if !held(key) {
                continue;
            }
            match *effect {
                Effect::Button(bit) => pad.buttons |= bit,
                Effect::LeftTrigger => pad.left_trigger = 1.0,
                Effect::RightTrigger => pad.right_trigger = 1.0,
                Effect::Stick(stick, axis, dir) => sticks[stick][axis] += dir,
            }
        }
        [pad.left_x, pad.left_y] = sticks[0];
        [pad.right_x, pad.right_y] = sticks[1];
        pad
    }
}

/// The keymap of this run: the defaults, with `VICTORY_KEYMAP` over them.
pub(super) fn current() -> &'static Keymap {
    static KEYMAP: LazyLock<Keymap> =
        LazyLock::new(|| Keymap::parse(&std::env::var("VICTORY_KEYMAP").unwrap_or_default()));
    &KEYMAP
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_documented_keys() {
        let map = Keymap::parse("");
        let pad = map.pad(|k| [b'W', b'D', 0x0D, b'C'].contains(&k));
        assert_eq!(
            pad,
            PadState {
                buttons: A,
                left_x: 1.0,
                left_y: 1.0,
                right_trigger: 1.0,
                ..Default::default()
            }
        );
    }

    #[test]
    fn changed_keys_replace_the_defaults() {
        let map = Keymap::parse("a=20, lt=A2,ls_up=26,bogus=41,b=zz,x=0");
        assert_eq!(map.pad(|k| k == 0x20).buttons, A);
        assert_eq!(map.pad(|k| k == 0x0D), PadState::default());
        assert_eq!(map.pad(|k| k == 0xA2).left_trigger, 1.0);
        // The up arrow is both the stick's up and the D-pad's.
        let pad = map.pad(|k| k == 0x26);
        assert_eq!((pad.buttons, pad.left_y), (UP, 1.0));
        // What did not parse keeps its default.
        assert_eq!(map.pad(|k| k == 0x08).buttons, B);
        assert_eq!(map.pad(|k| k == b'X').buttons, X);
    }

    #[test]
    fn opposite_directions_cancel() {
        let pad = Keymap::parse("").pad(|k| k == b'A' || k == b'D');
        assert_eq!(pad.left_x, 0.0);
    }
}
