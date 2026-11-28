//! Port of DexNom/VIctory PR 62 (MrToucann, 2152e55b), size-query part only: AMD's shader
//! compiler (amdxc64, RX 9060 XT / 9070 XT, driver 32.0.32029.2010) reads a null pointer
//! building a pipeline where a buffer's size is asked through the same uniform heap handle
//! the buffer's loads use (fxceless/victory-fork#1213, IxMxAMAR). Here the size gets a heap
//! handle of its own, marked non-uniform, so the compiler cannot merge the two.
//!
//! The shader cache keys on the translator's sources (`victory-gpu`'s `build.rs`), so
//! translations cached before this change are not served after it.

use super::super::bitcode::V;
use super::{Lower, Site, Source, Why};

impl Lower<'_> {
    /// The handle a buffer's size (`descriptor_data`) is asked through: for a buffer whose
    /// descriptor every lane shares, one of its own marked non-uniform, not counted in
    /// `nonuniform_handles` (its index is the same in every lane); otherwise the view's
    /// handle as before. The index is the loads' own, raw twin included (`raw_twin`), so
    /// the size is the view's the loads read.
    pub(super) fn size_view_handle(
        &mut self,
        site: Site,
        first: u8,
        src: &Source,
        props: (u32, u32),
        buffer: bool,
    ) -> Result<V, Why> {
        if !buffer || self.x.res.waterfall.loads.contains(&site) {
            return self.handle(site, src, false, props);
        }
        let index = self.heap_index(site, first, false)?;
        let (index, bounded) = self.raw_twin(src, index, false, props);
        let h = self.s.heap_handle_at(self.cur, index, false, true, props, bounded);
        self.s.nonuniform_handles -= 1;
        Ok(h)
    }
}
