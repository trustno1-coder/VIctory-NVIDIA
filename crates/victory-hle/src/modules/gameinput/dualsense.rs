//! Pads read straight from HID: PlayStation 5 (DualSense, DualSense Edge),
//! PlayStation 4 (DualShock 4, both models, and Sony's wireless adapter)
//! and the Nintendo Switch Pro Controller ([`super::switch_pro`]). They are
//! not XInput devices, so without a remapper (Steam Input, DS4Windows)
//! XInput never lists them. Reports can be USB or Bluetooth. A scan thread
//! looks for new pads every [`SCAN`]; each pad has a reader thread that
//! keeps its latest state until the pad goes away. By default XInput has
//! priority over HID, since a remapper can present the same pad twice
//! (`VICTORY_DUALSENSE` covers all of these pads).

use super::{
    A, B, DOWN, LEFT, LEFT_SHOULDER, LEFT_STICK, MENU, PadState, RIGHT, RIGHT_SHOULDER,
    RIGHT_STICK, UP, VIEW, X, Y,
};

const SONY: u16 = 0x054C;
/// DualSense, DualSense Edge.
const DUALSENSE: [u16; 2] = [0x0CE6, 0x0DF2];
/// DualShock 4 (first and second model), Sony's wireless adapter.
const DUALSHOCK4: [u16; 3] = [0x05C4, 0x09CC, 0x0BA0];

/// Which pad a HID device is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    DualSense,
    DualShock4,
    SwitchPro,
}

impl Kind {
    /// The pad of this vendor and product id, `None` for anything else.
    pub(super) fn of(vendor: u16, product: u16) -> Option<Kind> {
        match (vendor, product) {
            (SONY, p) if DUALSENSE.contains(&p) => Some(Kind::DualSense),
            (SONY, p) if DUALSHOCK4.contains(&p) => Some(Kind::DualShock4),
            (super::switch_pro::NINTENDO, super::switch_pro::PRODUCT) => Some(Kind::SwitchPro),
            _ => None,
        }
    }

    /// For the log.
    pub(super) fn name(self) -> &'static str {
        match self {
            Kind::DualSense => "DualSense",
            Kind::DualShock4 => "DualShock 4",
            Kind::SwitchPro => "Switch Pro Controller",
        }
    }
}

