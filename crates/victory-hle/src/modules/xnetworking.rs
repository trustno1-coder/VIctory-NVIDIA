//! XNetworking as an offline console answers it: the network stack is up
//! (titles wait for that before anything else) but reaches nothing, the
//! connectivity hint never changes after the first notification, and the
//! title's security allow list, which comes from the service, cannot be
//! fetched.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::BTreeSet;
use std::ffi::{c_char, c_void};
use std::sync::Mutex;

use super::xasync::{self, AsyncBlock};
use super::xgameruntime::{
    Guid, HRESULT, Interface, Methods, Object, add_ref, missing, put, text, token,
};

pub const PROVIDER: Guid = Guid::new(
    0x37e5_6907,
    0x2f10,
    0x41e8,
    [0xb7, 0x2f, 0x36, 0xed, 0xb1, 0x85, 0x33, 0x1a],
);

/// As the title (2026) asks for it; the slots it calls (0x18-0x88)
/// take the same arguments as under the provider's own id (older titles).
pub const IID_2026: Guid = Guid::new(
    0xbf23_46b2,
    0x39af,
    0x4658,
    [0xb5, 0xea, 0x44, 0x71, 0x3c, 0x7e, 0x83, 0xb3],
);

pub static OBJECT: Object = Object::new("networking", &VTABLE);
pub const INTERFACES: &[Interface] =
    &[(PROVIDER, PROVIDER, &OBJECT), (PROVIDER, IID_2026, &OBJECT)];

/// `HRESULT_FROM_WIN32(ERROR_NETWORK_UNREACHABLE)`: what needs the service.
pub const UNREACHABLE: HRESULT = 0x8007_04CF_u32 as i32;

/// The documented default for the preferred local UDP multiplayer port.
const UDP_PORT: u16 = 3074;

/// The connectivity hint: level, cost, interface type, then four flags.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectivityHint {
    pub level: u32,
    pub cost: u32,
    pub interface_type: u32,
    pub initialized: bool,
    pub approaching_data_limit: bool,
    pub over_data_limit: bool,
    pub roaming: bool,
}

/// Initialized, no connectivity (level 1), cost unknown (0).
const OFFLINE: ConnectivityHint = ConnectivityHint {
    level: 1,
    cost: 0,
    interface_type: 0,
    initialized: true,
    approaching_data_limit: false,
    over_data_limit: false,
    roaming: false,
};

type This = *const Object;

extern "system" fn udp_port(_: This, out: *mut u16) -> HRESULT {
    put(out, UDP_PORT)
}

/// Tells our port queries apart from other calls on the same block.
static PORT: u8 = 0;

extern "system" fn udp_port_async(_: This, block: *mut AsyncBlock) -> HRESULT {
    xasync::finished_with(block, (&raw const PORT).cast(), Ok(xasync::bytes(UDP_PORT)))
}
extern "system" fn udp_port_result(_: This, block: *mut AsyncBlock, out: *mut u16) -> HRESULT {
    xasync::result(block, (&raw const PORT).cast(), out)
}

static SECURITY: u8 = 0;

fn wide(text: *const u16) -> Option<String> {
    // SAFETY: the title passes a NUL-terminated UTF-16 string or null.
    (!text.is_null()).then(|| unsafe {
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    })
}

/// The allow list comes from the service: unreachable offline.
extern "system" fn security_info_async(
    _: This,
    url: *const c_char,
    block: *mut AsyncBlock,
) -> HRESULT {
    tracing::debug!(
        url = text(url).unwrap_or_default(),
        "security information: offline"
    );
    xasync::finished(block, (&raw const SECURITY).cast(), UNREACHABLE)
}
extern "system" fn security_info_utf16_async(
    _: This,
    url: *const u16,
    block: *mut AsyncBlock,
) -> HRESULT {
    tracing::debug!(
        url = wide(url).unwrap_or_default(),
        "security information (utf16): offline"
    );
    xasync::finished(block, (&raw const SECURITY).cast(), UNREACHABLE)
}
extern "system" fn security_info_size(_: This, block: *mut AsyncBlock, out: *mut usize) -> HRESULT {
    xasync::XAsyncGetResultSize(block, out)
}
extern "system" fn security_info_result(
    _: This,
    block: *mut AsyncBlock,
    size: usize,
    used: *mut usize,
    buffer: *mut u8,
    _info: *mut *const c_void,
) -> HRESULT {
    xasync::XAsyncGetResult(
        block,
        (&raw const SECURITY).cast(),
        size,
        buffer.cast(),
        used,
    )
}

/// No security information can exist offline, so no certificate passes.
extern "system" fn verify_certificate(
    _: This,
    _request: *mut c_void,
    _info: *const c_void,
) -> HRESULT {
    UNREACHABLE
}

