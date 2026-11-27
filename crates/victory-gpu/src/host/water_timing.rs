//! GPU timestamps for the water-light dispatches, resolved in the same list
//! and read when it lands. No fence or queue submission is added for timing.

use std::sync::atomic::{AtomicU64, Ordering};

use super::*;

const HASHES: [&str; 5] = [
    "5dc3b25986622949", // tiled
    "6f225b641c5b0708", // tiled local lights
    "6871d6e4b3ac9a10", // full volume
    "a10179ab7eda0dc3", // volume portals
    "926c46bab3b86bd6", // submerged
];
const LIMIT: u32 = 16;
static RUNS: [AtomicU64; HASHES.len()] = [const { AtomicU64::new(0) }; HASHES.len()];
static NS: [AtomicU64; HASHES.len()] = [const { AtomicU64::new(0) }; HASHES.len()];

pub(crate) fn kind(pipeline: &crate::pipeline::host::HostPipeline) -> Option<usize> {
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| crate::switch::on("VICTORY_WATER_LIGHT")) {
        return None;
    }
    let hash = &pipeline.stages.first()?.shader.hash;
    HASHES.iter().position(|&known| known == hash)
}

struct Stamps {
    heap: ID3D12QueryHeap,
    back: ID3D12Resource,
    data: usize,
    frequency: u64,
}

#[derive(Default)]
pub(super) struct List {
    stamps: OnceLock<Option<Stamps>>,
    passes: Mutex<Vec<usize>>,
}

impl List {
    fn stamps(&self) -> Option<&Stamps> {
        self.stamps.get_or_init(|| {
            let host = host()?;
            let mut heap = None;
            // SAFETY: an ordinary timestamp heap and a query of the host queue.
            let frequency = unsafe {
                host.device.CreateQueryHeap(
                    &D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: LIMIT * 2, NodeMask: 0 },
                    &mut heap,
                ).ok()?;
                host.queue.GetTimestampFrequency().ok()?
            };
            if frequency == 0 {
                return None;
            }
            let mut back: Option<ID3D12Resource> = None;
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: u64::from(LIMIT) * 16,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            // SAFETY: a readback buffer in the state required by ResolveQueryData.
            unsafe {
                host.device.CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() },
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut back,
                ).ok()?;
            }
            let back = back?;
            let mut data = std::ptr::null_mut();
            // SAFETY: the readback resource is kept alive with its mapping.
            unsafe { back.Map(0, None, Some(&mut data)).ok()? };
            if data.is_null() {
                return None;
            }
            Some(Stamps { heap: heap?, back, data: data as usize, frequency })
        }).as_ref()
    }

    pub(super) fn begin(&self, list: &ID3D12GraphicsCommandList, kind: usize) -> Option<u32> {
        let mut passes = self.passes.lock().unwrap_or_else(|e| e.into_inner());
        let index = passes.len() as u32;
        if index >= LIMIT {
            return None;
        }
        let stamps = self.stamps()?;
        passes.push(kind);
        // SAFETY: the open list records into its own heap, with index below LIMIT * 2.
        unsafe { list.EndQuery(&stamps.heap, D3D12_QUERY_TYPE_TIMESTAMP, index * 2) };
        Some(index)
    }

    pub(super) fn end(&self, list: &ID3D12GraphicsCommandList, index: u32) {
        if let Some(Some(stamps)) = self.stamps.get() {
            // SAFETY: begin recorded the first timestamp for this bounded pair.
            unsafe { list.EndQuery(&stamps.heap, D3D12_QUERY_TYPE_TIMESTAMP, index * 2 + 1) };
        }
    }

    pub(super) fn resolve(&self, list: &ID3D12GraphicsCommandList) {
        let Some(Some(stamps)) = self.stamps.get() else { return };
        let count = self.passes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32;
        if count != 0 {
            // SAFETY: all these queries were recorded on this list and fit the result buffer.
            unsafe { list.ResolveQueryData(&stamps.heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, count * 2, &stamps.back, 0) };
        }
    }

    pub(super) fn collect(&self) {
        let Some(Some(stamps)) = self.stamps.get() else { return };
        let passes = self.passes.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: the completed list resolved two timestamp values per pass.
        let data = unsafe { std::slice::from_raw_parts(stamps.data as *const u64, passes.len() * 2) };
        for (&kind, pair) in passes.iter().zip(data.chunks_exact(2)) {
            let ticks = pair[1].saturating_sub(pair[0]);
            if ticks != 0 {
                RUNS[kind].fetch_add(1, Ordering::Relaxed);
                NS[kind].fetch_add((u128::from(ticks) * 1_000_000_000 / u128::from(stamps.frequency)) as u64, Ordering::Relaxed);
            }
        }
    }

    pub(super) fn reset(&self) {
        if self.stamps.get().is_some() {
            self.passes.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
    }
}

pub(super) fn log(frames: f64) {
    for (kind, hash) in HASHES.into_iter().enumerate() {
        let ns = NS[kind].swap(0, Ordering::Relaxed);
        let runs = RUNS[kind].swap(0, Ordering::Relaxed);
        if runs != 0 {
            tracing::info!(hash, runs, ms_per_frame = ns as f64 / 1e6 / frames.max(1.0), "water light GPU time");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_completed_list_resolves_water_timestamps() {
        let Some(host) = host() else { return };
        let (allocator, list) = host.make_prelude().expect("timestamp test list");
        // SAFETY: the list has just been created and closed.
        unsafe {
            allocator.Reset().unwrap();
            list.Reset(&allocator, None).unwrap();
        }
        let timing = List::default();
        let first = timing.begin(&list, 0).expect("timestamp pair");
        timing.end(&list, first);
        timing.resolve(&list);
        let mut submitted = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: this queue, fence and event belong to the host and the
        // submission lock excludes other users of the event.
        unsafe {
            list.Close().unwrap();
            host.queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
            *submitted += 1;
            host.queue.Signal(&host.fence, *submitted).unwrap();
            host.fence.SetEventOnCompletion(*submitted, host.event).unwrap();
            WaitForSingleObject(host.event, INFINITE);
        }
        let stamps = timing.stamps().unwrap();
        // SAFETY: the completed list resolved the first pair of timestamps.
        let pair = unsafe { std::slice::from_raw_parts(stamps.data as *const u64, 2) };
        assert!(pair[0] > 0 && pair[1] >= pair[0], "{pair:?}");
        timing.collect();
        timing.reset();
        assert!(timing.passes.lock().unwrap().is_empty());
    }
}
