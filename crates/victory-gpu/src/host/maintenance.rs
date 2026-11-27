//! Fence-owned batches of the existing card-piece hash/compare work.
//!
//! Preparation is fallible and changes no command list. Recording only binds
//! immutable objects and dispatches. A recorded batch cannot reenter the pool
//! until its covering native fence is proved complete, including on error paths.

use std::collections::HashMap;
use std::sync::Arc;

use super::*;
use crate::shadow::CardSpan;

const VIEWS: usize = 32;
const SLOTS: u32 = (VIEWS * 2) as u32;
const ROW_BYTES: u64 = 32;
const MOST: usize = D3D12_CS_DISPATCH_MAX_THREAD_GROUPS_PER_DIMENSION as usize;
const FREE_BYTES: u64 = 32 << 20;
const FREE_HEAPS: usize = 256;

pub(super) struct Job {
    pub(super) source: ID3D12Resource,
    pub(super) hashes: ID3D12Resource,
    pub(super) span: CardSpan,
    pub(super) size: u32,
    /// DWORD offset, exactly as passed to the legacy `card_pass`.
    pub(super) out_at: u32,
    pub(super) mode: u32,
}

#[derive(Clone, Copy)]
struct Spec {
    source: usize,
    hashes: usize,
    span: CardSpan,
    size: u32,
    out_at: u32,
    mode: u32,
}

struct Part {
    sources: Vec<usize>,
    hashes: Vec<usize>,
    rows: Vec<[u32; 8]>,
}

/// Check the shader's complete footprint, rather than just the guest range.
fn footprint(s: &Spec) -> Option<(u64, u64)> {
    if s.mode > 2 || s.span.pieces == 0 || s.size == 0 || s.size % 16 != 0 || s.span.skew >= 65536 {
        return None;
    }
    if s.span.from >= s.span.to || s.span.to > s.size {
        return None;
    }
    let end = s.span.first.checked_add(s.span.pieces)?;
    // The legacy shader multiplies (piece + 1) in 32 bits.
    if u64::from(end) * 65536 > u64::from(u32::MAX) {
        return None;
    }
    let lo = (u64::from(s.span.first) * 65536).saturating_sub(u64::from(s.span.skew)) & !15;
    let last_lo = (u64::from(end - 1) * 65536).saturating_sub(u64::from(s.span.skew));
    let hi = (u64::from(end) * 65536 - u64::from(s.span.skew))
        .min(u64::from(s.size))
        .next_multiple_of(16);
    (last_lo < u64::from(s.size) && lo < hi && hi <= u64::from(s.size)).then_some((lo, hi))
}

/// Read-only repeats are independent. Any repeat involving a hash write falls
/// back as a whole phase: its flags may otherwise acquire different semantics.
fn plan(specs: &[Spec], differs_bytes: u64) -> Option<Vec<Part>> {
    if specs.is_empty() {
        return None;
    }
    let mut pieces = HashMap::new();
    let mut outputs = Vec::new();
    for s in specs {
        footprint(s)?;
        if s.mode != 0 {
            let end = u64::from(s.out_at)
                .checked_add(u64::from(s.span.pieces))?
                .checked_mul(4)?;
            if end > differs_bytes || end > u64::from(u32::MAX) {
                return None;
            }
            outputs.push((u64::from(s.out_at) * 4, end));
        }
        for piece in s.span.first..s.span.first + s.span.pieces {
            let writes = s.mode != 1;
            if let Some(old_writes) = pieces.insert((s.hashes, piece), writes)
                && (old_writes || writes)
            {
                return None;
            }
        }
    }
    outputs.sort_unstable();
    if outputs.windows(2).any(|p| p[0].1 > p[1].0) {
        return None;
    }
    let mut parts = Vec::new();
    let mut part = Part {
        sources: Vec::new(),
        hashes: Vec::new(),
        rows: Vec::new(),
    };
    for s in specs {
        for i in 0..s.span.pieces {
            let source = part.sources.iter().position(|&r| r == s.source);
            let hash = part.hashes.iter().position(|&r| r == s.hashes);
            if part.rows.len() == MOST
                || source.is_none() && part.sources.len() == VIEWS
                || hash.is_none() && part.hashes.len() == VIEWS
            {
                parts.push(part);
                part = Part {
                    sources: Vec::new(),
                    hashes: Vec::new(),
                    rows: Vec::new(),
                };
            }
            let source = match part.sources.iter().position(|&r| r == s.source) {
                Some(at) => at,
                None => {
                    part.sources.push(s.source);
                    part.sources.len() - 1
                }
            };
            let hash = match part.hashes.iter().position(|&r| r == s.hashes) {
                Some(at) => at,
                None => {
                    part.hashes.push(s.hashes);
                    part.hashes.len() - 1
                }
            };
            let out = if s.mode == 0 { 0 } else { (s.out_at + i) * 4 };
            part.rows.push([
                source as u32,
                hash as u32,
                s.span.first + i,
                s.span.skew,
                s.size,
                out,
                s.mode,
                0,
            ]);
        }
    }
    if !part.rows.is_empty() {
        parts.push(part);
    }
    Some(parts)
}