extern "system" fn connectivity_hint(_: This, out: *mut ConnectivityHint) -> HRESULT {
    put(out, OFFLINE)
}

/// `XTaskQueuePort::Completion`.
const COMPLETION: u32 = 1;

type HintCallback = extern "system" fn(*mut c_void, *const ConnectivityHint);
type PortCallback = extern "system" fn(*mut c_void, u16);

/// Registrations not taken back, by token.
static REGISTERED: Mutex<BTreeSet<u64>> = Mutex::new(BTreeSet::new());

fn registered() -> std::sync::MutexGuard<'static, BTreeSet<u64>> {
    REGISTERED.lock().unwrap_or_else(|e| e.into_inner())
}

/// A registration's initial notification, on its way through the queue.
struct Initial {
    token: u64,
    context: usize,
    callback: Callback,
}

#[derive(Clone, Copy)]
enum Callback {
    Hint(HintCallback),
    Port(PortCallback),
}

/// The initial notification, from the queue's completion port: the hint or
/// the port as they are (and stay). Not if the registration was taken back
/// meanwhile, or the queue terminated.
extern "system" fn notify(context: *mut c_void, canceled: bool) {
    // SAFETY: boxed in `register`; the queue runs each callback once.
    let initial = unsafe { Box::from_raw(context.cast::<Initial>()) };
    if canceled || !registered().contains(&initial.token) {
        return;
    }
    let context = initial.context as *mut c_void;
    match initial.callback {
        Callback::Hint(callback) => callback(context, &OFFLINE),
        Callback::Port(callback) => callback(context, UDP_PORT),
    }
}

/// Both registrations "fire an initial notification callback", on the
/// queue they are given (null: the process queue). The hint and preferred
/// port never change offline, so no other callback runs.
fn register(
    queue: *const c_void,
    context: *mut c_void,
    callback: Option<Callback>,
    out: *mut u64,
) -> HRESULT {
    if out.is_null() {
        return xasync::E_INVALIDARG;
    }
    let token = token();
    registered().insert(token);
    if let Some(callback) = callback {
        let initial = Box::into_raw(Box::new(Initial {
            token,
            context: context as usize,
            callback,
        }));
        let hr = xasync::XTaskQueueSubmitDelayedCallback(queue.cast(), COMPLETION, 0, initial.cast(), Some(notify));
        if hr < 0 {
            // SAFETY: not queued, so still ours.
            drop(unsafe { Box::from_raw(initial) });
            registered().remove(&token);
            return hr;
        }
    }
    put(out, token)
}
extern "system" fn register_hint(
    _: This,
    queue: *const c_void,
    context: *mut c_void,
    callback: Option<HintCallback>,
    out: *mut u64,
) -> HRESULT {
    register(queue, context, callback.map(Callback::Hint), out)
}
extern "system" fn register_port(
    _: This,
    queue: *const c_void,
    context: *mut c_void,
    callback: Option<PortCallback>,
    out: *mut u64,
) -> HRESULT {
    register(queue, context, callback.map(Callback::Port), out)
}
extern "system" fn unregister(_: This, token: u64, _wait: bool) -> bool {
    registered().remove(&token);
    true
}

