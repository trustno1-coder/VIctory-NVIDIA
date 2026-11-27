//! Run this ignored native test alone in its own process. No game or assets.
use super::*;
use crate::host::{Host, host, transition};
use windows::Win32::Graphics::Dxgi::Common::*;

/// Obtain our known COM owner without keeping an extra reference alive.
fn owner(resource: &ID3D12Resource, replace: Option<FenceSource>) -> Piece {
    let mut raw: *mut c_void = std::ptr::null_mut();
    let mut size = std::mem::size_of_val(&raw) as u32;
    // SAFETY: RECLAIM_KEY is attached only to our Reclaim IUnknown. This
    // isolated fixture changes its fence before any use or final COM release;
    // no other thread can access this resource or its owner at that point.
    unsafe {
        resource
            .GetPrivateData(&RECLAIM_KEY, &mut size, Some((&raw mut raw).cast()))
            .expect("pool COM owner");
        assert!(!raw.is_null());
        let reference = IUnknown::from_raw(raw);
        let attached = raw.cast::<Reclaim>();
        if let Some(source) = replace {
            (*attached).fence = Some(source);
        }
        let piece = (*attached).piece;
        drop(reference);
        piece
    }
}

fn heap_identity(piece: Piece) -> usize {
    lock(&TEXTURES).blocks[piece.block]
        .as_ref()
        .expect("occupied heap")
        .memory
        .as_raw() as usize
}

fn reclaim_after_rest() {
    // Advancing only the allocator's test clock avoids sleeping; it does not
    // bypass the native fence predicate or alter any native fence value.
    lock(&TEXTURES).reclaim(now().saturating_add(REST + 1), SPARE_KEPT, true);
}

fn submit(host: &Host, record: impl FnOnce(&ID3D12GraphicsCommandList)) {
    // This queue has no relationship to Host.submitted/pool_reserved. All
    // resources, allocator and list stay alive through actual completion.
    unsafe {
        let queue: ID3D12CommandQueue = host
            .device
            .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })
            .expect("native fixture queue");
        let allocator: ID3D12CommandAllocator = host
            .device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .expect("allocator");
        let list: ID3D12GraphicsCommandList = host
            .device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .expect("list");
        record(&list);
        list.Close().expect("close fixture list");
        let fence: ID3D12Fence = host
            .device
            .CreateFence(0, D3D12_FENCE_FLAG_NONE)
            .expect("completion fence");
        queue.ExecuteCommandLists(&[Some(list.cast().expect("native command list"))]);
        queue.Signal(&fence, 1).expect("signal fixture completion");
        let start = std::time::Instant::now();
        loop {
            let completed = fence.GetCompletedValue();
            assert_ne!(completed, u64::MAX, "device removed during fixture");
            if completed >= 1 {
                break;
            }
            if start.elapsed() > std::time::Duration::from_secs(30) {
                // Do not unwind and free potentially live native objects.
                std::process::abort();
            }
            std::thread::yield_now();
        }
    }
}

fn alias(resource: &ID3D12Resource) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_ALIASING,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Aliasing: std::mem::ManuallyDrop::new(D3D12_RESOURCE_ALIASING_BARRIER {
                pResourceBefore: std::mem::ManuallyDrop::new(None),
                // SAFETY: borrowed for the recording call; caller pins resource.
                pResourceAfter: unsafe { std::mem::transmute_copy(resource) },
            }),
        },
        ..Default::default()
    }
}

