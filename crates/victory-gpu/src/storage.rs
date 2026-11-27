//! Backing identity and observed writes shared by every virtual view.
//!
//! This extends the in-place mapping census; it is not a proof that every CPU
//! writer has been intercepted. In particular an unguarded physical alias can
//! still be written without a fault. Observed versions may invalidate a cache,
//! but must never justify omitting the existing refresh/publication checks.
//! This foundation provides identity/lifecycle metadata only: no GPU ownership,
//! native page protection, materialization, or eager-readback suppression.

use std::cell::Cell;
#[cfg(test)]
#[path = "storage_native_tests.rs"]
mod native_tests;
use std::collections::BTreeMap;
use std::sync::{
    Mutex, MutexGuard, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

const FAULT_CAPACITY: usize = 4096;
static FAULT_OVERFLOW: AtomicBool = AtomicBool::new(false);
static FAULT_ENABLED: AtomicBool = AtomicBool::new(false);
static MAPPING_EPOCH: AtomicU64 = AtomicU64::new(0);
// Written under REGISTRY whenever its VA identity changes. A fault samples
// this without taking a lock; tombstones retain removals as well as new maps.
static MAPPING_SERIAL: AtomicU64 = AtomicU64::new(0);
const MAX_MAPPING_MARKERS: usize = 131072;
static FAULTS: FaultJournal<FAULT_CAPACITY> = FaultJournal::new();
// faults, queued, drained, capacity, observed odd VM, changed VA, deferred,
// conservative whole-ledger events, logical backing bytes dirtied, incomplete
// mapping coverage under an odd/changed global VM epoch.
static FAULT_STATS: [AtomicU64; 10] = [const { AtomicU64::new(0) }; 10];

struct FaultSlot {
    published: AtomicUsize,
    consumed: AtomicUsize,
    block: AtomicUsize,
    epoch: AtomicU64,
    vm_epoch: AtomicU64,
}

impl FaultSlot {
    const fn new() -> Self {
        Self {
            published: AtomicUsize::new(0),
            consumed: AtomicUsize::new(0),
            block: AtomicUsize::new(0),
            epoch: AtomicU64::new(0),
            vm_epoch: AtomicU64::new(0),
        }
    }
}

/// Bounded MPSC journal. Only the REGISTRY owner consumes. Reserving a slot
/// does not publish its payload; an interrupted producer never blocks VEH
/// writers or makes a consumer read a half-written event.
struct FaultJournal<const N: usize> {
    head: AtomicUsize,
    tail: AtomicUsize,
    slots: [FaultSlot; N],
}

#[derive(Default)]
struct FaultDrain {
    events: usize,
    deferred: bool,
    mismatched: u64,
    uncovered: u64,
    conservative: bool,
    bytes: u64,
}

impl<const N: usize> FaultJournal<N> {
    const fn new() -> Self {
        assert!(N.is_power_of_two());
        Self {
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            slots: [const { FaultSlot::new() }; N],
        }
    }

    fn reserve(&self) -> Option<usize> {
        let mut tail = self.tail.load(Ordering::Relaxed);
        loop {
            if tail.wrapping_sub(self.head.load(Ordering::Acquire)) >= N {
                // A consumer may already have passed our stale tail snapshot.
                // Refresh it before reporting actual reservation exhaustion.
                let now = self.tail.load(Ordering::Acquire);
                if now != tail {
                    tail = now;
                    continue;
                }
                return None;
            }
            match self.tail.compare_exchange(
                tail,
                tail.wrapping_add(1),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(tail),
                // Retry only competing reservation progress. Never wait for
                // a producer to publish or for the consumer to free a slot.
                Err(now) => tail = now,
            }
        }
    }

    fn publish(&self, ticket: usize, block: usize, epoch: u64) {
        self.publish_observed(ticket, block, epoch, 0);
    }

    fn publish_observed(&self, ticket: usize, block: usize, epoch: u64, vm_epoch: u64) {
        let slot = &self.slots[ticket % N];
        slot.block.store(block, Ordering::Relaxed);
        slot.epoch.store(epoch, Ordering::Relaxed);
        slot.vm_epoch.store(vm_epoch, Ordering::Relaxed);
        slot.published
            .store(ticket.wrapping_add(1), Ordering::Release);
    }

    fn push(&self, block: usize, epoch: u64) -> bool {
        self.push_observed(block, epoch, 0)
    }

    fn push_observed(&self, block: usize, epoch: u64, vm_epoch: u64) -> bool {
        let Some(ticket) = self.reserve() else {
            return false;
        };
        self.publish_observed(ticket, block, epoch, vm_epoch);
        true
    }

    /// Consume every published event in the bounded reservation snapshot,
    /// including behind an interrupted producer. Such a later VEH can already
    /// have returned and its CPU store must invalidate aliases immediately.
    /// Only a contiguous consumed prefix permits slot reuse.
    fn drain(&self, mut consume: impl FnMut(usize, u64, u64)) -> (usize, bool) {
        let end = self.tail.load(Ordering::Acquire);
        let mut head = self.head.load(Ordering::Relaxed);
        let mut cursor = head;
        let mut drained = 0;
        while cursor != end {
            let slot = &self.slots[cursor % N];
            let sequence = cursor.wrapping_add(1);
            if slot.published.load(Ordering::Acquire) == sequence
                && slot.consumed.load(Ordering::Relaxed) != sequence
            {
                consume(
                    slot.block.load(Ordering::Relaxed),
                    slot.epoch.load(Ordering::Relaxed),
                    slot.vm_epoch.load(Ordering::Relaxed),
                );
                slot.consumed.store(sequence, Ordering::Relaxed);
                drained += 1;
            }
            cursor = cursor.wrapping_add(1);
        }
        while head != end
            && self.slots[head % N].consumed.load(Ordering::Relaxed) == head.wrapping_add(1)
        {
            head = head.wrapping_add(1);
        }
        // Publication of head is the only permission to reuse any slot. All
        // payload reads and consumed marks finish before new producer claims.
        self.head.store(head, Ordering::Release);
        (drained, head != end)
    }
}

/// VEH-safe: no allocation, lock acquisition, VM operation or waiting for
/// another thread. Record during a VM writer too; content consumers cannot
/// drain until its lifecycle writer and all mapping post-hooks have finished.
pub(crate) fn faulted(at: usize) {
    // The gate is initialized outside VEH by normal metadata/refresh calls.
    // Never initialize an environment/OnceLock path in an exception handler.
    if !FAULT_ENABLED.load(Ordering::Acquire) || GPU_PUBLICATION.with(|depth| depth.get() != 0) {
        return;
    }
    FAULT_STATS[0].fetch_add(1, Ordering::Relaxed);
    let epoch = MAPPING_EPOCH.load(Ordering::Acquire);
    if epoch & 1 != 0 {
        FAULT_STATS[4].fetch_add(1, Ordering::Relaxed);
    }
    let serial = MAPPING_SERIAL.load(Ordering::Acquire);
    if FAULTS.push_observed(at / 65536 * 65536, serial, epoch) {
        FAULT_STATS[1].fetch_add(1, Ordering::Relaxed);
    } else {
        FAULT_STATS[3].fetch_add(1, Ordering::Relaxed);
        FAULT_OVERFLOW.store(true, Ordering::Release);
    }
}

pub(crate) fn line(frames: f64) {
    if !enabled() {
        return;
    }
    let stats = FAULT_STATS.each_ref().map(|n| n.swap(0, Ordering::Relaxed));
    tracing::info!(
        faults = stats[0],
        queued = stats[1],
        drained = stats[2],
        capacity = stats[3],
        odd_epoch = stats[4],
        changed_mapping = stats[5],
        uncovered_mapping = stats[9],
        deferred = stats[6],
        whole_ledger_events = stats[7],
        whole_ledger_mib_per_frame = stats[8] as f64 / (1024.0 * 1024.0) / frames,
        "storage fault journal: observation events; whole-ledger bytes are logical invalidation work"
    );
}

pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    let on =
        *ON.get_or_init(|| std::env::var_os("VICTORY_SHARED_STORAGE").is_some_and(|v| v == "1"));
    if on {
        FAULT_ENABLED.store(true, Ordering::Release);
    }
    on
}

/// Cache-invalidation hint only; an unchanged value never proves ownership.
pub(crate) fn mapping_epoch() -> u64 {
    MAPPING_EPOCH.load(Ordering::Acquire)
}

// Retention materialization -> lifecycle -> brief registry metadata -> Shadow
// reference/journal locks. No GPU waits under lifecycle, no VM calls under
// registry. Native mutation callbacks already hold the writer and do not
// reacquire it. Null-base allocations use the same writer.
static LIFECYCLE: RwLock<()> = RwLock::new(());
// Temporary physical views pin native identity here, not in LIFECYCLE:
// a decoder may read a different retained guest source and demand it. VM
// writers wait here BEFORE holding LIFECYCLE, so that demand can publish.
static NATIVE_ACCESS: RwLock<()> = RwLock::new(());
pub(crate) struct Mutation {
    guard: Option<RwLockWriteGuard<'static, ()>>,
    // Drop the lifecycle writer first, then admit another native reader.
    _native: Option<RwLockWriteGuard<'static, ()>>,
}

pub(crate) fn mutation() -> Mutation {
    if !enabled() {
        return Mutation {
            guard: None,
            _native: None,
        };
    }
    let native = NATIVE_ACCESS.write().unwrap_or_else(|e| e.into_inner());
    let guard = LIFECYCLE.write().unwrap_or_else(|e| e.into_inner());
    // Attribute old VA fault events while the old mapping still exists.
    drop(registry());
    MAPPING_EPOCH.fetch_add(1, Ordering::AcqRel); // odd throughout VM/post-hooks
    Mutation {
        guard: Some(guard),
        _native: Some(native),
    }
}

impl Drop for Mutation {
    fn drop(&mut self) {
        if self.guard.is_some() {
            // Writer remains held until after Drop's body and the post-hooks.
            MAPPING_EPOCH.fetch_add(1, Ordering::Release);
        }
    }
}

