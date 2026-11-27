//! A memory bench for the GPU side, without the game: synthetic frames go
//! through the title's own calls, and what each class of the layer holds is
//! read after a warm-up and at the end, with the process's private bytes and
//! the card's memory as the system counts it. It is to memory changes what
//! the differential shader oracle is to the translator: a change that makes
//! a class grow frame over frame, or keeps what pressure should let go,
//! shows in its table (and fails its checks).
//!
//! The title's side is the real one, called through the objects' tables as
//! the title calls them: two queues and a fence, command allocators and
//! lists, committed buffers in write-watched memory from a title allocator
//! (`crate::resource::use_allocator`, as the runtime's xmem), buffer copies,
//! submissions, a queue that waits for the other's fence, and the queue side
//! of a present. A frame `f`:
//!
//! - the title's CPU writes words of the source buffer and of memory outside
//!   buffers (a heap of eight 4 MiB windows, one title buffer placed in it);
//! - six graphics lists, each: copies source to destination (a buffer whose
//!   host copy a view made, so the copy is done on the card out of the list's
//!   scratch, `HostList::place_copy`, and declared as written), a copy into a
//!   32 MiB buffer whose host copy is in the card's memory (compared and
//!   fetched back), and a draw's read of the heap through a root view (the
//!   steps of `draw::root_view`: `shadow::find`, `Shadow::refresh`), in two
//!   submissions;
//! - two lists on a compute queue that first waits for the graphics queue's
//!   fence: the submission is held, its host lists are taken from the
//!   title's (`crate::list::detach_parts`), run when the graphics queue
//!   signals, and given to the pool (`crate::list::recycle`). With the relay
//!   on (`VICTORY_RELAY=2`, as runs have it) the graphics lists go the same
//!   way, as flights;
//! - the queue side of a present (settle, release what queues hold) and the
//!   frame count moves on.
//!
//! What it cannot reach without the game (or translated programs): draws and
//! dispatches, with what only they touch (descriptor heaps and their views,
//! textures and their host copies, staging and blocks, indirect arguments,
//! pipelines), the swap chain's copy, and the thin path (`VICTORY_THIN=1`).
//!
//! Each test wants a process of its own (it names the title allocator, and
//! the switches are read once):
//!
//! `cargo test -p victory-gpu --lib memory_bench_steady_state -- --ignored --nocapture`
//!
//! and the same with `VICTORY_RELAY=2 VICTORY_RELAY_COPY=1` for the relay's
//! paths (`scripts/play-foxhound.ps1` sets both). `VICTORY_MEMBENCH_STRICT=1`
//! also asserts that pressure lets go of what a spike made (the list pool,
//! spare scratch, fetch buffers).

use std::ffi::c_void;

use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
};

use super::*;
use crate::object::{HRESULT, IUNKNOWN, S_OK, This, slot};

const WRITE_WATCH: u32 = 0x20_0000;

/// The bytes of the relay's fetch buffer (`Host::relay_card_back`), which
/// lives on the relay's thread.
pub(super) static RELAY_FETCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const MB: u64 = 1 << 20;

/// The source of the copies (the title's CPU writes it), their destination
/// (with a host copy in the PC's memory), and a buffer big enough for its
/// copy to be in the card's memory (16 MiB and up on a card of 12 GB).
const SRC: usize = 8 << 20;
const DST: usize = 4 << 20;
const BIG: usize = 32 << 20;
/// A copy of a frame's list, and of its one copy into the big buffer.
const COPY: usize = 16 << 10;
const BIG_COPY: usize = 256 << 10;
const COPIES: usize = 8;
/// What a draw reads through a root view.
const READ: u64 = 64 << 10;
/// The windows' grid (`crate::shadow::WINDOW`) and the heap's windows.
const WINDOW: usize = 4 << 20;
const WINDOWS: usize = 8;
/// Where draws read in a window: past the buffer placed in window 0.
const READ_AT: usize = 0x28_0000;
/// The buffer placed in the heap (a placed resource's memory): its
/// resource key, where it lies in window 0, its bytes.
const INNER_KEY: usize = 0x6d65_6d62_656e_6368;
const INNER_AT: usize = 1 << 20;
const INNER_LEN: usize = 1 << 20;
const GRAPHICS_LISTS: usize = 6;
const COMPUTE_LISTS: usize = 2;
/// The spike: a loading screen's many long lists at once, and copies of
/// 8 MiB into the big buffer (all of it written, all of it fetched back).
const SPIKE_LISTS: usize = 64;
const SPIKE_COPIES: usize = 2000;
const SPIKE_BIG: usize = 8 << 20;

