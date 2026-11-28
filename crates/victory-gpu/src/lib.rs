//! D3D12.X on host D3D12.
//!
//! D3D12.X records commands from code inlined into the title, which writes
//! packets into the command list while its put pointer is below a limit and
//! calls the list's method otherwise. Our lists keep the limits at zero, so
//! every command arrives here as a call, to be run on host D3D12. The
//! title's GPU addresses are CPU addresses (console memory is unified).
//!
//! [`device`] is the device object titles create, [`dxgi`] its adapter side,
//! [`resource`] heaps and resources, [`fence`] fences, [`queue`] command
//! queues, [`list`] command allocators and lists, [`descriptor`]
//! descriptor heaps, [`root`] root signatures, [`query`] query heaps,
//! [`signature`] command signatures, [`pipeline`] pipeline states,
//! [`frame`] frame pacing, [`object`] what all console graphics objects
//! share, [`console`] which console model the title sees. Planned:
//! `present` (swap chain).

#[cfg(windows)]
mod ahead;
mod ambient;
#[cfg(all(windows, feature = "diagnostics"))]
mod capture;
#[cfg(windows)]
mod compose;
pub mod console;
#[cfg(all(windows, feature = "diagnostics"))]
mod copystat;
#[cfg(all(windows, feature = "diagnostics"))]
mod faultbook;
#[cfg(windows)]
mod cores;
pub mod descriptor;
pub mod device;
#[cfg(windows)]
mod draw;
pub mod dxgi;
#[cfg(windows)]
mod external;
pub mod fence;
pub mod frame;
#[cfg(all(windows, feature = "diagnostics"))]
mod book;
#[cfg(windows)]
mod heap_marks;
#[cfg(windows)]
mod host;
/// The F1 settings menu is shown (`host/menu.rs`): the title gets no keyboard meanwhile
/// (read by the input module).
pub static SETTINGS_MENU_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(windows)]
mod indirect;
#[cfg(windows)]
mod inplace;
#[cfg(windows)]
mod port;
#[cfg(windows)]
mod land;
pub mod list;
pub mod object;
pub mod pipeline;
#[cfg(windows)]
mod picked;
#[cfg(windows)]
mod pool;
#[cfg(all(windows, feature = "diagnostics"))]
mod popwatch;
#[cfg(windows)]
mod primitive;
pub mod query;
pub mod queue;
#[cfg(all(windows, feature = "diagnostics"))]
pub mod replay;
#[cfg(windows)]
mod residency;
pub mod resource;
pub mod save_share;
pub mod root;
#[cfg(windows)]
mod safecopy;
#[cfg(windows)]
mod sampler;
#[cfg(windows)]
pub mod shader_cache;
#[cfg(windows)]
mod memory;
#[cfg(windows)]
mod shadow;
#[cfg(windows)]
mod tiles;
pub mod signature;
#[cfg(windows)]
pub mod streamline;
mod switch;
#[cfg(all(windows, feature = "diagnostics"))]
mod ab;
#[cfg(windows)]
mod tdr;
#[cfg(windows)]
mod texture;
pub mod timeline;
#[cfg(windows)]
pub mod titlewrite;
#[cfg(windows)]
mod upload;
#[cfg(windows)]
mod watch;
#[cfg(windows)]
mod texback;

#[cfg(windows)]
pub use host::on_failure;
/// Reserves the address space for memory shaders read by address; call it
/// first thing, before the process's other allocations can fill [4 GiB, 2^37).
#[cfg(windows)]
pub use upload::reserve_space as reserve_upload_space;

/// Whether `switch` is on now, if it is one of those `VICTORY_AB` turns on and
/// off while the title runs (`ab`).
#[cfg(all(windows, feature = "diagnostics"))]
pub fn switched(switch: &str) -> Option<bool> {
    ab::pick(switch)
}

/// What the frame is at (`copystat::phase`), for the sampling profiler.
#[cfg(all(windows, feature = "diagnostics"))]
pub fn frame_phase() -> u32 {
    copystat::phase()
}

/// Frames the title has begun (its frame waits that ended), for logs.
pub fn frames_begun() -> u64 {
    frame::current()
}

/// When the first frame of 500 draws or more was presented: the world, past
/// the menus and loading screens (the same test as `VICTORY_CAPTURE_WORLD`).
/// Scripted keys count from it with `VICTORY_KEYS_FROM=world`.
pub fn world_since() -> Option<std::time::Instant> {
    WORLD_SINCE.get().copied()
}

static WORLD_SINCE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// At a present: the draws run on the host so far.
#[cfg(windows)]
pub(crate) fn world_frame(drawn: u64) {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let before = LAST.swap(drawn, Relaxed);
    if WORLD_SINCE.get().is_none() && world_reached(before, drawn) && WORLD_SINCE.set(std::time::Instant::now()).is_ok() {
        tracing::info!(frames = frames_begun(), "the world: the first frame of 500 draws or more");
    }
}

/// Whether a frame that ran the draws from `before` to `drawn` is one of the world's.
fn world_reached(before: u64, drawn: u64) -> bool {
    drawn.saturating_sub(before) >= 500
}

#[cfg(windows)]
pub use inplace::{title_memory_freeing, title_memory_section, title_memory_section_gone, title_memory_view};
#[cfg(windows)]
pub use shadow::title_memory_write_combined;
#[cfg(windows)]
pub use watch::{
    SoftViews, kernel_writing, soft_view_guards, title_memory_allocated, title_memory_freed, title_memory_made_real,
    title_memory_reading, title_memory_remapped, title_memory_remapping, title_memory_splitting, title_memory_unmapping,
    title_memory_written, use_lazy_views, use_soft_views, writing as kernel_will_write,
};

#[cfg(test)]
mod world_tests {
    #[test]
    fn a_frame_of_500_draws_is_the_world() {
        assert!(!super::world_reached(0, 499));
        assert!(super::world_reached(0, 500));
        assert!(super::world_reached(10_000, 10_600));
        // A counter that went back (never expected) is not a world frame.
        assert!(!super::world_reached(10_000, 20));
    }
}
