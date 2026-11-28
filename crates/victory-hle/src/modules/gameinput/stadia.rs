//! Stadia controllers over HID (Bluetooth or USB).
//!
//! The console reports gamepads through GameInput; on a PC a Stadia
//! controller is not an XInput device, so the title never sees it. This
//! reads the pad's HID reports and gives a [`PadState`]; the host source
//! puts it in the first slot that XInput and the PlayStation pads
//! ([`super::playstation`]) leave empty (`VICTORY_HID_PADS=0`: not looked
//! for).
//!
//! Fields are read through Windows' HID parser (`HidP_*`)
//! by their declared usages, not by offsets, so Bluetooth and USB reports
//! read the same. The controller in Bluetooth mode describes itself as a
//! standard gamepad: sticks X/Y and Z/Rz, the d-pad as a hat switch,
//! triggers as the simulation page's brake and accelerator, and its buttons
//! numbered as Android's gamepad buttons (1 A, 2 B, 4 X, 5 Y, 7 L1, 8 R1,
//! 11 select, 12 start, 13 mode, 14 L3, 15 R3).
//!
//! `VICTORY_PAD_LOG=1` logs every change of the buttons with the raw HID
//! usages held, to check or correct that table on a given pad;
//! `cargo test -p victory-hle --lib stadia -- --ignored --nocapture` does the
//! same without the title for 30 seconds.
//!
//! shortcut: rumble is not sent back; the assistant and capture buttons are
//! not reported (the title's pad has none).

#![allow(non_snake_case)]

use std::sync::{Mutex, Once};
use std::time::Duration;

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    GUID_DEVINTERFACE_HID, HID_USAGE_GENERIC_HATSWITCH, HID_USAGE_GENERIC_RX, HID_USAGE_GENERIC_RY,
    HID_USAGE_GENERIC_RZ, HID_USAGE_GENERIC_X, HID_USAGE_GENERIC_Y, HID_USAGE_GENERIC_Z,
    HID_USAGE_PAGE_BUTTON, HID_USAGE_PAGE_GENERIC, HIDD_ATTRIBUTES, HIDP_CAPS, HIDP_STATUS_SUCCESS,
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetPreparsedData, HidD_GetProductString,
    HidP_GetCaps, HidP_GetUsageValue, HidP_GetUsages, HidP_Input,
};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
};

use super::{
    A, B, DOWN, LEFT, LEFT_SHOULDER, LEFT_STICK, MENU, PadState, RIGHT, RIGHT_SHOULDER,
    RIGHT_STICK, UP, VIEW, X, Y,
};

/// Google's vendor id and the Stadia controller's product id.
const GOOGLE: u16 = 0x18d1;
const STADIA: u16 = 0x9400;

/// The simulation controls page and its accelerator and brake usages (the
/// right and left triggers of a standard Bluetooth gamepad).
const PAGE_SIMULATION: u16 = 0x02;
const USAGE_ACCELERATOR: u16 = 0xc4;
const USAGE_BRAKE: u16 = 0xc5;

const GENERIC_READ: u32 = 0x8000_0000;

/// A pad reading: triggers 0 to 1, sticks -1 to 1 (up positive).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Reading {
    buttons: u32,
    left_trigger: f32,
    right_trigger: f32,
    left_x: f32,
    left_y: f32,
    right_x: f32,
    right_y: f32,
}

static READING: Mutex<Option<Reading>> = Mutex::new(None);
static START: Once = Once::new();

/// Whether HID pads are looked for (`VICTORY_HID_PADS=0`: not).
fn wanted() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HID_PADS").is_none_or(|v| v != "0"))
}

/// The pad's newest state, starting the reader on first use.
pub(super) fn latest() -> Option<PadState> {
    if !wanted() {
        return None;
    }
    START.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("stadia".into())
            .spawn(reader);
    });
    let r = (*READING.lock().unwrap_or_else(|e| e.into_inner()))?;
    Some(PadState {
        buttons: r.buttons,
        left_trigger: r.left_trigger,
        right_trigger: r.right_trigger,
        left_x: r.left_x,
        left_y: r.left_y,
        right_x: r.right_x,
        right_y: r.right_y,
    })
}