fn write_and_read_texture(host: &Host, texture: &ID3D12Resource, expected: &[u8]) {
    let desc = unsafe { texture.GetDesc() };
    let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
    let mut size = 0u64;
    unsafe {
        host.device.GetCopyableFootprints(
            &desc,
            0,
            1,
            0,
            Some(&raw mut footprint),
            None,
            None,
            Some(&raw mut size),
        );
    }
    assert_eq!(
        (
            footprint.Offset,
            footprint.Footprint.RowPitch,
            footprint.Footprint.Height,
            footprint.Footprint.Depth
        ),
        (0, 512, 128, 1)
    );
    assert_eq!(size, 65536);
    assert_eq!(expected.len(), size as usize);
    let (upload, mapped) = host
        .upload_buffer(size)
        .expect("texture whole-footprint upload");
    unsafe { std::ptr::copy_nonoverlapping(expected.as_ptr(), mapped, expected.len()) };
    let readback = host
        .read_back_buffer(size)
        .expect("texture whole-footprint readback");
    // Locations borrow their resources; the locals are pinned through the
    // actual completion in submit, without introducing hidden COM clones.
    let location = D3D12_TEXTURE_COPY_LOCATION {
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: 0,
        },
    };
    let rows = |resource: &ID3D12Resource| D3D12_TEXTURE_COPY_LOCATION {
        pResource: unsafe { std::mem::transmute_copy(resource) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: footprint,
        },
    };
    submit(host, |list| unsafe {
        list.ResourceBarrier(&[alias(texture)]);
        list.CopyTextureRegion(&location, 0, 0, 0, &rows(&upload), None);
        list.ResourceBarrier(&[transition(
            texture,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        list.CopyTextureRegion(&rows(&readback), 0, 0, 0, &location, None);
    });
    let mut actual: *mut c_void = std::ptr::null_mut();
    unsafe {
        readback
            .Map(
                0,
                Some(&D3D12_RANGE {
                    Begin: 0,
                    End: expected.len(),
                }),
                Some(&raw mut actual),
            )
            .expect("map completed texture readback");
        assert_eq!(
            std::slice::from_raw_parts(actual.cast::<u8>(), expected.len()),
            expected,
            "every texture byte, including the last row and tail"
        );
        readback.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
    }
}

#[test]
#[ignore = "native isolated process: VICTORY_POOL_WINDOWS_MB=128, RIPE_BLOCKS=0; WARP or RTX"]
fn mixed_pool_reuses_exact_extent_only_after_com_and_native_fence_then_initializes_all_bytes() {
    let host = host().expect("native adapter required; never silently skip");
    assert!(on());
    assert!(
        window_limit() >= 128 << 20,
        "VICTORY_POOL_WINDOWS_MB=128 required"
    );
    assert!(
        mixed_heaps(&host.device),
        "native ResourceHeapTier2 required"
    );
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: 128,
        Height: 128,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        ..Default::default()
    };
    let texture_a = unsafe { texture(&host.device, &desc, D3D12_RESOURCE_STATE_COMMON) }
        .expect("pooled texture A");
    let retirement: ID3D12Fence =
        unsafe { host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.expect("retirement fence");
    let reserved = Arc::new(AtomicU64::new(1));
    let first = owner(&texture_a, Some((retirement.clone(), reserved.clone())));
    assert_eq!(
        first.len, 65536,
        "fixture requires a native 64KiB allocation"
    );
    let heap = heap_identity(first);
    let blocks_before = lock(&TEXTURES).blocks();
    // Poison every pixel before release: the later zero gaps must overwrite real old GPU data.
    write_and_read_texture(host, &texture_a, &vec![0xAB; first.len as usize]);
    let held = HELD.load(Ordering::Relaxed);
    let clone = texture_a.clone();
    drop(texture_a);
    reclaim_after_rest();
    assert_eq!(
        HELD.load(Ordering::Relaxed),
        held,
        "bare COM clone retains occupancy"
    );
    let probe_a = window_buffer(host, first.len).expect("existing mixed heap must accept buffer");
    assert_ne!(
        owner(&probe_a, None),
        first,
        "live COM clone cannot be reused"
    );
    assert_eq!(
        lock(&TEXTURES).blocks(),
        blocks_before,
        "buffer factory must not grow heaps"
    );
    drop(clone);
    lock(&TEXTURES).reclaim(now(), SPARE_KEPT, true);
    assert_eq!(
        HELD.load(Ordering::Relaxed),
        held + first.len,
        "retired texture remains charged before REST"
    );
    reclaim_after_rest();
    assert_eq!(
        HELD.load(Ordering::Relaxed),
        held + first.len,
        "pending native fence keeps retired texture charged beyond REST"
    );
    assert!(
        lock(&TEXTURES).resting.iter().any(|r| r.piece == first),
        "incomplete native fence retains retired extent beyond REST"
    );
    let probe_b = window_buffer(host, first.len).expect("second existing hole");
    assert_ne!(
        owner(&probe_b, None),
        first,
        "time alone cannot free a pending native extent"
    );
    unsafe { retirement.Signal(1) }.expect("complete retirement watermark");
    reclaim_after_rest();
    let buffer = window_buffer(host, first.len).expect("reclaimed texture extent as buffer");
    assert_eq!(
        owner(&buffer, Some((retirement.clone(), reserved.clone()))),
        first
    );
    assert_eq!(
        heap_identity(first),
        heap,
        "same native heap, not committed fallback"
    );
    assert_eq!(lock(&TEXTURES).blocks(), blocks_before);

    // Whole-footprint upload includes zeros between nonzero markers and the
    // last byte. The production first-upload path has its own upcheck fixture.
    let len = first.len as usize;
    let mut expected = vec![0u8; len];
    expected[..16].fill(0x3C);
    expected[len / 2..len / 2 + 17].fill(0x71);
    expected[len - 9..].fill(0xE2);
    let (upload, bytes) = host
        .upload_buffer(first.len)
        .expect("complete immutable upload");
    unsafe { std::ptr::copy_nonoverlapping(expected.as_ptr(), bytes, len) };
    let readback = host.read_back_buffer(first.len).expect("readback");
    submit(host, |list| unsafe {
        list.ResourceBarrier(&[alias(&buffer)]);
        list.CopyBufferRegion(&buffer, 0, &upload, 0, first.len);
        list.ResourceBarrier(&[transition(
            &buffer,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        list.CopyBufferRegion(&readback, 0, &buffer, 0, first.len);
    });
    let mut read: *mut c_void = std::ptr::null_mut();
    unsafe {
        readback
            .Map(
                0,
                Some(&D3D12_RANGE { Begin: 0, End: len }),
                Some(&raw mut read),
            )
            .expect("map completed readback");
        assert_eq!(
            std::slice::from_raw_parts(read.cast::<u8>(), len),
            expected.as_slice()
        );
        readback.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
    }
    reserved.store(2, Ordering::Release);
    let borrowed = WINDOW_BYTES.load(Ordering::Relaxed);
    let occupied = HELD.load(Ordering::Relaxed);
    drop(buffer); // all native uses completed, but retirement fence is only 1.
    lock(&TEXTURES).reclaim(now(), SPARE_KEPT, true);
    assert_eq!(
        WINDOW_BYTES.load(Ordering::Relaxed),
        borrowed,
        "final COM release keeps the retired extent's cap before REST"
    );
    assert_eq!(
        HELD.load(Ordering::Relaxed),
        occupied,
        "final COM release keeps the retired extent accounted before REST"
    );
    reclaim_after_rest();
    assert_eq!(
        WINDOW_BYTES.load(Ordering::Relaxed),
        borrowed,
        "pending fence keeps the retired extent's cap beyond REST"
    );
    assert_eq!(
        HELD.load(Ordering::Relaxed),
        occupied,
        "pending fence keeps the retired extent accounted beyond REST"
    );
    {
        let remaining = window_limit() - borrowed;
        let _others = (remaining != 0)
            .then(|| WindowBytes::reserve(&WINDOW_BYTES, remaining, window_limit()).unwrap());
        assert!(
            window_buffer(host, first.len).is_none(),
            "retired bytes cannot be borrowed again through another hole"
        );
    }
    assert!(lock(&TEXTURES).resting.iter().any(|r| r.piece == first));
    unsafe { retirement.Signal(2) }.expect("second retirement completion");
    reclaim_after_rest();
    assert_eq!(
        WINDOW_BYTES.load(Ordering::Relaxed),
        borrowed - first.len,
        "completed and rested extent releases its cap exactly once"
    );
    assert_eq!(
        HELD.load(Ordering::Relaxed),
        occupied - first.len,
        "completed and rested extent releases accounting exactly once"
    );
    let texture_c = unsafe { texture(&host.device, &desc, D3D12_RESOURCE_STATE_COMMON) }
        .expect("reclaimed buffer extent as texture C");
    assert_eq!(
        owner(&texture_c, None),
        first,
        "texture→buffer→texture exact extent"
    );
    assert_eq!(heap_identity(first), heap);
    assert_eq!(lock(&TEXTURES).blocks(), blocks_before);
    let texture_c_bytes: Vec<u8> = (0..first.len as usize)
        .map(|i| (i % 251 + 1) as u8)
        .collect();
    write_and_read_texture(host, &texture_c, &texture_c_bytes);
    assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    assert_eq!(
        host.log_messages(),
        None,
        "native alias/copy/state validation"
    );
    // Both probes were kept occupied, so neither could masquerade as reuse.
    drop((texture_c, probe_a, probe_b));
    eprintln!(
        "mixed pool: exact texture→buffer→texture extent, COM/fence retirement, complete 64KiB zeros/markers/tail verified"
    );
}