const HEADER: &str = r#"
#define RS "RootConstants(num32BitConstants=2, b0), SRV(t32), DescriptorTable(SRV(t0, numDescriptors=32)), DescriptorTable(UAV(u0, numDescriptors=32)), UAV(u32)"
cbuffer Batch : register(b0) { uint first_job; uint job_count; };
ByteAddressBuffer jobs : register(t32);
ByteAddressBuffer sources[32] : register(t0);
RWByteAddressBuffer hashes[32] : register(u0);
RWByteAddressBuffer differs : register(u32);
groupshared uint4 parts[64];
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_GroupID, uint thread : SV_GroupIndex) {
    if (id.x >= job_count) return;
    uint4 a = jobs.Load4((first_job + id.x) * 32u);
    uint4 b = jobs.Load4((first_job + id.x) * 32u + 16u);
    uint source = a.x, hash = a.y, piece = a.z, skew = a.w;
    uint size = b.x, out_at = b.y, compare = b.z;
"#;

/// Reuse the legacy reduction verbatim, including its load ordering and hash
/// folding. Only resource selection and the already-expanded output offset differ.
fn shader_source() -> Option<String> {
    const START: &str = "    uint lo = piece * 65536u;";
    let (_, body) = CARD_COMPARE.split_once(START)?;
    let body = body
        .replace(
            "now.Load4(",
            "sources[NonUniformResourceIndex(source)].Load4(",
        )
        .replace("hashes.", "hashes[NonUniformResourceIndex(hash)].")
        .replace("out_at + id.x * 4u", "out_at");
    Some(format!("{HEADER}{START}{body}"))
}

struct Pass {
    _device: ID3D12Device,
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

/// Keep the device alive for failures too: a recycled native address must not
/// inherit a previous device's negative capability/compilation result.
struct CachedPass {
    device: ID3D12Device,
    pass: Option<Arc<Pass>>,
}

fn pass(host: &Host) -> Option<Arc<Pass>> {
    static PASSES: Mutex<Vec<CachedPass>> = Mutex::new(Vec::new());
    let mut passes = PASSES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = passes
        .iter()
        .find(|entry| entry.device.as_raw() == host.device.as_raw())
    {
        return entry.pass.clone();
    }
    let result = make_pass(host);
    let pass = match result {
        Ok(pass) => Some(pass),
        Err(reason) => {
            tracing::warn!(
                reason,
                "GPU maintenance batches unavailable on this device; retaining legacy passes"
            );
            None
        }
    };
    passes.push(CachedPass {
        device: host.device.clone(),
        pass: pass.clone(),
    });
    pass
}

fn make_pass(host: &Host) -> Result<Arc<Pass>, String> {
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
    // SAFETY: the feature structure has exactly the advertised layout/size.
    unsafe {
        host.device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
            .map_err(|_| "resource binding capability query failed")?;
    }
    if options.ResourceBindingTier.0 < D3D12_RESOURCE_BINDING_TIER_2.0 {
        return Err("resource binding tier below tier 2".into());
    }
    let source = shader_source().ok_or("legacy hash shader body unavailable")?;
    // Keep compiler diagnostics in the single cached warning, rather than the
    // generic compile helper's separate warning plus another phase warning.
    let code = compile_batch(&source)?;
    // SAFETY: live compiled bytecode, containing its root signature.
    let pass = unsafe {
        let root: ID3D12RootSignature = host
            .device
            .CreateRootSignature(0, &code)
            .map_err(|_| "batched hash root signature creation failed")?;
        let pso = host
            .device
            .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.as_ptr().cast(),
                    BytecodeLength: code.len(),
                },
                ..Default::default()
            })
            .map_err(|_| "batched hash pipeline creation failed")?;
        Arc::new(Pass {
            _device: host.device.clone(),
            root,
            pso,
        })
    };
    Ok(pass)
}

