//! Where every thread's time goes, in release builds (`VICTORY_CPU_STAGES`, on unless `0`):
//! a sampling profiler of our own process, summed up in a few lines beside each frame-rate
//! line.
//!
//! A thread of its own (`victory-stages`) stops each thread of the process a few hundred times
//! a second (`VICTORY_CPU_STAGES_HZ`, 250), reads where it is and walks its stack back through
//! every module but the title's image (their `.pdata`, read here once a second, and
//! `RtlVirtualUnwind`) until it reaches the title's code, then lets it go on. Nothing is
//! allocated, locked or logged while a thread stands: what a stopped thread may hold, nothing
//! here waits for. A sample is weighed by the time since the round before, so the sums are
//! milliseconds whatever the timer's grain.
//!
//! Per frame (the frame-rate line's frames) it says:
//! - **the title's threads**: time in the title's own code, in ours (by the function of ours the
//!   title called: an import of the runtime's, a method of a D3D12 object of ours) busy, waiting
//!   (the system's wait calls under it) or handling faults (our exception handlers: guards, soft
//!   views); the three busiest title threads split the same way, the busiest one's own time
//!   being the title's floor;
//! - **our threads**, by name (`victory-submit`, `victory-present`, `victory-relay`, the pools
//!   `victory-list`, `victory-land`, `victory-under`, …): CPU time (the kernel's count, every
//!   thread of the process, the unnamed ones (driver, system) together) and the functions of
//!   ours they were found in most, busy or waiting;
//! - **the critical path**: while a title thread waited in a present of ours, where the
//!   submitting thread was.
//!
//! Function names come from the program's own symbols (`dbghelp`, beside the program), looked
//! up on this thread when a report is written, never while a thread stands.

use std::collections::HashMap;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, LocalFree};
use windows_sys::Win32::System::Diagnostics::Debug::{
    CONTEXT, CONTEXT_FULL_AMD64, GetThreadContext, IMAGE_RUNTIME_FUNCTION_ENTRY, RtlVirtualUnwind, UNW_FLAG_NHANDLER,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW, TH32CS_SNAPMODULE, TH32CS_SNAPTHREAD,
    THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, GetThreadDescription, GetThreadTimes, OpenThread, ResumeThread,
    SuspendThread, THREAD_GET_CONTEXT, THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME,
};

/// Frames walked at most, and functions of ours kept of them.
const DEPTH: usize = 40;
const KEPT: usize = 8;

/// Whether it runs (`VICTORY_CPU_STAGES=0`: not).
fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CPU_STAGES").is_none_or(|v| v != "0"))
}

fn hz() -> u64 {
    std::env::var("VICTORY_CPU_STAGES_HZ").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(250u64).clamp(10, 2000)
}

/// The frames of the frame-rate line just written (f64 bits), for the next report.
static FRAMES: AtomicU64 = AtomicU64::new(0);
static ASKED: AtomicBool = AtomicBool::new(false);

/// Called beside the frame-rate line: a report of the time since the last one goes out (from
/// the sampling thread, soon after). Starts that thread the first time.
pub(crate) fn frame_line(frames: f64) {
    if !on() || frames <= 0.0 {
        return;
    }
    static START: Once = Once::new();
    START.call_once(|| {
        let _ = std::thread::Builder::new().name("victory-stages".into()).spawn(run);
    });
    FRAMES.store(frames.to_bits(), Ordering::Relaxed);
    ASKED.store(true, Ordering::Release);
}

/// `.pdata` of a module, as the image has it.
#[repr(C)]
#[derive(Clone, Copy)]
struct RuntimeFunction {
    begin: u32,
    end: u32,
    unwind: u32,
}

#[derive(Clone, Copy)]
struct Module {
    base: usize,
    end: usize,
    pdata: *const RuntimeFunction,
    count: usize,
    ours: bool,
}

/// The process's modules (not the title's image), by base.
struct Modules {
    list: Vec<Module>,
    /// The title's image.
    title: (usize, usize),
    /// The system's wait calls (their 32-byte stubs).
    waits: Vec<usize>,
    /// Where an exception's handling begins on a thread's stack.
    dispatcher: usize,
}

