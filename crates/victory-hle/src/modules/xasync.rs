//! XTaskQueue and XAsync: the runtime's callback queues and async-call
//! blocks, served as the task-queue provider's object from `QueryApiImpl`.
//!
//! As documented for the public API: a queue has a work port and a
//! completion port, each dispatched manually (the title calls dispatch),
//! on a thread pool, serialised on one thread, or immediately on the
//! submitting thread. A null queue handle means the process queue. An
//! async block's system area holds its status and result size, so both
//! stay readable after our state for the call is gone.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, Weak};
use std::time::Duration;

use super::xgameruntime::{Guid, HRESULT, Interface, Methods, Object, S_OK, add_ref, missing};

/// The task-queue provider; it is also the interface id titles ask for.
pub const PROVIDER: Guid = Guid {
    data1: 0x073b_7dcb,
    data2: 0x1fcf,
    data3: 0x4030,
    data4: [0x94, 0xbe, 0xe3, 0xc9, 0xeb, 0x62, 0x34, 0x28],
};

pub const E_PENDING: HRESULT = 0x8000_000A_u32 as i32;
pub const E_ABORT: HRESULT = 0x8000_4004_u32 as i32;
pub const E_NOTIMPL: HRESULT = 0x8000_4001_u32 as i32;
pub const E_INVALIDARG: HRESULT = 0x8007_0057_u32 as i32;
pub const E_NOT_SUFFICIENT_BUFFER: HRESULT = 0x8007_007A_u32 as i32;
pub const E_ILLEGAL_METHOD_CALL: HRESULT = 0x8000_000E_u32 as i32;

const WORK: u32 = 0;
const COMPLETION: u32 = 1;
const INFINITE: u32 = u32::MAX;

type Callback = extern "system" fn(*mut c_void, bool);
type MonitorCallback = extern "system" fn(*mut c_void, *const Queue, u32);
type TerminatedCallback = extern "system" fn(*mut c_void);
type CompletionRoutine = extern "system" fn(*mut AsyncBlock);
type Provider = extern "system" fn(u32, *const ProviderData) -> HRESULT;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Manual,
    ThreadPool,
    Serialized,
    Immediate,
}

impl Mode {
    fn from_raw(mode: u32) -> Option<Mode> {
        [
            Mode::Manual,
            Mode::ThreadPool,
            Mode::Serialized,
            Mode::Immediate,
        ]
        .get(mode as usize)
        .copied()
    }
}

