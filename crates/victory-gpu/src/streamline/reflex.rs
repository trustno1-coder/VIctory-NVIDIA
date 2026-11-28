//! Reflex by itself (`VICTORY_REFLEX`), the part that needs no library: the
//! switch, which latency markers a frame has sent and which it owes at each
//! point of its life, where the Reflex sleep goes, and what the reports of a
//! Reflex state average to.
//!
//! A Reflex frame is one index, and the Reflex sleep and every marker of it
//! are given a frame token made from that index, on whichever thread they
//! happen. [`Frames`] is what the threads share: who holds it decides, so a
//! frame's markers leave in order and once each.
//!
//! Where a frame's index comes from. The title begins its frames with a wait
//! for the frame origin (`crate::frame`), whose token it hands to the
//! frame's submissions and its present. The runs with the first version of
//! this (the sleep and the simulation start at that wait) showed the title
//! reading its pad once a frame and after the frame's first submission (in
//! the world 101 of 108 frames, and none before it): what a
//! frame submits right after its origin wait is a simulation made before
//! it, and the reading that follows is the input of a later frame's
//! simulation. Which thread reads, the one that waits for the origin or
//! another, the runs did not show; the ten-second line counts it. Either
//! way the sleep and the simulation start belong at that reading, and the
//! reading opens the simulation ([`Frames::sample`]): it gets a new index,
//! the sleep is made there, and the next origin wait takes the simulation
//! for its frame ([`Frames::begin`]), so that its token's submission ends
//! that simulation and its present shows it. This assumes
//! the title is one frame deep (the simulation read after frame N's first
//! submission is the one frame N + 1 submits); nothing seen from outside
//! the title can tell one frame deep from two. A title frame with no
//! simulation to take gets an index of its own: with no sleep while most of
//! the last [`HOLD`] frames took one (the title reads input nearly every
//! frame, and this frame's reading was missed), else with the sleep at the
//! origin wait, as before (loads, cutscenes, menus that read now and then or
//! never).

use super::abi;

/// `VICTORY_REFLEX`: Reflex by itself, with or without frame generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reflex {
    /// `on` or `1`: low latency.
    On,
    /// `boost`: low latency with boost.
    Boost,
}

impl Reflex {
    /// The switch's value: unset, empty, `0` and `off` are no Reflex by
    /// itself (everything as without the switch); any other word is an
    /// error, as an unknown DLSS mode is.
    pub fn from_switch(value: Option<&str>) -> Result<Option<Self>, String> {
        let Some(value) = value else { return Ok(None) };
        Ok(match value.trim().to_ascii_lowercase().as_str() {
            "" | "0" | "off" => None,
            "1" | "on" => Some(Reflex::On),
            "boost" => Some(Reflex::Boost),
            _ => return Err(format!("VICTORY_REFLEX={value}: not one of on, 1, boost, off, 0")),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Reflex::On => "on",
            Reflex::Boost => "boost",
        }
    }

    /// The `sl::ReflexMode` of the switch.
    pub fn sl_mode(self) -> u32 {
        match self {
            Reflex::On => abi::REFLEX_LOW_LATENCY,
            Reflex::Boost => abi::REFLEX_LOW_LATENCY_WITH_BOOST,
        }
    }
}

/// The points of a frame's life that have a marker, in the order a frame
/// passes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    /// The Reflex sleep before the frame's simulation returned (at the
    /// reading that opened it, or at the origin wait), or the frame began
    /// with no sleep.
    SimulationStart = 0,
    /// The title asked for the pad or keyboard reading that opened the
    /// frame's simulation.
    InputSample = 1,
    /// The title's first submission of command lists: its simulation is over
    /// and its submissions begin.
    SimulationEnd = 2,
    SubmitStart = 3,
    /// The present begins on the host.
    SubmitEnd = 4,
    PresentStart = 5,
    PresentEnd = 6,
}

/// The `sl::PCLMarker` of each stage.
const ORDER: [u32; 7] = [
    abi::SIMULATION_START,
    abi::CONTROLLER_INPUT_SAMPLE,
    abi::SIMULATION_END,
    abi::RENDER_SUBMIT_START,
    abi::RENDER_SUBMIT_END,
    abi::PRESENT_START,
    abi::PRESENT_END,
];

const fn bit(stage: Stage) -> u8 {
    1 << stage as u8
}

/// Markers to send now, as bits by stage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Markers(u8);

impl Markers {
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Their `sl::PCLMarker` values, in the order they are to be sent.
    pub fn iter(self) -> impl Iterator<Item = u32> {
        ORDER.iter().enumerate().filter(move |(stage, _)| self.0 & (1 << stage) != 0).map(|(_, &marker)| marker)
    }
}

/// One frame that is not over yet.
struct Frame {
    /// The index its frame tokens are made from.
    index: u32,
    /// The token the title's origin wait gave it (0: no origin wait has it
    /// yet, or the frame was made at a present the title began none for).
    origin: u64,
    /// The number of the title's present that shows it (0: none yet).
    present: u64,
    /// The stages whose markers were sent.
    sent: u8,
    /// A title's origin wait has it: it began the frame, or took the
    /// simulation a reading opened. False for a simulation not taken yet
    /// and for a frame made at a present.
    begun: bool,
    /// The thread of that origin wait, or of the reading that opened the
    /// frame while none has it (0: none).
    thread: u32,
    /// When (microseconds, the caller's clock) the origin wait had it, or
    /// the reading opened it while none has it.
    at: u64,
    /// The title submitted to a queue that draws for it, and when.
    submitted: bool,
    submitted_at: u64,
}

impl Frame {
    fn has(&self, stage: Stage) -> bool {
        self.sent & bit(stage) != 0
    }

    /// The frame's simulation began (its sleep is over): from here on the
    /// title's submissions and presents may end it.
    fn started(&self) -> bool {
        self.has(Stage::SimulationStart)
    }

    /// The markers of every stage up to `stage` that were not sent, now
    /// counted as sent. The input sample is never owed: it is sent with the
    /// simulation's start at the reading that opened the frame, or not at
    /// all. What was sent is always a run from the first stage on, so
    /// nothing ever leaves out of order.
    fn reach(&mut self, stage: Stage) -> Markers {
        let up_to = (bit(stage) << 1).wrapping_sub(1);
        let owed = up_to & !bit(Stage::InputSample) & !self.sent;
        self.sent |= owed;
        Markers(owed)
    }
}

/// Microseconds between two events of a frame, over the frames that had
/// both since the counts were last taken.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Gap {
    pub count: u32,
    pub sum_us: u64,
    pub longest_us: u64,
}

impl Gap {
    const ZERO: Gap = Gap { count: 0, sum_us: 0, longest_us: 0 };

    fn add(&mut self, us: u64) {
        self.count = self.count.saturating_add(1);
        self.sum_us = self.sum_us.saturating_add(us);
        self.longest_us = self.longest_us.max(us);
    }

    pub fn average_us(&self) -> u64 {
        self.sum_us / u64::from(self.count.max(1))
    }
}

