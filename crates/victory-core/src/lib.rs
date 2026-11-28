//! Shared plumbing for every VIctory crate and tool.
//!
//! * [`config`]: where the user's files live (never inside the repository).
//! * [`policy`]: the runtime half of rule #1, refusing to write
//!   game-derived data into the source tree.
//! * [`logging`]: one tracing setup for all binaries.
//! * [`fs`]: files on the disk before they are renamed, and replaced whole.

pub mod config;
pub mod fs;
pub mod logging;
pub mod policy;

pub use config::Config;
pub use policy::{PolicyError, ensure_outside_repo};