extern "system" fn title_alloc(size: usize, _attributes: u64) -> *mut c_void {
    // SAFETY: a fresh allocation of ours.
    unsafe {
        VirtualAlloc(
            std::ptr::null(),
            size,
            MEM_RESERVE | MEM_COMMIT | WRITE_WATCH,
            PAGE_READWRITE,
        )
    }
}

extern "system" fn title_free(at: *mut c_void, _attributes: u64) {
    // SAFETY: one of `title_alloc`'s.
    unsafe { VirtualFree(at, 0, MEM_RELEASE) };
}

fn release(object: This) -> u32 {
    slot::<extern "system" fn(This) -> u32>(object, 0x10)(object)
}

/// A title queue, called through its table (newer layouts have signal and
/// wait two slots later).
struct Queue {
    this: This,
    newer: bool,
}

impl Queue {
    fn new(kind: u32) -> Queue {
        // The title's description: type, priority, flags, node mask.
        let desc: [u32; 4] = [kind, 0, 0, 1];
        let mut this = std::ptr::null();
        assert_eq!(
            crate::queue::create(std::ptr::null(), desc.as_ptr().cast(), &IUNKNOWN, &mut this),
            S_OK
        );
        Queue {
            this,
            newer: crate::device::newer(),
        }
    }

    fn execute(&self, lists: &[This]) {
        slot::<extern "system" fn(This, u32, *const This)>(self.this, 0x50)(
            self.this,
            lists.len() as u32,
            lists.as_ptr(),
        );
    }

    fn signal(&self, fence: This, value: u64) {
        let at = if self.newer { 0x80 } else { 0x70 };
        assert_eq!(
            slot::<extern "system" fn(This, This, u64) -> HRESULT>(self.this, at)(
                self.this, fence, value
            ),
            S_OK
        );
    }

    fn wait(&self, fence: This, value: u64) {
        let at = if self.newer { 0x88 } else { 0x78 };
        assert_eq!(
            slot::<extern "system" fn(This, This, u64) -> HRESULT>(self.this, at)(
                self.this, fence, value
            ),
            S_OK
        );
    }
}

/// A title list with an allocator of its own, open after it is made.
struct List {
    this: This,
    allocator: This,
    open: bool,
}

impl List {
    fn new(kind: u32) -> List {
        let null = std::ptr::null();
        let (mut allocator, mut this) = (null, null);
        assert_eq!(
            crate::list::create_allocator(null, kind, &IUNKNOWN, &mut allocator),
            S_OK
        );
        assert_eq!(
            crate::list::create_list(null, 0, kind, allocator, null, &IUNKNOWN, &mut this),
            S_OK
        );
        List {
            this,
            allocator,
            open: true,
        }
    }

    /// The title's reset, before it records the list again.
    fn begin(&mut self) {
        if !self.open {
            let reset = slot::<extern "system" fn(This, This, This) -> HRESULT>(self.this, 0x50);
            assert_eq!(reset(self.this, self.allocator, std::ptr::null()), S_OK);
            self.open = true;
        }
    }

    fn copy(&self, dst: This, dst_offset: usize, src: This, src_offset: usize, len: usize) {
        let copy = slot::<extern "system" fn(This, This, u64, This, u64, u64)>(self.this, 0x78);
        copy(
            self.this,
            dst,
            dst_offset as u64,
            src,
            src_offset as u64,
            len as u64,
        );
    }

