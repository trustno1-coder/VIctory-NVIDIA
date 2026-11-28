//! Opt-in buffer uploads: COPY queue -> private VRAM -> DIRECT queue at
//! the original position. Only DIRECT writes the title's card buffers.
//! Buffers promote from COMMON and decay after ExecuteCommandLists; the
//! copy fence orders the handoff. Slots return only after the drawing fence.
use super::*;
use std::sync::atomic::AtomicU64;

/// The switch is a size gate, in MiB. Unset or `0` keeps every upload on the
/// original path; `1` sends every non-empty upload through the copy queue (the
/// behaviour of the first version); `N` above one only sends uploads of at least
/// `N` MiB, so small handoffs keep their original position and the queue is only
/// used where there is enough work to hide.
fn threshold() -> Option<u64> {
    static THRESHOLD: OnceLock<Option<u64>> = OnceLock::new();
    *THRESHOLD.get_or_init(|| {
        let asked = std::env::var("VICTORY_UPLOAD_QUEUE").ok()?;
        match asked.trim().parse::<u64>().ok()? {
            0 => None,
            1 => Some(0),
            // A MiB count too large to represent is a mistake in the setting, not a threshold: it
            // takes the same path as an unreadable one rather than wrapping into a small gate.
            mebibytes => mebibytes.checked_mul(1 << 20),
        }
    })
}

pub(super) fn on() -> bool {
    threshold().is_some()
}

/// Whether an upload of `total` bytes belongs on the copy queue at this gate.
pub(super) fn wants(total: u64) -> bool {
    gate(threshold(), total)
}

fn gate(threshold: Option<u64>, total: u64) -> bool {
    match threshold {
        Some(threshold) => total != 0 && total <= 512 << 20 && total >= threshold,
        None => false,
    }
}

pub(super) struct Slot {
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    bounce: Option<(ID3D12Resource, u64)>,
}

impl Slot {
    pub(super) fn bounce(&self) -> Option<&ID3D12Resource> {
        self.bounce.as_ref().map(|b| &b.0)
    }
}

struct Queue {
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    sent: u64,
    free: Vec<Slot>,
}

// SAFETY: free-threaded D3D12 objects; each slot has one owning submission.
unsafe impl Send for Slot {}
// SAFETY: queue access and the free slots are serialized by QUEUE.
unsafe impl Send for Queue {}
static QUEUE: Mutex<Option<Option<Queue>>> = Mutex::new(None);
static STATS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

fn make_queue(device: &ID3D12Device) -> Option<Queue> {
    // SAFETY: plain object creation.
    let made = unsafe {
        (|| -> windows::core::Result<Queue> {
            let queue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_COPY,
                ..Default::default()
            })?;
            let fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            Ok(Queue {
                queue,
                fence,
                sent: 0,
                free: Vec::new(),
            })
        })()
    };
    match made {
        Ok(q) => {
            tracing::info!("VICTORY_UPLOAD_QUEUE=1: independent buffer copy queue ready");
            Some(q)
        }
        Err(e) => {
            tracing::warn!(error = %e, "upload copy queue unavailable; using original uploads");
            None
        }
    }
}

/// None means nothing was submitted; the caller must record its original uploads.
/// # Safety
/// The source's first `total` bytes are initialized and immutable until drawing finishes.
/// The caller holds drawing submission order and retains the returned slot too.
pub(super) unsafe fn send(
    host: &Host,
    drawing: &ID3D12CommandQueue,
    source: &ID3D12Resource,
    total: u64,
) -> Option<Slot> {
    if total == 0 || total > 512 << 20 {
        return None;
    }
    let mut guard = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    let q = guard.get_or_insert_with(|| make_queue(&host.device)).as_mut()?;
    // SAFETY: free slots have completed their drawing fence; caller retains sources.
    let made = unsafe {
        (|| -> windows::core::Result<Slot> {
            let mut slot = match q.free.pop() {
                Some(slot) => slot,
                None => {
                    let allocator = host.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)?;
                    let list: ID3D12GraphicsCommandList =
                        host.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)?;
                    list.Close()?;
                    Slot {
                        allocator,
                        list,
                        bounce: None,
                    }
                }
            };
            if slot.bounce.as_ref().is_none_or(|b| b.1 < total) {
                slot.bounce = None;
                let size = total.next_power_of_two().max(4 << 20);
                let buffer = host.uav_buffer(size).ok_or_else(windows::core::Error::empty)?;
                slot.bounce = Some((buffer, size));
            }
            let bounce = slot.bounce().unwrap();
            slot.allocator.Reset()?;
            slot.list.Reset(&slot.allocator, None)?;
            slot.list.CopyBufferRegion(bounce, 0, source, 0, total);
            slot.list.Close()?;
            let list = slot.list.cast()?;
            q.queue.ExecuteCommandLists(&[Some(list)]);
            q.sent += 1;
            // After Execute, failure must never return a live slot to the pool.
            if let Err(e) = q.queue.Signal(&q.fence, q.sent) {
                fail(format_args!("upload copy queue signal failed ({e})"));
            }
            if let Err(e) = drawing.Wait(&q.fence, q.sent) {
                tracing::warn!(error = %e, "upload queue GPU wait failed; waiting on CPU");
                if let Err(e) = q.fence.SetEventOnCompletion(q.sent, HANDLE::default()) {
                    fail(format_args!("upload copy queue CPU wait failed ({e})"));
                }
            }
            STATS[0].fetch_add(1, Ordering::Relaxed);
            STATS[1].fetch_add(total, Ordering::Relaxed);
            Ok(slot)
        })()
    };
    match made {
        Ok(slot) => Some(slot),
        Err(e) => {
            tracing::warn!(error = %e, "upload queue preparation failed; using original uploads");
            None
        }
    }
}

