//! `VICTORY_SEND_THREAD=1` (step 5a; the recording on the thread is not
//! built: any other value is off). A submission sent ahead (`Host::submit`
//! with `late`) used to end, on the submitting thread (the one named
//! victory-submit in queue/behind.rs), with ExecuteCommandLists of its
//! lists, the Signal of its fence value, `awake::sent`, the timeline stamp
//! and `Host::card_sent`. With the switch the thread hands that tail to one
//! thread of ours, `victory-send`, and goes on to prepare the next
//! submission. The inline path calls the same [`Job::run`], so the old
//! behaviour is this code.
//!
//! Where the bytes are: none move here. By the time a job exists
//! `send::take` has frozen what the submission reads, and the lists are
//! closed. The thread only puts them on the card's queue.
//!
//! The order rules, which keep the card seeing what it saw before:
//!
//! 1. `send::take` has already frozen the submission's bytes when the job is
//!    handed over; nothing about the job reads title memory.
//! 2. The fence value is reserved by the submitting thread (`*submitted` was
//!    already incremented) and the `Flight` goes into `line()` from the
//!    submitting thread, as before, before the next submission looks at it.
//!    The value is not completed until this thread signals it, and every
//!    wait and every landing goes by completion, so no "sent" flag is kept.
//! 3. Everything that puts lists on the host queue from the submitting
//!    thread afterwards must come after this thread's jobs: `Host::card_before`
//!    calls [`wait_idle`] immediately before it executes its sync list. The
//!    other paths settle first, and a settle waits for the fences, which
//!    only this thread signals.
//! 4. The thread always signals: every job's fence value is signalled,
//!    whatever happened before it (an empty list, a panic, an error), so
//!    nothing that waits for a flight can wait for good.
//!
//! The thread never takes the queue-order lock, the submission lock or the
//! line lock: the submitting thread waits for it in [`wait_idle`] while it
//! holds the queue-order lock in `card_before`.

use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Instant;

use super::*;

/// Whether submissions sent ahead are sent by [`hand_over`]'s thread
/// (`VICTORY_SEND_THREAD=1`, only with `late()`). Any other value is off: the
/// recording on the thread (5b) is not built. Read once.
pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| late() && std::env::var("VICTORY_SEND_THREAD").as_deref() == Ok("1"))
}

/// The tail of a submission sent ahead: its closed lists and the fence value
/// to signal behind them.
pub(super) struct Job {
    /// The lists in the order they go to the queue.
    lists: Vec<Option<ID3D12CommandList>>,
    /// The places in `lists` where another ExecuteCommandLists call begins,
    /// after the first (`cuts` in `Host::submit`).
    cuts: Vec<usize>,
    /// The value signalled behind the lists.
    fence: u64,
    /// The card plan's number of the submission, 0 when it has none; only
    /// used to check the order the jobs come in.
    sent_as: u64,
    /// Set once the fence value was signalled, so a panic after that does
    /// not signal it a second time.
    signalled: bool,
    /// Ticks before sending, the stamp place and whether it is a compute
    /// queue's title submission (the arguments of `crate::timeline::sent`).
    #[cfg(feature = "diagnostics")]
    timeline: Option<(u64, Option<usize>, bool)>,
}

// SAFETY: closed lists of free-threaded D3D12 objects; nothing records into them again.
unsafe impl Send for Job {}

impl Job {
    /// A job for `lists` (cut where `cuts` say), to be followed by `fence`.
    pub(super) fn new(lists: Vec<Option<ID3D12CommandList>>, cuts: Vec<usize>, fence: u64, sent_as: u64) -> Job {
        Job {
            lists,
            cuts,
            fence,
            sent_as,
            signalled: false,
            #[cfg(feature = "diagnostics")]
            timeline: None,
        }
    }

    /// Notes what the timeline records when the lists are sent.
    #[cfg(feature = "diagnostics")]
    pub(super) fn timed(mut self, ticks: u64, stamp: Option<usize>, compute: bool) -> Job {
        self.timeline = Some((ticks, stamp, compute));
        self
    }