    /// A draw's read of title memory through a root view, as
    /// `draw::root_view` does it for a view without a length.
    fn draw_reads(&self, address: usize) {
        crate::list::with_state(self.this, |list, _| {
            let shadow =
                crate::shadow::find(address as u64, 1).expect("a host copy of title memory");
            shadow.refresh(list, address as u64, READ.min(shadow.left(address as u64)));
        });
    }

    fn end(&mut self) {
        assert_eq!(
            slot::<extern "system" fn(This) -> HRESULT>(self.this, 0x48)(self.this),
            S_OK
        );
        self.open = false;
    }
}

impl Drop for List {
    fn drop(&mut self) {
        release(self.this);
        release(self.allocator);
    }
}

/// The title of the bench: its queues, fence, lists and memory.
struct Rig {
    graphics: Queue,
    compute: Queue,
    fence: This,
    drawn: Vec<List>,
    computed: Vec<List>,
    /// Title buffers: the resource and the address of its memory.
    src: (This, usize),
    dst: (This, usize),
    big: (This, usize),
    /// The heap's first window (the reservation is kept for the process:
    /// host copies of its windows outlive the rig).
    heap: usize,
    frame: u64,
}

impl Rig {
    /// `None` without a host GPU.
    fn new() -> Option<Rig> {
        host()?;
        crate::resource::use_allocator(title_alloc, title_free);
        let mut fence = std::ptr::null();
        assert_eq!(
            crate::fence::create(
                std::ptr::null(),
                0,
                0,
                &crate::fence::FENCE_IIDS[0],
                &mut fence
            ),
            S_OK
        );
        let buffer = |size: usize| {
            let resource = crate::resource::test_buffer(size as u64);
            let (at, len) = crate::resource::buffer_bytes(resource).expect("a buffer");
            assert_eq!(len, size as u64);
            (resource, at as usize)
        };
        let (src, dst, big) = (buffer(SRC), buffer(DST), buffer(BIG));
        // Host copies made by views, as a draw's would.
        crate::shadow::find(dst.1 as u64, 1).expect("the destination's host copy");
        let carded = crate::shadow::find(big.1 as u64, 1)
            .expect("the big buffer's host copy")
            .carded();
        // SAFETY: a reservation of ours, never freed.
        let base = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                WINDOWS * WINDOW + WINDOW,
                MEM_RESERVE | MEM_COMMIT | WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(base, 0);
        let heap = base.next_multiple_of(WINDOW);
        crate::shadow::register(INNER_KEY, (heap + INNER_AT) as u64, INNER_LEN as u64);
        crate::shadow::find((heap + INNER_AT) as u64, 1).expect("the placed buffer's host copy");
        println!(
            "memory bench: relay {}, flights (late) {}, the present settles {}, the big buffer's copy on the card {carded}",
            relay(),
            late(),
            !late_free_present()
        );
        Some(Rig {
            graphics: Queue::new(0),
            compute: Queue::new(2),
            fence,
            drawn: (0..GRAPHICS_LISTS).map(|_| List::new(0)).collect(),
            computed: (0..COMPUTE_LISTS).map(|_| List::new(2)).collect(),
            src,
            dst,
            big,
            heap,
            frame: 0,
        })
    }

    /// Where a draw of the frame reads in window `w`.
    fn read_at(&self, w: usize) -> usize {
        self.heap + w * WINDOW + READ_AT + (self.frame as usize % 16) * READ as usize
    }

    /// The title's CPU: new words in the source, a word in each window read.
    fn title_writes(&self, windows: usize) {
        let f = self.frame as usize;
        // SAFETY: the rig's own title memory, inside the buffers and heap.
        unsafe {
            let src = self.src.1 as *mut u64;
            for k in 0..64 {
                src.add((f * 977 + k * 4111) % (SRC / 8))
                    .write_volatile((f << 16 | k) as u64);
            }
            for w in 0..windows {
                (self.read_at(w) as *mut u64)
                    .add(f % 512)
                    .write_volatile(f as u64);
            }
        }
    }

