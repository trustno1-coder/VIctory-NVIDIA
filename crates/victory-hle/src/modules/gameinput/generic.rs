//! Generic HID pads: any HID joystick or gamepad that XInput does not list
//! and that is not one of the Sony pads [`super::playstation`] reads. The
//! no-name USB pads sold as "PC Controller" or "USB Gamepad" (DragonRise and
//! alike), Logitech's pads in DirectInput mode and Nintendo's Switch Pro
//! Controller over Bluetooth are such pads. Their reports are read through
//! Windows' HID parser, by the usages their descriptors give: X and Y the
//! left stick, Z and Rz (else Rx and Ry) the right one, the hat switch the
//! D-pad, the buttons by number. No standard says which number is which
//! button, so the layout is picked by vendor ([`Layout::for_pad`]; the
//! PlayStation look-alike one was checked on a DragonRise "PC Controller",
//! 0079:0126, the others follow those pads' usual numbering), and
//! `VICTORY_PAD_BUTTONS` sets one of its own; `victory pads` shows the
//! numbers a pad sends. These pads do not rumble (they have no common way
//! to). A scan thread looks for new pads every [`SCAN`]; each pad has a
//! reader thread that keeps its latest state until the pad goes away.
//!
//! After FrancescoGelosoEterea/VIscosity, commit d8db7a0
//! (`gameinput/hid.rs`), GPL-3.0 like this crate: the axes' assignment and
//! the order of `VICTORY_PAD_BUTTONS` are its.

use super::{
    A, B, DOWN, LEFT, LEFT_SHOULDER, LEFT_STICK, MENU, PadState, RIGHT, RIGHT_SHOULDER,
    RIGHT_STICK, UP, VIEW, X, Y,
};

/// The Generic Desktop page: the six axes X (0x30) to Rz (0x35), the hat
/// switch, and the Start, Select and D-pad buttons some pads give.
const GENERIC_DESKTOP: u16 = 0x01;
const USAGE_X: u16 = 0x30;
const AXES: usize = 6;
const USAGE_HAT: u16 = 0x39;
const USAGE_START: u16 = 0x3D;
const USAGE_SELECT: u16 = 0x3E;
const DESKTOP_BUTTONS: [(u16, u32); 6] = [
    (USAGE_START, MENU),
    (USAGE_SELECT, VIEW),
    (0x90, UP),
    (0x91, DOWN),
    (0x92, RIGHT),
    (0x93, LEFT),
];
/// HID's button page: button 1 is usage 1, and so on.
const BUTTON_PAGE: u16 = 0x09;
/// The top-level collections of pads: joystick and gamepad (on the Generic
/// Desktop page). Not the multi-axis controller (0x08), which 3D mice are.
const PAD_USAGES: [u16; 2] = [0x04, 0x05];

/// The connection ids of generic pads have this bit; those of Sony's pads
/// do not.
pub(super) const ID: u64 = 1 << 63;

/// Names of the axes, X to Rz.
const AXIS_NAMES: [&str; AXES] = ["X", "Y", "Z", "Rx", "Ry", "Rz"];

/// What each entry of a [`Layout`] presses: A, B, X, Y, the shoulders, the
/// triggers (as the triggers, fully), View, Menu, the stick clicks, and up,
/// down, left and right for pads that give their D-pad as buttons.
const TARGETS: [u32; 16] = [
    A,
    B,
    X,
    Y,
    LEFT_SHOULDER,
    RIGHT_SHOULDER,
    0,
    0,
    VIEW,
    MENU,
    LEFT_STICK,
    RIGHT_STICK,
    UP,
    DOWN,
    LEFT,
    RIGHT,
];
const LEFT_TRIGGER: usize = 6;
const RIGHT_TRIGGER: usize = 7;

/// A pad's buttons by HID number (1 the first, 0 none), in the order of
/// [`TARGETS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Layout([u16; 16]);

impl Layout {
    /// The PlayStation look-alikes most no-name pads are: 1 triangle
    /// (north), 2 circle (east), 3 cross (south), 4 square (west), 5 L1, 6 R1,
    /// 7 L2, 8 R2, 9 select, 10 start, 11 L3, 12 R3.
    const PLAYSTATION: Layout = Layout([3, 2, 4, 1, 5, 6, 7, 8, 9, 10, 11, 12, 0, 0, 0, 0]);
    /// DirectInput's usual order (Logitech's pads): 1 west, 2 south, 3 east,
    /// 4 north, the rest as above.
    const DIRECTINPUT: Layout = Layout([2, 3, 1, 4, 5, 6, 7, 8, 9, 10, 11, 12, 0, 0, 0, 0]);
    /// Nintendo's Switch Pro Controller: 1 south (B), 2 east (A), 3 west
    /// (Y), 4 north (X), then L, R, ZL, ZR, minus, plus and the sticks; by
    /// position, as the console's pad has its buttons.
    const NINTENDO: Layout = Layout([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 0, 0, 0, 0]);

    /// The layout pads of this vendor have.
    fn for_pad(vendor: u16) -> Layout {
        match vendor {
            0x046D => Layout::DIRECTINPUT,
            0x057E => Layout::NINTENDO,
            _ => Layout::PLAYSTATION,
        }
    }