/// What happened since the counts were last taken (the ten-second line).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    /// Frames the title began (origin waits that made a token).
    pub begun: u32,
    /// Of them: frames that took the simulation a reading opened, frames
    /// with no simulation to take and no sleep (most of the last [`HOLD`]
    /// frames took one), and frames with no simulation to take that slept
    /// at the origin wait (fewer did).
    pub adopted: u32,
    pub unpaced: u32,
    pub origin_sleeps: u32,
    /// Frames whose simulation start and input sample were sent where the
    /// reading's sleep returned.
    pub sampled: u32,
    /// Readings that opened a simulation (with its sleep), that belonged to
    /// the one open, that came before the newest begun frame's simulation
    /// ended at a submission (the frame still simulates, or is in its sleep
    /// at the origin wait), and that came on another thread than the one
    /// that opens simulations.
    pub opened: u32,
    pub belongs: u32,
    pub held_back: u32,
    pub other_thread: u32,
    /// Simulations let go because a reading came while one was open and
    /// no origin wait had taken it for [`Frames::stale_after_us`] (a load).
    pub stale: u32,
    /// Simulations whose frame was over or past its simulation's end when
    /// the reading's sleep returned: no input sample was sent for them.
    pub lost: u32,
    /// Frames whose first submission came before their simulation started
    /// (the opening reading was still in its sleep): that submission was of
    /// a simulation before, so the title is deeper than one frame there.
    pub submitted_before_start: u32,
    /// Where things happened, by thread: \[0\] on the thread of the origin
    /// wait of the frame concerned, \[1\] on another. For an opening
    /// reading the frame is the newest begun one; for the first submission
    /// and the title's present, the frame they are of.
    pub openings_by_thread: [u32; 2],
    pub first_submits_by_thread: [u32; 2],
    pub presents_by_thread: [u32; 2],
    /// From a frame's origin wait to its first submission; from the newest
    /// begun frame's first submission to the reading that opens the next
    /// simulation; from the opening (after its sleep) to the origin wait
    /// that takes it.
    pub origin_to_submit: Gap,
    pub submit_to_open: Gap,
    pub open_to_adopt: Gap,
    /// Frames presented without a submission of the title's before.
    pub unsubmitted: u32,
    /// Presents the title began no frame for (a second present of one
    /// frame's token among them).
    pub unbegun: u32,
    /// Presents that found their frame by the title's token, and by order.
    pub by_token: u32,
    pub by_order: u32,
    /// Frames whose simulation was ended by a submission that named them
    /// with the title's token, and by one that did not (no token, or one no
    /// frame here has): the newest running frame was taken then, which is
    /// the wrong one if the title still submits a frame after it began the
    /// next.
    pub submitted_by_token: u32,
    pub submitted_by_order: u32,
    /// Frames begun and never presented.
    pub never_presented: u32,
}

impl Counts {
    const ZERO: Counts = Counts {
        begun: 0,
        adopted: 0,
        unpaced: 0,
        origin_sleeps: 0,
        sampled: 0,
        opened: 0,
        belongs: 0,
        held_back: 0,
        other_thread: 0,
        stale: 0,
        lost: 0,
        submitted_before_start: 0,
        openings_by_thread: [0; 2],
        first_submits_by_thread: [0; 2],
        presents_by_thread: [0; 2],
        origin_to_submit: Gap::ZERO,
        submit_to_open: Gap::ZERO,
        open_to_adopt: Gap::ZERO,
        unsubmitted: 0,
        unbegun: 0,
        by_token: 0,
        by_order: 0,
        submitted_by_token: 0,
        submitted_by_order: 0,
        never_presented: 0,
    };
}

/// Where a title frame's simulation starts ([`Frames::begin`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Begin {
    /// The frame took the simulation a reading opened, with this index: its
    /// sleep and its start were the reading's, nothing is sent here.
    Adopted(u32),
    /// A frame of its own with no sleep (most of the last [`HOLD`] frames
    /// took a simulation, just not this one): its start now.
    Unpaced(u32),
    /// A frame of its own that sleeps here first (the title reads input on
    /// fewer frames than that, or none): its start where the sleep returns.
    Paced(u32),
}

/// What the title asking for a reading is ([`Frames::sample`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sample {
    /// It opens a simulation with this index: the sleep is made now, on the
    /// reading's thread, and then [`Frames::sampled`] says its markers.
    Open(u32),
    /// A simulation is open and this reading is part of it.
    Belongs,
    /// The newest begun frame's simulation has not ended at a submission
    /// yet: the reading is of that simulation, or the frame is in its sleep
    /// at the origin wait.
    HeldBack,
    /// Another thread opens simulations.
    OtherThread,
}

/// The frames that are not over, oldest first, the last index given, and
/// the simulation a reading opened that no origin wait has taken yet.
pub(crate) struct Frames {
    last: u32,
    live: Vec<Frame>,
    /// The index of the open simulation: always newer than every frame an
    /// origin wait has (the next origin wait takes it instead of making a
    /// frame of its own), so indexes stay in the order the title presents
    /// its frames.
    open: Option<u32>,
    /// The thread that opened the last simulation (0: none yet).
    sampler: u32,
    /// Origin waits since the last simulation was opened (saturating; it
    /// starts above [`HOLD`], so any thread may open the first one).
    since_opened: u32,
    /// Which of the last [`HOLD`] title frames took a simulation a reading
    /// opened, one bit each, the newest lowest (none at first, so the first
    /// frames sleep at the origin wait).
    adopted_recently: u32,
    /// When the last origin wait came, and how long before it the one
    /// before came (microseconds, the caller's clock; 0 until two came).
    begun_at: Option<u64>,
    interval_us: u64,
    /// The title's present that was a second present of one frame's token,
    /// and that frame's index: the host's present of it shows that frame
    /// again ([`Frames::presenting`]).
    repeat: Option<(u64, u32)>,
    counts: Counts,
}

/// Frames kept at most (Streamline expects no more than three in flight; the
/// rest is room for frames the title begins and never presents).
const KEPT: usize = 8;

/// The title frames looked back on. A frame with no simulation to take goes
/// without a sleep only when more than half of the last `HOLD` frames took
/// one: the title reads input nearly every frame and missed this one (in
/// the world about one frame in fifteen had no reading counted), and a
/// sleep at the origin wait would hold back a finished frame again. Where
/// the title reads input on fewer frames (a menu that reads every tenth),
/// its other frames sleep at the origin wait as they did before readings
/// were looked at: Reflex's guide has `slReflexSleep` called always, and
/// whether its pacing or frame generation's bears frames with no sleep the
/// guides on disk do not say. Also the frames after which another thread
/// may open simulations. Eight frames is a third of a second to two seconds
/// in the world (4 to 28 frames a second there) and a few milliseconds in
/// the menus.
pub(crate) const HOLD: u32 = 8;
const _: () = assert!(HOLD > 0 && HOLD <= u32::BITS);

/// An open simulation is let go when a reading comes and no origin wait has
/// taken it for this many microseconds or for [`STALE_FRAMES`] times the
/// last interval between origin waits, whichever is longer: the title read
/// input and began no frame (a load), and the reading opens a new one. A
/// fixed bound alone let every simulation go in the world's slow stretches,
/// where the origin wait that takes a simulation comes 80 to 370 ms after
/// the reading that opened it.
pub(crate) const STALE_US: u64 = 250_000;
pub(crate) const STALE_FRAMES: u64 = 4;

impl Frames {
    pub const fn new() -> Self {
        Frames {
            last: 0,
            live: Vec::new(),
            open: None,
            sampler: 0,
            since_opened: u32::MAX,
            adopted_recently: 0,
            begun_at: None,
            interval_us: 0,
            repeat: None,
            counts: Counts::ZERO,
        }
    }

    /// How long an open simulation may wait for its origin wait while
    /// readings come, in microseconds ([`STALE_US`]).
    pub fn stale_after_us(&self) -> u64 {
        STALE_US.max(self.interval_us.saturating_mul(STALE_FRAMES))
    }

    /// A new frame with the next index (indexes count from 1 and wrap).
    fn push(&mut self, origin: u64, present: u64, begun: bool, thread: u32, at: u64) -> u32 {
        // The oldest goes if the title begins frames it never presents.
        // (Never the open simulation: it is the newest, and KEPT is above 1.)
        if self.live.len() >= KEPT {
            self.live.remove(0);
            self.counts.never_presented += 1;
        }
        self.last = self.last.wrapping_add(1);
        self.live.push(Frame {
            index: self.last,
            origin,
            present,
            sent: 0,
            begun,
            thread,
            at,
            submitted: false,
            submitted_at: 0,
        });
        self.last
    }

