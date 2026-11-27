//! Per-list D3D12 state tracking for buffers used directly on a [`HostList`].
//!
//! Card-side title buffers (`Shadow::buffer`, `DEFAULT`/`CUSTOM`/`GPU_UPLOAD`
//! heaps) are created in `D3D12_RESOURCE_STATE_COMMON` and decay back to
//! `COMMON` at the end of every `ExecuteCommandLists` call. Within one
//! `ExecuteCommandLists` call, however, implicit promotion to a write state
//! (such as `COPY_DEST` in `HostList::place_copy` or `UNORDERED_ACCESS` in a
//! direct root UAV) does not decay between commands or between lists of the
//! same batch, and a buffer already promoted to a read state cannot implicitly
//! promote to `COPY_DEST`.
//!
//! Each `HostList` tracks the buffers it directly promotes or transitions so
//! that:
//! - `place_copy` / `place_source` emit the exact `StateBefore` transition when
//!   a buffer was already promoted earlier in the list, and restore copied
//!   buffers to `COMMON` afterwards so later root views (and the submission's
//!   compare list `plan.check`) can promote cleanly;
//! - at submission time, `HostList::prelude` bridges any buffer left non-common
//!   by an earlier list in the same `ExecuteCommandLists` batch, and
//!   `plan.check` transitions any buffer still in a non-read state to
//!   `NON_PIXEL_SHADER_RESOURCE`.

use super::*;

pub(crate) const COMMON: u32 = D3D12_RESOURCE_STATE_COMMON.0 as u32;
pub(crate) const NON_PIXEL_SRV: u32 = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE.0 as u32;

/// Pure D3D12 buffer-state tracker within a single command list recording,
/// assuming the buffer enters the list in `COMMON` (which `prelude` guarantees
/// whenever [`Track::needs_common`] is true).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Track {
    /// Whether this list either wrote the buffer or emitted an explicit
    /// `ResourceBarrier` on it (whose `StateBefore` assumed entry in `COMMON`).
    /// False while the list has only done read-only implicit promotions.
    pub(crate) needs_common: bool,
    /// D3D12 state after the commands recorded so far (`0` == `COMMON`).
    pub(crate) current: u32,
    /// True while `current` was reached solely by implicit read-only promotion
    /// from `COMMON`, so further read-only states can be OR'd in implicitly.
    pub(crate) promoted_read: bool,
}

impl Track {
    pub(crate) const fn new() -> Self {
        Self {
            needs_common: false,
            current: COMMON,
            promoted_read: false,
        }
    }

    /// Records a direct read requiring `bits` (one or more read-only state
    /// bits). Returns `Some((before, after))` when an explicit transition
    /// barrier must be recorded before the read.
    pub(crate) fn read(&mut self, bits: u32) -> Option<(u32, u32)> {
        if self.current == COMMON {
            self.current = bits;
            self.promoted_read = true;
            None
        } else if self.promoted_read {
            self.current |= bits;
            None
        } else if (self.current & bits) == bits {
            None
        } else {
            let before = self.current;
            self.current = bits;
            self.promoted_read = false;
            self.needs_common = true;
            Some((before, bits))
        }
    }

    /// Records a direct write requiring exclusive state `bit` (`COPY_DEST` or
    /// `UNORDERED_ACCESS`). Returns `Some((before, after))` when an explicit
    /// transition barrier must be recorded before the write.
    pub(crate) fn write(&mut self, bit: u32) -> Option<(u32, u32)> {
        self.needs_common = true;
        if self.current == COMMON {
            self.current = bit;
            self.promoted_read = false;
            None
        } else if self.current == bit {
            None
        } else {
            let before = self.current;
            self.current = bit;
            self.promoted_read = false;
            Some((before, bit))
        }
    }

    /// Transitions the buffer back to `COMMON` if it is not already there.
    pub(crate) fn reset_to_common(&mut self) -> Option<(u32, u32)> {
        if self.current == COMMON {
            None
        } else {
            let before = self.current;
            self.current = COMMON;
            self.promoted_read = false;
            self.needs_common = true;
            Some((before, COMMON))
        }
    }
}