thread_local! {
    /// The queues whose callbacks this thread is running, innermost last.
    static RUNNING: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// A title callback and its context (addresses, so tasks can cross threads).
#[derive(Clone)]
struct Task {
    context: usize,
    callback: usize,
    /// The queue it was submitted to, which counts it until it has run.
    queue: Weak<Queue>,
}

impl Task {
    fn new(context: *mut c_void, callback: Callback) -> Task {
        Task {
            context: context as usize,
            callback: callback as usize,
            queue: Weak::new(),
        }
    }

    fn run(self, canceled: bool) {
        // SAFETY: built from a `Callback` in `new`.
        let callback: Callback = unsafe { std::mem::transmute(self.callback) };
        // A stack: an immediate port runs a callback inside another.
        RUNNING.with_borrow_mut(|r| r.push(self.queue.as_ptr() as usize));
        callback(self.context as *mut c_void, canceled);
        RUNNING.with_borrow_mut(|r| r.pop());
        if let Some(queue) = self.queue.upgrade() {
            queue.ran();
        }
    }

    fn of(&self, queue: &Arc<Queue>) -> bool {
        std::ptr::eq(self.queue.as_ptr(), Arc::as_ptr(queue))
    }
}

struct Port {
    mode: Mode,
    tasks: Mutex<VecDeque<Task>>,
    ready: Condvar,
}

impl Port {
    fn new(mode: Mode) -> Arc<Port> {
        let port = Arc::new(Port {
            mode,
            tasks: Mutex::default(),
            ready: Condvar::new(),
        });
        if mode == Mode::Serialized {
            // The worker ends once it holds the last reference (every queue
            // using the port is closed) and nothing is left to run.
            let worker = port.clone();
            std::thread::spawn(move || {
                loop {
                    match worker.take(1000, || false) {
                        Some(task) => task.run(false),
                        None if Arc::strong_count(&worker) == 1 => break,
                        None => {}
                    }
                }
            });
        }
        port
    }

    fn submit(&self, task: Task) {
        match self.mode {
            Mode::Immediate => task.run(false),
            Mode::ThreadPool => POOL.submit(task),
            Mode::Manual | Mode::Serialized => {
                lock(&self.tasks).push_back(task);
                self.ready.notify_one();
            }
        }
    }

    /// The next queued task, waiting up to `timeout_ms`, or until `ended`
    /// (checked whenever the port is woken) says no task will come.
    fn take(&self, timeout_ms: u32, ended: impl Fn() -> bool) -> Option<Task> {
        let waiting = |t: &mut VecDeque<Task>| t.is_empty() && !ended();
        let tasks = lock(&self.tasks);
        let mut tasks = if timeout_ms == INFINITE {
            self.ready
                .wait_while(tasks, waiting)
                .unwrap_or_else(|e| e.into_inner())
        } else {
            let timeout = Duration::from_millis(timeout_ms.into());
            self.ready
                .wait_timeout_while(tasks, timeout, waiting)
                .unwrap_or_else(|e| e.into_inner())
                .0
        };
        tasks.pop_front()
    }
}

/// The thread pool behind thread-pool ports: idle workers are reused, and a
/// new one starts whenever more tasks wait than workers idle, so a callback
/// that blocks on another never starves it. Workers stay for reuse.
struct Pool {
    /// Queued tasks and the number of idle workers.
    state: Mutex<(VecDeque<Task>, usize)>,
    ready: Condvar,
}

static POOL: Pool = Pool {
    state: Mutex::new((VecDeque::new(), 0)),
    ready: Condvar::new(),
};

impl Pool {
    fn submit(&'static self, task: Task) {
        let mut state = lock(&self.state);
        state.0.push_back(task);
        if state.0.len() > state.1 {
            std::thread::spawn(move || self.work());
        } else {
            self.ready.notify_one();
        }
    }

    fn work(&self) {
        let mut state = lock(&self.state);
        loop {
            while state.0.is_empty() {
                state.1 += 1;
                state = self.ready.wait(state).unwrap_or_else(|e| e.into_inner());
                state.1 -= 1;
            }
            let task = state.0.pop_front();
            drop(state);
            if let Some(task) = task {
                task.run(false);
            }
            state = lock(&self.state);
        }
    }
}

/// A queue handle is `Arc::into_raw` of this; duplicating and closing a
/// handle add and drop a reference.
pub struct Queue {
    ports: [Arc<Port>; 2],
    monitors: Mutex<Vec<(u64, usize, usize)>>,
    terminated: AtomicBool,
    /// Delayed callbacks still waiting out their delay, in submission
    /// order: terminating the queue cancels them with the queued ones.
    delayed: Mutex<BTreeMap<u64, Task>>,
    /// Callbacks submitted and not yet run (live or canceled), and the
    /// termination callback (context, callback) waiting for them.
    running: Mutex<(usize, Option<(usize, usize)>)>,
    idle: Condvar,
}

impl Queue {
    fn new(ports: [Arc<Port>; 2]) -> Arc<Queue> {
        Arc::new(Queue {
            ports,
            monitors: Mutex::default(),
            terminated: AtomicBool::new(false),
            delayed: Mutex::default(),
            running: Mutex::default(),
            idle: Condvar::new(),
        })
    }

    fn submit(self: &Arc<Self>, port: u32, delay_ms: u32, mut task: Task) -> HRESULT {
        static NEXT_DELAYED: AtomicU64 = AtomicU64::new(0);
        if port > COMPLETION {
            return E_INVALIDARG;
        }
        if self.terminated.load(Ordering::Acquire) {
            return E_ABORT;
        }
        task.queue = Arc::downgrade(self);
        if delay_ms == 0 {
            return self.deliver(port, task, false);
        }
        let id = NEXT_DELAYED.fetch_add(1, Ordering::Relaxed);
        {
            // Checked again under the lock termination drains: the callback
            // is either canceled with the others or refused. Counted from
            // here, so termination also waits for one whose delay ran out
            // just before it and that is on its way to the port.
            let mut delayed = lock(&self.delayed);
            if self.terminated.load(Ordering::Acquire) {
                return E_ABORT;
            }
            lock(&self.running).0 += 1;
            delayed.insert(id, task);
        }
        let queue = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay_ms.into()));
            // Gone if the queue was terminated meanwhile, which canceled it.
            let task = lock(&queue.delayed).remove(&id);
            if let Some(task) = task {
                queue.deliver(port, task, true);
            }
        });
        S_OK
    }

    /// Hands `task` to its port, unless the queue was terminated: a new one
    /// is then refused, and one `counted` already (delayed) is canceled.
    fn deliver(self: &Arc<Self>, port: u32, task: Task, counted: bool) -> HRESULT {
        let target = &self.ports[port as usize];
        let inline = {
            // Termination sets its flag under this lock and drains the ports
            // after: a task either reaches its port first, and is drained,
            // or sees the flag. Counted before, so termination waits for it.
            let mut running = lock(&self.running);
            if self.terminated.load(Ordering::Acquire) {
                drop(running);
                if counted {
                    task.run(true);
                }
                return E_ABORT;
            }
            running.0 += usize::from(!counted);
            if target.mode == Mode::Immediate {
                Some(task)
            } else {
                target.submit(task);
                None
            }
        };
        if let Some(task) = inline {
            target.submit(task);
        }
        // Copied out: a monitor may unregister itself.
        let monitors = lock(&self.monitors).clone();
        for (_, context, callback) in monitors {
            // SAFETY: stored from a `MonitorCallback` in `register_monitor`.
            let callback: MonitorCallback = unsafe { std::mem::transmute(callback) };
            callback(context as *mut c_void, Arc::as_ptr(self), port);
        }
        S_OK
    }

    /// One of its callbacks has run: the last one to end after termination
    /// completes it.
    fn ran(&self) {
        let terminated = {
            let mut running = lock(&self.running);
            running.0 -= 1;
            if running.0 > 0 {
                return;
            }
            self.idle.notify_all();
            running.1.take()
        };
        if let Some((context, callback)) = terminated {
            // SAFETY: stored from a `TerminatedCallback` in `XTaskQueueTerminate`.
            let callback: TerminatedCallback = unsafe { std::mem::transmute(callback) };
            callback(context as *mut c_void);
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The process queue: `None` until first used (then a thread-pool queue),
/// `Some(None)` once the title clears it.
static PROCESS_QUEUE: Mutex<Option<Option<Arc<Queue>>>> = Mutex::new(None);

fn process_queue() -> Option<Arc<Queue>> {
    lock(&PROCESS_QUEUE)
        .get_or_insert_with(|| {
            Some(Queue::new([
                Port::new(Mode::ThreadPool),
                Port::new(Mode::ThreadPool),
            ]))
        })
        .clone()
}

/// A new reference to the queue behind `handle` (null: the process queue).
fn queue(handle: *const Queue) -> Option<Arc<Queue>> {
    if handle.is_null() {
        return process_queue();
    }
    // SAFETY: a handle we returned, which the title has not closed.
    unsafe {
        Arc::increment_strong_count(handle);
        Some(Arc::from_raw(handle))
    }
}

pub fn XTaskQueueCreate(work: u32, completion: u32, out: *mut *const Queue) -> HRESULT {
    let (Some(work), Some(completion)) = (Mode::from_raw(work), Mode::from_raw(completion)) else {
        return E_INVALIDARG;
    };
    if out.is_null() {
        return E_INVALIDARG;
    }
    let queue = Queue::new([Port::new(work), Port::new(completion)]);
    // SAFETY: checked non-null; the title passes room for a handle.
    unsafe { out.write(Arc::into_raw(queue)) };
    S_OK
}

/// A port handle points at the port, which its queue keeps alive.
pub fn XTaskQueueGetPort(handle: *const Queue, port: u32, out: *mut *const c_void) -> HRESULT {
    let Some(queue) = queue(handle) else {
        return E_INVALIDARG;
    };
    if port > COMPLETION || out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null.
    unsafe { out.write(Arc::as_ptr(&queue.ports[port as usize]).cast()) };
    S_OK
}

pub fn XTaskQueueCreateComposite(
    work: *const c_void,
    completion: *const c_void,
    out: *mut *const Queue,
) -> HRESULT {
    if work.is_null() || completion.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let port = |p: *const c_void| {
        // SAFETY: a port handle from `XTaskQueueGetPort`, whose queue is open.
        unsafe {
            Arc::increment_strong_count(p.cast::<Port>());
            Arc::from_raw(p.cast::<Port>())
        }
    };
    let queue = Queue::new([port(work), port(completion)]);
    // SAFETY: checked non-null.
    unsafe { out.write(Arc::into_raw(queue)) };
    S_OK
}

pub fn XTaskQueueDuplicateHandle(handle: *const Queue, out: *mut *const Queue) -> HRESULT {
    match queue(handle) {
        Some(queue) if !out.is_null() => {
            // SAFETY: checked non-null.
            unsafe { out.write(Arc::into_raw(queue)) };
            S_OK
        }
        _ => E_INVALIDARG,
    }
}

pub fn XTaskQueueCloseHandle(handle: *const Queue) {
    if !handle.is_null() {
        // SAFETY: a handle we returned; this drops its reference.
        unsafe { Arc::decrement_strong_count(handle) };
    }
}

/// Runs one queued callback of a manual port; false if none came in time,
/// or none can come: the queue was terminated (which ends a wait under way,
/// as a dispatcher thread's loop expects).
pub fn XTaskQueueDispatch(handle: *const Queue, port: u32, timeout_ms: u32) -> bool {
    let Some(queue) = queue(handle) else {
        return false;
    };
    let Some(port) = queue.ports.get(port as usize) else {
        return false;
    };
    if port.mode != Mode::Manual {
        return false;
    }
    match port.take(timeout_ms, || queue.terminated.load(Ordering::Acquire)) {
        Some(task) => {
            task.run(false);
            true
        }
        None => false,
    }
}

pub fn XTaskQueueSubmitDelayedCallback(
    handle: *const Queue,
    port: u32,
    delay_ms: u32,
    context: *mut c_void,
    callback: Option<Callback>,
) -> HRESULT {
    match (queue(handle), callback) {
        (Some(queue), Some(callback)) => queue.submit(port, delay_ms, Task::new(context, callback)),
        _ => E_INVALIDARG,
    }
}

/// Takes the queued tasks of `queue` out of `tasks`, leaving those of other
/// queues that share the port (composite queues) in order.
fn take_tasks(tasks: &mut VecDeque<Task>, queue: &Arc<Queue>) -> VecDeque<Task> {
    let (ours, others) = std::mem::take(tasks).into_iter().partition(|t| t.of(queue));
    *tasks = others;
    ours
}

/// Cancels what is still queued or waiting out a delay (callbacks see
/// `canceled`) and refuses new work. Termination is complete, and `callback`
/// runs, once the callbacks already running have returned too: here when
/// `wait`, otherwise on the thread of the last.
pub fn XTaskQueueTerminate(
    handle: *const Queue,
    wait: bool,
    context: *mut c_void,
    callback: Option<TerminatedCallback>,
) -> HRESULT {
    let Some(queue) = queue(handle) else {
        return E_INVALIDARG;
    };
    // Under the lock deliveries check it under; see `Queue::deliver`.
    {
        let _running = lock(&queue.running);
        queue.terminated.store(true, Ordering::Release);
    }
    let mut pending = Vec::new();
    for port in &queue.ports {
        pending.extend(take_tasks(&mut lock(&port.tasks), &queue));
        // Dispatches waiting on this queue end: nothing more will come.
        // (Those of other queues on the port see theirs is open and wait
        // on.) After the lock above, so none misses both flag and wake.
        port.ready.notify_all();
    }
    // A thread-pool port's tasks wait in the shared pool.
    pending.extend(take_tasks(&mut lock(&POOL.state).0, &queue));
    pending.extend(std::mem::take(&mut *lock(&queue.delayed)).into_values());
    for task in pending {
        task.run(true);
    }
    // Called from one of the queue's own callbacks, waiting would wait for
    // the caller: it returns instead, and the callback's end completes it.
    let inside = RUNNING.with_borrow(|r| r.contains(&(Arc::as_ptr(&queue) as usize)));
    let mut running = lock(&queue.running);
    if wait && !inside {
        running = queue.idle.wait_while(running, |r| r.0 > 0).unwrap_or_else(|e| e.into_inner());
    }
    if running.0 > 0 {
        running.1 = callback.map(|callback| (context as usize, callback as usize));
    } else if let Some(callback) = callback {
        drop(running);
        callback(context);
    }
    S_OK
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

pub fn XTaskQueueRegisterMonitor(
    handle: *const Queue,
    context: *mut c_void,
    callback: Option<MonitorCallback>,
    token: *mut u64,
) -> HRESULT {
    let (Some(queue), Some(callback)) = (queue(handle), callback) else {
        return E_INVALIDARG;
    };
    if token.is_null() {
        return E_INVALIDARG;
    }
    let id = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    lock(&queue.monitors).push((id, context as usize, callback as usize));
    // SAFETY: checked non-null.
    unsafe { token.write(id) };
    S_OK
}

pub fn XTaskQueueUnregisterMonitor(handle: *const Queue, token: u64) {
    if let Some(queue) = queue(handle) {
        lock(&queue.monitors).retain(|m| m.0 != token);
    }
}

/// A new handle to the process queue, if there is one.
pub fn XTaskQueueGetCurrentProcessTaskQueue(out: *mut *const Queue) -> bool {
    let found = process_queue();
    if !out.is_null() {
        // SAFETY: checked non-null.
        unsafe { out.write(found.clone().map_or(std::ptr::null(), Arc::into_raw)) };
    }
    found.is_some()
}

pub fn XTaskQueueSetCurrentProcessTaskQueue(handle: *const Queue) {
    let new = if handle.is_null() {
        None
    } else {
        queue(handle)
    };
    *lock(&PROCESS_QUEUE) = Some(new);
}

/// An async block: what the title fills in, then the system area.
#[repr(C)]
pub struct AsyncBlock {
    pub queue: *const Queue,
    pub context: *mut c_void,
    pub callback: Option<CompletionRoutine>,
    /// Ours: [unused, status, result size, unused].
    pub internal: [usize; 4],
}

#[repr(C)]
pub struct ProviderData {
    pub block: *mut AsyncBlock,
    pub buffer_size: usize,
    pub buffer: *mut c_void,
    pub context: *mut c_void,
}

const OP_BEGIN: u32 = 0;
const OP_DO_WORK: u32 = 1;
const OP_GET_RESULT: u32 = 2;
const OP_CANCEL: u32 = 3;
const OP_CLEANUP: u32 = 4;

/// One call in flight, until its result is taken or it fails.
struct Call {
    provider: Provider,
    context: usize,
    identity: usize,
    done: Mutex<bool>,
    finished: Condvar,
    /// Provider operations under way, and whether the call was released:
    /// its cleanup then waits for the last of them to return.
    busy: Mutex<(usize, bool)>,
}

impl Call {
    /// Runs provider operation `op`, unless the call was released (`None`).
    /// A provider frees its context in cleanup, and a cancel or complete on
    /// another thread may release the call while `DoWork` or `Cancel` runs:
    /// cleanup is put off until they return, and none starts after it. (The
    /// call is over by then, so the title may have reused the block.)
    fn run(
        &self,
        op: u32,
        block: *mut AsyncBlock,
        size: usize,
        buffer: *mut c_void,
    ) -> Option<HRESULT> {
        {
            let mut busy = lock(&self.busy);
            if busy.1 {
                return None;
            }
            busy.0 += 1;
        }
        let hr = self.provider(op, block, size, buffer);
        let last = {
            let mut busy = lock(&self.busy);
            busy.0 -= 1;
            busy.0 == 0 && busy.1
        };
        if last {
            self.provider(OP_CLEANUP, block, 0, std::ptr::null_mut());
        }
        Some(hr)
    }

    /// The provider's cleanup, now or when the operation under way returns.
    /// Once only: `release` takes the call out of `CALLS` first.
    fn cleanup(&self, block: *mut AsyncBlock) {
        let now = {
            let mut busy = lock(&self.busy);
            busy.1 = true;
            busy.0 == 0
        };
        if now {
            self.provider(OP_CLEANUP, block, 0, std::ptr::null_mut());
        }
    }

    fn provider(
        &self,
        op: u32,
        block: *mut AsyncBlock,
        size: usize,
        buffer: *mut c_void,
    ) -> HRESULT {
        let data = ProviderData {
            block,
            buffer_size: size,
            buffer,
            context: self.context as *mut c_void,
        };
        (self.provider)(op, &data)
    }
}

/// Calls by block address.
static CALLS: LazyLock<Mutex<HashMap<usize, Arc<Call>>>> = LazyLock::new(Default::default);

fn call(block: *mut AsyncBlock) -> Option<Arc<Call>> {
    lock(&CALLS).get(&(block as usize)).cloned()
}

/// Whether `call` is still the call in flight on `block`: once it is over,
/// the title may begin another call on the same block.
fn is_current(call: &Arc<Call>, block: *mut AsyncBlock) -> bool {
    lock(&CALLS)
        .get(&(block as usize))
        .is_some_and(|current| Arc::ptr_eq(current, call))
}

/// The block's status and result-size words.
fn words(block: *mut AsyncBlock) -> (&'static AtomicUsize, &'static AtomicUsize) {
    // SAFETY: the title's block outlives the call; its system area is ours.
    unsafe {
        let internal = std::ptr::addr_of_mut!((*block).internal).cast::<AtomicUsize>();
        (&*internal.add(1), &*internal.add(2))
    }
}

fn status(block: *mut AsyncBlock) -> HRESULT {
    words(block).0.load(Ordering::Acquire) as u32 as i32
}

/// Whether the block's system area is all zero, as in a block not yet used.
fn zeroed(block: *mut AsyncBlock) -> bool {
    // SAFETY: the title's block, whose system area is ours to read.
    unsafe { std::ptr::addr_of!((*block).internal).read_volatile() == [0; 4] }
}

fn release(block: *mut AsyncBlock) {
    // Unlocked before the provider runs: it may call back into us.
    let removed = lock(&CALLS).remove(&(block as usize));
    if let Some(call) = removed {
        call.cleanup(block);
    }
}

pub fn XAsyncBegin(
    block: *mut AsyncBlock,
    context: *mut c_void,
    identity: *const c_void,
    _identity_name: *const u8,
    provider: Option<Provider>,
) -> HRESULT {
    let Some(provider) = provider else {
        return E_INVALIDARG;
    };
    if block.is_null() {
        return E_INVALIDARG;
    }
    if let Some(left) = call(block) {
        // Calls are filed by block address, and one that succeeded stays
        // filed until its result is taken. A title that never takes it and
        // frees the block may get the same address for its next one, which
        // it zeroes as every new block must be; the block of a result left
        // behind never reads zero (it holds the result's size). So this is
        // a new block, and the call left behind ends here. A call in flight
        // keeps its address, and a block still holding a result is in use.
        let over = *lock(&left.done);
        if !(over && zeroed(block)) {
            return E_INVALIDARG;
        }
        release(block);
    }
    let (status, size) = words(block);
    status.store(E_PENDING as u32 as usize, Ordering::Release);
    size.store(0, Ordering::Release);
    let call = Arc::new(Call {
        provider,
        context: context as usize,
        identity: identity as usize,
        done: Mutex::new(false),
        finished: Condvar::new(),
        busy: Mutex::default(),
    });
    lock(&CALLS).insert(block as usize, call.clone());
    // A failed begin fails the call, so the completion routine still runs.
    let hr = call.run(OP_BEGIN, block, 0, std::ptr::null_mut());
    if let Some(hr) = hr.filter(|hr| *hr < 0) {
        complete_call(&call, block, hr, 0);
    }
    S_OK
}

/// Queues the provider's `DoWork` on the block's work port. The work
/// belongs to the call that scheduled it: when it runs, that call may be
/// over and the block may hold another.
pub fn XAsyncSchedule(block: *mut AsyncBlock, delay_ms: u32) -> HRESULT {
    struct Work {
        call: Arc<Call>,
        block: *mut AsyncBlock,
    }
    extern "system" fn work(context: *mut c_void, canceled: bool) {
        // SAFETY: boxed in `XAsyncSchedule`; a task runs once.
        let Work { call, block } = *unsafe { Box::from_raw(context.cast::<Work>()) };
        if !is_current(&call, block) {
            return;
        }
        let hr = if canceled {
            E_ABORT
        } else {
            match call.run(OP_DO_WORK, block, 0, std::ptr::null_mut()) {
                Some(hr) => hr,
                None => return,
            }
        };
        complete_call(&call, block, hr, 0);
    }
    if block.is_null() {
        return E_INVALIDARG;
    }
    let Some(call) = call(block) else {
        return E_INVALIDARG;
    };
    // SAFETY: a block in flight is valid.
    let handle = unsafe { (*block).queue };
    let Some(queue) = queue(handle) else {
        return E_INVALIDARG;
    };
    let context = Box::into_raw(Box::new(Work { call, block }));
    let hr = queue.submit(WORK, delay_ms, Task::new(context.cast(), work));
    if hr < 0 {
        // Refused, so the task never runs.
        // SAFETY: still ours.
        drop(unsafe { Box::from_raw(context) });
    }
    hr
}

/// Records the result (the first one wins), wakes waiters and queues the
/// completion routine on the block's completion port.
pub fn XAsyncComplete(block: *mut AsyncBlock, result: HRESULT, required_size: usize) {
    if let Some(call) = call(block) {
        complete_call(&call, block, result, required_size);
    }
}

/// [`XAsyncComplete`] for `call`, which is or was in flight on `block`. Our
/// own completions name the call they belong to, not the block: if the
/// provider already completed it, the title may have begun another call on
/// the block, which a second complete by block would end.
fn complete_call(call: &Call, block: *mut AsyncBlock, result: HRESULT, required_size: usize) {
    extern "system" fn complete(block: *mut c_void, _canceled: bool) {
        let block = block.cast::<AsyncBlock>();
        // SAFETY: read before the status was published; see below.
        if let Some(routine) = unsafe { (*block).callback } {
            routine(block);
        }
    }
    if result == E_PENDING {
        return;
    }
    let mut done = lock(&call.done);
    if *done {
        return;
    }
    // Read before publishing: once the status is set, the title may free
    // the block.
    // SAFETY: a block in flight is valid.
    let (handle, routine) = unsafe { ((*block).queue, (*block).callback) };
    let (status, size) = words(block);
    if result < 0 || required_size == 0 {
        release(block);
    }
    size.store(required_size, Ordering::Release);
    status.store(result as u32 as usize, Ordering::Release);
    *done = true;
    call.finished.notify_all();
    drop(done);
    if routine.is_some() {
        let task = Task::new(block.cast(), complete);
        match queue(handle) {
            Some(queue) if queue.submit(COMPLETION, 0, task.clone()) == S_OK => {}
            _ => task.run(true),
        }
    }
}

pub fn XAsyncGetStatus(block: *mut AsyncBlock, wait: bool) -> HRESULT {
    if block.is_null() {
        return E_INVALIDARG;
    }
    if wait && let Some(call) = call(block) {
        let done = lock(&call.done);
        drop(call.finished.wait_while(done, |d| !*d));
    }
    status(block)
}

pub fn XAsyncGetResultSize(block: *mut AsyncBlock, out: *mut usize) -> HRESULT {
    if block.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let hr = status(block);
    // Any nonnegative completion carries a result, not just S_OK: S_FALSE
    // is a success code too and a provider may return data with it.
    if hr >= 0 {
        // SAFETY: checked non-null.
        unsafe { out.write(words(block).1.load(Ordering::Acquire)) };
    }
    hr
}

/// Asks the provider to cancel; the call fails with `E_ABORT` unless the
/// provider completed it first. A completed call is left alone.
pub fn XAsyncCancel(block: *mut AsyncBlock) {
    if let Some(call) = call(block) {
        if *lock(&call.done) {
            return;
        }
        call.run(OP_CANCEL, block, 0, std::ptr::null_mut());
        complete_call(&call, block, E_ABORT, 0);
    }
}

/// Has the provider write the result into `buffer`, then ends the call.
pub fn XAsyncGetResult(
    block: *mut AsyncBlock,
    identity: *const c_void,
    buffer_size: usize,
    buffer: *mut c_void,
    used: *mut usize,
) -> HRESULT {
    if block.is_null() {
        return E_INVALIDARG;
    }
    let hr = status(block);
    let size = words(block).1.load(Ordering::Acquire);
    if hr < 0 || size == 0 {
        if hr >= 0 && !used.is_null() {
            // SAFETY: checked non-null.
            unsafe { used.write(0) };
        }
        return hr;
    }
    let Some(call) = call(block) else {
        return E_ILLEGAL_METHOD_CALL;
    };
    if call.identity != identity as usize {
        return E_INVALIDARG;
    }
    if buffer_size < size || buffer.is_null() {
        return E_NOT_SUFFICIENT_BUFFER;
    }
    let Some(hr) = call.run(OP_GET_RESULT, block, buffer_size, buffer) else {
        return E_ILLEGAL_METHOD_CALL;
    };
    if !used.is_null() {
        // SAFETY: checked non-null.
        unsafe { used.write(size) };
    }
    release(block);
    hr
}

/// Starts a call on `block` that is already over: it completes with `hr`
/// and no result data, through the block's completion port as usual.
pub fn finished(block: *mut AsyncBlock, identity: *const c_void, hr: HRESULT) -> HRESULT {
    finished_with(
        block,
        identity,
        if hr < 0 { Err(hr) } else { Ok(Vec::new()) },
    )
}

/// Like [`finished`], with the bytes `XAsyncGetResult` hands out for
/// `identity` (an error has none).
pub fn finished_with(
    block: *mut AsyncBlock,
    identity: *const c_void,
    result: Result<Vec<u8>, HRESULT>,
) -> HRESULT {
    type Data = Result<Vec<u8>, HRESULT>;
    extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data; the context is the
        // boxed result until cleanup frees it.
        let data = unsafe { &*data };
        let result = data.context.cast::<Data>();
        match op {
            // SAFETY: boxed in `finished_with`; nothing runs after cleanup.
            OP_CLEANUP => drop(unsafe { Box::from_raw(result) }),
            OP_BEGIN => {
                // SAFETY: as above. Copied out: completing may clean up.
                let (hr, len) = match unsafe { &*result } {
                    Ok(bytes) => (S_OK, bytes.len()),
                    Err(hr) => (*hr, 0),
                };
                XAsyncComplete(data.block, hr, len);
            }
            // SAFETY: as above; the runtime checked the buffer's size.
            OP_GET_RESULT => unsafe {
                if let Ok(bytes) = &*result {
                    data.buffer
                        .cast::<u8>()
                        .copy_from(bytes.as_ptr(), bytes.len());
                }
            },
            _ => {}
        }
        S_OK
    }
    let context = Box::into_raw(Box::new(result));
    let hr = XAsyncBegin(
        block,
        context.cast(),
        identity,
        std::ptr::null(),
        Some(provider),
    );
    if hr < 0 {
        // Refused before the call existed, so no cleanup will come.
        // SAFETY: still ours.
        drop(unsafe { Box::from_raw(context) });
    }
    hr
}

/// Like [`finished_with`], but the result comes from `work`, run on a
/// thread of ours; the call stays pending until it returns (a dialog, say).
pub fn pending_with(
    block: *mut AsyncBlock,
    identity: *const c_void,
    work: impl FnOnce() -> Result<Vec<u8>, HRESULT> + Send + 'static,
) -> HRESULT {
    type Data = Mutex<(Option<Box<dyn FnOnce() -> Result<Vec<u8>, HRESULT> + Send>>, Vec<u8>)>;
    extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data; the context is the
        // shared state until cleanup releases it.
        let data = unsafe { &*data };
        let state = data.context.cast::<Data>();
        match op {
            // SAFETY: from `Arc::into_raw` in `pending_with`; released once.
            OP_CLEANUP => drop(unsafe { Arc::from_raw(state) }),
            OP_BEGIN => {
                let Some(call) = call(data.block) else {
                    return E_INVALIDARG;
                };
                // SAFETY: the context is a live `Arc`; this takes a count of
                // our own, which the thread keeps. Cleanup may run first (a
                // cancel ends the call), so the thread must not borrow it.
                let state = unsafe {
                    Arc::increment_strong_count(state);
                    Arc::from_raw(state)
                };
                let work = state.lock().unwrap_or_else(|e| e.into_inner()).0.take();
                let block = data.block as usize;
                std::thread::spawn(move || {
                    let result = work.map_or(Err(E_ABORT), |work| work());
                    let (hr, len) = match &result {
                        Ok(bytes) => (S_OK, bytes.len()),
                        Err(hr) => (*hr, 0),
                    };
                    if let Ok(bytes) = result {
                        state.lock().unwrap_or_else(|e| e.into_inner()).1 = bytes;
                    }
                    // Completes the call this began with, not whatever the
                    // block holds by now: a canceled call is over (and does
                    // not touch the block again), and the title may have
                    // begun another call on it.
                    complete_call(&call, block as *mut AsyncBlock, hr, len);
                });
            }
            // SAFETY: as above; the runtime checked the buffer's size.
            OP_GET_RESULT => unsafe {
                let state = (*state).lock().unwrap_or_else(|e| e.into_inner());
                data.buffer
                    .cast::<u8>()
                    .copy_from(state.1.as_ptr(), state.1.len());
            },
            _ => {}
        }
        S_OK
    }
    let state: Arc<Data> = Arc::new(Mutex::new((Some(Box::new(work)), Vec::new())));
    let context = Arc::into_raw(state);
    let hr = XAsyncBegin(
        block,
        context.cast_mut().cast(),
        identity,
        std::ptr::null(),
        Some(provider),
    );
    if hr < 0 {
        // SAFETY: refused before the call existed; still ours.
        drop(unsafe { Arc::from_raw(context) });
    }
    hr
}

/// The result of a call begun with [`finished_with`] for `identity`, as a
/// `T` written through `out`.
pub fn result<T>(block: *mut AsyncBlock, identity: *const c_void, out: *mut T) -> HRESULT {
    XAsyncGetResult(
        block,
        identity,
        size_of::<T>(),
        out.cast(),
        std::ptr::null_mut(),
    )
}

/// The bytes of a value, for [`finished_with`].
pub fn bytes<T: Copy>(value: T) -> Vec<u8> {
    // SAFETY: a `Copy` value is plain data; padding bytes are never read.
    unsafe { std::slice::from_raw_parts((&raw const value).cast::<u8>(), size_of::<T>()) }.to_vec()
}

type Work = extern "system" fn(*mut AsyncBlock) -> HRESULT;

/// Runs `work` on the block's work port as an async call of its own; its
/// return value is the call's result.
pub fn XAsyncRun(block: *mut AsyncBlock, work: Option<Work>) -> HRESULT {
    extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data.
        let data = unsafe { &*data };
        match op {
            OP_BEGIN => XAsyncSchedule(data.block, 0),
            OP_DO_WORK => {
                // SAFETY: the context is the `Work` given to `XAsyncRun`.
                let work = unsafe { std::mem::transmute::<*mut c_void, Work>(data.context) };
                work(data.block)
            }
            _ => S_OK,
        }
    }
    let Some(work) = work else {
        return E_INVALIDARG;
    };
    let identity = XAsyncRun as *const c_void;
    XAsyncBegin(
        block,
        work as *mut c_void,
        identity,
        std::ptr::null(),
        Some(provider),
    )
}