    /// The open simulation is let go if no origin wait took it within
    /// [`Frames::stale_after_us`] (`now` is the caller's clock, as at the
    /// opening). Only a reading asks this: the origin wait that comes always
    /// takes the open simulation, however long after, since that is the
    /// frame that submits it.
    fn let_go_stale(&mut self, now: u64) {
        let Some(index) = self.open else { return };
        let Some(at) = self.live.iter().position(|f| f.index == index) else {
            self.open = None;
            return;
        };
        if now.saturating_sub(self.live[at].at) > self.stale_after_us() {
            self.live.remove(at);
            self.open = None;
            self.counts.stale += 1;
        }
    }

    /// The title begins a frame on `thread` at `now`: its origin wait made
    /// token `origin`. The frame takes the open simulation if there is one,
    /// else it gets an index of its own, with or without a sleep here.
    pub fn begin(&mut self, origin: u64, thread: u32, now: u64) -> Begin {
        self.counts.begun += 1;
        // The interval between origin waits, for how long an open
        // simulation may wait for one while readings come.
        if let Some(before) = self.begun_at {
            self.interval_us = now.saturating_sub(before);
        }
        self.begun_at = Some(now);
        self.since_opened = self.since_opened.saturating_add(1);
        // The frames before this one that took a simulation; this one's bit
        // is set below if it takes one.
        let window = u32::MAX >> (u32::BITS - HOLD);
        let took_before = (self.adopted_recently & window).count_ones();
        self.adopted_recently = (self.adopted_recently << 1) & window;
        // No time bound here: the simulation open now is the one this frame
        // submits, whether the frame came 3 ms or 370 ms after its reading.
        if let Some(index) = self.open.take()
            && let Some(frame) = self.live.iter_mut().find(|f| f.index == index)
        {
            self.counts.open_to_adopt.add(now.saturating_sub(frame.at));
            frame.origin = origin;
            frame.begun = true;
            frame.thread = thread;
            frame.at = now;
            self.counts.adopted += 1;
            self.adopted_recently |= 1;
            return Begin::Adopted(index);
        }
        let index = self.push(origin, 0, true, thread, now);
        if took_before > HOLD / 2 {
            self.counts.unpaced += 1;
            Begin::Unpaced(index)
        } else {
            self.counts.origin_sleeps += 1;
            Begin::Paced(index)
        }
    }

    /// The markers frame `index` owes up to `stage`, now sent; none if the
    /// frame is over.
    pub fn reach(&mut self, index: u32, stage: Stage) -> Markers {
        self.live.iter_mut().find(|f| f.index == index).map_or(Markers(0), |f| f.reach(stage))
    }

    /// The title asks for a pad or keyboard reading on `thread` at `now`,
    /// before the reading is picked. It opens a simulation only when none
    /// is open, the newest begun frame's simulation has ended at its
    /// submission (or it has its present, or there is no begun frame), and
    /// the thread is the one that opens them (or
    /// that one has not for more than [`HOLD`] frames). So at most one
    /// simulation opens between two origin waits, one sleep is made for
    /// it, and it never runs while a sleep at the origin wait does (that
    /// frame has not submitted yet). A simulation that waited too long for
    /// its origin wait ([`Frames::stale_after_us`]) is let go first, and the
    /// reading opens another.
    pub fn sample(&mut self, thread: u32, now: u64) -> Sample {
        self.let_go_stale(now);
        if self.sampler != 0 && self.sampler != thread && self.since_opened <= HOLD {
            self.counts.other_thread += 1;
            return Sample::OtherThread;
        }
        if self.open.is_some() {
            self.counts.belongs += 1;
            return Sample::Belongs;
        }
        let newest = self.live.iter().rev().find(|f| f.begun);
        if let Some(frame) = newest {
            // (Its simulation's end, not its first submission: a submission
            // that came while the opening reading still slept ended nothing,
            // and the readings of that simulation must not open another.)
            if !frame.has(Stage::SimulationEnd) && frame.present == 0 {
                self.counts.held_back += 1;
                return Sample::HeldBack;
            }
            // Which thread reads, against the one that begins frames; and
            // how long after that frame's first submission.
            self.counts.openings_by_thread[usize::from(frame.thread != thread)] += 1;
            if frame.submitted {
                self.counts.submit_to_open.add(now.saturating_sub(frame.submitted_at));
            }
        }
        let index = self.push(0, 0, false, thread, now);
        self.open = Some(index);
        self.sampler = thread;
        self.since_opened = 0;
        self.counts.opened += 1;
        Sample::Open(index)
    }

    /// The sleep of the simulation `index` opened returned at `now`: its
    /// simulation start and its input sample, now sent; none if the frame
    /// is over or its present already sent its simulation's markers (the
    /// sample is lost then, and counted).
    pub fn sampled(&mut self, index: u32, now: u64) -> Markers {
        let Some(frame) = self.live.iter_mut().find(|f| f.index == index) else {
            self.counts.lost += 1;
            return Markers(0);
        };
        if frame.started() {
            self.counts.lost += 1;
            return Markers(0);
        }
        // While no origin wait has it, the opening counts from here (the
        // gap to the adoption is measured after the sleep).
        if !frame.begun {
            frame.at = frame.at.max(now);
        }
        let owed = bit(Stage::SimulationStart) | bit(Stage::InputSample);
        frame.sent |= owed;
        self.counts.sampled += 1;
        Markers(owed)
    }

    /// The title submits command lists to a queue that draws, on `thread`
    /// at `now`, tagged with frame token `origin` (0: not tagged): at the
    /// frame's first submission its simulation ends and its submissions
    /// begin. The frame is the one of that token, else the newest begun
    /// frame whose simulation runs and whose present the title has not
    /// called (never a simulation no origin wait has taken). Which of the
    /// two ended a frame's simulation is counted, for the line.
    pub fn submit(&mut self, origin: u64, thread: u32, now: u64) -> Option<(u32, Markers)> {
        let tagged = (origin != 0).then(|| self.live.iter().position(|f| f.origin == origin)).flatten();
        let at = match tagged {
            Some(at) => at,
            None => self.live.iter().rposition(|f| f.begun && f.started() && f.present == 0)?,
        };
        let frame = &mut self.live[at];
        let first = !frame.submitted;
        if first {
            frame.submitted = true;
            frame.submitted_at = now;
            self.counts.first_submits_by_thread[usize::from(frame.thread != thread)] += 1;
            self.counts.origin_to_submit.add(now.saturating_sub(frame.at));
        }
        // A frame still in its sleep has no simulation to end. (At the
        // origin wait that cannot be: the title has not got the token yet.
        // At a reading it means this submission is of a simulation before.)
        if !frame.started() {
            if first {
                self.counts.submitted_before_start += 1;
            }
            return None;
        }
        let owed = frame.reach(Stage::SubmitStart);
        // Later submissions of the frame owe nothing and count nothing.
        if owed.is_empty() {
            return None;
        }
        // Counted where it has an effect: this submission is the one that
        // ends the frame's simulation.
        match tagged {
            Some(_) => self.counts.submitted_by_token += 1,
            None => self.counts.submitted_by_order += 1,
        }
        Some((frame.index, owed))
    }