    /// `VICTORY_PAD_BUTTONS`: 12 button numbers, comma-separated, for A, B,
    /// X, Y, LB, RB, LT, RT, View, Menu, LS, RS (0 for none), and 4 more for
    /// up, down, left and right where the pad gives its D-pad as buttons.
    /// `None` for anything else.
    fn parse(text: &str) -> Option<Layout> {
        let numbers = text
            .split(',')
            .map(|n| n.trim().parse::<u16>().ok())
            .collect::<Option<Vec<u16>>>()?;
        if numbers.len() != 12 && numbers.len() != 16 {
            return None;
        }
        let mut layout = [0; 16];
        layout[..numbers.len()].copy_from_slice(&numbers);
        Some(Layout(layout))
    }

    /// As `VICTORY_PAD_BUTTONS` would give it.
    fn text(&self) -> String {
        let end = if self.0[12..].iter().all(|&n| n == 0) {
            12
        } else {
            16
        };
        let numbers: Vec<String> = self.0[..end].iter().map(u16::to_string).collect();
        numbers.join(",")
    }
}

/// A value's logical range, as a pad's descriptor gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Range {
    min: i64,
    max: i64,
    /// Values below zero are possible: the raw bits are sign-extended.
    signed: bool,
    bits: u32,
}

impl Range {
    /// The range of a value `bits` wide, `None` when it is empty. A maximum
    /// below the minimum is an unsigned one whose top bit was taken for the
    /// sign (255 in 8 bits given as -1, which some pads do): it reads as
    /// unsigned.
    fn new(min: i32, max: i32, bits: u16) -> Option<Range> {
        let bits = u32::from(bits).clamp(1, 32);
        let mask = (1i64 << bits) - 1;
        let (mut min, mut max, mut signed) = (i64::from(min), i64::from(max), min < 0);
        if max < min {
            (min, max, signed) = (min & mask, max & mask, false);
        }
        (max > min).then_some(Range {
            min,
            max,
            signed,
            bits,
        })
    }

    /// The value of these raw bits (HID's parser leaves the sign to us).
    fn value(self, raw: u32) -> i64 {
        let raw = i64::from(raw) & ((1i64 << self.bits) - 1);
        if self.signed && raw >> (self.bits - 1) & 1 != 0 {
            raw - (1i64 << self.bits)
        } else {
            raw
        }
    }

    /// -1 to 1 across the range.
    fn stick(self, value: i64) -> f32 {
        let span = (self.max - self.min) as f64;
        ((2.0 * (value - self.min) as f64 / span - 1.0) as f32).clamp(-1.0, 1.0)
    }

    /// 0 to 1 across the range.
    fn trigger(self, value: i64) -> f32 {
        let span = (self.max - self.min) as f64;
        (((value - self.min) as f64 / span) as f32).clamp(0.0, 1.0)
    }
}

/// What a pad's reports hold: the range of each axis it has (X to Rz) and
/// of its hat switch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Shape {
    axes: [Option<Range>; AXES],
    hat: Option<Range>,
}

impl Shape {
    /// "X Y Z Rz, hat".
    fn describe(&self) -> String {
        let mut names: Vec<&str> = AXIS_NAMES
            .iter()
            .zip(self.axes)
            .filter_map(|(name, range)| range.map(|_| *name))
            .collect();
        if self.hat.is_some() {
            names.push("hat");
        }
        if names.is_empty() {
            "no axes".to_owned()
        } else {
            names.join(" ")
        }
    }
}

/// What a pad's reports said last: the raw value of each axis and of the
/// hat, `None` until a report had it; the buttons held, by number; the
/// Generic Desktop buttons held (Start, Select, the D-pad), by usage.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Raw {
    axes: [Option<i64>; AXES],
    hat: Option<i64>,
    buttons: Vec<u16>,
    desktop: Vec<u16>,
}

/// A hat switch's value as the D-pad: its first value up, then clockwise in
/// eighths (in quarters for a four-way hat); anything outside its range
/// (the null state of a pad at rest) is the centre.
fn hat(range: Range, value: i64) -> u32 {
    if value < range.min || value > range.max {
        return 0;
    }
    let step = value - range.min;
    let eighths = if range.max - range.min + 1 == 4 {
        step * 2
    } else {
        step
    };
    match eighths {
        0 => UP,
        1 => UP | RIGHT,
        2 => RIGHT,
        3 => RIGHT | DOWN,
        4 => DOWN,
        5 => DOWN | LEFT,
        6 => LEFT,
        7 => LEFT | UP,
        _ => 0,
    }
}

/// The axes a pad holds at their centre at rest: all it has but Rx and Ry
/// where they are the triggers (beside Z and Rz, in [`state`]).
fn centred(shape: &Shape) -> [bool; AXES] {
    let triggers = shape.axes[2].is_some() && shape.axes[5].is_some();
    std::array::from_fn(|i| shape.axes[i].is_some() && !(triggers && (i == 3 || i == 4)))
}

/// Whether an axis that should rest at its centre is far from it in the
/// first report that has it: a throttle, a pedal or a slider (a flight
/// stick's, a wheel's) rather than a pad's stick. Such an axis is not read
/// for that connection, so it cannot hold a stick or a trigger down.
pub(super) fn off_centre(shape: &Shape, axis: usize, value: i64) -> bool {
    centred(shape)[axis] && shape.axes[axis].is_some_and(|r| r.stick(value).abs() > 0.5)
}