impl Modules {
    fn read() -> Modules {
        let mut list = Vec::new();
        let mut title = (0, 0);
        // SAFETY: plain queries of our own process; the modules stay loaded while listed
        // (a module let go later is read no more: the list is read again each second, and an
        // address outside every module ends a walk).
        unsafe {
            let ours = GetModuleHandleW(std::ptr::null()) as usize;
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, GetCurrentProcessId());
            if !snap.is_null() && snap as isize != -1 {
                let mut e: MODULEENTRY32W = std::mem::zeroed();
                e.dwSize = size_of::<MODULEENTRY32W>() as u32;
                let mut more = Module32FirstW(snap, &mut e) != 0;
                while more {
                    let base = e.modBaseAddr as usize;
                    let end = base + e.modBaseSize as usize;
                    let name = String::from_utf16_lossy(&e.szModule[..e.szModule.iter().position(|&c| c == 0).unwrap_or(0)]);
                    if name.to_ascii_lowercase().starts_with("victory-image") {
                        title = (base, end);
                    } else if let Some((pdata, count)) = pdata(base, end) {
                        list.push(Module { base, end, pdata, count, ours: base == ours });
                    }
                    more = Module32NextW(snap, &mut e) != 0;
                }
                CloseHandle(snap);
            }
        }
        list.sort_by_key(|m| m.base);
        let mut waits = Vec::new();
        let mut dispatcher = 0;
        // SAFETY: the system's own library, by name; addresses only.
        unsafe {
            let ntdll = GetModuleHandleW(windows_sys::core::w!("ntdll.dll"));
            if !ntdll.is_null() {
                for name in [
                    c"NtWaitForSingleObject",
                    c"NtWaitForMultipleObjects",
                    c"NtDelayExecution",
                    c"NtWaitForAlertByThreadId",
                    c"NtRemoveIoCompletion",
                    c"NtRemoveIoCompletionEx",
                    c"NtSignalAndWaitForSingleObject",
                    c"NtWaitForKeyedEvent",
                    c"NtYieldExecution",
                    c"NtWaitForWorkViaWorkerFactory",
                    c"NtUserMsgWaitForMultipleObjectsEx",
                ] {
                    if let Some(f) = GetProcAddress(ntdll, name.as_ptr().cast()) {
                        waits.push(f as usize);
                    }
                }
                if let Some(f) = GetProcAddress(ntdll, c"KiUserExceptionDispatcher".as_ptr().cast()) {
                    dispatcher = f as usize;
                }
            }
        }
        Modules { list, title, waits, dispatcher }
    }

    fn module(&self, at: usize) -> Option<&Module> {
        let i = self.list.partition_point(|m| m.base <= at);
        let m = self.list.get(i.checked_sub(1)?)?;
        (at < m.end).then_some(m)
    }

    fn in_title(&self, at: usize) -> bool {
        at >= self.title.0 && at < self.title.1
    }

    fn waiting(&self, at: usize) -> bool {
        self.waits.iter().any(|&w| at >= w && at < w + 0x20)
    }
}

/// The exception directory of the image at `base`.
///
/// # Safety
/// `base..end` is a loaded image.
unsafe fn pdata(base: usize, end: usize) -> Option<(*const RuntimeFunction, usize)> {
    // SAFETY: the image's headers, inside it.
    unsafe {
        if end - base < 0x400 || *(base as *const u16) != 0x5A4D {
            return None;
        }
        let nt = base + *((base + 0x3C) as *const u32) as usize;
        if nt + 0x108 > end || *(nt as *const u32) != 0x4550 || *((nt + 0x18) as *const u16) != 0x20B {
            return None;
        }
        // (the optional header's data directories begin 112 bytes in; the exception one is the 4th)
        let dir = nt + 0x18 + 112 + 3 * 8;
        let rva = *(dir as *const u32) as usize;
        let size = *((dir + 4) as *const u32) as usize;
        if rva == 0 || size < 12 || base + rva + size > end {
            return None;
        }
        Some(((base + rva) as *const RuntimeFunction, size / 12))
    }
}

/// The function `at` lies in: its entry, if it has one.
///
/// # Safety
/// `m` is loaded.
unsafe fn lookup(m: &Module, at: usize) -> Option<*const RuntimeFunction> {
    let rva = (at - m.base) as u32;
    // SAFETY: the module's own table.
    let table = unsafe { std::slice::from_raw_parts(m.pdata, m.count) };
    let i = table.partition_point(|f| f.begin <= rva);
    let mut f = table.get(i.checked_sub(1)?)? as *const RuntimeFunction;
    // SAFETY: as above; an entry that points at another (its low bit) is followed once.
    unsafe {
        if rva >= (*f).end {
            return None;
        }
        if (*f).unwind & 1 != 0 {
            f = (m.base + ((*f).unwind & !1) as usize) as *const RuntimeFunction;
        }
    }
    Some(f)
}