fn compile_batch(source: &str) -> Result<Vec<u8>, String> {
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: all source/name arguments and output storage live through the
    // call. Returned blobs own their bytes until they have been copied below.
    unsafe {
        let compiled = D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            windows::core::s!("maintenance batches"),
            None,
            None,
            windows::core::s!("main"),
            windows::core::s!("cs_5_1"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        );
        if let Err(error) = compiled {
            let detail = errors
                .as_ref()
                .map(|blob| {
                    String::from_utf8_lossy(std::slice::from_raw_parts(
                        blob.GetBufferPointer().cast::<u8>(),
                        blob.GetBufferSize(),
                    ))
                    .into_owned()
                })
                .unwrap_or_default();
            return Err(format!(
                "batched hash shader compilation failed: {error}; {detail}"
            ));
        }
        let code = code.ok_or("batched hash compiler returned no bytecode")?;
        Ok(
            std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize())
                .to_vec(),
        )
    }
}

struct Storage {
    device: ID3D12Device,
    table: ID3D12Resource,
    data: usize,
    bytes: u64,
    heap: ID3D12DescriptorHeap,
}

// SAFETY: free-threaded native objects; the mapping has one CPU owner and is
// never modified while recorded GPU work may read it.
unsafe impl Send for Storage {}

struct Chunk {
    storage: Storage,
    rows: u32,
}

struct Pending {
    chunks: Vec<Chunk>,
    _pins: Vec<ID3D12Resource>,
    _pass: Arc<Pass>,
    fence: Option<(ID3D12Fence, u64)>,
}

#[derive(Default)]
struct Pool {
    free: Vec<Storage>,
    pending: Vec<Pending>,
}

static POOL: Mutex<Pool> = Mutex::new(Pool {
    free: Vec::new(),
    pending: Vec::new(),
});

impl Pool {
    fn put(&mut self, storage: Storage) {
        if self.free.len() < FREE_HEAPS
            && self.free.iter().map(|m| m.bytes).sum::<u64>() + storage.bytes <= FREE_BYTES
        {
            self.free.push(storage);
        }
    }

    fn collect(&mut self) {
        let mut i = 0;
        while i < self.pending.len() {
            let complete = self.pending[i].fence.as_ref().is_some_and(|(f, target)| {
                // SAFETY: the fence is retained by this pending allocation.
                let value = unsafe { f.GetCompletedValue() };
                value != u64::MAX && value >= *target
            });
            if complete {
                let pending = self.pending.swap_remove(i);
                for chunk in pending.chunks {
                    self.put(chunk.storage);
                }
            } else {
                i += 1;
            }
        }
    }
}

fn storage(host: &Host, bytes: u64) -> Option<Storage> {
    {
        let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
        pool.collect();
        let at = pool
            .free
            .iter()
            .enumerate()
            .filter(|(_, s)| s.device.as_raw() == host.device.as_raw() && s.bytes >= bytes)
            .min_by_key(|(_, s)| s.bytes)
            .map(|(i, _)| i);
        if let Some(at) = at {
            return Some(pool.free.swap_remove(at));
        }
    }
    let bytes = bytes.max(4096).next_power_of_two();
    let (table, data) = host.upload_buffer(bytes)?;
    // SAFETY: an ordinary bounded shader-visible descriptor heap.
    let heap = unsafe {
        host.device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: SLOTS,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })
    }
    .ok()?;
    Some(Storage {
        device: host.device.clone(),
        table,
        data: data as usize,
        bytes,
        heap,
    })
}