// The object `QueryApiImpl` returns. Each method takes the object first,
// then the API's arguments; slot offsets as titles call them.

pub static OBJECT: Object = Object::new("task queue", &VTABLE);

/// Its interface id is the provider's.
pub const INTERFACES: &[Interface] = &[(PROVIDER, PROVIDER, &OBJECT)];

type This = *const Object;
extern "system" fn m_get_status(_: This, b: *mut AsyncBlock, wait: u8) -> HRESULT {
    XAsyncGetStatus(b, wait != 0)
}
extern "system" fn m_get_result_size(_: This, b: *mut AsyncBlock, out: *mut usize) -> HRESULT {
    XAsyncGetResultSize(b, out)
}
extern "system" fn m_cancel(_: This, b: *mut AsyncBlock) {
    XAsyncCancel(b)
}
extern "system" fn m_run(_: This, b: *mut AsyncBlock, work: Option<Work>) -> HRESULT {
    XAsyncRun(b, work)
}
extern "system" fn m_begin(
    _: This,
    b: *mut AsyncBlock,
    context: *mut c_void,
    identity: *const c_void,
    name: *const u8,
    provider: Option<Provider>,
) -> HRESULT {
    XAsyncBegin(b, context, identity, name, provider)
}
extern "system" fn m_schedule(_: This, b: *mut AsyncBlock, delay: u32) -> HRESULT {
    XAsyncSchedule(b, delay)
}
extern "system" fn m_complete(_: This, b: *mut AsyncBlock, hr: HRESULT, size: usize) {
    XAsyncComplete(b, hr, size)
}
extern "system" fn m_get_result(
    _: This,
    b: *mut AsyncBlock,
    identity: *const c_void,
    size: usize,
    buffer: *mut c_void,
    used: *mut usize,
) -> HRESULT {
    XAsyncGetResult(b, identity, size, buffer, used)
}
extern "system" fn m_create(
    _: This,
    work: u32,
    completion: u32,
    out: *mut *const Queue,
) -> HRESULT {
    XTaskQueueCreate(work, completion, out)
}
extern "system" fn m_create_composite(
    _: This,
    work: *const c_void,
    completion: *const c_void,
    out: *mut *const Queue,
) -> HRESULT {
    XTaskQueueCreateComposite(work, completion, out)
}
extern "system" fn m_get_port(
    _: This,
    q: *const Queue,
    port: u32,
    out: *mut *const c_void,
) -> HRESULT {
    XTaskQueueGetPort(q, port, out)
}
extern "system" fn m_duplicate(_: This, q: *const Queue, out: *mut *const Queue) -> HRESULT {
    XTaskQueueDuplicateHandle(q, out)
}
extern "system" fn m_dispatch(_: This, q: *const Queue, port: u32, timeout: u32) -> bool {
    XTaskQueueDispatch(q, port, timeout)
}
extern "system" fn m_close(_: This, q: *const Queue) {
    XTaskQueueCloseHandle(q)
}
extern "system" fn m_submit(
    _: This,
    q: *const Queue,
    port: u32,
    context: *mut c_void,
    callback: Option<Callback>,
) -> HRESULT {
    XTaskQueueSubmitDelayedCallback(q, port, 0, context, callback)
}
extern "system" fn m_submit_delayed(
    _: This,
    q: *const Queue,
    port: u32,
    delay: u32,
    context: *mut c_void,
    callback: Option<Callback>,
) -> HRESULT {
    XTaskQueueSubmitDelayedCallback(q, port, delay, context, callback)
}
extern "system" fn m_terminate(
    _: This,
    q: *const Queue,
    wait: u8,
    context: *mut c_void,
    callback: Option<TerminatedCallback>,
) -> HRESULT {
    XTaskQueueTerminate(q, wait != 0, context, callback)
}
extern "system" fn m_register_monitor(
    _: This,
    q: *const Queue,
    context: *mut c_void,
    callback: Option<MonitorCallback>,
    token: *mut u64,
) -> HRESULT {
    XTaskQueueRegisterMonitor(q, context, callback, token)
}
extern "system" fn m_unregister_monitor(_: This, q: *const Queue, token: u64) {
    XTaskQueueUnregisterMonitor(q, token)
}
extern "system" fn m_get_process_queue(_: This, out: *mut *const Queue) -> bool {
    XTaskQueueGetCurrentProcessTaskQueue(out)
}
extern "system" fn m_set_process_queue(_: This, q: *const Queue) {
    XTaskQueueSetCurrentProcessTaskQueue(q)
}
/// The runtime's debug check that the caller is not a time-critical thread,
/// which every runtime call in a title makes first.
extern "system" fn m_check_thread(_: This) {}