    /// One frame; its draws read the first `windows` windows of the heap.
    fn frame(&mut self, windows: usize) {
        let f = self.frame as usize;
        self.title_writes(windows);
        let (src, dst, big) = (self.src.0, self.dst.0, self.big.0);
        let reads: Vec<usize> = (0..GRAPHICS_LISTS + COMPUTE_LISTS)
            .map(|i| self.read_at(i % windows))
            .collect();
        for (i, list) in self.drawn.iter_mut().enumerate() {
            list.begin();
            for k in 0..COPIES {
                let to = (f * 31 + i * COPIES + k) % (DST / COPY) * COPY;
                let from = (f * 17 + k * 7 + i) % (SRC / COPY) * COPY;
                list.copy(dst, to, src, from, COPY);
            }
            list.copy(
                big,
                (f * GRAPHICS_LISTS + i) % (BIG / BIG_COPY) * BIG_COPY,
                src,
                i * BIG_COPY,
                BIG_COPY,
            );
            list.draw_reads(reads[i]);
            list.draw_reads(self.dst.1 + (f + i) % (DST / COPY) * COPY);
            list.end();
        }
        let drawn: Vec<This> = self.drawn.iter().map(|l| l.this).collect();
        let half = GRAPHICS_LISTS / 2;
        self.graphics.execute(&drawn[..half]);
        self.graphics.execute(&drawn[half..]);
        let value = self.frame + 1;
        self.compute.wait(self.fence, value);
        for (j, list) in self.computed.iter_mut().enumerate() {
            list.begin();
            for k in 0..COPIES / 2 {
                let to = (f * 13 + j * COPIES + k + DST / COPY / 2) % (DST / COPY) * COPY;
                list.copy(dst, to, src, (f + k) % (SRC / COPY) * COPY, COPY);
            }
            list.draw_reads(reads[GRAPHICS_LISTS + j]);
            list.end();
        }
        let computed: Vec<This> = self.computed.iter().map(|l| l.this).collect();
        self.compute.execute(&computed);
        self.graphics.signal(self.fence, value);
        present();
        self.frame += 1;
    }

    /// A loading screen's spike: many long lists in one held submission,
    /// and the big buffer written and fetched back whole. The title lets
    /// go of those lists afterwards.
    fn spike(&mut self) {
        let value = self.frame + 1;
        let (src, dst, big) = (self.src.0, self.dst.0, self.big.0);
        let mut heavy = List::new(0);
        for k in 0..BIG / SPIKE_BIG {
            heavy.copy(big, k * SPIKE_BIG, src, 0, SPIKE_BIG);
        }
        heavy.end();
        self.compute.wait(self.fence, value);
        let mut lists: Vec<List> = (0..SPIKE_LISTS).map(|_| List::new(2)).collect();
        for (i, list) in lists.iter_mut().enumerate() {
            for k in 0..SPIKE_COPIES {
                list.copy(dst, (k * 4096 + i * 256) % DST, src, (k * 512) % SRC, 256);
            }
            list.end();
        }
        self.graphics.execute(&[heavy.this]);
        self.compute
            .execute(&lists.iter().map(|l| l.this).collect::<Vec<_>>());
        self.graphics.signal(self.fence, value);
        present();
        self.frame += 1;
        quiesce();
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        quiesce();
        self.drawn.clear();
        self.computed.clear();
        crate::shadow::unregister(INNER_KEY, (self.heap + INNER_AT) as u64);
        for object in [
            self.src.0,
            self.dst.0,
            self.big.0,
            self.graphics.this,
            self.compute.this,
            self.fence,
        ] {
            release(object);
        }
    }
}

