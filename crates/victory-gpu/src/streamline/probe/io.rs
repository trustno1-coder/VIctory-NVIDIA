//! Staging uploads and completed readbacks for the synthetic SR check.
use super::*;
fn buffer(
    d: &ID3D12Device,
    size: u64,
    heap: D3D12_HEAP_TYPE,
    state: D3D12_RESOURCE_STATES,
) -> Result<ID3D12Resource, String> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut made = None;
    // SAFETY: valid buffer description and out pointer.
    unsafe {
        d.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES {
                Type: heap,
                ..Default::default()
            },
            D3D12_HEAP_FLAG_NONE,
            &desc,
            state,
            None,
            &mut made,
        )
    }
    .map_err(err)?;
    made.ok_or_else(|| "no staging buffer".into())
}

fn layout(d: &ID3D12Device, t: &ID3D12Resource) -> (D3D12_PLACED_SUBRESOURCE_FOOTPRINT, u64) {
    let mut f = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
    let mut size = 0;
    // SAFETY: live single-subresource texture and writable outputs.
    unsafe {
        d.GetCopyableFootprints(
            &t.GetDesc(),
            0,
            1,
            0,
            Some(&mut f),
            None,
            None,
            Some(&mut size),
        );
    }
    (f, size)
}

pub(super) fn transition(l: &ID3D12GraphicsCommandList, t: &ID3D12Resource, before: u32, after: u32) {
    // SAFETY: borrowed resource remains alive; union is a transition; no COM reference is consumed.
    unsafe {
        l.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: std::mem::transmute_copy(t),
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: D3D12_RESOURCE_STATES(before as i32),
                    StateAfter: D3D12_RESOURCE_STATES(after as i32),
                }),
            },
        }]);
    }
}

fn locations(
    t: &ID3D12Resource,
    b: &ID3D12Resource,
    f: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
) -> (D3D12_TEXTURE_COPY_LOCATION, D3D12_TEXTURE_COPY_LOCATION) {
    // SAFETY: borrowed resources outlive the immediate copy; ManuallyDrop fields do not release them.
    unsafe {
        (
            D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(t),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            },
            D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(b),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: f },
            },
        )
    }
}

pub(super) fn upload(
    d: &ID3D12Device,
    l: &ID3D12GraphicsCommandList,
    t: &ID3D12Resource,
    bpp: usize,
    state: u32,
    fill: impl Fn(u32, u32, &mut [u8]),
) -> Result<ID3D12Resource, String> {
    let (f, size) = layout(d, t);
    let b = buffer(
        d,
        size,
        D3D12_HEAP_TYPE_UPLOAD,
        D3D12_RESOURCE_STATE_GENERIC_READ,
    )?;
    let mut data = std::ptr::null_mut();
    // SAFETY: mapped upload buffer spans the footprint; each pixel is within its unpadded row.
    unsafe {
        b.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }), Some(&mut data))
            .map_err(err)?;
        let bytes = std::slice::from_raw_parts_mut(data.cast::<u8>(), size as usize);
        bytes.fill(0);
        for y in 0..f.Footprint.Height {
            for x in 0..f.Footprint.Width {
                let at = f.Offset as usize
                    + y as usize * f.Footprint.RowPitch as usize
                    + x as usize * bpp;
                fill(x, y, &mut bytes[at..at + bpp]);
            }
        }
        b.Unmap(0, None);
        let (to, from) = locations(t, &b, f);
        l.CopyTextureRegion(&to, 0, 0, 0, &from, None);
    }
    transition(l, t, COPY_DEST, state);
    Ok(b)
}

pub(super) fn readback(
    d: &ID3D12Device,
    host: &crate::host::Host,
    t: &ID3D12Resource,
) -> Result<Vec<u8>, String> {
    let (f, size) = layout(d, t);
    let b = buffer(
        d,
        size,
        D3D12_HEAP_TYPE_READBACK,
        D3D12_RESOURCE_STATE_COPY_DEST,
    )?;
    let l = host.new_list().map_err(err)?;
    transition(
        &l.list,
        t,
        UNORDERED,
        D3D12_RESOURCE_STATE_COPY_SOURCE.0 as u32,
    );
    let (from, to) = locations(t, &b, f);
    // SAFETY: initialized source texture, suitable readback buffer, open list.
    unsafe {
        l.list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
    }
    transition(
        &l.list,
        t,
        D3D12_RESOURCE_STATE_COPY_SOURCE.0 as u32,
        UNORDERED,
    );
    l.close();
    host.execute_sync_internal(&l);
    let mut data = std::ptr::null_mut();
    let mut pixels = Vec::new();
    // SAFETY: completed copy, exact footprint bounds; discard row padding.
    unsafe {
        b.Map(
            0,
            Some(&D3D12_RANGE {
                Begin: 0,
                End: size as usize,
            }),
            Some(&mut data),
        )
        .map_err(err)?;
        let bytes = std::slice::from_raw_parts(data.cast::<u8>(), size as usize);
        for y in 0..f.Footprint.Height as usize {
            let at = f.Offset as usize + y * f.Footprint.RowPitch as usize;
            pixels.extend_from_slice(&bytes[at..at + f.Footprint.Width as usize * 8]);
        }
        b.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
    }
    Ok(pixels)
}