#[repr(C, align(16))]
struct Aligned(CONTEXT);

/// What a stopped thread was found doing.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
enum Kind {
    /// In the title's own code.
    Title,
    /// In ours (or a library's under ours), running.
    Busy,
    /// In one of the system's wait calls.
    Wait,
    /// In our handling of an exception (a fault on a guarded page and the like).
    Fault,
    /// Nowhere we know (no module, a walk that broke off).
    Other,
}

/// One sample: what kind, the function of ours the title called (0 on our threads or in the
/// title's code) and the functions of ours on the stack, innermost first.
#[derive(Clone, Copy)]
struct Sample {
    kind: Kind,
    entry: usize,
    ours: [usize; KEPT],
}

/// Stops `thread`, reads it, lets it go. Nothing here allocates or takes a lock between the
/// stop and the go.
///
/// # Safety
/// `thread` is a handle with the rights to stop and read it, not this thread.
unsafe fn sample(thread: HANDLE, stack: (usize, usize), m: &Modules) -> Option<Sample> {
    let mut ctx: Aligned = Aligned(unsafe { std::mem::zeroed() });
    ctx.0.ContextFlags = CONTEXT_FULL_AMD64;
    // SAFETY: the caller's handle.
    if unsafe { SuspendThread(thread) } == u32::MAX {
        return None;
    }
    // SAFETY: a stopped thread of ours, an aligned context.
    let read = unsafe { GetThreadContext(thread, &mut ctx.0) } != 0;
    let out = if read { Some(walk(&mut ctx, stack, m)) } else { None };
    // SAFETY: as above.
    unsafe { ResumeThread(thread) };
    out
}

/// The stack of a stopped thread, from `ctx` back to the title's code.
fn walk(ctx: &mut Aligned, stack: (usize, usize), m: &Modules) -> Sample {
    let first = ctx.0.Rip as usize;
    let mut s = Sample { kind: Kind::Other, entry: 0, ours: [0; KEPT] };
    if m.in_title(first) {
        s.kind = Kind::Title;
        return s;
    }
    let waiting = m.waiting(first);
    let mut kept = 0;
    let mut last_ours = 0;
    let mut fault = false;
    let mut reached = false;
    for _ in 0..DEPTH {
        let rip = ctx.0.Rip as usize;
        if rip == 0 {
            break;
        }
        if m.in_title(rip) {
            reached = true;
            break;
        }
        let Some(module) = m.module(rip) else { break };
        let rsp = ctx.0.Rsp as usize;
        // (a stack pointer outside the thread's stack ends the walk: nothing is read there)
        if rsp & 7 != 0 || rsp < stack.0 || rsp + 64 > stack.1 {
            break;
        }
        // SAFETY: the module is loaded (listed this second).
        let entry = unsafe { lookup(module, rip) };
        if let Some(f) = entry {
            // SAFETY: as above.
            let begin = module.base + unsafe { (*f).begin } as usize;
            if begin == m.dispatcher {
                fault = true;
            }
            if module.ours {
                if kept < KEPT {
                    s.ours[kept] = begin;
                    kept += 1;
                }
                last_ours = begin;
            }
            let mut handler: *mut std::ffi::c_void = std::ptr::null_mut();
            let mut frame = 0u64;
            // SAFETY: the function's own unwind data, a context of a stopped thread whose
            // stack is where its RSP says (checked above).
            unsafe {
                RtlVirtualUnwind(
                    UNW_FLAG_NHANDLER,
                    module.base as u64,
                    rip as u64,
                    f as *const IMAGE_RUNTIME_FUNCTION_ENTRY,
                    &mut ctx.0,
                    &mut handler,
                    &mut frame,
                    std::ptr::null_mut(),
                );
            }
        } else {
            // A leaf: its return address is on top of the stack.
            if module.ours && kept < KEPT {
                s.ours[kept] = rip;
                kept += 1;
            }
            // SAFETY: the top of a stopped thread's stack.
            ctx.0.Rip = unsafe { *(rsp as *const u64) };
            ctx.0.Rsp += 8;
        }
        if (ctx.0.Rsp as usize) <= rsp {
            break;
        }
    }
    s.kind = if fault {
        Kind::Fault
    } else if waiting {
        Kind::Wait
    } else if kept > 0 || m.module(first).is_some() {
        Kind::Busy
    } else {
        Kind::Other
    };
    if reached {
        s.entry = last_ours;
    }
    s
}