/// The queue side of the title's present (`crate::queue::present`): what
/// was sent ahead is settled unless the present goes ahead too, and what
/// queues hold runs; then the frame count moves on.
fn present() {
    if late() && !late_free_present() {
        settle_for_present();
        crate::queue::release_held(false);
        settle_for_present();
    } else if late() {
        crate::queue::release_held(false);
    }
    crate::queue::release_held(true);
    crate::frame::test_advance();
}

/// Nothing in the air and nothing held: for a census.
fn quiesce() {
    settle_now(WHY_SYNC);
    crate::queue::release_held(true);
    settle_now(WHY_SYNC);
}

#[derive(Clone, Copy, PartialEq)]
enum Unit {
    Count,
    Bytes,
}

/// How far an item may move between two censuses of a steady state.
#[derive(Clone, Copy)]
enum Flat {
    /// Not looked at (the working set, uses counted since the start).
    Any,
    /// Up to this many of the item's unit.
    Within(u64),
}

#[derive(Clone)]
struct Item {
    class: &'static str,
    what: &'static str,
    unit: Unit,
    flat: Flat,
    value: u64,
}

/// What the layer holds now, by class, with the process and the card.
#[rustfmt::skip]
fn census() -> Vec<Item> {
    use Flat::{Any, Within};
    use Unit::{Bytes, Count};
    use std::sync::atomic::Ordering::Relaxed;
    let mut items = Vec::new();
    let mut add = |class, what, unit, flat, value| {
        items.push(Item { class, what, unit, flat, value });
    };
    let (working, private) = process_memory();
    add("process", "private bytes", Bytes, Within(16 * MB), private);
    add("process", "working set", Bytes, Any, working);
    let [own, shared] = host().and_then(|h| h.video_memory_groups()).unwrap_or_default();
    add("card", "its own memory in use", Bytes, Within(16 * MB), own[1]);
    add("card", "PC memory shared with it in use", Bytes, Within(16 * MB), shared[1]);
    let alive = ALIVE.each_ref().map(|a| a.load(Relaxed));
    let lists = LISTS.each_ref().map(|l| l.load(Relaxed));
    let made = MADE.each_ref().map(|m| m.load(Relaxed));
    let (idle, idle_bytes) = crate::list::idle();
    let (landed, landed_scratch) = {
        let line = line();
        let lists = line.landed.iter().flat_map(|(l, _)| l.iter());
        let scratch = lists.clone().map(|l| l.host().scratch_kept()).sum::<u64>();
        (lists.count() as u64, scratch)
    };
    add("host lists", "alive", Count, Within(16), alive[0]);
    add("host lists", "idle in the pool", Count, Within(16), idle as u64);
    add("host lists", "scratch the idle ones keep", Bytes, Within(8 * MB), idle_bytes);
    add("host lists", "landed, waiting for the bury lag", Count, Within(64), landed);
    add("host lists", "scratch the landed ones keep", Bytes, Within(32 * MB), landed_scratch);
    add("host lists", "made since the start", Count, Within(8), lists[0]);
    add("host lists", "preludes made", Count, Within(8), lists[1]);
    add("host lists", "never reset", Count, Any, lists[3]);
    add("list scratch", "chunks alive", Count, Within(16), alive[1]);
    add("list scratch", "their bytes", Bytes, Within(32 * MB), alive[2]);
    add("list scratch", "spare chunks", Count, Within(16), alive[3]);
    add("list scratch", "spare bytes", Bytes, Within(32 * MB), alive[4]);
    add("list scratch", "taken from the spare ones", Count, Any, alive[5]);
    add("list uploads", "alive", Count, Within(16), alive[6]);
    add("list uploads", "their bytes", Bytes, Within(32 * MB), alive[7]);
    let copies = crate::shadow::memory();
    let on_card = crate::shadow::live_span_bytes();
    add("host copies", "alive", Count, Within(4), copies[0]);
    add("host copies", "title bytes they copy", Bytes, Within(8 * MB), copies[1]);
    add("host copies", "on the card (copies, windows)", Bytes, Within(8 * MB), on_card);
    add("windows", "alive", Count, Within(2), copies[2]);
    add("windows", "their bytes", Bytes, Within(8 * MB), copies[3]);
    add("windows", "bytes over title buffers", Bytes, Within(8 * MB), windows_over_buffers());
    add("windows", "let go since the start", Count, Any, crate::shadow::windows()[0]);
    add("snapshots", "kept for reuse", Count, Within(16), copies[4]);
    add("snapshots", "bytes they reserve", Bytes, Within(32 * MB), copies[5]);
    let (fetch, preludes) = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map_or(
        (0, 0),
        |w| (w.fetched.as_ref().map_or(0, |f| f.2), (w.sync.len() + w.check.len()) as u64),
    );
    add("card copies", "fetch buffer (no relay)", Bytes, Within(0), fetch);
    add("card copies", "fetch buffer (relay)", Bytes, Within(0), RELAY_FETCH.load(Relaxed));
    add("card copies", "send and compare lists kept", Count, Within(4), preludes);
    let staged = crate::texture::memory();
    let blocks = crate::pool::memory();
    add("staging", "alive", Count, Within(4), staged[0]);
    add("staging", "their bytes", Bytes, Within(8 * MB), staged[1]);
    add("staging", "let go since the start", Count, Any, crate::texture::let_go()[0]);
    add("blocks", "of textures", Bytes, Within(0), blocks[0] * MB);
    add("blocks", "of staging", Bytes, Within(0), blocks[1] * MB);
    let textures = crate::resource::HOST_TEXTURES[0].load(Relaxed);
    add("host textures", "alive", Count, Within(0), textures);
    let retired = RETIRED.lock().unwrap_or_else(|e| e.into_inner()).len();
    add("retired", "waiting for the bury lag", Count, Within(256), retired as u64);
    // Made since the start: a steady state makes (next to) none.
    add("made", "upload buffers", Count, Within(8), made[0]);
    add("made", "upload bytes", Bytes, Any, made[1]);
    add("made", "shared buffers", Count, Within(8), made[2]);
    add("made", "shared bytes", Bytes, Any, made[3]);
    add("made", "card buffers", Count, Within(8), made[4]);
    add("made", "card bytes", Bytes, Any, made[5]);
    items
}