    /// The title calls its present number `present` (counted from 1) on
    /// `thread` with frame token `origin` in its first plane (0: none): the
    /// present is that frame's, else the newest begun frame's whose
    /// simulation runs and that has no present yet. Frames begun before it
    /// that have none are over: the title never presented them. (By the
    /// token the frame need not have started: a simulation whose reading is
    /// still in its sleep may be presented, and then sends what it owes at
    /// the present.)
    pub fn title_presents(&mut self, present: u64, origin: u64, thread: u32) {
        let tagged = (origin != 0).then(|| self.live.iter().position(|f| f.origin == origin)).flatten();
        let at = match tagged {
            Some(at) if self.live[at].present == 0 => {
                self.counts.by_token += 1;
                at
            }
            // The token's frame has its present: this one shows that frame
            // again, and the host's present of it names that frame's index
            // ([`Frames::presenting`]).
            Some(at) => {
                self.repeat = Some((present, self.live[at].index));
                return;
            }
            None => {
                let Some(at) = self.live.iter().rposition(|f| f.begun && f.started() && f.present == 0) else { return };
                self.counts.by_order += 1;
                at
            }
        };
        self.counts.presents_by_thread[usize::from(self.live[at].thread != thread)] += 1;
        self.live[at].present = present;
        let before = self.live.len();
        let mut place = 0;
        // (An open simulation is the newest, after `at`: it stays.)
        self.live.retain(|f| {
            let keep = place >= at || f.present != 0;
            place += 1;
            keep
        });
        self.counts.never_presented += (before - self.live.len()) as u32;
    }

    /// Present number `present` begins on the host: its frame's index. The
    /// indexes the host presents must keep rising: frame generation's
    /// constants, tags and present markers use them, and its guide ties
    /// present markers out of step with the frame shown to constants it
    /// cannot find. So a present the title began no frame for does not get
    /// an index newer than a simulation still to be presented:
    /// - a second present of one frame's token shows that frame again and
    ///   gets its index (its markers were sent once, nothing more is);
    /// - another, while a simulation a reading opened is open, takes that
    ///   simulation (the next index in order): its markers are completed
    ///   here, and the next origin wait has none to take;
    /// - else it gets a frame made here, which owes every marker (the
    ///   check's presents, a present before the title's first frame).
    ///
    /// The line counts all three as presents with no frame begun; in the
    /// runs there were none in the title.
    pub fn presenting(&mut self, present: u64) -> u32 {
        if present != 0
            && let Some(frame) = self.live.iter().find(|f| f.present == present)
        {
            if !frame.has(Stage::SubmitStart) {
                self.counts.unsubmitted += 1;
            }
            return frame.index;
        }
        self.counts.unbegun += 1;
        if present != 0
            && let Some((repeated, index)) = self.repeat
            && repeated == present
        {
            self.repeat = None;
            return index;
        }
        if let Some(index) = self.open.take()
            && let Some(frame) = self.live.iter_mut().find(|f| f.index == index)
        {
            frame.present = present;
            return index;
        }
        self.push(0, present, false, 0, 0)
    }

    /// The present of frame `index` returned: its last marker, and the frame
    /// is over.
    pub fn presented(&mut self, index: u32) -> Markers {
        let Some(at) = self.live.iter().position(|f| f.index == index) else { return Markers(0) };
        let owed = self.live[at].reach(Stage::PresentEnd);
        self.live.remove(at);
        owed
    }

    /// The counts since they were last taken.
    pub fn take_counts(&mut self) -> Counts {
        std::mem::take(&mut self.counts)
    }
}

/// A Reflex sleep longer than this, in microseconds, is said in the log,
/// once. The sleep is there to delay a frame's simulation by part of a
/// frame; a quarter of a second is several whole frames at any rate the
/// title runs at.
pub(crate) const SLEEP_LONG_US: u64 = 250_000;
/// After a Reflex sleep longer than this, in microseconds, no further one is
/// asked for. The header gives the sleep no upper bound, it runs on the
/// title's thread (in its request for a reading, or in its wait for the
/// frame origin), and a frame that stands for 30 seconds ends the run: two
/// seconds is far outside anything the sleep is for and well inside what
/// the title forgives once.
pub(crate) const SLEEP_TOO_LONG_US: u64 = 2_000_000;

/// What the length of one Reflex sleep asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slept {
    /// As a sleep is meant to be.
    Fine,
    /// Longer than [`SLEEP_LONG_US`]: worth a warning.
    Long,
    /// Longer than [`SLEEP_TOO_LONG_US`]: the last one asked for.
    TooLong,
}

/// What a Reflex sleep that took `micros` microseconds asks for.
pub(crate) fn slept(micros: u64) -> Slept {
    if micros > SLEEP_TOO_LONG_US {
        Slept::TooLong
    } else if micros > SLEEP_LONG_US {
        Slept::Long
    } else {
        Slept::Fine
    }
}

/// What the frame reports of a Reflex state average to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Latency {
    /// The reports that hold a frame.
    pub frames: u32,
    /// From the simulation's start to the end of the GPU's rendering (the
    /// frame's whole way from game to render), in the unit of the report's
    /// time stamps.
    pub game_to_render: u64,
    /// The GPU's frame time and the time it rendered, in microseconds.
    pub gpu_frame_us: u64,
    pub gpu_active_render_us: u64,
}