/// Called only after the plan's DIRECT fence, which follows the COPY fence.
pub(super) fn give_back(slot: Slot) {
    let mut guard = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(Some(q)) = guard.as_mut() {
        q.free.push(slot);
        while q.free.iter().map(|s| s.bounce.as_ref().map_or(0, |b| b.1)).sum::<u64>() > 256 << 20 {
            q.free.pop();
        }
    }
}

pub(super) fn fallback() {
    STATS[2].fetch_add(1, Ordering::Relaxed);
}

pub(super) fn line(frames: f64) {
    if let Some(threshold) = threshold() {
        let [sent, bytes, fallback] = [0, 1, 2].map(|i| STATS[i].swap(0, Ordering::Relaxed) as f64 / frames);
        if threshold == 0 {
            tracing::info!(
                "VICTORY_UPLOAD_QUEUE: {sent:.1} uploads/frame, {:.1} MB/frame, {fallback:.1} fallbacks/frame",
                bytes / 1048576.0
            );
        } else {
            tracing::info!(
                "VICTORY_UPLOAD_QUEUE at {} MiB: {sent:.1} uploads/frame, {:.1} MB/frame, {fallback:.1} fallbacks/frame",
                threshold >> 20,
                bytes / 1048576.0
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_sends_only_what_clears_it() {
        // Unset and 0 keep every upload on the original path.
        assert!(!gate(None, 4096));
        // 1 means every non-empty upload, which is what the first version did.
        assert!(!gate(Some(0), 0));
        assert!(gate(Some(0), 1));
        assert!(gate(Some(0), 4096));
        // A size gate sends only the large handoffs, and still refuses absurd ones.
        assert!(!gate(Some(4 << 20), 4096));
        assert!(gate(Some(4 << 20), 4 << 20));
        assert!(gate(Some(4 << 20), 8 << 20));
        assert!(!gate(Some(4 << 20), (512 << 20) + 1));
    }

    #[test]
    fn a_threshold_too_large_to_hold_is_not_a_threshold() {
        // What `threshold` does with its MiB count, without reading the environment (which a test
        // cannot change once the switch has been read).
        let from_mib = |mib: u64| match mib {
            0 => None,
            1 => Some(0),
            mib => mib.checked_mul(1 << 20),
        };
        assert_eq!(from_mib(0), None);
        assert_eq!(from_mib(1), Some(0));
        assert_eq!(from_mib(4), Some(4 << 20));
        // A count that would wrap must not turn into a small gate.
        assert_eq!(from_mib(u64::MAX), None);
        assert_eq!(from_mib(u64::MAX / (1 << 20) + 1), None);
    }

    #[test]
    fn uploads_preserve_bytes_and_reuse_only_after_drawing() {
        let host = host().expect("D3D12 device required for upload queue test");
        // SAFETY: test-owned buffers; all commands finish before resources are reused.
        unsafe {
            let (source, data) = host.upload_buffer(1024).unwrap();
            let (readback, result) = host.shared_buffer(1024).unwrap();
            let card = host.uav_buffer(1024).unwrap();
            let (allocator, list) = host.make_prelude().unwrap();
            let fence: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            assert!(send(host, &host.queue, &source, 0).is_none());
            assert!(send(host, &host.queue, &source, (512 << 20) + 1).is_none());
            for round in 1..=3u64 {
                let expected: Vec<u8> = (0..1024).map(|i| (i as u8).wrapping_mul(37).wrapping_add(round as u8)).collect();
                std::ptr::copy_nonoverlapping(expected.as_ptr(), data, 1024);
                allocator.Reset().unwrap();
                list.Reset(&allocator, None).unwrap();
                // Unaligned sizes/offsets, separated ranges, and a repeated source span.
                let slot = send(host, &host.queue, &source, 1024).expect("copy queue must upload");
                let bounce = slot.bounce().unwrap();
                for (dst, src, len) in [(0, 3, 13), (300, 311, 257), (600, 3, 13)] {
                    list.CopyBufferRegion(&card, dst, bounce, src, len);
                }
                list.ResourceBarrier(&[transition(&card, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                list.CopyBufferRegion(&readback, 0, &card, 0, 1024);
                list.Close().unwrap();
                host.queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
                host.queue.Signal(&fence, round).unwrap();
                fence.SetEventOnCompletion(round, HANDLE::default()).unwrap();
                let bytes = std::slice::from_raw_parts(result, 1024);
                assert_eq!(&bytes[..13], &expected[3..16]);
                assert_eq!(&bytes[300..557], &expected[311..568]);
                assert_eq!(&bytes[600..613], &expected[3..16]);
                give_back(slot);
            }
        }
    }
}