/// The process's working set and private bytes (committed), from Windows.
fn process_memory() -> (u64, u64) {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) } == 0 {
        return (0, 0);
    }
    (c.WorkingSetSize as u64, c.PagefileUsage as u64)
}

/// The bytes of windows that lie over title buffers (memory that a buffer's
/// host copy may copy as well).
fn windows_over_buffers() -> u64 {
    let spans = crate::shadow::spans();
    let (windows, buffers): (Vec<_>, Vec<_>) = spans.iter().partition(|s| s.2);
    windows
        .iter()
        .map(|&&(at, len, _)| {
            buffers
                .iter()
                .map(|&&(b, n, _)| (at + len).min(b + n).saturating_sub(at.max(b)))
                .sum::<u64>()
        })
        .sum()
}

fn shown(unit: Unit, value: i128) -> String {
    match unit {
        Unit::Count => format!("{value}"),
        Unit::Bytes => format!("{:.1} MB", value as f64 / MB as f64),
    }
}

/// The censuses side by side, and what changed from the first to the last
/// (per frame over `frames`, if given).
fn table(heading: &str, columns: &[(&str, &[Item])], frames: Option<u64>) {
    println!("\n{heading}");
    let mut line = format!("{:<14} {:<34}", "class", "item");
    for (name, _) in columns {
        line += &format!(" {name:>14}");
    }
    line += &format!(" {:>12}", "change");
    if frames.is_some() {
        line += &format!(" {:>12}", "per frame");
    }
    println!("{line}");
    let (first, last) = (columns[0].1, columns[columns.len() - 1].1);
    for (i, item) in first.iter().enumerate() {
        let mut line = format!("{:<14} {:<34}", item.class, item.what);
        for (_, census) in columns {
            line += &format!(" {:>14}", shown(item.unit, i128::from(census[i].value)));
        }
        let change = i128::from(last[i].value) - i128::from(item.value);
        let sign = if change > 0 { "+" } else { "" };
        line += &format!(
            " {:>12}",
            if change == 0 {
                "-".to_owned()
            } else {
                format!("{sign}{}", shown(item.unit, change))
            }
        );
        if let Some(frames) = frames {
            let per = change as f64 / frames.max(1) as f64;
            line += &format!(
                " {:>12}",
                match item.unit {
                    Unit::Count => format!("{per:.2}"),
                    Unit::Bytes => format!("{:.1} KB", per / 1024.0),
                }
            );
        }
        println!("{line}");
    }
}