/// Cross-list buffer state carried across lists executed in the same
/// `ExecuteCommandLists` batch (cleared at every `ExecuteCommandLists`
/// boundary where D3D12 decays buffers back to `COMMON`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LiveState {
    pub(crate) state: u32,
    pub(crate) promoted_read: bool,
}

impl LiveState {
    /// Updates `live` for a list whose recorded buffer track is `entry`.
    /// Returns `Some((before, COMMON))` when the list's prelude must transition
    /// the buffer back to `COMMON` before the list runs.
    pub(crate) fn bridge_list(live: &mut Option<Self>, entry: Track) -> Option<(u32, u32)> {
        match *live {
            None => {
                *live = (entry.current != COMMON).then_some(Self {
                    state: entry.current,
                    promoted_read: entry.promoted_read,
                });
                None
            }
            Some(prev) if prev.promoted_read && !entry.needs_common => {
                *live = Some(Self {
                    state: prev.state | entry.current,
                    promoted_read: true,
                });
                None
            }
            Some(prev) => {
                *live = (entry.current != COMMON).then_some(Self {
                    state: entry.current,
                    promoted_read: entry.promoted_read,
                });
                (prev.state != COMMON).then_some((prev.state, COMMON))
            }
        }
    }

    /// Returns `Some((before, NON_PIXEL_SRV))` if `plan.check`'s
    /// `SetComputeRootShaderResourceView` cannot legally read a buffer left
    /// in `live` by earlier lists of the same `ExecuteCommandLists` batch.
    pub(crate) fn bridge_check(live: Option<Self>) -> Option<(u32, u32)> {
        let prev = live?;
        if prev.state != COMMON && !prev.promoted_read && (prev.state & NON_PIXEL_SRV) == 0 {
            Some((prev.state, NON_PIXEL_SRV))
        } else {
            None
        }
    }
}

pub(crate) struct Entry {
    pub(crate) track: Track,
    /// Keeps the resource alive while this recording holds borrowed pointers
    /// to it in prelude or deferred barriers.
    pub(crate) buffer: ID3D12Resource,
}

pub(crate) type BufferStates = std::collections::HashMap<usize, Entry>;
pub(super) type LiveBuffers = std::collections::HashMap<usize, LiveState>;

fn state_of(bits: u32) -> D3D12_RESOURCE_STATES {
    D3D12_RESOURCE_STATES(bits as i32)
}

impl HostList {
    /// Updates the tracked state for a direct read of `buffer` and returns any
    /// required pre-transition barrier.
    pub(super) fn buffer_read_barrier(
        &self,
        buffer: &ID3D12Resource,
        needed: D3D12_RESOURCE_STATES,
    ) -> Option<D3D12_RESOURCE_BARRIER> {
        let key = buffer.as_raw() as usize;
        let mut states = self.buffer_states.lock().unwrap_or_else(|e| e.into_inner());
        let entry = states.entry(key).or_insert_with(|| Entry {
            track: Track::new(),
            buffer: buffer.clone(),
        });
        entry
            .track
            .read(needed.0 as u32)
            .map(|(before, after)| transition(buffer, state_of(before), state_of(after)))
    }

    /// Updates the tracked state for a direct write to `buffer` and returns any
    /// required pre-transition barrier.
    pub(super) fn buffer_write_barrier(
        &self,
        buffer: &ID3D12Resource,
        needed: D3D12_RESOURCE_STATES,
    ) -> Option<D3D12_RESOURCE_BARRIER> {
        let key = buffer.as_raw() as usize;
        let mut states = self.buffer_states.lock().unwrap_or_else(|e| e.into_inner());
        let entry = states.entry(key).or_insert_with(|| Entry {
            track: Track::new(),
            buffer: buffer.clone(),
        });
        entry
            .track
            .write(needed.0 as u32)
            .map(|(before, after)| transition(buffer, state_of(before), state_of(after)))
    }

