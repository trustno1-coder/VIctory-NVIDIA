//! Nintendo Switch Pro Controller read straight from HID (USB or
//! Bluetooth), like the PlayStation pads in [`super::dualsense`]: what the
//! reports mean, the packets that set the pad up, and its stick calibration.
//! Opening it, the handshake and reading are in `dualsense`'s host part.
//!
//! The pad first sends short reports (0x3F); asked for it (subcommand 0x03,
//! 0x30) it sends full ones (0x30) all the time. Over USB it talks HID only
//! after a handshake (output report 0x80). Subcommands go out as output
//! report 0x01 and are answered by input report 0x21, which carries the
//! pad's state as a 0x30 report does.

use super::{
    A, B, DOWN, LEFT, LEFT_SHOULDER, LEFT_STICK, MENU, PadState, RIGHT, RIGHT_SHOULDER,
    RIGHT_STICK, UP, VIEW, X, Y,
};

pub(super) const NINTENDO: u16 = 0x057E;
/// The Pro Controller.
pub(super) const PRODUCT: u16 = 0x2009;

/// USB-only commands (output report 0x80, answered by 0x81 with the same
/// command byte).
pub(super) const USB_STATUS: u8 = 0x01;
pub(super) const USB_HANDSHAKE: u8 = 0x02;
pub(super) const USB_HIGH_SPEED: u8 = 0x03;
/// HID only, no Bluetooth and no time-out: not answered.
pub(super) const USB_HID_ONLY: u8 = 0x04;

/// Subcommands (output report 0x01, answered by 0x21).
pub(super) const DEVICE_INFO: u8 = 0x02;
pub(super) const INPUT_MODE: u8 = 0x03;
pub(super) const SPI_READ: u8 = 0x10;
pub(super) const PLAYER_LIGHTS: u8 = 0x30;
/// The input mode of full reports.
pub(super) const FULL: u8 = 0x30;

/// Factory stick calibration in the pad's flash: left then right, nine
/// bytes each.
pub(super) const FACTORY_STICKS: u32 = 0x603D;
/// User stick calibration: per stick a magic word (0xB2 0xA1) then nine
/// bytes, left then right.
pub(super) const USER_STICKS: u32 = 0x8010;
const USER_MAGIC: [u8; 2] = [0xB2, 0xA1];

/// Rumble data that moves nothing, part of every subcommand.
const NO_RUMBLE: [u8; 8] = [0x00, 0x01, 0x40, 0x40, 0x00, 0x01, 0x40, 0x40];

/// A USB command into `out` (the pad's output report length).
pub(super) fn usb_command(out: &mut [u8], command: u8) {
    out.fill(0);
    out[0] = 0x80;
    out[1] = command;
}

/// Subcommand `id` with `args` into `out`; `counter` goes up by one for
/// every packet sent (the pad wants it to change).
pub(super) fn subcommand(out: &mut [u8], counter: &mut u8, id: u8, args: &[u8]) {
    out.fill(0);
    out[0] = 0x01;
    out[1] = *counter & 0xF;
    *counter = counter.wrapping_add(1);
    out[2..10].copy_from_slice(&NO_RUMBLE);
    out[10] = id;
    let room = out.len().saturating_sub(11).min(args.len());
    out[11..11 + room].copy_from_slice(&args[..room]);
}

/// The arguments of a flash read of `len` bytes (at most 0x1D) at `at`.
pub(super) fn spi_read(at: u32, len: u8) -> [u8; 5] {
    let a = at.to_le_bytes();
    [a[0], a[1], a[2], a[3], len]
}

/// Whether `report` answers USB command `command`.
pub(super) fn answers_usb(report: &[u8], command: u8) -> bool {
    report.len() >= 2 && report[0] == 0x81 && report[1] == command
}

/// The data of the answer to subcommand `id`, `None` for another report.
pub(super) fn answer(report: &[u8], id: u8) -> Option<&[u8]> {
    (report.len() >= 15 && report[0] == 0x21 && report[14] == id).then(|| &report[15..])
}

/// The bytes a flash read answered with, `None` if they are not those of
/// `at` or fewer than `len`.
pub(super) fn spi_data(data: &[u8], at: u32, len: u8) -> Option<&[u8]> {
    let echo = spi_read(at, len);
    (data.len() >= 5 + usize::from(len) && data[..5] == echo)
        .then(|| &data[5..5 + usize::from(len)])
}

