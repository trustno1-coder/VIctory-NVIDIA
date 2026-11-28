//! Xbox Series shaders to host shaders.
//!
//! An Xbox Series shader is a standard DXBC container ([`dxbc`]). Its `DXIL`
//! part ([`dxil`]) may carry Xbox additions around the LLVM bitcode: an
//! `XDAT` table before it indexing blobs after it, among them `xbop`
//! (compiler options) and the zlib-compressed GFX10 code. The bitcode may use
//! console-only intrinsics, so the GFX10 code is translated in general.

pub mod bvh;
pub mod dxbc;
pub mod dxil;
pub mod emit;
pub mod gfx10;
pub mod io;
pub mod ir;
pub mod native;
pub mod resource;
pub mod spill;
pub mod structure;

pub use dxbc::{Container, FourCC, Part};
pub use dxil::{DxilProgram, ShaderKind, XboxExtras};
pub use native::{
    HwStage, NativeProgram, ResourceType, RootParam, RootType, UsedResource, native_programs,
};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShaderError {
    #[error("not a DXBC container")]
    NotDxbc,
    #[error("malformed shader: {0}")]
    Malformed(&'static str),
    #[error("GFX10 code at {offset:#x}: {reason}")]
    Isa { offset: usize, reason: &'static str },
    /// Valid code the translator cannot handle yet: an opcode name or a construct.
    #[error("GFX10 code at {offset:#x}: {what} is not supported")]
    Unsupported { offset: usize, what: &'static str },
}

/// Fast non-cryptographic hashing for the translator's maps (keys are small
/// integers, registers, offsets): SipHash's DoS resistance buys nothing here.
pub(crate) mod fxmap {
    #[derive(Default, Clone, Copy)]
    pub struct Fx(u64);
    const K: u64 = 0xf135_7aea_2e62_a9c5;
    impl std::hash::Hasher for Fx {
        #[inline]
        fn finish(&self) -> u64 { self.0.rotate_left(26) }
        #[inline]
        fn write(&mut self, bytes: &[u8]) {
            let mut c = bytes.chunks_exact(8);
            for w in &mut c { self.write_u64(u64::from_le_bytes(w.try_into().unwrap())); }
            let r = c.remainder();
            if !r.is_empty() { let mut w = [0u8; 8]; w[..r.len()].copy_from_slice(r); self.write_u64(u64::from_le_bytes(w)); }
        }
        #[inline] fn write_u8(&mut self, i: u8) { self.write_u64(i as u64) }
        #[inline] fn write_u16(&mut self, i: u16) { self.write_u64(i as u64) }
        #[inline] fn write_u32(&mut self, i: u32) { self.write_u64(i as u64) }
        #[inline] fn write_u64(&mut self, i: u64) { self.0 = self.0.wrapping_add(i).wrapping_mul(K); }
        #[inline] fn write_usize(&mut self, i: usize) { self.write_u64(i as u64) }
    }
    pub type B = std::hash::BuildHasherDefault<Fx>;
    pub type HashMap<K, V> = std::collections::HashMap<K, V, B>;
    pub type HashSet<K> = std::collections::HashSet<K, B>;
}