/// A thread of the process: whether the title's, its group's name.
struct Thread {
    handle: HANDLE,
    /// Its stack: the lowest address it may grow to, and its base.
    stack: (usize, usize),
    title: bool,
    group: usize,
    /// CPU time (100 ns) at the last report.
    cpu: u64,
}

/// A group of threads' name: the thread's name without a trailing number.
fn group_of(name: &str) -> String {
    let t = name.trim_end_matches(|c: char| c.is_ascii_digit()).trim_end_matches(['-', ' ', '#', '_']);
    if t.is_empty() { "(unnamed)".to_owned() } else { t.to_owned() }
}

fn cpu_time(h: HANDLE) -> u64 {
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut c, mut e, mut k, mut u) = (z, z, z, z);
    // SAFETY: a handle with the right to ask.
    if unsafe { GetThreadTimes(h, &mut c, &mut e, &mut k, &mut u) } == 0 {
        return 0;
    }
    let t = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    t(k) + t(u)
}

/// The thread's start address (`ThreadQuerySetWin32StartAddress`).
fn start_of(h: HANDLE) -> usize {
    type Query = unsafe extern "system" fn(HANDLE, u32, *mut std::ffi::c_void, u32, *mut u32) -> i32;
    static QUERY: std::sync::OnceLock<Option<Query>> = std::sync::OnceLock::new();
    let q = QUERY.get_or_init(|| {
        // SAFETY: the system's library and function, by name and signature.
        unsafe {
            let ntdll = GetModuleHandleW(windows_sys::core::w!("ntdll.dll"));
            GetProcAddress(ntdll, c"NtQueryInformationThread".as_ptr().cast()).map(|f| std::mem::transmute::<_, Query>(f))
        }
    });
    let mut start = 0usize;
    if let Some(q) = q {
        // SAFETY: a pointer-sized answer.
        unsafe { q(h, 9, (&mut start as *mut usize).cast(), size_of::<usize>() as u32, std::ptr::null_mut()) };
    }
    start
}

/// The thread's stack, from its TEB: (`DeallocationStack`, `StackBase`); (0, 0) if unknown.
fn stack_of(h: HANDLE) -> (usize, usize) {
    type Query = unsafe extern "system" fn(HANDLE, u32, *mut std::ffi::c_void, u32, *mut u32) -> i32;
    // SAFETY: the system's library and function, by name and signature.
    let q: Option<Query> = unsafe {
        let ntdll = GetModuleHandleW(windows_sys::core::w!("ntdll.dll"));
        GetProcAddress(ntdll, c"NtQueryInformationThread".as_ptr().cast()).map(|f| std::mem::transmute::<_, Query>(f))
    };
    let Some(q) = q else { return (0, 0) };
    // THREAD_BASIC_INFORMATION: the TEB's address 8 bytes in (48 bytes in all).
    let mut info = [0usize; 6];
    // SAFETY: a buffer of the class's size.
    if unsafe { q(h, 0, info.as_mut_ptr().cast(), 48, std::ptr::null_mut()) } < 0 || info[1] == 0 {
        return (0, 0);
    }
    let teb = info[1];
    // SAFETY: a live thread's TEB (the thread is ours and its handle open): `NT_TIB.StackBase`
    // at 8, `DeallocationStack` at 0x1478 (x64).
    unsafe { (*((teb + 0x1478) as *const usize), *((teb + 8) as *const usize)) }
}

fn name_of(h: HANDLE) -> String {
    let mut p: *mut u16 = std::ptr::null_mut();
    // SAFETY: the system's string, freed after.
    unsafe {
        if GetThreadDescription(h, &mut p) < 0 || p.is_null() {
            return String::new();
        }
        let n = (0..).take_while(|&i| *p.add(i) != 0).count();
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
        LocalFree(p.cast());
        s
    }
}

/// The function names of our program, by address (dbghelp, loaded on first use).
struct Names {
    from_address: Option<(unsafe extern "system" fn(*mut std::ffi::c_void, u64, *mut u64, *mut u8) -> i32, HANDLE)>,
    known: HashMap<usize, String>,
}