/// The console pad a generic pad's last reports make. X and Y are the left
/// stick. Z and Rz are the right one where the pad has both, and then Rx and
/// Ry, if it has them, the triggers; else Rx and Ry are the right stick, and
/// Z, if there, both triggers on one axis (as DirectInput gives an Xbox
/// pad's: the left one above the centre, the right one below). Down is
/// positive on the pad, up is here. The hat and the D-pad buttons are the
/// D-pad; the trigger buttons press the triggers fully.
pub(super) fn state(shape: &Shape, raw: &Raw, layout: &Layout) -> PadState {
    let has = |i: usize| shape.axes[i].is_some();
    let stick = |i: usize| Some(shape.axes[i]?.stick(raw.axes[i]?));
    let trigger = |i: usize| Some(shape.axes[i]?.trigger(raw.axes[i]?));
    let (right, (left_trigger, right_trigger)) = if has(2) && has(5) {
        let triggers = (trigger(3).unwrap_or(0.0), trigger(4).unwrap_or(0.0));
        ((stick(2), stick(5)), triggers)
    } else {
        let z = stick(2).unwrap_or(0.0);
        ((stick(3), stick(4)), (z.max(0.0), (-z).max(0.0)))
    };
    let held = |n: u16| n != 0 && raw.buttons.contains(&n);
    let mut buttons = shape.hat.zip(raw.hat).map_or(0, |(range, v)| hat(range, v));
    for (&n, bit) in layout.0.iter().zip(TARGETS) {
        if held(n) {
            buttons |= bit;
        }
    }
    for (usage, bit) in DESKTOP_BUTTONS {
        if raw.desktop.contains(&usage) {
            buttons |= bit;
        }
    }
    let pressed = |n: u16| if held(n) { 1.0 } else { 0.0 };
    PadState {
        buttons,
        left_trigger: left_trigger.max(pressed(layout.0[LEFT_TRIGGER])),
        right_trigger: right_trigger.max(pressed(layout.0[RIGHT_TRIGGER])),
        left_x: stick(0).unwrap_or(0.0),
        left_y: -stick(1).unwrap_or(0.0),
        right_x: right.0.unwrap_or(0.0),
        right_y: -right.1.unwrap_or(0.0),
    }
}

/// `VICTORY_GENERIC_PADS=0` reads no generic pad.
pub(super) fn enabled() -> bool {
    std::env::var_os("VICTORY_GENERIC_PADS").is_none_or(|v| v != "0")
}

/// The layout for a pad of this vendor: `VICTORY_PAD_BUTTONS`'s when set
/// (and readable), else the vendor's.
fn layout(vendor: u16) -> Layout {
    let set = std::env::var("VICTORY_PAD_BUTTONS")
        .ok()
        .filter(|t| !t.trim().is_empty());
    match set.as_deref().map(|t| (t, Layout::parse(t))) {
        Some((_, Some(layout))) => layout,
        Some((text, None)) => {
            tracing::warn!(
                text,
                "VICTORY_PAD_BUTTONS: not 12 or 16 button numbers; the pad's own layout is used"
            );
            Layout::for_pad(vendor)
        }
        None => Layout::for_pad(vendor),
    }
}

/// The console pad's buttons and triggers held, by name ("A LB LT").
fn names(pad: &PadState) -> String {
    const NAMES: [(u32, &str); 14] = [
        (A, "A"),
        (B, "B"),
        (X, "X"),
        (Y, "Y"),
        (LEFT_SHOULDER, "LB"),
        (RIGHT_SHOULDER, "RB"),
        (VIEW, "View"),
        (MENU, "Menu"),
        (LEFT_STICK, "LS"),
        (RIGHT_STICK, "RS"),
        (UP, "Up"),
        (DOWN, "Down"),
        (LEFT, "Left"),
        (RIGHT, "Right"),
    ];
    let mut held: Vec<String> = NAMES
        .iter()
        .filter(|(bit, _)| pad.buttons & bit != 0)
        .map(|(_, name)| (*name).to_owned())
        .collect();
    for (name, value) in [("LT", pad.left_trigger), ("RT", pad.right_trigger)] {
        if value > 0.05 {
            held.push(format!("{name} {value:.2}"));
        }
    }
    if held.is_empty() {
        "-".to_owned()
    } else {
        held.join(" ")
    }
}

/// One line of `victory pads` for a pad's last reports: the buttons held by
/// number, the axes and hat, and what the console pad gets.
fn line(shape: &Shape, raw: &Raw, pad: &PadState) -> String {
    let mut parts = vec![format!("buttons {:?}", raw.buttons)];
    for ((name, range), value) in AXIS_NAMES.iter().zip(shape.axes).zip(raw.axes) {
        if let (Some(range), Some(value)) = (range, value) {
            parts.push(format!("{name} {:+.2}", range.stick(value)));
        }
    }
    if let (Some(_), Some(value)) = (shape.hat, raw.hat) {
        parts.push(format!("hat {value}"));
    }
    if !raw.desktop.is_empty() {
        parts.push(format!("desktop {:02X?}", raw.desktop));
    }
    format!(
        "{}  ->  {}  (sticks L {:+.2} {:+.2} R {:+.2} {:+.2})",
        parts.join(", "),
        names(pad),
        pad.left_x,
        pad.left_y,
        pad.right_x,
        pad.right_y
    )
}