static VTABLE: Methods<18> = Methods([
    missing::<0> as *const (),
    add_ref as *const (),
    add_ref as *const (),
    udp_port as *const (),                  // 0x18
    udp_port_async as *const (),            // 0x20
    udp_port_result as *const (),           // 0x28
    register_port as *const (),             // 0x30 port changed
    unregister as *const (),                // 0x38
    security_info_async as *const (),       // 0x40
    security_info_size as *const (),        // 0x48
    security_info_result as *const (),      // 0x50
    security_info_utf16_async as *const (), // 0x58
    security_info_size as *const (),        // 0x60
    security_info_result as *const (),      // 0x68
    verify_certificate as *const (),        // 0x70
    connectivity_hint as *const (),         // 0x78
    register_hint as *const (),             // 0x80 hint changed
    unregister as *const (),                // 0x88
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::xgameruntime::{S_OK, method};

    fn block() -> AsyncBlock {
        AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        }
    }

    #[test]
    fn offline_but_initialized() {
        let this: This = &OBJECT;
        type Hint = extern "system" fn(This, *mut ConnectivityHint) -> HRESULT;
        let mut hint = OFFLINE;
        hint.initialized = false;
        assert_eq!(method::<Hint>(&OBJECT, 0x78)(this, &mut hint), S_OK);
        assert!(hint.initialized && hint.level == 1);
        assert_eq!(size_of::<ConnectivityHint>(), 16);

        type Begin = extern "system" fn(This, *mut AsyncBlock) -> HRESULT;
        type Port = extern "system" fn(This, *mut AsyncBlock, *mut u16) -> HRESULT;
        let mut b = block();
        assert_eq!(method::<Begin>(&OBJECT, 0x20)(this, &mut b), S_OK);
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        let mut port = 0;
        assert_eq!(method::<Port>(&OBJECT, 0x28)(this, &mut b, &mut port), S_OK);
        assert_eq!(port, 3074);

        type Register = extern "system" fn(
            This,
            *const c_void,
            *mut c_void,
            *const c_void,
            *mut u64,
        ) -> HRESULT;
        type Unregister = extern "system" fn(This, u64, bool) -> bool;
        let mut token = 0;
        assert_eq!(
            method::<Register>(&OBJECT, 0x30)(
                this,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                &mut token,
            ),
            S_OK
        );
        assert_ne!(token, 0);
        assert!(method::<Unregister>(&OBJECT, 0x38)(this, token, false));

        type Url = extern "system" fn(This, *const c_char, *mut AsyncBlock) -> HRESULT;
        type Size = extern "system" fn(This, *mut AsyncBlock, *mut usize) -> HRESULT;
        let mut b = block();
        let url = c"https://example.invalid/";
        assert_eq!(
            method::<Url>(&OBJECT, 0x40)(this, url.as_ptr(), &mut b),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), UNREACHABLE);
        let mut size = 0;
        assert_eq!(
            method::<Size>(&OBJECT, 0x48)(this, &mut b, &mut size),
            UNREACHABLE
        );

        type UrlUtf16 = extern "system" fn(This, *const u16, *mut AsyncBlock) -> HRESULT;
        let mut b = block();
        let url_utf16: Vec<u16> = "https://example.invalid/\0".encode_utf16().collect();
        assert_eq!(
            method::<UrlUtf16>(&OBJECT, 0x58)(this, url_utf16.as_ptr(), &mut b),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), UNREACHABLE);
        let mut size = 0;
        assert_eq!(
            method::<Size>(&OBJECT, 0x60)(this, &mut b, &mut size),
            UNREACHABLE
        );

        token = 0;
        assert_eq!(
            method::<Register>(&OBJECT, 0x80)(
                this,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                &mut token,
            ),
            S_OK
        );
        assert_ne!(token, 0);
        assert!(method::<Unregister>(&OBJECT, 0x88)(this, token, false));
    }

    /// Each registration sends its initial notification once, on the
    /// completion port of the queue it was given; one taken back first
    /// sends none.
    #[test]
    fn registrations_send_an_initial_notification_on_their_queue() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static HINTS: AtomicU32 = AtomicU32::new(0);
        static PORT: AtomicU32 = AtomicU32::new(0);
        extern "system" fn hint(context: *mut c_void, hint: *const ConnectivityHint) {
            assert_eq!(context as usize, 7);
            // SAFETY: the hint we pass.
            assert!(unsafe { &*hint }.initialized);
            HINTS.fetch_add(1, Ordering::SeqCst);
        }
        extern "system" fn port(_: *mut c_void, port: u16) {
            PORT.store(u32::from(port), Ordering::SeqCst);
        }
        type Register<F> = extern "system" fn(This, *const xasync::Queue, *mut c_void, F, *mut u64) -> HRESULT;
        type Unregister = extern "system" fn(This, u64, bool) -> bool;
        let this: This = &OBJECT;
        let mut q = std::ptr::null();
        assert_eq!(xasync::XTaskQueueCreate(0, 0, &mut q), S_OK); // manual
        let dispatch = |port| xasync::XTaskQueueDispatch(q, port, 0);

        let mut token = 0;
        let register = method::<Register<HintCallback>>(&OBJECT, 0x80);
        assert_eq!(register(this, q, 7 as *mut c_void, hint, &mut token), S_OK);
        assert!(!dispatch(0), "nothing on the work port");
        assert_eq!(HINTS.load(Ordering::SeqCst), 0, "not before the queue runs it");
        assert!(dispatch(COMPLETION));
        assert_eq!(HINTS.load(Ordering::SeqCst), 1);
        assert!(!dispatch(COMPLETION), "once");
        assert!(method::<Unregister>(&OBJECT, 0x88)(this, token, false));

        let register = method::<Register<PortCallback>>(&OBJECT, 0x30);
        assert_eq!(register(this, q, std::ptr::null_mut(), port, &mut token), S_OK);
        assert!(dispatch(COMPLETION));
        assert_eq!(PORT.load(Ordering::SeqCst), u32::from(UDP_PORT));

        // Taken back before the queue runs it.
        let register = method::<Register<HintCallback>>(&OBJECT, 0x80);
        assert_eq!(register(this, q, 7 as *mut c_void, hint, &mut token), S_OK);
        assert!(method::<Unregister>(&OBJECT, 0x88)(this, token, false));
        assert!(dispatch(COMPLETION));
        assert_eq!(HINTS.load(Ordering::SeqCst), 1);
        xasync::XTaskQueueCloseHandle(q);
    }
}
