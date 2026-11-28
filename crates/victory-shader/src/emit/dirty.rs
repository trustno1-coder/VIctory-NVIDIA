//! GPU-side 64 KiB dirty-page bitmap (`u<SLOT_DIRTY>`) instrumentation for
//! translated buffer and flat-memory stores and atomics.
//!
//! Each bit `p` of `u<SLOT_DIRTY>` (`DIRTY_WORDS` dwords = 256 KiB) records
//! that a shader wrote at least one byte in title 64 KiB page `p` (`addr >> 16`
//! below 128 GiB). When every buffer/memory write in a shader resolves its
//! title address and the root signature stays within D3D12's 64-dword budget,
//! [`super::Lowered::dirty_tracked`] is set so `victory-gpu` skips the CPU-side
//! `Shadow::before` full-range snapshot and diff.

use super::super::shader::{
    DIRTY_WORDS, MEMORY_PAGES, SLOT_DIRTY, USER_ROOT_ADDRESS, USER_VIEW_ADDRESS, VIEW_STRIDE_BITS,
};
use super::{Attr, Bin, Cast, HwStage, Lower, Lowered, ResourceType, RootType, Source, Tag, V, Why, pred};

/// Whether shader lowering emits `u<SLOT_DIRTY>` dirty-page updates
/// (`VICTORY_DIRTY_BITMAP`, enabled by default; `=0` disables).
pub fn dirty_bitmap() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_DIRTY_BITMAP.get() {
        return on;
    }
    if let Some(on) = TEST_DIRTY_OVERRIDE.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DIRTY_BITMAP").is_none_or(|v| v != "0"))
}