    /// Sends the lists, signals the fence, and notes it, in the order
    /// `Host::submit` always did. The signal's result is returned; the
    /// caller fails the run on an error, after the bookkeeping it owes.
    pub(super) fn run(&mut self, host: &Host) -> windows::core::Result<()> {
        // SAFETY: closed lists; the fence is ours.
        let signal = unsafe {
            let mut from = 0;
            for to in self.cuts.iter().copied().chain([self.lists.len()]) {
                host.queue.ExecuteCommandLists(&self.lists[from..to]);
                from = to;
            }
            host.queue.Signal(&host.fence, self.fence)
        };
        self.signalled = true;
        awake::sent(&host.device);
        #[cfg(feature = "diagnostics")]
        if let Some((ticks, stamp, compute)) = self.timeline {
            crate::timeline::sent(ticks, self.fence, stamp, compute);
        }
        host.card_sent();
        signal
    }
}

/// What the thread and its waiters share.
struct State {
    /// Jobs not taken by the thread yet, in the order they were handed over.
    jobs: VecDeque<Job>,
    /// Jobs handed over so far.
    handed: u64,
    /// Jobs the thread is finished with (sent, or failed and noted).
    done: u64,
}

static STATE: Mutex<State> = Mutex::new(State { jobs: VecDeque::new(), handed: 0, done: 0 });
/// The thread waits here for a job.
static WORK: Condvar = Condvar::new();
/// Threads in [`wait_idle`] wait here for `done` to reach `handed`.
static IDLE: Condvar = Condvar::new();

/// Jobs, nanoseconds the thread was busy in `run`, waits of the submitting
/// thread that found it busy, nanoseconds of those waits, order violations.
static STATS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// Queues `job` for the thread, starting it on the first call. `Err(job)`
/// gives the job back when the thread could not be made (said once, never
/// tried again): the caller sends it itself.
pub(super) fn hand_over(job: Job) -> Result<(), Job> {
    static STARTED: OnceLock<bool> = OnceLock::new();
    let started = *STARTED.get_or_init(|| match std::thread::Builder::new().name("victory-send".into()).spawn(worker) {
        Ok(_) => {
            tracing::info!("submissions sent ahead are sent by a thread of their own (VICTORY_SEND_THREAD=1)");
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "no thread to send submissions with: they are sent by the submitting thread (VICTORY_SEND_THREAD=1)");
            false
        }
    });
    if !started {
        return Err(job);
    }
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    state.jobs.push_back(job);
    state.handed += 1;
    drop(state);
    WORK.notify_one();
    Ok(())
}