/// The Bluetooth address in the answer to the USB status command, most
/// significant byte first (the pad sends it the other way round).
pub(super) fn usb_mac(report: &[u8]) -> Option<[u8; 6]> {
    let mut mac: [u8; 6] = report.get(4..10)?.try_into().ok()?;
    mac.reverse();
    (mac != [0; 6]).then_some(mac)
}

/// The Bluetooth address in the answer to the device info subcommand, most
/// significant byte first.
pub(super) fn info_mac(data: &[u8]) -> Option<[u8; 6]> {
    let mac: [u8; 6] = data.get(4..10)?.try_into().ok()?;
    (mac != [0; 6]).then_some(mac)
}

/// One stick axis's calibration: the raw value at rest, and how far below
/// and above it the stick goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Axis {
    center: u16,
    below: u16,
    above: u16,
}

impl Axis {
    /// `raw` (12 bits) as -1 to 1.
    fn value(self, raw: u16) -> f32 {
        let off = f32::from(raw) - f32::from(self.center);
        let range = if off < 0.0 { self.below } else { self.above };
        (off / f32::from(range.max(1))).clamp(-1.0, 1.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Stick {
    x: Axis,
    y: Axis,
}

/// Both sticks' calibration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Sticks {
    left: Stick,
    right: Stick,
}

impl Default for Sticks {
    /// What a pad whose flash could not be read gets: about what the
    /// factory writes.
    fn default() -> Self {
        let axis = Axis { center: 2048, below: 1500, above: 1500 };
        let stick = Stick { x: axis, y: axis };
        Sticks { left: stick, right: stick }
    }
}

/// The three 12-bit pairs (x, y) in nine bytes of calibration.
fn pairs(d: &[u8]) -> [(u16, u16); 3] {
    std::array::from_fn(|g| {
        let b = &d[3 * g..3 * g + 3];
        (
            u16::from(b[0]) | (u16::from(b[1]) & 0xF) << 8,
            u16::from(b[1]) >> 4 | u16::from(b[2]) << 4,
        )
    })
}

/// A stick from three pairs: (center, below, above); `None` for flash that
/// holds no calibration (all ones) or nonsense.
fn stick(center: (u16, u16), below: (u16, u16), above: (u16, u16)) -> Option<Stick> {
    let axis = |center: u16, below: u16, above: u16| {
        let fine = |v: u16| v != 0 && v != 0xFFF;
        (fine(center) && fine(below) && fine(above)).then_some(Axis { center, below, above })
    };
    Some(Stick {
        x: axis(center.0, below.0, above.0)?,
        y: axis(center.1, below.1, above.1)?,
    })
}

/// The left stick in its nine bytes: above, center, below.
fn left_stick(d: &[u8]) -> Option<Stick> {
    let [above, center, below] = pairs(d);
    stick(center, below, above)
}

/// The right stick in its nine bytes: center, below, above.
fn right_stick(d: &[u8]) -> Option<Stick> {
    let [center, below, above] = pairs(d);
    stick(center, below, above)
}

impl Sticks {
    /// From the 18 bytes at [`FACTORY_STICKS`] and, if the user calibrated a
    /// stick, the 22 at [`USER_STICKS`], which win; what cannot be read
    /// stays as [`Sticks::default`].
    pub(super) fn from_flash(factory: Option<&[u8]>, user: Option<&[u8]>) -> Sticks {
        let mut sticks = Sticks::default();
        if let Some(f) = factory.filter(|f| f.len() >= 18) {
            sticks.left = left_stick(&f[..9]).unwrap_or(sticks.left);
            sticks.right = right_stick(&f[9..18]).unwrap_or(sticks.right);
        }
        if let Some(u) = user.filter(|u| u.len() >= 22) {
            if u[..2] == USER_MAGIC {
                sticks.left = left_stick(&u[2..11]).unwrap_or(sticks.left);
            }
            if u[11..13] == USER_MAGIC {
                sticks.right = right_stick(&u[13..22]).unwrap_or(sticks.right);
            }
        }
        sticks
    }
}

/// Face buttons by where they sit, as on an Xbox pad (Nintendo's B, at the
/// bottom, is A), or by the letter printed on them (`VICTORY_SWITCH_LABELS=1`).
#[cfg(windows)]
pub(super) fn by_label() -> bool {
    static LABELS: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        std::env::var("VICTORY_SWITCH_LABELS").is_ok_and(|v| v == "1")
    });
    *LABELS
}