#[cfg(windows)]
pub(super) use host::{monitor, pads};

#[cfg(windows)]
mod host {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{LazyLock, Mutex, PoisonError};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Devices::HumanInterfaceDevice::{
        HIDD_ATTRIBUTES, HIDP_CAPS, HIDP_STATUS_SUCCESS, HIDP_VALUE_CAPS, HidD_FreePreparsedData,
        HidD_GetAttributes, HidD_GetPreparsedData, HidD_GetProductString, HidP_GetCaps,
        HidP_GetUsageValue, HidP_GetUsages, HidP_GetValueCaps, HidP_Input, HidP_MaxUsageListLength,
        PHIDP_PREPARSED_DATA,
    };
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
    };

    use super::super::playstation::{Sony, hid_paths};
    use super::{
        AXES, AXIS_NAMES, BUTTON_PAGE, GENERIC_DESKTOP, ID, Layout, PAD_USAGES, PadState, Range,
        Raw, Shape, USAGE_HAT, USAGE_X, layout, line, off_centre, state,
    };

    /// How often to look for newly connected pads.
    const SCAN: Duration = Duration::from_secs(2);

    struct Pad {
        /// This connection's: a pad that reconnects gets a new one.
        id: u64,
        path: Vec<u16>,
        name: String,
        shape: Shape,
        /// How many buttons its reports have.
        buttons: usize,
        layout: Layout,
        raw: Raw,
        /// `None` until the first report.
        state: Option<PadState>,
    }

    /// The open pads, in the order they were found.
    static PADS: Mutex<Vec<Pad>> = Mutex::new(Vec::new());

    fn lock() -> std::sync::MutexGuard<'static, Vec<Pad>> {
        PADS.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Paths found to be no pad of ours (keyboards, mice, Sony's pads,
    /// XInput's), not looked at again while they are present.
    static OTHERS: Mutex<Vec<Vec<u16>>> = Mutex::new(Vec::new());

    fn others() -> std::sync::MutexGuard<'static, Vec<Vec<u16>>> {
        OTHERS.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The open pads' latest states with their connection ids, oldest
    /// connection first, without the pads yet to report; the first call
    /// starts looking for them.
    pub(in super::super) fn pads() -> Vec<(u64, PadState)> {
        static STARTED: LazyLock<()> = LazyLock::new(|| {
            let started = std::thread::Builder::new()
                .name("generic pad scan".into())
                .spawn(|| {
                    loop {
                        scan();
                        std::thread::sleep(SCAN);
                    }
                });
            if let Err(error) = started {
                tracing::warn!(%error, "no generic pads: the thread looking for them could not start");
            }
        });
        LazyLock::force(&STARTED);
        lock()
            .iter()
            .filter_map(|pad| Some((pad.id, pad.state?)))
            .collect()
    }

    /// A device handle, closed when dropped.
    struct Handle(HANDLE);

    // SAFETY: a file handle any thread may use.
    unsafe impl Send for Handle {}

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: ours, closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    impl Handle {
        /// The device at `path` (no terminator), with `access` (0 to ask it
        /// about itself only).
        fn open(path: &[u16], access: u32) -> Option<Handle> {
            let mut name = path.to_vec();
            name.push(0);
            // SAFETY: a terminated path; no security attributes or template.
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    access,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            (handle != INVALID_HANDLE_VALUE).then_some(Handle(handle))
        }
    }

    /// A device's parser data, freed when dropped.
    struct Parser(PHIDP_PREPARSED_DATA);

    // SAFETY: read-only data the HID parser reads from any thread.
    unsafe impl Send for Parser {}

    impl Drop for Parser {
        fn drop(&mut self) {
            // SAFETY: ours, freed once.
            unsafe { HidD_FreePreparsedData(self.0) };
        }
    }

    impl Parser {
        fn of(handle: &Handle) -> Option<Parser> {
            let mut data = 0;
            // SAFETY: an open device; the output is writable.
            unsafe { HidD_GetPreparsedData(handle.0, &mut data) }.then_some(Parser(data))
        }

        /// The value of a Generic Desktop usage in this report, `None` when
        /// the report does not hold it (it is in another report).
        fn value(&self, usage: u16, report: &[u8]) -> Option<u32> {
            let mut value = 0;
            // SAFETY: this device's parser data and one of its reports.
            let status = unsafe {
                HidP_GetUsageValue(
                    HidP_Input,
                    GENERIC_DESKTOP,
                    0,
                    usage,
                    &mut value,
                    self.0,
                    report.as_ptr(),
                    report.len() as u32,
                )
            };
            (status == HIDP_STATUS_SUCCESS).then_some(value)
        }

        /// The buttons of `page` held in this report (`list` as long as
        /// the most there can be), `None` when the report has none of them.
        fn usages(&self, page: u16, report: &mut [u8], list: &mut [u16]) -> Option<Vec<u16>> {
            if list.is_empty() {
                return None;
            }
            let mut count = list.len() as u32;
            // SAFETY: room for `count` usages; this device's parser data and
            // one of its reports.
            let status = unsafe {
                HidP_GetUsages(
                    HidP_Input,
                    page,
                    0,
                    list.as_mut_ptr(),
                    &mut count,
                    self.0,
                    report.as_mut_ptr(),
                    report.len() as u32,
                )
            };
            (status == HIDP_STATUS_SUCCESS).then(|| {
                let mut held = list[..count as usize].to_vec();
                held.sort_unstable();
                held
            })
        }

        /// The most buttons of `page` a report can hold.
        fn buttons(&self, page: u16) -> usize {
            // SAFETY: this device's parser data.
            unsafe { HidP_MaxUsageListLength(HidP_Input, page, self.0) as usize }
        }

        /// The ranges of the axes and the hat the pad's input reports have.
        fn shape(&self, caps: &HIDP_CAPS) -> Shape {
            let mut count = caps.NumberInputValueCaps;
            let mut values = vec![HIDP_VALUE_CAPS::default(); usize::from(count)];
            // SAFETY: room for `count` capabilities; this device's parser data.
            let status =
                unsafe { HidP_GetValueCaps(HidP_Input, values.as_mut_ptr(), &mut count, self.0) };
            let mut shape = Shape::default();
            if status != HIDP_STATUS_SUCCESS {
                return shape;
            }
            values.truncate(usize::from(count));
            for cap in values.iter().filter(|cap| cap.UsagePage == GENERIC_DESKTOP) {
                // SAFETY: `IsRange` says which of the two the union holds.
                let (first, last) = unsafe {
                    if cap.IsRange {
                        (cap.Anonymous.Range.UsageMin, cap.Anonymous.Range.UsageMax)
                    } else {
                        (cap.Anonymous.NotRange.Usage, cap.Anonymous.NotRange.Usage)
                    }
                };
                let Some(range) = Range::new(cap.LogicalMin, cap.LogicalMax, cap.BitSize) else {
                    continue;
                };
                for usage in first..=last {
                    let slot = match usage {
                        USAGE_HAT => &mut shape.hat,
                        _ if (USAGE_X..USAGE_X + AXES as u16).contains(&usage) => {
                            &mut shape.axes[usize::from(usage - USAGE_X)]
                        }
                        _ => continue,
                    };
                    // The first one given, where a pad gives one twice.
                    slot.get_or_insert(range);
                }
            }
            shape
        }
    }

    /// What the device at `path` is (one at a time, never kept: its size
    /// does not matter).
    #[allow(clippy::large_enum_variant)]
    enum Found {
        /// Not a generic pad: not looked at again while it is present.
        Other,
        /// Could not be asked (yet): asked again at the next scan.
        Unknown,
        Pad {
            handle: Handle,
            parser: Parser,
            name: String,
            vendor: u16,
            product: u16,
            input: usize,
            shape: Shape,
        },
    }

    fn look(path: &[u16]) -> Found {
        // Asked about itself first, without read access, which keyboards and
        // mice (that Windows keeps to itself) refuse.
        let Some(query) = Handle::open(path, 0) else {
            return Found::Unknown;
        };
        let mut attributes = HIDD_ATTRIBUTES {
            Size: size_of::<HIDD_ATTRIBUTES>() as u32,
            ..Default::default()
        };
        // SAFETY: an open device and room for its attributes.
        if !unsafe { HidD_GetAttributes(query.0, &mut attributes) } {
            return Found::Unknown;
        }
        let (vendor, product) = (attributes.VendorID, attributes.ProductID);
        // Sony's pads are the PlayStation reader's.
        if Sony::of(vendor, product).is_some() {
            return Found::Other;
        }
        let Some(parser) = Parser::of(&query) else {
            return Found::Unknown;
        };
        let mut caps = HIDP_CAPS::default();
        // SAFETY: this device's parser data.
        if unsafe { HidP_GetCaps(parser.0, &mut caps) } != HIDP_STATUS_SUCCESS {
            return Found::Unknown;
        }
        if caps.UsagePage != GENERIC_DESKTOP
            || !PAD_USAGES.contains(&caps.Usage)
            || caps.InputReportByteLength == 0
        {
            return Found::Other;
        }
        let mut name = [0u16; 128];
        // SAFETY: the buffer is as long as we say, in bytes.
        let named = unsafe {
            HidD_GetProductString(query.0, name.as_mut_ptr().cast(), size_of_val(&name) as u32)
        };
        let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        let name = if named {
            String::from_utf16_lossy(&name[..end])
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            String::new()
        };
        let shape = parser.shape(&caps);
        let Some(handle) = Handle::open(path, GENERIC_READ) else {
            return Found::Unknown;
        };
        Found::Pad {
            handle,
            parser,
            name,
            vendor,
            product,
            input: usize::from(caps.InputReportByteLength),
            shape,
        }
    }

    /// Opens the pads not open yet, each with a reader thread.
    fn scan() {
        let paths = hid_paths();
        others().retain(|path| paths.contains(path));
        // A pad whose device is gone is forgotten, so it holds nothing down:
        // a Bluetooth pad switched off can leave its read waiting for ever.
        // Its reader ends at its next report, if one ever comes. (An empty
        // list is a failed look, not every device gone: the keyboard is one.)
        if paths.is_empty() {
            return;
        }
        lock().retain(|pad| {
            let present = paths.contains(&pad.path);
            if !present {
                tracing::info!(name = %pad.name, "generic pad gone");
            }
            present
        });
        for path in paths {
            // XInput's own devices (`IG_` in the path) are XInput's.
            let lower = String::from_utf16_lossy(&path).to_ascii_lowercase();
            if lower.contains("ig_")
                || lock().iter().any(|pad| pad.path == path)
                || others().contains(&path)
            {
                continue;
            }
            let (handle, parser, name, vendor, product, input, shape) = match look(&path) {
                Found::Other => {
                    others().push(path);
                    continue;
                }
                Found::Unknown => continue,
                Found::Pad {
                    handle,
                    parser,
                    name,
                    vendor,
                    product,
                    input,
                    shape,
                } => (handle, parser, name, vendor, product, input, shape),
            };
            let layout = layout(vendor);
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let id = ID | NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let buttons = parser.buttons(BUTTON_PAGE);
            tracing::info!(
                path = %lower,
                name = %name,
                id = %format!("{vendor:04X}:{product:04X}"),
                values = %shape.describe(),
                buttons,
                layout = %layout.text(),
                "generic pad connected (VICTORY_PAD_BUTTONS sets another layout; `victory pads` shows its buttons)"
            );
            lock().push(Pad {
                id,
                path: path.clone(),
                name: format!("{name} ({vendor:04X}:{product:04X})"),
                shape,
                buttons,
                layout,
                raw: Raw::default(),
                state: None,
            });
            let reader = Reader {
                id,
                handle,
                parser,
                input,
                shape,
                layout,
            };
            if let Err(error) = std::thread::Builder::new()
                .name("generic pad reader".into())
                .spawn(move || reader.run())
            {
                lock().retain(|pad| pad.id != id);
                tracing::warn!(%error, "could not start a generic pad reader");
            }
        }
    }

    /// What a reader needs to know about its pad.
    struct Reader {
        id: u64,
        handle: Handle,
        parser: Parser,
        input: usize,
        shape: Shape,
        layout: Layout,
    }

    impl Reader {
        /// Reads reports until the pad goes away, then forgets it. These
        /// pads may send a report only when something changes, so a quiet
        /// pad keeps its state.
        fn run(self) {
            let mut report = vec![0u8; self.input];
            let mut buttons = vec![0u16; self.parser.buttons(BUTTON_PAGE)];
            let mut desktop = vec![0u16; self.parser.buttons(GENERIC_DESKTOP)];
            let mut raw = Raw::default();
            // The axes seen in a report yet, and those not read ([`off_centre`]).
            let (mut seen, mut off) = ([false; AXES], [false; AXES]);
            let why = loop {
                let mut got = 0;
                // SAFETY: a buffer as long as we say, read into while we wait.
                let read = unsafe {
                    ReadFile(
                        self.handle.0,
                        report.as_mut_ptr(),
                        report.len() as u32,
                        &mut got,
                        std::ptr::null_mut(),
                    )
                };
                if read == 0 {
                    break "read failed";
                }
                if got == 0 {
                    break "read nothing";
                }
                // HID's parser takes a report as long as the device's longest
                // one; a shorter one (of another report id) is padded with zeros.
                let got = (got as usize).min(report.len());
                report[got..].fill(0);
                let report = report.as_mut_slice();
                for (i, range) in self.shape.axes.iter().enumerate() {
                    if let Some(range) = range
                        && let Some(value) = self.parser.value(USAGE_X + i as u16, report)
                    {
                        let value = range.value(value);
                        raw.axes[i] = Some(value);
                        if !seen[i] {
                            seen[i] = true;
                            off[i] = off_centre(&self.shape, i, value);
                            if off[i] {
                                tracing::warn!(
                                    axis = AXIS_NAMES[i],
                                    "generic pad: an axis that should rest at its centre does not \
                                     (a throttle or a pedal?); it is not read until the pad reconnects"
                                );
                            }
                        }
                    }
                }
                if let Some(range) = self.shape.hat
                    && let Some(value) = self.parser.value(USAGE_HAT, report)
                {
                    raw.hat = Some(range.value(value));
                }
                if let Some(held) = self.parser.usages(BUTTON_PAGE, report, &mut buttons) {
                    raw.buttons = held;
                }
                if let Some(held) = self.parser.usages(GENERIC_DESKTOP, report, &mut desktop) {
                    raw.desktop = held;
                }
                let mut read = raw.clone();
                for (value, off) in read.axes.iter_mut().zip(off) {
                    if off {
                        *value = None;
                    }
                }
                let state = state(&self.shape, &read, &self.layout);
                let mut pads = lock();
                let Some(pad) = pads.iter_mut().find(|pad| pad.id == self.id) else {
                    break "forgotten";
                };
                pad.state = Some(state);
                if pad.raw != raw {
                    pad.raw = raw.clone();
                }
            };
            let name = lock()
                .iter()
                .find(|pad| pad.id == self.id)
                .map(|pad| pad.name.clone())
                .unwrap_or_default();
            tracing::info!(why, name = %name, "generic pad disconnected");
            lock().retain(|pad| pad.id != self.id);
        }
    }

    /// `victory pads`: lists the generic pads found and, for `seconds`,
    /// prints what each one sends whenever it changes.
    pub(in super::super) fn monitor(seconds: u64) {
        let found = Instant::now();
        pads();
        while lock().is_empty() && found.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(100));
        }
        {
            let pads = lock();
            if pads.is_empty() {
                println!("Generic HID pads: none found.");
                return;
            }
            println!("Generic HID pads:");
            for (n, pad) in pads.iter().enumerate() {
                println!(
                    "  pad {}: {}, {}, {} buttons, VICTORY_PAD_BUTTONS={}",
                    n + 1,
                    pad.name,
                    pad.shape.describe(),
                    pad.buttons,
                    pad.layout.text()
                );
            }
        }
        println!(
            "\nPress each button and move each stick ({seconds} s; Ctrl+C ends).\n\
             VICTORY_PAD_BUTTONS lists the button numbers for A,B,X,Y,LB,RB,LT,RT,View,Menu,LS,RS\n\
             (Xbox names, by position: A south, B east, X west, Y north).\n"
        );
        let end = Instant::now() + Duration::from_secs(seconds);
        let mut last: Vec<(u64, String)> = Vec::new();
        while Instant::now() < end {
            let now: Vec<(u64, String)> = lock()
                .iter()
                .enumerate()
                .filter_map(|(n, pad)| {
                    let state = pad.state?;
                    Some((
                        pad.id,
                        format!("pad {}: {}", n + 1, line(&pad.shape, &pad.raw, &state)),
                    ))
                })
                .collect();
            for entry in &now {
                if !last.contains(entry) {
                    println!("{}", entry.1);
                }
            }
            last = now;
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(min: i32, max: i32, bits: u16) -> Range {
        Range::new(min, max, bits).unwrap()
    }

    /// A DragonRise-style pad: X Y Z Rz 0..255, an eight-way hat 0..7.
    fn dragonrise() -> Shape {
        let byte = range(0, 255, 8);
        Shape {
            axes: [Some(byte), Some(byte), Some(byte), None, None, Some(byte)],
            hat: Some(range(0, 7, 4)),
        }
    }

    fn at_rest() -> Raw {
        Raw {
            axes: [Some(128), Some(128), Some(128), None, None, Some(128)],
            hat: Some(15),
            ..Raw::default()
        }
    }

    #[test]
    fn a_playstation_look_alike_reads_as_the_console_pad() {
        let shape = dragonrise();
        let mut raw = at_rest();
        let pad = state(&shape, &raw, &Layout::PLAYSTATION);
        assert_eq!(pad.buttons, 0);
        assert!(pad.left_x.abs() < 0.01 && pad.right_y.abs() < 0.01);
        assert_eq!((pad.left_trigger, pad.right_trigger), (0.0, 0.0));
        // Cross, triangle, L1, L2, start; left stick up and left, right
        // stick right; hat down-right.
        raw.buttons = vec![1, 3, 5, 7, 10];
        raw.axes = [Some(0), Some(0), Some(255), None, None, Some(128)];
        raw.hat = Some(3);
        let pad = state(&shape, &raw, &Layout::PLAYSTATION);
        assert_eq!(pad.buttons, A | Y | LEFT_SHOULDER | MENU | RIGHT | DOWN);
        assert_eq!((pad.left_x, pad.left_y, pad.right_x), (-1.0, 1.0, 1.0));
        assert_eq!((pad.left_trigger, pad.right_trigger), (1.0, 0.0));
    }

    #[test]
    fn rx_and_ry_are_the_right_stick_without_z_and_rz() {
        // A Switch Pro Controller over Bluetooth: X Y Rx Ry, 16 bits.
        let word = range(0, 65535, 16);
        let shape = Shape {
            axes: [Some(word), Some(word), None, Some(word), Some(word), None],
            hat: Some(range(0, 7, 4)),
        };
        let raw = Raw {
            axes: [Some(32768), Some(32768), None, Some(0), Some(65535), None],
            hat: Some(8),
            buttons: vec![1, 4],
            ..Raw::default()
        };
        let pad = state(&shape, &raw, &Layout::NINTENDO);
        assert_eq!(pad.buttons, A | Y);
        assert_eq!((pad.right_x, pad.right_y), (-1.0, -1.0));
        assert!(pad.left_x.abs() < 0.01);
    }

    #[test]
    fn z_alone_is_both_triggers_and_rx_ry_beside_z_rz_are_the_triggers() {
        let signed = range(-32768, 32767, 16);
        let shape = Shape {
            axes: [
                Some(signed),
                Some(signed),
                Some(signed),
                Some(signed),
                Some(signed),
                None,
            ],
            hat: None,
        };
        let mut raw = Raw {
            axes: [Some(0), Some(0), Some(32767), Some(0), Some(0), None],
            ..Raw::default()
        };
        let pad = state(&shape, &raw, &Layout::DIRECTINPUT);
        assert!((pad.left_trigger - 1.0).abs() < 0.01 && pad.right_trigger == 0.0);
        raw.axes[2] = Some(-32768);
        let pad = state(&shape, &raw, &Layout::DIRECTINPUT);
        assert!(pad.left_trigger == 0.0 && (pad.right_trigger - 1.0).abs() < 0.01);
        // Z and Rz the right stick, Rx and Ry the triggers.
        let byte = range(0, 255, 8);
        let shape = Shape {
            axes: [Some(byte); AXES],
            hat: None,
        };
        let raw = Raw {
            axes: [Some(128), Some(128), Some(255), Some(255), Some(0), Some(0)],
            ..Raw::default()
        };
        let pad = state(&shape, &raw, &Layout::DIRECTINPUT);
        assert_eq!((pad.right_x, pad.right_y), (1.0, 1.0));
        assert_eq!((pad.left_trigger, pad.right_trigger), (1.0, 0.0));
    }

    #[test]
    fn hats_read_in_eighths_or_quarters_and_out_of_range_is_the_centre() {
        let eight = range(0, 7, 4);
        assert_eq!(hat(eight, 0), UP);
        assert_eq!(hat(eight, 5), DOWN | LEFT);
        assert_eq!(hat(eight, 8), 0);
        assert_eq!(hat(eight, 15), 0);
        // From 1, as some pads count.
        let from_one = range(1, 8, 4);
        assert_eq!(hat(from_one, 1), UP);
        assert_eq!(hat(from_one, 8), LEFT | UP);
        assert_eq!(hat(from_one, 0), 0);
        let four = range(0, 3, 2);
        assert_eq!(hat(four, 1), RIGHT);
        assert_eq!(hat(four, 3), LEFT);
    }

    #[test]
    fn ranges_read_signed_values_and_unsigned_maxima_given_as_negative() {
        let signed = range(-128, 127, 8);
        assert_eq!(signed.value(0xFF), -1);
        assert_eq!(signed.value(0x80), -128);
        assert_eq!(signed.stick(-128), -1.0);
        // 255 in 8 bits given as -1.
        let unsigned = range(0, -1, 8);
        assert_eq!(
            (unsigned.min, unsigned.max, unsigned.signed),
            (0, 255, false)
        );
        assert_eq!(unsigned.value(0xFF), 255);
        assert_eq!(unsigned.stick(255), 1.0);
        let word = range(0, -1, 16);
        assert_eq!(word.max, 65535);
        // Empty ranges are no range.
        assert_eq!(Range::new(5, 5, 8), None);
        // Bits beyond the value's are not its.
        assert_eq!(range(0, 255, 8).value(0x1FF), 255);
    }

    #[test]
    fn the_button_layout_can_be_set() {
        assert_eq!(
            Layout::parse(" 2, 3,1,4,5,6,7,8,9,10,11,12"),
            Some(Layout::DIRECTINPUT)
        );
        let dpad = Layout::parse("3,2,4,1,5,6,7,8,9,10,11,12,13,14,15,16").unwrap();
        assert_eq!(dpad.text(), "3,2,4,1,5,6,7,8,9,10,11,12,13,14,15,16");
        assert_eq!(Layout::PLAYSTATION.text(), "3,2,4,1,5,6,7,8,9,10,11,12");
        assert_eq!(Layout::parse("1,2,3"), None);
        assert_eq!(Layout::parse("a,b,c,d,e,f,g,h,i,j,k,l"), None);
        assert_eq!(Layout::parse(""), None);
        // D-pad buttons.
        let raw = Raw {
            buttons: vec![13, 16],
            ..Raw::default()
        };
        assert_eq!(state(&Shape::default(), &raw, &dpad).buttons, UP | RIGHT);
        // 0 is no button.
        let none = Layout::parse("0,0,0,0,0,0,0,0,0,0,0,0").unwrap();
        assert_eq!(state(&Shape::default(), &raw, &none).buttons, 0);
    }

    #[test]
    fn axes_resting_away_from_the_centre_are_not_sticks() {
        // A flight stick: X Y Z (throttle, at its bottom) Rz (twist).
        let shape = dragonrise();
        assert!(off_centre(&shape, 2, 0));
        assert!(off_centre(&shape, 2, 255));
        assert!(!off_centre(&shape, 2, 128));
        assert!(!off_centre(&shape, 0, 140));
        // Rx and Ry beside Z and Rz are triggers: at rest at an end.
        let byte = range(0, 255, 8);
        let six = Shape {
            axes: [Some(byte); AXES],
            hat: None,
        };
        assert!(!off_centre(&six, 3, 0) && !off_centre(&six, 4, 0));
        assert!(off_centre(&six, 5, 0));
        // Without Z and Rz, Rx and Ry are the right stick.
        let mut four = six;
        four.axes[2] = None;
        four.axes[5] = None;
        assert!(off_centre(&four, 3, 0));
        // An axis the pad does not have is never off.
        assert!(!off_centre(&four, 2, 0));
    }

    #[test]
    fn layouts_by_vendor() {
        assert_eq!(Layout::for_pad(0x0079), Layout::PLAYSTATION);
        assert_eq!(Layout::for_pad(0x046D), Layout::DIRECTINPUT);
        assert_eq!(Layout::for_pad(0x057E), Layout::NINTENDO);
    }

    #[test]
    fn desktop_buttons_are_start_select_and_the_d_pad() {
        let raw = Raw {
            desktop: vec![USAGE_START, 0x90, 0x93],
            ..Raw::default()
        };
        assert_eq!(
            state(&Shape::default(), &raw, &Layout::PLAYSTATION).buttons,
            MENU | UP | LEFT
        );
    }

    #[test]
    fn a_monitor_line_names_numbers_and_console_buttons() {
        let shape = dragonrise();
        let mut raw = at_rest();
        raw.buttons = vec![3];
        let pad = state(&shape, &raw, &Layout::PLAYSTATION);
        let text = line(&shape, &raw, &pad);
        assert!(text.starts_with("buttons [3], X +0.00"), "{text}");
        assert!(text.contains("->  A  ("), "{text}");
    }
}