/// An open pad: its input handle, the HID parser's description of it (freed
/// on drop) and the input report length.
struct Pad {
    handle: HANDLE,
    preparsed: isize,
    input_len: usize,
}

impl Drop for Pad {
    fn drop(&mut self) {
        // SAFETY: both came from the HID API for this handle.
        unsafe {
            HidD_FreePreparsedData(self.preparsed);
            CloseHandle(self.handle);
        }
    }
}

fn reader() {
    loop {
        if let Some(pad) = open_pad() {
            read_reports(&pad, None);
        }
        *READING.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // A pad that is off, asleep or out of range is looked for again.
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Reads until the pad goes away (or `until` passes). A report that does
/// not parse is ignored.
fn read_reports(pad: &Pad, until: Option<std::time::Instant>) {
    let mut buf = vec![0u8; pad.input_len.max(64)];
    let log = std::env::var_os("VICTORY_PAD_LOG").is_some_and(|v| v != "0");
    let mut last: Option<Reading> = None;
    let mut last_usages: Vec<u16> = Vec::new();
    loop {
        if until.is_some_and(|t| std::time::Instant::now() > t) {
            return;
        }
        let mut read = 0u32;
        // SAFETY: a live handle and a buffer of its report length.
        let ok = unsafe {
            ReadFile(
                pad.handle,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || read == 0 {
            return;
        }
        let report = &buf[..read as usize];
        let usages = usage_buttons(pad.preparsed, report);
        let reading = parse(pad.preparsed, report, &usages);
        if (log || until.is_some()) && usages != last_usages {
            let raw: Vec<String> = report.iter().map(|b| format!("{b:02x}")).collect();
            let text = format!(
                "Stadia report: usages {usages:?} -> buttons {:#x}; left {:.2},{:.2} right {:.2},{:.2} triggers {:.2},{:.2}; raw {}",
                reading.buttons,
                reading.left_x,
                reading.left_y,
                reading.right_x,
                reading.right_y,
                reading.left_trigger,
                reading.right_trigger,
                raw.join(" ")
            );
            if until.is_some() {
                println!("{text}");
            } else {
                tracing::info!("{text}");
            }
        }
        last_usages = usages;
        if last == Some(reading) {
            continue;
        }
        last = Some(reading);
        *READING.lock().unwrap_or_else(|e| e.into_inner()) = Some(reading);
    }
}

/// Whether a HID device is a Stadia controller: by its ids, or by its name
/// (a Bluetooth pairing may report other ids).
fn is_stadia(handle: HANDLE, attributes: &HIDD_ATTRIBUTES) -> bool {
    if attributes.VendorID == GOOGLE && attributes.ProductID == STADIA {
        return true;
    }
    let mut name = [0u16; 128];
    // SAFETY: a live HID handle and a buffer of the size given (in bytes).
    let ok =
        unsafe { HidD_GetProductString(handle, name.as_mut_ptr().cast(), (name.len() * 2) as u32) };
    if !ok {
        return false;
    }
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    String::from_utf16_lossy(&name[..len])
        .to_ascii_lowercase()
        .contains("stadia")
}

/// Opens the first Stadia controller and keeps its HID description.
fn open_pad() -> Option<Pad> {
    for path in device_paths() {
        let mut wide = path.clone();
        wide.push(0);
        // SAFETY: a NUL-terminated path from SetupAPI, opened for reading only.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            continue;
        }
        // SAFETY: a plain Windows struct for which all zeroes is valid.
        let mut attributes: HIDD_ATTRIBUTES = unsafe { std::mem::zeroed() };
        attributes.Size = std::mem::size_of::<HIDD_ATTRIBUTES>() as u32;
        // SAFETY: a live HID handle and room for the attributes.
        let known = unsafe { HidD_GetAttributes(handle, &mut attributes) };
        if known && is_stadia(handle, &attributes) {
            let mut preparsed: isize = 0;
            // SAFETY: a live HID handle and room for the pointer.
            if !unsafe { HidD_GetPreparsedData(handle, &mut preparsed) } {
                // SAFETY: the handle came from CreateFileW above.
                unsafe { CloseHandle(handle) };
                continue;
            }
            // SAFETY: a plain Windows struct for which all zeroes is valid.
            let mut caps: HIDP_CAPS = unsafe { std::mem::zeroed() };
            // SAFETY: the preparations are ours until freed.
            let status = unsafe { HidP_GetCaps(preparsed, &mut caps) };
            // A collection that is not a gamepad's (the controller's other
            // interfaces) is passed by.
            const GAMEPAD: u16 = 0x05;
            const JOYSTICK: u16 = 0x04;
            if status != HIDP_STATUS_SUCCESS
                || caps.UsagePage != HID_USAGE_PAGE_GENERIC
                || !matches!(caps.Usage, GAMEPAD | JOYSTICK)
            {
                // SAFETY: both came from the HID API for this handle.
                unsafe {
                    HidD_FreePreparsedData(preparsed);
                    CloseHandle(handle);
                }
                continue;
            }
            let input_len = usize::from(caps.InputReportByteLength).max(1);
            tracing::info!(
                vendor = format_args!("{:#06x}", attributes.VendorID),
                product = format_args!("{:#06x}", attributes.ProductID),
                report = input_len,
                "Stadia controller connected over HID"
            );
            return Some(Pad {
                handle,
                preparsed,
                input_len,
            });
        }
        // SAFETY: the handle came from CreateFileW above and was not kept.
        unsafe { CloseHandle(handle) };
    }
    None
}

/// Every present HID interface's device path (without its NUL).
fn device_paths() -> Vec<Vec<u16>> {
    let mut paths = Vec::new();
    // SAFETY: the HID interface class, no enumerator or parent window.
    let set: HDEVINFO = unsafe {
        SetupDiGetClassDevsW(
            &GUID_DEVINTERFACE_HID,
            std::ptr::null(),
            std::ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    if set == -1 {
        return paths;
    }
    let mut index = 0u32;
    loop {
        // SAFETY: a plain Windows struct for which all zeroes is valid.
        let mut interface: SP_DEVICE_INTERFACE_DATA = unsafe { std::mem::zeroed() };
        interface.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
        // SAFETY: the set from above, and room for one interface.
        if unsafe {
            SetupDiEnumDeviceInterfaces(
                set,
                std::ptr::null(),
                &GUID_DEVINTERFACE_HID,
                index,
                &mut interface,
            )
        } == 0
        {
            break;
        }
        index += 1;
        let mut required = 0u32;
        // SAFETY: asks for the room the detail needs.
        unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                set,
                &interface,
                std::ptr::null_mut(),
                0,
                &mut required,
                std::ptr::null_mut(),
            )
        };
        if required == 0 {
            continue;
        }
        let mut buffer = vec![0u8; required as usize];
        let detail = buffer
            .as_mut_ptr()
            .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        // SAFETY: the buffer is the size the API asked for; cbSize is its header.
        unsafe {
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(
                set,
                &interface,
                detail,
                required,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ) == 0
            {
                continue;
            }
        }
        // The path starts after the u32 cbSize, as NUL-terminated UTF-16.
        let words = (buffer.len() - 4) / 2;
        // SAFETY: the API wrote the path at this offset inside our buffer.
        let path =
            unsafe { std::slice::from_raw_parts(buffer.as_ptr().add(4).cast::<u16>(), words) };
        let len = path.iter().position(|&c| c == 0).unwrap_or(words);
        paths.push(path[..len].to_vec());
    }
    // SAFETY: the set is ours.
    unsafe { SetupDiDestroyDeviceInfoList(set) };
    paths
}

/// A declared usage's value in `report`, if the descriptor has it.
fn usage_value(preparsed: isize, report: &[u8], page: u16, usage: u16) -> Option<u32> {
    let mut value = 0u32;
    // SAFETY: the report and the parser's description are live.
    let status = unsafe {
        HidP_GetUsageValue(
            HidP_Input,
            page,
            0,
            usage,
            &mut value,
            preparsed,
            report.as_ptr(),
            report.len() as u32,
        )
    };
    (status == HIDP_STATUS_SUCCESS).then_some(value)
}

/// The buttons held in `report`, as HID usage numbers.
fn usage_buttons(preparsed: isize, report: &[u8]) -> Vec<u16> {
    let mut usages = [0u16; 32];
    let mut len = usages.len() as u32;
    // SAFETY: the report and the parser's description are live; the API only
    // reads the report.
    let status = unsafe {
        HidP_GetUsages(
            HidP_Input,
            HID_USAGE_PAGE_BUTTON,
            0,
            usages.as_mut_ptr(),
            &mut len,
            preparsed,
            report.as_ptr() as *mut u8,
            report.len() as u32,
        )
    };
    if status == HIDP_STATUS_SUCCESS {
        usages[..len as usize].to_vec()
    } else {
        Vec::new()
    }
}

/// The pad state from the report's declared fields.
fn parse(preparsed: isize, report: &[u8], usages: &[u16]) -> Reading {
    let generic = |usage| usage_value(preparsed, report, HID_USAGE_PAGE_GENERIC, usage);
    let axis = |usage| generic(usage).unwrap_or(128);
    // Triggers: the simulation page's, else the generic Rx/Ry some
    // firmwares use.
    let trigger = |simulation: u16, generic_usage: u16| {
        usage_value(preparsed, report, PAGE_SIMULATION, simulation)
            .or_else(|| generic(generic_usage))
            .unwrap_or(0)
    };
    state(
        [
            axis(HID_USAGE_GENERIC_X),
            axis(HID_USAGE_GENERIC_Y),
            axis(HID_USAGE_GENERIC_Z),
            axis(HID_USAGE_GENERIC_RZ),
        ],
        [
            trigger(USAGE_BRAKE, HID_USAGE_GENERIC_RX),
            trigger(USAGE_ACCELERATOR, HID_USAGE_GENERIC_RY),
        ],
        generic(HID_USAGE_GENERIC_HATSWITCH).unwrap_or(15),
        usages,
    )
}

/// Turns the declared fields into the title's pad state. Buttons by the
/// Android gamepad numbering (module comment); A, B, X and Y as printed on
/// the Stadia controller, which has the Xbox layout.
fn state(sticks: [u32; 4], triggers: [u32; 2], hat: u32, usages: &[u16]) -> Reading {
    let centered = |v: u32| ((f32::from(v.min(255) as u8) - 127.5) / 127.5).clamp(-1.0, 1.0);
    let mut buttons = 0u32;
    for usage in usages {
        buttons |= match usage {
            1 => A,
            2 => B,
            4 => X,
            5 => Y,
            7 => LEFT_SHOULDER,
            8 => RIGHT_SHOULDER,
            11 => VIEW, // options (the three dots)
            12 => MENU, // menu (the three lines)
            13 => MENU, // the Stadia button
            14 => LEFT_STICK,
            15 => RIGHT_STICK,
            _ => 0,
        };
    }
    // 0 is up and the directions run clockwise; anything else is released
    // (8 or 15, as a descriptor's null state).
    buttons |= match hat {
        0 => UP,
        1 => UP | RIGHT,
        2 => RIGHT,
        3 => DOWN | RIGHT,
        4 => DOWN,
        5 => DOWN | LEFT,
        6 => LEFT,
        7 => UP | LEFT,
        _ => 0,
    };
    Reading {
        buttons,
        left_trigger: f32::from(triggers[0].min(255) as u8) / 255.0,
        right_trigger: f32::from(triggers[1].min(255) as u8) / 255.0,
        left_x: centered(sticks[0]),
        // The pad's Y axes grow downwards; the title's sticks grow upwards.
        left_y: -centered(sticks[1]),
        right_x: centered(sticks[2]),
        right_y: -centered(sticks[3]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CENTER: [u32; 4] = [128, 128, 128, 128];
    const RELEASED: u32 = 15;

    fn pad(usages: &[u16], hat: u32) -> Reading {
        state(CENTER, [0, 0], hat, usages)
    }

    #[test]
    fn face_buttons_map_as_printed() {
        assert_eq!(pad(&[1], RELEASED).buttons, A);
        assert_eq!(pad(&[2], RELEASED).buttons, B);
        assert_eq!(pad(&[4], RELEASED).buttons, X);
        assert_eq!(pad(&[5], RELEASED).buttons, Y);
    }

    #[test]
    fn shoulders_system_and_stick_clicks_map() {
        assert_eq!(
            pad(&[7, 8, 11, 12, 14, 15], RELEASED).buttons,
            LEFT_SHOULDER | RIGHT_SHOULDER | VIEW | MENU | LEFT_STICK | RIGHT_STICK
        );
        // Unknown usages (assistant, capture) press nothing.
        assert_eq!(pad(&[3, 6, 9, 10, 16, 17], RELEASED).buttons, 0);
    }

    #[test]
    fn hat_directions_run_clockwise_from_up_and_null_is_released() {
        let dirs = [
            UP,
            UP | RIGHT,
            RIGHT,
            DOWN | RIGHT,
            DOWN,
            DOWN | LEFT,
            LEFT,
            UP | LEFT,
        ];
        for (hat, want) in dirs.into_iter().enumerate() {
            assert_eq!(pad(&[], hat as u32).buttons, want, "hat {hat}");
        }
        assert_eq!(pad(&[], 8).buttons, 0);
        assert_eq!(pad(&[], 15).buttons, 0);
    }

    #[test]
    fn sticks_are_centered_signed_and_up_positive() {
        let r = state(CENTER, [0, 0], RELEASED, &[]);
        assert!(r.left_x.abs() < 0.01 && r.left_y.abs() < 0.01);
        assert!(r.right_x.abs() < 0.01 && r.right_y.abs() < 0.01);
        // Full right and full up (HID 0 is up) on the left stick; left and down on the right.
        let r = state([255, 0, 0, 255], [0, 0], RELEASED, &[]);
        assert_eq!((r.left_x, r.left_y), (1.0, 1.0));
        assert_eq!((r.right_x, r.right_y), (-1.0, -1.0));
    }

    #[test]
    fn triggers_run_zero_to_one() {
        let r = state(CENTER, [255, 0], RELEASED, &[]);
        assert_eq!((r.left_trigger, r.right_trigger), (1.0, 0.0));
        let r = state(CENTER, [0, 300], RELEASED, &[]);
        assert_eq!(r.right_trigger, 1.0);
    }

    /// Not part of the suite: opens the Stadia controller and prints, for 30
    /// seconds, every change of the buttons held with the raw usages and the
    /// report's bytes. Press each button once to check the table above.
    /// `cargo test -p victory-hle --lib stadia -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn shows_what_the_stadia_controller_reports() {
        let paths = device_paths();
        println!("hid interfaces: {}", paths.len());
        match open_pad() {
            Some(pad) => {
                println!(
                    "opened a Stadia controller: report length {}; press its buttons",
                    pad.input_len
                );
                read_reports(
                    &pad,
                    Some(std::time::Instant::now() + Duration::from_secs(30)),
                );
            }
            None => {
                println!("no Stadia controller found: is it on, in Bluetooth mode and connected?")
            }
        }
    }
}