/// The pad in a full report (0x30) or a subcommand's answer (0x21), `None`
/// for any other report. The home and capture buttons have no console pad
/// button; ZL and ZR are not analog.
pub(super) fn parse(report: &[u8], sticks: &Sticks, labels: bool) -> Option<PadState> {
    if !matches!(report.first()?, 0x30 | 0x21) || report.len() < 12 {
        return None;
    }
    let (right, shared, left) = (report[3], report[4], report[5]);
    // Nintendo's Y, X, B, A in bits 0 to 3 of the right byte.
    let face = if labels { [Y, X, B, A] } else { [X, Y, A, B] };
    let table: [(u8, u8, u32); 14] = [
        (right, 0x01, face[0]),
        (right, 0x02, face[1]),
        (right, 0x04, face[2]),
        (right, 0x08, face[3]),
        (right, 0x40, RIGHT_SHOULDER),
        (shared, 0x01, VIEW), // minus
        (shared, 0x02, MENU), // plus
        (shared, 0x04, RIGHT_STICK),
        (shared, 0x08, LEFT_STICK),
        (left, 0x01, DOWN),
        (left, 0x02, UP),
        (left, 0x04, RIGHT),
        (left, 0x08, LEFT),
        (left, 0x40, LEFT_SHOULDER),
    ];
    let raw = |at: usize| {
        let b = &report[at..at + 3];
        (
            u16::from(b[0]) | (u16::from(b[1]) & 0xF) << 8,
            u16::from(b[1]) >> 4 | u16::from(b[2]) << 4,
        )
    };
    let (lx, ly) = raw(6);
    let (rx, ry) = raw(9);
    let pulled = |byte: u8| if byte & 0x80 != 0 { 1.0 } else { 0.0 };
    Some(PadState {
        buttons: table
            .iter()
            .filter(|&&(byte, mask, _)| byte & mask != 0)
            .fold(0, |all, &(_, _, bit)| all | bit),
        left_trigger: pulled(left),
        right_trigger: pulled(right),
        // Up is the larger value on this pad, as here.
        left_x: sticks.left.x.value(lx),
        left_y: sticks.left.y.value(ly),
        right_x: sticks.right.x.value(rx),
        right_y: sticks.right.y.value(ry),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two 12-bit values packed as the pad packs them.
    fn pack(x: u16, y: u16) -> [u8; 3] {
        [(x & 0xFF) as u8, ((x >> 8) as u8 & 0xF) | ((y & 0xF) as u8) << 4, (y >> 4) as u8]
    }

    /// Nine bytes of calibration from three pairs, in the order given.
    fn nine(pairs: [(u16, u16); 3]) -> Vec<u8> {
        pairs.iter().flat_map(|&(x, y)| pack(x, y)).collect()
    }

    fn report(right: u8, shared: u8, left: u8, l: (u16, u16), r: (u16, u16)) -> Vec<u8> {
        let mut report = vec![0u8; 64];
        report[0] = 0x30;
        report[3] = right;
        report[4] = shared;
        report[5] = left;
        report[6..9].copy_from_slice(&pack(l.0, l.1));
        report[9..12].copy_from_slice(&pack(r.0, r.1));
        report
    }

    #[test]
    fn calibration_is_unpacked_per_stick_in_its_own_order() {
        // Left: above, center, below. Right: center, below, above.
        let mut factory = nine([(1000, 1100), (2000, 2100), (900, 950)]);
        factory.extend(nine([(1990, 2010), (1200, 1300), (1400, 1500)]));
        let sticks = Sticks::from_flash(Some(&factory), None);
        assert_eq!(sticks.left.x, Axis { center: 2000, below: 900, above: 1000 });
        assert_eq!(sticks.left.y, Axis { center: 2100, below: 950, above: 1100 });
        assert_eq!(sticks.right.x, Axis { center: 1990, below: 1200, above: 1400 });
        assert_eq!(sticks.right.y, Axis { center: 2010, below: 1300, above: 1500 });
    }

    #[test]
    fn a_user_calibration_wins_and_blank_flash_keeps_the_default() {
        let factory = vec![0xFF; 18];
        let mut user = vec![0xFF; 22];
        user[..2].copy_from_slice(&USER_MAGIC);
        user[2..11].copy_from_slice(&nine([(1000, 1000), (2050, 2040), (1000, 1000)]));
        let sticks = Sticks::from_flash(Some(&factory), Some(&user));
        assert_eq!(sticks.left.x.center, 2050);
        assert_eq!(sticks.left.y.center, 2040);
        // right: no magic, blank factory flash
        assert_eq!(sticks.right, Sticks::default().right);
        assert_eq!(Sticks::from_flash(None, None), Sticks::default());
    }

    #[test]
    fn a_full_report_reads_as_a_pad_with_buttons_by_place() {
        let sticks = Sticks::default();
        // Nintendo B (bottom) and X (top), R, ZR; plus; D-pad left, ZL.
        let r = report(0x04 | 0x02 | 0x40 | 0x80, 0x02, 0x08 | 0x80, (2048 - 1500, 2048 + 1500), (2048, 2048));
        let pad = parse(&r, &sticks, false).unwrap();
        assert_eq!(pad.buttons, A | Y | RIGHT_SHOULDER | MENU | LEFT);
        assert_eq!((pad.left_trigger, pad.right_trigger), (1.0, 1.0));
        assert_eq!((pad.left_x, pad.left_y), (-1.0, 1.0));
        assert_eq!((pad.right_x, pad.right_y), (0.0, 0.0));
        // by label: Nintendo B is B, X is X
        assert_eq!(parse(&r, &sticks, true).unwrap().buttons & (A | B | X | Y), B | X);
    }

    #[test]
    fn subcommand_answers_carry_the_state_too_and_other_reports_do_not() {
        let sticks = Sticks::default();
        let mut r = report(0x08, 0x01, 0x01, (2048, 2048), (2048, 2048));
        r[0] = 0x21;
        assert_eq!(parse(&r, &sticks, false).unwrap().buttons, B | VIEW | DOWN);
        r[0] = 0x3F;
        assert_eq!(parse(&r, &sticks, false), None);
        assert_eq!(parse(&[0x30; 5], &sticks, false), None);
    }

    #[test]
    fn packets_and_answers() {
        let mut out = [0xEEu8; 49];
        let mut counter = 15;
        subcommand(&mut out, &mut counter, INPUT_MODE, &[FULL]);
        assert_eq!(out[..12], [0x01, 15, 0x00, 0x01, 0x40, 0x40, 0x00, 0x01, 0x40, 0x40, INPUT_MODE, FULL]);
        assert!(out[12..].iter().all(|&b| b == 0));
        subcommand(&mut out, &mut counter, PLAYER_LIGHTS, &[1]);
        assert_eq!((out[1], counter), (0, 17));
        usb_command(&mut out, USB_HANDSHAKE);
        assert_eq!(out[..3], [0x80, USB_HANDSHAKE, 0]);
        assert!(answers_usb(&[0x81, USB_HANDSHAKE], USB_HANDSHAKE));
        assert!(!answers_usb(&[0x81, USB_STATUS], USB_HANDSHAKE));

        // An answer to a flash read: ack, id, then the read echoed and the data.
        let mut reply = vec![0u8; 64];
        reply[0] = 0x21;
        reply[13] = 0x90;
        reply[14] = SPI_READ;
        reply[15..20].copy_from_slice(&spi_read(FACTORY_STICKS, 18));
        reply[20..38].copy_from_slice(&[7; 18]);
        let data = answer(&reply, SPI_READ).unwrap();
        assert_eq!(spi_data(data, FACTORY_STICKS, 18), Some(&[7u8; 18][..]));
        assert_eq!(spi_data(data, USER_STICKS, 18), None);
        assert_eq!(answer(&reply, INPUT_MODE), None);
    }

    #[test]
    fn addresses_come_out_most_significant_byte_first() {
        assert_eq!(usb_mac(&[0x81, 0x01, 0x00, 0x03, 6, 5, 4, 3, 2, 1]), Some([1, 2, 3, 4, 5, 6]));
        assert_eq!(usb_mac(&[0x81, 0x01, 0x00, 0x03, 0, 0, 0, 0, 0, 0]), None);
        assert_eq!(info_mac(&[4, 0x21, 3, 2, 1, 2, 3, 4, 5, 6]), Some([1, 2, 3, 4, 5, 6]));
        assert_eq!(info_mac(&[4, 0x21]), None);
    }
}