/// The D-pad hat (low nibble of the first button byte): 0 up, clockwise in
/// eighths, 8 and above centred.
fn hat(value: u8) -> u32 {
    match value & 0xF {
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

/// Face buttons in the first button byte, the rest in the second and third
/// (the PS button and the mute button have no console pad button).
const BUTTONS: [(usize, u8, u32); 11] = [
    (0, 0x10, X), // square
    (0, 0x20, A), // cross
    (0, 0x40, B), // circle
    (0, 0x80, Y), // triangle
    (1, 0x01, LEFT_SHOULDER),
    (1, 0x02, RIGHT_SHOULDER),
    (1, 0x10, VIEW), // create
    (1, 0x20, MENU), // options
    (1, 0x40, LEFT_STICK),
    (1, 0x80, RIGHT_STICK),
    (2, 0x02, VIEW), // touchpad click
];

/// The PlayStation pad in one input report, `None` for a report of
/// another kind (and for a Switch Pro Controller, [`super::switch_pro`]).
/// `bluetooth`: the pad is connected over Bluetooth, where the full report
/// is 0x31 on a DualSense (one byte further in) and 0x11 on a DualShock 4
/// (two bytes), and 0x01 is the short report the pad sends before it was
/// asked for the full one. Both pads have the same three button bytes.
pub(super) fn parse(kind: Kind, report: &[u8], bluetooth: bool) -> Option<PadState> {
    // Offsets of the sticks (LX LY RX RY), the triggers (L2 R2) and the
    // three button bytes.
    let (sticks, triggers, buttons) = match (kind, report.first()?, bluetooth) {
        (Kind::DualSense, 0x01, false) => (1, 5, 8),
        (Kind::DualSense, 0x31, true) => (2, 6, 9),
        (Kind::DualShock4, 0x11, true) => (3, 10, 7),
        (Kind::DualSense | Kind::DualShock4, 0x01, _) => (1, 8, 5),
        _ => return None,
    };
    if report.len() < sticks + 4 || report.len() < triggers + 2 || report.len() < buttons + 3 {
        return None;
    }
    let bytes = &report[buttons..buttons + 3];
    // 0 to 255 with 128 centred; down is positive on the pad, up here.
    let axis = |v: u8| ((f32::from(v) - 127.5) / 127.5).clamp(-1.0, 1.0);
    Some(PadState {
        buttons: BUTTONS
            .iter()
            .filter(|&&(byte, mask, _)| bytes[byte] & mask != 0)
            .fold(hat(bytes[0]), |all, &(_, _, bit)| all | bit),
        left_trigger: f32::from(report[triggers]) / 255.0,
        right_trigger: f32::from(report[triggers + 1]) / 255.0,
        left_x: axis(report[sticks]),
        left_y: -axis(report[sticks + 1]),
        right_x: axis(report[sticks + 2]),
        right_y: -axis(report[sticks + 3]),
    })
}

/// The Bluetooth address in a pairing feature report (0x09, the address at
/// 1..7 on both transports), `None` when it is too short or all zeros.
fn mac(pairing: &[u8]) -> Option<[u8; 6]> {
    let mac: [u8; 6] = pairing.get(1..7)?.try_into().ok()?;
    (mac != [0; 6]).then_some(mac)
}

/// A DualShock 4's address over USB, in its pairing report (0x12, least
/// significant byte first), most significant byte first as
/// [`serial_mac`] has it.
fn dualshock4_mac(pairing: &[u8]) -> Option<[u8; 6]> {
    let mut mac = mac(pairing)?;
    mac.reverse();
    Some(mac)
}

/// A Bluetooth pad's address in its HID serial number string, which
/// Windows fills with it (twelve hex digits, perhaps with separators);
/// a DualShock 4 has no feature report with it over Bluetooth.
fn serial_mac(serial: &[u16]) -> Option<[u8; 6]> {
    let digits: Vec<u8> = String::from_utf16_lossy(serial)
        .chars()
        .take_while(|&c| c != '\0')
        .filter_map(|c| c.to_digit(16).map(|d| d as u8))
        .collect();
    let mac: [u8; 6] = (digits.len() == 12)
        .then(|| std::array::from_fn(|i| digits[2 * i] << 4 | digits[2 * i + 1]))?;
    (mac != [0; 6]).then_some(mac)
}

#[cfg(windows)]
pub(super) use host::pads;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Off,
    Auto,
    Both,
}

#[cfg(windows)]
pub(super) fn mode() -> Mode {
    static MODE: std::sync::LazyLock<Mode> =
        std::sync::LazyLock::new(|| match std::env::var("VICTORY_DUALSENSE").as_deref() {
            Ok("0") => Mode::Off,
            Ok("1") => Mode::Both,
            _ => Mode::Auto,
        });
    *MODE
}

/// XInput does not identify the corresponding physical HID device. Prefer
/// it in automatic mode. If both sources are requested, retain the slot of
/// every connected HID pad unless that slot has been claimed by XInput.
pub(super) fn slots(
    mode: Mode,
    mut xinput: [Option<PadState>; super::PADS],
    assigned: &mut [Option<u64>; super::PADS],
    hid: impl FnOnce() -> Vec<(u64, PadState)>,
) -> [Option<PadState>; super::PADS] {
    if mode == Mode::Off || mode == Mode::Auto && xinput.iter().any(Option::is_some) {
        assigned.fill(None);
        return xinput;
    }
    let pads = hid();
    for (slot, owner) in xinput.iter_mut().zip(assigned.iter_mut()) {
        if slot.is_none()
            && let Some((_, state)) = pads.iter().find(|(id, _)| Some(*id) == *owner)
        {
            *slot = Some(*state);
        } else {
            *owner = None;
        }
    }
    for (id, state) in pads {
        if assigned.contains(&Some(id)) {
            continue;
        }
        let Some(slot) = xinput.iter().position(Option::is_none) else {
            break;
        };
        xinput[slot] = Some(state);
        assigned[slot] = Some(id);
    }
    xinput
}

#[cfg(windows)]
mod host {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{LazyLock, Mutex, PoisonError};
    use std::time::Duration;

    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA,
        SP_DEVICE_INTERFACE_DETAIL_DATA_W, SetupDiDestroyDeviceInfoList,
        SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
    };
    use windows_sys::Win32::Devices::HumanInterfaceDevice::{
        HIDD_ATTRIBUTES, HIDP_CAPS, HIDP_STATUS_SUCCESS, HidD_FreePreparsedData,
        HidD_GetAttributes, HidD_GetFeature, HidD_GetHidGuid, HidD_GetPreparsedData,
        HidD_GetSerialNumberString, HidP_GetCaps,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
        INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        ReadFile, WriteFile,
    };
    use windows_sys::Win32::System::IO::{
        CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED,
    };
    use windows_sys::Win32::System::Threading::CreateEventW;

    use super::super::switch_pro::{self, Sticks};
    use super::{Kind, PadState, dualshock4_mac, mac, parse, serial_mac};

    /// How often to look for newly connected pads.
    const SCAN: Duration = Duration::from_secs(2);

    /// A pad in full-report mode sends reports all the time (hundreds a
    /// second); this long without one and we ask it whether it is still
    /// there. Over Bluetooth Windows can keep the device after the pad is
    /// switched off or out of range, and a read then waits for ever.
    const QUIET_MS: u32 = 500;

    struct Pad {
        /// This connection's: a pad that reconnects gets a new one.
        id: u64,
        path: Vec<u16>,
        /// The pad's Bluetooth address, the same over USB and Bluetooth:
        /// one pad seen through both is opened once.
        mac: Option<[u8; 6]>,
        /// `None` until the first report, and while the pad is silent.
        state: Option<PadState>,
    }

    /// The open pads, in the order they were found.
    static PADS: Mutex<Vec<Pad>> = Mutex::new(Vec::new());

    fn lock() -> std::sync::MutexGuard<'static, Vec<Pad>> {
        PADS.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Paths found to be a pad already open over its other transport, with
    /// its address: not opened again on every scan (two feature reports
    /// and a log line each time) while that pad stays open.
    static TWINS: Mutex<Vec<(Vec<u16>, [u8; 6])>> = Mutex::new(Vec::new());

    /// The open pads' latest states with their connection ids, oldest
    /// connection first, without the pads that are silent or yet to report;
    /// the first call starts looking for them.
    pub(in super::super) fn pads() -> Vec<(u64, PadState)> {
        static STARTED: LazyLock<()> = LazyLock::new(|| {
            std::thread::spawn(|| {
                loop {
                    scan();
                    std::thread::sleep(SCAN);
                }
            });
        });
        LazyLock::force(&STARTED);
        lock()
            .iter()
            .filter_map(|pad| Some((pad.id, pad.state?)))
            .collect()
    }

    /// Windows sizes each report buffer for its top-level HID collection.
    /// The calibration and pairing reports use the same feature buffer; a
    /// Switch Pro Controller has no feature reports, only output reports.
    struct Reports {
        input: Vec<u8>,
        feature: Vec<u8>,
        output: Vec<u8>,
    }

    impl Reports {
        fn for_device(handle: HANDLE, kind: Kind) -> Option<Self> {
            let mut data = 0;
            // SAFETY: the device is open and the output is writable.
            if !unsafe { HidD_GetPreparsedData(handle, &mut data) } {
                return None;
            }
            let mut caps = HIDP_CAPS::default();
            // SAFETY: `data` was obtained above and is released after its last use.
            let status = unsafe { HidP_GetCaps(data, &mut caps) };
            unsafe { HidD_FreePreparsedData(data) };
            let needed = match kind {
                Kind::DualSense | Kind::DualShock4 => caps.FeatureReportByteLength,
                // Room for a subcommand with a flash read's arguments.
                Kind::SwitchPro => caps.OutputReportByteLength.saturating_sub(15),
            };
            if status != HIDP_STATUS_SUCCESS || caps.InputReportByteLength == 0 || needed == 0 {
                return None;
            }
            Some(Self {
                input: vec![0; usize::from(caps.InputReportByteLength)],
                feature: vec![0; usize::from(caps.FeatureReportByteLength).max(1)],
                output: vec![0; usize::from(caps.OutputReportByteLength).max(1)],
            })
        }
    }

    /// Feature report `id` into `buffer` (its first byte), `false` when the
    /// pad did not answer. A round trip to the pad, so over Bluetooth it also
    /// tells whether the pad is still there.
    fn feature(handle: HANDLE, id: u8, buffer: &mut [u8]) -> bool {
        buffer.fill(0);
        buffer[0] = id;
        // SAFETY: the buffer is as long as we say.
        unsafe { HidD_GetFeature(handle, buffer.as_mut_ptr().cast(), buffer.len() as u32) }
    }

    /// Asks for the calibration report (0x05; 0x02 on a DualShock 4 over
    /// USB): it switches a pad on Bluetooth to full reports (0x31, 0x11),
    /// again after it reconnected; over USB it changes nothing. `false` when
    /// the pad did not answer.
    fn full_reports(handle: HANDLE, kind: Kind, bluetooth: bool, buffer: &mut [u8]) -> bool {
        let id = if kind == Kind::DualShock4 && !bluetooth { 0x02 } else { 0x05 };
        feature(handle, id, buffer)
    }

    /// A PlayStation pad's Bluetooth address, the same over USB and
    /// Bluetooth, if it tells it.
    fn address(handle: HANDLE, kind: Kind, bluetooth: bool, buffer: &mut [u8]) -> Option<[u8; 6]> {
        match (kind, bluetooth) {
            (Kind::DualSense, _) => feature(handle, 0x09, buffer).then(|| mac(buffer)).flatten(),
            (Kind::DualShock4, false) => {
                feature(handle, 0x12, buffer).then(|| dualshock4_mac(buffer)).flatten()
            }
            (Kind::DualShock4, true) => {
                let mut serial = [0u16; 64];
                // SAFETY: the buffer is as long as we say, in bytes.
                unsafe { HidD_GetSerialNumberString(handle, serial.as_mut_ptr().cast(), 128) }
                    .then(|| serial_mac(&serial))
                    .flatten()
            }
            // From its set-up ([`Switch::start`]).
            (Kind::SwitchPro, _) => None,
        }
    }

    /// How long the set-up of a Switch Pro Controller waits for each answer.
    const ANSWER_MS: u32 = 300;

    /// Waits up to `ms` for the overlapped transfer `overlapped` started by
    /// `started` (`ReadFile`'s or `WriteFile`'s result), cancelling it if it
    /// does not end: the bytes moved, `None` if it failed or timed out.
    ///
    /// # Safety
    /// The transfer's buffer and `overlapped` live until this returns.
    unsafe fn finish(handle: HANDLE, started: i32, overlapped: &OVERLAPPED, ms: u32) -> Option<u32> {
        let mut moved = 0;
        // SAFETY: the caller's transfer, on this handle.
        unsafe {
            if started == 0 && GetLastError() != ERROR_IO_PENDING {
                return None;
            }
            if GetOverlappedResultEx(handle, overlapped, &mut moved, ms, 0) != 0 {
                return Some(moved);
            }
            CancelIoEx(handle, overlapped);
            GetOverlappedResult(handle, overlapped, &mut moved, 1);
        }
        None
    }

    /// Writes one output report; `false` if it was not taken within `ms`
    /// (a Bluetooth pad that is gone). The handle may have a read pending.
    fn write(handle: HANDLE, report: &[u8], ms: u32) -> bool {
        // SAFETY: an unnamed manual-reset event; no security attributes.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return false;
        }
        // SAFETY: plain data; only the event is set.
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event;
        // SAFETY: the report and `overlapped` outlive the write, which
        // `finish` waits for.
        let written = unsafe {
            let started = WriteFile(handle, report.as_ptr(), report.len() as u32, std::ptr::null_mut(), &mut overlapped);
            finish(handle, started, &overlapped, ms)
        };
        // SAFETY: ours, and the write is over.
        unsafe { CloseHandle(event) };
        written.is_some()
    }

    /// Reads input reports into `buffer` until one for which `wanted` holds
    /// (its length then), for at most [`ANSWER_MS`]. Only while no reader
    /// thread has the pad.
    fn answer(handle: HANDLE, buffer: &mut [u8], wanted: impl Fn(&[u8]) -> bool) -> Option<usize> {
        // SAFETY: an unnamed manual-reset event; no security attributes.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return None;
        }
        let until = std::time::Instant::now() + Duration::from_millis(ANSWER_MS.into());
        let mut found = None;
        while found.is_none() {
            let left = until.saturating_duration_since(std::time::Instant::now()).as_millis() as u32;
            if left == 0 {
                break;
            }
            // SAFETY: plain data; only the event is set.
            let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
            overlapped.hEvent = event;
            // SAFETY: the buffer and `overlapped` outlive the read, which
            // `finish` waits for.
            let got = unsafe {
                let started = ReadFile(handle, buffer.as_mut_ptr(), buffer.len() as u32, std::ptr::null_mut(), &mut overlapped);
                finish(handle, started, &overlapped, left)
            };
            let Some(got) = got else {
                break;
            };
            if wanted(&buffer[..got as usize]) {
                found = Some(got as usize);
            }
        }
        // SAFETY: ours, and no read is pending on it.
        unsafe { CloseHandle(event) };
        found
    }

    /// A Switch Pro Controller's per-connection state: its stick
    /// calibration and the counter of the packets sent to it.
    struct Switch {
        sticks: Sticks,
        counter: u8,
    }

    impl Switch {
        /// Sends subcommand `id` and waits for its answer's data, copied
        /// out (`None`: not answered).
        fn ask(&mut self, handle: HANDLE, reports: &mut Reports, id: u8, args: &[u8]) -> Option<Vec<u8>> {
            switch_pro::subcommand(&mut reports.output, &mut self.counter, id, args);
            if !write(handle, &reports.output, ANSWER_MS) {
                return None;
            }
            let got = answer(handle, &mut reports.input, |r| switch_pro::answer(r, id).is_some())?;
            switch_pro::answer(&reports.input[..got], id).map(<[u8]>::to_vec)
        }

        /// `len` bytes at `at` in the pad's flash.
        fn flash(&mut self, handle: HANDLE, reports: &mut Reports, at: u32, len: u8) -> Option<Vec<u8>> {
            let data = self.ask(handle, reports, switch_pro::SPI_READ, &switch_pro::spi_read(at, len))?;
            switch_pro::spi_data(&data, at, len).map(<[u8]>::to_vec)
        }

        /// Asks for full reports without waiting for the answer (a reader
        /// may be reading): what tells whether a quiet pad is still there,
        /// and what a pad that reconnected needs again.
        fn full_reports(&mut self, handle: HANDLE, output: &mut [u8]) -> bool {
            switch_pro::subcommand(output, &mut self.counter, switch_pro::INPUT_MODE, &[switch_pro::FULL]);
            write(handle, output, ANSWER_MS)
        }

        /// Sets the pad up for reading: over USB the handshake that makes it
        /// talk HID, then its address, its stick calibration, the first
        /// player light (they blink until one is set) and full reports.
        /// `None` if it did not answer.
        fn start(handle: HANDLE, bluetooth: bool, reports: &mut Reports) -> Option<(Switch, Option<[u8; 6]>)> {
            let mut pad = Switch { sticks: Sticks::default(), counter: 0 };
            let mut mac = None;
            if bluetooth {
                let info = pad.ask(handle, reports, switch_pro::DEVICE_INFO, &[])?;
                mac = switch_pro::info_mac(&info);
                if mac.is_some() && lock().iter().any(|open| open.mac == mac) {
                    return Some((pad, mac));
                }
            } else {
                // A USB command, and whether it was taken (and answered, if
                // it is one that is).
                let usb = |reports: &mut Reports, command: u8, answered: bool| {
                    switch_pro::usb_command(&mut reports.output, command);
                    write(handle, &reports.output, ANSWER_MS)
                        && (!answered
                            || answer(handle, &mut reports.input, |r| switch_pro::answers_usb(r, command)).is_some())
                };
                if usb(reports, switch_pro::USB_STATUS, true) {
                    mac = switch_pro::usb_mac(&reports.input);
                }
                // Open over Bluetooth already: the handshake would take the
                // pad from there; the caller sees the twin and lets it be.
                if mac.is_some() && lock().iter().any(|open| open.mac == mac) {
                    return Some((pad, mac));
                }
                for command in [switch_pro::USB_HANDSHAKE, switch_pro::USB_HIGH_SPEED, switch_pro::USB_HANDSHAKE] {
                    if !usb(reports, command, true) {
                        return None;
                    }
                }
                usb(reports, switch_pro::USB_HID_ONLY, false);
            }
            let factory = pad.flash(handle, reports, switch_pro::FACTORY_STICKS, 18);
            let user = pad.flash(handle, reports, switch_pro::USER_STICKS, 22);
            if factory.is_none() {
                tracing::warn!("Switch Pro Controller: no stick calibration; the sticks are read as a typical pad's");
            }
            pad.sticks = Sticks::from_flash(factory.as_deref(), user.as_deref());
            pad.ask(handle, reports, switch_pro::PLAYER_LIGHTS, &[0x01]);
            pad.ask(handle, reports, switch_pro::INPUT_MODE, &[switch_pro::FULL])?;
            Some((pad, mac))
        }
    }

    /// How a pad's reports are read, and how it is asked for full ones.
    enum Decoder {
        PlayStation(Kind),
        Switch(Switch, bool),
    }

    impl Decoder {
        fn parse(&self, report: &[u8], bluetooth: bool) -> Option<PadState> {
            match self {
                Decoder::PlayStation(kind) => parse(*kind, report, bluetooth),
                Decoder::Switch(pad, labels) => switch_pro::parse(report, &pad.sticks, *labels),
            }
        }

        fn kind(&self) -> Kind {
            match self {
                Decoder::PlayStation(kind) => *kind,
                Decoder::Switch(..) => Kind::SwitchPro,
            }
        }

        /// Asks for full reports; `false` when the pad did not take it.
        fn full_reports(&mut self, handle: HANDLE, bluetooth: bool, reports: &mut Reports) -> bool {
            match self {
                Decoder::PlayStation(kind) => full_reports(handle, *kind, bluetooth, &mut reports.feature),
                Decoder::Switch(pad, _) => pad.full_reports(handle, &mut reports.output),
            }
        }
    }

    /// Opens the pads not open yet, each with a reader thread.
    fn scan() {
        let paths = hid_paths();
        {
            let pads = lock();
            // A twin whose pad went away is opened again: over USB, say,
            // after the Bluetooth link dropped with the cable still in.
            TWINS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .retain(|(path, mac)| {
                    paths.contains(path) && pads.iter().any(|pad| pad.mac == Some(*mac))
                });
        }
        for path in paths {
            // Cheap filter before opening anything: Sony's or Nintendo's
            // vendor id is in the path, as `vid_054c` (USB) or
            // `vid&0002054c` (Bluetooth).
            let lower = String::from_utf16_lossy(&path).to_ascii_lowercase();
            if !lower.contains("054c") && !lower.contains("057e") {
                continue;
            }
            if lock().iter().any(|pad| pad.path == path)
                || TWINS
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .iter()
                    .any(|(twin, _)| *twin == path)
            {
                continue;
            }
            let Some((handle, kind, mut reports)) = open(&path) else {
                continue;
            };
            let bluetooth = lower.contains("{00001124-");
            // A paired pad that is switched off can still be listed over
            // Bluetooth: it does not answer.
            let started = match kind {
                Kind::DualSense | Kind::DualShock4 => full_reports(handle, kind, bluetooth, &mut reports.feature)
                    .then(|| (Decoder::PlayStation(kind), address(handle, kind, bluetooth, &mut reports.feature))),
                Kind::SwitchPro => Switch::start(handle, bluetooth, &mut reports)
                    .map(|(pad, mac)| (Decoder::Switch(pad, switch_pro::by_label()), mac)),
            };
            let Some((decoder, mac)) = started else {
                tracing::debug!(path = %lower, pad = kind.name(), "pad listed but not answering");
                // SAFETY: just opened, nothing else has it.
                unsafe { CloseHandle(handle) };
                continue;
            };
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let id = {
                let mut pads = lock();
                let twin = mac.is_some() && pads.iter().any(|pad| pad.mac == mac);
                (!twin).then(|| {
                    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                    pads.push(Pad {
                        id,
                        path: path.clone(),
                        mac,
                        state: None,
                    });
                    id
                })
            };
            let Some(id) = id else {
                tracing::info!(path = %lower, bluetooth, pad = kind.name(), "pad already open over the other transport");
                if let Some(mac) = mac {
                    TWINS
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push((path, mac));
                }
                // SAFETY: just opened, nothing else has it.
                unsafe { CloseHandle(handle) };
                continue;
            };
            tracing::info!(path = %lower, bluetooth, pad = kind.name(), "pad connected");
            let raw = handle as usize;
            if let Err(error) = std::thread::Builder::new()
                .name(format!("{} reader", kind.name()))
                .spawn(move || read(raw as HANDLE, id, bluetooth, decoder, reports))
            {
                lock().retain(|pad| pad.id != id);
                // SAFETY: just opened, and no reader has it.
                unsafe { CloseHandle(handle) };
                tracing::warn!(%error, pad = kind.name(), "could not start a pad reader");
            }
        }
    }

    /// Reads reports until the pad goes away, then forgets it.
    fn read(handle: HANDLE, id: u64, bluetooth: bool, mut decoder: Decoder, mut reports: Reports) {
        let set = |state: Option<PadState>| {
            if let Some(pad) = lock().iter_mut().find(|pad| pad.id == id) {
                pad.state = state;
            }
        };
        // SAFETY: an unnamed manual-reset event; no security attributes.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        let why = 'pad: loop {
            if event.is_null() {
                break 'pad "no event";
            }
            // SAFETY: plain data; only the event is set.
            let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
            overlapped.hEvent = event;
            let mut got = 0;
            // SAFETY: the buffer and `overlapped` outlive the read: every way
            // out below waits for it to end first.
            let started = unsafe {
                ReadFile(
                    handle,
                    reports.input.as_mut_ptr(),
                    reports.input.len() as u32,
                    std::ptr::null_mut(),
                    &mut overlapped,
                )
            };
            // SAFETY: plain query.
            if started == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
                break 'pad "read failed";
            }
            loop {
                // SAFETY: the read started above.
                let done =
                    unsafe { GetOverlappedResultEx(handle, &overlapped, &mut got, QUIET_MS, 0) };
                if done != 0 {
                    if got == 0 {
                        break 'pad "read nothing";
                    }
                    break;
                }
                // SAFETY: plain query.
                if unsafe { GetLastError() } != WAIT_TIMEOUT {
                    break 'pad "read failed";
                }
                // Silent: whatever was held is not held any more, and the
                // pad may be gone without the read failing.
                set(None);
                if !decoder.full_reports(handle, bluetooth, &mut reports) {
                    // SAFETY: cancel the read and wait for it to end, so
                    // nothing writes into the input buffer once we return.
                    unsafe {
                        CancelIoEx(handle, &overlapped);
                        GetOverlappedResult(handle, &overlapped, &mut got, 1);
                    }
                    break 'pad "not answering";
                }
            }
            if let Some(state) = decoder.parse(&reports.input[..got as usize], bluetooth) {
                set(Some(state));
            }
        };
        tracing::info!(why, bluetooth, pad = decoder.kind().name(), "pad disconnected");
        lock().retain(|pad| pad.id != id);
        // SAFETY: ours, and nothing uses them any more.
        unsafe {
            if !event.is_null() {
                CloseHandle(event);
            }
            CloseHandle(handle);
        }
    }

    /// The pad at `path` opened for reading, `None` when it is something else
    /// or cannot be opened.
    fn open(path: &[u16]) -> Option<(HANDLE, Kind, Reports)> {
        let mut name = path.to_vec();
        name.push(0);
        // SAFETY: a terminated path; no security attributes or template.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut attributes = HIDD_ATTRIBUTES {
            Size: size_of::<HIDD_ATTRIBUTES>() as u32,
            ..Default::default()
        };
        // SAFETY: an open device and room for its attributes.
        let kind = unsafe { HidD_GetAttributes(handle, &mut attributes) }
            .then(|| Kind::of(attributes.VendorID, attributes.ProductID))
            .flatten();
        if let Some(kind) = kind
            && let Some(reports) = Reports::for_device(handle, kind)
        {
            return Some((handle, kind, reports));
        }
        // SAFETY: just opened.
        unsafe { CloseHandle(handle) };
        None
    }

    /// The device paths of the HID devices present, without terminators.
    fn hid_paths() -> Vec<Vec<u16>> {
        let mut paths = Vec::new();
        // SAFETY: plain queries into buffers we own; the list is destroyed
        // before returning.
        unsafe {
            let mut guid = std::mem::zeroed();
            HidD_GetHidGuid(&mut guid);
            let set = SetupDiGetClassDevsW(
                &guid,
                std::ptr::null(),
                std::ptr::null_mut(),
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            );
            if set == INVALID_HANDLE_VALUE as isize {
                return paths;
            }
            for index in 0.. {
                let mut interface = SP_DEVICE_INTERFACE_DATA {
                    cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                    ..std::mem::zeroed()
                };
                if SetupDiEnumDeviceInterfaces(set, std::ptr::null(), &guid, index, &mut interface)
                    == 0
                {
                    break;
                }
                let mut size = 0;
                SetupDiGetDeviceInterfaceDetailW(
                    set,
                    &interface,
                    std::ptr::null_mut(),
                    0,
                    &mut size,
                    std::ptr::null_mut(),
                );
                if size == 0 {
                    continue;
                }
                // u64s keep the detail's alignment.
                let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
                let detail = buffer
                    .as_mut_ptr()
                    .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
                (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
                if SetupDiGetDeviceInterfaceDetailW(
                    set,
                    &interface,
                    detail,
                    size,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                ) == 0
                {
                    continue;
                }
                let start = std::ptr::addr_of!((*detail).DevicePath).cast::<u16>();
                let room = (size as usize
                    - std::mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath))
                    / 2;
                let wide = std::slice::from_raw_parts(start, room);
                let end = wide.iter().position(|&c| c == 0).unwrap_or(room);
                paths.push(wide[..end].to_vec());
            }
            SetupDiDestroyDeviceInfoList(set);
        }
        paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_input_prefers_the_remapper_in_any_slot_and_falls_back_to_hid() {
        let physical = PadState {
            buttons: A,
            ..PadState::default()
        };
        let remapped = PadState {
            buttons: Y,
            ..PadState::default()
        };
        let mut assigned = [None; super::super::PADS];
        let none = [None; super::super::PADS];
        let xinput = [None, None, Some(remapped), None];

        assert_eq!(
            slots(Mode::Auto, none, &mut assigned, || vec![(1, physical)])[0],
            Some(physical)
        );
        assert_eq!(
            slots(Mode::Auto, xinput, &mut assigned, || panic!(
                "HID not needed"
            )),
            xinput
        );
        assert_eq!(
            slots(Mode::Off, none, &mut assigned, || panic!("HID disabled")),
            none
        );
        assert_eq!(
            slots(Mode::Auto, none, &mut assigned, || vec![(2, physical)])[0],
            Some(physical)
        );
    }

    #[test]
    fn a_surviving_hid_pad_keeps_its_slot_through_disconnect_and_reconnect() {
        let a = PadState {
            buttons: A,
            ..PadState::default()
        };
        let b = PadState {
            buttons: B,
            ..PadState::default()
        };
        let x = PadState {
            buttons: X,
            ..PadState::default()
        };
        let xinput = [None, Some(x), None, None];
        let mut assigned = [None; super::super::PADS];
        assert_eq!(
            slots(Mode::Both, xinput, &mut assigned, || vec![(7, a), (8, b)]),
            [Some(a), Some(x), Some(b), None]
        );
        assert_eq!(
            slots(Mode::Both, xinput, &mut assigned, || vec![(8, b)]),
            [None, Some(x), Some(b), None]
        );
        assert_eq!(
            slots(Mode::Both, xinput, &mut assigned, || vec![(8, b), (9, a)]),
            [Some(a), Some(x), Some(b), None]
        );
    }

    #[test]
    fn the_pairing_report_holds_the_address() {
        let mut pairing = [0u8; 20];
        pairing[0] = 0x09;
        pairing[1..7].copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(mac(&pairing), Some([1, 2, 3, 4, 5, 6]));
        // a pad that answered with nothing cannot be told apart
        assert_eq!(mac(&[0x09; 1]), None);
        assert_eq!(mac(&[0x09, 0, 0, 0, 0, 0, 0]), None);
    }

    #[test]
    fn a_usb_report_reads_as_a_pad() {
        let mut report = [0u8; 64];
        report[0] = 0x01;
        report[1..5].copy_from_slice(&[0, 255, 128, 128]); // left stick: left, down
        report[5] = 255; // L2
        report[8] = 0x20 | 2; // cross, D-pad right
        report[9] = 0x20 | 0x01; // options, L1
        let pad = parse(Kind::DualSense, &report, false).unwrap();
        assert_eq!(pad.buttons, A | RIGHT | MENU | LEFT_SHOULDER);
        assert_eq!(
            (pad.left_x, pad.left_y, pad.left_trigger),
            (-1.0, -1.0, 1.0)
        );
        assert!(pad.right_x.abs() < 0.01 && pad.right_y.abs() < 0.01);
    }

    #[test]
    fn bluetooth_reports_read_one_byte_further_in() {
        let mut report = [0u8; 78];
        report[0] = 0x31;
        report[2..6].copy_from_slice(&[128, 0, 255, 128]); // left up, right right
        report[7] = 255; // R2
        report[9] = 0x80 | 8; // triangle, D-pad centred
        let pad = parse(Kind::DualSense, &report, true).unwrap();
        assert_eq!(pad.buttons, Y);
        assert_eq!(
            (pad.left_y, pad.right_x, pad.right_trigger),
            (1.0, 1.0, 1.0)
        );
        // the same bytes over USB are not a pad report
        assert_eq!(parse(Kind::DualSense, &report, false), None);
    }

    #[test]
    fn the_short_bluetooth_report_reads_too() {
        let mut report = [0u8; 78];
        report[0] = 0x01;
        report[1..5].copy_from_slice(&[128; 4]);
        report[5] = 0x40 | 4; // circle, D-pad down
        report[7] = 0x02; // touchpad click
        report[8] = 128; // L2 halfway
        let pad = parse(Kind::DualSense, &report, true).unwrap();
        assert_eq!(pad.buttons, B | DOWN | VIEW);
        assert!((pad.left_trigger - 0.5).abs() < 0.01);
        // a DualShock 4's short report is laid out the same
        assert_eq!(parse(Kind::DualShock4, &report, true), Some(pad));
    }

    /// Not a test but a probe for real pads, without the game: prints every
    /// change of what the connected HID pads read as, for
    /// `VICTORY_PAD_SECONDS` (20) seconds.
    /// `cargo test -p victory-hle --lib -- --ignored --nocapture live_pads`
    #[cfg(windows)]
    #[test]
    #[ignore = "needs a pad in hand"]
    fn live_pads() {
        let seconds = std::env::var("VICTORY_PAD_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(20);
        let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).try_init();
        println!("Reading HID pads for {seconds} s: press buttons, move the sticks and triggers.");
        let until = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
        let mut last = Vec::new();
        while std::time::Instant::now() < until {
            let now = pads();
            if now != last {
                for (id, pad) in &now {
                    println!(
                        "pad {id}: buttons {:#06x} triggers {:.2} {:.2} left {:+.2} {:+.2} right {:+.2} {:+.2}",
                        pad.buttons,
                        pad.left_trigger,
                        pad.right_trigger,
                        pad.left_x,
                        pad.left_y,
                        pad.right_x,
                        pad.right_y
                    );
                }
                if now.is_empty() {
                    println!("no pad reporting");
                }
                last = now;
            }
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
    }

    #[test]
    fn pads_are_told_apart_by_vendor_and_product() {
        assert_eq!(Kind::of(0x054C, 0x0CE6), Some(Kind::DualSense));
        assert_eq!(Kind::of(0x054C, 0x0DF2), Some(Kind::DualSense));
        for product in [0x05C4, 0x09CC, 0x0BA0] {
            assert_eq!(Kind::of(0x054C, product), Some(Kind::DualShock4));
        }
        assert_eq!(Kind::of(0x057E, 0x2009), Some(Kind::SwitchPro));
        // a Joy-Con, a DualShock 3, another vendor's pad
        assert_eq!(Kind::of(0x057E, 0x2006), None);
        assert_eq!(Kind::of(0x054C, 0x0268), None);
        assert_eq!(Kind::of(0x045E, 0x0B12), None);
    }

    #[test]
    fn a_dualshock4_usb_report_has_its_buttons_before_its_triggers() {
        let mut report = [0u8; 64];
        report[0] = 0x01;
        report[1..5].copy_from_slice(&[255, 128, 128, 0]); // left right, right up
        report[5] = 0x10 | 6; // square, D-pad left
        report[6] = 0x02 | 0x10 | 0x80; // R1, share, R3
        report[7] = 0x01; // PS: no console button
        report[8] = 0; // L2
        report[9] = 255; // R2
        let pad = parse(Kind::DualShock4, &report, false).unwrap();
        assert_eq!(pad.buttons, X | LEFT | RIGHT_SHOULDER | VIEW | RIGHT_STICK);
        assert_eq!(
            (pad.left_x, pad.right_y, pad.left_trigger, pad.right_trigger),
            (1.0, 1.0, 0.0, 1.0)
        );
        // read as a DualSense's, the same bytes mean something else
        assert_ne!(parse(Kind::DualSense, &report, false), Some(pad));
        // and a switch pad's reports are not read here
        assert_eq!(parse(Kind::SwitchPro, &report, false), None);
    }

    #[test]
    fn a_dualshock4_bluetooth_report_reads_two_bytes_further_in() {
        let mut report = [0u8; 78];
        report[0] = 0x11;
        report[1] = 0xC0;
        report[3..7].copy_from_slice(&[128, 255, 0, 128]); // left down, right left
        report[7] = 0x80 | 0x20 | 1; // triangle, cross, D-pad up-right
        report[8] = 0x20 | 0x04; // options, L2 pressed
        report[10] = 255; // L2
        let pad = parse(Kind::DualShock4, &report, true).unwrap();
        assert_eq!(pad.buttons, Y | A | UP | RIGHT | MENU);
        assert_eq!(
            (pad.left_y, pad.right_x, pad.left_trigger),
            (-1.0, -1.0, 1.0)
        );
        // 0x11 is not a report a DualSense sends, nor one over USB
        assert_eq!(parse(Kind::DualSense, &report, true), None);
        assert_eq!(parse(Kind::DualShock4, &report, false), None);
    }

    #[test]
    fn a_dualshock4_address_reads_the_same_over_usb_and_bluetooth() {
        // USB pairing report: least significant byte first.
        let mut pairing = [0u8; 16];
        pairing[0] = 0x12;
        pairing[1..7].copy_from_slice(&[0x56, 0x34, 0x12, 0x6D, 0x66, 0x1C]);
        let usb = dualshock4_mac(&pairing);
        assert_eq!(usb, Some([0x1C, 0x66, 0x6D, 0x12, 0x34, 0x56]));
        // Bluetooth: the serial number string, with or without separators.
        let wide = |s: &str| s.encode_utf16().chain([0, 0x41]).collect::<Vec<u16>>();
        assert_eq!(serial_mac(&wide("1c666d123456")), usb);
        assert_eq!(serial_mac(&wide("1C:66:6D:12:34:56")), usb);
        assert_eq!(serial_mac(&wide("")), None);
        assert_eq!(serial_mac(&wide("000000000000")), None);
        assert_eq!(serial_mac(&wide("1c666d1234")), None);
    }
}