/// Immutable native objects held from preparation through actual completion.
pub(super) struct Batch {
    chunks: Vec<Chunk>,
    pins: Vec<ID3D12Resource>,
    pass: Arc<Pass>,
    differs: ID3D12Resource,
    recorded: bool,
    fence: Option<(ID3D12Fence, u64)>,
}

impl Batch {
    pub(super) fn dispatches(&self) -> usize {
        self.chunks.len()
    }

    /// # Safety
    /// `list` is open; producer and alias barriers have already been recorded;
    /// resources have the same states required by the legacy `card_pass`.
    /// A batch is recorded exactly once.
    pub(super) unsafe fn record(&mut self, list: &ID3D12GraphicsCommandList) {
        assert!(!self.recorded, "a maintenance batch was recorded twice");
        self.recorded = true;
        // SAFETY: the caller's command list and states; every descriptor/table
        // and complete footprint was validated and pinned during preparation.
        unsafe {
            list.SetComputeRootSignature(&self.pass.root);
            list.SetPipelineState(&self.pass.pso);
            list.SetComputeRootUnorderedAccessView(4, self.differs.GetGPUVirtualAddress());
            for chunk in &self.chunks {
                let s = &chunk.storage;
                list.SetDescriptorHeaps(&[Some(s.heap.clone())]);
                let start = s.heap.GetGPUDescriptorHandleForHeapStart();
                let step = s
                    .device
                    .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
                list.SetComputeRootShaderResourceView(1, s.table.GetGPUVirtualAddress());
                list.SetComputeRootDescriptorTable(2, start);
                list.SetComputeRootDescriptorTable(
                    3,
                    D3D12_GPU_DESCRIPTOR_HANDLE {
                        ptr: start.ptr + u64::from(step) * VIEWS as u64,
                    },
                );
                let constants = [0u32, chunk.rows];
                list.SetComputeRoot32BitConstants(0, 2, constants.as_ptr().cast(), 0);
                list.Dispatch(chunk.rows, 1, 1);
            }
        }
    }

    /// Tag before native submission, with a freshly reserved fence value which
    /// will be signalled after every command recorded from this batch.
    ///
    /// # Safety
    /// No command from this batch may execute after that signal. `target` must
    /// be the future native value reserved for those commands, not a logical
    /// guest/flight number. Missing signals intentionally retain the objects.
    pub(super) unsafe fn submitted(&mut self, fence: &ID3D12Fence, target: u64) {
        self.fence = None;
        // SAFETY: a live retained native fence.
        let completed = unsafe { fence.GetCompletedValue() };
        if self.recorded && target > completed && completed != u64::MAX {
            self.fence = Some((fence.clone(), target));
        }
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
        if self.recorded {
            // None means quarantine: work was recorded without a proved native
            // retirement point, so neither storage nor pins can be discarded.
            pool.pending.push(Pending {
                chunks: std::mem::take(&mut self.chunks),
                _pins: std::mem::take(&mut self.pins),
                _pass: self.pass.clone(),
                fence: self.fence.take(),
            });
        } else {
            for chunk in std::mem::take(&mut self.chunks) {
                pool.put(chunk.storage);
            }
        }
        pool.collect();
    }
}