impl Names {
    fn new() -> Names {
        use windows_sys::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExA};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        type Initialize = unsafe extern "system" fn(HANDLE, *const u16, i32) -> i32;
        type Options = unsafe extern "system" fn(u32) -> u32;
        // SAFETY: the system's library and functions, by name and signature.
        let from_address = unsafe {
            let module = LoadLibraryExA(c"dbghelp.dll".as_ptr().cast(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
            (|| {
                if module.is_null() {
                    return None;
                }
                let options: Options = std::mem::transmute(GetProcAddress(module, c"SymSetOptions".as_ptr().cast())?);
                let initialize: Initialize = std::mem::transmute(GetProcAddress(module, c"SymInitializeW".as_ptr().cast())?);
                let from = std::mem::transmute::<
                    unsafe extern "system" fn() -> isize,
                    unsafe extern "system" fn(*mut std::ffi::c_void, u64, *mut u64, *mut u8) -> i32,
                >(GetProcAddress(module, c"SymFromAddr".as_ptr().cast())?);
                let process = GetCurrentProcess();
                options(0x2 | 0x4 | 0x200);
                let beside = std::env::current_exe().ok().and_then(|p| Some(p.parent()?.to_path_buf())).unwrap_or_default();
                let beside: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(beside.as_os_str()).chain([0]).collect();
                initialize(process, beside.as_ptr(), 1);
                Some((from, process))
            })()
        };
        Names { from_address, known: HashMap::new() }
    }

    fn name(&mut self, at: usize) -> String {
        if at == 0 {
            return "(none)".to_owned();
        }
        if let Some(n) = self.known.get(&at) {
            return n.clone();
        }
        const NAME: usize = 84;
        const ROOM: usize = 512;
        let mut info = [0u64; (NAME + ROOM) / 8 + 1];
        let p = info.as_mut_ptr().cast::<u8>();
        let mut off = 0u64;
        let found = self.from_address.and_then(|(from, process)| {
            // SAFETY: a buffer of the structure's size and the name's, sizes set as asked.
            unsafe {
                p.cast::<u32>().write(88);
                p.add(80).cast::<u32>().write(ROOM as u32);
                if from(process, at as u64, &mut off, p) == 0 {
                    return None;
                }
                Some(std::ffi::CStr::from_ptr(p.add(NAME).cast()).to_string_lossy().into_owned())
            }
        });
        let n = short(&found.unwrap_or_else(|| format!("{at:#x}")));
        self.known.insert(at, n.clone());
        n
    }
}

/// A function's name without its generic arguments and hash, and only its last three parts.
fn short(name: &str) -> String {
    let mut out = String::new();
    let mut depth = 0i32;
    for c in name.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    // (a legacy symbol's hash: `::h` and 16 hex digits)
    let out = match out.rfind("::h") {
        Some(i) if out.len() - i == 19 && out[i + 3..].bytes().all(|b| b.is_ascii_hexdigit()) => &out[..i],
        _ => out.as_str(),
    };
    let parts: Vec<&str> = out.split("::").filter(|p| !p.is_empty() && !p.starts_with("{{closure")).collect();
    let tail = parts.len().saturating_sub(3);
    parts[tail..].join("::")
}

/// The innermost function of ours on a stack whose name says it is the runtime's own (not the
/// standard library's), else the innermost.
fn stage(names: &mut Names, s: &Sample) -> String {
    let mut first = None;
    for &f in s.ours.iter().filter(|&&f| f != 0) {
        let n = names.name(f);
        let full = names.full(f);
        if full.starts_with("victory") {
            return n;
        }
        first.get_or_insert(n);
    }
    first.unwrap_or_else(|| "(not ours)".to_owned())
}

impl Names {
    /// The name as dbghelp has it (for telling ours from the standard library's).
    fn full(&mut self, at: usize) -> String {
        // (`name` keeps the last parts; the crate is the first)
        let key = at ^ (1usize << 63);
        if let Some(n) = self.known.get(&key) {
            return n.clone();
        }
        let short = self.name(at);
        let crate_of = self
            .from_address
            .and_then(|(from, process)| {
                const NAME: usize = 84;
                const ROOM: usize = 512;
                let mut info = [0u64; (NAME + ROOM) / 8 + 1];
                let p = info.as_mut_ptr().cast::<u8>();
                let mut off = 0u64;
                // SAFETY: as in `name`.
                unsafe {
                    p.cast::<u32>().write(88);
                    p.add(80).cast::<u32>().write(ROOM as u32);
                    if from(process, at as u64, &mut off, p) == 0 {
                        return None;
                    }
                    Some(std::ffi::CStr::from_ptr(p.add(NAME).cast()).to_string_lossy().into_owned())
                }
            })
            .unwrap_or(short);
        self.known.insert(key, crate_of.clone());
        crate_of
    }
}

/// What was summed since the last report.
#[derive(Default)]
struct Sums {
    /// ms by (group, kind, entry, stage key = innermost function of ours of the sample).
    by: HashMap<(usize, Kind, usize, usize), f64>,
    /// A sample of each key kept, for its stack.
    stacks: HashMap<(usize, Kind, usize, usize), Sample>,
    /// The title's threads: ms by thread, (title, busy, wait, fault).
    title_threads: HashMap<u32, [f64; 4]>,
    /// While a title thread waited in a present: where the submitting thread was (its key).
    at_present: HashMap<(Kind, usize), f64>,
    present_ms: f64,
}

/// Threads of the runtime, the driver or NVIDIA's libraries (D3D12's background threads,
/// Streamline's frame generation and logging threads): never stopped to be sampled, only
/// counted by their CPU time. Frame generation paces presents on its threads, and the
/// driver's threads take part in what the card is fed: stopping them, even for tens of
/// microseconds at a few hundred hertz, is a risk to the frames shown and to the device.
fn foreign(group: &str) -> bool {
    let g = group.to_ascii_lowercase();
    g.starts_with("d3d") || g.starts_with("nv") || g.starts_with("sl.") || g.starts_with("dxgi") || g.contains("(not ours)")
}

fn run() {
    let period = Duration::from_micros(1_000_000 / hz());
    let me = unsafe { GetCurrentThreadId() };
    let pid = unsafe { GetCurrentProcessId() };
    let mut threads: HashMap<u32, Thread> = HashMap::new();
    let mut groups: Vec<String> = Vec::new();
    let mut modules = Modules::read();
    let mut read_at = Instant::now();
    refresh_threads(pid, me, &modules, &mut threads, &mut groups);
    let mut names = Names::new();
    let mut sums = Sums::default();
    let mut last = Instant::now();
    let mut handles: Vec<(u32, HANDLE, (usize, usize), bool, usize)> = Vec::new();
    let mut round: Vec<(u32, bool, usize, Sample)> = Vec::new();
    // (the submitting thread's group, once named)
    let mut submit_group = usize::MAX;
    let mut refresh = false;
    let mut handles_ready = false;
    loop {
        if refresh || !handles_ready || read_at.elapsed() >= Duration::from_secs(1) {
            handles_ready = true;
            modules = Modules::read();
            read_at = Instant::now();
            refresh_threads(pid, me, &modules, &mut threads, &mut groups);
            submit_group = groups.iter().position(|g| g == "victory-submit").unwrap_or(usize::MAX);
            handles.clear();
            handles.extend(threads.iter().map(|(&id, t)| (id, t.handle, t.stack, t.title, t.group)));
            refresh = false;
        }
        let now = Instant::now();
        let weight = now.duration_since(last).as_secs_f64().min(0.05) * 1e3;
        last = now;
        round.clear();
        for &(id, h, stack, title, group) in &handles {
            // (unnamed threads that are not the title's — the driver's, the system's — are
            // only counted by their CPU time)
            if !title && (groups[group] == "(unnamed)" || foreign(&groups[group])) {
                continue;
            }
            // SAFETY: a thread of ours other than this one, opened with those rights.
            if let Some(s) = unsafe { sample(h, stack, &modules) } {
                round.push((id, title, group, s));
            }
        }
        // Sums (the threads go on now).
        let mut presenting = false;
        let mut submit_key = None;
        for &(id, title, group, s) in &round {
            let key = (group, s.kind, s.entry, s.ours[0]);
            *sums.by.entry(key).or_insert(0.0) += weight;
            sums.stacks.entry(key).or_insert(s);
            if title {
                let t = sums.title_threads.entry(id).or_insert([0.0; 4]);
                let i = match s.kind {
                    Kind::Title => 0,
                    Kind::Wait => 2,
                    Kind::Fault => 3,
                    _ => 1,
                };
                t[i] += weight;
                if s.kind == Kind::Wait && s.entry != 0 && names.name(s.entry).to_ascii_lowercase().contains("present") {
                    presenting = true;
                }
            }
            if group == submit_group {
                submit_key = Some((s.kind, s.ours.iter().copied().find(|&f| f != 0).unwrap_or(0), s));
            }
        }
        if presenting {
            sums.present_ms += weight;
            if let Some((kind, _, s)) = submit_key {
                // (keyed by the innermost function of ours, named at the report)
                let at = s.ours[0];
                *sums.at_present.entry((kind, at)).or_insert(0.0) += weight;
                sums.stacks.entry((submit_group, kind, 0, at)).or_insert(s);
            }
        }
        if ASKED.swap(false, Ordering::Acquire) {
            let frames = f64::from_bits(FRAMES.load(Ordering::Relaxed)).max(1.0);
            refresh_threads(pid, me, &modules, &mut threads, &mut groups);
            report(&mut sums, &mut threads, &groups, &mut names, frames, submit_group);
            sums = Sums::default();
            refresh = true;
        }
        let spent = now.elapsed();
        if spent < period {
            std::thread::sleep(period - spent);
        }
    }
}

/// The process's threads now: new ones opened and named, gone ones closed.
fn refresh_threads(pid: u32, me: u32, m: &Modules, threads: &mut HashMap<u32, Thread>, groups: &mut Vec<String>) {
    let mut alive = Vec::new();
    // SAFETY: plain queries of our own process.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap.is_null() || snap as isize == -1 {
            return;
        }
        let mut e: THREADENTRY32 = std::mem::zeroed();
        e.dwSize = size_of::<THREADENTRY32>() as u32;
        let mut more = Thread32First(snap, &mut e) != 0;
        while more {
            if e.th32OwnerProcessID == pid && e.th32ThreadID != me {
                alive.push(e.th32ThreadID);
            }
            more = Thread32Next(snap, &mut e) != 0;
        }
        CloseHandle(snap);
    }
    threads.retain(|id, t| {
        let keep = alive.contains(id);
        if !keep {
            // SAFETY: our own handle.
            unsafe { CloseHandle(t.handle) };
        }
        keep
    });
    for id in alive {
        if threads.contains_key(&id) {
            continue;
        }
        // SAFETY: a thread of our process.
        let h = unsafe { OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_LIMITED_INFORMATION, 0, id) };
        if h.is_null() {
            continue;
        }
        let title = m.in_title(start_of(h));
        let g = if title { "title".to_owned() } else { group_of(&name_of(h)) };
        let group = match groups.iter().position(|x| *x == g) {
            Some(i) => i,
            None => {
                groups.push(g);
                groups.len() - 1
            }
        };
        let stack = stack_of(h);
        if stack.1 <= stack.0 {
            // SAFETY: our own handle.
            unsafe { CloseHandle(h) };
            continue;
        }
        threads.insert(id, Thread { handle: h, stack, title, group, cpu: cpu_time(h) });
    }
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Title => "title code",
        Kind::Busy => "busy",
        Kind::Wait => "waiting",
        Kind::Fault => "fault handling",
        Kind::Other => "unknown",
    }
}