pub(crate) fn publication() -> Option<RwLockReadGuard<'static, ()>> {
    enabled().then(|| LIFECYCLE.read().unwrap_or_else(|e| e.into_inner()))
}

/// Pin native mappings before taking a lifecycle/reference/ownership lock.
/// Never retain this across a GPU wait. VM mutation takes native then lifecycle.
pub(crate) fn native_reading() -> Option<RwLockReadGuard<'static, ()>> {
    enabled().then(|| NATIVE_ACCESS.read().unwrap_or_else(|e| e.into_inner()))
}
pub(crate) fn native_reading_now() -> Option<RwLockReadGuard<'static, ()>> {
    if !enabled() {
        return None;
    }
    match NATIVE_ACCESS.try_read() {
        Ok(guard) => Some(guard),
        Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

thread_local! { static GPU_PUBLICATION: Cell<u32> = const { Cell::new(0) }; }

pub(crate) struct GpuPublication;

pub(crate) fn gpu_publication_origin() -> GpuPublication {
    GPU_PUBLICATION.with(|depth| depth.set(depth.get() + 1));
    GpuPublication
}

impl Drop for GpuPublication {
    fn drop(&mut self) {
        GPU_PUBLICATION.with(|depth| depth.set(depth.get() - 1));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BackingToken(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Provenance {
    PrivateReservation,
    PhysicalSection,
}

/// Piecewise-constant versions, in backing byte coordinates.
#[derive(Default)]
struct Versions(BTreeMap<u64, u64>);

impl Versions {
    fn at(&self, at: u64) -> u64 {
        self.0.range(..=at).next_back().map_or(0, |(_, &v)| v)
    }

    fn set(&mut self, lo: u64, hi: u64, version: u64) {
        if lo >= hi {
            return;
        }
        let after = self.at(hi);
        let gone: Vec<_> = self.0.range(lo..hi).map(|(&at, _)| at).collect();
        for at in gone {
            self.0.remove(&at);
        }
        self.0.insert(lo, version);
        self.0.insert(hi, after);
    }

    fn maximum(&self, lo: u64, hi: u64) -> u64 {
        self.0
            .range(lo..hi)
            .fold(self.at(lo), |v, (_, &next)| v.max(next))
    }

    fn changed(&self, lo: u64, hi: u64, since: u64) -> Vec<(u64, u64)> {
        self.runs(lo, hi)
            .into_iter()
            .filter_map(|(lo, hi, v)| (v > since).then_some((lo, hi)))
            .collect()
    }

    fn runs(&self, lo: u64, hi: u64) -> Vec<(u64, u64, u64)> {
        let mut result = Vec::new();
        let (mut at, mut version) = (lo, self.at(lo));
        for (&next, &v) in self
            .0
            .range((std::ops::Bound::Excluded(lo), std::ops::Bound::Excluded(hi)))
        {
            result.push((at, next, version));
            (at, version) = (next, v);
        }
        if at < hi {
            result.push((at, hi, version));
        }
        result
    }
}

struct Backing {
    bytes: u64,
    provenance: Provenance,
    live: bool,
    versions: Versions,
    incarnations: Versions,
    // A temporary native view whose unmap failed is an unknown live alias.
    // No later physical access/admission may mistake this census for closed.
    temporary_alias_escaped: bool,
}

#[derive(Clone, Copy)]
struct Binding {
    len: usize,
    backing: BackingToken,
    offset: u64,
    epoch: u64,
}

/// The single mapping registry also owns the legacy in-place census indexes.
/// The indexes and identity bindings are mutated under this same lock.
#[derive(Default)]
pub(crate) struct Registry {
    pub(crate) views: BTreeMap<usize, (usize, usize, u64)>,
    pub(crate) sections: Vec<(usize, u64, bool)>,
    /// Legacy alias indexes share the canonical identity lock.
    pub(crate) pages: BTreeMap<(usize, u64), Vec<usize>>,
    pub(crate) pages_twice: usize,
    /// Privileged native whole-section views. Removal returns their base so
    /// the bridge can unmap outside this metadata lock.
    pub(crate) whole: Vec<(usize, usize)>,
    handles: BTreeMap<usize, BackingToken>,
    backings: BTreeMap<BackingToken, Backing>,
    bindings: BTreeMap<usize, Binding>,
    reservations: BTreeMap<usize, (usize, BackingToken)>,
    mapping_versions: Versions,
    mapping_overflow: bool,
    next: u64,
    clock: u64,
}

impl Registry {
    fn mapping_changed(&mut self, at: usize, len: usize) {
        let Some(end) = at.checked_add(len) else {
            self.mapping_overflow = true;
            return;
        };
        if end == at {
            return;
        }
        let serial = MAPPING_SERIAL
            .fetch_add(1, Ordering::AcqRel)
            .checked_add(1)
            .unwrap_or_else(|| {
                crate::host::fail(format_args!("mapping observation serial exhausted"))
            });
        if self.mapping_overflow {
            return;
        }
        if self.mapping_versions.0.len() >= MAX_MAPPING_MARKERS {
            // Losing a tombstone must never bless a delayed old-VA event.
            self.mapping_overflow = true;
            return;
        }
        self.mapping_versions.set(at as u64, end as u64, serial);
    }

    fn fault_mapping_unchanged(&self, at: usize, serial: u64) -> bool {
        !self.mapping_overflow
            && at
                .checked_add(65536)
                .is_some_and(|end| self.mapping_versions.maximum(at as u64, end as u64) <= serial)
    }

    fn fault_mapping_covered(&self, at: usize) -> bool {
        let Some(end) = at.checked_add(65536) else {
            return false;
        };
        let mut cursor = at;
        while cursor < end {
            let Some((&base, binding)) = self.bindings.range(..=cursor).next_back() else {
                return false;
            };
            let Some(stop) = base.checked_add(binding.len).filter(|stop| *stop > cursor) else {
                return false;
            };
            if !self
                .backings
                .get(&binding.backing)
                .is_some_and(|backing| backing.live)
            {
                return false;
            }
            cursor = stop.min(end);
        }
        true
    }

    /// Bindings never overlap: bind first removes/splits its old interval.
    /// One predecessor plus bases inside the query covers every possible hit.
    fn binding_candidates(
        &self,
        at: usize,
        end: usize,
    ) -> std::collections::btree_map::Range<'_, usize, Binding> {
        let first = if at == end {
            end
        } else {
            self.bindings
                .range(..=at)
                .next_back()
                .map_or(at, |(&base, _)| base)
        };
        self.bindings.range(first..end)
    }

    fn tick(&mut self) -> u64 {
        self.clock = self
            .clock
            .checked_add(1)
            .expect("storage version exhausted");
        self.clock
    }

    fn backing(&mut self, bytes: u64, provenance: Provenance) -> BackingToken {
        self.next = self
            .next
            .checked_add(1)
            .expect("storage identity exhausted");
        let token = BackingToken(self.next);
        self.backings.insert(
            token,
            Backing {
                bytes,
                provenance,
                live: true,
                versions: Versions::default(),
                incarnations: Versions::default(),
                temporary_alias_escaped: false,
            },
        );
        token
    }

    pub(crate) fn token_for_section(&self, handle: usize) -> Option<BackingToken> {
        self.handles.get(&handle).copied()
    }

    pub(crate) fn section(&mut self, handle: usize, bytes: u64, committed: bool) -> BackingToken {
        let token = self.backing(bytes, Provenance::PhysicalSection);
        self.handles.insert(handle, token);
        self.sections.retain(|s| s.0 != handle);
        // As in the existing census, never unmap a previous incarnation's
        // privileged view merely because its raw handle number was reused.
        self.whole.retain(|w| w.0 != handle);
        self.sections.push((handle, bytes, committed));
        token
    }

    // Identity remains valid for an existing pinned access even if another
    // temporary alias escapes. Eligibility to start NEW access is separate.
    fn physical_identity(
        &self,
        token: BackingToken,
        offset: u64,
        len: usize,
    ) -> Option<BackingRange> {
        let end = offset.checked_add(u64::try_from(len).ok()?)?;
        let backing = self.backings.get(&token)?;
        if len == 0
            || !backing.live
            || backing.provenance != Provenance::PhysicalSection
            || end > backing.bytes
        {
            return None;
        }
        Some(BackingRange {
            token,
            offset,
            len,
            incarnations: backing.incarnations.runs(offset, end),
        })
    }

    fn physical_range(&self, token: BackingToken, offset: u64, len: usize) -> Option<BackingRange> {
        let identity = self.physical_identity(token, offset, len)?;
        (!self.backings.get(&token)?.temporary_alias_escaped).then_some(identity)
    }

    fn temporary_written(&mut self, range: &BackingRange, within: usize, len: usize) -> bool {
        if within.checked_add(len).is_none_or(|end| end > range.len)
            || self
                .physical_identity(range.token, range.offset, range.len)
                .as_ref()
                != Some(range)
        {
            return false;
        }
        self.backing_written(range.token, range.offset + within as u64, len);
        true
    }

    // Metadata only. The epoch makes a removed/recreated alias different,
    // even when the replacement maps the same physical bytes at the same VA.
    fn physical_aliases(&self, range: &BackingRange) -> Option<Vec<(usize, usize, u64)>> {
        if self.physical_range(range.token, range.offset, range.len)? != *range {
            return None;
        }
        let end = range.offset + range.len as u64;
        Some(
            self.bindings
                .iter()
                .filter_map(|(&base, binding)| {
                    if binding.backing != range.token {
                        return None;
                    }
                    let lo = range.offset.max(binding.offset);
                    let hi = end.min(binding.offset + binding.len as u64);
                    (lo < hi).then(|| {
                        (
                            base + (lo - binding.offset) as usize,
                            (hi - lo) as usize,
                            binding.epoch,
                        )
                    })
                })
                .collect(),
        )
    }

    /// Remove whole intersecting legacy views, as the existing census does.
    /// The runtime re-notifies surviving native pieces after its VM operation.
    /// Canonical bindings must not outlive the alias index that described them.
    pub(crate) fn forget(&mut self, at: usize, len: usize) {
        let end = at.saturating_add(len);
        if at == end {
            return;
        }
        let first = self
            .views
            .range(..=at)
            .next_back()
            .filter(|(base, view)| base.saturating_add(view.0) > at)
            .map(|(&base, _)| base);
        let gone: Vec<_> = first
            .into_iter()
            .chain(
                self.views
                    .range(at.saturating_add(1)..end)
                    .map(|(&base, _)| base),
            )
            .collect();
        for base in gone {
            let Some((bytes, section, offset)) = self.views.remove(&base) else {
                continue;
            };
            self.forget_bindings(base, bytes);
            for k in 0..bytes.div_ceil(65536) {
                let key = (section, offset + (k * 65536) as u64);
                if let Some(pages) = self.pages.get_mut(&key) {
                    let page = base + k * 65536;
                    let before = pages.len();
                    pages.retain(|&shown| shown != page);
                    let after = pages.len();
                    if after == 0 {
                        self.pages.remove(&key);
                    }
                    self.pages_twice = self
                        .pages_twice
                        .saturating_sub(usize::from(before >= 2 && after < 2));
                }
            }
        }
        self.forget_bindings(at, len);
    }

    /// Private/non-view byte intervals retain their unaffected identity.
    fn forget_bindings(&mut self, at: usize, len: usize) {
        let end = at.saturating_add(len);
        if at == end {
            return;
        }
        let hit: Vec<_> = self
            .binding_candidates(at, end)
            .filter(|(base, b)| base.saturating_add(b.len) > at)
            .map(|(&base, &b)| (base, b))
            .collect();
        // A pre-unmap hook can precede a CPU store into the old native view.
        // Lifecycle readers stay excluded until the native operation ends;
        // dirty its physical bytes before dropping the VA identity so another
        // surviving alias cannot retain a pre-store upload watermark.
        for &(base, binding) in &hit {
            let lo = base.max(at);
            let hi = (base + binding.len).min(end);
            if lo < hi {
                self.backing_written(
                    binding.backing,
                    binding.offset + (lo - base) as u64,
                    hi - lo,
                );
            }
        }
        self.mapping_changed(at, len);
        for (base, binding) in hit {
            self.bindings.remove(&base);
            let stop = base + binding.len;
            if base < at {
                self.bindings.insert(
                    base,
                    Binding {
                        len: at - base,
                        ..binding
                    },
                );
            }
            if stop > end {
                self.bindings.insert(
                    end,
                    Binding {
                        len: stop - end,
                        offset: binding.offset + (end - base) as u64,
                        ..binding
                    },
                );
            }
        }
    }

    fn bind(&mut self, at: usize, len: usize, token: BackingToken, offset: u64) {
        let valid = at.checked_add(len).is_some()
            && self.backings.get(&token).is_some_and(|b| {
                b.live
                    && offset
                        .checked_add(len as u64)
                        .is_some_and(|end| end <= b.bytes)
            });
        if len == 0 || !valid {
            return;
        }
        self.forget(at, len);
        let epoch = self.tick();
        self.bindings.insert(
            at,
            Binding {
                len,
                backing: token,
                offset,
                epoch,
            },
        );
        // A native replacement is already visible before its metadata
        // post-hook. Mark the newly exposed bytes as well as the old bytes
        // dirtied by forget(), including through another physical alias.
        self.backing_written(token, offset, len);
    }

    pub(crate) fn view(
        &mut self,
        at: usize,
        len: usize,
        handle: usize,
        offset: u64,
        token: Option<BackingToken>,
    ) {
        // A token-aware callback must not fall back to raw-handle census
        // semantics: a delayed old mapping must leave the current VA and its
        // alias indexes untouched. Validate before forget changes either one.
        if let Some(token) = token {
            let valid = len != 0
                && at.checked_add(len).is_some()
                && self.handles.get(&handle) == Some(&token)
                && self.backings.get(&token).is_some_and(|backing| {
                    backing.live
                        && backing.provenance == Provenance::PhysicalSection
                        && offset
                            .checked_add(len as u64)
                            .is_some_and(|end| end <= backing.bytes)
                });
            if !valid {
                return;
            }
        }
        // Forget/bind first: both may retire old census entries. Inserting
        // the new view before binding would make bind forget that same view.
        self.forget(at, len);
        // A legacy raw handle alone is not a lifetime-safe identity.
        if let Some(token) = token {
            self.bind(at, len, token, offset);
        }
        if len == 0 || at.checked_add(len).is_none() || offset.checked_add(len as u64).is_none() {
            return;
        }
        self.views.insert(at, (len, handle, offset));
        for k in 0..len.div_ceil(65536) {
            let shown = self
                .pages
                .entry((handle, offset + (k * 65536) as u64))
                .or_default();
            shown.push(at + k * 65536);
            self.pages_twice += usize::from(shown.len() == 2);
        }
    }

    /// Return a privileged cached view for native unmapping after unlocking.
    /// Releasing an old token cannot retire a reused handle's new incarnation.
    pub(crate) fn release_section(&mut self, token: BackingToken) -> Option<usize> {
        if let Some(backing) = self.backings.get_mut(&token) {
            backing.live = false;
        }
        let handle = self
            .handles
            .iter()
            .find_map(|(&handle, &current)| (current == token).then_some(handle))?;
        self.handles.remove(&handle);
        self.sections.retain(|s| s.0 != handle);
        let at = self.whole.iter().position(|w| w.0 == handle)?;
        Some(self.whole.swap_remove(at).1)
    }

    fn reserved(&mut self, at: usize, len: usize) {
        let Some(end) = at.checked_add(len) else {
            return;
        };
        if len == 0 {
            return;
        }
        let old: Vec<_> = self
            .reservations
            .range(..end)
            .filter(|(base, (n, _))| base.saturating_add(*n) > at)
            .map(|(&base, &(_, token))| (base, token))
            .collect();
        for (base, token) in old {
            self.reservations.remove(&base);
            if let Some(backing) = self.backings.get_mut(&token) {
                backing.live = false;
            }
        }
        let token = self.backing(len as u64, Provenance::PrivateReservation);
        self.reservations.insert(at, (len, token));
        self.forget(at, len);
    }

    fn committed(&mut self, at: usize, len: usize) {
        let Some(end) = at.checked_add(len) else {
            return;
        };
        let Some((&base, &(size, token))) = self.reservations.range(..=at).next_back() else {
            return;
        };
        if end > base.saturating_add(size) {
            return;
        }
        // Redundant MEM_COMMIT preserves existing bytes and ownership. Only
        // holes get new physical incarnations; reserve alone never made one.
        let covered: Vec<_> = self
            .binding_candidates(at, end)
            .filter(|(_, b)| b.backing == token)
            .filter_map(|(&start, b)| {
                let lo = start.max(at);
                let hi = start.saturating_add(b.len).min(end);
                (lo < hi).then_some((lo, hi))
            })
            .collect();
        let (mut cursor, mut holes) = (at, Vec::new());
        for (lo, hi) in covered {
            if cursor < lo {
                holes.push((cursor, lo));
            }
            cursor = cursor.max(hi);
        }
        if cursor < end {
            holes.push((cursor, end));
        }
        for (lo, hi) in holes {
            let version = self.tick();
            let offset = (lo - base) as u64;
            if let Some(backing) = self.backings.get_mut(&token) {
                backing
                    .incarnations
                    .set(offset, offset + (hi - lo) as u64, version);
                backing
                    .versions
                    .set(offset, offset + (hi - lo) as u64, version);
            }
            self.bind(lo, hi - lo, token, offset);
        }
    }

    fn lease(&self, at: usize, len: usize) -> Option<Lease> {
        let end = at.checked_add(len)?;
        if len == 0 {
            return None;
        }
        let mut parts = Vec::new();
        let mut cursor = at;
        while cursor < end {
            let (&base, binding) = self.bindings.range(..=cursor).next_back()?;
            let stop = base.checked_add(binding.len)?.min(end);
            if cursor >= stop {
                return None;
            }
            let backing = self.backings.get(&binding.backing).filter(|b| b.live)?;
            let offset = binding.offset + (cursor - base) as u64;
            for (lo, hi, incarnation) in backing
                .incarnations
                .runs(offset, offset + (stop - cursor) as u64)
            {
                parts.push(Part {
                    at: cursor + (lo - offset) as usize,
                    len: (hi - lo) as usize,
                    backing: binding.backing,
                    offset: lo,
                    epoch: binding.epoch,
                    incarnation,
                    provenance: backing.provenance,
                });
            }
            cursor = stop;
        }
        Some(Lease { at, len, parts })
    }

    fn valid(&self, lease: &Lease) -> bool {
        self.lease(lease.at, lease.len)
            .is_some_and(|now| now.parts == lease.parts)
    }

    /// Compare current boundaries with the original snapshot, never a newly
    /// captured identity. A changed neighbor must not discard valid output.
    fn valid_ranges(&self, lease: &Lease) -> Vec<(usize, usize)> {
        let mut valid: Vec<(usize, usize)> = Vec::new();
        for old in &lease.parts {
            let end = old.at + old.len;
            let mut cursor = old.at;
            while cursor < end {
                let present = self
                    .bindings
                    .range(..=cursor)
                    .next_back()
                    .filter(|(base, b)| cursor < base.saturating_add(b.len));
                let Some((&base, current)) = present else {
                    cursor = self
                        .bindings
                        .range((
                            std::ops::Bound::Excluded(cursor),
                            std::ops::Bound::Unbounded,
                        ))
                        .next()
                        .map_or(end, |(&next, _)| next.min(end));
                    continue;
                };
                let stop = end.min(base + current.len);
                let offset = current.offset + (cursor - base) as u64;
                let same = current.backing == old.backing
                    && current.epoch == old.epoch
                    && offset == old.offset + (cursor - old.at) as u64;
                if same && let Some(backing) = self.backings.get(&old.backing).filter(|b| b.live) {
                    for (lo, hi, incarnation) in backing
                        .incarnations
                        .runs(offset, offset + (stop - cursor) as u64)
                    {
                        if incarnation != old.incarnation {
                            continue;
                        }
                        let (lo, hi) = (
                            cursor + (lo - offset) as usize,
                            cursor + (hi - offset) as usize,
                        );
                        if let Some(last) = valid.last_mut()
                            && last.1 == lo
                        {
                            last.1 = hi;
                        } else {
                            valid.push((lo, hi));
                        }
                    }
                }
                cursor = stop;
            }
        }
        valid
    }

    fn backing_written(&mut self, token: BackingToken, offset: u64, len: usize) {
        let Some(end) = offset.checked_add(len as u64) else {
            return;
        };
        if len == 0
            || !self
                .backings
                .get(&token)
                .is_some_and(|b| b.live && end <= b.bytes)
        {
            return;
        }
        let version = self.tick();
        self.backings
            .get_mut(&token)
            .unwrap()
            .versions
            .set(offset, end, version);
    }

    fn reincarnated(&mut self, token: BackingToken, offset: u64, len: usize) {
        let Some(end) = offset.checked_add(len as u64) else {
            return;
        };
        if len == 0
            || !self
                .backings
                .get(&token)
                .is_some_and(|b| b.live && end <= b.bytes)
        {
            return;
        }
        let version = self.tick();
        let backing = self.backings.get_mut(&token).unwrap();
        backing.incarnations.set(offset, end, version);
        backing.versions.set(offset, end, version);
    }

    fn discarded(&mut self, at: usize, len: usize) {
        let Some(end) = at.checked_add(len) else {
            return;
        };
        let affected: Vec<_> = self
            .binding_candidates(at, end)
            .filter_map(|(&base, binding)| {
                let lo = base.max(at);
                let hi = base.saturating_add(binding.len).min(end);
                (lo < hi).then(|| {
                    (
                        binding.backing,
                        binding.offset + (lo - base) as u64,
                        hi - lo,
                    )
                })
            })
            .collect();
        for (token, offset, bytes) in affected {
            self.reincarnated(token, offset, bytes);
        }
    }

    fn cpu_written(&mut self, at: usize, len: usize) {
        let Some(end) = at.checked_add(len) else {
            return;
        };
        // Known portions still count when the whole range contains a hole.
        let writes: Vec<_> = self
            .binding_candidates(at, end)
            .filter_map(|(&base, binding)| {
                let lo = base.max(at);
                let hi = base.saturating_add(binding.len).min(end);
                (lo < hi).then(|| {
                    (
                        binding.backing,
                        binding.offset + (lo - base) as u64,
                        hi - lo,
                    )
                })
            })
            .collect();
        for (token, offset, bytes) in writes {
            self.backing_written(token, offset, bytes);
        }
    }

    fn drain_faults<const N: usize>(
        &mut self,
        faults: &FaultJournal<N>,
        epoch: u64,
        overflow: bool,
    ) -> FaultDrain {
        if epoch & 1 != 0 {
            return FaultDrain {
                deferred: true,
                ..Default::default()
            };
        }
        let mut result = FaultDrain {
            conservative: overflow,
            ..Default::default()
        };
        (result.events, result.deferred) = faults.drain(|block, recorded, vm_epoch| {
            // Preserve the legacy exact stable-VM partial-range behavior.
            // Relax an odd/changed global VM epoch only for a completely
            // tracked block whose own VA identity did not change. Unknown
            // physical aliases are not proved by an unchanged empty map.
            let unchanged = self.fault_mapping_unchanged(block, recorded);
            if unchanged && (vm_epoch == epoch || self.fault_mapping_covered(block)) {
                self.cpu_written(block, 65536);
            } else {
                if unchanged {
                    result.uncovered += 1;
                } else {
                    result.mismatched += 1;
                }
                result.conservative = true;
            }
        });
        if result.conservative {
            let version = self.tick();
            for backing in self.backings.values_mut().filter(|b| b.live) {
                result.bytes = result.bytes.saturating_add(backing.bytes);
                backing.versions.set(0, backing.bytes, version);
            }
        }
        result
    }
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    views: BTreeMap::new(),
    sections: Vec::new(),
    pages: BTreeMap::new(),
    pages_twice: 0,
    whole: Vec::new(),
    handles: BTreeMap::new(),
    backings: BTreeMap::new(),
    bindings: BTreeMap::new(),
    reservations: BTreeMap::new(),
    mapping_versions: Versions(BTreeMap::new()),
    mapping_overflow: false,
    next: 0,
    clock: 0,
});

pub(crate) fn registry() -> MutexGuard<'static, Registry> {
    let mut r = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    if enabled() {
        let epoch = MAPPING_EPOCH.load(Ordering::Acquire);
        // VM post-hooks use registry metadata under the lifecycle writer.
        // All byte/version consumers pin a lifecycle reader before observing
        // content. Do not consume their events against a half-updated map.
        if epoch & 1 != 0 {
            FAULT_STATS[6].fetch_add(1, Ordering::Relaxed);
            return r;
        }
        let overflow = FAULT_OVERFLOW.swap(false, Ordering::AcqRel);
        let result = r.drain_faults(&FAULTS, epoch, overflow);
        FAULT_STATS[2].fetch_add(result.events as u64, Ordering::Relaxed);
        FAULT_STATS[5].fetch_add(result.mismatched, Ordering::Relaxed);
        FAULT_STATS[9].fetch_add(result.uncovered, Ordering::Relaxed);
        FAULT_STATS[6].fetch_add(u64::from(result.deferred), Ordering::Relaxed);
        FAULT_STATS[7].fetch_add(u64::from(result.conservative), Ordering::Relaxed);
        FAULT_STATS[8].fetch_add(result.bytes, Ordering::Relaxed);
    }
    r
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Part {
    at: usize,
    len: usize,
    backing: BackingToken,
    offset: u64,
    epoch: u64,
    incarnation: u64,
    provenance: Provenance,
}

/// Immutable identity snapshot. Its mapping/incarnation must still be checked
/// when consumed; keeping this object does not prevent remapping guest memory.
#[derive(Clone, Debug)]
pub(crate) struct Lease {
    at: usize,
    len: usize,
    parts: Vec<Part>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StorageKey {
    pub(crate) backing: BackingToken,
    pub(crate) incarnation: u64,
    pub(crate) offset: u64,
    pub(crate) len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PhysicalKey {
    pub(crate) backing: BackingToken,
    pub(crate) incarnation: u64,
    pub(crate) offset: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PhysicalAlias {
    pub(crate) at: usize,
    pub(crate) len: usize,
    pub(crate) epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PhysicalSlice {
    pub(crate) key: PhysicalKey,
    pub(crate) handle: usize,
    pub(crate) aliases: Vec<PhysicalAlias>,
}

impl Registry {
    fn physical_key(&self, token: BackingToken, offset: u64) -> Option<PhysicalKey> {
        const PAGE: usize = 64 << 10;
        let range = self.physical_range(token, offset, PAGE)?;
        let [(_, _, incarnation)] = range.incarnations.as_slice() else {
            return None;
        };
        Some(PhysicalKey {
            backing: token,
            incarnation: *incarnation,
            offset,
        })
    }

    fn physical_slice(&self, key: PhysicalKey) -> Option<PhysicalSlice> {
        const PAGE: usize = 64 << 10;
        if !key.offset.is_multiple_of(PAGE as u64)
            || self.physical_key(key.backing, key.offset)? != key
        {
            return None;
        }
        let range = self.physical_range(key.backing, key.offset, PAGE)?;
        let aliases = self
            .physical_aliases(&range)?
            .into_iter()
            .map(|(at, len, epoch)| PhysicalAlias { at, len, epoch })
            .collect::<Vec<_>>();
        if aliases.is_empty()
            || aliases
                .iter()
                .any(|a| !a.at.is_multiple_of(PAGE) || a.len != PAGE)
        {
            return None;
        }
        let handle = self
            .handles
            .iter()
            .find_map(|(&handle, &token)| (token == key.backing).then_some(handle))?;
        Some(PhysicalSlice {
            key,
            handle,
            aliases,
        })
    }

    fn physical_keys_in(&self, at: usize, len: usize) -> Vec<PhysicalKey> {
        self.physical_keys_in_visiting(at, len, || {})
    }

    fn physical_keys_in_visiting(
        &self,
        at: usize,
        len: usize,
        mut visit: impl FnMut(),
    ) -> Vec<PhysicalKey> {
        const PAGE: u64 = 64 << 10;
        let Some(end) = at.checked_add(len) else {
            return Vec::new();
        };
        if len == 0 {
            return Vec::new();
        }
        let mut keys = Vec::new();
        for (&base, binding) in self.binding_candidates(at, end) {
            visit();
            let (lo, hi) = (at.max(base), end.min(base.saturating_add(binding.len)));
            if lo >= hi {
                continue;
            }
            let Some(backing) = self
                .backings
                .get(&binding.backing)
                .filter(|b| b.live && b.provenance == Provenance::PhysicalSection)
            else {
                continue;
            };
            let from = binding.offset + (lo - base) as u64;
            let to = binding.offset + (hi - base) as u64;
            for offset in (from / PAGE..to.div_ceil(PAGE)).map(|p| p * PAGE) {
                if offset
                    .checked_add(PAGE)
                    .is_none_or(|end| end > backing.bytes)
                {
                    continue;
                }
                if let Some(key) = self.physical_key(binding.backing, offset) {
                    keys.push(key);
                }
            }
        }
        keys.sort_unstable();
        keys.dedup();
        keys
    }
}

/// Metadata only. A native pin must span consumption of an alias/handle snapshot.
pub(crate) fn physical_slice_for_key(key: PhysicalKey) -> Option<PhysicalSlice> {
    registry().physical_slice(key)
}
pub(crate) fn physical_keys_for_range(at: usize, len: usize) -> Vec<PhysicalKey> {
    registry().physical_keys_in(at, len)
}
pub(crate) fn physical_key_at(at: usize) -> Option<PhysicalKey> {
    let at = at / 65536 * 65536;
    let r = registry();
    let lease = r.lease(at, 64 << 10)?;
    let [part] = lease.parts.as_slice() else {
        return None;
    };
    if part.provenance != Provenance::PhysicalSection
        || !part.offset.is_multiple_of(64 << 10)
        || part.len != 64 << 10
    {
        return None;
    }
    r.physical_key(part.backing, part.offset)
}
pub(crate) fn physical_alias_current(key: PhysicalKey, at: usize, epoch: u64) -> bool {
    let r = registry();
    let Some(lease) = r.lease(at, 65536) else {
        return false;
    };
    let [part] = lease.parts.as_slice() else {
        return false;
    };
    part.provenance == Provenance::PhysicalSection
        && part.backing == key.backing
        && part.offset == key.offset
        && part.incarnation == key.incarnation
        && part.epoch == epoch
        && part.len == 65536
}

impl BackingRange {
    pub(crate) fn physical_keys(&self) -> Vec<PhysicalKey> {
        let r = registry();
        if r.physical_identity(self.token, self.offset, self.len)
            .as_ref()
            != Some(self)
        {
            return Vec::new();
        }
        (self.offset / 65536..(self.offset + self.len as u64).div_ceil(65536))
            .filter_map(|p| r.physical_key(self.token, p * 65536))
            .collect()
    }
}

impl Lease {
    pub(crate) fn current_ranges(&self, at: usize, len: usize) -> Vec<(usize, usize)> {
        let Some(original) = self.subrange(at, len) else {
            return Vec::new();
        };
        registry().valid_ranges(&original)
    }
    /// Clip the original identity snapshot, including its original mapping
    /// epochs and incarnation boundaries. Never resolve a replacement here.
    pub(crate) fn subrange(&self, at: usize, len: usize) -> Option<Self> {
        let end = at.checked_add(len)?;
        if len == 0 || at < self.at || end > self.at.checked_add(self.len)? {
            return None;
        }
        let parts = self
            .parts
            .iter()
            .filter_map(|part| {
                let lo = at.max(part.at);
                let hi = end.min(part.at + part.len);
                (lo < hi).then(|| Part {
                    at: lo,
                    len: hi - lo,
                    offset: part.offset + (lo - part.at) as u64,
                    ..part.clone()
                })
            })
            .collect();
        Some(Self { at, len, parts })
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = StorageKey> + '_ {
        self.parts.iter().map(|p| StorageKey {
            backing: p.backing,
            incarnation: p.incarnation,
            offset: p.offset,
            len: p.len,
        })
    }
    pub(crate) fn tracking_complete(&self) -> bool {
        false
    }

    /// A finite original VirtualAlloc page, not general alias completeness.
    /// Caller pins native identity/lifecycle through use of this proof.
    pub(crate) fn private_page_current(&self, at: usize, len: usize) -> bool {
        if len != 65536 || !at.is_multiple_of(65536) {
            return false;
        }
        let Some(page) = self.subrange(at, len) else {
            return false;
        };
        !page.parts.is_empty()
            && page
                .parts
                .iter()
                .all(|part| part.provenance == Provenance::PrivateReservation)
            && registry().valid(&page)
    }

    pub(crate) fn provenance(&self) -> Option<Provenance> {
        let first = self.parts.first()?.provenance;
        self.parts
            .iter()
            .all(|p| p.provenance == first)
            .then_some(first)
    }

    pub(crate) fn same_identity(&self, other: &Self) -> bool {
        self.parts == other.parts
    }

    pub(crate) fn current_version(&self) -> Option<u64> {
        if mapping_epoch() & 1 != 0 {
            return None;
        }
        let r = registry();
        if !r.valid(self) {
            return None;
        }
        Some(
            self.parts
                .iter()
                .map(|p| {
                    r.backings[&p.backing]
                        .versions
                        .maximum(p.offset, p.offset + p.len as u64)
                })
                .max()
                .unwrap_or(0),
        )
    }

    /// Global observation cursor plus changed virtual byte intervals. This
    /// does not acknowledge a CPU copy: callers invalidate all returned ranges
    /// before advancing their cursor, including ranges not requested today.
    pub(crate) fn changes_since(&self, since: u64) -> Option<(u64, Vec<(usize, usize)>)> {
        if mapping_epoch() & 1 != 0 {
            return None;
        }
        let r = registry();
        if !r.valid(self) {
            return None;
        }
        let mut changed = Vec::new();
        for p in &self.parts {
            for (lo, hi) in
                r.backings[&p.backing]
                    .versions
                    .changed(p.offset, p.offset + p.len as u64, since)
            {
                changed.push((
                    p.at + (lo - p.offset) as usize,
                    p.at + (hi - p.offset) as usize,
                ));
            }
        }
        Some((r.clock, changed))
    }

    /// Observed-write guard only. False tracking completeness means even true
    /// here cannot authorize skipping the legacy CPU merge or read checks.
    pub(crate) fn permits_observed_version(&self, version: u64) -> bool {
        self.current_version().is_some_and(|now| now <= version)
    }
}

pub(crate) fn resolve(at: usize, len: usize) -> Option<Lease> {
    if !enabled() {
        return None;
    }
    registry().lease(at, len)
}
pub(crate) fn cpu_written(at: usize, len: usize) {
    if !enabled() || GPU_PUBLICATION.with(|depth| depth.get() != 0) {
        return;
    }
    registry().cpu_written(at, len);
}
pub(crate) fn allocated(at: usize, len: usize) {
    registry().reserved(at, len);
    committed(at, len);
}

pub(crate) fn committed(at: usize, len: usize) {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_PRIVATE, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    let Some(end) = at.checked_add(len) else {
        return;
    };
    let mut cursor = at;
    while cursor < end {
        // Native query occurs outside the registry lock, while the VM caller
        // retains its lifecycle writer. Do not infer commitment from reserve.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { VirtualQuery(cursor as *const _, &mut info, std::mem::size_of_val(&info)) } == 0
        {
            return;
        }
        let Some(stop) = (info.BaseAddress as usize)
            .checked_add(info.RegionSize)
            .map(|stop| stop.min(end))
        else {
            return;
        };
        if stop <= cursor {
            return;
        }
        if info.State == MEM_COMMIT && info.Type == MEM_PRIVATE {
            registry().committed(cursor, stop - cursor);
        }
        cursor = stop;
    }
}

pub(crate) fn discarded(at: usize, len: usize) {
    registry().discarded(at, len);
}

pub(crate) fn releasing(at: usize, len: usize) {
    let mut r = registry();
    let len = if len != 0 {
        len
    } else {
        r.reservations
            .get(&at)
            .map(|r| r.0)
            .or_else(|| r.bindings.get(&at).map(|b| b.len))
            .unwrap_or(0)
    };
    r.forget(at, len);
}

/// Physical identity captured while the HLE still holds its page-table lock.
/// This is an incarnation snapshot, not alias-closure or retention eligibility.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackingRange {
    token: BackingToken,
    offset: u64,
    len: usize,
    incarnations: Vec<(u64, u64, u64)>,
}

pub fn title_memory_backing_range(
    token: BackingToken,
    offset: u64,
    len: usize,
) -> Option<BackingRange> {
    registry().physical_range(token, offset, len)
}

/// Identity-only VM preflight: unlike temporary access admission this can
/// describe an eager backing with an escaped alias. It never grants ownership.
pub fn title_memory_backing_identity(
    token: BackingToken,
    offset: u64,
    len: usize,
) -> Option<BackingRange> {
    registry().physical_identity(token, offset, len)
}
pub fn title_memory_backing_current(range: &BackingRange) -> bool {
    registry()
        .physical_identity(range.token, range.offset, range.len)
        .as_ref()
        == Some(range)
}

/// Called once for a real section creation, not for each alias.
pub fn title_memory_section_created(section: usize, bytes: u64, committed: bool) -> BackingToken {
    registry().section(section, bytes, committed)
}

/// Token-aware counterpart of the legacy mapping census callback.
pub fn title_memory_view_backing(
    at: usize,
    len: usize,
    section: usize,
    offset: u64,
    token: BackingToken,
) {
    registry().view(at, len, section, offset, Some(token));
}

/// A write through a temporary mapping, including a partial successful write.
pub fn title_memory_backing_written(token: BackingToken, offset: u64, len: usize) {
    if enabled() {
        registry().backing_written(token, offset, len);
    }
}

/// A PFN slice retired/reassigned within an existing section. The caller
/// holds the lifecycle mutation lease, never the registry lock.
pub fn title_memory_backing_reincarnated(token: BackingToken, offset: u64, len: usize) {
    registry().reincarnated(token, offset, len);
}

/// Metadata-only release; the bridge unmaps a returned privileged view after
/// this function has dropped the registry guard.
pub(crate) fn title_memory_section_released(token: BackingToken) -> Option<usize> {
    registry().release_section(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_token_view_cannot_replace_current_identity_or_census() {
        const PAGE: usize = 65536;
        let mut r = Registry::default();
        let old = r.section(7, (4 * PAGE) as u64, false);
        let current = r.section(7, (4 * PAGE) as u64, false);
        let (a, b) = (8 * PAGE, 16 * PAGE);
        r.view(a, 2 * PAGE, 7, 0, Some(current));
        r.view(b, PAGE, 7, PAGE as u64, Some(current));
        let lease = r.lease(a, 2 * PAGE).unwrap();
        let other = r.lease(b, PAGE).unwrap();
        let key = r.physical_key(current, PAGE as u64).unwrap();
        let aliases = r.physical_slice(key).unwrap();
        let views = r.views.clone();
        let pages = r.pages.clone();
        let clock = r.clock;

        for (handle, token, offset) in [
            (7, old, 0),
            (9, current, 0),
            (7, current, (4 * PAGE) as u64),
        ] {
            r.view(a, PAGE, handle, offset, Some(token));
            assert_eq!(r.views, views, "rejected callback changed the VA census");
            assert_eq!(r.pages, pages, "rejected callback changed reverse aliases");
            assert_eq!(r.pages_twice, 1);
            assert_eq!(
                r.clock, clock,
                "rejected callback changed observed versions"
            );
            assert!(r.valid(&lease) && r.valid(&other));
            assert_eq!(r.physical_slice(key), Some(aliases.clone()));
        }
        // Explicit token validation does not disable the established metadata
        // callback for producers that cannot supply a creation token.
        r.view(a, PAGE, 99, 0, None);
        assert_eq!(r.views[&a], (PAGE, 99, 0));
        assert_eq!(r.pages[&(99, 0)], [a]);
        assert!(r.lease(a, PAGE).is_none());
    }

    #[test]
    fn census_and_identity_forget_the_same_whole_intersecting_view() {
        const PAGE: usize = 65536;
        let mut r = Registry::default();
        let token = r.section(7, (4 * PAGE) as u64, false);
        let (a, b) = (8 * PAGE, 16 * PAGE);
        r.view(a, 2 * PAGE, 7, 0, Some(token));
        r.view(b, PAGE, 7, PAGE as u64, Some(token));
        let other = r.lease(b, PAGE).unwrap();
        assert_eq!(r.pages[&(7, 0)], [a]);
        assert_eq!(r.pages[&(7, PAGE as u64)], [a + PAGE, b]);
        assert_eq!(r.pages_twice, 1);

        // The current runtime re-notifies surviving pieces. Until then neither
        // canonical identity nor the alias census may retain this old view.
        r.forget(a + 1, PAGE / 2);
        assert!(!r.views.contains_key(&a));
        assert!(r.lease(a, 1).is_none());
        assert!(r.lease(a + PAGE, 1).is_none());
        assert!(!r.pages.contains_key(&(7, 0)));
        assert_eq!(r.pages[&(7, PAGE as u64)], [b]);
        assert_eq!(r.pages_twice, 0);
        assert!(r.valid(&other));
    }

    #[test]
    fn replacing_a_view_retires_old_indexes_before_adding_the_new_binding() {
        const PAGE: usize = 65536;
        let mut r = Registry::default();
        let token = r.section(7, (4 * PAGE) as u64, false);
        let a = 8 * PAGE;
        r.view(a, 2 * PAGE, 7, 0, Some(token));
        r.view(a + PAGE, PAGE, 7, (2 * PAGE) as u64, Some(token));
        assert!(r.lease(a, PAGE).is_none());
        let new = r.lease(a + PAGE, PAGE).unwrap();
        assert_eq!(new.keys().next().unwrap().offset, (2 * PAGE) as u64);
        assert_eq!(r.views.len(), 1);
        assert_eq!(r.pages.len(), 1);
        assert_eq!(r.pages[&(7, (2 * PAGE) as u64)], [a + PAGE]);
        assert_eq!(r.pages_twice, 0);
    }

    #[test]
    fn stale_token_release_cannot_retire_a_reused_handles_cached_view() {
        let mut r = Registry::default();
        let old = r.section(7, 65536, false);
        r.whole.push((7, 0x10000));
        let new = r.section(7, 131072, true);
        assert!(
            r.whole.is_empty(),
            "old native view is no longer a reusable cache entry"
        );
        r.whole.push((7, 0x20000));
        assert_eq!(r.release_section(old), None);
        assert_eq!(r.sections, [(7, 131072, true)]);
        assert_eq!(r.whole, [(7, 0x20000)]);
        assert_eq!(r.release_section(new), Some(0x20000));
        assert!(r.sections.is_empty() && r.whole.is_empty());
        assert!(r.handles.is_empty());
    }

    #[test]
    fn token_must_name_the_current_section_and_never_proves_closed_tracking() {
        let mut r = Registry::default();
        let token = r.section(7, 65536, false);
        r.view(0x10000, 65536, 8, 0, Some(token));
        assert!(r.lease(0x10000, 65536).is_none());
        r.view(0x20000, 65536, 7, 0, Some(token));
        let lease = r.lease(0x20000, 65536).unwrap();
        assert!(!lease.tracking_complete());
        r.cpu_written(0x20000, 65536);
        assert!(
            !lease.tracking_complete(),
            "observed writes do not establish GPU ownership"
        );
    }

    #[test]
    fn physical_key_queries_visit_only_overlaps_and_one_predecessor() {
        const PAGE: usize = 64 << 10;
        let mut r = Registry::default();
        let private = r.backing(PAGE as u64, Provenance::PrivateReservation);
        // Populate independent, disjoint earlier mappings without spending
        // this query regression's time on unrelated bind/forget mutations.
        for i in 0..4096 {
            r.bindings.insert(
                (i + 1) * 2 * PAGE,
                Binding {
                    len: PAGE,
                    backing: private,
                    offset: 0,
                    epoch: i as u64 + 1,
                },
            );
        }
        let at = (4096 + 8) * 2 * PAGE;
        let token = r.section(7, (8 * PAGE) as u64, false);
        r.view(at, 2 * PAGE, 7, PAGE as u64, Some(token));
        r.bind(at + 2 * PAGE, PAGE, private, 0);
        // The next page is a gap, then another two-page physical view.
        r.view(at + 4 * PAGE, 2 * PAGE, 7, (4 * PAGE) as u64, Some(token));
        // An alias must deduplicate to the exact same physical key.
        r.view(at + 8 * PAGE, PAGE, 7, PAGE as u64, Some(token));
        let key = |offset| r.physical_key(token, offset as u64 * PAGE as u64).unwrap();
        let cases = [
            (at + 3, 1, vec![key(1)]),
            (at + PAGE - 1, 2, vec![key(1), key(2)]),
            (at + 2 * PAGE, PAGE, vec![]),
            (at + 3 * PAGE, PAGE, vec![]),
            (at + 3 * PAGE, PAGE + 1, vec![key(4)]),
            (at + 6 * PAGE, PAGE, vec![]),
            (at, 6 * PAGE, vec![key(1), key(2), key(4), key(5)]),
            (at, 9 * PAGE, vec![key(1), key(2), key(4), key(5)]),
            (at - 1, 1, vec![]),
            (0, 0, vec![]),
            (at, 0, vec![]),
            (usize::MAX - 3, 8, vec![]),
        ];
        for (start, len, expected) in cases {
            let mut visits = 0;
            let actual = r.physical_keys_in_visiting(start, len, || visits += 1);
            assert_eq!(actual, expected, "exact keys for {start:#x}+{len:#x}");
            assert_eq!(r.physical_keys_in(start, len), expected);
            let overlaps = start.checked_add(len).map_or(0, |end| {
                r.bindings
                    .range(..end)
                    .filter(|(base, binding)| start.max(**base) < end.min(**base + binding.len))
                    .count()
            });
            assert!(
                visits <= overlaps + 1,
                "query visited {visits} bindings for {overlaps} overlaps after 4096 unrelated maps"
            );
        }
        // The same bounded candidate iterator serves hot write journaling;
        // preserve exact changed bytes and the surrounding physical versions.
        let before = r.backings[&token].versions.maximum(0, (8 * PAGE) as u64);
        let changed = (2 * PAGE - 1) as u64;
        let left = r.backings[&token].versions.at(changed - 1);
        let right = r.backings[&token].versions.at(changed + 2);
        r.cpu_written(at + PAGE - 1, 2);
        assert_eq!(r.backings[&token].versions.at(changed - 1), left);
        assert!(r.backings[&token].versions.at(changed) > before);
        assert!(r.backings[&token].versions.at(changed + 1) > before);
        assert_eq!(r.backings[&token].versions.at(changed + 2), right);
        // Discard still reincarnates just the affected physical bytes. A
        // reused sub-page must fail the complete64KiB incarnation proof.
        r.discarded(at + 4 * PAGE, PAGE / 2);
        assert_eq!(
            r.physical_keys_in(at + 4 * PAGE, 2 * PAGE),
            vec![r.physical_key(token, (5 * PAGE) as u64).unwrap()]
        );
    }

    #[test]
    fn physical_access_snapshot_rejects_reincarnation_reuse_and_escaped_aliases() {
        let mut r = Registry::default();
        let token = r.section(7, 2 * 65536, false);
        let original = r.physical_range(token, 0, 65536).unwrap();
        r.view(0x100_0000, 65536, 7, 0, Some(token));
        r.view(0x200_0000, 65536, 7, 0, Some(token));
        let aliases = r.physical_aliases(&original).unwrap();
        assert_eq!(
            aliases
                .iter()
                .map(|&(at, n, _)| (at, n))
                .collect::<Vec<_>>(),
            [(0x100_0000, 65536), (0x200_0000, 65536)]
        );
        r.forget(0x200_0000, 65536);
        r.view(0x200_0000, 65536, 7, 0, Some(token));
        assert_ne!(
            r.physical_aliases(&original).unwrap(),
            aliases,
            "same-address remap changes the snapshot epoch"
        );
        r.reincarnated(token, 0, 65536);
        assert!(
            r.physical_aliases(&original).is_none(),
            "a captured PFN generation cannot follow reuse"
        );
        let fresh = r.physical_range(token, 0, 65536).unwrap();
        assert_ne!(fresh, original);
        r.backings.get_mut(&token).unwrap().temporary_alias_escaped = true;
        assert!(
            r.physical_aliases(&fresh).is_none(),
            "failed unmap never restores a closed census"
        );
        let before = r.backings[&token].versions.maximum(0, 65536);
        assert!(
            r.temporary_written(&fresh, 7, 3),
            "an existing pinned A still journals after B escapes"
        );
        assert!(r.backings[&token].versions.at(7) > before);
        assert_eq!(r.backings[&token].versions.at(6), before);
        assert_eq!(r.backings[&token].versions.at(10), before);
        assert!(
            !r.temporary_written(&original, 7, 3),
            "a stale incarnation cannot journal into its replacement"
        );
        r.backings.get_mut(&token).unwrap().live = false;
        let replacement = r.section(7, 2 * 65536, false);
        assert_ne!(
            replacement, token,
            "handle reuse does not reuse backing identity"
        );
        assert!(r.physical_aliases(&original).is_none());
        assert!(r.physical_range(replacement, 0, 65536).is_some());
        assert!(r.physical_range(replacement, 2 * 65536, 1).is_none());
        assert!(r.physical_range(replacement, u64::MAX, 1).is_none());
        assert!(r.physical_range(replacement, 0, 0).is_none());
    }

    #[test]
    fn physical_aliases_share_exact_range_versions() {
        let mut r = Registry::default();
        let token = r.section(7, 4 * 4096, false);
        r.view(0x10000, 8192, 7, 4096, Some(token));
        r.view(0x20000, 4096, 7, 8192, Some(token));
        let a = r.lease(0x10000, 8192).unwrap();
        let b = r.lease(0x20000, 4096).unwrap();
        let since = r.clock;
        let untouched = r.backings[&token].versions.maximum(4096, 8192);
        r.cpu_written(0x20010, 3);
        let backing = &r.backings[&token];
        assert_eq!(
            backing.versions.changed(4096, 12288, since),
            vec![(8208, 8211)]
        );
        assert!(r.valid(&a) && r.valid(&b));
        assert_eq!(backing.versions.maximum(4096, 8192), untouched);
    }

    #[test]
    fn remap_changes_mapping_epoch_not_other_alias_incarnation() {
        let mut r = Registry::default();
        let token = r.section(7, 8192, false);
        r.view(0x10000, 4096, 7, 0, Some(token));
        r.view(0x20000, 4096, 7, 0, Some(token));
        let a = r.lease(0x10000, 4096).unwrap();
        let b = r.lease(0x20000, 4096).unwrap();
        r.view(0x10000, 4096, 7, 0, Some(token));
        assert!(!r.valid(&a));
        assert!(r.valid(&b));
    }

    #[test]
    fn section_handle_and_private_address_reuse_get_new_identity() {
        let mut r = Registry::default();
        let first = r.section(7, 4096, false);
        r.view(0x10000, 4096, 7, 0, Some(first));
        let old = r.lease(0x10000, 4096).unwrap();
        let second = r.section(7, 4096, false);
        r.view(0x10000, 4096, 7, 0, Some(second));
        assert_ne!(first, second);
        assert!(!r.valid(&old));
        r.reserved(0x30000, 8192);
        r.committed(0x30000, 8192);
        let old = r.lease(0x30000, 4096).unwrap();
        r.reserved(0x30000, 8192);
        assert!(!r.valid(&old));
    }

    #[test]
    fn recommit_invalidates_only_affected_incarnation() {
        let mut r = Registry::default();
        r.reserved(0x10000, 8192);
        r.committed(0x10000, 8192);
        let first = r.lease(0x10000, 4096).unwrap();
        let second = r.lease(0x11000, 4096).unwrap();
        r.forget(0x10000, 4096);
        assert!(!r.valid(&first));
        assert!(r.valid(&second));
        r.committed(0x10000, 4096);
        assert!(!r.valid(&first));
        assert!(r.valid(&second));
        assert!(r.lease(0x10000, 4096).is_some());
    }

    #[test]
    fn older_gpu_observation_cannot_cover_a_newer_cpu_write() {
        let mut r = Registry::default();
        let token = r.section(7, 8192, false);
        r.view(0x10000, 8192, 7, 0, Some(token));
        let produced = r.clock;
        let untouched = r.backings[&token].versions.maximum(4096, 8192);
        r.backing_written(token, 12, 4);
        let versions = &r.backings[&token].versions;
        assert!(versions.maximum(0, 4096) > produced);
        assert_eq!(versions.maximum(4096, 8192), untouched);
    }

    #[test]
    fn legacy_handle_only_and_holes_do_not_claim_identity() {
        let mut r = Registry::default();
        r.section(7, 8192, false);
        r.view(0x10000, 4096, 7, 0, None);
        assert!(r.lease(0x10000, 4096).is_none());
        assert!(r.lease(0x10000, usize::MAX).is_none());
    }

    #[test]
    fn redundant_commit_preserves_owner_mapping_and_versions() {
        let mut r = Registry::default();
        r.reserved(0x10000, 8192);
        assert!(r.lease(0x10000, 4096).is_none());
        r.committed(0x10000, 8192);
        let old = r.lease(0x10000, 8192).unwrap();
        let cursor = r.clock;
        r.committed(0x11000, 4096);
        assert!(r.valid(&old));
        assert_eq!(r.clock, cursor);
    }

    #[test]
    fn discard_reincarnates_only_affected_committed_bytes() {
        let mut r = Registry::default();
        r.reserved(0x10000, 8192);
        r.committed(0x10000, 8192);
        let changed = r.lease(0x10000, 4096).unwrap();
        let disjoint = r.lease(0x11000, 4096).unwrap();
        r.discarded(0x10000, 4096);
        assert!(!r.valid(&changed));
        assert!(r.valid(&disjoint));
        assert!(r.lease(0x10000, 4096).is_some());
    }

    #[test]
    fn original_disjoint_gpu_result_still_publishes_after_other_page_reuse() {
        for recommit in [false, true] {
            let mut r = Registry::default();
            r.reserved(0x10000, 8192);
            r.committed(0x10000, 8192);
            let original = r.lease(0x10000, 8192).unwrap();
            if recommit {
                r.forget(0x10000, 4096);
                r.committed(0x10000, 4096);
            } else {
                r.discarded(0x10000, 4096);
            }
            assert!(!r.valid(&original));
            let affected = original.subrange(0x10000, 4096).unwrap();
            let disjoint = original.subrange(0x11000, 4096).unwrap();
            let mut bytes = vec![9u8; 8192];
            assert!(!r.valid(&affected));
            assert!(r.valid(&disjoint));
            let valid = r.valid_ranges(&original);
            assert_eq!(valid, vec![(0x11000, 0x12000)]);
            for (lo, hi) in valid {
                bytes[lo - 0x10000..hi - 0x10000].fill(3);
            }
            assert!(
                bytes[..4096].iter().all(|&b| b == 9),
                "affected old result must be rejected"
            );
            assert!(
                bytes[4096..].iter().all(|&b| b == 3),
                "disjoint old result must publish"
            );
        }
    }

    #[test]
    fn slice_reuse_splits_incarnations_without_changing_disjoint_owner() {
        let mut r = Registry::default();
        let token = r.section(7, 8192, false);
        r.view(0x10000, 8192, 7, 0, Some(token));
        let old = r.lease(0x10000, 4096).unwrap();
        let disjoint = r.lease(0x11000, 4096).unwrap();
        r.reincarnated(token, 0, 4096);
        assert!(!r.valid(&old));
        assert!(r.valid(&disjoint));
        let whole = r.lease(0x10000, 8192).unwrap();
        assert_eq!(whole.parts.len(), 2);
        assert_ne!(whole.parts[0].incarnation, whole.parts[1].incarnation);
        assert!(!whole.tracking_complete());
    }

    #[test]
    fn old_fault_va_is_not_attributed_to_new_mapping() {
        const PAGE: usize = 65536;
        let mut r = Registry::default();
        for at in [PAGE, 2 * PAGE] {
            r.reserved(at, PAGE);
            r.committed(at, PAGE);
        }
        let old = r.lease(PAGE, PAGE).unwrap().parts[0].backing;
        let other = r.lease(2 * PAGE, PAGE).unwrap().parts[0].backing;
        let recorded = MAPPING_SERIAL.load(Ordering::Acquire);
        r.bind(PAGE, PAGE, other, 0);
        assert!(!r.fault_mapping_unchanged(PAGE, recorded));
        let before = r.clock;
        let journal = FaultJournal::<1>::new();
        assert!(journal.push(PAGE, recorded));
        let result = r.drain_faults(&journal, 4, false);
        assert!(result.conservative);
        assert!(
            r.backings[&old].versions.maximum(0, PAGE as u64) > before,
            "delayed old-VA events must cover the surviving old physical alias"
        );
        assert!(r.backings[&other].versions.maximum(0, PAGE as u64) > before);
    }

    #[test]
    fn fault_journal_defers_unpublished_slots_without_dirtying_unrelated_backings() {
        const PAGE: usize = 65536;
        let mut r = Registry::default();
        for at in [PAGE, 2 * PAGE, 3 * PAGE] {
            r.reserved(at, PAGE);
            r.committed(at, PAGE);
        }
        let paused_backing = r.lease(PAGE, PAGE).unwrap().parts[0].backing;
        let later_backing = r.lease(2 * PAGE, PAGE).unwrap().parts[0].backing;
        let untouched = r.lease(3 * PAGE, PAGE).unwrap().parts[0].backing;
        let observed = MAPPING_SERIAL.load(Ordering::Acquire);
        let since = r.clock;
        let journal = FaultJournal::<2>::new();
        let paused = journal.reserve().unwrap();
        assert!(journal.push(2 * PAGE, observed));
        assert!(
            journal.reserve().is_none(),
            "in-flight reservations count toward capacity"
        );
        let pending = r.drain_faults(&journal, 2, false);
        assert!(pending.deferred && !pending.conservative);
        assert_eq!(pending.events, 1);
        let later_version = r.backings[&later_backing].versions.maximum(0, PAGE as u64);
        assert!(
            later_version > since,
            "a completed later writer must invalidate before the head resumes"
        );
        assert!(r.backings[&paused_backing].versions.maximum(0, PAGE as u64) <= since);
        assert!(r.backings[&untouched].versions.maximum(0, PAGE as u64) <= since);
        assert!(
            journal.reserve().is_none(),
            "consumed later slots cannot recycle past a paused head"
        );
        assert_eq!(
            r.drain_faults(&journal, 2, false).events,
            0,
            "later events are consumed once"
        );
        journal.publish(paused, PAGE, observed);
        let ready = r.drain_faults(&journal, 2, false);
        assert_eq!(ready.events, 1);
        assert!(!ready.deferred && !ready.conservative);
        assert!(r.backings[&paused_backing].versions.maximum(0, PAGE as u64) > since);
        assert_eq!(
            r.backings[&later_backing].versions.maximum(0, PAGE as u64),
            later_version
        );
        assert!(r.backings[&untouched].versions.maximum(0, PAGE as u64) <= since);
        // Reused slots must not expose the previous generation's payload.
        let next = journal.reserve().unwrap();
        let pending = r.drain_faults(&journal, 2, false);
        assert_eq!(pending.events, 0);
        assert!(pending.deferred && !pending.conservative);
        journal.publish(next, 2 * PAGE, observed);
        assert_eq!(r.drain_faults(&journal, 2, false).events, 1);
        assert!(r.backings[&later_backing].versions.maximum(0, PAGE as u64) > later_version);
        assert!(r.backings[&untouched].versions.maximum(0, PAGE as u64) <= since);
    }

    #[test]
    fn fault_journal_concurrent_producers_deliver_each_complete_event_once() {
        use std::sync::{Arc, Barrier};
        use std::time::{Duration, Instant};
        const EACH: usize = 256;
        const WRITERS: usize = 4;
        let journal = Arc::new(FaultJournal::<2048>::new());
        let start = Arc::new(Barrier::new(WRITERS + 1));
        let writers: Vec<_> = (0..WRITERS)
            .map(|writer| {
                let (journal, start) = (journal.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    for n in 0..EACH {
                        let id = writer * EACH + n + 1;
                        assert!(journal.push(id * 65536, id as u64 ^ 0x55aa));
                    }
                })
            })
            .collect();
        start.wait();
        let began = Instant::now();
        let mut found = Vec::new();
        while found.len() < WRITERS * EACH {
            journal.drain(|at, epoch, _| {
                let id = at / 65536;
                assert_eq!(
                    epoch,
                    id as u64 ^ 0x55aa,
                    "only complete payloads may be consumed"
                );
                found.push(id);
            });
            assert!(
                began.elapsed() < Duration::from_secs(5),
                "accepted fault events must not be lost"
            );
            std::thread::yield_now();
        }
        for writer in writers {
            writer.join().unwrap();
        }
        found.sort_unstable();
        assert_eq!(found, (1..=WRITERS * EACH).collect::<Vec<_>>());
        assert_eq!(
            journal.drain(|_, _, _| panic!("duplicate event")),
            (0, false)
        );
    }

    #[test]
    fn fault_journal_capacity_and_marker_loss_remain_conservative() {
        const PAGE: usize = 65536;
        for overflow in [true, false] {
            let mut r = Registry::default();
            for at in [PAGE, 2 * PAGE] {
                r.reserved(at, PAGE);
                r.committed(at, PAGE);
            }
            let recorded = MAPPING_SERIAL.load(Ordering::Acquire);
            r.mapping_overflow = !overflow;
            let since = r.clock;
            let journal = FaultJournal::<1>::new();
            assert!(journal.push(PAGE, recorded));
            assert!(!journal.push(2 * PAGE, recorded));
            let result = r.drain_faults(&journal, 4, overflow);
            assert!(result.conservative);
            assert_eq!(result.bytes, (2 * PAGE) as u64);
            assert_eq!(result.mismatched, u64::from(!overflow));
            assert!(
                r.backings
                    .values()
                    .all(|backing| backing.versions.maximum(0, PAGE as u64) > since)
            );
        }
    }

    #[test]
    fn unrelated_vm_changes_do_not_dirty_untouched_backings() {
        const PAGE: usize = 65536;
        let mut r = Registry::default();
        for at in [PAGE, 2 * PAGE, 3 * PAGE] {
            r.reserved(at, PAGE);
            r.committed(at, PAGE);
        }
        let touched = r.lease(PAGE, PAGE).unwrap().parts[0].backing;
        let untouched = r.lease(3 * PAGE, PAGE).unwrap().parts[0].backing;
        let recorded = MAPPING_SERIAL.load(Ordering::Acquire);
        let journal = FaultJournal::<1>::new();
        assert!(journal.push(PAGE, recorded));
        let mid_vm = r.drain_faults(&journal, 3, false);
        assert!(mid_vm.deferred && !mid_vm.conservative && mid_vm.events == 0);
        r.forget(2 * PAGE, PAGE);
        let since = r.clock;
        let result = r.drain_faults(&journal, 6, false);
        assert!(result.events == 1 && !result.conservative);
        assert!(r.backings[&touched].versions.maximum(0, PAGE as u64) > since);
        assert!(r.backings[&untouched].versions.maximum(0, PAGE as u64) <= since);
    }

    #[test]
    fn replacement_dirties_old_and_new_physical_aliases_and_retains_tombstone() {
        const PAGE: usize = 65536;
        let mut r = Registry::default();
        let old = r.backing(PAGE as u64, Provenance::PhysicalSection);
        let new = r.backing(PAGE as u64, Provenance::PhysicalSection);
        r.bind(PAGE, PAGE, old, 0);
        r.bind(2 * PAGE, PAGE, old, 0);
        r.bind(3 * PAGE, PAGE, new, 0);
        let before = r.clock;
        let recorded = MAPPING_SERIAL.load(Ordering::Acquire);
        r.bind(PAGE, PAGE, new, 0);
        assert!(r.backings[&old].versions.maximum(0, PAGE as u64) > before);
        assert!(r.backings[&new].versions.maximum(0, PAGE as u64) > before);
        assert!(!r.fault_mapping_unchanged(PAGE, recorded));
        assert!(r.fault_mapping_unchanged(2 * PAGE, recorded));
        let removed = MAPPING_SERIAL.load(Ordering::Acquire);
        r.forget(PAGE, PAGE);
        assert!(
            !r.fault_mapping_unchanged(PAGE, removed),
            "a missing current VA must retain its change history"
        );
    }

    #[test]
    fn fault_in_partial_block_updates_known_bytes_without_global_dirt() {
        const PAGE: usize = 65536;
        let mut r = Registry::default();
        r.reserved(PAGE + 4096, 4096);
        r.committed(PAGE + 4096, 4096);
        r.reserved(3 * PAGE, PAGE);
        r.committed(3 * PAGE, PAGE);
        let partial = r.lease(PAGE + 4096, 4096).unwrap().parts[0].backing;
        let unrelated = r.lease(3 * PAGE, PAGE).unwrap().parts[0].backing;
        let recorded = MAPPING_SERIAL.load(Ordering::Acquire);
        let since = r.clock;
        let journal = FaultJournal::<1>::new();
        assert!(journal.push_observed(PAGE, recorded, 8));
        let result = r.drain_faults(&journal, 8, false);
        assert!(!result.conservative);
        assert!(r.backings[&partial].versions.maximum(0, 4096) > since);
        assert!(r.backings[&unrelated].versions.maximum(0, PAGE as u64) <= since);
        let unknown = FaultJournal::<1>::new();
        assert!(unknown.push_observed(PAGE, recorded, 9));
        let result = r.drain_faults(&unknown, 10, false);
        assert!(
            result.conservative,
            "an odd-VM observation with incomplete identity stays conservative"
        );
        assert_eq!((result.mismatched, result.uncovered), (0, 1));
    }

    #[test]
    fn fault_after_pre_unmap_metadata_cannot_lose_old_physical_alias_dirt() {
        const PAGE: usize = 65536;
        let mut r = Registry::default();
        let old = r.backing(PAGE as u64, Provenance::PhysicalSection);
        let unrelated = r.backing(PAGE as u64, Provenance::PhysicalSection);
        r.bind(PAGE, PAGE, old, 0);
        r.bind(2 * PAGE, PAGE, old, 0);
        r.bind(3 * PAGE, PAGE, unrelated, 0);
        let since = r.clock;
        // Native unmap has not run yet; the releasing hook already removed
        // the identity. Its old physical dirt remains visible after the VM
        // writer ends even if the fault sampled the new observation serial.
        r.forget(PAGE, PAGE);
        assert!(r.backings[&old].versions.maximum(0, PAGE as u64) > since);
        assert!(r.backings[&unrelated].versions.maximum(0, PAGE as u64) <= since);
        let recorded = MAPPING_SERIAL.load(Ordering::Acquire);
        let journal = FaultJournal::<1>::new();
        assert!(journal.push(PAGE, recorded));
        let result = r.drain_faults(&journal, 10, false);
        assert!(
            result.conservative,
            "an unbound VA cannot qualify an odd/changed VM observation"
        );
        assert!(r.backings[&old].versions.maximum(0, PAGE as u64) > since);
        assert!(
            r.backings[&unrelated].versions.maximum(0, PAGE as u64) > since,
            "the unknown-VA fallback is deliberately global"
        );
    }

    #[test]
    fn lifecycle_blocks_reuse_and_old_gpu_result_cannot_overwrite_new_bytes() {
        use std::sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
            mpsc,
        };
        use std::time::Duration;
        let bytes = Arc::new(AtomicU64::new(1));
        let at = Arc::as_ptr(&bytes) as usize;
        {
            let mut r = registry();
            r.reserved(at, 8);
            r.committed(at, 8);
        }
        let old = registry().lease(at, 8).unwrap();
        let reader = LIFECYCLE.read().unwrap();
        let (entered, started) = mpsc::channel();
        let (done, completed) = mpsc::channel();
        let other = bytes.clone();
        let worker = std::thread::spawn(move || {
            entered.send(()).unwrap();
            let _writer = LIFECYCLE.write().unwrap();
            {
                let mut r = registry();
                r.reserved(at, 8);
                r.committed(at, 8);
            }
            other.store(9, Ordering::Release);
            done.send(()).unwrap();
        });
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            completed.recv_timeout(Duration::from_millis(30)).is_err(),
            "reuse must wait for publication"
        );
        assert!(registry().valid(&old));
        drop(reader);
        completed.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        let _reader = LIFECYCLE.read().unwrap();
        if old.current_version().is_some() {
            bytes.store(2, Ordering::Release);
        }
        assert_eq!(
            bytes.load(Ordering::Acquire),
            9,
            "old GPU result must not overwrite new guest bytes"
        );
    }
}