    /// Transitions `buffer` back to `COMMON` if this list currently tracks it
    /// in a non-common state.
    pub(super) fn buffer_common_barrier(&self, buffer: &ID3D12Resource) -> Option<D3D12_RESOURCE_BARRIER> {
        let key = buffer.as_raw() as usize;
        let mut states = self.buffer_states.lock().unwrap_or_else(|e| e.into_inner());
        let entry = states.get_mut(&key)?;
        entry
            .track
            .reset_to_common()
            .map(|(before, after)| transition(buffer, state_of(before), state_of(after)))
    }

    /// Records a direct read of `buffer`, emitting a transition barrier on this
    /// list if `buffer` is currently in an incompatible state.
    pub(crate) fn buffer_read(&self, buffer: &ID3D12Resource, needed: D3D12_RESOURCE_STATES) {
        if let Some(barrier) = self.buffer_read_barrier(buffer, needed) {
            // SAFETY: an open list and a live buffer kept by `buffer_states`.
            unsafe { self.list.ResourceBarrier(&[barrier]) };
        }
    }

    /// Records a direct write to `buffer`, emitting a transition barrier on
    /// this list if `buffer` is currently in a different non-common state.
    pub(crate) fn buffer_write(&self, buffer: &ID3D12Resource, needed: D3D12_RESOURCE_STATES) {
        if let Some(barrier) = self.buffer_write_barrier(buffer, needed) {
            // SAFETY: an open list and a live buffer kept by `buffer_states`.
            unsafe { self.list.ResourceBarrier(&[barrier]) };
        }
    }

    /// Computes prelude `-> COMMON` transitions for buffers that an earlier
    /// list in the same `ExecuteCommandLists` batch left non-common, and
    /// advances `live` to the states this list leaves them in.
    ///
    /// The returned barriers borrow `ID3D12Resource` pointers owned by
    /// `self.buffer_states`, which stay alive until [`HostList::reset`].
    pub(super) fn prelude_buffer_barriers(&self, live: &mut LiveBuffers) -> Vec<D3D12_RESOURCE_BARRIER> {
        let states = self.buffer_states.lock().unwrap_or_else(|e| e.into_inner());
        let mut barriers = Vec::new();
        for (&key, entry) in states.iter() {
            let mut slot = live.get(&key).copied();
            if let Some((before, after)) = LiveState::bridge_list(&mut slot, entry.track) {
                barriers.push(transition(&entry.buffer, state_of(before), state_of(after)));
            }
            match slot {
                Some(state) => {
                    live.insert(key, state);
                }
                None => {
                    live.remove(&key);
                }
            }
        }
        barriers
    }
}

/// Transitions needed at the start of `plan.check` so every compared shadow
/// buffer is readable by `SetComputeRootShaderResourceView`.
pub(super) fn check_barriers(
    live: &LiveBuffers,
    ranges: &[CardRange],
) -> Vec<D3D12_RESOURCE_BARRIER> {
    if live.is_empty() {
        return Vec::new();
    }
    let mut seen = Vec::new();
    let mut barriers = Vec::new();
    for r in ranges.iter().filter(|r| r.compared) {
        let key = r.shadow.buffer.as_raw() as usize;
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        if let Some((before, after)) = LiveState::bridge_check(live.get(&key).copied()) {
            barriers.push(transition(&r.shadow.buffer, state_of(before), state_of(after)));
        }
    }
    barriers
}

#[cfg(test)]
mod tests {
    use super::*;

    const VB_CBV: u32 = D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER.0 as u32;
    const UAV: u32 = D3D12_RESOURCE_STATE_UNORDERED_ACCESS.0 as u32;
    const COPY_DEST: u32 = D3D12_RESOURCE_STATE_COPY_DEST.0 as u32;
    const COPY_SOURCE: u32 = D3D12_RESOURCE_STATE_COPY_SOURCE.0 as u32;