thread_local! {
    static TEST_DIRTY_OVERRIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[doc(hidden)]
pub fn set_dirty_bitmap_for_test(on: Option<bool>) {
    TEST_DIRTY_OVERRIDE.set(on);
}

#[cfg(test)]
thread_local! {
    pub(crate) static TEST_DIRTY_BITMAP: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_dirty_bitmap<R>(on: bool, f: impl FnOnce() -> R) -> R {
    let was = TEST_DIRTY_BITMAP.replace(Some(on));
    let out = f();
    TEST_DIRTY_BITMAP.set(was);
    out
}

/// Per-lowering state for `SLOT_DIRTY` instrumentation.
#[derive(Default)]
pub(super) struct DirtyState {
    pub enabled: bool,
    pub has_writes: bool,
    pub aborted: bool,
    /// From [`Lower::memory`] when `store` is set: `page_there & !closed`.
    pub writable_page: Option<V>,
    /// From [`Lower::root_memory`] when `store` is set: `(lo, hi, writable_page)`.
    pub root_mem: Option<(V, V, Option<V>)>,
}

/// Shape and offsets of a `buffer_store_*` or `buffer_atomic_*` write.
pub(super) enum DirtyShape {
    /// Raw buffer UAV (shape 11): `byte_in_view` is the byte offset inside `h`,
    /// `unbiased_off` is the byte offset before [`Lower::built_offset`] added
    /// `address` and `USER_VIEW_BIAS`, and `bytes` is the store width (`1..=16`).
    Raw {
        h: V,
        byte_in_view: V,
        unbiased_off: V,
        bytes: u32,
    },
    /// Structured buffer UAV (shape 12): element `index`, byte `off` within the
    /// element, `stride` bytes per element, and store width `bytes`.
    Structured {
        h: V,
        index: V,
        off: V,
        stride: u32,
        bytes: u32,
    },
    /// Typed buffer UAV (shape 10, `buffer_store_format_*`): element `index`
    /// and packed [`SLOT_TABLE_STRIDES`] word read before the guard.
    Typed {
        h: V,
        index: V,
        stride_word: Option<V>,
    },
}

/// Worst-case D3D12 root signature dwords `l` needs, and the stage budget.
/// Compute pipelines have one stage (64 dwords); graphics/mesh pipelines pair
/// two stages, so a stage that adds `SLOT_DIRTY` stays within 32 dwords.
pub(super) fn within_root_budget(stage: HwStage, l: &Lowered) -> bool {
    let user_dwords = l
        .user_data
        .iter()
        .map(|u| u.index + 1)
        .max()
        .unwrap_or(0)
        .next_multiple_of(4);
    let root_views = (l.root_cbvs.len() + l.root_srvs.len() + l.root_uavs.len()) as u32;
    let cost = user_dwords + 2 * root_views;
    let limit = match stage {
        HwStage::Cs => 64,
        HwStage::Vs | HwStage::Ps | HwStage::Ms => 32,
        _ => 32,
    };
    cost <= limit
}

impl<'a> Lower<'a> {
    /// Whether `src` is a static descriptor-table UAV whose base address the
    /// host can supply via [`USER_VIEW_ADDRESS`].
    pub(super) fn is_static_table_uav(&self, src: &Source) -> Option<(u8, u32)> {
        let Source::Loaded { load, first } = src else {
            return None;
        };
        if self.x.res.waterfall.loads.contains(load) {
            return None;
        }
        let a = self.scalar.get(load)?;
        let rp = *self.x.native.layout.get(a.param)?;
        let byte_off = a.offset.checked_add(4 * u32::from(*first))?;
        if rp.root != RootType::Table || a.dynamic.is_some() || byte_off % 32 != 0 {
            return None;
        }
        let dword = byte_off / 4;
        let used = self
            .x
            .native
            .used
            .iter()
            .filter(|u| {
                u.root == RootType::Table
                    && u.user_data == rp.user_data
                    && u.table_offset_dwords <= dword
            })
            .max_by_key(|u| u.table_offset_dwords)?;
        if used.dwords != 4 || used.kind != ResourceType::Uav {
            return None;
        }
        Some((rp.slot, dword))
    }

    /// Records dirty 64 KiB page(s) for a `global_store_*` or `global_atomic_*`
    /// at 40-bit title address `(lo, hi)` of `bytes` (`4..=16`).
    pub(super) fn dirty_memory(&mut self, lo: V, hi: V, bytes: u32) {
        self.dirty.has_writes = true;
        if !self.dirty.enabled || self.dirty.aborted {
            return;
        }
        let valid = self.dirty.writable_page;
        let end = (bytes > 1).then(|| {
            let add = self.s.m.i32(bytes - 1);
            self.add_u32_to_64(lo, hi, add)
        });
        self.emit_dirty_span(valid, (lo, hi), end);
    }

    /// Records dirty 64 KiB page(s) for a `buffer_store_*` or `buffer_atomic_*`.
    pub(super) fn dirty_buffer(&mut self, src: &Source, shape: DirtyShape) -> Result<(), Why> {
        self.dirty.has_writes = true;
        if !self.dirty.enabled || self.dirty.aborted {
            return Ok(());
        }

        // 1. Spilled root UAV routed through `SLOT_MEMORY` (`root_memory`).
        if let Some((lo, hi, writable)) = self.dirty.root_mem {
            let bytes = match shape {
                DirtyShape::Raw { bytes, .. } | DirtyShape::Structured { bytes, .. } => bytes,
                DirtyShape::Typed { .. } => {
                    self.dirty.aborted = true;
                    return Ok(());
                }
            };
            let end = (bytes > 1).then(|| {
                let add = self.s.m.i32(bytes - 1);
                self.add_u32_to_64(lo, hi, add)
            });
            self.emit_dirty_span(writable, (lo, hi), end);
            return Ok(());
        }

        // 2. Non-spilled root UAV (`Source::Built` on `RootType::Descriptor`).
        // Root descriptors have no D3D12 view bounds; every active lane writes
        // `[root_address + signed_const_off + dynamic_off + unbiased_off, + bytes - 1]`.
        if let Source::Built { address, .. } = src {
            let Some(rp) = self.x.native.layout.get(address.param).copied() else {
                self.dirty.aborted = true;
                return Ok(());
            };
            let is_uav = self
                .x
                .native
                .used
                .iter()
                .any(|u| u.user_data == rp.user_data && u.kind == ResourceType::Uav);
            if rp.root != RootType::Descriptor || !is_uav {
                self.dirty.aborted = true;
                return Ok(());
            }
            let (rel_off, bytes) = match shape {
                DirtyShape::Raw { unbiased_off, bytes, .. } => (unbiased_off, bytes),
                DirtyShape::Structured { index, off, stride, bytes, .. } => {
                    let c = self.cur;
                    let k = self.s.m.i32(stride);
                    let scaled = self.s.m.bin(c, Bin::Mul, index, k);
                    (self.s.m.bin(c, Bin::Add, scaled, off), bytes)
                }
                DirtyShape::Typed { .. } => {
                    self.dirty.aborted = true;
                    return Ok(());
                }
            };
            let root_lo = self.host_word(RootType::Other(USER_ROOT_ADDRESS), rp.slot, 0);
            let root_hi = self.host_word(RootType::Other(USER_ROOT_ADDRESS + 1), rp.slot, 0);
            let c = self.cur;
            let zero = self.s.m.i32(0);
            let any_addr = self.s.m.bin(c, Bin::Or, root_lo, root_hi);
            let non_null = self.s.m.cmp(c, pred::NE, any_addr, zero);
            let (mut base_lo, mut base_hi) =
                self.address_plus(root_lo, root_hi, address.offset as i32);
            if let Some(dyn_val) = address.dynamic {
                let d = self.i32_of(dyn_val)?;
                (base_lo, base_hi) = self.add_signed_i32_to_64(base_lo, base_hi, d);
            }
            let (start_lo, start_hi) = self.add_u32_to_64(base_lo, base_hi, rel_off);
            let end = (bytes > 1).then(|| {
                let add = self.s.m.i32(bytes - 1);
                self.add_u32_to_64(start_lo, start_hi, add)
            });
            self.emit_dirty_span(Some(non_null), (start_lo, start_hi), end);
            return Ok(());
        }

        // 3. Static descriptor-table UAV (`Source::Loaded`).
        let Some((slot, dword)) = self.is_static_table_uav(src) else {
            self.dirty.aborted = true;
            return Ok(());
        };
        let base_lo = self.host_word(RootType::Other(USER_VIEW_ADDRESS), slot, dword);
        let base_hi = self.host_word(RootType::Other(USER_VIEW_ADDRESS + 1), slot, dword);
        let non_null = {
            let c = self.cur;
            let zero = self.s.m.i32(0);
            let any_addr = self.s.m.bin(c, Bin::Or, base_lo, base_hi);
            self.s.m.cmp(c, pred::NE, any_addr, zero)
        };

        match shape {
            DirtyShape::Raw { h, byte_in_view, bytes, .. } => {
                let dim0 = self.uav_dim0(h);
                let c = self.cur;
                let in_bounds = self.s.m.cmp(c, pred::IULT, byte_in_view, dim0);
                let in_bounds = self.s.m.bin(c, Bin::And, in_bounds, non_null);
                let (start_lo, start_hi) = self.add_u32_to_64(base_lo, base_hi, byte_in_view);
                let end = if bytes > 1 {
                    let c = self.cur;
                    let one = self.s.m.i32(1);
                    let max_byte = self.s.m.bin(c, Bin::Sub, dim0, one);
                    let add = self.s.m.i32(bytes - 1);
                    let cand = self.s.m.bin(c, Bin::Add, byte_in_view, add);
                    // Clamp on unsigned wrap or past `dim0 - 1`.
                    let wrapped = self.s.m.cmp(c, pred::IULT, cand, byte_in_view);
                    let cand = self.s.m.select(c, wrapped, max_byte, cand);
                    let i32t = self.s.t.i32;
                    let last_off =
                        self.s.op(c, "binary.i32", Attr::ReadNone, i32t, 40, &[cand, max_byte]);
                    self.s.m.tag(last_off, Tag::Clamp);
                    Some(self.add_u32_to_64(base_lo, base_hi, last_off))
                } else {
                    None
                };
                self.emit_dirty_span(Some(in_bounds), (start_lo, start_hi), end);
            }
            DirtyShape::Structured { h, index, off, stride, bytes } => {
                let dim0 = self.uav_dim0(h);
                let c = self.cur;
                let k_stride = self.s.m.i32(stride);
                let idx_ok = self.s.m.cmp(c, pred::IULT, index, dim0);
                let off_ok = self.s.m.cmp(c, pred::IULT, off, k_stride);
                let in_bounds = self.s.m.bin(c, Bin::And, idx_ok, off_ok);
                let in_bounds = self.s.m.bin(c, Bin::And, in_bounds, non_null);
                let elem_byte = self.s.m.bin(c, Bin::Mul, index, k_stride);
                let byte_in_view = self.s.m.bin(c, Bin::Add, elem_byte, off);
                let (start_lo, start_hi) = self.add_u32_to_64(base_lo, base_hi, byte_in_view);
                let end = (bytes > 1).then(|| {
                    let c = self.cur;
                    let max_in_elem = self.s.m.i32(stride - 1);
                    let add = self.s.m.i32(bytes - 1);
                    let cand_in_elem = self.s.m.bin(c, Bin::Add, off, add);
                    let i32t = self.s.t.i32;
                    let last_in_elem = self.s.op(
                        c,
                        "binary.i32",
                        Attr::ReadNone,
                        i32t,
                        40,
                        &[cand_in_elem, max_in_elem],
                    );
                    self.s.m.tag(last_in_elem, Tag::Clamp);
                    let last_off = self.s.m.bin(c, Bin::Add, elem_byte, last_in_elem);
                    self.add_u32_to_64(base_lo, base_hi, last_off)
                });
                self.emit_dirty_span(Some(in_bounds), (start_lo, start_hi), end);
            }
            DirtyShape::Typed { h, index, stride_word } => {
                let Some(word) = stride_word else {
                    self.dirty.aborted = true;
                    return Ok(());
                };
                let dim0 = self.uav_dim0(h);
                let c = self.cur;
                let mask = self.s.m.i32(VIEW_STRIDE_BITS);
                let elem_bytes = self.s.m.bin(c, Bin::And, word, mask);
                let zero = self.s.m.i32(0);
                let idx_ok = self.s.m.cmp(c, pred::IULT, index, dim0);
                let stride_ok = self.s.m.cmp(c, pred::NE, elem_bytes, zero);
                let in_bounds = self.s.m.bin(c, Bin::And, idx_ok, stride_ok);
                let in_bounds = self.s.m.bin(c, Bin::And, in_bounds, non_null);
                let byte_in_view = self.s.m.bin(c, Bin::Mul, index, elem_bytes);
                let one = self.s.m.i32(1);
                let span_minus_1 = self.s.m.bin(c, Bin::Sub, elem_bytes, one);
                let last_off = self.s.m.bin(c, Bin::Add, byte_in_view, span_minus_1);
                let (start_lo, start_hi) = self.add_u32_to_64(base_lo, base_hi, byte_in_view);
                let end = Some(self.add_u32_to_64(base_lo, base_hi, last_off));
                self.emit_dirty_span(Some(in_bounds), (start_lo, start_hi), end);
            }
        }
        Ok(())
    }

    /// Queries `getDimensions(h, undef).0` on a descriptor-table buffer UAV `h`.
    fn uav_dim0(&mut self, h: V) -> V {
        let c = self.cur;
        let i32t = self.s.t.i32;
        let dims = self.s.m.named_struct("dx.types.Dimensions", vec![i32t; 4]);
        let undef = self.s.m.undef(i32t);
        let d = self
            .s
            .op(c, "getDimensions", Attr::ReadOnly, dims, 72, &[h, undef]);
        self.s.m.extract(c, d, 0)
    }

    /// Unsigned 32-bit `off` added to 64-bit `(lo, hi)`.
    fn add_u32_to_64(&mut self, lo: V, hi: V, off: V) -> (V, V) {
        let c = self.cur;
        let i32t = self.s.t.i32;
        let sum = self.s.m.bin(c, Bin::Add, lo, off);
        let wrapped = self.s.m.cmp(c, pred::IULT, sum, lo);
        let carry = self.s.m.cast(c, Cast::ZExt, wrapped, i32t);
        let hi = self.s.m.bin(c, Bin::Add, hi, carry);
        (sum, hi)
    }

    /// Signed 32-bit `off` added to 64-bit `(lo, hi)`.
    fn add_signed_i32_to_64(&mut self, lo: V, hi: V, off: V) -> (V, V) {
        let c = self.cur;
        let i32t = self.s.t.i32;
        let sum = self.s.m.bin(c, Bin::Add, lo, off);
        let wrapped = self.s.m.cmp(c, pred::IULT, sum, lo);
        let carry = self.s.m.cast(c, Cast::ZExt, wrapped, i32t);
        let k31 = self.s.m.i32(31);
        let sign = self.s.m.bin(c, Bin::AShr, off, k31);
        let hi = self.s.m.bin(c, Bin::Add, hi, carry);
        let hi = self.s.m.bin(c, Bin::Add, hi, sign);
        (sum, hi)
    }

    /// Clamped 64 KiB title page index `((lo >> 16) | ((hi & 0xFF) << 16)) & (MEMORY_PAGES - 1)`.
    fn dirty_page_of(&mut self, lo: V, hi: V) -> V {
        let c = self.cur;
        let k16 = self.s.m.i32(16);
        let k8 = self.s.m.i32(0xFF);
        let page_mask = self.s.m.i32(MEMORY_PAGES - 1);
        let low = self.s.m.bin(c, Bin::LShr, lo, k16);
        let high = self.s.m.bin(c, Bin::And, hi, k8);
        let high = self.s.m.bin(c, Bin::Shl, high, k16);
        let page = self.s.m.bin(c, Bin::Or, low, high);
        self.s.m.bin(c, Bin::And, page, page_mask)
    }

    /// Byte offset `((page >> 5) & (DIRTY_WORDS - 1)) << 2` and bit mask `1 << (page & 31)`.
    fn dirty_word_and_mask(&mut self, page: V) -> (V, V) {
        let c = self.cur;
        let k5 = self.s.m.i32(5);
        let word_mask = self.s.m.i32(DIRTY_WORDS - 1);
        let k2 = self.s.m.i32(2);
        let k31 = self.s.m.i32(31);
        let one = self.s.m.i32(1);
        let word = self.s.m.bin(c, Bin::LShr, page, k5);
        let word = self.s.m.bin(c, Bin::And, word, word_mask);
        let word_byte = self.s.m.bin(c, Bin::Shl, word, k2);
        let bit = self.s.m.bin(c, Bin::And, page, k31);
        let mask = self.s.m.bin(c, Bin::Shl, one, bit);
        (word_byte, mask)
    }

    /// Sets dirty bits in `u<SLOT_DIRTY>` for `start` and (if present) `end`.
    fn emit_dirty_span(&mut self, valid: Option<V>, start: (V, V), end: Option<(V, V)>) {
        let dirty = self.s.root_uav_handle(SLOT_DIRTY);
        let outer = self.begin_guarded(valid);
        let p0 = self.dirty_page_of(start.0, start.1);
        let (w0, m0) = self.dirty_word_and_mask(p0);
        let i32t = self.s.t.i32;
        let iu = self.s.m.undef(i32t);
        let op_or = self.s.m.i32(2);
        match end {
            None => {
                let args = [dirty, op_or, w0, iu, iu, m0];
                self.s
                    .op(self.cur, "atomicBinOp.i32", Attr::NoUnwind, i32t, 78, &args);
            }
            Some((end_lo, end_hi)) => {
                let p1 = self.dirty_page_of(end_lo, end_hi);
                let (w1, m1) = self.dirty_word_and_mask(p1);
                let c = self.cur;
                let same_word = self.s.m.cmp(c, pred::EQ, w0, w1);
                let both = self.s.m.bin(c, Bin::Or, m0, m1);
                let first_mask = self.s.m.select(c, same_word, both, m0);
                let args0 = [dirty, op_or, w0, iu, iu, first_mask];
                self.s
                    .op(c, "atomicBinOp.i32", Attr::NoUnwind, i32t, 78, &args0);
                let true_v = self.s.m.bool(true);
                let diff_word = self.s.m.bin(c, Bin::Xor, same_word, true_v);
                let second = self.begin_guarded(Some(diff_word));
                let args1 = [dirty, op_or, w1, iu, iu, m1];
                self.s
                    .op(self.cur, "atomicBinOp.i32", Attr::NoUnwind, i32t, 78, &args1);
                self.end_guarded(second, &[]);
            }
        }
        self.end_guarded(outer, &[]);
    }
}
