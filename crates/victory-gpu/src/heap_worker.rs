//! The pass that uploads textures picked by index, on a thread of its own
//! (`VICTORY_HEAP_WORKER=1`; off by default; needs `VICTORY_LATE_WAIT` /
//! `VICTORY_RELAY`).
//!
//! [`crate::draw::State::heap_textures`] runs on whichever thread of the
//! title records the frame's first shader that picks textures by index: it
//! asks what of each due texture is mapped, makes the host texture, makes a
//! staging buffer and records the copy, up to 250 ms of every frame while a
//! backlog waits, and the threads waiting for that list wait with it. The
//! detiling of what it recorded then runs on the thread that submits. With
//! the switch the pass runs here, and the recording thread only wakes it.
//!
//! What keeps the picture as it was:
//! - the uploads are recorded into host lists of ours ([`Batch`]), which the
//!   next submission of the title's runs ahead of its own lists
//!   ([`take_ready_for_late`]): texture states are settled by submission
//!   order, as between any two lists;
//! - a texture's host slot is given its view at that submission, not before
//!   ([`Publish`]), and always through a key-pinned publish that builds the
//!   descriptor from the recorded key rather than re-reading the title's
//!   slot. Under `VICTORY_SHOW_AFTER=1` or `VICTORY_SHOW_NEW_LATER=1`, the
//!   descriptor flip waits until the upload list's submission completes. A
//!   slot the title wrote again meanwhile is left to the write's own turn
//!   in the queue;
//! - the detiling done here is checked at submission as any texture's is:
//!   memory the title wrote in between is read again (except for `once`
//!   textures, which skip write-watching just as on the recording thread);
//! - a title texture the pass works on is held ([`crate::resource::hold`]),
//!   and so is the title's heap;
//! - the uploads before the title's decoration bake, which must be there
//!   for that draw, still run on the recording thread; the worker makes way
//!   for them ([`urgent`]), and any batches already flushed to [`READY`]
//!   are executed synchronously and published first ([`drain_for_bake`]).
// shortcut: what a submission shows for a texture is up to a frame later
// than when the recording thread did it all; with a backlog it is earlier,
// since the pass has the whole frame instead of a slice of it.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};

use crate::descriptor::Heap;
use crate::host::HostList;
use crate::object::This;

/// Whether the pass runs on its own thread now.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_WORKER").is_some_and(|v| v == "1"))
        && crate::host::late()
}

/// For the frame line, since the last one: nanoseconds the worker was at
/// work, of them detiling, lists given to submissions, textures published,
/// slots the title had written again, and textures in those lists whose
/// detiling was not done here.
pub(crate) static STATS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// One upload list holds at most this much texture memory, or this many
/// milliseconds of work, so that its textures show soon.
const LIST_BYTES: u64 = 128 << 20;
const LIST_MS: u128 = 80;

/// A view to give a host slot once its texture's upload is in the queue:
/// the title's heap, the slot, the record's handle, and the record's key as
/// it was when the upload was recorded.
pub(crate) struct Publish {
    heap: usize,
    index: u32,
    handle: u64,
    key: u64,
}

/// Holds a reference to a title heap.
struct HeldHeap(This);
// SAFETY: a reference counted title object, of which only the count and the
// parts behind locks are touched.
unsafe impl Send for HeldHeap {}

impl HeldHeap {
    fn new(heap: &Heap) -> Self {
        let this = std::ptr::from_ref(heap).cast();
        crate::object::add_ref(this);
        HeldHeap(this)
    }

    fn heap(&self) -> &Heap {
        crate::descriptor::heap(self.0)
    }
}

impl Drop for HeldHeap {
    fn drop(&mut self) {
        crate::object::release_boxed::<Heap>(self.0);
    }
}

/// A closed upload list (or `None` when a pass only found already-uploaded
/// textures), and what is published when it is submitted.
struct Ready {
    list: Option<HostList>,
    publish: Vec<Publish>,
    _heap: HeldHeap,
}
// SAFETY: D3D12 objects are free-threaded; the list is closed and touched
// by nobody until it is taken out.
unsafe impl Send for Ready {}

static READY: Mutex<Vec<Ready>> = Mutex::new(Vec::new());

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a pass on the worker's thread records into: one upload list after
/// the other, made when first needed.
pub(crate) struct Batch {
    heap: This,
    list: Option<HostList>,
    publish: Vec<Publish>,
    bytes: u64,
    began: std::time::Instant,
}

impl Batch {
    fn new(heap: &Heap) -> Self {
        Batch {
            heap: std::ptr::from_ref(heap).cast(),
            list: None,
            publish: Vec::new(),
            bytes: 0,
            began: std::time::Instant::now(),
        }
    }