fn report(sums: &mut Sums, threads: &mut HashMap<u32, Thread>, groups: &[String], names: &mut Names, frames: f64, submit_group: usize) {
    let per = |ms: f64| ms / frames;
    let title_group = groups.iter().position(|g| g == "title");
    // The title's threads.
    let mut split = [0.0f64; 4];
    for t in sums.title_threads.values() {
        for i in 0..4 {
            split[i] += t[i];
        }
    }
    let mut busiest: Vec<(u32, [f64; 4])> = sums.title_threads.iter().map(|(&id, &t)| (id, t)).collect();
    busiest.sort_by(|a, b| (b.1[0] + b.1[1] + b.1[3]).total_cmp(&(a.1[0] + a.1[1] + a.1[3])));
    let floor = busiest.first().map_or(0.0, |b| per(b.1[0]));
    let tops: Vec<String> = busiest
        .iter()
        .take(3)
        .map(|(id, t)| format!("thread {id}: title {:.1}, ours {:.1}, faults {:.1}, waiting {:.1}", per(t[0]), per(t[1]), per(t[3]), per(t[2])))
        .collect();
    tracing::info!(
        "cpu stages, the title's threads per frame (VICTORY_CPU_STAGES): title code {:.1} ms, in ours busy {:.1} ms, fault handling {:.1} ms, waiting {:.1} ms; the title's floor (its busiest thread's own code) {floor:.1} ms; busiest: {}",
        per(split[0]),
        per(split[1]),
        per(split[3]),
        per(split[2]),
        tops.join("; ")
    );
    if let Some(tg) = title_group {
        // What of ours the title called: busy and waiting, by the function it called.
        for (kind, label) in [(Kind::Busy, "busy in"), (Kind::Wait, "waiting in"), (Kind::Fault, "faults under")] {
            let mut by: HashMap<usize, f64> = HashMap::new();
            for (&(g, k, entry, _), &ms) in &sums.by {
                if g == tg && k == kind {
                    *by.entry(entry).or_insert(0.0) += ms;
                }
            }
            let mut v: Vec<(usize, f64)> = by.into_iter().collect();
            v.sort_by(|a, b| b.1.total_cmp(&a.1));
            let list: Vec<String> = v.iter().take(15).filter(|x| per(x.1) >= 0.05).map(|(e, ms)| format!("{} {:.2}", names.name(*e), per(*ms))).collect();
            if !list.is_empty() {
                tracing::info!("cpu stages, the title's threads {label} ours, ms a frame: {}", list.join(", "));
            }
        }
    }
    // Our threads: CPU time by group, and where they were.
    let mut cpu: HashMap<usize, (f64, usize)> = HashMap::new();
    for t in threads.values_mut() {
        let now = cpu_time(t.handle);
        let d = now.saturating_sub(t.cpu) as f64 / 1e4;
        t.cpu = now;
        let e = cpu.entry(t.group).or_insert((0.0, 0));
        e.0 += d;
        e.1 += 1;
    }
    let mut order: Vec<(usize, f64, usize)> = cpu.iter().map(|(&g, &(ms, n))| (g, ms, n)).collect();
    order.sort_by(|a, b| b.1.total_cmp(&a.1));
    let cpu_list: Vec<String> = order
        .iter()
        .filter(|x| per(x.1) >= 0.1)
        .map(|(g, ms, n)| format!("{} {:.1}{}", groups[*g], per(*ms), if *n > 1 { format!(" ({n} threads)") } else { String::new() }))
        .collect();
    tracing::info!("cpu stages, CPU time per frame by thread (ms, the kernel's count): {}", cpu_list.join(", "));
    for &(g, ms, _) in order.iter().take(14) {
        if Some(g) == title_group || groups[g] == "(unnamed)" || per(ms) < 0.5 {
            continue;
        }
        let mut by: HashMap<(Kind, String), f64> = HashMap::new();
        for (key, &w) in &sums.by {
            if key.0 != g {
                continue;
            }
            let s = sums.stacks[key];
            *by.entry((key.1, stage(names, &s))).or_insert(0.0) += w;
        }
        let mut v: Vec<((Kind, String), f64)> = by.into_iter().collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1));
        let list: Vec<String> =
            v.iter().take(8).filter(|x| per(x.1) >= 0.1).map(|((k, n), w)| format!("{n} {} {:.1}", kind_name(*k), per(*w))).collect();
        if !list.is_empty() {
            tracing::info!("cpu stages, {} per frame (ms, sampled): {}", groups[g], list.join(", "));
        }
    }
    // The critical path.
    if sums.present_ms > 0.0 && submit_group != usize::MAX {
        let mut by: HashMap<(Kind, String), f64> = HashMap::new();
        for (&(k, at), &w) in &sums.at_present {
            let s = sums.stacks.get(&(submit_group, k, 0, at)).copied();
            let n = match s {
                Some(s) => stage(names, &s),
                None => names.name(at),
            };
            *by.entry((k, n)).or_insert(0.0) += w;
        }
        let mut v: Vec<((Kind, String), f64)> = by.into_iter().collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1));
        let list: Vec<String> = v.iter().take(6).map(|((k, n), w)| format!("{n} {} {:.1}", kind_name(*k), per(*w))).collect();
        tracing::info!(
            "cpu stages, critical path: the title's threads waited in a present of ours {:.1} ms a frame; the submitting thread meanwhile: {}",
            per(sums.present_ms),
            list.join(", ")
        );
    }
}