/// All validation, compilation and allocation precede the first mutation of a
/// command list. Any unsupported plan simply uses the existing implementation.
pub(super) fn prepare(host: &Host, jobs: &[Job], differs: &ID3D12Resource) -> Option<Batch> {
    let pass = pass(host)?;
    // SAFETY: resources are live COM references throughout this function.
    let differs_desc = unsafe { differs.GetDesc() };
    if differs_desc.Dimension != D3D12_RESOURCE_DIMENSION_BUFFER
        || !differs_desc
            .Flags
            .contains(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS)
    {
        return None;
    }
    let mut resources: HashMap<usize, &ID3D12Resource> = HashMap::new();
    let mut specs = Vec::with_capacity(jobs.len());
    for job in jobs {
        let source = job.source.as_raw() as usize;
        let hashes = job.hashes.as_raw() as usize;
        let spec = Spec {
            source,
            hashes,
            span: job.span,
            size: job.size,
            out_at: job.out_at,
            mode: job.mode,
        };
        let (_, hi) = footprint(&spec)?;
        // SAFETY: live retained resources.
        let (src, hash) = unsafe { (job.source.GetDesc(), job.hashes.GetDesc()) };
        if src.Dimension != D3D12_RESOURCE_DIMENSION_BUFFER
            || hash.Dimension != D3D12_RESOURCE_DIMENSION_BUFFER
            || src.Width % 4 != 0
            || hash.Width % 4 != 0
            || src.Width / 4 > u64::from(u32::MAX)
            || hash.Width / 4 > u64::from(u32::MAX)
            || src.Width < hi
            || src.Flags.contains(D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE)
            || !hash
                .Flags
                .contains(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS)
            || u64::from(job.span.first + job.span.pieces) * 16 > hash.Width
        {
            return None;
        }
        resources.insert(source, &job.source);
        resources.insert(hashes, &job.hashes);
        specs.push(spec);
    }
    // The normal representation owns separate source, hash and result
    // allocations. Refuse unusual aliases whose cross-job write dependencies
    // the independently scheduled groups cannot reproduce.
    let result = differs.as_raw() as usize;
    if specs
        .iter()
        .any(|s| s.source == result || s.hashes == result)
        || specs
            .iter()
            .any(|s| specs.iter().any(|other| s.source == other.hashes))
    {
        return None;
    }
    let parts = plan(&specs, differs_desc.Width)?;
    let mut batch = Batch {
        chunks: Vec::with_capacity(parts.len()),
        pins: resources
            .values()
            .map(|r| (*r).clone())
            .chain(std::iter::once(differs.clone()))
            .collect(),
        pass,
        differs: differs.clone(),
        recorded: false,
        fence: None,
    };
    for part in parts {
        let storage = storage(host, part.rows.len() as u64 * ROW_BYTES)?;
        // SAFETY: unique CPU ownership before recording, with sufficient room.
        unsafe {
            std::ptr::copy_nonoverlapping(
                part.rows.as_ptr(),
                storage.data as *mut [u32; 8],
                part.rows.len(),
            );
        }
        // SAFETY: the heap has exactly two bounded 32-descriptor ranges. Null
        // descriptors initialize every unused slot, including tier2 requirements.
        unsafe {
            let start = storage.heap.GetCPUDescriptorHandleForHeapStart();
            let step = host
                .device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                as usize;
            for i in 0..VIEWS {
                let source = part.sources.get(i).and_then(|k| resources.get(k).copied());
                let width = source.map_or(4, |r| r.GetDesc().Width);
                let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: DXGI_FORMAT_R32_TYPELESS,
                    ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_SRV {
                            FirstElement: 0,
                            NumElements: (width / 4) as u32,
                            StructureByteStride: 0,
                            Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                        },
                    },
                };
                host.device.CreateShaderResourceView(
                    source,
                    Some(&desc),
                    D3D12_CPU_DESCRIPTOR_HANDLE {
                        ptr: start.ptr + i * step,
                    },
                );
                let hash = part.hashes.get(i).and_then(|k| resources.get(k).copied());
                let width = hash.map_or(4, |r| r.GetDesc().Width);
                let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                    Format: DXGI_FORMAT_R32_TYPELESS,
                    ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                    Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_UAV {
                            FirstElement: 0,
                            NumElements: (width / 4) as u32,
                            StructureByteStride: 0,
                            CounterOffsetInBytes: 0,
                            Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                        },
                    },
                };
                host.device.CreateUnorderedAccessView(
                    hash,
                    None,
                    Some(&desc),
                    D3D12_CPU_DESCRIPTOR_HANDLE {
                        ptr: start.ptr + (VIEWS + i) * step,
                    },
                );
            }
        }
        batch.chunks.push(Chunk {
            storage,
            rows: part.rows.len() as u32,
        });
    }
    Some(batch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(source: usize, hashes: usize, first: u32, pieces: u32, mode: u32, out_at: u32) -> Spec {
        Spec {
            source,
            hashes,
            span: CardSpan {
                from: 0,
                to: 4 << 20,
                first,
                pieces,
                skew: 0,
            },
            size: 4 << 20,
            out_at,
            mode,
        }
    }

    #[test]
    fn keeps_each_piece_and_output_slot() {
        let p = plan(&[spec(1, 2, 3, 2, 2, 11), spec(7, 8, 9, 1, 1, 20)], 1024).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(
            p[0].rows,
            vec![
                [0, 0, 3, 0, 4 << 20, 44, 2, 0],
                [0, 0, 4, 0, 4 << 20, 48, 2, 0],
                [1, 1, 9, 0, 4 << 20, 80, 1, 0]
            ]
        );
    }

    #[test]
    fn writable_piece_repeats_fall_back_even_for_disjoint_guest_ranges() {
        for a in 0..=2 {
            for b in 0..=2 {
                let mut second = spec(3, 2, 3, 1, b, 20);
                second.span.from = 65536;
                let p = plan(&[spec(1, 2, 3, 1, a, 11), second], 1024);
                assert_eq!(p.is_some(), a == 1 && b == 1);
            }
        }
    }

    #[test]
    fn overlapping_result_slots_and_invalid_footprints_fall_back() {
        assert!(plan(&[spec(1, 2, 3, 2, 1, 11), spec(3, 4, 7, 1, 1, 12)], 1024).is_none());
        assert!(plan(&[spec(1, 2, 3, 2, 1, 11)], 48).is_none());
        assert!(plan(&[spec(1, 2, 64, 1, 1, 0)], 1024).is_none());
        assert!(plan(&[spec(1, 2, 0, 0, 1, 0)], 1024).is_none());
        assert!(plan(&[spec(1, 2, 0, 1, 3, 0)], 1024).is_none());
    }

    #[test]
    fn preserves_skew_and_the_complete_widened_last_piece() {
        let mut s = spec(1, 2, 64, 1, 1, 0);
        s.span.skew = 65535;
        assert_eq!(
            footprint(&s),
            Some(((63 * 65536) as u64, (64 * 65536) as u64))
        );
        let p = plan(&[s], 1024).unwrap();
        assert_eq!(p[0].rows[0][3], 65535);
    }

    #[test]
    fn small_title_buffers_hash_their_rounded_logical_size() {
        let mut s = spec(1, 2, 0, 1, 1, 0);
        s.size = 288;
        s.span.to = 281;
        assert_eq!(footprint(&s), Some((0, 288)));
        assert_eq!(plan(&[s], 1024).unwrap()[0].rows[0][4], 288);
    }

    #[test]
    fn descriptor_capacity_splits_without_losing_work() {
        let specs: Vec<_> = (0..33).map(|i| spec(i + 1, i + 100, 0, 1, 0, 0)).collect();
        let parts = plan(&specs, 0).unwrap();
        assert_eq!(
            parts.iter().map(|p| p.rows.len()).collect::<Vec<_>>(),
            [32, 1]
        );
        assert!(
            parts
                .iter()
                .all(|p| p.sources.len() <= VIEWS && p.hashes.len() <= VIEWS)
        );
    }

    #[test]
    fn dispatch_capacity_splits_without_losing_work() {
        let mut full = spec(1, 2, 0, MOST as u32, 0, 0);
        full.size = MOST as u32 * 65536;
        full.span.to = full.size;
        let parts = plan(&[full, spec(3, 4, 0, 1, 0, 0)], 0).unwrap();
        assert_eq!(
            parts.iter().map(|p| p.rows.len()).collect::<Vec<_>>(),
            [MOST, 1]
        );
        assert_eq!(parts[0].rows.last().unwrap()[2], MOST as u32 - 1);
        assert_eq!(parts[1].rows[0][2], 0);
    }

    #[test]
    fn shader_keeps_legacy_hash_reduction_verbatim() {
        let source = shader_source().unwrap();
        let body = source
            .split_once("    uint lo = piece * 65536u;")
            .unwrap()
            .1
            .replace(
                "sources[NonUniformResourceIndex(source)].Load4(",
                "now.Load4(",
            )
            .replace("hashes[NonUniformResourceIndex(hash)].", "hashes.")
            .replace("differs.Store(out_at,", "differs.Store(out_at + id.x * 4u,");
        assert_eq!(
            body,
            CARD_COMPARE
                .split_once("    uint lo = piece * 65536u;")
                .unwrap()
                .1
        );
        assert!(source.contains("if (id.x >= job_count) return;"));
    }
}