    /// The open upload list.
    pub(crate) fn list(&mut self) -> Option<&HostList> {
        if self.list.is_none() {
            self.list = crate::list::fresh_list();
            self.began = std::time::Instant::now();
        }
        self.list.as_ref()
    }

    /// The currently open upload list, if one has been created.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn open_list(&self) -> Option<&HostList> {
        self.list.as_ref()
    }

    /// Slot `index` (its record at `handle`, then with `key`) shows its
    /// texture when the open list is submitted.
    pub(crate) fn publish(&mut self, index: u32, handle: u64, key: u64) {
        self.publish.push(Publish {
            heap: self.heap as usize,
            index,
            handle,
            key,
        });
    }

    /// `bytes` more of texture memory in the open list, which is closed
    /// and handed over when it holds enough.
    pub(crate) fn uploaded(&mut self, bytes: u64) {
        self.bytes += bytes;
        if self.bytes >= LIST_BYTES || self.began.elapsed().as_millis() >= LIST_MS {
            self.flush();
        }
    }

    /// Detiles what the open list uploads, closes it and hands it to the
    /// next submission. Also flushes `same`-only publishes without
    /// allocating an empty D3D12 command list.
    fn flush(&mut self) {
        self.bytes = 0;
        let publish = std::mem::take(&mut self.publish);
        let list = self.list.take();
        if list.is_none() && publish.is_empty() {
            return;
        }
        if let Some(list) = &list {
            let started = std::time::Instant::now();
            let left = list.refresh_textures_ahead();
            STATS[1].fetch_add(started.elapsed().as_nanos() as u64, Relaxed);
            STATS[5].fetch_add(left, Relaxed);
            list.close();
        }
        let heap = HeldHeap::new(crate::descriptor::heap(self.heap));
        lock(&READY).push(Ready {
            list,
            publish,
            _heap: heap,
        });
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        self.flush();
    }
}

/// The upload lists ready now, for the late submission about to be made,
/// which runs them ahead of its own lists; their host slots are published
/// (or scheduled for deferred flip under `VICTORY_SHOW_AFTER` /
/// `VICTORY_SHOW_NEW_LATER`). Called by the thread that submits.
pub(crate) fn take_ready_for_late() -> Vec<HostList> {
    // (Asked without the lock by every submission of a run without it.)
    if JOB.get().is_none() {
        return Vec::new();
    }
    let ready = std::mem::take(&mut *lock(&READY));
    let mut out = Vec::with_capacity(ready.len());
    for r in ready {
        for p in &r.publish {
            let Some(host) = crate::descriptor::heap(p.heap as This).host() else {
                continue;
            };
            let shown = host.publish_for_late(r.list.as_ref(), p.index, p.handle, p.key);
            STATS[if shown { 3 } else { 4 }].fetch_add(1, Relaxed);
        }
        if let Some(list) = r.list {
            out.push(list);
        }
    }
    STATS[2].fetch_add(out.len() as u64, Relaxed);
    out
}

/// Executes and publishes any worker batches already flushed to [`READY`]
/// before a synchronous decoration bake runs. Called while holding [`PASS`]
/// via [`urgent`], so no worker pass can flush concurrently.
pub(crate) fn drain_for_bake(gpu: &crate::host::Host) -> usize {
    if JOB.get().is_none() {
        return 0;
    }
    let ready = std::mem::take(&mut *lock(&READY));
    if ready.is_empty() {
        return 0;
    }
    let mut lists = Vec::new();
    let mut publishes = Vec::new();
    let mut heaps = Vec::new();
    for r in ready {
        if let Some(list) = r.list {
            lists.push(list);
        }
        publishes.extend(r.publish);
        heaps.push(r._heap);
    }
    let list_count = lists.len();
    if !lists.is_empty() {
        STATS[2].fetch_add(list_count as u64, Relaxed);
        let refs: Vec<&HostList> = lists.iter().collect();
        gpu.execute_sync_internal_many(&refs);
        let order: Vec<usize> = (0..lists.len()).collect();
        let detached: Vec<_> = lists
            .into_iter()
            .map(crate::list::Detached::ours)
            .collect();
        crate::list::recycle(detached, &order);
    } else if !publishes.is_empty() {
        crate::host::settle_for_fence();
    }
    let mut shown_count = 0usize;
    for p in &publishes {
        let Some(host) = crate::descriptor::heap(p.heap as This).host() else {
            continue;
        };
        let shown = host.publish_uploaded_slot(p.index, p.handle, p.key);
        STATS[if shown { 3 } else { 4 }].fetch_add(1, Relaxed);
        if shown {
            shown_count += 1;
        }
    }
    drop(heaps);
    shown_count.max(list_count)
}

