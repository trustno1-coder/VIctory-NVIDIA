//! Native CPU mappings only: prove that two real aliases resolve one backing,
//! while unmapping/reusing a VA or a physical incarnation rejects old leases.

use super::*;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    MapViewOfFileEx, PAGE_READWRITE, SEC_COMMIT, UnmapViewOfFile,
};

const PAGE: usize = 64 << 10;

struct NativeSection {
    handle: HANDLE,
    token: BackingToken,
    views: Vec<(usize, usize)>,
}

impl NativeSection {
    fn new() -> Self {
        // SAFETY: pagefile-backed two-page section; no borrowed pointers.
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE | SEC_COMMIT,
                0,
                (2 * PAGE) as u32,
                std::ptr::null(),
            )
        };
        assert!(
            !handle.is_null(),
            "native section: {}",
            std::io::Error::last_os_error()
        );
        let token =
            crate::inplace::title_memory_section_created(handle as usize, (2 * PAGE) as u64, true);
        Self {
            handle,
            token,
            views: Vec::new(),
        }
    }

    fn map(&mut self, offset: usize, len: usize, wanted: Option<usize>) -> usize {
        let _changing = crate::watch::title_memory_remapping();
        // SAFETY: valid section offsets; wanted, when present, is a view this
        // fixture just unmapped. Failure cannot expose a different mapping.
        let view = unsafe {
            match wanted {
                Some(at) => MapViewOfFileEx(
                    self.handle,
                    FILE_MAP_ALL_ACCESS,
                    0,
                    offset as u32,
                    len,
                    at as *const _,
                ),
                None => MapViewOfFile(self.handle, FILE_MAP_ALL_ACCESS, 0, offset as u32, len),
            }
        };
        let at = view.Value as usize;
        assert_ne!(at, 0, "native map: {}", std::io::Error::last_os_error());
        if let Some(wanted) = wanted {
            assert_eq!(at, wanted);
        }
        self.views.push((at, len));
        crate::watch::title_memory_remapped(at, len);
        crate::inplace::title_memory_view_backing(
            at,
            len,
            self.handle as usize,
            offset as u64,
            self.token,
        );
        at
    }

    fn unmap(&mut self, at: usize) {
        let index = self.views.iter().position(|v| v.0 == at).unwrap();
        let (_, len) = self.views[index];
        let _changing = crate::watch::title_memory_remapping();
        crate::watch::title_memory_unmapping(at, len);
        // SAFETY: an owned native view, with lifecycle readers excluded.
        assert_ne!(
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: at as *mut _,
                })
            },
            0
        );
        self.views.swap_remove(index);
    }
}

impl Drop for NativeSection {
    fn drop(&mut self) {
        let _changing = crate::watch::title_memory_remapping();
        for (at, len) in self.views.drain(..) {
            crate::watch::title_memory_unmapping(at, len);
            // SAFETY: each successful view is retained until this cleanup.
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: at as *mut _,
                })
            };
        }
        crate::inplace::title_memory_section_released(self.token);
        // SAFETY: this fixture owns its one section handle.
        unsafe { CloseHandle(self.handle) };
    }
}

#[test]
#[ignore = "native CPU mappings; isolated process with VICTORY_SHARED_STORAGE=1, one test thread"]
fn real_section_aliases_share_versions_and_reject_reused_identity() {
    assert!(enabled(), "fixture requires VICTORY_SHARED_STORAGE=1");
    let mut section = NativeSection::new();
    let a = section.map(0, 2 * PAGE, None);
    let b = section.map(0, 2 * PAGE, None);
    let a_before = resolve(a, PAGE).expect("first registered alias");
    let b_before = resolve(b, PAGE).expect("second registered alias");
    assert_eq!(
        a_before.keys().collect::<Vec<_>>(),
        b_before.keys().collect::<Vec<_>>()
    );
    assert!(
        !a_before.tracking_complete(),
        "observations must never authorize retention"
    );
    let key = physical_key_at(a).unwrap();
    let slice = physical_slice_for_key(key).unwrap();
    assert_eq!(slice.aliases.len(), 2);
    let version = b_before.current_version().unwrap();
    let untouched = resolve(b + PAGE, PAGE).unwrap();
    let untouched_version = untouched.current_version().unwrap();
    {
        let _writing = crate::watch::kernel_writing(a + 32, 4);
        // SAFETY: a writable native alias retained throughout the write lease.
        unsafe { std::ptr::write_volatile((a + 32) as *mut u32, 0x1357_2468) };
    }
    // SAFETY: the second native alias maps exactly those physical bytes.
    assert_eq!(
        unsafe { std::ptr::read_volatile((b + 32) as *const u32) },
        0x1357_2468
    );
    assert!(b_before.current_version().unwrap() > version);
    assert!(!b_before.changes_since(version).unwrap().1.is_empty());
    assert_eq!(untouched.current_version(), Some(untouched_version));

    let before_fault = b_before.current_version().unwrap();
    assert!(crate::watch::protect_storage_fixture(a));
    assert!(crate::watch::protect_storage_fixture(b));
    // SAFETY: these owned mapped pages are guarded by the real VEH. This
    // native store exercises the bounded fault journal through an alias.
    unsafe { std::ptr::write_volatile((a + 64) as *mut u32, 0x89ab_cdef) };
    assert_eq!(
        unsafe { std::ptr::read_volatile((b + 64) as *const u32) },
        0x89ab_cdef
    );
    assert!(b_before.current_version().unwrap() > before_fault);

    title_memory_backing_reincarnated(section.token, 0, PAGE);
    assert!(a_before.current_version().is_none());
    assert!(b_before.current_version().is_none());
    assert!(physical_slice_for_key(key).is_none());
    let fresh_key = physical_key_at(b).unwrap();
    assert_ne!(fresh_key, key);
    let b_current = resolve(b, PAGE).unwrap();
    let b_version = b_current.current_version().unwrap();
    section.unmap(a);
    assert!(resolve(a, PAGE).is_none());
    assert_eq!(physical_slice_for_key(fresh_key).unwrap().aliases.len(), 1);
    assert!(b_current.current_version().unwrap() > b_version);
    assert!(b_current.same_identity(&resolve(b, PAGE).unwrap()));
    let replacement = section.map(PAGE, PAGE, Some(a));
    let replaced = resolve(replacement, PAGE).unwrap();
    assert_ne!(
        replaced.keys().collect::<Vec<_>>(),
        b_current.keys().collect::<Vec<_>>()
    );
    assert!(
        a_before.current_version().is_none(),
        "VA reuse revived an old lease"
    );
    assert_eq!(physical_key_at(replacement), physical_key_at(b + PAGE));
    // SAFETY: replacement is the untouched, zero-initialized second page.
    assert_eq!(
        unsafe { std::ptr::read_volatile((replacement + 32) as *const u32) },
        0
    );
    println!(
        "SECTION_IDENTITY_NATIVE_PASS aliases=2 shared_versions=true native_store_fault=true incarnation_reuse=true va_reuse=true completeness=false"
    );
}