    #[test]
    fn copy_dest_returns_to_common_for_later_srv_and_check() {
        let mut t = Track::new();
        // Fresh buffer: implicit COMMON -> COPY_DEST, then explicit COPY_DEST -> COMMON.
        assert_eq!(t.write(COPY_DEST), None);
        assert_eq!(t.reset_to_common(), Some((COPY_DEST, COMMON)));
        assert_eq!(t.current, COMMON);
        assert!(t.needs_common);

        // Later compute root SRV in the same list promotes COMMON -> NON_PIXEL_SRV cleanly.
        assert_eq!(t.read(NON_PIXEL_SRV), None);
        assert_eq!(t.current, NON_PIXEL_SRV);
        assert!(t.promoted_read);

        // If `plan.check` follows in the same ExecuteCommandLists, no barrier is needed.
        let mut live = None;
        assert_eq!(LiveState::bridge_list(&mut live, t), None);
        assert_eq!(LiveState::bridge_check(live), None);
    }

    #[test]
    fn read_before_place_copy_transitions_exact_promoted_mask() {
        let mut t = Track::new();
        // Two earlier read promotions combine without barriers.
        assert_eq!(t.read(VB_CBV), None);
        assert_eq!(t.read(NON_PIXEL_SRV), None);
        assert!(!t.needs_common);
        assert_eq!(t.current, VB_CBV | NON_PIXEL_SRV);

        // `place_copy` into the same buffer must transition out of the combined read mask.
        assert_eq!(t.write(COPY_DEST), Some((VB_CBV | NON_PIXEL_SRV, COPY_DEST)));
        assert_eq!(t.reset_to_common(), Some((COPY_DEST, COMMON)));
        assert!(t.needs_common);
    }

    #[test]
    fn same_buffer_place_copy_transitions_source_to_dest_to_common() {
        // Case 1: buffer starts in COMMON.
        let mut t = Track::new();
        assert_eq!(t.read(COPY_SOURCE), None);
        assert_eq!(t.write(COPY_DEST), Some((COPY_SOURCE, COPY_DEST)));
        assert_eq!(t.reset_to_common(), Some((COPY_DEST, COMMON)));

        // Case 2: buffer was already read as NON_PIXEL_SRV earlier in the list.
        let mut t2 = Track::new();
        assert_eq!(t2.read(NON_PIXEL_SRV), None);
        assert_eq!(t2.read(COPY_SOURCE), None);
        assert_eq!(t2.write(COPY_DEST), Some((NON_PIXEL_SRV | COPY_SOURCE, COPY_DEST)));
        assert_eq!(t2.reset_to_common(), Some((COPY_DEST, COMMON)));
    }

    #[test]
    fn cross_list_reads_need_no_prelude_but_later_write_bridges_to_common() {
        let mut list0 = Track::new();
        assert_eq!(list0.read(VB_CBV), None);

        let mut list1 = Track::new();
        assert_eq!(list1.read(NON_PIXEL_SRV), None);

        let mut list2 = Track::new();
        assert_eq!(list2.write(COPY_DEST), None);
        assert_eq!(list2.reset_to_common(), Some((COPY_DEST, COMMON)));

        let mut live = None;
        assert_eq!(LiveState::bridge_list(&mut live, list0), None);
        assert_eq!(LiveState::bridge_list(&mut live, list1), None);
        assert_eq!(
            live,
            Some(LiveState {
                state: VB_CBV | NON_PIXEL_SRV,
                promoted_read: true,
            })
        );
        // list2 writes the buffer, so its prelude must transition the combined read state back to COMMON.
        assert_eq!(
            LiveState::bridge_list(&mut live, list2),
            Some((VB_CBV | NON_PIXEL_SRV, COMMON))
        );
        // list2 ended in COMMON, so `live` is cleared.
        assert_eq!(live, None);
    }

    #[test]
    fn direct_root_uav_is_bridged_before_next_list_and_before_check() {
        let mut list0 = Track::new();
        assert_eq!(list0.write(UAV), None);

        let mut live = None;
        assert_eq!(LiveState::bridge_list(&mut live, list0), None);
        assert_eq!(LiveState::bridge_check(live), Some((UAV, NON_PIXEL_SRV)));

        let mut list1 = Track::new();
        assert_eq!(list1.read(NON_PIXEL_SRV), None);
        assert_eq!(LiveState::bridge_list(&mut live, list1), Some((UAV, COMMON)));
        assert_eq!(LiveState::bridge_check(live), None);
    }
}