/// The averages over the reports that hold a frame (an index, and a
/// rendering that ended after its simulation started); `None` if none does.
/// The header has no field for the total: it is the difference of the two
/// time stamps at its ends.
pub(crate) fn latency(reports: &[abi::ReflexReport]) -> Option<Latency> {
    let mut frames = 0u64;
    let mut sums = [0u64; 3];
    for report in reports {
        if report.frame_id == 0 || report.sim_start_time == 0 || report.gpu_render_end_time <= report.sim_start_time {
            continue;
        }
        frames += 1;
        sums[0] += report.gpu_render_end_time - report.sim_start_time;
        sums[1] += u64::from(report.gpu_frame_time_us);
        sums[2] += u64::from(report.gpu_active_render_time_us);
    }
    (frames > 0).then(|| Latency {
        frames: frames as u32,
        game_to_render: sums[0] / frames,
        gpu_frame_us: sums[1] / frames,
        gpu_active_render_us: sums[2] / frames,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(markers: Markers) -> Vec<u32> {
        markers.iter().collect()
    }

    /// The thread that waits for the frame origin, submits and presents.
    const ORIGIN: u32 = 100;
    /// The thread that reads the pad.
    const READER: u32 = 200;
    /// Another thread that reads the pad.
    const OTHER: u32 = 300;

    /// A frame the title begins with no reading before it (it sleeps at the
    /// origin wait), whose sleep returned: its index.
    fn paced(frames: &mut Frames, origin: u64, now: u64) -> u32 {
        let Begin::Paced(index) = frames.begin(origin, ORIGIN, now) else { panic!("not paced at the origin wait") };
        assert_eq!(sent(frames.reach(index, Stage::SimulationStart)), [abi::SIMULATION_START]);
        index
    }

    /// The title presents the frame of token `origin` as present `present`,
    /// and the host presents it: the markers the frame sends there.
    fn present(frames: &mut Frames, present: u64, origin: u64) -> (u32, Vec<u32>) {
        frames.title_presents(present, origin, ORIGIN);
        host_presents(frames, present)
    }

    /// The host presents the title's present `present`: its index and the
    /// markers the frame sends there.
    fn host_presents(frames: &mut Frames, present: u64) -> (u32, Vec<u32>) {
        let index = frames.presenting(present);
        let mut markers = sent(frames.reach(index, Stage::SubmitEnd));
        markers.extend(sent(frames.reach(index, Stage::PresentStart)));
        markers.extend(sent(frames.presented(index)));
        (index, markers)
    }

    const AT_PRESENT: [u32; 3] = [abi::RENDER_SUBMIT_END, abi::PRESENT_START, abi::PRESENT_END];

    #[test]
    fn the_switch_reads_on_boost_and_off() {
        assert_eq!(Reflex::from_switch(None), Ok(None));
        assert_eq!(Reflex::from_switch(Some("")), Ok(None));
        assert_eq!(Reflex::from_switch(Some("0")), Ok(None));
        assert_eq!(Reflex::from_switch(Some(" Off ")), Ok(None));
        assert_eq!(Reflex::from_switch(Some("1")), Ok(Some(Reflex::On)));
        assert_eq!(Reflex::from_switch(Some(" ON ")), Ok(Some(Reflex::On)));
        assert_eq!(Reflex::from_switch(Some("boost")), Ok(Some(Reflex::Boost)));
        assert!(Reflex::from_switch(Some("fast")).is_err_and(|why| why.contains("VICTORY_REFLEX=fast")));
        // The modes of `sl_reflex.h`: low latency 1, with boost 2.
        assert_eq!(Reflex::On.sl_mode(), 1);
        assert_eq!(Reflex::Boost.sl_mode(), 2);
        assert_eq!((Reflex::On.name(), Reflex::Boost.name()), ("on", "boost"));
    }

    #[test]
    fn a_frame_sends_each_marker_once_and_in_order() {
        let mut frames = Frames::new();
        // No reading yet: the first frame sleeps at the origin wait.
        let frame = 1;
        assert_eq!(frames.begin(41, ORIGIN, 0), Begin::Paced(frame));
        // In its sleep nothing is the frame's yet.
        assert_eq!(frames.submit(0, ORIGIN, 0), None);
        assert_eq!(sent(frames.reach(frame, Stage::SimulationStart)), [abi::SIMULATION_START]);
        assert!(frames.reach(frame, Stage::SimulationStart).is_empty());
        // The first submission ends the simulation and begins the submissions.
        assert_eq!(
            frames.submit(0, ORIGIN, 1).map(|(i, m)| (i, sent(m))),
            Some((frame, vec![abi::SIMULATION_END, abi::RENDER_SUBMIT_START]))
        );
        assert_eq!(frames.submit(0, ORIGIN, 2), None);
        assert_eq!(frames.submit(41, ORIGIN, 2), None);
        // The present: the submissions end where it begins.
        frames.title_presents(7, 41, ORIGIN);
        assert_eq!(frames.presenting(7), frame);
        assert_eq!(sent(frames.reach(frame, Stage::SubmitEnd)), [abi::RENDER_SUBMIT_END]);
        assert_eq!(sent(frames.reach(frame, Stage::PresentStart)), [abi::PRESENT_START]);
        assert_eq!(sent(frames.presented(frame)), [abi::PRESENT_END]);
        // The frame is over.
        assert!(frames.presented(frame).is_empty());
        assert!(frames.reach(frame, Stage::PresentEnd).is_empty());
        let counts = frames.take_counts();
        assert_eq!((counts.begun, counts.origin_sleeps, counts.adopted, counts.unpaced), (1, 1, 0, 0));
        assert_eq!((counts.sampled, counts.opened, counts.by_token), (0, 0, 1));
        assert_eq!((counts.unsubmitted, counts.unbegun, counts.by_order, counts.never_presented), (0, 0, 0, 0));
        // One submission ended the simulation, an untagged one; those after
        // it are not counted.
        assert_eq!((counts.submitted_by_token, counts.submitted_by_order), (0, 1));
        assert_eq!((counts.first_submits_by_thread, counts.presents_by_thread), ([1, 0], [1, 0]));
        assert_eq!(frames.take_counts(), Counts::default());
    }

    #[test]
    fn the_reading_after_a_frames_submission_opens_the_next_frames_simulation() {
        // The order the title keeps in the world: the origin wait, the
        // frame's submission at once, then the pad reading of the next
        // simulation on another thread.
        let mut frames = Frames::new();
        let first = paced(&mut frames, 1, 0);
        assert_eq!(
            frames.submit(1, ORIGIN, 10).map(|(i, m)| (i, sent(m))),
            Some((first, vec![abi::SIMULATION_END, abi::RENDER_SUBMIT_START]))
        );
        // The reading opens a simulation with the next index (the sleep is
        // made now); its start and its input sample go when the sleep is over.
        assert_eq!(frames.sample(READER, 20), Sample::Open(2));
        assert_eq!(sent(frames.sampled(2, 25)), [abi::SIMULATION_START, abi::CONTROLLER_INPUT_SAMPLE]);
        // A second reading of the same simulation (another kind, another
        // device) is part of it: no second sleep, nothing sent.
        assert_eq!(frames.sample(READER, 26), Sample::Belongs);
        assert_eq!(present(&mut frames, 1, 1), (first, AT_PRESENT.to_vec()));
        // The next origin wait takes the simulation: no sleep there and no
        // second start.
        assert_eq!(frames.begin(2, ORIGIN, 30), Begin::Adopted(2));
        assert!(frames.reach(2, Stage::SimulationStart).is_empty());
        // A reading before that frame's submission is still of its
        // simulation: held back, it opens nothing.
        assert_eq!(frames.sample(READER, 31), Sample::HeldBack);
        // Its token's submission ends the simulation the reading opened.
        assert_eq!(
            frames.submit(2, ORIGIN, 32).map(|(i, m)| (i, sent(m))),
            Some((2, vec![abi::SIMULATION_END, abi::RENDER_SUBMIT_START]))
        );
        assert_eq!(frames.sample(READER, 40), Sample::Open(3));
        // Its present shows that simulation: one index from the reading to
        // the present (frame generation's constants get it too).
        assert_eq!(present(&mut frames, 2, 2), (2, AT_PRESENT.to_vec()));
        let counts = frames.take_counts();
        assert_eq!((counts.begun, counts.adopted, counts.origin_sleeps, counts.unpaced), (2, 1, 1, 0));
        assert_eq!((counts.sampled, counts.opened, counts.belongs, counts.held_back, counts.other_thread), (1, 2, 1, 1, 0));
        assert_eq!((counts.by_token, counts.submitted_by_token, counts.lost, counts.stale), (2, 2, 0, 0));
        // The readings were on another thread than the origin wait's; the
        // submissions and presents on its own.
        assert_eq!(counts.openings_by_thread, [0, 2]);
        assert_eq!((counts.first_submits_by_thread, counts.presents_by_thread), ([2, 0], [2, 0]));
        assert_eq!(counts.origin_to_submit, Gap { count: 2, sum_us: 10 + 2, longest_us: 10 });
        assert_eq!(counts.submit_to_open, Gap { count: 2, sum_us: 10 + 8, longest_us: 10 });
        // (From the end of the reading's sleep.)
        assert_eq!(counts.open_to_adopt, Gap { count: 1, sum_us: 5, longest_us: 5 });

        // From here on every frame has its input sample, and the indexes
        // the host presents keep rising.
        let mut shown = 2;
        for origin in 3..40u64 {
            let now = origin * 1000;
            assert_eq!(sent(frames.sampled(origin as u32, now)), [abi::SIMULATION_START, abi::CONTROLLER_INPUT_SAMPLE]);
            let Begin::Adopted(index) = frames.begin(origin, ORIGIN, now + 100) else { panic!("not adopted") };
            assert!(frames.submit(origin, ORIGIN, now + 200).is_some_and(|(i, _)| i == index));
            assert!(matches!(frames.sample(READER, now + 300), Sample::Open(_)));
            let (presented, markers) = present(&mut frames, origin, origin);
            assert_eq!(markers, AT_PRESENT);
            assert!(presented > shown);
            shown = presented;
        }
        let counts = frames.take_counts();
        assert_eq!((counts.begun, counts.adopted, counts.sampled), (37, 37, 37));
        assert_eq!((counts.origin_sleeps, counts.unpaced, counts.held_back, counts.never_presented), (0, 0, 0, 0));
    }

    /// One title frame in the world's order, `length` microseconds from
    /// `now`: the origin wait, the frame's submission, then (if `reads`) the
    /// reading of the next simulation and its sleep. What the origin wait
    /// gave, and the index the host presented.
    fn frame(frames: &mut Frames, origin: u64, now: u64, reads: bool) -> (Begin, u32) {
        let begin = frames.begin(origin, ORIGIN, now);
        if let Begin::Paced(index) | Begin::Unpaced(index) = begin {
            assert_eq!(sent(frames.reach(index, Stage::SimulationStart)), [abi::SIMULATION_START]);
        }
        frames.submit(origin, ORIGIN, now + 100);
        if reads {
            let Sample::Open(opened) = frames.sample(READER, now + 200) else { panic!("not opened") };
            assert_eq!(sent(frames.sampled(opened, now + 300)), [abi::SIMULATION_START, abi::CONTROLLER_INPUT_SAMPLE]);
        }
        let (index, markers) = present(frames, origin, origin);
        assert_eq!(markers, AT_PRESENT);
        (begin, index)
    }

    #[test]
    fn a_missed_reading_leaves_its_frame_unpaced_while_the_title_reads_nearly_every_frame() {
        // The world: a reading every frame but one in fifteen.
        let mut frames = Frames::new();
        let mut shown = 0;
        for origin in 1..=40u64 {
            let (begin, index) = frame(&mut frames, origin, origin * 100_000, origin % 15 != 0);
            match origin {
                // Nothing read before the first frame: it sleeps at the
                // origin wait.
                1 => assert_eq!(begin, Begin::Paced(index)),
                // The frames after a missed reading have no simulation to
                // take, and most frames before them took one: no sleep.
                16 | 31 => assert_eq!(begin, Begin::Unpaced(index)),
                _ => assert_eq!(begin, Begin::Adopted(index)),
            }
            assert!(index > shown);
            shown = index;
        }
        let counts = frames.take_counts();
        assert_eq!((counts.begun, counts.adopted, counts.unpaced, counts.origin_sleeps), (40, 37, 2, 1));
    }

    #[test]
    fn a_menu_that_reads_every_tenth_frame_keeps_its_sleep_at_the_origin() {
        // 400 frames a second, a reading after the submission of every
        // tenth frame: the frame after it takes the simulation, every other
        // frame sleeps at the origin wait as it did before readings were
        // looked at. None goes without a sleep.
        let mut frames = Frames::new();
        for origin in 1..=200u64 {
            let (begin, index) = frame(&mut frames, origin, origin * 2_500, origin % 10 == 0);
            if origin > 10 && origin % 10 == 1 {
                assert_eq!(begin, Begin::Adopted(index));
            } else {
                assert_eq!(begin, Begin::Paced(index));
            }
        }
        let counts = frames.take_counts();
        assert_eq!((counts.begun, counts.adopted, counts.unpaced, counts.origin_sleeps), (200, 19, 0, 181));
        // Every frame had one sleep, at the reading or at the origin wait.
        assert_eq!(counts.adopted + counts.origin_sleeps, counts.begun);

        // Readings on every other frame: still not most of them, so still
        // no frame without a sleep.
        for origin in 201..=260u64 {
            frame(&mut frames, origin, origin * 2_500, origin % 2 == 0);
        }
        let counts = frames.take_counts();
        assert_eq!((counts.adopted, counts.unpaced, counts.origin_sleeps), (30, 0, 30));
    }

    #[test]
    fn world_frames_of_300_to_400_ms_still_take_the_simulation_their_reading_opened() {
        // The world's slowest stretches: the origin wait that takes a
        // simulation comes a whole frame after its reading.
        let mut frames = Frames::new();
        let first = paced(&mut frames, 1, 0);
        frames.submit(1, ORIGIN, 1_000);
        let Sample::Open(mut opened) = frames.sample(READER, 2_000) else { panic!("not opened") };
        assert_eq!(sent(frames.sampled(opened, 3_000)), [abi::SIMULATION_START, abi::CONTROLLER_INPUT_SAMPLE]);
        // A later reading of the same simulation is part of it.
        assert_eq!(frames.sample(READER, 200_000), Sample::Belongs);
        let mut shown = present(&mut frames, 1, 1).0;
        assert_eq!(shown, first);
        let mut now = 0;
        for (n, length) in [300_000u64, 370_000, 400_000, 330_000, 390_000, 360_000].into_iter().enumerate() {
            let origin = n as u64 + 2;
            now += length;
            assert_eq!(frames.begin(origin, ORIGIN, now), Begin::Adopted(opened));
            assert_eq!(
                frames.submit(origin, ORIGIN, now + 1_000).map(|(i, m)| (i, sent(m))),
                Some((opened, vec![abi::SIMULATION_END, abi::RENDER_SUBMIT_START]))
            );
            let Sample::Open(next) = frames.sample(READER, now + 2_000) else { panic!("not opened") };
            assert_eq!(sent(frames.sampled(next, now + 3_000)), [abi::SIMULATION_START, abi::CONTROLLER_INPUT_SAMPLE]);
            // Late in the frame, more than 250 ms after the opening: part
            // of it, not a second sleep.
            assert_eq!(frames.sample(READER, now + 290_000), Sample::Belongs);
            let (presented, markers) = present(&mut frames, origin, origin);
            assert_eq!((presented, markers), (opened, AT_PRESENT.to_vec()));
            assert!(presented > shown);
            shown = presented;
            opened = next;
        }
        // The bound follows the frames: four of the last interval.
        assert_eq!(frames.stale_after_us(), 4 * 360_000);
        let counts = frames.take_counts();
        assert_eq!((counts.begun, counts.adopted, counts.unpaced, counts.origin_sleeps), (7, 6, 0, 1));
        assert_eq!((counts.opened, counts.sampled, counts.belongs, counts.stale, counts.lost), (7, 7, 7, 0, 0));
    }

    #[test]
    fn with_no_reading_at_all_every_frame_sleeps_at_the_origin() {
        // Loads, cutscenes, menus that read nothing: as before readings
        // were looked at.
        let mut frames = Frames::new();
        for origin in 1..=20 {
            let index = paced(&mut frames, origin, origin * 1000);
            assert_eq!(index, origin as u32);
            frames.submit(origin, ORIGIN, origin * 1000 + 1);
            present(&mut frames, origin, origin);
        }
        let counts = frames.take_counts();
        assert_eq!((counts.begun, counts.origin_sleeps, counts.adopted, counts.unpaced, counts.opened), (20, 20, 0, 0, 0));
    }

    #[test]
    fn readings_on_two_threads_open_on_one() {
        let mut frames = Frames::new();
        paced(&mut frames, 1, 0);
        frames.submit(1, ORIGIN, 0);
        assert_eq!(frames.sample(READER, 0), Sample::Open(2));
        // Another thread's reading opens nothing and never sleeps, while a
        // simulation is open and after.
        assert_eq!(frames.sample(OTHER, 0), Sample::OtherThread);
        frames.sampled(2, 0);
        present(&mut frames, 1, 1);
        assert_eq!(frames.begin(2, ORIGIN, 0), Begin::Adopted(2));
        frames.submit(2, ORIGIN, 0);
        assert_eq!(frames.sample(OTHER, 0), Sample::OtherThread);
        present(&mut frames, 2, 2);
        // The reading thread stops reading: after HOLD frames the other
        // thread may open (the frame before it slept at the origin wait).
        for origin in 3..=u64::from(HOLD) + 1 {
            frames.begin(origin, ORIGIN, 0);
            frames.reach(origin as u32, Stage::SimulationStart);
            frames.submit(origin, ORIGIN, 0);
            assert_eq!(frames.sample(OTHER, 0), Sample::OtherThread);
            present(&mut frames, origin, origin);
        }
        let origin = u64::from(HOLD) + 2;
        paced(&mut frames, origin, 0);
        frames.submit(origin, ORIGIN, 0);
        assert!(matches!(frames.sample(OTHER, 0), Sample::Open(_)));
        // And now the first thread is the other one.
        assert_eq!(frames.sample(READER, 0), Sample::OtherThread);
        let counts = frames.take_counts();
        assert_eq!((counts.opened, counts.other_thread), (2, 2 + HOLD as u32 - 1 + 1));
    }

    #[test]
    fn a_simulation_no_frame_takes_is_let_go_at_a_later_reading() {
        // Frames 10 ms apart: the bound is its floor, 250 ms.
        let mut frames = Frames::new();
        for origin in 1..=2 {
            paced(&mut frames, origin, origin * 10_000);
            frames.submit(origin, ORIGIN, origin * 10_000 + 1);
            present(&mut frames, origin, origin);
        }
        assert_eq!(frames.stale_after_us(), STALE_US);
        assert_eq!(frames.sample(READER, 30_000), Sample::Open(3));
        frames.sampled(3, 30_000);
        // The title reads on and begins no frame (a load): the simulation
        // stays open within the bound ...
        assert_eq!(frames.sample(READER, 30_000 + STALE_US), Sample::Belongs);
        // ... and a reading after it lets it go and opens another, with a
        // sleep of its own.
        assert_eq!(frames.sample(READER, 30_001 + STALE_US), Sample::Open(4));
        assert!(frames.reach(3, Stage::PresentEnd).is_empty());
        assert_eq!(sent(frames.sampled(4, 30_001 + STALE_US)), [abi::SIMULATION_START, abi::CONTROLLER_INPUT_SAMPLE]);
        // The origin wait takes the open simulation however long after it
        // comes: no time bound there.
        assert_eq!(frames.begin(3, ORIGIN, 5_000_000), Begin::Adopted(4));
        let counts = frames.take_counts();
        assert_eq!((counts.stale, counts.opened, counts.belongs, counts.adopted), (1, 2, 1, 1));
        // That gap is now the interval the next bound is made of.
        assert_eq!(frames.stale_after_us(), STALE_FRAMES * (5_000_000 - 20_000));
    }

    #[test]
    fn presents_the_title_began_no_frame_for_keep_the_indexes_rising() {
        // What frame generation's constants, tags and present markers see:
        // the host's presents in order, never an index older than one shown.
        let mut frames = Frames::new();
        let mut shown = Vec::new();
        let first = paced(&mut frames, 1, 0);
        frames.submit(1, ORIGIN, 10);
        assert_eq!(frames.sample(READER, 20), Sample::Open(2));
        frames.sampled(2, 21);
        // The title presents frame 1 twice (one token, two presents).
        frames.title_presents(1, 1, ORIGIN);
        frames.title_presents(2, 1, ORIGIN);
        let (index, markers) = host_presents(&mut frames, 1);
        assert_eq!((index, markers), (first, AT_PRESENT.to_vec()));
        shown.push(index);
        // The second shows frame 1 again: its index, and no marker twice.
        let (index, markers) = host_presents(&mut frames, 2);
        assert_eq!((index, markers), (first, vec![]));
        shown.push(index);
        // The open simulation is still the next frame's.
        assert_eq!(frames.begin(2, ORIGIN, 30), Begin::Adopted(2));
        frames.submit(2, ORIGIN, 31);
        assert_eq!(frames.sample(READER, 32), Sample::Open(3));
        frames.sampled(3, 33);
        let (index, markers) = present(&mut frames, 3, 2);
        assert_eq!((index, markers), (2, AT_PRESENT.to_vec()));
        shown.push(index);
        // A present with no token and no frame begun for it, while the
        // simulation 3 is open: it takes that simulation, whose start and
        // input sample went at its reading, and completes it.
        frames.title_presents(4, 0, ORIGIN);
        assert_eq!(frames.presenting(4), 3);
        assert_eq!(
            sent(frames.reach(3, Stage::PresentStart)),
            [abi::SIMULATION_END, abi::RENDER_SUBMIT_START, abi::RENDER_SUBMIT_END, abi::PRESENT_START]
        );
        assert_eq!(sent(frames.presented(3)), [abi::PRESENT_END]);
        shown.push(3);
        // The next origin wait has no simulation to take: a newer index.
        let Begin::Paced(next) = frames.begin(3, ORIGIN, 40) else { panic!("not paced") };
        frames.reach(next, Stage::SimulationStart);
        frames.submit(3, ORIGIN, 41);
        let (index, markers) = present(&mut frames, 5, 3);
        assert_eq!((index, markers), (next, AT_PRESENT.to_vec()));
        shown.push(index);
        assert_eq!(shown, [1, 1, 2, 3, 4]);
        assert!(shown.windows(2).all(|pair| pair[0] <= pair[1]));
        let counts = frames.take_counts();
        assert_eq!((counts.unbegun, counts.adopted, counts.stale), (2, 1, 0));
    }

    #[test]
    fn a_submission_while_the_reading_sleeps_does_not_end_its_simulation() {
        let mut frames = Frames::new();
        paced(&mut frames, 1, 0);
        frames.submit(1, ORIGIN, 0);
        assert_eq!(frames.sample(READER, 0), Sample::Open(2));
        present(&mut frames, 1, 1);
        // The origin wait takes the simulation while its reading still
        // sleeps, and the frame submits at once: that submission is of a
        // simulation made before (the title is deeper than one frame here).
        assert_eq!(frames.begin(2, ORIGIN, 0), Begin::Adopted(2));
        assert_eq!(frames.submit(2, ORIGIN, 0), None);
        // Another thread's reading meanwhile opens nothing.
        assert_eq!(frames.sample(OTHER, 0), Sample::OtherThread);
        // The sleep returns: start and input sample. A further reading of
        // the simulation opens no other (its frame's simulation has not
        // ended), and the next submission ends it.
        assert_eq!(sent(frames.sampled(2, 0)), [abi::SIMULATION_START, abi::CONTROLLER_INPUT_SAMPLE]);
        assert_eq!(frames.sample(READER, 0), Sample::HeldBack);
        assert_eq!(
            frames.submit(2, ORIGIN, 0).map(|(i, m)| (i, sent(m))),
            Some((2, vec![abi::SIMULATION_END, abi::RENDER_SUBMIT_START]))
        );
        let counts = frames.take_counts();
        assert_eq!((counts.submitted_before_start, counts.sampled, counts.lost), (1, 1, 0));
    }

    #[test]
    fn a_present_before_the_readings_sleep_returns_loses_the_sample() {
        let mut frames = Frames::new();
        paced(&mut frames, 1, 0);
        frames.submit(1, ORIGIN, 0);
        assert_eq!(frames.sample(READER, 0), Sample::Open(2));
        present(&mut frames, 1, 1);
        assert_eq!(frames.begin(2, ORIGIN, 0), Begin::Adopted(2));
        // The frame is presented by its token although its simulation has
        // not started: it sends everything it owes, in order.
        frames.title_presents(2, 2, ORIGIN);
        assert_eq!(frames.presenting(2), 2);
        assert_eq!(
            sent(frames.reach(2, Stage::PresentStart)),
            [abi::SIMULATION_START, abi::SIMULATION_END, abi::RENDER_SUBMIT_START, abi::RENDER_SUBMIT_END, abi::PRESENT_START]
        );
        // The sleep returns too late for an input sample.
        assert!(frames.sampled(2, 0).is_empty());
        assert_eq!(sent(frames.presented(2)), [abi::PRESENT_END]);
        assert!(frames.sampled(2, 0).is_empty());
        let counts = frames.take_counts();
        assert_eq!((counts.sampled, counts.lost, counts.by_token, counts.unsubmitted), (0, 2, 2, 1));
    }

    #[test]
    fn lookups_by_order_never_take_an_open_simulation() {
        let mut frames = Frames::new();
        let first = paced(&mut frames, 1, 0);
        frames.submit(1, ORIGIN, 0);
        assert_eq!(frames.sample(READER, 0), Sample::Open(2));
        frames.sampled(2, 0);
        // An untagged submission and an untagged present take the newest
        // frame an origin wait has, not the open simulation, although its
        // simulation runs.
        assert_eq!(frames.submit(0, ORIGIN, 0), None);
        frames.title_presents(1, 0, ORIGIN);
        assert_eq!(frames.presenting(1), first);
        assert_eq!(frames.begin(2, ORIGIN, 0), Begin::Adopted(2));
        let counts = frames.take_counts();
        assert_eq!((counts.by_order, counts.submitted_by_order), (1, 0));
    }

    #[test]
    fn a_frame_that_never_submits_is_completed_at_its_present() {
        let mut frames = Frames::new();
        let frame = paced(&mut frames, 1, 0);
        // (No token in the present's plane: the frame is found by order.)
        frames.title_presents(1, 0, ORIGIN);
        assert_eq!(frames.presenting(1), frame);
        assert_eq!(
            sent(frames.reach(frame, Stage::SubmitEnd)),
            [abi::SIMULATION_END, abi::RENDER_SUBMIT_START, abi::RENDER_SUBMIT_END]
        );
        assert_eq!(sent(frames.reach(frame, Stage::PresentStart)), [abi::PRESENT_START]);
        assert_eq!(sent(frames.presented(frame)), [abi::PRESENT_END]);
        let counts = frames.take_counts();
        assert_eq!((counts.unsubmitted, counts.by_order, counts.by_token), (1, 1, 0));
    }

    #[test]
    fn a_present_with_no_frame_begun_gets_one_of_its_own() {
        let mut frames = Frames::new();
        // (As the check's presents are: no origin wait, no title present.)
        let frame = frames.presenting(5);
        assert_eq!(
            sent(frames.reach(frame, Stage::PresentStart)),
            [abi::SIMULATION_START, abi::SIMULATION_END, abi::RENDER_SUBMIT_START, abi::RENDER_SUBMIT_END, abi::PRESENT_START]
        );
        assert_eq!(sent(frames.presented(frame)), [abi::PRESENT_END]);
        // The next one has another index.
        assert_eq!(frames.presenting(6), frame + 1);
        // A second present of a frame's token shows that frame again: its
        // index, with no marker sent twice and no frame made for it.
        let begun = paced(&mut frames, 9, 0);
        frames.title_presents(7, 9, ORIGIN);
        frames.title_presents(8, 9, ORIGIN);
        assert_eq!(host_presents(&mut frames, 7).0, begun);
        assert_eq!(host_presents(&mut frames, 8), (begun, vec![]));
        // Only once: a later present of that number is one of its own.
        assert_eq!(frames.presenting(8), begun + 1);
        assert_eq!(frames.take_counts().unbegun, 4);
    }

    #[test]
    fn submissions_and_presents_find_their_frame() {
        let mut frames = Frames::new();
        // The title is two frames deep: 10 is being submitted while 11 is
        // simulated, and 12 is in its sleep.
        let [a, b] = [10, 11].map(|origin| paced(&mut frames, origin, 0));
        let Begin::Paced(c) = frames.begin(12, ORIGIN, 0) else { panic!("not paced") };
        // A tagged submission is its token's frame's, an untagged one the
        // newest running frame's; neither is the sleeping frame's.
        assert_eq!(frames.submit(10, ORIGIN, 0).map(|(i, _)| i), Some(a));
        assert_eq!(frames.submit(0, ORIGIN, 0).map(|(i, _)| i), Some(b));
        assert_eq!(frames.submit(12, ORIGIN, 0), None);
        // The presents arrive on the host after the title called both.
        frames.title_presents(1, 10, ORIGIN);
        frames.title_presents(2, 11, ORIGIN);
        assert_eq!((frames.presenting(1), frames.presenting(2)), (a, b));
        assert_eq!(sent(frames.reach(a, Stage::PresentStart)), [abi::RENDER_SUBMIT_END, abi::PRESENT_START]);
        assert_eq!(sent(frames.presented(a)), [abi::PRESENT_END]);
        // The third frame's simulation starts only now.
        assert_eq!(sent(frames.reach(c, Stage::SimulationStart)), [abi::SIMULATION_START]);
        assert_eq!(frames.submit(0, ORIGIN, 0).map(|(i, _)| i), Some(c));
        let counts = frames.take_counts();
        assert_eq!(counts.by_token, 2);
        // The first frame's simulation was ended by its token, the other
        // two's by order.
        assert_eq!((counts.submitted_by_token, counts.submitted_by_order), (1, 2));
    }

    #[test]
    fn a_submission_with_a_token_no_frame_has_is_taken_by_order() {
        let mut frames = Frames::new();
        let frame = paced(&mut frames, 5, 0);
        // (A token of a frame that is over, or one the title makes itself.)
        assert_eq!(frames.submit(99, ORIGIN, 0).map(|(i, _)| i), Some(frame));
        let counts = frames.take_counts();
        assert_eq!((counts.submitted_by_token, counts.submitted_by_order), (0, 1));
        // With no frame running there is none to take, and none is counted.
        frames.title_presents(1, 5, ORIGIN);
        assert_eq!(frames.submit(0, ORIGIN, 0), None);
        assert_eq!(frames.take_counts().submitted_by_order, 0);
    }

    #[test]
    fn a_sleep_is_fine_long_or_the_last() {
        assert_eq!(slept(0), Slept::Fine);
        assert_eq!(slept(16_000), Slept::Fine);
        assert_eq!(slept(SLEEP_LONG_US), Slept::Fine);
        assert_eq!(slept(SLEEP_LONG_US + 1), Slept::Long);
        assert_eq!(slept(SLEEP_TOO_LONG_US), Slept::Long);
        assert_eq!(slept(SLEEP_TOO_LONG_US + 1), Slept::TooLong);
        assert_eq!(slept(u64::MAX), Slept::TooLong);
        // The second bound is the larger, and far below the 30 seconds a
        // standing frame is given.
        const { assert!(SLEEP_LONG_US < SLEEP_TOO_LONG_US && SLEEP_TOO_LONG_US < 30_000_000) };
    }

    #[test]
    fn frames_never_presented_are_let_go() {
        let mut frames = Frames::new();
        let begun: Vec<u32> = (1..=3).map(|origin| paced(&mut frames, origin, 0)).collect();
        // The title presents the third: the two before it are over.
        frames.title_presents(1, 3, ORIGIN);
        assert_eq!(frames.take_counts().never_presented, 2);
        assert!(frames.reach(begun[0], Stage::PresentEnd).is_empty());
        assert_eq!(frames.presenting(1), begun[2]);
        // And however many frames begin, few are kept.
        for origin in 4..40 {
            frames.begin(origin, ORIGIN, 0);
        }
        assert_eq!(frames.live.len(), KEPT);
        assert_eq!(frames.take_counts().never_presented, 36 + 1 - KEPT as u32);
    }

    #[test]
    fn the_reports_average_over_their_frames() {
        let mut reports = [abi::ReflexReport::empty(); 4];
        assert_eq!(latency(&reports), None);
        for (frame, total, gpu) in [(0usize, 30_000u64, 9_000u32), (1, 50_000, 11_000)] {
            reports[frame].frame_id = frame as u64 + 1;
            reports[frame].sim_start_time = 1_000_000;
            reports[frame].gpu_render_end_time = 1_000_000 + total;
            reports[frame].gpu_frame_time_us = gpu;
            reports[frame].gpu_active_render_time_us = gpu / 2;
        }
        // A report whose rendering has no end yet does not count.
        reports[2].frame_id = 3;
        reports[2].sim_start_time = 2_000_000;
        assert_eq!(
            latency(&reports),
            Some(Latency { frames: 2, game_to_render: 40_000, gpu_frame_us: 10_000, gpu_active_render_us: 5_000 })
        );
    }
}