/// The heap the next pass is over, and what wakes the worker.
struct Job {
    heap: Mutex<Option<HeldHeap>>,
    wake: Condvar,
}

static JOB: OnceLock<Job> = OnceLock::new();
/// Held by whoever runs the pass.
static PASS: Mutex<()> = Mutex::new(());
/// Number of recording threads waiting for [`PASS`].
static URGENT: AtomicUsize = AtomicUsize::new(0);

/// A pass over `heap` is due (once a frame). False if there is no worker:
/// the caller runs the pass itself.
pub(crate) fn kick(heap: &Heap) -> bool {
    if !on() {
        return false;
    }
    static STARTED: OnceLock<bool> = OnceLock::new();
    let started = *STARTED.get_or_init(|| {
        JOB.get_or_init(|| Job {
            heap: Mutex::new(None),
            wake: Condvar::new(),
        });
        match std::thread::Builder::new()
            .name("victory-heap".into())
            .spawn(worker)
        {
            Ok(_) => {
                tracing::info!(
                    "textures picked by index are uploaded by a thread of their own (VICTORY_HEAP_WORKER)"
                );
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "no thread for textures picked by index");
                false
            }
        }
    });
    let Some(job) = JOB.get().filter(|_| started) else {
        return false;
    };
    let _prev = lock(&job.heap).replace(HeldHeap::new(heap));
    job.wake.notify_one();
    true
}

/// Takes the pass from the worker for the caller, which cannot wait for a
/// later submission: what the worker has recorded so far is handed over
/// first, so the caller's list runs behind it.
pub(crate) fn urgent() -> MutexGuard<'static, ()> {
    if let Ok(pass) = PASS.try_lock() {
        return pass;
    }
    struct UrgentWaiter;
    impl Drop for UrgentWaiter {
        fn drop(&mut self) {
            URGENT.fetch_sub(1, Relaxed);
        }
    }
    URGENT.fetch_add(1, Relaxed);
    let _waiter = UrgentWaiter;
    lock(&PASS)
}

/// Whether the worker is to stop where it is ([`urgent`]).
pub(crate) fn make_way() -> bool {
    URGENT.load(Relaxed) > 0
}

fn worker() {
    crate::cores::keep_on_fast_cores("victory-heap");
    let Some(job) = JOB.get() else {
        return;
    };
    loop {
        let heap = {
            let mut next = lock(&job.heap);
            loop {
                match next.take() {
                    Some(heap) => break heap,
                    None => next = job.wake.wait(next).unwrap_or_else(|e| e.into_inner()),
                }
            }
        };
        let started = std::time::Instant::now();
        {
            let _pass = lock(&PASS);
            let heap = heap.heap();
            if let Some(host) = heap.host() {
                let mut batch = Batch::new(heap);
                let _ = crate::draw::State::heap_textures(
                    crate::draw::TextureTarget::Ahead(&mut batch),
                    heap,
                    host,
                    None,
                );
            }
        }
        STATS[0].fetch_add(started.elapsed().as_nanos() as u64, Relaxed);
        // Whoever wants the pass now has it first.
        while make_way() {
            std::thread::yield_now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urgent_stops_worker_and_ready_drains_cleanly() {
        assert!(!make_way());
        assert_eq!(URGENT.load(Relaxed), 0);
        let held = lock(&PASS);
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = std::sync::Arc::new(Mutex::new(release_rx));
        let spawn_waiter = || {
            let acquired_tx = acquired_tx.clone();
            let release_rx = std::sync::Arc::clone(&release_rx);
            std::thread::spawn(move || {
                let pass = urgent();
                acquired_tx.send((make_way(), URGENT.load(Relaxed))).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
                drop(pass);
            })
        };
        let waiter1 = spawn_waiter();
        let waiter2 = spawn_waiter();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while URGENT.load(Relaxed) < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "both concurrent urgent() calls should increment URGENT while PASS is held"
            );
            std::thread::yield_now();
        }
        assert!(make_way());
        drop(held);

        let (still_urgent, remaining) = acquired_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("first urgent() should acquire PASS once released");
        assert!(still_urgent);
        assert_eq!(remaining, 1);
        release_tx.send(()).unwrap();

        let (after, remaining) = acquired_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("second urgent() should acquire PASS once first releases it");
        assert!(!after);
        assert_eq!(remaining, 0);
        release_tx.send(()).unwrap();

        waiter1.join().unwrap();
        waiter2.join().unwrap();
        assert!(!make_way());
        assert_eq!(URGENT.load(Relaxed), 0);

        assert!(take_ready_for_late().is_empty());
        assert!(lock(&READY).is_empty());
    }
}