/// Returns once the thread has sent everything handed over so far; at once
/// when nothing is outstanding (always, with the switch off). Called by the
/// submitting thread before it puts a list of its own on the queue, so that
/// list comes after the jobs.
pub(super) fn wait_idle() {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if state.handed == state.done {
        return;
    }
    let began = Instant::now();
    while state.handed != state.done {
        state = IDLE.wait(state).unwrap_or_else(|e| e.into_inner());
    }
    drop(state);
    STATS[2].fetch_add(1, Ordering::Relaxed);
    STATS[3].fetch_add(began.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

/// Whether a job comes in the order it was reserved in: its fence value is
/// above the last job's, and a card plan number (non-zero) is not below the
/// largest seen. `last` holds (last fence, largest plan number) and moves on
/// to this job whatever the answer.
fn in_order(last: &mut (u64, u64), fence: u64, sent_as: u64) -> bool {
    let ok = fence > last.0 && (sent_as == 0 || sent_as >= last.1);
    last.0 = fence;
    last.1 = last.1.max(sent_as);
    ok
}

/// The thread: takes jobs in the order they were handed over and runs them.
/// It does nothing else, and takes none of the host's other locks.
fn worker() {
    crate::cores::keep_on_fast_cores("victory-send");
    let Some(host) = host() else {
        fail(format_args!("no host device for the thread that sends submissions (VICTORY_SEND_THREAD=1)"));
    };
    let mut last = (0u64, 0u64);
    loop {
        // wait for the next job
        let mut job = {
            let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(job) = state.jobs.pop_front() {
                    break job;
                }
                state = WORK.wait(state).unwrap_or_else(|e| e.into_inner());
            }
        };
        // a job out of order is counted and said a few times, then run
        // anyway: holding it back would leave its fence value unsignalled
        if !in_order(&mut last, job.fence, job.sent_as) {
            let seen = STATS[4].fetch_add(1, Ordering::Relaxed);
            if seen < 4 {
                tracing::warn!(
                    fence = job.fence,
                    sent_as = job.sent_as,
                    "a submission reached the thread that sends out of order (VICTORY_SEND_THREAD=1)"
                );
            }
        }
        let began = Instant::now();
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| job.run(host)));
        let busy = began.elapsed().as_nanos() as u64;
        // the signal comes first when a panic skipped it, so nothing that
        // waits for this flight waits for good
        let panicked = outcome.is_err();
        if panicked && !job.signalled {
            // SAFETY: our queue and fence; the value was reserved for this job.
            let _ = unsafe { host.queue.Signal(&host.fence, job.fence) };
        }
        // book the job as done and wake waiters before anything that can end the run
        STATS[0].fetch_add(1, Ordering::Relaxed);
        STATS[1].fetch_add(busy, Ordering::Relaxed);
        {
            let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
            state.done += 1;
        }
        IDLE.notify_all();
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(e)) => fail(format_args!("a submission sent ahead could not be followed by a signal ({e})")),
            Err(_) => fail(format_args!("the thread that sends submissions panicked")),
        }
    }
}

/// The counters, per frame, for the frame-rate line (called from
/// `submit_split_line`); nothing is logged when no job was sent.
pub(super) fn line(frames: f64) {
    let frames = frames.max(1.0);
    let s: [f64; 5] = std::array::from_fn(|i| STATS[i].swap(0, Ordering::Relaxed) as f64);
    if s[0] <= 0.0 && s[2] <= 0.0 && s[4] <= 0.0 {
        return;
    }
    tracing::info!(
        "submissions sent by a thread of their own per frame (VICTORY_SEND_THREAD=1): {:.1} jobs, {:.2} ms in the sends on it, \
         the submitting thread waited {:.2} ms in {:.1} waits before putting its next list on the queue, {:.0} sent out of order",
        s[0] / frames,
        s[1] / 1e6 / frames,
        s[3] / 1e6 / frames,
        s[2] / frames,
        s[4] / frames
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rising_fences_are_in_order() {
        let mut last = (0, 0);
        assert!(in_order(&mut last, 1, 0));
        assert!(in_order(&mut last, 2, 0));
        assert!(in_order(&mut last, 5, 0));
        assert_eq!(last.0, 5);
    }

    #[test]
    fn a_plan_number_of_zero_is_ignored() {
        let mut last = (0, 0);
        assert!(in_order(&mut last, 1, 7));
        assert!(in_order(&mut last, 2, 0));
        assert!(in_order(&mut last, 3, 7));
        assert!(in_order(&mut last, 4, 9));
    }

    #[test]
    fn a_lower_plan_number_is_flagged() {
        let mut last = (0, 0);
        assert!(in_order(&mut last, 1, 9));
        assert!(!in_order(&mut last, 2, 8));
        // the largest stays the largest after a violation
        assert_eq!(last.1, 9);
        assert!(!in_order(&mut last, 3, 8));
    }

    #[test]
    fn a_repeated_or_lower_fence_is_flagged() {
        let mut last = (0, 0);
        assert!(in_order(&mut last, 4, 0));
        assert!(!in_order(&mut last, 4, 0));
        assert!(!in_order(&mut last, 3, 0));
        assert!(in_order(&mut last, 5, 0));
    }

    // The queue state machine (hand_over, the thread, wait_idle against
    // card_before) needs a Host and is not unit-tested. A run with
    // VICTORY_SEND_THREAD=1 must show: the sending and copies-in-the-card rows
    // of the preparing line down, zero sent out of order on this module's line,
    // and the landing never blocked.
}