/// The items that moved more than they may in a steady state.
fn grown(before: &[Item], after: &[Item]) -> Vec<String> {
    before
        .iter()
        .zip(after)
        .filter_map(|(a, b)| match a.flat {
            Flat::Within(most) if a.value.abs_diff(b.value) > most => Some(format!(
                "{} {}: {} to {}",
                a.class,
                a.what,
                shown(a.unit, i128::from(a.value)),
                shown(a.unit, i128::from(b.value))
            )),
            _ => None,
        })
        .collect()
}

fn value(census: &[Item], class: &str, what: &str) -> u64 {
    census
        .iter()
        .find(|i| i.class == class && i.what == what)
        .map_or(0, |i| i.value)
}

/// Frames run, each at least `least` long (a title's pace, so that the
/// once-a-second sweep comes by).
fn frames(rig: &mut Rig, n: u64, windows: usize, least: std::time::Duration) {
    for _ in 0..n {
        let started = std::time::Instant::now();
        rig.frame(windows);
        if let Some(rest) = least.checked_sub(started.elapsed()) {
            std::thread::sleep(rest);
        }
    }
}

/// Frames until `ms` milliseconds went by, at 60 a second.
fn frames_for(rig: &mut Rig, ms: u64, windows: usize) -> u64 {
    let started = std::time::Instant::now();
    let mut n = 0;
    while started.elapsed().as_millis() < u128::from(ms) || n < 8 {
        frames(rig, 1, windows, std::time::Duration::from_millis(16));
        n += 1;
    }
    n
}

/// The steady state: after a warm-up, frame after frame of the same work
/// holds the same memory in every class.
#[test]
#[ignore = "on the card; a process of its own: cargo test -p victory-gpu --lib memory_bench_steady_state -- --ignored --nocapture"]
fn memory_bench_steady_state() {
    const WARM: u64 = 60;
    const MEASURED: u64 = 240;
    let Some(mut rig) = Rig::new() else {
        eprintln!("no host GPU: nothing measured");
        return;
    };
    let pace = std::time::Duration::from_millis(8);
    let started = std::time::Instant::now();
    frames(&mut rig, WARM, WINDOWS, pace);
    quiesce();
    let warm = census();
    let measured = std::time::Instant::now();
    frames(&mut rig, MEASURED, WINDOWS, pace);
    quiesce();
    let end = census();
    println!(
        "memory bench, steady state: {WARM} frames of warm-up, {MEASURED} measured in {:.1} s ({:.1} s in all)",
        measured.elapsed().as_secs_f64(),
        started.elapsed().as_secs_f64()
    );
    table(
        "what each class holds",
        &[("after warm-up", &warm), ("at the end", &end)],
        Some(MEASURED),
    );
    let grown = grown(&warm, &end);
    drop(rig);
    assert!(
        grown.is_empty(),
        "memory grew in the steady state: {grown:#?}"
    );
    // Lists whose submission has landed give their scratch chunks back then, not at their
    // reset after the bury lag (`HostList::release_scratch`; `VICTORY_SCRATCH_AT_LANDING=0`: not).
    if super::scratch_at_landing() {
        let kept = value(&end, "host lists", "scratch the landed ones keep");
        assert_eq!(kept, 0, "landed lists keep {kept} bytes of scratch");
    }
}

