use super::*;

/// Run alone with diagnostics, POOL_WINDOWS_MB=128, RELAY=2,
/// UPCOPY_CHECK=1 and SEND_OWN=0. Both software and native adapters work.
#[test]
#[ignore = "requires an isolated process with mixed-pool and relay settings"]
fn pooled_window_first_upload_initializes_the_whole_footprint_from_frozen_bytes() {
    let host = host().expect("a GPU adapter is required; this regression must not silently skip");
    assert!(on(), "VICTORY_UPCOPY_CHECK=1 is required");
    assert!(
        !super::super::send::own(),
        "test the forced immutable path with VICTORY_SEND_OWN=0"
    );
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: 256,
        Height: 256,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        ..Default::default()
    };
    // SAFETY: valid texture description. Its block provides an existing hole.
    let _texture =
        unsafe { crate::pool::texture(&host.device, &desc, D3D12_RESOURCE_STATE_COMMON) }
            .expect("a mixed texture heap is required");
    const LEN: usize = 4 << 20;
    // SAFETY: private memory retained until this test process exits. Align the
    // tested window inside it so every possible refresh stays in the allocation.
    let allocation = unsafe {
        use windows_sys::Win32::System::Memory::*;
        VirtualAlloc(
            std::ptr::null(),
            2 * LEN,
            MEM_RESERVE | MEM_COMMIT | 0x20_0000,
            PAGE_READWRITE,
        )
    } as usize;
    assert_ne!(allocation, 0);
    let start = allocation.next_multiple_of(LEN);
    let marker = start + 65536 + 8;
    // SAFETY: inside the private allocation.
    unsafe { std::ptr::write_bytes(marker as *mut u8, 0x31, 16) };
    let shadow = crate::shadow::find(marker as u64, 16).expect("a raw-address window");
    assert!(
        shadow.card_pooled(),
        "the fixture must exercise pooled placement"
    );
    assert!(shadow.card_needs_alias());
    assert_eq!(shadow.span(), (start as u64, LEN as u64));
    assert_eq!(
        shadow.card_dirty_now(),
        vec![(0, LEN)],
        "already dirty before publication"
    );
    shadow.update(marker, 16);
    // SAFETY: the live staging mapping is LEN bytes; nobody writes it here.
    let expected = unsafe { std::slice::from_raw_parts(shadow.card_data(), LEN) }.to_vec();
    assert_eq!(&expected[65536 + 8..65536 + 24], &[0x31; 16]);
    assert_eq!(
        expected[LEN - 1],
        0,
        "unrequested tail starts as staging zeros"
    );

    // Poison the exclusive new allocation to model contents left in reused
    // backing. Activate it for this setup copy, then leave it in COMMON.
    let (poison, bytes) = host.upload_buffer(LEN as u64).expect("setup upload");
    // SAFETY: our complete mapping.
    unsafe { std::ptr::write_bytes(bytes, 0xAB, LEN) };
    run(host, |list| unsafe {
        list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_ALIASING,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Aliasing: std::mem::ManuallyDrop::new(D3D12_RESOURCE_ALIASING_BARRIER {
                    pResourceBefore: std::mem::ManuallyDrop::new(None),
                    pResourceAfter: std::mem::transmute_copy(&shadow.buffer),
                }),
            },
            ..Default::default()
        }]);
        list.CopyBufferRegion(&shadow.buffer, 0, &poison, 0, LEN as u64);
        list.ResourceBarrier(&[transition(
            &shadow.buffer,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_COMMON,
        )]);
    })
    .expect("setup copy completed");

    struct ReleaseGate(ID3D12Fence);
    impl Drop for ReleaseGate {
        fn drop(&mut self) {
            // SAFETY: our CPU-signalled fence, also released on assertion failure.
            let _ = unsafe { self.0.Signal(1) };
        }
    }
    // SAFETY: native fence creation, then queue work held until the CPU signal.
    let gate = ReleaseGate(unsafe { host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.unwrap());
    unsafe { host.queue().Wait(&gate.0, 1) }.expect("queue gate");
    let plan = host
        .card_before(&[], &crate::picked::Ranges::default(), true)
        .expect("first upload prefix");
    assert!(
        !shadow.card_needs_alias(),
        "activation follows successful prefix enqueue"
    );
    assert!(
        !NOTED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&shadow.id()),
        "no invalid first-upcheck baseline"
    );
    // The first upload is enqueued but cannot execute yet. A mutable COPY
    // source would now give the first consumer this newer data incorrectly.
    unsafe { std::ptr::write_bytes(shadow.card_data().cast_mut(), 0xCC, LEN) };
    drop(gate);
    let (readback, data) = host
        .shared_buffer(LEN as u64)
        .expect("full-footprint readback");
    run(host, |list| unsafe {
        list.CopyBufferRegion(&readback, 0, &shadow.buffer, 0, LEN as u64);
    })
    .expect("first consumer completed");
    let actual = unsafe { std::slice::from_raw_parts(data, LEN) };
    assert_eq!(
        actual,
        expected.as_slice(),
        "all bytes, including zeros and tail, come from the frozen first upload"
    );
    drop(plan); // the immutable send is pinned through the completed consumer.

    // Restore the test-only staging mutation, then refresh a genuine later
    // guest write. The second prefix must preserve the rest of the footprint.
    unsafe {
        std::ptr::copy_nonoverlapping(expected.as_ptr(), shadow.card_data().cast_mut(), LEN);
        std::ptr::write_bytes(marker as *mut u8, 0x79, 16);
    }
    shadow.update(marker, 16);
    let second = host
        .card_before(&[], &crate::picked::Ranges::default(), true)
        .expect("second upload prefix");
    run(host, |list| unsafe {
        list.CopyBufferRegion(&readback, 0, &shadow.buffer, 0, LEN as u64);
    })
    .expect("second consumer completed");
    let mut expected_later = expected;
    expected_later[65536 + 8..65536 + 24].fill(0x79);
    assert_eq!(
        unsafe { std::slice::from_raw_parts(data, LEN) },
        expected_later.as_slice()
    );
    drop(second);
    assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    assert_eq!(
        host.log_messages(),
        None,
        "native first-upload/copy/state validation"
    );
    eprintln!(
        "mixed window production prefix: complete footprint, frozen COPY source and later update verified"
    );
}
