//! Platform-level data formats, shared by every Xbox Series title.
//!
//! Each module is pure (bytes in, bytes out, no I/O): [`texture`] (the
//! GPU's tiled texture layouts and copies to and from linear rows),
//! [`bcpack`] (BC textures as DirectStorage unpacks them), [`xma`] (the
//! audio codec), [`bc7`] (a BC7 encoder), [`swf`] (Scaleform movies) and
//! [`zlib_exact`] (zlib streams of an exact length).

pub mod bc7;
pub mod bcpack;
pub mod swf;
pub mod texture;
pub mod xma;
pub mod zlib_exact;