static VTABLE: Methods<31> = Methods([
    missing::<0> as *const (),         // 0x00 QueryInterface
    add_ref as *const (),              // 0x08 AddRef
    add_ref as *const (),              // 0x10 Release: the object is static
    m_get_status as *const (),         // 0x18
    m_get_result_size as *const (),    // 0x20
    m_cancel as *const (),             // 0x28
    m_run as *const (),                // 0x30
    m_begin as *const (),              // 0x38
    missing::<8> as *const (),         // 0x40
    m_schedule as *const (),           // 0x48
    m_complete as *const (),           // 0x50
    m_get_result as *const (),         // 0x58
    m_create as *const (),             // 0x60
    m_create_composite as *const (),   // 0x68
    m_get_port as *const (),           // 0x70
    m_duplicate as *const (),          // 0x78
    m_dispatch as *const (),           // 0x80
    m_close as *const (),              // 0x88
    m_submit as *const (),             // 0x90
    m_submit_delayed as *const (),     // 0x98
    missing::<20> as *const (),        // 0xa0 register a wait handle
    missing::<21> as *const (),        // 0xa8 unregister it
    m_terminate as *const (),          // 0xb0
    m_register_monitor as *const (),   // 0xb8
    m_unregister_monitor as *const (), // 0xc0
    m_get_process_queue as *const (),  // 0xc8
    m_set_process_queue as *const (),  // 0xd0
    missing::<27> as *const (),        // 0xd8 thread time-sensitivity
    missing::<28> as *const (),        // 0xe0
    m_check_thread as *const (),       // 0xe8
    missing::<30> as *const (),        // 0xf0
]);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    fn slot<F: Copy>(offset: usize) -> F {
        crate::modules::xgameruntime::method(&OBJECT, offset)
    }

    static RAN: AtomicU32 = AtomicU32::new(0);
    extern "system" fn count(context: *mut c_void, canceled: bool) {
        RAN.fetch_add(
            if canceled { 100 } else { context as u32 },
            Ordering::SeqCst,
        );
    }

    #[test]
    fn manual_ports_run_callbacks_when_dispatched() {
        type Create = extern "system" fn(This, u32, u32, *mut *const Queue) -> HRESULT;
        type Submit = extern "system" fn(This, *const Queue, u32, *mut c_void, Callback) -> HRESULT;
        type Dispatch = extern "system" fn(This, *const Queue, u32, u32) -> bool;
        type Close = extern "system" fn(This, *const Queue);
        let this: This = &OBJECT;
        let mut q = std::ptr::null();
        assert_eq!(slot::<Create>(0x60)(this, 0, 0, &mut q), S_OK);
        assert_eq!(
            slot::<Submit>(0x90)(this, q, COMPLETION, 5 as *mut c_void, count),
            S_OK
        );
        assert_eq!(RAN.load(Ordering::SeqCst), 0, "manual: nothing runs yet");
        assert!(
            !slot::<Dispatch>(0x80)(this, q, WORK, 0),
            "work port is empty"
        );
        assert!(slot::<Dispatch>(0x80)(this, q, COMPLETION, 0));
        assert_eq!(RAN.load(Ordering::SeqCst), 5);
        assert!(!slot::<Dispatch>(0x80)(this, q, COMPLETION, 0));
        slot::<Close>(0x88)(this, q);
    }

    /// A provider that schedules its work and returns a u64 result.
    extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data.
        let data = unsafe { &*data };
        match op {
            OP_BEGIN => XAsyncSchedule(data.block, 0),
            OP_DO_WORK => {
                XAsyncComplete(data.block, S_OK, 8);
                S_OK
            }
            OP_GET_RESULT => {
                // SAFETY: at least 8 bytes, checked by the runtime.
                unsafe { data.buffer.cast::<u64>().write(data.context as u64) };
                S_OK
            }
            _ => S_OK,
        }
    }

    static COMPLETED: AtomicU32 = AtomicU32::new(0);
    extern "system" fn completed(_: *mut AsyncBlock) {
        COMPLETED.fetch_add(1, Ordering::SeqCst);
    }

    /// A title that never takes a call's result and frees its block: the
    /// next block allocated at that address (zeroed, as a new block must
    /// be) begins a call like any other.
    #[test]
    fn a_new_block_where_a_result_was_left_begins_a_call() {
        static CLEANED: AtomicU32 = AtomicU32::new(0);
        /// Completes at once with one byte to fetch; counts its cleanups.
        extern "system" fn left(op: u32, data: *const ProviderData) -> HRESULT {
            match op {
                // SAFETY: the runtime passes valid provider data.
                OP_BEGIN => XAsyncComplete(unsafe { (*data).block }, S_OK, 1),
                OP_CLEANUP => drop(CLEANED.fetch_add(1, Ordering::SeqCst)),
                _ => {}
            }
            S_OK
        }
        /// Never completes by itself.
        extern "system" fn idle(_: u32, _: *const ProviderData) -> HRESULT {
            S_OK
        }
        let fresh = || AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        let (none, identity) = (std::ptr::null_mut(), 0x1e as *const c_void);
        let begin = |block: &mut AsyncBlock, provider: Provider| {
            XAsyncBegin(block, none, identity, std::ptr::null(), Some(provider))
        };

        let mut block = fresh();
        assert_eq!(begin(&mut block, left), S_OK);
        assert_eq!(XAsyncGetStatus(&mut block, false), S_OK);
        // The block as the call left it is still that call's.
        assert_eq!(begin(&mut block, left), E_INVALIDARG);
        assert_eq!(CLEANED.load(Ordering::SeqCst), 0);

        // Freed with its result never taken, and a new block in its place.
        block = fresh();
        assert_eq!(finished_with(&mut block, identity, Ok(vec![9])), S_OK);
        assert_eq!(CLEANED.load(Ordering::SeqCst), 1, "the call left behind");
        let mut value = 0u8;
        assert_eq!(result(&mut block, identity, &mut value), S_OK);
        assert_eq!(value, 9);
        assert!(call(&mut block).is_none());

        // A call still in flight keeps its address, whatever the block holds.
        let mut block = fresh();
        assert_eq!(begin(&mut block, idle), S_OK);
        block = fresh();
        assert_eq!(begin(&mut block, left), E_INVALIDARG);
        XAsyncComplete(&mut block, E_ABORT, 0);
        assert!(call(&mut block).is_none());
    }

    #[test]
    fn async_calls_complete_and_return_results() {
        let mut q = std::ptr::null();
        // Work on a thread pool, completion dispatched by hand.
        assert_eq!(XTaskQueueCreate(1, 0, &mut q), S_OK);
        let mut block = AsyncBlock {
            queue: q,
            context: std::ptr::null_mut(),
            callback: Some(completed),
            internal: [0; 4],
        };
        let identity = 0x1d as *const c_void;
        assert_eq!(
            XAsyncBegin(
                &mut block,
                42 as *mut c_void,
                identity,
                std::ptr::null(),
                Some(provider)
            ),
            S_OK
        );
        assert_eq!(XAsyncGetStatus(&mut block, true), S_OK);
        let mut size = 0;
        assert_eq!(XAsyncGetResultSize(&mut block, &mut size), S_OK);
        assert_eq!(size, 8);
        assert_eq!(COMPLETED.load(Ordering::SeqCst), 0, "completion is manual");
        assert!(XTaskQueueDispatch(q, COMPLETION, INFINITE));
        assert_eq!(COMPLETED.load(Ordering::SeqCst), 1);

        let mut value = 0u64;
        let mut small = 0u32;
        let out = (&mut small as *mut u32).cast();
        assert_eq!(
            XAsyncGetResult(&mut block, identity, 4, out, std::ptr::null_mut()),
            E_NOT_SUFFICIENT_BUFFER
        );
        let out = (&mut value as *mut u64).cast();
        let mut used = 0;
        assert_eq!(
            XAsyncGetResult(&mut block, identity, 8, out, &mut used),
            S_OK
        );
        assert_eq!((value, used), (42, 8));
        assert!(call(&mut block).is_none(), "the call ends with its result");
        XTaskQueueCloseHandle(q);
    }

    /// What a test sees of a block that a title reuses: its completion
    /// routine begins the next call on the same block.
    #[derive(Default)]
    struct Reuse {
        completions: AtomicU32,
        next_begun: AtomicBool,
        next_worked: AtomicBool,
    }

    /// A block whose completion routine is [`begin_next`].
    fn reused_block(queue: *const Queue, reuse: &Reuse) -> AsyncBlock {
        AsyncBlock {
            queue,
            context: std::ptr::from_ref(reuse).cast_mut().cast(),
            callback: Some(begin_next),
            internal: [0; 4],
        }
    }

    /// Begins a call without an identity.
    fn begin(block: *mut AsyncBlock, context: *mut c_void, provider: Provider) -> HRESULT {
        let none = std::ptr::null();
        XAsyncBegin(block, context, none, none.cast(), Some(provider))
    }

    /// Begins the next call on the block the first time a call completes,
    /// as a title that keeps one block for repeated calls does.
    extern "system" fn begin_next(block: *mut AsyncBlock) {
        // SAFETY: the block is from `reused_block`, its `Reuse` still alive.
        let context = unsafe { (*block).context };
        // SAFETY: as above.
        let reuse = unsafe { &*context.cast::<Reuse>() };
        if reuse.completions.fetch_add(1, Ordering::SeqCst) == 0 {
            let hr = begin(block, context, next_call);
            reuse.next_begun.store(hr == S_OK, Ordering::SeqCst);
        }
    }

    /// The next call's provider: it never ends on its own, and notes being
    /// asked to work.
    extern "system" fn next_call(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data; its context is
        // the test's `Reuse`.
        let reuse = unsafe { &*(*data).context.cast::<Reuse>() };
        if op == OP_DO_WORK {
            reuse.next_worked.store(true, Ordering::SeqCst);
            return E_PENDING;
        }
        S_OK
    }

    /// A provider that ends its call itself, in `DoWork` and on cancel, then
    /// returns `S_OK`, as providers do.
    extern "system" fn ends_itself(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data.
        let block = unsafe { (*data).block };
        match op {
            OP_DO_WORK => XAsyncComplete(block, S_OK, 0),
            OP_CANCEL => XAsyncComplete(block, E_ABORT, 0),
            _ => {}
        }
        S_OK
    }

    #[test]
    fn a_finished_calls_work_result_leaves_the_next_call_alone() {
        let mut q = std::ptr::null();
        // Work dispatched by hand; completion routines run where the call
        // completes, inside the provider's `DoWork`.
        assert_eq!(XTaskQueueCreate(0, 3, &mut q), S_OK);
        let reuse = Reuse::default();
        let mut block = reused_block(q, &reuse);
        assert_eq!(begin(&mut block, std::ptr::null_mut(), ends_itself), S_OK);
        assert_eq!(XAsyncSchedule(&mut block, 0), S_OK);
        assert!(XTaskQueueDispatch(q, WORK, 0));
        assert!(reuse.next_begun.load(Ordering::SeqCst));
        assert_eq!(XAsyncGetStatus(&mut block, false), E_PENDING);
        assert_eq!(reuse.completions.load(Ordering::SeqCst), 1);

        XAsyncCancel(&mut block);
        assert_eq!(XAsyncGetStatus(&mut block, false), E_ABORT);
        XTaskQueueCloseHandle(q);
    }

    #[test]
    fn a_canceled_call_leaves_the_next_call_alone() {
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(0, 3, &mut q), S_OK);
        let reuse = Reuse::default();
        let mut block = reused_block(q, &reuse);
        assert_eq!(begin(&mut block, std::ptr::null_mut(), ends_itself), S_OK);
        assert_eq!(XAsyncSchedule(&mut block, 0), S_OK);
        // The provider ends the call on cancel, before its work has run.
        XAsyncCancel(&mut block);
        assert!(reuse.next_begun.load(Ordering::SeqCst));
        assert_eq!(XAsyncGetStatus(&mut block, false), E_PENDING);
        // The canceled call's work is still queued; it runs nothing now.
        assert!(XTaskQueueDispatch(q, WORK, 0));
        assert!(!reuse.next_worked.load(Ordering::SeqCst));
        assert_eq!(XAsyncGetStatus(&mut block, false), E_PENDING);
        assert_eq!(reuse.completions.load(Ordering::SeqCst), 1);

        XAsyncCancel(&mut block);
        assert_eq!(XAsyncGetStatus(&mut block, false), E_ABORT);
        XTaskQueueCloseHandle(q);
    }

    #[test]
    fn a_nonnegative_completion_still_returns_its_payload() {
        const S_FALSE: HRESULT = 1;
        extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
            // SAFETY: the runtime passes valid provider data.
            let data = unsafe { &*data };
            match op {
                OP_BEGIN => XAsyncSchedule(data.block, 0),
                OP_DO_WORK => {
                    XAsyncComplete(data.block, S_FALSE, 8);
                    S_OK
                }
                OP_GET_RESULT => {
                    // SAFETY: the caller checked eight bytes.
                    unsafe { data.buffer.cast::<u64>().write(0x1234) };
                    S_OK
                }
                _ => S_OK,
            }
        }
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(0, 0, &mut q), S_OK);
        let mut block = AsyncBlock {
            queue: q,
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        let identity = 0x77 as *const c_void;
        assert_eq!(
            XAsyncBegin(&mut block, std::ptr::null_mut(), identity, std::ptr::null(), Some(provider)),
            S_OK
        );
        assert!(XTaskQueueDispatch(q, WORK, 0));
        assert_eq!(XAsyncGetStatus(&mut block, false), S_FALSE);
        let mut size = 0;
        assert_eq!(XAsyncGetResultSize(&mut block, &mut size), S_FALSE);
        assert_eq!(size, 8);
        let mut value = 0u64;
        let mut used = 0;
        assert_eq!(
            XAsyncGetResult(&mut block, identity, 8, (&mut value as *mut u64).cast(), &mut used),
            S_OK
        );
        assert_eq!((value, used), (0x1234, 8));
        assert!(call(&raw mut block).is_none(), "the call ends with its result");
        XTaskQueueCloseHandle(q);
    }

    #[test]
    fn canceling_a_completed_call_leaves_it_alone() {
        static CANCELS: AtomicU32 = AtomicU32::new(0);
        extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
            // SAFETY: the runtime passes valid provider data.
            let data = unsafe { &*data };
            match op {
                OP_BEGIN => {
                    XAsyncComplete(data.block, S_OK, 1);
                    S_OK
                }
                OP_CANCEL => {
                    CANCELS.fetch_add(1, Ordering::SeqCst);
                    S_OK
                }
                OP_GET_RESULT => {
                    // SAFETY: the runtime checked one byte.
                    unsafe { data.buffer.cast::<u8>().write(0x5a) };
                    S_OK
                }
                _ => S_OK,
            }
        }
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(0, 0, &mut q), S_OK);
        let mut block = AsyncBlock {
            queue: q,
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        let identity = 0x78 as *const c_void;
        assert_eq!(
            XAsyncBegin(
                &mut block,
                std::ptr::null_mut(),
                identity,
                std::ptr::null(),
                Some(provider),
            ),
            S_OK
        );
        assert_eq!(XAsyncGetStatus(&mut block, false), S_OK);
        XAsyncCancel(&mut block);
        assert_eq!(CANCELS.load(Ordering::SeqCst), 0);
        assert_eq!(XAsyncGetStatus(&mut block, false), S_OK);
        let mut byte = 0u8;
        let mut used = 0;
        assert_eq!(
            XAsyncGetResult(
                &mut block,
                identity,
                1,
                (&mut byte as *mut u8).cast(),
                &mut used,
            ),
            S_OK
        );
        assert_eq!((byte, used), (0x5a, 1));
        XTaskQueueCloseHandle(q);
    }

    #[test]
    fn a_canceled_pending_call_cannot_complete_the_next_call_on_its_block() {
        extern "system" fn idle(_: u32, _: *const ProviderData) -> HRESULT {
            S_OK
        }
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let mut block = AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        let first = 0x91 as *const c_void;
        assert_eq!(
            pending_with(&mut block, first, move || {
                let _ = gate.recv();
                Ok(vec![1, 2])
            }),
            S_OK
        );
        XAsyncCancel(&mut block);
        assert_eq!(XAsyncGetStatus(&mut block, false), E_ABORT);

        // The title reuses the block for a new call, zeroed as new blocks are.
        block.internal = [0; 4];
        let second = 0x92 as *const c_void;
        assert_eq!(
            XAsyncBegin(
                &mut block,
                std::ptr::null_mut(),
                second,
                std::ptr::null(),
                Some(idle)
            ),
            S_OK
        );
        assert_eq!(XAsyncGetStatus(&mut block, false), E_PENDING);

        // The canceled call's work ends now: it must not finish the new call.
        assert!(release.send(()).is_ok());
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            XAsyncGetStatus(&mut block, false),
            E_PENDING,
            "the second call is still pending"
        );
        XAsyncComplete(&mut block, S_OK, 0);
    }

    #[test]
    fn a_pending_call_hands_out_what_its_work_returns() {
        let mut block = AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        let identity = 0x93 as *const c_void;
        assert_eq!(
            pending_with(&mut block, identity, || Ok(vec![7, 8, 9])),
            S_OK
        );
        assert_eq!(XAsyncGetStatus(&mut block, true), S_OK);
        let mut size = 0;
        assert_eq!(XAsyncGetResultSize(&mut block, &mut size), S_OK);
        assert_eq!(size, 3);
        let mut out = [0u8; 3];
        assert_eq!(
            XAsyncGetResult(
                &mut block,
                identity,
                out.len(),
                out.as_mut_ptr().cast(),
                std::ptr::null_mut()
            ),
            S_OK
        );
        assert_eq!(out, [7, 8, 9]);
    }

    #[test]
    fn run_returns_the_work_result() {
        extern "system" fn work(_: *mut AsyncBlock) -> HRESULT {
            E_ABORT
        }
        let mut block = AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        assert_eq!(
            slot::<extern "system" fn(This, *mut AsyncBlock, Option<Work>) -> HRESULT>(0x30)(
                &OBJECT,
                &mut block,
                Some(work)
            ),
            S_OK
        );
        assert_eq!(XAsyncGetStatus(&mut block, true), E_ABORT);
    }

    #[test]
    fn pool_callbacks_may_wait_for_each_other() {
        static SECOND: AtomicBool = AtomicBool::new(false);
        static FIRST: AtomicBool = AtomicBool::new(false);
        extern "system" fn waits(_: *mut c_void, _: bool) {
            let start = std::time::Instant::now();
            while !SECOND.load(Ordering::SeqCst) && start.elapsed().as_secs() < 5 {
                std::thread::yield_now();
            }
            FIRST.store(SECOND.load(Ordering::SeqCst), Ordering::SeqCst);
        }
        extern "system" fn releases(_: *mut c_void, _: bool) {
            SECOND.store(true, Ordering::SeqCst);
        }
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(1, 1, &mut q), S_OK);
        let none = std::ptr::null_mut();
        assert_eq!(
            XTaskQueueSubmitDelayedCallback(q, WORK, 0, none, Some(waits)),
            S_OK
        );
        assert_eq!(
            XTaskQueueSubmitDelayedCallback(q, WORK, 0, none, Some(releases)),
            S_OK
        );
        let start = std::time::Instant::now();
        while !FIRST.load(Ordering::SeqCst) && start.elapsed().as_secs() < 10 {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(FIRST.load(Ordering::SeqCst));
        XTaskQueueCloseHandle(q);
    }

    /// A dispatcher thread's loop, `while (XTaskQueueDispatch(queue, port,
    /// INFINITE));`, ends when its queue is terminated: nothing can be
    /// submitted to it any more, so nothing else would ever wake it.
    #[test]
    fn terminate_ends_a_waiting_dispatch() {
        static RAN: AtomicU32 = AtomicU32::new(0);
        extern "system" fn ran(_: *mut c_void, _: bool) {
            RAN.fetch_add(1, Ordering::SeqCst);
        }
        /// Dispatches `queue`'s work until a dispatch returns false, then
        /// says so.
        fn dispatcher(queue: *const Queue) -> std::sync::mpsc::Receiver<()> {
            let (done, ended) = std::sync::mpsc::channel();
            let queue = queue as usize;
            std::thread::spawn(move || {
                while XTaskQueueDispatch(queue as *const Queue, WORK, INFINITE) {}
                let _ = done.send(());
            });
            ended
        }
        let run_one = |queue, total| {
            let none = std::ptr::null_mut();
            assert_eq!(
                XTaskQueueSubmitDelayedCallback(queue, WORK, 0, none, Some(ran)),
                S_OK
            );
            for _ in 0..500 {
                if RAN.load(Ordering::SeqCst) == total {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("callback {total} did not run");
        };
        let (long, short) = (Duration::from_secs(5), Duration::from_millis(200));
        let none = std::ptr::null_mut();

        // Two queues on the same manual ports.
        let (mut a, mut b) = (std::ptr::null(), std::ptr::null());
        assert_eq!(XTaskQueueCreate(0, 0, &mut a), S_OK);
        let (mut work, mut completion) = (std::ptr::null(), std::ptr::null());
        assert_eq!(XTaskQueueGetPort(a, WORK, &mut work), S_OK);
        assert_eq!(XTaskQueueGetPort(a, COMPLETION, &mut completion), S_OK);
        assert_eq!(XTaskQueueCreateComposite(work, completion, &mut b), S_OK);

        let ended = dispatcher(a);
        run_one(a, 1);
        // The other queue's end is not this one's: the wait goes on and
        // still runs what comes in.
        assert_eq!(XTaskQueueTerminate(b, false, none, None), S_OK);
        assert!(
            ended.recv_timeout(short).is_err(),
            "ended with another queue"
        );
        run_one(a, 2);
        // Its own queue's end is.
        assert_eq!(XTaskQueueTerminate(a, false, none, None), S_OK);
        assert!(
            ended.recv_timeout(long).is_ok(),
            "still waiting on a terminated queue"
        );
        // And a dispatch that starts after the end returns at once.
        assert!(!XTaskQueueDispatch(a, WORK, INFINITE));
        XTaskQueueCloseHandle(a);
        XTaskQueueCloseHandle(b);
    }

    #[test]
    fn terminate_cancels_what_is_queued() {
        static TERMINATED: AtomicBool = AtomicBool::new(false);
        extern "system" fn terminated(_: *mut c_void) {
            TERMINATED.store(true, Ordering::SeqCst);
        }
        extern "system" fn note(context: *mut c_void, canceled: bool) {
            // SAFETY: the test passes a pointer to its flag.
            unsafe { *context.cast::<bool>() = canceled };
        }
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(0, 0, &mut q), S_OK);
        let mut canceled = false;
        let flag = (&mut canceled as *mut bool).cast();
        assert_eq!(
            XTaskQueueSubmitDelayedCallback(q, WORK, 0, flag, Some(note)),
            S_OK
        );
        assert_eq!(
            XTaskQueueTerminate(q, true, std::ptr::null_mut(), Some(terminated)),
            S_OK
        );
        assert!(canceled && TERMINATED.load(Ordering::SeqCst));
        assert_eq!(
            XTaskQueueSubmitDelayedCallback(q, WORK, 0, flag, Some(note)),
            E_ABORT
        );
        XTaskQueueCloseHandle(q);
    }

    #[test]
    fn terminate_cancels_delayed_callbacks() {
        static EVENTS: Mutex<Vec<&str>> = Mutex::new(Vec::new());
        extern "system" fn delayed(_: *mut c_void, canceled: bool) {
            lock(&EVENTS).push(if canceled { "canceled" } else { "ran" });
        }
        extern "system" fn terminated(_: *mut c_void) {
            lock(&EVENTS).push("terminated");
        }
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(1, 1, &mut q), S_OK);
        let none = std::ptr::null_mut();
        let hr = XTaskQueueSubmitDelayedCallback(q, WORK, 50, none, Some(delayed));
        assert_eq!(hr, S_OK);
        assert_eq!(XTaskQueueTerminate(q, true, none, Some(terminated)), S_OK);
        assert_eq!(*lock(&EVENTS), ["canceled", "terminated"]);
        // Its delay passes without it running.
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(*lock(&EVENTS), ["canceled", "terminated"]);
        XTaskQueueCloseHandle(q);
    }

    const LONG: Duration = Duration::from_secs(10);

    /// A cancel on another thread while the provider's `DoWork` runs: the
    /// call ends at once, but its cleanup (which frees the provider's
    /// context) waits for `DoWork` to return.
    #[test]
    fn cleanup_waits_for_work_under_way() {
        use std::sync::mpsc::{Receiver, Sender, channel};
        struct Gate {
            entered: Sender<()>,
            go: Mutex<Receiver<()>>,
            cleaned: Sender<bool>,
            worked: AtomicBool,
        }
        extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
            // SAFETY: the runtime passes valid provider data; its context
            // is the test's leaked `Gate`.
            let (data, gate) = unsafe { (&*data, &*(*data).context.cast::<Gate>()) };
            match op {
                OP_BEGIN => return XAsyncSchedule(data.block, 0),
                OP_DO_WORK => {
                    gate.entered.send(()).unwrap();
                    let _ = lock(&gate.go).recv();
                    gate.worked.store(true, Ordering::SeqCst);
                }
                OP_CLEANUP => gate.cleaned.send(gate.worked.load(Ordering::SeqCst)).unwrap(),
                _ => {}
            }
            S_OK
        }
        let (entered, entered_rx) = channel();
        let (go, go_rx) = channel();
        let (cleaned, cleaned_rx) = channel();
        let gate: &'static Gate = Box::leak(Box::new(Gate {
            entered,
            go: Mutex::new(go_rx),
            cleaned,
            worked: AtomicBool::new(false),
        }));
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(1, 1, &mut q), S_OK);
        let mut block = AsyncBlock {
            queue: q,
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        let context = std::ptr::from_ref(gate).cast_mut().cast();
        assert_eq!(begin(&mut block, context, provider), S_OK);
        entered_rx.recv_timeout(LONG).unwrap();
        XAsyncCancel(&mut block);
        assert_eq!(XAsyncGetStatus(&mut block, false), E_ABORT);
        assert!(cleaned_rx.try_recv().is_err(), "cleaned up under DoWork");
        go.send(()).unwrap();
        assert!(cleaned_rx.recv_timeout(LONG).unwrap(), "cleaned up before DoWork returned");
        XTaskQueueCloseHandle(q);
    }

    /// Termination is complete, and its callback runs, only once callbacks
    /// already running have returned, whether `XTaskQueueTerminate` waits
    /// for that or returns first.
    #[test]
    fn terminate_completes_once_running_callbacks_return() {
        use std::sync::mpsc::{Receiver, Sender, channel};
        struct Hold {
            entered: Sender<()>,
            go: Mutex<Receiver<()>>,
            release: Sender<()>,
            events: Mutex<Vec<&'static str>>,
            done: Sender<()>,
        }
        fn hold(context: *mut c_void) -> &'static Hold {
            // SAFETY: the test's leaked `Hold`.
            unsafe { &*context.cast::<Hold>() }
        }
        /// Runs on the pool until let go.
        extern "system" fn holds(context: *mut c_void, _: bool) {
            let hold = hold(context);
            hold.entered.send(()).unwrap();
            let _ = lock(&hold.go).recv();
            lock(&hold.events).push("ran");
        }
        /// Queued on the manual port: terminating cancels it, which lets
        /// the held callback go while termination is under way.
        extern "system" fn releases(context: *mut c_void, canceled: bool) {
            let hold = hold(context);
            lock(&hold.events).push(if canceled { "canceled" } else { "released" });
            hold.release.send(()).unwrap();
        }
        extern "system" fn terminated(context: *mut c_void) {
            let hold = hold(context);
            lock(&hold.events).push("terminated");
            hold.done.send(()).unwrap();
        }
        for wait in [true, false] {
            let (entered, entered_rx) = channel();
            let (release, go) = channel();
            let (done, done_rx) = channel();
            let hold: &'static Hold = Box::leak(Box::new(Hold {
                entered,
                go: Mutex::new(go),
                release,
                events: Mutex::default(),
                done,
            }));
            let context = std::ptr::from_ref(hold).cast_mut().cast();
            let mut q = std::ptr::null();
            // Work on the pool, completion dispatched by hand.
            assert_eq!(XTaskQueueCreate(1, 0, &mut q), S_OK);
            let submit = |port, callback: Callback| {
                XTaskQueueSubmitDelayedCallback(q, port, 0, context, Some(callback))
            };
            assert_eq!(submit(WORK, holds), S_OK);
            entered_rx.recv_timeout(LONG).unwrap();
            assert_eq!(submit(COMPLETION, releases), S_OK);
            assert_eq!(XTaskQueueTerminate(q, wait, context, Some(terminated)), S_OK);
            if !wait {
                done_rx.recv_timeout(LONG).unwrap();
            }
            assert_eq!(*lock(&hold.events), ["canceled", "ran", "terminated"], "wait: {wait}");
            XTaskQueueCloseHandle(q);
        }
    }

    /// Composite queues share ports: terminating one cancels its own queued
    /// callbacks, not the other's.
    #[test]
    fn terminate_leaves_other_queues_on_the_port_alone() {
        extern "system" fn note(context: *mut c_void, canceled: bool) {
            // SAFETY: the test passes a pointer to its counter.
            unsafe { *context.cast::<u32>() = if canceled { 2 } else { 1 } };
        }
        let (mut a, mut b) = (std::ptr::null(), std::ptr::null());
        assert_eq!(XTaskQueueCreate(0, 0, &mut a), S_OK);
        let (mut work, mut completion) = (std::ptr::null(), std::ptr::null());
        assert_eq!(XTaskQueueGetPort(a, WORK, &mut work), S_OK);
        assert_eq!(XTaskQueueGetPort(a, COMPLETION, &mut completion), S_OK);
        assert_eq!(XTaskQueueCreateComposite(work, completion, &mut b), S_OK);
        let (mut of_a, mut of_b) = (0u32, 0u32);
        let (to_a, to_b) = ((&raw mut of_a).cast(), (&raw mut of_b).cast());
        assert_eq!(XTaskQueueSubmitDelayedCallback(a, WORK, 0, to_a, Some(note)), S_OK);
        assert_eq!(XTaskQueueSubmitDelayedCallback(b, WORK, 0, to_b, Some(note)), S_OK);
        assert_eq!(XTaskQueueTerminate(b, false, std::ptr::null_mut(), None), S_OK);
        assert_eq!((of_a, of_b), (0, 2));
        assert!(XTaskQueueDispatch(a, WORK, 0));
        assert_eq!(of_a, 1);
        XTaskQueueCloseHandle(a);
        XTaskQueueCloseHandle(b);
    }

    /// A callback that terminates its own queue and asks to wait: waiting
    /// would wait for itself, so the call returns, and termination
    /// completes when the callback does.
    #[test]
    fn terminate_waiting_from_its_own_callback_returns() {
        struct Own {
            queue: usize,
            events: Mutex<Vec<&'static str>>,
            done: std::sync::mpsc::Sender<()>,
        }
        fn own(context: *mut c_void) -> &'static Own {
            // SAFETY: the test's leaked `Own`.
            unsafe { &*context.cast::<Own>() }
        }
        extern "system" fn terminates(context: *mut c_void, _: bool) {
            let own = own(context);
            let queue = own.queue as *const Queue;
            assert_eq!(XTaskQueueTerminate(queue, true, context, Some(terminated)), S_OK);
            lock(&own.events).push("returned");
        }
        extern "system" fn terminated(context: *mut c_void) {
            let own = own(context);
            lock(&own.events).push("terminated");
            own.done.send(()).unwrap();
        }
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(1, 1, &mut q), S_OK);
        let (done, done_rx) = std::sync::mpsc::channel();
        let own: &'static Own = Box::leak(Box::new(Own {
            queue: q as usize,
            events: Mutex::default(),
            done,
        }));
        let context = std::ptr::from_ref(own).cast_mut().cast();
        assert_eq!(XTaskQueueSubmitDelayedCallback(q, WORK, 0, context, Some(terminates)), S_OK);
        done_rx.recv_timeout(LONG).expect("the termination never completed");
        assert_eq!(*lock(&own.events), ["returned", "terminated"]);
        XTaskQueueCloseHandle(q);
    }
}
