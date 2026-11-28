use super::*;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D12::D3D12_RANGE;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
};

const K64: usize = 64 << 10;

struct Title(u64);
impl Title {
    fn new() -> Self {
        // SAFETY: private committed synthetic memory of two pages.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                2 * K64,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        assert!(!p.is_null());
        Self(p as u64)
    }

    fn fill(&self, value: u8) {
        // SAFETY: fixture owns the full committed allocation.
        unsafe { (self.0 as *mut u8).write_bytes(value, 2 * K64) };
    }
}

impl Drop for Title {
    fn drop(&mut self) {
        unregister(1, self.0);
        unregister(2, self.0 + K64 as u64);
        // SAFETY: the fixture's private allocation, after workers joined.
        unsafe { VirtualFree(self.0 as *mut _, 0, MEM_RELEASE) };
    }
}

fn allocate(size: u64) -> Option<(ID3D12Resource, *mut u8)> {
    host()?.shared_buffer(size)
}

fn delayed(ready: &Sender<()>, go: &Receiver<()>, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
    let made = allocate(size);
    ready.send(()).unwrap();
    go.recv().unwrap();
    made
}

fn probe_while_pending<T>(access: impl Fn() -> Option<T>) -> Option<T> {
    // Other workspace tests can briefly take the same table lock.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(value) = access() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn an_unrelated_lookup_runs_while_a_fresh_shadow_is_pending() {
    if host().is_none() {
        return;
    }
    assert!(!crate::port::on() && crate::inplace::level() < 2);
    let title = Title::new();
    let addr = title.0;
    let other = addr + K64 as u64;
    register(1, addr, K64 as u64);
    register(2, other, K64 as u64);
    let existing = lookup_with(other, 1, false, allocate).unwrap();
    let made = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let (ready, waiting) = channel();
        let (go, resume) = channel();
        let first = scope.spawn(move || {
            lookup_with(addr, 1, false, |size| delayed(&ready, &resume, size)).unwrap()
        });
        waiting.recv().unwrap();
        // Do not block in a negative control that keeps the global writer.
        let unrelated = probe_while_pending(|| {
            BUFFERS
                .try_read()
                .ok()
                .and_then(|map| map.get(&(other, 2))?.shadow.clone())
        });
        let maker = &made;
        let follower = scope.spawn(move || {
            lookup_with(addr, 1, false, |size| {
                maker.fetch_add(1, Ordering::Relaxed);
                allocate(size)
            })
            .unwrap()
        });
        go.send(()).unwrap();
        let winner = first.join().unwrap();
        assert!(Arc::ptr_eq(&winner, &follower.join().unwrap()));
        assert!(Arc::ptr_eq(
            &existing,
            &unrelated.expect("other entry must remain readable")
        ));
        assert_eq!(made.load(Ordering::Relaxed), 0);
    });
}

#[test]
fn a_pending_allocation_reselects_the_live_registration_and_refreshes_its_bytes() {
    let Some(host) = host() else {
        return;
    };
    assert!(!crate::port::on() && crate::inplace::level() < 2);
    for overlap in [false, true] {
        let title = Title::new();
        title.fill(0x11);
        let addr = title.0;
        let ask = addr + K64 as u64 + 13;
        register(1, addr, (2 * K64) as u64);
        let made = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let (ready, waiting) = channel();
            let (go, resume) = channel();
            let count = &made;
            let worker = scope.spawn(move || {
                lookup_with(ask, 1, false, |size| {
                    if count.fetch_add(1, Ordering::Relaxed) == 0 {
                        delayed(&ready, &resume, size)
                    } else {
                        allocate(size)
                    }
                })
                .unwrap()
            });
            waiting.recv().unwrap();
            // Fail without leaving the worker waiting if allocation accidentally
            // regains the global writer.
            if probe_while_pending(|| BUFFERS.try_write().ok().map(drop)).is_none() {
                go.send(()).unwrap();
                worker.join().unwrap();
                panic!("registration must be able to change during allocation");
            }
            if overlap {
                register(2, addr + K64 as u64, K64 as u64);
            } else {
                unregister(1, addr);
                register(1, addr, (2 * K64) as u64);
            }
            title.fill(0x65);
            go.send(()).unwrap();
            let winner = worker.join().unwrap();
            assert_eq!(
                made.load(Ordering::Relaxed),
                2,
                "obsolete backing must not be published"
            );
            assert_eq!(winner.start, if overlap { addr + K64 as u64 } else { addr });
            // Exercise the submit-time refresh and read the GPU's view, not just
            // the CPU mapping of the shadow.
            let readback = host.read_back_buffer(K64 as u64).unwrap();
            let list = host.new_list().unwrap();
            let from = addr + K64 as u64;
            winner.refresh(&list, from, K64 as u64);
            // SAFETY: the requested page is inside the refreshed shadow. The
            // destination is a live readback buffer in COPY_DEST.
            unsafe {
                list.list.CopyBufferRegion(
                    &readback,
                    0,
                    &winner.buffer,
                    from - winner.start,
                    K64 as u64,
                )
            };
            list.close();
            host.execute_sync_internal(&list);
            let mut bytes = std::ptr::null_mut();
            let read = D3D12_RANGE { Begin: 0, End: K64 };
            let written = D3D12_RANGE { Begin: 0, End: 0 };
            // SAFETY: the submitting call waited for the copy. We read only
            // the mapped allocation and do not write it.
            let good = unsafe {
                readback.Map(0, Some(&read), Some(&mut bytes)).unwrap();
                assert!(!bytes.is_null());
                let good = std::slice::from_raw_parts(bytes.cast::<u8>(), K64)
                    .iter()
                    .all(|&b| b == 0x65);
                readback.Unmap(0, Some(&written));
                good
            };
            assert!(
                good,
                "GPU readback did not see the live registration's bytes"
            );
            assert!(
                host.log_messages().is_none(),
                "D3D12 reported validation errors"
            );
            if overlap {
                assert!(buffers_read().get(&(addr, 1)).unwrap().shadow.is_none());
            }
        });
    }
}

#[test]
fn an_allocation_failure_leaves_the_registration_open_to_retry() {
    if host().is_none() {
        return;
    }
    let title = Title::new();
    let addr = title.0;
    register(1, addr, K64 as u64);
    assert!(lookup_with(addr, 1, false, |_| None).is_none());
    assert!(lookup_with(addr, 1, false, allocate).is_some());
}