/// Pressure: what idles while the PC has room is kept (five minutes with
/// 12 GiB free, ten seconds on a PC of 32 GB or less); told that only 1 GiB
/// is free (`FREE_RAM_SAID`, which `crate::residency` hears too), the same
/// idle memory is let go within a few seconds. Then what pressure does not
/// reach is listed (and with `VICTORY_MEMBENCH_STRICT=1` asserted to come
/// down).
#[test]
#[ignore = "on the card; a process of its own: cargo test -p victory-gpu --lib memory_bench_pressure_lets_idle_go -- --ignored --nocapture"]
fn memory_bench_pressure_lets_idle_go() {
    use std::sync::atomic::Ordering::Relaxed;
    const WARM: u64 = 40;
    /// Longer than the idle time under pressure (2 s) and the sweep's second.
    const IDLE_MS: u64 = 3_500;
    let Some(mut rig) = Rig::new() else {
        eprintln!("no host GPU: nothing measured");
        return;
    };
    let pace = std::time::Duration::from_millis(16);
    frames(&mut rig, WARM, WINDOWS, pace);
    quiesce();
    let warm = census();
    rig.spike();
    // Until the spike's lists are past the bury lag and in the pool.
    frames(&mut rig, 32, WINDOWS, pace);
    quiesce();
    let spiked = census();
    // Idle: only window 0 is read from here on, with the PC's own free RAM.
    let free = available_physical_ram().unwrap_or(0);
    let calm = frames_for(&mut rig, IDLE_MS, 1);
    quiesce();
    let roomy = census();
    // The same, with 1 GiB free as far as the layer can tell.
    FREE_RAM_SAID.store((1u64 << 30) + 1, Relaxed);
    let pressed = frames_for(&mut rig, IDLE_MS, 1);
    quiesce();
    let short = census();
    FREE_RAM_SAID.store(0, Relaxed);
    println!(
        "memory bench, pressure: {WARM} frames, the spike ({SPIKE_LISTS} lists of {SPIKE_COPIES} copies, {} MB fetched back), \
         32 frames; idle {calm} frames with {} MB free, {pressed} frames told 1024 MB",
        BIG >> 20,
        free >> 20
    );
    table(
        "what each class holds",
        &[
            ("before spike", &warm),
            ("after spike", &spiked),
            ("idle, roomy", &roomy),
            ("idle, short", &short),
        ],
        None,
    );
    let windows = |c: &[Item]| value(c, "windows", "alive");
    // Kept by free RAM: idle windows stay while 12 GiB are free.
    if free >= 13u64 << 30 {
        assert_eq!(
            windows(&roomy),
            windows(&spiked),
            "idle windows are kept while the PC has room"
        );
    } else {
        println!(
            "(only {} MB free: the retention by free RAM is not looked at)",
            free >> 20
        );
    }
    // Let go under pressure: all but the window still read.
    assert!(
        windows(&short) <= 1,
        "idle windows are let go under pressure: {} alive",
        windows(&short)
    );
    // What pressure does not reach.
    let kept: Vec<(&str, &str)> = [
        ("host lists", "idle in the pool"),
        ("list scratch", "spare bytes"),
        ("card copies", "fetch buffer (no relay)"),
        ("card copies", "fetch buffer (relay)"),
        ("process", "private bytes"),
    ]
    .into_iter()
    .filter(|&(class, what)| {
        value(&short, class, what) > value(&warm, class, what)
            && value(&short, class, what) >= value(&spiked, class, what)
    })
    .collect();
    for &(class, what) in &kept {
        println!(
            "kept under pressure since the spike: {class} {what}: {} before it, {} under pressure",
            value(&warm, class, what),
            value(&short, class, what)
        );
    }
    drop(rig);
    if std::env::var_os("VICTORY_MEMBENCH_STRICT").is_some_and(|v| v == "1") {
        assert!(
            kept.is_empty(),
            "pressure keeps what a spike made: {kept:?}"
        );
    }
}
