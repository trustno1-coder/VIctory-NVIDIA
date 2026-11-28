//! Static LDS alias and hazard analysis for splitting a console `WaveSize(64)` region
//! across two 32-lane host waves when branches keep barriers out of the region.
//!
//! Each `!all` connected region is accepted only when every LDS memory access inside it can
//! be proven hazard-free across lanes and host halves:
//! - read-only regions (zero writes, zero RMW/atomics);
//! - **Refinement A (Disjoint Byte Ranges)**: all accesses resolve to constant byte offsets on a
//!   single group-uniform base with non-wrapping bounds, group-uniform stored data on every write,
//!   zero RMW self-overlap, zero RAW/WAR byte overlap, and zero distinct-write WAW overlap;
//! - **Refinement B (Lane-Private Addresses)**: every writing/RMW cluster resolves to exclusive
//!   per-thread slots across all active group dimensions (`base + linear(tid) + off`) with
//!   proper alignment and non-overlapping footprints across clusters.
//!   Pure write-only regions also require Refinement A or B by default; the legacy write-only
//!   shortcut stays behind the opt-in 32-lane `VICTORY_LDS_WRITE_ONLY_OK=1` switch.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::gfx10::{Fields, NULL, Operand};
use crate::io::{Input, Io};
use crate::ir::{Block, EXEC, Inst, PARKED, Program, Reg, Value, idoms, reverse_postorder};
use crate::native::ComputeIo;
use crate::resource::{Address, CODE, MEMORY, Resources, inline};
use crate::structure::{Atom, LaneExit, Structured};

/// Upper bound (1 MiB) on symbolic byte offsets and per-group lane footprints so 32-bit
/// address addition cannot wrap modulo 2^32.
const MAX_OFFSET: u64 = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Read,
    Write,
    Rmw,
}

#[derive(Debug, Clone, Copy)]
enum DsClass {
    /// Register-only cross-lane shuffle (`ds_swizzle`, `ds_permute`, `ds_bpermute`), no LDS access.
    Skip,
    /// One LDS access at `(addr + off, width)` in bytes.
    One { kind: Kind, off: u64, width: u64 },
    /// Two LDS accesses from `ds_read2*` / `ds_write2*` / `ds_wrxchg2*`.
    Two {
        kind: Kind,
        off0: u64,
        off1: u64,
        width: u64,
    },
    /// Unhandled or non-addressable DS opcode; forces conservative refusal of the region.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AddrExpr {
    offset: u64,
    uniforms: Vec<(Value, u32)>,
    uniform_tz: u32,
    tid: [u64; 3],
    console_lane: u64,
    known_zero: u32,
    max_val: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SymVal {
    Const(u32),
    Linear(AddrExpr),
    /// `v_mbcnt_lo_u32_b32(-1, 0)` holds `min(lane, 32)`; requires pairing with `hi(-1, lo)`.
    MbcntLoZero,
    /// `v_mbcnt_hi_u32_b32(-1, 0)` holds `lane.saturating_sub(32)`; pairs with `lo(-1, hi)`.
    MbcntHiZero,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputKind {
    Tid(usize),
    GroupUniform,
    NonUniform,
}

#[derive(Debug, Clone)]
struct Access {
    kind: Kind,
    width: u64,
    expr: Option<ResolvedAddr>,
    /// For `Kind::Write`, whether every stored data dword is provably group-uniform
    /// so concurrent writes from both host halves to the same uniform address are idempotent.
    uniform_data: bool,
}

#[derive(Debug, Clone)]
struct ResolvedAddr {
    uniforms: Vec<(Value, u32)>,
    uniform_tz: u32,
    tid: [u64; 3],
    console_lane: u64,
    off: u64,
}

/// Reason a `!all` LDS region cannot be proven safe for native Wave32 execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refusal {
    /// Divergent `s_barrier` inside a `!all` region.
    DivergentBarrier,
    /// Unhandled or non-addressable DS opcode inside a `!all` region.
    UnknownDs,
    /// Shared/unproven LDS atomic with no plain `ds_read*` in the `!all` region.
    AtomicWithoutPlainRead,
    /// Mixed plain LDS read (`ds_read*`) and write/atomic in one `!all` region.
    ReadWriteOverlap,
    /// Two plain LDS writes provably overlapping on a shared uniform base in one `!all` region.
    WriteWriteOverlap,
    /// Shared or unproven plain LDS write inside a cyclic `!all` region.
    CyclicSharedWrite,
    /// Write-only `!all` region whose writes are neither provably disjoint with uniform data nor lane-private.
    UnprovenWrite,
}

impl Refusal {
    pub(super) fn reason(self) -> &'static str {
        match self {
            Self::DivergentBarrier => "a barrier in a lane-branch region",
            Self::UnknownDs => "an unhandled LDS instruction in a lane-branch region",
            Self::AtomicWithoutPlainRead => {
                "an LDS atomic with no plain LDS read in one lane-branch region"
            }
            Self::ReadWriteOverlap => "an LDS read and write in one lane-branch region",
            Self::WriteWriteOverlap => "overlapping LDS writes in one lane-branch region",
            Self::CyclicSharedWrite => "a shared LDS write in a lane-branch loop",
            Self::UnprovenWrite => "unproven LDS writes in one lane-branch region",
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_LDS_WRITE_ONLY_OK: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(super) fn with_test_write_only_ok<T>(enabled: bool, f: impl FnOnce() -> T) -> T {
    struct Reset(Option<bool>);
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_LDS_WRITE_ONLY_OK.set(self.0);
        }
    }
    let _guard = Reset(TEST_LDS_WRITE_ONLY_OK.replace(Some(enabled)));
    f()
}

/// Opt-in 32-lane host compatibility switch (`VICTORY_LDS_WRITE_ONLY_OK=1`) that restores the
/// legacy write-only shortcut for `!all` regions with no plain `ds_read*` (including non-plain-read
/// LDS atomics and shared-base write-only regions). Always disabled on non-32-lane hosts.
fn lds_write_only_ok() -> bool {
    if super::lower::host_max_wave() != 32 {
        return false;
    }
    #[cfg(test)]
    if let Some(enabled) = TEST_LDS_WRITE_ONLY_OK.get() {
        return enabled;
    }
    std::env::var_os("VICTORY_LDS_WRITE_ONLY_OK").is_some_and(|v| v == "1")
}

/// Computes per-block `(meet_before, meet_before_exit)` barrier placements for LDS regions in a
/// `WaveSize(64)` shader lowered to 32-lane waves, or `None` if any `!all` region may have an
/// inter-half LDS hazard.
#[cfg(test)]
pub(super) fn meetings(
    p: &Program,
    st: &Structured,
    io: &Io,
    res: &Resources,
    cs: ComputeIo,
    all: &[bool],
) -> Option<Vec<(bool, bool)>> {
    analyze(p, st, io, res, cs, all, &HashSet::new()).ok()
}

/// Computes per-block `(meet_before, meet_before_exit)` barrier placements for LDS regions in a
/// `WaveSize(64)` shader lowered to 32-lane waves, or returns the specific [`Refusal`] reason
/// explaining why a `!all` region kept Wave64.
pub(super) fn analyze(
    p: &Program,
    st: &Structured,
    io: &Io,
    res: &Resources,
    cs: ComputeIo,
    all: &[bool],
    unordered: &HashSet<(usize, usize)>,
) -> Result<Vec<(bool, bool)>, Refusal> {
    let n = st.exits.len();
    let succ = |b: usize| -> Vec<usize> {
        match st.exits[b] {
            LaneExit::Jump(t) => vec![t],
            LaneExit::Branch {
                taken, not_taken, ..
            } => vec![taken, not_taken],
            LaneExit::End | LaneExit::Indirect => Vec::new(),
        }
    };

    let mut region: Vec<usize> = (0..n).collect();
    fn find(region: &mut [usize], mut b: usize) -> usize {
        while region[b] != b {
            region[b] = region[region[b]];
            b = region[b];
        }
        b
    }
    for b in 0..n {
        for t in succ(b) {
            if !all[b] && !all[t] {
                let (x, y) = (find(&mut region, b), find(&mut region, t));
                region[x] = y;
            }
        }
    }

    let mut region_has_cycle = vec![false; n];
    // Detect internal cycles strictly within `!all` blocks (`!all[b] && !all[t]`).
    let mut color = vec![0u8; n];
    for start in 0..n {
        if all[start] || color[start] != 0 {
            continue;
        }
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
        color[start] = 1;
        while let Some((u, idx)) = stack.pop() {
            let nexts = succ(u);
            if idx < nexts.len() {
                stack.push((u, idx + 1));
                let t = nexts[idx];
                if !all[t] {
                    if color[t] == 1 {
                        let r = find(&mut region, u);
                        region_has_cycle[r] = true;
                    } else if color[t] == 0 {
                        color[t] = 1;
                        stack.push((t, 0));
                    }
                }
            } else {
                color[u] = 2;
            }
        }
    }

    let ctx = AddrCtx::new(p, st, io, res, cs, all);
    let mut region_accesses: Vec<Vec<Access>> = vec![Vec::new(); n];
    let mut reads = vec![false; n];
    let mut writes = vec![false; n];
    let mut has_plain_read = vec![false; n];
    let mut has_rmw = vec![false; n];
    let mut has_unknown = vec![false; n];
    let mut has_barrier = vec![false; n];

    for (b, block) in p.blocks.iter().enumerate().take(n) {
        if all[b] {
            continue;
        }
        let r = find(&mut region, b);
        for (k, inst) in block.insts.iter().enumerate() {
            if inst.name() == "s_barrier" {
                has_barrier[r] = true;
                continue;
            }
            if !matches!(inst.fields, Fields::Ds { .. }) {
                continue;
            }
            if unordered.contains(&(b, k)) {
                continue; // (no meeting: [`super::lower::LdsOrder`])
            }
            match classify_ds(inst) {
                DsClass::Skip => continue,
                DsClass::Unknown => {
                    has_unknown[r] = true;
                    writes[r] = true;
                    reads[r] = true;
                }
                DsClass::One { kind, off, width } => {
                    record_kind(
                        &mut reads[r],
                        &mut writes[r],
                        &mut has_plain_read[r],
                        &mut has_rmw[r],
                        kind,
                    );
                    let expr = ctx.resolve_ds_addr(inst, b, off, width);
                    let uniform_data = kind == Kind::Write && ctx.ds_write_data_uniform(inst, b, 0);
                    region_accesses[r].push(Access {
                        kind,
                        width,
                        expr,
                        uniform_data,
                    });
                }
                DsClass::Two {
                    kind,
                    off0,
                    off1,
                    width,
                } => {
                    record_kind(
                        &mut reads[r],
                        &mut writes[r],
                        &mut has_plain_read[r],
                        &mut has_rmw[r],
                        kind,
                    );
                    let e0 = ctx.resolve_ds_addr(inst, b, off0, width);
                    let e1 = ctx.resolve_ds_addr(inst, b, off1, width);
                    let u0 = kind == Kind::Write && ctx.ds_write_data_uniform(inst, b, 0);
                    let u1 = kind == Kind::Write && ctx.ds_write_data_uniform(inst, b, 1);
                    region_accesses[r].push(Access {
                        kind,
                        width,
                        expr: e0,
                        uniform_data: u0,
                    });
                    region_accesses[r].push(Access {
                        kind,
                        width,
                        expr: e1,
                        uniform_data: u1,
                    });
                }
            }
        }
    }

    let write_only_ok = lds_write_only_ok();
    for r in 0..n {
        if region[r] != r {
            continue;
        }
        if has_barrier[r] {
            return Err(Refusal::DivergentBarrier);
        }
        if has_unknown[r] {
            return Err(Refusal::UnknownDs);
        }
        if reads[r] && writes[r] {
            // Cyclic `!all` regions lack an in-loop barrier, so Refinement A cannot prevent
            // cross-iteration RAW/WAR/WAW between host halves; only lane-private slots are safe.
            let disjoint_ok = !region_has_cycle[r] && region_disjoint_ranges(&region_accesses[r]);
            let safe = disjoint_ok || region_lane_private(&region_accesses[r], cs.threads);
            if !safe {
                if write_only_ok && !has_plain_read[r] {
                    continue;
                }
                return Err(if has_plain_read[r] {
                    Refusal::ReadWriteOverlap
                } else {
                    Refusal::AtomicWithoutPlainRead
                });
            }
        } else if writes[r] && !has_rmw[r] {
            // Pure write-only `!all` regions require positive proof (Refinement A with
            // group-uniform stored data, or Refinement B lane-private slots) by default;
            // the legacy write-only shortcut stays strictly behind `VICTORY_LDS_WRITE_ONLY_OK=1`.
            let disjoint_ok = !region_has_cycle[r] && region_disjoint_ranges(&region_accesses[r]);
            let safe = disjoint_ok || region_lane_private(&region_accesses[r], cs.threads);
            if !safe {
                if write_only_ok {
                    continue;
                }
                if has_provable_shared_waw(&region_accesses[r]) {
                    return Err(Refusal::WriteWriteOverlap);
                }
                if region_has_cycle[r] {
                    return Err(Refusal::CyclicSharedWrite);
                }
                return Err(Refusal::UnprovenWrite);
            }
        }
    }

    let mut meet = vec![(false, false); n];
    for b in 0..n {
        for t in succ(b) {
            if all[b] && !all[t] {
                let r = find(&mut region, t);
                meet[b].1 |= reads[r] || writes[r];
            } else if !all[b] && all[t] {
                let r = find(&mut region, b);
                meet[t].0 |= reads[r] || writes[r];
            }
        }
    }
    Ok(meet)
}

fn record_kind(
    reads: &mut bool,
    writes: &mut bool,
    has_plain_read: &mut bool,
    has_rmw: &mut bool,
    kind: Kind,
) {
    match kind {
        Kind::Read => {
            *reads = true;
            *has_plain_read = true;
        }
        Kind::Write => *writes = true,
        Kind::Rmw => {
            *reads = true;
            *writes = true;
            *has_rmw = true;
        }
    }
}

fn classify_ds(inst: &Inst) -> DsClass {
    let Fields::Ds {
        addr,
        offset0,
        offset1,
        gds,
        ..
    } = &inst.fields
    else {
        return DsClass::Skip;
    };
    let name = inst.name();
    if name.starts_with("ds_swizzle") || name.contains("permute") {
        return DsClass::Skip;
    }
    if *gds
        || addr.is_none()
        || matches!(name, "ds_nop" | "ds_append" | "ds_consume" | "ds_ordered_count")
        || name.starts_with("ds_gws")
        || name.contains("addtid")
        || name.contains("src2")
    {
        return DsClass::Unknown;
    }

    if let Some((kind, width, st64)) = match name {
        "ds_read2_b32" => Some((Kind::Read, 4, false)),
        "ds_read2st64_b32" => Some((Kind::Read, 4, true)),
        "ds_read2_b64" => Some((Kind::Read, 8, false)),
        "ds_read2st64_b64" => Some((Kind::Read, 8, true)),
        "ds_write2_b32" => Some((Kind::Write, 4, false)),
        "ds_write2st64_b32" => Some((Kind::Write, 4, true)),
        "ds_write2_b64" => Some((Kind::Write, 8, false)),
        "ds_write2st64_b64" => Some((Kind::Write, 8, true)),
        "ds_wrxchg2_rtn_b32" => Some((Kind::Rmw, 4, false)),
        "ds_wrxchg2st64_rtn_b32" => Some((Kind::Rmw, 4, true)),
        "ds_wrxchg2_rtn_b64" => Some((Kind::Rmw, 8, false)),
        "ds_wrxchg2st64_rtn_b64" => Some((Kind::Rmw, 8, true)),
        _ => None,
    } {
        let scale = width * if st64 { 64 } else { 1 };
        return DsClass::Two {
            kind,
            off0: u64::from(*offset0) * scale,
            off1: u64::from(*offset1) * scale,
            width,
        };
    }

    let off = u64::from(*offset0) | (u64::from(*offset1) << 8);
    if let Some(width) = match name {
        "ds_read_i8"
        | "ds_read_u8"
        | "ds_read_u8_d16"
        | "ds_read_u8_d16_hi"
        | "ds_read_i8_d16"
        | "ds_read_i8_d16_hi" => Some(1),
        "ds_read_i16" | "ds_read_u16" | "ds_read_u16_d16" | "ds_read_u16_d16_hi" => Some(2),
        "ds_read_b32" => Some(4),
        "ds_read_b64" => Some(8),
        "ds_read_b96" => Some(12),
        "ds_read_b128" => Some(16),
        _ => None,
    } {
        return DsClass::One {
            kind: Kind::Read,
            off,
            width,
        };
    }

    if let Some(width) = match name {
        "ds_write_b8" | "ds_write_b8_d16_hi" => Some(1),
        "ds_write_b16" | "ds_write_b16_d16_hi" => Some(2),
        "ds_write_b32" => Some(4),
        "ds_write_b64" => Some(8),
        "ds_write_b96" => Some(12),
        "ds_write_b128" => Some(16),
        _ => None,
    } {
        return DsClass::One {
            kind: Kind::Write,
            off,
            width,
        };
    }

    if let Some(rest) = name.strip_prefix("ds_") {
        let stem = rest.replace("_rtn", "");
        let width = match stem.as_str() {
            "add_u32" | "sub_u32" | "rsub_u32" | "inc_u32" | "dec_u32" | "min_i32"
            | "max_i32" | "min_u32" | "max_u32" | "and_b32" | "or_b32" | "xor_b32"
            | "mskor_b32" | "wrxchg_b32" | "cmpst_b32" | "cmpst_f32" | "cmpswap_b32"
            | "cmpswap_f32" | "min_f32" | "max_f32" | "add_f32" | "wrap_b32" => Some(4),
            "add_u64" | "sub_u64" | "rsub_u64" | "inc_u64" | "dec_u64" | "min_i64"
            | "max_i64" | "min_u64" | "max_u64" | "and_b64" | "or_b64" | "xor_b64"
            | "mskor_b64" | "wrxchg_b64" | "cmpst_b64" | "cmpst_f64" | "cmpswap_b64"
            | "cmpswap_f64" | "min_f64" | "max_f64" | "condxchg32_b64" => Some(8),
            _ => None,
        };
        if let Some(width) = width {
            return DsClass::One {
                kind: Kind::Rmw,
                off,
                width,
            };
        }
    }

    DsClass::Unknown
}

/// Refinement A: accepts when every access resolves to a constant offset on one shared
/// group-uniform base, with zero RMWs (multi-lane same-address RMWs conflict across halves),
/// zero RAW/WAR overlaps, and zero distinct-write WAW overlaps.
fn region_disjoint_ranges(accesses: &[Access]) -> bool {
    let Some(first) = accesses.first().and_then(|a| a.expr.as_ref()) else {
        return false;
    };
    if first.tid != [0, 0, 0] || first.console_lane != 0 {
        return false;
    }
    let shared_uniforms = &first.uniforms;
    let align4 = first.uniform_tz >= 2;

    for a in accesses {
        if a.kind == Kind::Rmw {
            return false;
        }
        if a.kind == Kind::Write && !a.uniform_data {
            return false;
        }
        let Some(e) = &a.expr else {
            return false;
        };
        if e.tid != [0, 0, 0] || e.console_lane != 0 || &e.uniforms != shared_uniforms {
            return false;
        }
    }

    for (i, a) in accesses.iter().enumerate() {
        for b in accesses.iter().skip(i + 1) {
            if !matches!((a.kind, b.kind), (Kind::Write, _) | (_, Kind::Write)) {
                continue;
            }
            let (Some(ea), Some(eb)) = (&a.expr, &b.expr) else {
                return false;
            };
            if ranges_may_overlap(ea.off, a.width, eb.off, b.width, align4) {
                return false;
            }
        }
    }
    true
}

/// Conservative overlap test for constant offsets relative to a shared uniform base.
/// When `align4` is false, a `>=4`-byte dword-indexed access may share an enclosing dword with
/// a `<4`-byte access up to 3 bytes below `off`.
fn ranges_may_overlap(off_a: u64, w_a: u64, off_b: u64, w_b: u64, align4: bool) -> bool {
    let range_for = |off: u64, w: u64, peer_w: u64| -> (u64, u64) {
        if w >= 4 {
            if align4 {
                let lo = off & !3;
                let hi = ((off + w - 4) & !3) + 4;
                (lo, hi)
            } else if peer_w < 4 {
                (off.saturating_sub(3), off + w)
            } else {
                (off, off + w)
            }
        } else {
            (off, off + w)
        }
    };
    let (a0, a1) = range_for(off_a, w_a, w_b);
    let (b0, b1) = range_for(off_b, w_b, w_a);
    a0 < b1 && b0 < a1
}

/// Checks if a write-only region contains two distinct writes that provably resolve to
/// overlapping byte ranges on a shared group-uniform base.
fn has_provable_shared_waw(accesses: &[Access]) -> bool {
    for (i, a) in accesses.iter().enumerate() {
        let Some(ea) = &a.expr else { continue };
        if ea.tid != [0, 0, 0] || ea.console_lane != 0 {
            continue;
        }
        for b in accesses.iter().skip(i + 1) {
            let Some(eb) = &b.expr else { continue };
            if eb.tid != [0, 0, 0] || eb.console_lane != 0 || ea.uniforms != eb.uniforms {
                continue;
            }
            if ranges_may_overlap(ea.off, a.width, eb.off, b.width, ea.uniform_tz >= 2) {
                return true;
            }
        }
    }
    false
}

/// Refinement B: accepts when every access in the region resolves cleanly and every writing/RMW
/// cluster proves exclusive per-thread slots whose cross-cluster footprints do not alias.
fn region_lane_private(accesses: &[Access], threads: [u32; 3]) -> bool {
    if accesses.is_empty() || threads.contains(&0) {
        return false;
    }
    let nx = u64::from(threads[0]);
    let ny = u64::from(threads[1]);
    let nz = u64::from(threads[2]);
    let Some(group) = nx.checked_mul(ny).and_then(|v| v.checked_mul(nz)) else {
        return false;
    };
    if group == 0 {
        return false;
    }

    #[derive(Clone, PartialEq, Eq)]
    struct ClusterKey {
        uniforms: Vec<(Value, u32)>,
        tid: [u64; 3],
        console_lane: u64,
    }

    struct Cluster<'a> {
        key: ClusterKey,
        uniform_tz: u32,
        items: Vec<(&'a Access, &'a ResolvedAddr)>,
    }

    let mut clusters: Vec<Cluster<'_>> = Vec::new();
    for a in accesses {
        let Some(e) = &a.expr else {
            return false;
        };
        let mut tid = e.tid;
        let mut cl = e.console_lane;
        // On 1D groups up to 64 threads, console_lane equals ThreadIdInGroup(0).
        if ny == 1 && nz == 1 && group <= 64 && cl != 0 {
            let Some(sum) = tid[0].checked_add(cl) else {
                return false;
            };
            tid[0] = sum;
            cl = 0;
        }
        let key = ClusterKey {
            uniforms: e.uniforms.clone(),
            tid,
            console_lane: cl,
        };
        if let Some(c) = clusters.iter_mut().find(|c| c.key == key) {
            c.items.push((a, e));
        } else {
            clusters.push(Cluster {
                key,
                uniform_tz: e.uniform_tz,
                items: vec![(a, e)],
            });
        }
    }

    struct Footprint {
        uniforms: Vec<(Value, u32)>,
        uniform_tz: u32,
        has_write: bool,
        min_lo: u64,
        max_hi: u64,
        max_width: u64,
    }

    let mut footprints = Vec::with_capacity(clusters.len());
    for c in &clusters {
        let cluster_has_write = c.items.iter().any(|(a, _)| a.kind != Kind::Read);
        let max_width = c.items.iter().map(|(a, _)| a.width).max().unwrap_or(0);
        let need_align4 = max_width >= 4;
        let need_align2 = !need_align4 && c.items.iter().any(|(a, _)| a.width == 2);

        if need_align4 && c.uniform_tz < 2 {
            return false;
        }
        if need_align2 && c.uniform_tz < 1 {
            return false;
        }

        let unit_disp = if c.key.tid == [0, 0, 0] && c.key.console_lane == 0 {
            if cluster_has_write {
                return false;
            }
            0
        } else {
            let Some(d) = injective_lane_disp(
                c.key.tid,
                c.key.console_lane,
                [nx, ny, nz],
                group,
                1,
                need_align4,
                need_align2,
            ) else {
                return false;
            };
            d
        };

        let mut spans: Vec<(u64, u64, bool, u64)> = Vec::with_capacity(c.items.len());
        for (a, e) in &c.items {
            let (lo, hi) = if need_align4 {
                if a.width >= 4 && (!e.off.is_multiple_of(4) || !a.width.is_multiple_of(4)) {
                    return false;
                }
                (e.off & !3, (e.off + a.width).next_multiple_of(4))
            } else if need_align2 {
                if a.width == 2 && !e.off.is_multiple_of(2) {
                    return false;
                }
                (e.off & !1, (e.off + a.width).next_multiple_of(2))
            } else {
                (e.off, e.off + a.width)
            };
            spans.push((lo, hi, a.kind != Kind::Read, a.width));
        }
        spans.sort_unstable_by_key(|&(lo, hi, _, _)| (lo, hi));

        // Split non-overlapping sub-arrays that share the same `(base, tid_stride)` when a
        // subsequent offset begins at or past the end of the prior array's full group footprint.
        let mut sub_start = 0usize;
        while sub_start < spans.len() {
            let min_slot = spans[sub_start].0;
            let mut max_slot = spans[sub_start].1;
            let mut sub_write = spans[sub_start].2;
            let mut sub_max_w = spans[sub_start].3;
            let mut sub_end = sub_start + 1;
            while sub_end < spans.len() {
                let (lo, hi, is_w, w) = spans[sub_end];
                let Some(reach) = unit_disp.checked_add(max_slot) else {
                    return false;
                };
                if lo >= reach {
                    break;
                }
                max_slot = max_slot.max(hi);
                sub_write |= is_w;
                sub_max_w = sub_max_w.max(w);
                sub_end += 1;
            }

            let slot_span = max_slot.saturating_sub(min_slot);
            if slot_span == 0 {
                return false;
            }
            if (sub_write || !(c.key.tid == [0, 0, 0] && c.key.console_lane == 0))
                && injective_lane_disp(
                    c.key.tid,
                    c.key.console_lane,
                    [nx, ny, nz],
                    group,
                    slot_span,
                    need_align4,
                    need_align2,
                )
                .is_none()
            {
                return false;
            }
            let Some(max_hi) = unit_disp.checked_add(max_slot) else {
                return false;
            };
            if max_hi > MAX_OFFSET {
                return false;
            }
            footprints.push(Footprint {
                uniforms: c.key.uniforms.clone(),
                uniform_tz: c.uniform_tz,
                has_write: sub_write,
                min_lo: min_slot,
                max_hi,
                max_width: sub_max_w,
            });
            sub_start = sub_end;
        }
    }

    for (i, a) in footprints.iter().enumerate() {
        for b in footprints.iter().skip(i + 1) {
            if !a.has_write && !b.has_write {
                continue;
            }
            if a.uniforms != b.uniforms {
                return false;
            }
            // Compare the already-computed half-open footprint intervals `[min_lo, max_hi)`
            // directly: `ranges_may_overlap` expects single-instruction widths and truncates
            // non-4-multiple aggregate widths (e.g. `n = 33`) down to a multiple of 4 on the
            // aligned path.
            let align4 = a.uniform_tz >= 2 && b.uniform_tz >= 2;
            if (a.min_lo < b.max_hi && b.min_lo < a.max_hi)
                || ((a.max_width >= 4 || b.max_width >= 4) && !align4)
            {
                return false;
            }
        }
    }
    true
}

/// Verifies that `(tid[0..3], console_lane)` maps every thread in `threads` injectively with
/// spacing at least `slot_span` and required byte alignment, returning the maximum byte
/// displacement across all threads in the workgroup.
fn injective_lane_disp(
    tid: [u64; 3],
    console_lane: u64,
    threads: [u64; 3],
    group: u64,
    slot_span: u64,
    need_align4: bool,
    need_align2: bool,
) -> Option<u64> {
    let check_align = |stride: u64| -> bool {
        (!need_align4 || stride.is_multiple_of(4)) && (!need_align2 || stride.is_multiple_of(2))
    };

    if console_lane != 0 {
        if group > 64 || tid != [0, 0, 0] || !check_align(console_lane) || slot_span > console_lane {
            return None;
        }
        return (group - 1).checked_mul(console_lane);
    }

    let mut axes: Vec<(u64, u64)> = Vec::new();
    for d in 0..3 {
        if threads[d] > 1 {
            if tid[d] == 0 || !check_align(tid[d]) {
                return None;
            }
            axes.push((tid[d], threads[d]));
        }
    }
    if axes.is_empty() {
        return None;
    }
    axes.sort_unstable_by_key(|&(c, _)| c);

    let mut running_disp = 0u64;
    for &(stride, count) in &axes {
        let min_next = running_disp.checked_add(slot_span)?;
        if stride < min_next {
            return None;
        }
        running_disp = running_disp.checked_add((count - 1).checked_mul(stride)?)?;
    }
    Some(running_disp)
}

struct AddrCtx<'a> {
    p: &'a Program,
    st: &'a Structured,
    all: &'a [bool],
    cs: ComputeIo,
    idom: Vec<usize>,
    uniform_cfg: Vec<bool>,
    def_inst: HashMap<Value, (usize, usize)>,
    def_phi: HashMap<Value, (usize, usize)>,
    waterfall_loads: HashSet<(usize, usize)>,
    scalar: HashMap<(usize, usize), Address>,
    writes_memory: bool,
    reg_inputs: HashMap<Value, InputKind>,
    /// Memoized acyclic `eval_val` results (never populated when a cycle hit occurred).
    eval_memo: RefCell<HashMap<Value, Option<SymVal>>>,
}

impl<'a> AddrCtx<'a> {
    fn new(
        p: &'a Program,
        st: &'a Structured,
        io: &'a Io,
        res: &'a Resources,
        cs: ComputeIo,
        all: &'a [bool],
    ) -> Self {
        let n = p.blocks.len();
        let order = reverse_postorder(&p.blocks);
        let mut rpo_index = vec![usize::MAX; n];
        for (idx, &b) in order.iter().enumerate() {
            rpo_index[b] = idx;
        }
        let idom = idoms(&p.blocks, &order, &rpo_index);

        let mut def_inst = HashMap::new();
        let mut def_phi = HashMap::new();
        for (b, block) in p.blocks.iter().enumerate() {
            for (pi, phi) in block.phis.iter().enumerate() {
                def_phi.insert(phi.value, (b, pi));
            }
            for (ii, inst) in block.insts.iter().enumerate() {
                for &(_, v) in &inst.writes {
                    def_inst.insert(v, (b, ii));
                }
            }
        }

        let writes_memory = p.blocks.iter().flat_map(|b| &b.insts).any(|inst| {
            let n = inst.name();
            match inst.fields {
                Fields::Buffer { .. } | Fields::Image { .. } => {
                    n.contains("store") || n.contains("atomic")
                }
                Fields::Flat { .. } => !n.starts_with("global_load_"),
                Fields::Smem { .. } => n.contains("store") || n.contains("atomic"),
                _ => false,
            }
        });

        let io_map: HashMap<Value, Input> = io.inputs.iter().copied().collect();
        let user_sgprs = cs.user_sgprs();
        let mut reg_inputs = HashMap::new();
        for &(reg, val) in &p.inputs {
            let kind = match io_map.get(&val).copied() {
                Some(Input::ThreadIdInGroup(c)) if c < 3 => InputKind::Tid(usize::from(c)),
                Some(Input::GroupId(_)) => InputKind::GroupUniform,
                Some(_) => InputKind::NonUniform,
                None => match reg {
                    Reg::S(c) if c < user_sgprs => InputKind::GroupUniform,
                    _ => InputKind::NonUniform,
                },
            };
            reg_inputs.insert(val, kind);
        }

        let waterfall_loads: HashSet<(usize, usize)> = res.waterfall.loads.iter().copied().collect();
        let scalar: HashMap<(usize, usize), Address> = res.scalar.iter().copied().collect();
        let uniform_cfg = compute_uniform_cfg(
            p,
            st,
            all,
            &def_inst,
            &reg_inputs,
            &waterfall_loads,
            &scalar,
            writes_memory,
        );

        Self {
            p,
            st,
            all,
            cs,
            idom,
            uniform_cfg,
            def_inst,
            def_phi,
            waterfall_loads,
            scalar,
            writes_memory,
            reg_inputs,
            eval_memo: RefCell::new(HashMap::new()),
        }
    }

    fn dominates(&self, a: usize, mut b: usize) -> bool {
        if self.idom.get(b).copied().unwrap_or(usize::MAX) == usize::MAX {
            return false;
        }
        loop {
            if a == b {
                return true;
            }
            let d = self.idom.get(b).copied().unwrap_or(usize::MAX);
            if d == b || d == usize::MAX {
                return false;
            }
            b = d;
        }
    }

    fn ds_write_data_uniform(&self, inst: &Inst, use_block: usize, slot: usize) -> bool {
        let Fields::Ds { data, .. } = &inst.fields else {
            return false;
        };
        let Some(op) = data.get(slot) else {
            return false;
        };
        self.is_uniform_operand(inst, op, use_block)
    }

    fn is_uniform_operand(&self, inst: &Inst, op: &Operand, use_block: usize) -> bool {
        match *op {
            Operand::Const(c) => inline(c).is_some(),
            Operand::Literal(_) => true,
            Operand::Scalar { code, .. } if code == NULL => true,
            Operand::Scalar { code, dwords } => {
                if (106..PARKED).contains(&code) {
                    return false;
                }
                (0..dwords).all(|k| {
                    inst.read(Reg::S(code + k))
                        .is_some_and(|v| self.is_uniform_val_at(v, use_block))
                })
            }
            Operand::Vector { reg, dwords } => (0..dwords).all(|k| {
                inst.read(Reg::V(reg + k))
                    .is_some_and(|v| self.is_uniform_val_at(v, use_block))
            }),
            Operand::Attr { .. } => false,
        }
    }

    fn is_uniform_val_at(&self, v: Value, use_block: usize) -> bool {
        let mut visiting = HashSet::new();
        let mut hit_cycle = false;
        if let Some(sym) = self.eval_val(v, use_block, &mut visiting, &mut hit_cycle) {
            match sym {
                SymVal::Const(_) => return true,
                SymVal::Linear(e) if e.tid == [0, 0, 0] && e.console_lane == 0 => return true,
                _ => {}
            }
        }
        if let Some(&(def_b, _)) = self.def_inst.get(&v).or_else(|| self.def_phi.get(&v))
            && !self.dominates(def_b, use_block)
        {
            return false;
        }
        let mut seen = HashSet::new();
        is_group_uniform_val(
            self.p,
            self.st,
            self.all,
            &self.uniform_cfg,
            &self.def_inst,
            &self.def_phi,
            &self.waterfall_loads,
            &self.scalar,
            self.writes_memory,
            &self.reg_inputs,
            v,
            &mut seen,
        )
    }

    fn resolve_ds_addr(
        &self,
        inst: &Inst,
        use_block: usize,
        ds_off: u64,
        width: u64,
    ) -> Option<ResolvedAddr> {
        let Fields::Ds { addr: Some(op), .. } = &inst.fields else {
            return None;
        };
        let mut visiting = HashSet::new();
        let mut hit_cycle = false;
        let sym = self.eval_operand(inst, op, use_block, &mut visiting, &mut hit_cycle)?;
        match sym {
            SymVal::Const(c) => {
                let off = u64::from(c).checked_add(ds_off)?;
                if off.checked_add(width)? > MAX_OFFSET {
                    return None;
                }
                Some(ResolvedAddr {
                    uniforms: Vec::new(),
                    uniform_tz: 32,
                    tid: [0; 3],
                    console_lane: 0,
                    off,
                })
            }
            SymVal::Linear(e) => {
                let off = e.offset.checked_add(ds_off)?;
                if off.checked_add(width)? > MAX_OFFSET {
                    return None;
                }
                Some(ResolvedAddr {
                    uniforms: e.uniforms,
                    uniform_tz: e.uniform_tz,
                    tid: e.tid,
                    console_lane: e.console_lane,
                    off,
                })
            }
            SymVal::MbcntLoZero | SymVal::MbcntHiZero => None,
        }
    }

    fn eval_operand(
        &self,
        inst: &Inst,
        op: &Operand,
        use_block: usize,
        visiting: &mut HashSet<Value>,
        hit_cycle: &mut bool,
    ) -> Option<SymVal> {
        match *op {
            Operand::Const(c) => inline(c).map(SymVal::Const),
            Operand::Literal(x) => Some(SymVal::Const(x)),
            Operand::Scalar { code, .. } if code == NULL => Some(SymVal::Const(0)),
            Operand::Scalar { code, dwords: 1 } if !(106..PARKED).contains(&code) => {
                let v = inst.read(Reg::S(code))?;
                self.eval_val(v, use_block, visiting, hit_cycle)
            }
            Operand::Vector { reg, dwords: 1 } => {
                let v = inst.read(Reg::V(reg))?;
                self.eval_val(v, use_block, visiting, hit_cycle)
            }
            _ => None,
        }
    }

    fn eval_val(
        &self,
        v: Value,
        use_block: usize,
        visiting: &mut HashSet<Value>,
        hit_cycle: &mut bool,
    ) -> Option<SymVal> {
        if let Some(&kind) = self.reg_inputs.get(&v) {
            return match kind {
                InputKind::Tid(c) => Some(self.tid_sym(c)),
                InputKind::GroupUniform => Some(SymVal::Linear(uniform_unit(v, 0))),
                InputKind::NonUniform => None,
            };
        }

        if let Some(&(def_b, pi)) = self.def_phi.get(&v) {
            if !self.uniform_block(def_b) || !self.dominates(def_b, use_block) {
                return None;
            }
            if let Some(cached) = self.eval_memo.borrow().get(&v).cloned() {
                return cached;
            }
            let phi = &self.p.blocks[def_b].phis[pi];
            if !self.p.blocks[def_b].preds.iter().all(|&q| self.uniform_block(q)) {
                self.eval_memo.borrow_mut().insert(v, None);
                return None;
            }
            let mut seen = HashSet::new();
            let uniform_ok = phi.args.iter().all(|&a| {
                is_group_uniform_val(
                    self.p,
                    self.st,
                    self.all,
                    &self.uniform_cfg,
                    &self.def_inst,
                    &self.def_phi,
                    &self.waterfall_loads,
                    &self.scalar,
                    self.writes_memory,
                    &self.reg_inputs,
                    a,
                    &mut seen,
                )
            });
            let res = uniform_ok.then(|| SymVal::Linear(uniform_unit(v, 0)));
            self.eval_memo.borrow_mut().insert(v, res.clone());
            return res;
        }

        let &(def_b, def_i) = self.def_inst.get(&v)?;
        if !self.dominates(def_b, use_block)
            || self.st.guards[def_b][def_i].is_some()
            || self.waterfall_loads.contains(&(def_b, def_i))
        {
            return None;
        }
        if let Some(cached) = self.eval_memo.borrow().get(&v).cloned() {
            return cached;
        }
        if !visiting.insert(v) {
            *hit_cycle = true;
            return None;
        }

        let inst = &self.p.blocks[def_b].insts[def_i];
        let mut sub_cycle = false;
        let folded = self.eval_inst(inst, def_b, v, visiting, &mut sub_cycle);
        visiting.remove(&v);
        let res = if folded.is_some() {
            folded
        } else if self.uniform_block(def_b) {
            let mut seen = HashSet::new();
            if is_group_uniform_val(
                self.p,
                self.st,
                self.all,
                &self.uniform_cfg,
                &self.def_inst,
                &self.def_phi,
                &self.waterfall_loads,
                &self.scalar,
                self.writes_memory,
                &self.reg_inputs,
                v,
                &mut seen,
            ) {
                let tz = uniform_result_tz(inst, v);
                Some(SymVal::Linear(uniform_unit(v, tz)))
            } else {
                None
            }
        } else {
            None
        };
        if sub_cycle {
            *hit_cycle = true;
        } else {
            self.eval_memo.borrow_mut().insert(v, res.clone());
        }
        res
    }

    fn uniform_block(&self, b: usize) -> bool {
        self.all.get(b).copied().unwrap_or(false)
            && self.uniform_cfg.get(b).copied().unwrap_or(false)
    }

    fn tid_sym(&self, c: usize) -> SymVal {
        let count = u64::from(self.cs.threads[c]);
        if count <= 1 {
            return SymVal::Const(0);
        }
        let max_tid = self.cs.threads[c] - 1;
        let active_bits = u32::BITS - max_tid.leading_zeros();
        let active_mask = if active_bits == 32 {
            u32::MAX
        } else {
            (1u32 << active_bits) - 1
        };
        let mut tid = [0u64; 3];
        tid[c] = 1;
        SymVal::Linear(AddrExpr {
            offset: 0,
            uniforms: Vec::new(),
            uniform_tz: 32,
            tid,
            console_lane: 0,
            known_zero: !active_mask,
            max_val: Some(u64::from(max_tid)),
        })
    }

    fn eval_inst(
        &self,
        inst: &Inst,
        def_b: usize,
        val: Value,
        visiting: &mut HashSet<Value>,
        hit_cycle: &mut bool,
    ) -> Option<SymVal> {
        let name = inst.name();
        match &inst.fields {
            Fields::Sop { dst, src, imm } => {
                if !matches!(
                    dst,
                    Some(Operand::Scalar { code, dwords: 1 })
                        if inst.write(Reg::S(*code)) == Some(val)
                ) {
                    return None;
                }
                let mut sop = |k: usize| {
                    src.get(k)
                        .and_then(|op| self.eval_operand(inst, op, def_b, visiting, hit_cycle))
                };
                match name {
                    "s_mov_b32" => sop(0),
                    "s_movk_i32" => Some(SymVal::Const(i32::from(imm.unwrap_or(0) as i16) as u32)),
                    "s_add_u32" | "s_add_i32" => sym_add(sop(0)?, sop(1)?),
                    "s_addk_i32" => {
                        let a = self.eval_operand(inst, dst.as_ref()?, def_b, visiting, hit_cycle)?;
                        let b = SymVal::Const(i32::from(imm.unwrap_or(0) as i16) as u32);
                        sym_add(a, b)
                    }
                    "s_mul_i32" => sym_mul(sop(0)?, sop(1)?),
                    "s_mulk_i32" => {
                        let a = self.eval_operand(inst, dst.as_ref()?, def_b, visiting, hit_cycle)?;
                        let b = SymVal::Const(i32::from(imm.unwrap_or(0) as i16) as u32);
                        sym_mul(a, b)
                    }
                    "s_lshl_b32" => sym_shl(sop(0)?, sop(1)?),
                    "s_lshl1_add_u32" => sym_add(sym_shl(sop(0)?, SymVal::Const(1))?, sop(1)?),
                    "s_lshl2_add_u32" => sym_add(sym_shl(sop(0)?, SymVal::Const(2))?, sop(1)?),
                    "s_lshl3_add_u32" => sym_add(sym_shl(sop(0)?, SymVal::Const(3))?, sop(1)?),
                    "s_lshl4_add_u32" => sym_add(sym_shl(sop(0)?, SymVal::Const(4))?, sop(1)?),
                    "s_and_b32" => sym_and(sop(0)?, sop(1)?),
                    "s_or_b32" => sym_or(sop(0)?, sop(1)?),
                    _ => None,
                }
            }
            Fields::Valu(v) => {
                if has_valu_modifiers(v)
                    || !matches!(
                        v.dst,
                        Some(Operand::Vector { reg, dwords: 1 })
                            if inst.write(Reg::V(reg)) == Some(val)
                    )
                {
                    return None;
                }
                let mut src = |k: usize| {
                    v.src
                        .get(k)
                        .and_then(|op| self.eval_operand(inst, op, def_b, visiting, hit_cycle))
                };
                match name {
                    "v_mov_b32" => src(0),
                    "v_add_nc_u32" | "v_add_u32" | "v_add_co_u32" => sym_add(src(0)?, src(1)?),
                    "v_add3_u32" => sym_add(sym_add(src(0)?, src(1)?)?, src(2)?),
                    "v_mul_lo_u32" => sym_mul(src(0)?, src(1)?),
                    "v_mul_u32_u24" | "v_mul_i32_i24" => {
                        let signed = name.ends_with("i24");
                        sym_mul24(src(0)?, src(1)?, signed)
                    }
                    "v_mad_u32_u24" | "v_mad_i32_i24" => {
                        let signed = name.ends_with("i24");
                        let prod = sym_mul24(src(0)?, src(1)?, signed)?;
                        sym_add(prod, src(2)?)
                    }
                    "v_lshlrev_b32" => sym_shl(src(1)?, src(0)?),
                    "v_lshl_add_u32" => sym_add(sym_shl(src(0)?, src(1)?)?, src(2)?),
                    "v_add_lshl_u32" => sym_shl(sym_add(src(0)?, src(1)?)?, src(2)?),
                    "v_lshl_or_b32" => sym_or(sym_shl(src(0)?, src(1)?)?, src(2)?),
                    "v_and_b32" => sym_and(src(0)?, src(1)?),
                    "v_or_b32" => sym_or(src(0)?, src(1)?),
                    "v_bfe_u32" => match (src(0)?, src(1)?, src(2)?) {
                        (x, SymVal::Const(0), SymVal::Const(w)) => {
                            let w = w & 31;
                            if w == 0 {
                                Some(SymVal::Const(0))
                            } else {
                                let mask = (1u32 << w) - 1;
                                sym_and(x, SymVal::Const(mask))
                            }
                        }
                        _ => None,
                    },
                    "v_mbcnt_lo_u32_b32" => match (src(0)?, src(1)?) {
                        (SymVal::Const(u32::MAX), SymVal::Const(0)) => Some(SymVal::MbcntLoZero),
                        (SymVal::Const(u32::MAX), SymVal::MbcntHiZero) => Some(console_lane_sym()),
                        _ => None,
                    },
                    "v_mbcnt_hi_u32_b32" => match (src(0)?, src(1)?) {
                        (SymVal::Const(u32::MAX), SymVal::Const(0)) => Some(SymVal::MbcntHiZero),
                        (SymVal::Const(u32::MAX), SymVal::MbcntLoZero) => Some(console_lane_sym()),
                        _ => None,
                    },
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

fn has_valu_modifiers(v: &crate::gfx10::Valu) -> bool {
    v.neg != 0
        || v.abs != 0
        || v.neg_hi != 0
        || v.opsel != 0
        || v.opsel_hi != 0
        || v.clamp
        || v.omod != 0
        || v.sdwa.is_some()
        || v.dpp.is_some()
}

fn uniform_unit(val: Value, tz: u32) -> AddrExpr {
    let known_zero = if tz >= 32 {
        u32::MAX
    } else if tz == 0 {
        0
    } else {
        (1u32 << tz) - 1
    };
    AddrExpr {
        offset: 0,
        uniforms: vec![(val, 1)],
        uniform_tz: tz,
        tid: [0; 3],
        console_lane: 0,
        known_zero,
        max_val: None,
    }
}

fn console_lane_sym() -> SymVal {
    SymVal::Linear(AddrExpr {
        offset: 0,
        uniforms: Vec::new(),
        uniform_tz: 32,
        tid: [0; 3],
        console_lane: 1,
        known_zero: !63,
        max_val: Some(63),
    })
}

fn known_zero_of(s: &SymVal) -> u32 {
    match s {
        SymVal::Const(c) => !c,
        SymVal::Linear(e) => e.known_zero,
        SymVal::MbcntLoZero | SymVal::MbcntHiZero => !63,
    }
}

fn max_u24_bound(s: &SymVal, signed: bool) -> Option<u64> {
    let limit = if signed { 1u64 << 23 } else { 1u64 << 24 };
    let v = match s {
        SymVal::Const(c) => u64::from(*c),
        SymVal::Linear(e) => e.max_val?,
        SymVal::MbcntLoZero | SymVal::MbcntHiZero => 63,
    };
    (v < limit).then_some(v)
}

fn merge_uniforms(a: &[(Value, u32)], b: &[(Value, u32)]) -> Option<Vec<(Value, u32)>> {
    let mut out: Vec<(Value, u32)> = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].0.0.cmp(&b[j].0.0) {
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                let sum = a[i].1.checked_add(b[j].1)?;
                out.push((a[i].0, sum));
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    Some(out)
}

fn tz_mask(tz: u32) -> u32 {
    if tz >= 32 {
        u32::MAX
    } else if tz == 0 {
        0
    } else {
        (1u32 << tz) - 1
    }
}

fn sym_add(a: SymVal, b: SymVal) -> Option<SymVal> {
    match (a, b) {
        (SymVal::Const(x), SymVal::Const(y)) => Some(SymVal::Const(x.wrapping_add(y))),
        (SymVal::Linear(mut e), SymVal::Const(c)) | (SymVal::Const(c), SymVal::Linear(mut e)) => {
            if u64::from(c) >= MAX_OFFSET {
                return None;
            }
            let off = e.offset.checked_add(u64::from(c))?;
            if off >= MAX_OFFSET {
                return None;
            }
            let tz = e.known_zero.trailing_ones().min(c.trailing_zeros());
            e.offset = off;
            e.known_zero = tz_mask(tz);
            e.max_val = e
                .max_val
                .and_then(|m| m.checked_add(u64::from(c)))
                .filter(|&m| m <= u64::from(u32::MAX));
            Some(SymVal::Linear(e))
        }
        (SymVal::Linear(ea), SymVal::Linear(eb)) => {
            let off = ea.offset.checked_add(eb.offset)?;
            if off >= MAX_OFFSET {
                return None;
            }
            let uniforms = merge_uniforms(&ea.uniforms, &eb.uniforms)?;
            let uniform_tz = ea.uniform_tz.min(eb.uniform_tz);
            let mut tid = [0u64; 3];
            for d in 0..3 {
                tid[d] = ea.tid[d].checked_add(eb.tid[d])?;
                if tid[d] >= MAX_OFFSET {
                    return None;
                }
            }
            let console_lane = ea.console_lane.checked_add(eb.console_lane)?;
            if console_lane >= MAX_OFFSET {
                return None;
            }
            let tz = ea.known_zero.trailing_ones().min(eb.known_zero.trailing_ones());
            let max_val = match (ea.max_val, eb.max_val) {
                (Some(ma), Some(mb)) => ma.checked_add(mb).filter(|&m| m <= u64::from(u32::MAX)),
                _ => None,
            };
            Some(SymVal::Linear(AddrExpr {
                offset: off,
                uniforms,
                uniform_tz,
                tid,
                console_lane,
                known_zero: tz_mask(tz),
                max_val,
            }))
        }
        _ => None,
    }
}

fn sym_shl(x: SymVal, shift: SymVal) -> Option<SymVal> {
    let SymVal::Const(raw) = shift else {
        return None;
    };
    let s = raw & 31;
    match x {
        SymVal::Const(c) => Some(SymVal::Const(c.wrapping_shl(s))),
        SymVal::Linear(mut e) => {
            if s > 20 {
                return None;
            }
            let factor_u64 = 1u64 << s;
            let factor_u32 = u32::try_from(factor_u64).ok()?;
            let off = e.offset.checked_mul(factor_u64)?;
            if off >= MAX_OFFSET {
                return None;
            }
            for (_, coeff) in &mut e.uniforms {
                *coeff = coeff.checked_mul(factor_u32)?;
            }
            e.uniform_tz = if e.uniforms.is_empty() {
                32
            } else {
                e.uniform_tz.saturating_add(s).min(32)
            };
            for c in &mut e.tid {
                *c = c.checked_mul(factor_u64)?;
                if *c >= MAX_OFFSET {
                    return None;
                }
            }
            e.console_lane = e.console_lane.checked_mul(factor_u64)?;
            if e.console_lane >= MAX_OFFSET {
                return None;
            }
            e.offset = off;
            e.known_zero = e.known_zero.wrapping_shl(s) | tz_mask(s);
            e.max_val = e
                .max_val
                .and_then(|m| m.checked_mul(factor_u64))
                .filter(|&m| m <= u64::from(u32::MAX));
            Some(SymVal::Linear(e))
        }
        _ => None,
    }
}

fn sym_mul(a: SymVal, b: SymVal) -> Option<SymVal> {
    match (a, b) {
        (SymVal::Const(x), SymVal::Const(y)) => Some(SymVal::Const(x.wrapping_mul(y))),
        (SymVal::Linear(mut e), SymVal::Const(k)) | (SymVal::Const(k), SymVal::Linear(mut e)) => {
            if k == 0 {
                return Some(SymVal::Const(0));
            }
            let k64 = u64::from(k);
            if k64 >= MAX_OFFSET {
                return None;
            }
            let off = e.offset.checked_mul(k64)?;
            if off >= MAX_OFFSET {
                return None;
            }
            for (_, coeff) in &mut e.uniforms {
                *coeff = coeff.checked_mul(k)?;
            }
            e.uniform_tz = if e.uniforms.is_empty() {
                32
            } else {
                e.uniform_tz.saturating_add(k.trailing_zeros()).min(32)
            };
            for c in &mut e.tid {
                *c = c.checked_mul(k64)?;
                if *c >= MAX_OFFSET {
                    return None;
                }
            }
            e.console_lane = e.console_lane.checked_mul(k64)?;
            if e.console_lane >= MAX_OFFSET {
                return None;
            }
            e.offset = off;
            let tz = e
                .known_zero
                .trailing_ones()
                .saturating_add(k.trailing_zeros())
                .min(32);
            e.known_zero = tz_mask(tz);
            e.max_val = e
                .max_val
                .and_then(|m| m.checked_mul(k64))
                .filter(|&m| m <= u64::from(u32::MAX));
            Some(SymVal::Linear(e))
        }
        _ => None,
    }
}

fn sym_mul24(a: SymVal, b: SymVal, signed: bool) -> Option<SymVal> {
    max_u24_bound(&a, signed)?;
    max_u24_bound(&b, signed)?;
    sym_mul(a, b)
}

fn sym_and(a: SymVal, b: SymVal) -> Option<SymVal> {
    match (a, b) {
        (SymVal::Const(x), SymVal::Const(y)) => Some(SymVal::Const(x & y)),
        (SymVal::Linear(e), SymVal::Const(mask)) | (SymVal::Const(mask), SymVal::Linear(e)) => {
            let active = !e.known_zero;
            ((mask & active) == active).then_some(SymVal::Linear(e))
        }
        _ => None,
    }
}

fn sym_or(a: SymVal, b: SymVal) -> Option<SymVal> {
    if let (SymVal::Const(x), SymVal::Const(y)) = (&a, &b) {
        return Some(SymVal::Const(x | y));
    }
    let active_a = !known_zero_of(&a);
    let active_b = !known_zero_of(&b);
    if (active_a & active_b) != 0 {
        return None;
    }
    let SymVal::Linear(mut e) = sym_add(a, b)? else {
        return None;
    };
    e.known_zero = !(active_a | active_b);
    Some(SymVal::Linear(e))
}

/// Computes blocks reachable from entry using only group-uniform scalar branches and `all` blocks.
#[allow(clippy::too_many_arguments)]
fn compute_uniform_cfg(
    p: &Program,
    st: &Structured,
    all: &[bool],
    def_inst: &HashMap<Value, (usize, usize)>,
    reg_inputs: &HashMap<Value, InputKind>,
    waterfall_loads: &HashSet<(usize, usize)>,
    scalar: &HashMap<(usize, usize), Address>,
    writes_memory: bool,
) -> Vec<bool> {
    let n = p.blocks.len();
    let mut bad_branch = vec![false; n];
    for (b, block) in p.blocks.iter().enumerate().take(st.exits.len()) {
        if !all[b] {
            continue;
        }
        if let LaneExit::Branch {
            taken,
            not_taken,
            when,
        } = st.exits[b]
        {
            let scalar_or_wide = st.wide.contains(&b)
                || st
                    .bools
                    .support(when)
                    .iter()
                    .all(|a| matches!(a, Atom::Uniform(_)))
                || (all[taken] && all[not_taken]);
            if scalar_or_wide {
                bad_branch[b] = !is_uniform_branch(
                    p,
                    st,
                    all,
                    def_inst,
                    reg_inputs,
                    waterfall_loads,
                    scalar,
                    writes_memory,
                    block,
                );
            }
        }
    }

    // Propagate conservatively: any block downstream of a non-group-uniform scalar/wide branch
    // is marked non-uniform.
    let mut non_uniform = vec![false; n];
    let mut queue = VecDeque::new();
    for b in 0..n {
        if bad_branch[b]
            && let LaneExit::Branch {
                taken, not_taken, ..
            } = st.exits[b]
        {
            for t in [taken, not_taken] {
                if !non_uniform[t] {
                    non_uniform[t] = true;
                    queue.push_back(t);
                }
            }
        }
    }
    while let Some(b) = queue.pop_front() {
        for t in p.blocks[b].exit.successors() {
            if !non_uniform[t] {
                non_uniform[t] = true;
                queue.push_back(t);
            }
        }
    }
    all.iter().enumerate().map(|(b, &a)| a && !non_uniform[b]).collect()
}

#[allow(clippy::too_many_arguments)]
fn is_uniform_branch(
    p: &Program,
    st: &Structured,
    all: &[bool],
    def_inst: &HashMap<Value, (usize, usize)>,
    reg_inputs: &HashMap<Value, InputKind>,
    waterfall_loads: &HashSet<(usize, usize)>,
    scalar: &HashMap<(usize, usize), Address>,
    writes_memory: bool,
    block: &Block,
) -> bool {
    let Some(last) = block.insts.last() else {
        return false;
    };
    if !last.name().starts_with("s_cbranch_scc") {
        return false;
    }
    let Some(scc) = last.read(Reg::Scc) else {
        return false;
    };
    let all_true = vec![true; p.blocks.len()];
    let def_phi = HashMap::new();
    let mut seen = HashSet::new();
    is_group_uniform_val(
        p,
        st,
        all,
        &all_true,
        def_inst,
        &def_phi,
        waterfall_loads,
        scalar,
        writes_memory,
        reg_inputs,
        scc,
        &mut seen,
    )
}

#[allow(clippy::too_many_arguments)]
fn is_group_uniform_val(
    p: &Program,
    st: &Structured,
    all: &[bool],
    uniform_cfg: &[bool],
    def_inst: &HashMap<Value, (usize, usize)>,
    def_phi: &HashMap<Value, (usize, usize)>,
    waterfall_loads: &HashSet<(usize, usize)>,
    scalar: &HashMap<(usize, usize), Address>,
    writes_memory: bool,
    reg_inputs: &HashMap<Value, InputKind>,
    v: Value,
    seen: &mut HashSet<Value>,
) -> bool {
    if let Some(&kind) = reg_inputs.get(&v) {
        return kind == InputKind::GroupUniform;
    }
    if !seen.insert(v) {
        return true;
    }

    if let Some(&(b, pi)) = def_phi.get(&v) {
        if !all[b] || !uniform_cfg[b] {
            return false;
        }
        if !p.blocks[b].preds.iter().all(|&q| all[q] && uniform_cfg[q]) {
            return false;
        }
        return p.blocks[b].phis[pi].args.iter().all(|&a| {
            is_group_uniform_val(
                p,
                st,
                all,
                uniform_cfg,
                def_inst,
                def_phi,
                waterfall_loads,
                scalar,
                writes_memory,
                reg_inputs,
                a,
                seen,
            )
        });
    }

    let Some(&(b, i)) = def_inst.get(&v) else {
        return false;
    };
    if !all[b] || !uniform_cfg[b] || st.guards[b][i].is_some() || waterfall_loads.contains(&(b, i)) {
        return false;
    }
    let inst = &p.blocks[b].insts[i];
    let name = inst.name();
    let check_op = |op: &Operand, seen: &mut HashSet<Value>| -> bool {
        match *op {
            Operand::Const(c) => inline(c).is_some(),
            Operand::Literal(_) => true,
            Operand::Scalar { code, .. } if code == NULL => true,
            Operand::Scalar { code, dwords } => {
                if (106..PARKED).contains(&code) {
                    return false;
                }
                (0..dwords).all(|k| {
                    inst.read(Reg::S(code + k)).is_some_and(|rv| {
                        is_group_uniform_val(
                            p,
                            st,
                            all,
                            uniform_cfg,
                            def_inst,
                            def_phi,
                            waterfall_loads,
                            scalar,
                            writes_memory,
                            reg_inputs,
                            rv,
                            seen,
                        )
                    })
                })
            }
            Operand::Vector { reg, dwords } => (0..dwords).all(|k| {
                inst.read(Reg::V(reg + k)).is_some_and(|rv| {
                    is_group_uniform_val(
                        p,
                        st,
                        all,
                        uniform_cfg,
                        def_inst,
                        def_phi,
                        waterfall_loads,
                        scalar,
                        writes_memory,
                        reg_inputs,
                        rv,
                        seen,
                    )
                })
            }),
            Operand::Attr { .. } => false,
        }
    };

    match &inst.fields {
        Fields::Sop { src, .. } => {
            if matches!(
                name,
                "s_memtime" | "s_memrealtime" | "s_getreg_b32" | "s_getpc_b64" | "s_sendmsg"
            ) || name.contains("readfirstlane")
                || name.contains("readlane")
            {
                return false;
            }
            if src.iter().any(|op| {
                matches!(*op, Operand::Attr { .. })
                    || matches!(*op, Operand::Const(c) if inline(c).is_none() && c != 253)
            }) {
                return false;
            }
            inst.reads.iter().all(|&(r, rv)| match r {
                Reg::S(c) if (106..PARKED).contains(&c) => false,
                Reg::Scc | Reg::S(_) | Reg::V(_) => is_group_uniform_val(
                    p,
                    st,
                    all,
                    uniform_cfg,
                    def_inst,
                    def_phi,
                    waterfall_loads,
                    scalar,
                    writes_memory,
                    reg_inputs,
                    rv,
                    seen,
                ),
            })
        }
        Fields::Smem {
            base,
            soffset,
            cache,
            ..
        } => {
            let immutable_source = scalar.get(&(b, i)).is_some_and(|a| {
                a.param == CODE || (a.param != MEMORY && a.dynamic.is_none() && !writes_memory)
            });
            name.starts_with("s_load_dword")
                && !cache.glc
                && !cache.slc
                && immutable_source
                && check_op(base, seen)
                && check_op(soffset, seen)
        }
        Fields::Valu(valu) => {
            let is_primary_dst = match valu.dst {
                Some(Operand::Vector { reg, dwords }) => {
                    (0..dwords).any(|k| inst.write(Reg::V(reg + k)) == Some(v))
                }
                Some(Operand::Scalar { code, dwords }) => {
                    (0..dwords).any(|k| inst.write(Reg::S(code + k)) == Some(v))
                }
                _ => false,
            };
            let safe_valu = matches!(
                name,
                "v_mov_b32"
                    | "v_add_nc_u32"
                    | "v_add_u32"
                    | "v_add_co_u32"
                    | "v_add3_u32"
                    | "v_sub_nc_u32"
                    | "v_subrev_nc_u32"
                    | "v_sub_co_u32"
                    | "v_subrev_co_u32"
                    | "v_mul_lo_u32"
                    | "v_mul_hi_u32"
                    | "v_mul_hi_i32"
                    | "v_mul_u32_u24"
                    | "v_mul_i32_i24"
                    | "v_mad_u32_u24"
                    | "v_mad_i32_i24"
                    | "v_lshlrev_b32"
                    | "v_lshrrev_b32"
                    | "v_ashrrev_i32"
                    | "v_lshl_add_u32"
                    | "v_add_lshl_u32"
                    | "v_lshl_or_b32"
                    | "v_and_b32"
                    | "v_or_b32"
                    | "v_xor_b32"
                    | "v_and_or_b32"
                    | "v_or3_b32"
                    | "v_xor3_b32"
                    | "v_xad_u32"
                    | "v_bfe_u32"
                    | "v_bfe_i32"
                    | "v_bfi_b32"
                    | "v_bfrev_b32"
                    | "v_bcnt_u32_b32"
                    | "v_min_u32"
                    | "v_max_u32"
                    | "v_min_i32"
                    | "v_max_i32"
                    | "v_min3_u32"
                    | "v_max3_u32"
                    | "v_min3_i32"
                    | "v_max3_i32"
                    | "v_med3_u32"
                    | "v_med3_i32"
                    | "v_alignbit_b32"
                    | "v_alignbyte_b32"
            );
            if has_valu_modifiers(valu) || !safe_valu || !is_primary_dst {
                return false;
            }
            let bad_read = inst.reads.iter().any(|&(r, _)| match r {
                Reg::Scc => true,
                Reg::S(c) => (106..PARKED).contains(&c) && c != EXEC && c != EXEC + 1,
                _ => false,
            });
            !bad_read && valu.src.iter().all(|op| check_op(op, seen))
        }
        _ => false,
    }
}

fn uniform_result_tz(inst: &Inst, v: Value) -> u32 {
    let const_op = |op: &Operand| match *op {
        Operand::Const(c) => inline(c),
        Operand::Literal(x) => Some(x),
        Operand::Scalar { code, .. } if code == NULL => Some(0),
        _ => None,
    };
    match (inst.name(), &inst.fields) {
        (
            "s_and_b32",
            Fields::Sop {
                dst: Some(Operand::Scalar { code, dwords: 1 }),
                src,
                ..
            },
        ) if inst.write(Reg::S(*code)) == Some(v) => src
            .iter()
            .filter_map(const_op)
            .map(u32::trailing_zeros)
            .max()
            .unwrap_or(0),
        ("v_and_b32", Fields::Valu(valu))
            if matches!(
                valu.dst,
                Some(Operand::Vector { reg, dwords: 1 }) if inst.write(Reg::V(reg)) == Some(v)
            ) =>
        {
            valu.src
                .iter()
                .filter_map(const_op)
                .map(u32::trailing_zeros)
                .max()
                .unwrap_or(0)
        }
        (
            "s_lshl_b32",
            Fields::Sop {
                dst: Some(Operand::Scalar { code, dwords: 1 }),
                src,
                ..
            },
        ) if inst.write(Reg::S(*code)) == Some(v) => {
            src.get(1).and_then(const_op).map_or(0, |s| s & 31)
        }
        ("v_lshlrev_b32", Fields::Valu(valu))
            if matches!(
                valu.dst,
                Some(Operand::Vector { reg, dwords: 1 }) if inst.write(Reg::V(reg)) == Some(v)
            ) =>
        {
            valu.src.first().and_then(const_op).map_or(0, |s| s & 31)
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::super::lower;
    use crate::native::{
        ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
    };
    use crate::{io, ir, resource, structure};

    const S_ENDPGM: u32 = 0xBF81_0000;

    fn make_cs(body: &[u32], threads: [u32; 3], lds_blocks: u32, extra_vgprs: u16) -> NativeProgram {
        let mut words = vec![
            0xBE85_0380,                         // s_mov_b32 s5, 0
            0x3D << 26 | 0x02 << 18 | 8 << 6 | 2, // s_load_dwordx4 s[8:11], s[4:5], 0
            125 << 25,
        ];
        words.extend_from_slice(body);
        words.push(S_ENDPGM);
        let thread_id_bits = if threads[2] > 1 {
            2
        } else if threads[1] > 1 {
            1
        } else {
            0
        };
        NativeProgram {
            stage: HwStage::Cs,
            wave64: true,
            vgprs: 16.max(extra_vgprs),
            layout: vec![
                RootParam {
                    root: RootType::Constants,
                    table_offset_dwords: 0,
                    sgprs: 4,
                    base_register: 0,
                    user_data: 0,
                    slot: 0,
                },
                RootParam {
                    root: RootType::Table,
                    table_offset_dwords: 0,
                    sgprs: 1,
                    base_register: 0,
                    user_data: 4,
                    slot: 1,
                },
            ],
            used: vec![UsedResource {
                kind: ResourceType::Uav,
                root: RootType::Table,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 0,
                dwords: 4,
                user_data: 4,
                slot: 1,
            }],
            io: StageIo::Cs(ComputeIo {
                rsrc2: (5 << 1) | (thread_id_bits << 11) | (lds_blocks << 15),
                threads,
            }),
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        }
    }

    fn lowered_wave(body: &[u32], threads: [u32; 3], lds_blocks: u32) -> u32 {
        let native = make_cs(body, threads, lds_blocks, 16);
        let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
        let st = structure::structure(&p).expect("structure");
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
        let io = io::raise(&p, &native).expect("io::raise");
        let was = lower::TEST_HOST_WAVE.replace(Some(32));
        let lowered = lower::lower(&lower::Parts {
            native: &native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: Vec::new(),
            outputs: Vec::new(),
            prims: Vec::new(),
            mesh_stride: 0,
        });
        lower::TEST_HOST_WAVE.set(was);
        lowered.expect("lower").wave_size.unwrap_or(64)
    }

    fn run_meetings(body: &[u32], threads: [u32; 3], lds_blocks: u32) -> Option<Vec<(bool, bool)>> {
        run_analyze(body, threads, lds_blocks).ok()
    }

    fn run_analyze(
        body: &[u32],
        threads: [u32; 3],
        lds_blocks: u32,
    ) -> Result<Vec<(bool, bool)>, super::Refusal> {
        let native = make_cs(body, threads, lds_blocks, 16);
        let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
        let st = structure::structure(&p).expect("structure");
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
        let io = io::raise(&p, &native).expect("io::raise");
        let all = lower::all_lane_blocks(&st).expect("all_lane_blocks");
        let StageIo::Cs(cs) = native.io else {
            unreachable!();
        };
        super::analyze(&p, &st, &io, &res, cs, &all, &HashSet::new())
    }

    fn with_write_only_ok<T>(host_wave: u32, enabled: bool, f: impl FnOnce() -> T) -> T {
        let prev_wave = lower::TEST_HOST_WAVE.replace(Some(host_wave));
        let prev_flag = super::TEST_LDS_WRITE_ONLY_OK.replace(Some(enabled));
        let out = f();
        super::TEST_LDS_WRITE_ONLY_OK.set(prev_flag);
        lower::TEST_HOST_WAVE.set(prev_wave);
        out
    }

    fn wave_why_reasons(body: &[u32], threads: [u32; 3], lds_blocks: u32) -> Vec<&'static str> {
        let native = make_cs(body, threads, lds_blocks, 16);
        let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
        let st = structure::structure(&p).expect("structure");
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
        let io = io::raise(&p, &native).expect("io::raise");
        let all = lower::all_lane_blocks(&st);
        let StageIo::Cs(cs) = native.io else {
            unreachable!();
        };
        let group = cs.threads.iter().product::<u32>();
        let refusal = all
            .as_ref()
            .and_then(|a| super::analyze(&p, &st, &io, &res, cs, a, &HashSet::new()).err());
        lower::wave64_why_reasons(&p, &st, group, cs.lds_bytes() > 0, all.is_none(), refusal)
    }

    // Wrap a divergent branch around `inner_words` (`if (v0 < s0) { ... }`).
    fn if_v0_lt_s0(inner: &[u32]) -> Vec<u32> {
        let skip = inner.len() as u32; // jump past inner directly to restore EXEC
        let mut out = vec![
            0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
            0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
            0xBF88_0000 | skip,
        ];
        out.extend_from_slice(inner);
        out.push(0xBEFE_040C); // s_mov_b64 exec, s[12:13]
        out
    }

    // DS instruction encoders (GFX10 64-bit VDS encoding: top 6 bits = 0x36).
    fn ds_enc(op: u8, off0: u8, off1: u8, addr: u8, data0: u8, data1: u8, vdst: u8) -> [u32; 2] {
        let w0 = (0x36u32 << 26)
            | (u32::from(op) << 18)
            | (u32::from(off1) << 8)
            | u32::from(off0);
        let w1 = (u32::from(vdst) << 24)
            | (u32::from(data1) << 16)
            | (u32::from(data0) << 8)
            | u32::from(addr);
        [w0, w1]
    }

    fn ds_write_b32(addr: u8, data: u8, off: u16) -> [u32; 2] {
        ds_enc(0x0d, off as u8, (off >> 8) as u8, addr, data, 0, 0)
    }
    fn ds_read_b32(vdst: u8, addr: u8, off: u16) -> [u32; 2] {
        ds_enc(0x36, off as u8, (off >> 8) as u8, addr, 0, 0, vdst)
    }
    fn ds_write_b64(addr: u8, data: u8, off: u16) -> [u32; 2] {
        ds_enc(0x4d, off as u8, (off >> 8) as u8, addr, data, 0, 0)
    }
    fn ds_read_b64(vdst: u8, addr: u8, off: u16) -> [u32; 2] {
        ds_enc(0x76, off as u8, (off >> 8) as u8, addr, 0, 0, vdst)
    }
    fn ds_write_b128(addr: u8, data: u8, off: u16) -> [u32; 2] {
        ds_enc(0xdf, off as u8, (off >> 8) as u8, addr, data, 0, 0)
    }
    fn ds_read_b128(vdst: u8, addr: u8, off: u16) -> [u32; 2] {
        ds_enc(0xff, off as u8, (off >> 8) as u8, addr, 0, 0, vdst)
    }
    fn ds_write_b8(addr: u8, data: u8, off: u16) -> [u32; 2] {
        ds_enc(0x1e, off as u8, (off >> 8) as u8, addr, data, 0, 0)
    }
    fn ds_read_u8(vdst: u8, addr: u8, off: u16) -> [u32; 2] {
        ds_enc(0x3a, off as u8, (off >> 8) as u8, addr, 0, 0, vdst)
    }
    fn ds_read2_b32(vdst: u8, addr: u8, off0: u8, off1: u8) -> [u32; 2] {
        ds_enc(0x37, off0, off1, addr, 0, 0, vdst)
    }
    fn ds_write2_b32(addr: u8, data0: u8, data1: u8, off0: u8, off1: u8) -> [u32; 2] {
        ds_enc(0x0e, off0, off1, addr, data0, data1, 0)
    }
    fn ds_read2st64_b32(vdst: u8, addr: u8, off0: u8, off1: u8) -> [u32; 2] {
        ds_enc(0x38, off0, off1, addr, 0, 0, vdst)
    }
    fn ds_add_rtn_u32(vdst: u8, addr: u8, data: u8, off: u16) -> [u32; 2] {
        ds_enc(0x20, off as u8, (off >> 8) as u8, addr, data, 0, vdst)
    }
    fn ds_cmpst_rtn_b32(vdst: u8, addr: u8, cmp: u8, data: u8, off: u16) -> [u32; 2] {
        ds_enc(0x30, off as u8, (off >> 8) as u8, addr, cmp, data, vdst)
    }
    fn ds_add_u32(addr: u8, data: u8, off: u16) -> [u32; 2] {
        ds_enc(0x00, off as u8, (off >> 8) as u8, addr, data, 0, 0)
    }

    // VOP1 v_mov_b32 (op 1 -> bit 9 = 0x0200):
    fn v_mov_k(vdst: u8, k: u8) -> u32 {
        0x7E00_0200 | (u32::from(vdst) << 17) | u32::from(128 + k)
    }
    fn v_mov_lit(vdst: u8, lit: u32) -> [u32; 2] {
        [0x7E00_02FF | (u32::from(vdst) << 17), lit]
    }
    fn v_mov_s(vdst: u8, sgpr: u8) -> u32 {
        0x7E00_0200 | (u32::from(vdst) << 17) | u32::from(sgpr)
    }
    // v_lshlrev_b32 vdst, shift(const), vsrc -> 0x3400_0000 | (vdst << 17) | (vsrc << 9) | (128 + shift)
    fn v_lshlrev_k(vdst: u8, shift: u8, vsrc: u8) -> u32 {
        0x3400_0000 | (u32::from(vdst) << 17) | (u32::from(vsrc) << 9) | u32::from(128 + shift)
    }
    // v_add_nc_u32 vdst, src0_s_or_const, vsrc1 -> 0x4A00_0000 | (vdst << 17) | (vsrc1 << 9) | src0
    fn v_add_nc_sv(vdst: u8, src0: u8, vsrc1: u8) -> u32 {
        0x4A00_0000 | (u32::from(vdst) << 17) | (u32::from(vsrc1) << 9) | u32::from(src0)
    }
    fn v_add_nc_vv(vdst: u8, vsrc0: u8, vsrc1: u8) -> u32 {
        0x4A00_0000 | (u32::from(vdst) << 17) | (u32::from(vsrc1) << 9) | (256 + u32::from(vsrc0))
    }
    fn v_add_nc_lit(vdst: u8, lit: u32, vsrc1: u8) -> [u32; 2] {
        [
            0x4A00_00FF | (u32::from(vdst) << 17) | (u32::from(vsrc1) << 9),
            lit,
        ]
    }
    // v_xor_b32 vdst, src0_const, vsrc1 -> 0x3A00_0000
    fn v_xor_kv(vdst: u8, k: u8, vsrc1: u8) -> u32 {
        0x3A00_0000 | (u32::from(vdst) << 17) | (u32::from(vsrc1) << 9) | u32::from(128 + k)
    }
    // v_and_b32 vdst, src0_const, vsrc1 -> 0x3600_0000
    fn v_and_kv(vdst: u8, k: u8, vsrc1: u8) -> u32 {
        0x3600_0000 | (u32::from(vdst) << 17) | (u32::from(vsrc1) << 9) | u32::from(128 + k)
    }
    // v_or_b32 vdst, src0_const, vsrc1 -> 0x3800_0000
    fn v_or_kv(vdst: u8, k: u8, vsrc1: u8) -> u32 {
        0x3800_0000 | (u32::from(vdst) << 17) | (u32::from(vsrc1) << 9) | u32::from(128 + k)
    }
    // v_mul_u32_u24 vdst, src0_const, vsrc1 -> 0x1600_0000
    fn v_mul24_kv(vdst: u8, k: u8, vsrc1: u8) -> u32 {
        0x1600_0000 | (u32::from(vdst) << 17) | (u32::from(vsrc1) << 9) | u32::from(128 + k)
    }
    // v_mul_lo_u32 vdst, src0_lit, vsrc1 -> VOP3 op 0x169
    fn v_mul_lo_lit(vdst: u8, lit: u32, vsrc1: u8) -> [u32; 3] {
        [
            (0xD5u32 << 24) | (0x169u32 << 16) | u32::from(vdst),
            ((256 + u32::from(vsrc1)) << 9) | 0xFF,
            lit,
        ]
    }
    // v_mbcnt_lo_u32_b32 vdst, -1(193), 0(128) -> VOP3 op 0x365
    fn v_mbcnt_lo_init(vdst: u8) -> [u32; 2] {
        [
            (0xD4u32 << 24) | (0x365u32 << 16) | u32::from(vdst),
            (128 << 9) | 193,
        ]
    }
    // v_mbcnt_hi_u32_b32 vdst, -1(193), vsrc -> VOP3 op 0x366
    fn v_mbcnt_hi(vdst: u8, vsrc: u8) -> [u32; 2] {
        [
            (0xD4u32 << 24) | (0x366u32 << 16) | u32::from(vdst),
            ((256 + u32::from(vsrc)) << 9) | 193,
        ]
    }
    fn store_uav(vdata: u8) -> [u32; 2] {
        [0x38u32 << 26 | 0x1Cu32 << 18, 0x80u32 << 24 | 2u32 << 16 | (u32::from(vdata) << 8)]
    }

    #[test]
    fn disjoint_constant_offsets_recover_wave32() {
        let mut inner = vec![v_mov_k(2, 0), v_mov_k(3, 7)];
        inner.extend(ds_write_b32(2, 3, 0));
        inner.extend(ds_read_b32(4, 2, 4));
        inner.extend(store_uav(4));
        let body = if_v0_lt_s0(&inner);
        assert!(run_meetings(&body, [64, 1, 1], 1).is_some());
        assert_eq!(lowered_wave(&body, [64, 1, 1], 1), 32);
    }

    #[test]
    fn overlapping_constant_offsets_refuse_wave32() {
        let mut inner = vec![v_mov_k(2, 0), v_mov_k(3, 7)];
        inner.extend(ds_write_b64(2, 2, 0)); // [0, 8)
        inner.extend(ds_read_b32(4, 2, 4)); // [4, 8) overlaps write!
        inner.extend(store_uav(4));
        let body = if_v0_lt_s0(&inner);
        assert!(run_meetings(&body, [64, 1, 1], 1).is_none());
        assert_eq!(lowered_wave(&body, [64, 1, 1], 1), 64);
    }

    #[test]
    fn wide_b128_disjoint_and_overlap_checks() {
        let mut disjoint = vec![
            v_mov_k(2, 0),
            v_mov_k(4, 1),
            v_mov_k(5, 2),
            v_mov_k(6, 3),
            v_mov_k(7, 4),
        ];
        disjoint.extend(ds_write_b128(2, 4, 0)); // [0, 16)
        disjoint.extend(ds_read_b128(8, 2, 16)); // [16, 32)
        disjoint.extend(store_uav(8));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&disjoint), [64, 1, 1], 1), 32);

        let mut overlap = vec![
            v_mov_k(2, 0),
            v_mov_k(4, 1),
            v_mov_k(5, 2),
            v_mov_k(6, 3),
            v_mov_k(7, 4),
        ];
        overlap.extend(ds_write_b128(2, 4, 0)); // [0, 16)
        overlap.extend(ds_read_b32(8, 2, 12)); // [12, 16) overlaps [0, 16)!
        overlap.extend(store_uav(8));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&overlap), [64, 1, 1], 1), 64);
    }

    #[test]
    fn uniform_base_shared_disjoint_and_mismatch_refused() {
        // Shared root constant base s1 (4-aligned via s_and_b32 s2, s1, -4) + disjoint 0 vs 4.
        let mut body = vec![0x8702_C401]; // s_and_b32 s2, s1, -4 (SOP2 op 0x0e, inline const 196 = -4)
        let mut inner = vec![v_mov_s(2, 2), v_mov_k(3, 9)];
        inner.extend(ds_write_b32(2, 3, 0));
        inner.extend(ds_read_b32(4, 2, 4));
        inner.extend(store_uav(4));
        body.extend(if_v0_lt_s0(&inner));
        assert_eq!(lowered_wave(&body, [64, 1, 1], 1), 32);

        // Unaligned s1 with b8 at +3 and b32 at +4 CAN collide in the same dword!
        let mut mixed_inner = vec![v_mov_s(2, 1), v_mov_k(3, 9)];
        mixed_inner.extend(ds_write_b8(2, 3, 3));
        mixed_inner.extend(ds_read_b32(4, 2, 4));
        mixed_inner.extend(store_uav(4));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&mixed_inner), [64, 1, 1], 1), 64);

        // Two different unknown uniform bases s1 vs s2 -> refused.
        let mut diff_inner = vec![v_mov_s(2, 1), v_mov_s(3, 2), v_mov_k(4, 5)];
        diff_inner.extend(ds_write_b32(2, 4, 0));
        diff_inner.extend(ds_read_b32(5, 3, 4));
        diff_inner.extend(store_uav(5));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&diff_inner), [64, 1, 1], 1), 64);
    }

    #[test]
    fn integer_wrap_tricks_are_refused() {
        // Constant address 0xFFFF_FFFC + offset 4 wraps u32 to 0 -> must refuse.
        let mut inner = vec![v_mov_k(2, 0), v_mov_k(3, 5)];
        inner.extend(v_mov_lit(4, 0xFFFF_FFFC));
        inner.extend(ds_write_b32(2, 3, 0));
        inner.extend(ds_read_b32(5, 4, 4));
        inner.extend(store_uav(5));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&inner), [64, 1, 1], 1), 64);

        // Uniform base s1 vs s1 + 0xFFFF_FFFC (+ 4) -> must refuse.
        let mut wrap_u = vec![v_mov_s(2, 1), v_mov_k(3, 5)];
        wrap_u.extend(v_add_nc_lit(4, 0xFFFF_FFFC, 2));
        wrap_u.extend(ds_write_b32(2, 3, 0));
        wrap_u.extend(ds_read_b32(5, 4, 4));
        wrap_u.extend(store_uav(5));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&wrap_u), [64, 1, 1], 1), 64);

        // Huge lane stride 0x0400_0000 * 64 wraps u32 -> must refuse.
        let mut huge_stride = Vec::new();
        huge_stride.extend(v_mul_lo_lit(2, 0x0400_0000, 0));
        huge_stride.push(v_mov_k(3, 5));
        huge_stride.extend(ds_write_b32(2, 3, 0));
        huge_stride.extend(ds_read_b32(4, 2, 0));
        huge_stride.extend(store_uav(4));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&huge_stride), [64, 1, 1], 1), 64);
    }

    #[test]
    fn dual_offset_and_st64_disjoint_vs_overlap() {
        // ds_write2_b32 off0=0 ([0,4)), off1=1 ([4,8)) and ds_read2_b32 off0=2 ([8,12)), off1=3 ([12,16))
        let mut disjoint = vec![v_mov_k(2, 0), v_mov_k(3, 1), v_mov_k(4, 2)];
        disjoint.extend(ds_write2_b32(2, 3, 4, 0, 1));
        disjoint.extend(ds_read2_b32(6, 2, 2, 3));
        disjoint.extend(store_uav(6));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&disjoint), [64, 1, 1], 1), 32);

        // ds_write2_b32 [0,4) + [8,12) vs ds_read2st64_b32 [0,4) + [256,260) -> overlaps at [0,4)!
        let mut overlap_st64 = vec![v_mov_k(2, 0), v_mov_k(3, 1), v_mov_k(4, 2)];
        overlap_st64.extend(ds_write2_b32(2, 3, 4, 0, 2));
        overlap_st64.extend(ds_read2st64_b32(6, 2, 0, 1));
        overlap_st64.extend(store_uav(6));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&overlap_st64), [64, 1, 1], 2), 64);
    }

    #[test]
    fn atomics_are_rmw_and_require_lane_privacy() {
        // Returning atomic at shared address 0 -> self-RMW conflict -> MUST refuse.
        let mut shared_rtn = vec![v_mov_k(2, 0), v_mov_k(3, 1)];
        shared_rtn.extend(ds_add_rtn_u32(4, 2, 3, 0));
        shared_rtn.extend(store_uav(4));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&shared_rtn), [64, 1, 1], 1), 64);

        // Non-returning atomic ds_add_u32 + ds_write_b32 at shared address -> RMW -> MUST refuse.
        let mut shared_add = vec![v_mov_k(2, 0), v_mov_k(3, 1)];
        shared_add.extend(ds_add_u32(2, 3, 0));
        shared_add.extend(store_uav(3));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&shared_add), [64, 1, 1], 1), 64);

        // ds_cmpst_rtn_b32 at shared address 0 -> MUST refuse.
        let mut shared_cmp = vec![v_mov_k(2, 0), v_mov_k(3, 1), v_mov_k(4, 2)];
        shared_cmp.extend(ds_cmpst_rtn_b32(5, 2, 3, 4, 0));
        shared_cmp.extend(store_uav(5));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&shared_cmp), [64, 1, 1], 1), 64);

        // ds_add_rtn_u32 inside thread's private slot (tid * 4) -> ACCEPTED!
        let mut priv_rtn = vec![v_lshlrev_k(2, 2, 0), v_mov_k(3, 1)];
        priv_rtn.extend(ds_add_rtn_u32(4, 2, 3, 0));
        priv_rtn.extend(store_uav(4));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&priv_rtn), [64, 1, 1], 1), 32);
    }

    #[test]
    fn lane_private_slots_bounds_masks_and_2d() {
        // Valid 8-byte per-thread slot: tid * 8, writes [0..4), reads [4..8) via |4 and +4.
        let mut valid_slot = vec![v_lshlrev_k(2, 3, 0), v_mov_k(3, 7)];
        valid_slot.push(v_or_kv(4, 4, 2)); // (tid << 3) | 4 == tid * 8 + 4
        valid_slot.extend(ds_write_b32(2, 3, 0));
        valid_slot.extend(ds_write_b32(4, 3, 0));
        valid_slot.extend(ds_read_b32(5, 2, 4));
        valid_slot.extend(store_uav(5));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&valid_slot), [64, 1, 1], 2), 32);

        // Stride < width: tid * 4 with b64 (width 8 > stride 4) -> MUST refuse.
        let mut narrow_stride = vec![v_lshlrev_k(2, 2, 0), v_mov_k(4, 1), v_mov_k(5, 2)];
        narrow_stride.extend(ds_write_b64(2, 4, 0));
        narrow_stride.extend(ds_read_b64(6, 2, 0));
        narrow_stride.extend(store_uav(6));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&narrow_stride), [64, 1, 1], 2), 64);

        // Cross-lane XOR mask (tid ^ 1) * 4 paired with tid * 4 -> MUST refuse.
        let mut xor_lane = vec![v_lshlrev_k(2, 2, 0), v_xor_kv(3, 1, 0), v_lshlrev_k(3, 2, 3), v_mov_k(4, 9)];
        xor_lane.extend(ds_write_b32(2, 4, 0));
        xor_lane.extend(ds_read_b32(5, 3, 0));
        xor_lane.extend(store_uav(5));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&xor_lane), [64, 1, 1], 1), 64);

        // Cross-lane neighbor read (tid + 1) * 4 with stride 4 -> MUST refuse.
        let mut neighbor = vec![v_lshlrev_k(2, 2, 0), v_mov_k(3, 9)];
        neighbor.extend(ds_write_b32(2, 3, 0));
        neighbor.extend(ds_read_b32(4, 2, 4));
        neighbor.extend(store_uav(4));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&neighbor), [64, 1, 1], 1), 64);

        // Lane alias tid & 7 when group=64 -> MUST refuse.
        let mut masked_tid = vec![v_and_kv(2, 7, 0), v_lshlrev_k(2, 2, 2), v_mov_k(3, 9)];
        masked_tid.extend(ds_write_b32(2, 3, 0));
        masked_tid.extend(ds_read_b32(4, 2, 0));
        masked_tid.extend(store_uav(4));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&masked_tid), [64, 1, 1], 1), 64);

        // 2D threadgroup [8, 8, 1]: valid (tid_y * 8 + tid_x) * 4 -> ACCEPTED;
        // invalid tid_x * 4 (ignores tid_y!) -> REFUSED.
        let mut two_d_ok = vec![
            v_lshlrev_k(2, 3, 1), // v2 = tid_y * 8
            v_add_nc_vv(2, 0, 2), // v2 = tid_x + tid_y * 8
            v_lshlrev_k(2, 2, 2), // v2 *= 4
            v_mov_k(3, 5),
        ];
        two_d_ok.extend(ds_write_b32(2, 3, 0));
        two_d_ok.extend(ds_read_b32(4, 2, 0));
        two_d_ok.extend(store_uav(4));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&two_d_ok), [8, 8, 1], 1), 32);

        let mut two_d_bad = vec![v_lshlrev_k(2, 2, 0), v_mov_k(3, 5)];
        two_d_bad.extend(ds_write_b32(2, 3, 0));
        two_d_bad.extend(ds_read_b32(4, 2, 0));
        two_d_bad.extend(store_uav(4));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&two_d_bad), [8, 8, 1], 1), 64);
    }

    #[test]
    fn mbcnt_half_only_is_refused_full_pair_is_lane_private() {
        // Half mbcnt only (`v_mbcnt_lo(-1, 0)`): lanes 32..63 all read 32 -> MUST refuse!
        let mut half = Vec::new();
        half.extend(v_mbcnt_lo_init(2));
        half.push(v_lshlrev_k(2, 2, 2));
        half.push(v_mov_k(3, 1));
        half.extend(ds_write_b32(2, 3, 0));
        half.extend(ds_read_b32(4, 2, 0));
        half.extend(store_uav(4));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&half), [64, 1, 1], 1), 64);

        // Full console_lane (`lo(-1, 0)` + `hi(-1, lo)`): group=64 -> ACCEPTED as Wave32!
        let mut full = Vec::new();
        full.extend(v_mbcnt_lo_init(2));
        full.extend(v_mbcnt_hi(2, 2));
        full.push(v_lshlrev_k(2, 2, 2));
        full.push(v_mov_k(3, 1));
        full.extend(ds_write_b32(2, 3, 0));
        full.extend(ds_read_b32(4, 2, 0));
        full.extend(store_uav(4));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&full), [64, 1, 1], 1), 32);

        // Full console_lane on group=128 (two waves share lanes 0..63!) -> MUST refuse!
        assert_eq!(lowered_wave(&if_v0_lt_s0(&full), [128, 1, 1], 1), 64);
    }

    #[test]
    fn waw_and_byte_access_and_u24_checks() {
        // Sub-dword ds_write_b8 + ds_read_u8 per thread with stride 1 -> ACCEPTED.
        let mut b8_priv = vec![v_mov_k(3, 7)];
        b8_priv.extend(ds_write_b8(0, 3, 0));
        b8_priv.extend(ds_read_u8(4, 0, 0));
        b8_priv.extend(store_uav(4));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&b8_priv), [64, 1, 1], 1), 32);

        // WAW on same constant offset 0 without reads -> refused.
        let mut waw = vec![v_mov_k(2, 0), v_mov_k(3, 1), v_mov_k(4, 2)];
        waw.extend(ds_write_b32(2, 3, 0));
        waw.extend(ds_write_b32(2, 4, 0));
        waw.extend(store_uav(3));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&waw), [64, 1, 1], 1), 64);

        // Dual write ds_write2_b32 hitting the exact same offset (off0 == off1 == 0) -> refused.
        let mut self_waw = vec![v_mov_k(2, 0), v_mov_k(3, 1), v_mov_k(4, 2)];
        self_waw.extend(ds_write2_b32(2, 3, 4, 0, 0));
        self_waw.extend(ds_read_b32(5, 2, 4));
        self_waw.extend(store_uav(5));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&self_waw), [64, 1, 1], 1), 64);

        // v_mul_u32_u24(tid, 4) -> ACCEPTED.
        let mut u24 = vec![v_mul24_kv(2, 4, 0), v_mov_k(3, 5)];
        u24.extend(ds_write_b32(2, 3, 0));
        u24.extend(ds_read_b32(4, 2, 0));
        u24.extend(store_uav(4));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&u24), [64, 1, 1], 1), 32);

        // Overlapping stride-1 `ds_write_b32` at `s1 + tid` is refused by default and
        // allowed only under opt-in `VICTORY_LDS_WRITE_ONLY_OK=1` on a 32-lane host.
        let mut write_only = vec![v_add_nc_sv(2, 1, 0), v_mov_k(3, 5)]; // s1 + tid (stride 1, b32)
        write_only.extend(ds_write_b32(2, 3, 0));
        write_only.extend(store_uav(3));
        let write_only_body = if_v0_lt_s0(&write_only);
        with_write_only_ok(32, false, || {
            assert_eq!(lowered_wave(&write_only_body, [64, 1, 1], 1), 64);
        });
        with_write_only_ok(32, true, || {
            assert_eq!(lowered_wave(&write_only_body, [64, 1, 1], 1), 32);
        });
    }

    #[test]
    fn multi_cluster_mixed_width_loop_phi_and_unknown_ds_refused() {
        // Two disjoint lane-private clusters (0 + tid*4 and 512 + tid*4) -> ACCEPTED (32).
        let mut disjoint_clusters = vec![v_lshlrev_k(2, 2, 0), v_mov_k(3, 5)];
        disjoint_clusters.extend(v_add_nc_lit(4, 512, 2));
        disjoint_clusters.extend(ds_write_b32(2, 3, 0));
        disjoint_clusters.extend(ds_read_b32(5, 2, 0));
        disjoint_clusters.extend(ds_write_b32(4, 5, 0));
        disjoint_clusters.extend(ds_read_b32(6, 4, 0));
        disjoint_clusters.extend(store_uav(6));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&disjoint_clusters), [64, 1, 1], 2), 32);

        // Overlapping clusters (0 + tid*4 = [0..256) and 128 + tid*4 = [128..384)) -> REFUSED (64).
        let mut overlap_clusters = vec![v_lshlrev_k(2, 2, 0), v_mov_k(3, 5)];
        overlap_clusters.extend(v_add_nc_lit(4, 128, 2));
        overlap_clusters.extend(ds_write_b32(2, 3, 0));
        overlap_clusters.extend(ds_read_b32(5, 4, 0));
        overlap_clusters.extend(store_uav(5));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&overlap_clusters), [64, 1, 1], 1), 64);

        // Mixed b32 + b8 in stride-8 private slot (tid*8+0 and tid*8+4) -> ACCEPTED (32);
        // same in stride-6 unaligned slot -> REFUSED (64).
        let mut mixed8 = vec![v_lshlrev_k(2, 3, 0), v_mov_k(3, 5)];
        mixed8.extend(ds_write_b32(2, 3, 0));
        mixed8.extend(ds_write_b8(2, 3, 4));
        mixed8.extend(ds_read_u8(4, 2, 4));
        mixed8.extend(store_uav(4));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&mixed8), [64, 1, 1], 2), 32);

        let mut mixed6 = vec![v_mul24_kv(2, 6, 0), v_mov_k(3, 5)];
        mixed6.extend(ds_write_b32(2, 3, 0));
        mixed6.extend(ds_write_b8(2, 3, 4));
        mixed6.extend(ds_read_u8(4, 2, 4));
        mixed6.extend(store_uav(4));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&mixed6), [64, 1, 1], 1), 64);

        // Returning ds_wrxchg_rtn_b32 at shared 0 -> REFUSED; at tid*4 -> ACCEPTED.
        let mut xchg_shared = vec![v_mov_k(2, 0), v_mov_k(3, 1)];
        xchg_shared.extend(ds_enc(0x2d, 0, 0, 2, 3, 0, 4)); // ds_wrxchg_rtn_b32
        xchg_shared.extend(store_uav(4));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&xchg_shared), [64, 1, 1], 1), 64);

        let mut xchg_priv = vec![v_lshlrev_k(2, 2, 0), v_mov_k(3, 1)];
        xchg_priv.extend(ds_enc(0x2d, 0, 0, 2, 3, 0, 4));
        xchg_priv.extend(store_uav(4));
        assert_eq!(lowered_wave(&if_v0_lt_s0(&xchg_priv), [64, 1, 1], 1), 32);

        // Dual returning ds_wrxchg2_rtn_b32 classification via run_meetings.
        let mut xchg2_shared = vec![v_mov_k(2, 0), v_mov_k(3, 1)];
        xchg2_shared.extend(ds_enc(0x2e, 0, 1, 2, 3, 3, 4));
        xchg2_shared.extend(store_uav(4));
        assert!(run_meetings(&if_v0_lt_s0(&xchg2_shared), [64, 1, 1], 1).is_none());

        let mut xchg2_priv = vec![v_lshlrev_k(2, 3, 0), v_mov_k(3, 1)];
        xchg2_priv.extend(ds_enc(0x2e, 0, 1, 2, 3, 3, 4));
        xchg2_priv.extend(store_uav(4));
        assert!(run_meetings(&if_v0_lt_s0(&xchg2_priv), [64, 1, 1], 2).is_some());

        // Divergent loop Phi inside `!all`: addr = phi(v2) incremented per loop step -> REFUSED.
        // block 1 loop: v2 += 4; ds_write_b32(v2, v3); ds_read_b32(v4, v2); v_cmp_gt vcc, s0, v2; s_cbranch_vccnz -8
        let mut loop_body = vec![
            v_mov_k(2, 0),
            v_mov_k(3, 7),
            0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
            0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
            0xBF88_0007, // s_cbranch_execz -> restore
            // loop header (6 dwords before branch back):
            v_add_nc_sv(2, 132, 2), // v2 += 4
            ds_write_b32(2, 3, 0)[0],
            ds_write_b32(2, 3, 0)[1],
            ds_read_b32(4, 2, 0)[0],
            ds_read_b32(4, 2, 0)[1],
            0x7D88_0401, // v_cmp_gt_u32 vcc, s1, v2
            0xBF87_FFF8, // s_cbranch_vccnz loop (-8 words = back to v_add_nc_sv)
            0xBEFE_040C, // s_mov_b64 exec, s[12:13]
        ];
        loop_body.extend(store_uav(4));
        assert_eq!(lowered_wave(&loop_body, [64, 1, 1], 1), 64);

        // ds_nop inside `!all` alongside LDS write/read -> unhandled DS op -> REFUSED (`None`).
        let mut nop_body = vec![v_mov_k(2, 0), v_mov_k(3, 7)];
        nop_body.extend(ds_enc(0x14, 0, 0, 0, 0, 0, 0)); // ds_nop
        nop_body.extend(ds_write_b32(2, 3, 0));
        nop_body.extend(store_uav(3));
        assert!(run_meetings(&if_v0_lt_s0(&nop_body), [64, 1, 1], 1).is_none());
    }

    #[test]
    fn non_uniform_sop_cycles_smem_and_shift_overflow_refused() {
        // 1a. s_addc_u32 reading non-uniform SCC from v_readfirstlane -> MUST refuse (`64`).
        // VOP1 op 2: v_readfirstlane_b32 s2, v0 -> 0x7E04_0500.
        // s_cmp_eq_u32 s2, 0 -> SOPC op 0: 0xBF00_8002; s_addc_u32 s3, 0, 0 -> SOP2 op 4: 0x8203_8080;
        // s_lshl_b32 s3, s3, 2 -> SOP2 op 0x1e: 0x8F03_8203.
        let mut addc_body = vec![
            0x7E04_0500, // v_readfirstlane_b32 s2, v0
            0xBF00_8002, // s_cmp_eq_u32 s2, 0
            0x8203_8080, // s_addc_u32 s3, 0, 0
            0x8F03_8203, // s_lshl_b32 s3, s3, 2
        ];
        let mut addc_inner = vec![v_mov_s(2, 3), v_mov_k(3, 7)];
        addc_inner.extend(ds_write_b32(2, 3, 0));
        addc_inner.extend(ds_read_b32(4, 2, 4));
        addc_inner.extend(store_uav(4));
        addc_body.extend(if_v0_lt_s0(&addc_inner));
        assert!(run_meetings(&addc_body, [64, 1, 1], 1).is_none());
        assert_eq!(lowered_wave(&addc_body, [64, 1, 1], 1), 64);

        // 1b. s_bitset0_b32 mutating a non-uniform SGPR (reads old dst) -> MUST refuse (`64`).
        // SOP1 op 0x1b (s_bitset0_b32): 0xBE80_0000 | (sdst<<16) | (0x1b<<8) | src0 = 0xBE82_1B80 (s2, 0)
        let mut bitset_body = vec![
            0x7E04_0500, // v_readfirstlane_b32 s2, v0
            0xBE82_1B80, // s_bitset0_b32 s2, 0
        ];
        let mut bitset_inner = vec![v_mov_s(2, 2), v_mov_k(3, 7)];
        bitset_inner.extend(ds_write_b32(2, 3, 0));
        bitset_inner.extend(ds_read_b32(4, 2, 4));
        bitset_inner.extend(store_uav(4));
        bitset_body.extend(if_v0_lt_s0(&bitset_inner));
        assert!(run_meetings(&bitset_body, [64, 1, 1], 1).is_none());
        assert_eq!(lowered_wave(&bitset_body, [64, 1, 1], 1), 64);

        // 1c. s_and_saveexec_b64 reading EXEC in `all` block -> old EXEC s2 MUST NOT be treated uniform!
        let mut saveexec_body = vec![
            0xBE82_2480, // s_and_saveexec_b64 s[2:3], 0
            0xBEFE_0402, // s_mov_b64 exec, s[2:3]
        ];
        let mut saveexec_inner = vec![v_mov_s(2, 2), v_mov_k(3, 7)];
        saveexec_inner.extend(ds_write_b32(2, 3, 0));
        saveexec_inner.extend(ds_read_b32(4, 2, 4));
        saveexec_inner.extend(store_uav(4));
        saveexec_body.extend(if_v0_lt_s0(&saveexec_inner));
        assert!(run_meetings(&saveexec_body, [64, 1, 1], 1).is_none());

        // 2. Non-uniform s_cbranch_scc0 feeding a scalar Phi -> MUST refuse (`64`).
        let mut phi_cfg_body = vec![
            0x7E04_0500, // v_readfirstlane_b32 s2, v0
            0xBF00_8002, // s_cmp_eq_u32 s2, 0
            0xBF84_0002, // s_cbranch_scc0 else_blk
            0xBE83_0380, // s_mov_b32 s3, 0
            0xBF82_0001, // s_branch join
            0xBE83_0384, // else_blk: s_mov_b32 s3, 4
        ];
        let mut phi_inner = vec![v_mov_s(2, 3), v_mov_k(3, 7)];
        phi_inner.extend(ds_write_b32(2, 3, 0));
        phi_inner.extend(ds_read_b32(4, 2, 4));
        phi_inner.extend(store_uav(4));
        phi_cfg_body.extend(if_v0_lt_s0(&phi_inner));
        assert!(run_meetings(&phi_cfg_body, [64, 1, 1], 1).is_none());

        // 3a. !all loop with constant disjoint [0, 4) write + [4, 8) read (Refinement A) -> MUST refuse (`64`)
        // because cross-iteration WAW on byte 0 has no in-loop barrier!
        let mut loop_ref_a = vec![
            v_mov_k(2, 0),
            v_mov_k(3, 7),
            0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
            0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
            0xBF88_0007, // s_cbranch_execz -> restore
            // loop header (6 dwords before s_cbranch_vccnz back to v_add_nc_sv):
            v_add_nc_sv(3, 129, 3), // v3 += 1
            ds_write_b32(2, 3, 0)[0],
            ds_write_b32(2, 3, 0)[1],
            ds_read_b32(4, 2, 4)[0],
            ds_read_b32(4, 2, 4)[1],
            0x7D88_0401, // v_cmp_gt_u32 vcc, s1, v3
            0xBF87_FFF8, // s_cbranch_vccnz loop
            0xBEFE_040C, // s_mov_b64 exec, s[12:13]
        ];
        loop_ref_a.extend(store_uav(4));
        assert!(run_meetings(&loop_ref_a, [64, 1, 1], 1).is_none());
        assert_eq!(lowered_wave(&loop_ref_a, [64, 1, 1], 1), 64);

        // 3b. !all loop with Refinement B lane-private tid * 4 write+read -> ACCEPTED (`32`).
        let mut loop_ref_b = vec![
            v_lshlrev_k(2, 2, 0),
            v_mov_k(3, 7),
            0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
            0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
            0xBF88_0007, // s_cbranch_execz -> restore
            v_add_nc_sv(3, 129, 3), // v3 += 1
            ds_write_b32(2, 3, 0)[0],
            ds_write_b32(2, 3, 0)[1],
            ds_read_b32(4, 2, 0)[0],
            ds_read_b32(4, 2, 0)[1],
            0x7D88_0401,
            0xBF87_FFF8,
            0xBEFE_040C,
        ];
        loop_ref_b.extend(store_uav(4));
        assert!(run_meetings(&loop_ref_b, [64, 1, 1], 1).is_some());
        assert_eq!(lowered_wave(&loop_ref_b, [64, 1, 1], 1), 32);

        // 4. Fields::Smem mutable global MEMORY load (s_load_dword s2, s[0:1], 0) + UAV write -> MUST refuse.
        let mut smem_mut = vec![
            0x3D << 26 | 2 << 6, // s_load_dword s2, s[0:1], 0 (s[0:1] = Constants root param -> MEMORY)
            125 << 25,
        ];
        let mut smem_inner = vec![v_mov_s(2, 2), v_mov_k(3, 7)];
        smem_inner.extend(ds_write_b32(2, 3, 0));
        smem_inner.extend(ds_read_b32(4, 2, 4));
        smem_inner.extend(store_uav(4));
        smem_mut.extend(if_v0_lt_s0(&smem_inner));
        assert!(run_meetings(&smem_mut, [64, 1, 1], 1).is_none());

        // 5. sym_shl overflow: (s1 << 16) << 16 wraps coeff to 0, and (tid << 16) << 16 wraps tid to 0 -> REFUSED.
        let mut shl_wrap_u = vec![
            v_lshlrev_k(2, 16, 0),
            v_lshlrev_k(2, 16, 2), // v2 == (tid << 16) << 16 == 0!
            v_mov_k(3, 7),
        ];
        shl_wrap_u.extend(ds_write_b32(2, 3, 0));
        shl_wrap_u.extend(ds_read_b32(4, 2, 0));
        shl_wrap_u.extend(store_uav(4));
        assert!(run_meetings(&if_v0_lt_s0(&shl_wrap_u), [64, 1, 1], 1).is_none());
        assert_eq!(lowered_wave(&if_v0_lt_s0(&shl_wrap_u), [64, 1, 1], 1), 64);

        // Uniform base double shift inside !all: v2 = (s1 << 16) << 16 (wraps coeff to 0 -> looks like 0!)
        // paired with v5 = 0 -> MUST refuse!
        let mut shl_wrap_coeff = vec![
            0x7E04_0201, // v_mov_b32 v2, s1
            v_lshlrev_k(2, 16, 2),
            v_lshlrev_k(2, 16, 2),
            v_mov_k(5, 0),
            v_mov_k(3, 7),
        ];
        shl_wrap_coeff.extend(ds_write_b32(5, 3, 0));
        shl_wrap_coeff.extend(ds_read_b32(4, 2, 0));
        shl_wrap_coeff.extend(store_uav(4));
        assert!(run_meetings(&if_v0_lt_s0(&shl_wrap_coeff), [64, 1, 1], 1).is_none());
        assert_eq!(lowered_wave(&if_v0_lt_s0(&shl_wrap_coeff), [64, 1, 1], 1), 64);
    }

    #[test]
    fn valu_carry_scratch_sgpr_divergent_barrier_and_special_const_refused() {
        // VOP3B v_add_co_u32 vdst, s[sdst:sdst+1], src0, src1 (opcode 0x30f).
        let v_add_co_u32 = |vdst: u8, sdst: u8, src0: u16, src1: u16| -> [u32; 2] {
            [
                (0xD4u32 << 24) | (0x30Fu32 << 16) | (u32::from(sdst) << 8) | u32::from(vdst),
                (u32::from(src1) << 9) | u32::from(src0),
            ]
        };

        // 1a. v_add_co_u32 carry-out SGPR (`s2`) MUST NOT fold as `tid * 4 + 4` (v.dst).
        let mut carry_priv = vec![v_lshlrev_k(1, 2, 0)]; // v1 = tid * 4
        carry_priv.extend(v_add_co_u32(2, 2, 257, 132)); // v2 = v1 + 4, s[2:3] = carry
        carry_priv.push(v_mov_s(3, 2)); // v3 = carry lo (0, NOT lane-private!)
        carry_priv.push(v_mov_k(4, 7));
        carry_priv.extend(ds_write_b32(3, 4, 0));
        carry_priv.extend(ds_read_b32(5, 3, 0));
        carry_priv.extend(store_uav(5));
        assert!(run_meetings(&if_v0_lt_s0(&carry_priv), [64, 1, 1], 1).is_none());
        assert_eq!(lowered_wave(&if_v0_lt_s0(&carry_priv), [64, 1, 1], 1), 64);

        // 1b. v_add_co_u32 carry-out SGPR (`s2`) from uniform inputs MUST NOT be treated uniform.
        let mut carry_unif = Vec::new();
        carry_unif.extend(v_add_co_u32(2, 2, 1, 132)); // v2 = s1 + 4, s[2:3] = carry mask
        let mut carry_unif_inner = vec![v_mov_s(3, 2), v_mov_k(4, 7)];
        carry_unif_inner.extend(ds_write_b32(3, 4, 0));
        carry_unif_inner.extend(ds_read_b32(5, 3, 4));
        carry_unif_inner.extend(store_uav(5));
        carry_unif.extend(if_v0_lt_s0(&carry_unif_inner));
        assert!(run_meetings(&carry_unif, [64, 1, 1], 1).is_none());

        // 1c. Primary destination `v2` of v_add_co_u32 (`tid * 4 + 4`) remains lane-private (`32`).
        let mut primary_dst = vec![v_lshlrev_k(1, 2, 0)];
        primary_dst.extend(v_add_co_u32(2, 2, 257, 132));
        primary_dst.push(v_mov_k(4, 7));
        primary_dst.extend(ds_write_b32(2, 4, 0));
        primary_dst.extend(ds_read_b32(5, 2, 0));
        primary_dst.extend(store_uav(5));
        assert!(run_meetings(&if_v0_lt_s0(&primary_dst), [64, 1, 1], 1).is_some());
        assert_eq!(lowered_wave(&if_v0_lt_s0(&primary_dst), [64, 1, 1], 1), 32);

        // 2a. Per-wave scratch_wave_offset SGPR (`rsrc2 & 1 != 0`) MUST NOT be treated as
        // group-uniform even when aligned with -4. With 5 user SGPRs and TGID_X enabled (bit 7),
        // TGID_X is s5 and scratch_wave_offset sits at s6 (untouched by the make_cs prologue).
        // Contrast against TGID_Y (bit 8) at the exact same register s6, which IS group-uniform.
        let mut scratch_body_s6 = vec![0x8707_C406]; // s_and_b32 s7, s6, -4
        let mut scratch_inner_s6 = vec![v_mov_s(2, 7), v_mov_k(3, 9)];
        scratch_inner_s6.extend(ds_write_b32(2, 3, 0));
        scratch_inner_s6.extend(ds_read_b32(4, 2, 4));
        scratch_inner_s6.extend(store_uav(4));
        scratch_body_s6.extend(if_v0_lt_s0(&scratch_inner_s6));
        let mut scratch_native = make_cs(&scratch_body_s6, [64, 1, 1], 1, 16);
        if let StageIo::Cs(cs) = &mut scratch_native.io {
            cs.rsrc2 |= (1 << 7) | 1; // TGID_X in s5, scratch_wave_offset in s6
        }
        {
            let p = ir::build(&scratch_native.code, scratch_native.wave64, scratch_native.vgprs)
                .expect("ir::build");
            let st = structure::structure(&p).expect("structure");
            let res = resource::track(&p, &scratch_native.layout, scratch_native.stage, Some(&st))
                .expect("track");
            let io = io::raise(&p, &scratch_native).expect("io::raise");
            let all = lower::all_lane_blocks(&st).expect("all_lane_blocks");
            let StageIo::Cs(cs) = scratch_native.io else {
                unreachable!();
            };
            assert_eq!(cs.initialized_sgprs(), 7);
            assert!(super::meetings(&p, &st, &io, &res, cs, &all).is_none());
        }
        let mut tgid_native = make_cs(&scratch_body_s6, [64, 1, 1], 1, 16);
        if let StageIo::Cs(cs) = &mut tgid_native.io {
            cs.rsrc2 |= (1 << 7) | (1 << 8); // TGID_X in s5, TGID_Y in s6 (group-uniform)
        }
        {
            let p = ir::build(&tgid_native.code, tgid_native.wave64, tgid_native.vgprs)
                .expect("ir::build");
            let st = structure::structure(&p).expect("structure");
            let res = resource::track(&p, &tgid_native.layout, tgid_native.stage, Some(&st))
                .expect("track");
            let io = io::raise(&p, &tgid_native).expect("io::raise");
            let all = lower::all_lane_blocks(&st).expect("all_lane_blocks");
            let StageIo::Cs(cs) = tgid_native.io else {
                unreachable!();
            };
            assert!(super::meetings(&p, &st, &io, &res, cs, &all).is_some());
        }

        // 2b. Uninitialized SGPR `s14` (>= user_sgprs) MUST NOT be treated as group-uniform.
        let mut uninit_body = vec![0x870F_C40E]; // s_and_b32 s15, s14, -4
        let mut uninit_inner = vec![v_mov_s(2, 15), v_mov_k(3, 9)];
        uninit_inner.extend(ds_write_b32(2, 3, 0));
        uninit_inner.extend(ds_read_b32(4, 2, 4));
        uninit_inner.extend(store_uav(4));
        uninit_body.extend(if_v0_lt_s0(&uninit_inner));
        assert!(run_meetings(&uninit_body, [64, 1, 1], 1).is_none());

        // 3a. s_barrier inside `!all` with otherwise-disjoint Refinement A LDS -> MUST refuse.
        let mut bar_disjoint = vec![v_mov_k(2, 0), v_mov_k(3, 7)];
        bar_disjoint.extend(ds_write_b32(2, 3, 0));
        bar_disjoint.push(0xBF8A_0000); // s_barrier
        bar_disjoint.extend(ds_read_b32(4, 2, 4));
        bar_disjoint.extend(store_uav(4));
        assert!(run_meetings(&if_v0_lt_s0(&bar_disjoint), [64, 1, 1], 1).is_none());
        assert_eq!(lowered_wave(&if_v0_lt_s0(&bar_disjoint), [64, 1, 1], 1), 64);

        // 3b. s_barrier inside `!all` with otherwise-lane-private Refinement B LDS -> MUST refuse.
        let mut bar_priv = vec![v_lshlrev_k(2, 2, 0), v_mov_k(3, 7)];
        bar_priv.extend(ds_write_b32(2, 3, 0));
        bar_priv.push(0xBF8A_0000); // s_barrier
        bar_priv.extend(ds_read_b32(4, 2, 0));
        bar_priv.extend(store_uav(4));
        assert!(run_meetings(&if_v0_lt_s0(&bar_priv), [64, 1, 1], 1).is_none());
        assert_eq!(lowered_wave(&if_v0_lt_s0(&bar_priv), [64, 1, 1], 1), 64);

        // 4. Special non-inline SOP const (239 = POPS_EXITING_WAVE_ID) MUST NOT be group-uniform.
        let mut pops_body = vec![
            0xBE82_03EF, // s_mov_b32 s2, 239 (POPS_EXITING_WAVE_ID)
            0x8702_C402, // s_and_b32 s2, s2, -4
        ];
        let mut pops_inner = vec![v_mov_s(2, 2), v_mov_k(3, 9)];
        pops_inner.extend(ds_write_b32(2, 3, 0));
        pops_inner.extend(ds_read_b32(4, 2, 4));
        pops_inner.extend(store_uav(4));
        pops_body.extend(if_v0_lt_s0(&pops_inner));
        assert!(run_meetings(&pops_body, [64, 1, 1], 1).is_none());
    }

    #[test]
    fn write_only_ok_flag_handles_atomics_without_plain_read_and_four_kernels() {
        // 1. Shared-address LDS atomic with NO plain ds_read* in the `!all` region:
        //    default refuses (64); opt-in VICTORY_LDS_WRITE_ONLY_OK=1 on 32-lane host accepts (32);
        //    non-32-lane host keeps refusing even with the opt-in flag set.
        let mut shared_add = vec![v_mov_k(2, 0), v_mov_k(3, 1)];
        shared_add.extend(ds_add_u32(2, 3, 0));
        shared_add.extend(store_uav(3));
        let shared_add = if_v0_lt_s0(&shared_add);

        let mut shared_rtn = vec![v_mov_k(2, 0), v_mov_k(3, 1)];
        shared_rtn.extend(ds_add_rtn_u32(4, 2, 3, 0));
        shared_rtn.extend(store_uav(4));
        let shared_rtn = if_v0_lt_s0(&shared_rtn);

        for kernel in [&shared_add, &shared_rtn] {
            with_write_only_ok(32, false, || {
                assert_eq!(
                    run_analyze(kernel, [64, 1, 1], 1),
                    Err(super::Refusal::AtomicWithoutPlainRead)
                );
                assert_eq!(lowered_wave(kernel, [64, 1, 1], 1), 64);
            });
            with_write_only_ok(32, true, || {
                assert!(run_analyze(kernel, [64, 1, 1], 1).is_ok());
                assert_eq!(lowered_wave(kernel, [64, 1, 1], 1), 32);
            });
            with_write_only_ok(64, true, || {
                assert_eq!(
                    run_analyze(kernel, [64, 1, 1], 1),
                    Err(super::Refusal::AtomicWithoutPlainRead)
                );
            });
        }

        // 2. Shared-address LDS atomic WITH an explicit plain ds_read_b32 in the same `!all`
        //    region MUST stay refused (64) even with VICTORY_LDS_WRITE_ONLY_OK=1.
        let mut atomic_plus_read = vec![v_mov_k(2, 0), v_mov_k(3, 1)];
        atomic_plus_read.extend(ds_add_u32(2, 3, 0));
        atomic_plus_read.extend(ds_read_b32(4, 2, 0));
        atomic_plus_read.extend(store_uav(4));
        let atomic_plus_read = if_v0_lt_s0(&atomic_plus_read);
        with_write_only_ok(32, false, || {
            assert_eq!(
                run_analyze(&atomic_plus_read, [64, 1, 1], 1),
                Err(super::Refusal::ReadWriteOverlap)
            );
            assert_eq!(lowered_wave(&atomic_plus_read, [64, 1, 1], 1), 64);
        });
        with_write_only_ok(32, true, || {
            assert_eq!(
                run_analyze(&atomic_plus_read, [64, 1, 1], 1),
                Err(super::Refusal::ReadWriteOverlap)
            );
            assert_eq!(lowered_wave(&atomic_plus_read, [64, 1, 1], 1), 64);
        });

        // 3. Four-kernel lowering matrix:
        //    (a) same-dword write-only: 64 default -> 32 under opt-in flag on 32-lane host (64 on 64-lane)
        let mut same_waw = vec![v_mov_k(2, 0), v_mov_k(3, 1), v_mov_k(4, 2)];
        same_waw.extend(ds_write_b32(2, 3, 0));
        same_waw.extend(ds_write_b32(2, 4, 0));
        same_waw.extend(store_uav(3));
        let same_waw = if_v0_lt_s0(&same_waw);
        with_write_only_ok(32, false, || {
            assert_eq!(
                run_analyze(&same_waw, [64, 1, 1], 1),
                Err(super::Refusal::WriteWriteOverlap)
            );
            assert_eq!(lowered_wave(&same_waw, [64, 1, 1], 1), 64);
        });
        with_write_only_ok(32, true, || {
            assert!(run_analyze(&same_waw, [64, 1, 1], 1).is_ok());
            assert_eq!(lowered_wave(&same_waw, [64, 1, 1], 1), 32);
        });
        with_write_only_ok(64, true, || {
            assert_eq!(
                run_analyze(&same_waw, [64, 1, 1], 1),
                Err(super::Refusal::WriteWriteOverlap)
            );
        });

        //    (b) separate writes: 32 in both modes
        let mut sep_writes = vec![v_mov_k(2, 0), v_mov_k(3, 1), v_mov_k(4, 2)];
        sep_writes.extend(ds_write_b32(2, 3, 0));
        sep_writes.extend(ds_write_b32(2, 4, 4));
        sep_writes.extend(store_uav(3));
        let sep_writes = if_v0_lt_s0(&sep_writes);
        for enabled in [false, true] {
            with_write_only_ok(32, enabled, || {
                assert!(run_analyze(&sep_writes, [64, 1, 1], 1).is_ok());
                assert_eq!(lowered_wave(&sep_writes, [64, 1, 1], 1), 32);
            });
        }

        //    (c) overlapping read + write: 64 in both modes
        let mut overlap_rw = vec![v_mov_k(2, 0), v_mov_k(3, 1)];
        overlap_rw.extend(ds_write_b32(2, 3, 0));
        overlap_rw.extend(ds_read_b32(4, 2, 0));
        overlap_rw.extend(store_uav(4));
        let overlap_rw = if_v0_lt_s0(&overlap_rw);
        for enabled in [false, true] {
            with_write_only_ok(32, enabled, || {
                assert_eq!(
                    run_analyze(&overlap_rw, [64, 1, 1], 1),
                    Err(super::Refusal::ReadWriteOverlap)
                );
                assert_eq!(lowered_wave(&overlap_rw, [64, 1, 1], 1), 64);
            });
        }

        //    (d) read disjoint from both writes: 32 in both modes
        let mut disjoint_r_ww = vec![v_mov_k(2, 0), v_mov_k(3, 1), v_mov_k(4, 2)];
        disjoint_r_ww.extend(ds_write_b32(2, 3, 0));
        disjoint_r_ww.extend(ds_write_b32(2, 4, 4));
        disjoint_r_ww.extend(ds_read_b32(5, 2, 8));
        disjoint_r_ww.extend(store_uav(5));
        let disjoint_r_ww = if_v0_lt_s0(&disjoint_r_ww);
        for enabled in [false, true] {
            with_write_only_ok(32, enabled, || {
                assert!(run_analyze(&disjoint_r_ww, [64, 1, 1], 1).is_ok());
                assert_eq!(lowered_wave(&disjoint_r_ww, [64, 1, 1], 1), 32);
            });
        }

        // 4. Divergent s_barrier and unhandled ds_nop stay refused even under VICTORY_LDS_WRITE_ONLY_OK=1.
        let mut bar_write = vec![v_mov_k(2, 0), v_mov_k(3, 1)];
        bar_write.extend(ds_write_b32(2, 3, 0));
        bar_write.push(0xBF8A_0000); // s_barrier
        bar_write.extend(store_uav(3));
        let bar_write = if_v0_lt_s0(&bar_write);
        with_write_only_ok(32, true, || {
            assert_eq!(
                run_analyze(&bar_write, [64, 1, 1], 1),
                Err(super::Refusal::DivergentBarrier)
            );
        });

        let mut nop_write = vec![v_mov_k(2, 0), v_mov_k(3, 1)];
        nop_write.extend(ds_enc(0x14, 0, 0, 0, 0, 0, 0)); // ds_nop
        nop_write.extend(ds_write_b32(2, 3, 0));
        nop_write.extend(store_uav(3));
        let nop_write = if_v0_lt_s0(&nop_write);
        with_write_only_ok(32, true, || {
            assert_eq!(
                run_analyze(&nop_write, [64, 1, 1], 1),
                Err(super::Refusal::UnknownDs)
            );
        });
    }

    #[test]
    fn wave64_why_distinguishes_write_write_atomic_and_mixed_read_write() {
        // (a) Two plain writes to the same dword -> write/write overlap, NOT read+write.
        let mut same_waw = vec![v_mov_k(2, 0), v_mov_k(3, 1), v_mov_k(4, 2)];
        same_waw.extend(ds_write_b32(2, 3, 0));
        same_waw.extend(ds_write_b32(2, 4, 0));
        same_waw.extend(store_uav(3));
        let waw_why = wave_why_reasons(&if_v0_lt_s0(&same_waw), [64, 1, 1], 1);
        assert!(waw_why.contains(&"overlapping LDS writes in one lane-branch region"));
        assert!(!waw_why.contains(&"an LDS read and write in one lane-branch region"));

        // (b) Shared atomic with no plain ds_read* -> atomic-without-plain-read, NOT read+write.
        let mut shared_atomic = vec![v_mov_k(2, 0), v_mov_k(3, 1)];
        shared_atomic.extend(ds_add_u32(2, 3, 0));
        shared_atomic.extend(store_uav(3));
        let atomic_why = wave_why_reasons(&if_v0_lt_s0(&shared_atomic), [64, 1, 1], 1);
        assert!(atomic_why.contains(&"an LDS atomic with no plain LDS read in one lane-branch region"));
        assert!(!atomic_why.contains(&"an LDS read and write in one lane-branch region"));

        // (c) Mixed region with explicit plain ds_read* + atomic/write -> read and write.
        let mut mixed_atomic = vec![v_mov_k(2, 0), v_mov_k(3, 1)];
        mixed_atomic.extend(ds_add_u32(2, 3, 0));
        mixed_atomic.extend(ds_read_b32(4, 2, 0));
        mixed_atomic.extend(store_uav(4));
        let mixed_why = wave_why_reasons(&if_v0_lt_s0(&mixed_atomic), [64, 1, 1], 1);
        assert!(mixed_why.contains(&"an LDS read and write in one lane-branch region"));

        // (d) Divergent s_barrier inside `!all` -> barrier reason.
        let mut bar_inner = vec![v_mov_k(2, 0), v_mov_k(3, 1)];
        bar_inner.extend(ds_write_b32(2, 3, 0));
        bar_inner.push(0xBF8A_0000); // s_barrier
        bar_inner.extend(ds_read_b32(4, 2, 4));
        bar_inner.extend(store_uav(4));
        let bar_why = wave_why_reasons(&if_v0_lt_s0(&bar_inner), [64, 1, 1], 1);
        assert!(bar_why.contains(&"a barrier in a lane-branch region"));
    }

    #[test]
    fn partial_workgroup_byte_alias_is_refused() {
        let make_byte_access = |kind: super::Kind, tid_x: u64, off: u64| super::Access {
            kind,
            width: 1,
            expr: Some(super::ResolvedAddr {
                uniforms: Vec::new(),
                uniform_tz: 32,
                tid: [tid_x, 0, 0],
                console_lane: 0,
                off,
            }),
            uniform_data: false,
        };

        // Partial workgroups (`n = 33`, `n = 63`) and full multiples (`32`, `64`):
        // write at `tid.x` (`[0, n)`) aliases uniform read at `off = n - 1` (written by thread `n - 1`).
        for n in [32u32, 33, 63, 64] {
            let write = make_byte_access(super::Kind::Write, 1, 0);
            let alias_read = make_byte_access(super::Kind::Read, 0, u64::from(n - 1));
            assert!(
                !super::region_lane_private(&[write.clone(), alias_read.clone()], [n, 1, 1]),
                "n={n}: byte write [0, {n}) aliases uniform byte read at {}",
                n - 1
            );
            assert!(
                !super::region_lane_private(&[alias_read, write.clone()], [n, 1, 1]),
                "n={n}: reversed order must also reject byte alias at {}",
                n - 1
            );

            // Disjoint uniform byte read at `off = n` (immediately past `[0, n)`) stays accepted.
            let disjoint_read = make_byte_access(super::Kind::Read, 0, u64::from(n));
            assert!(
                super::region_lane_private(&[write, disjoint_read], [n, 1, 1]),
                "n={n}: uniform byte read at {n} is disjoint from [0, {n})"
            );
        }

        // End-to-end shader lowering check for `n = 33` and `n = 63`:
        // `ds_write_b8(v0, v3, 0)` + `ds_read_u8(v4, 0, n - 1)` inside `!all` must stay Wave64,
        // while `off = n` recovers Wave32.
        for n in [33u32, 63] {
            let mut alias_inner = vec![v_mov_k(2, 0), v_mov_k(3, 7)];
            alias_inner.extend(ds_write_b8(0, 3, 0));
            alias_inner.extend(ds_read_u8(4, 2, (n - 1) as u16));
            alias_inner.extend(store_uav(4));
            let alias_body = if_v0_lt_s0(&alias_inner);
            assert_eq!(
                run_analyze(&alias_body, [n, 1, 1], 1),
                Err(super::Refusal::ReadWriteOverlap),
                "n={n}: end-to-end analysis must refuse cross-lane byte alias"
            );
            assert_eq!(lowered_wave(&alias_body, [n, 1, 1], 1), 64);

            let mut disjoint_inner = vec![v_mov_k(2, 0), v_mov_k(3, 7)];
            disjoint_inner.extend(ds_write_b8(0, 3, 0));
            disjoint_inner.extend(ds_read_u8(4, 2, n as u16));
            disjoint_inner.extend(store_uav(4));
            let disjoint_body = if_v0_lt_s0(&disjoint_inner);
            assert!(
                run_analyze(&disjoint_body, [n, 1, 1], 1).is_ok(),
                "n={n}: disjoint byte read at {n} should be accepted"
            );
            assert_eq!(lowered_wave(&disjoint_body, [n, 1, 1], 1), 32);
        }
    }

    #[test]
    fn exhaustive_byte_access_oracle_has_zero_false_accepts() {
        let mut total = 0usize;
        let mut false_accepts = 0usize;
        let mut sharing_cases = 0usize;
        for n in 32u64..=65 {
            for stride in [1u64, 2, 4] {
                for initial in 0u64..4 {
                    for r in 0u64..=(n * stride + 4) {
                        total += 1;
                        let write = super::Access {
                            kind: super::Kind::Write,
                            width: 1,
                            expr: Some(super::ResolvedAddr {
                                uniforms: Vec::new(),
                                uniform_tz: 32,
                                tid: [stride, 0, 0],
                                console_lane: 0,
                                off: initial,
                            }),
                            uniform_data: false,
                        };
                        let read = super::Access {
                            kind: super::Kind::Read,
                            width: 1,
                            expr: Some(super::ResolvedAddr {
                                uniforms: Vec::new(),
                                uniform_tz: 32,
                                tid: [0, 0, 0],
                                console_lane: 0,
                                off: r,
                            }),
                            uniform_data: false,
                        };
                        let shares = (0..n).any(|tid| initial + stride * tid == r);
                        if shares {
                            sharing_cases += 1;
                        }
                        let accepted = super::region_lane_private(&[write, read], [n as u32, 1, 1]);
                        if shares && accepted {
                            false_accepts += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(total, 48_212);
        assert!(sharing_cases > 0);
        assert_eq!(false_accepts, 0);
    }

    #[test]
    fn unordered_lds_accesses_are_ignored_in_analyze() {
        // An overlapping read+write in a `!all` region is refused when ordered,
        // and accepted without extra meetings when marked unordered by `LdsOrder`.
        let mut inner = Vec::new();
        inner.push(v_mov_k(2, 0));
        inner.push(v_mov_k(3, 7));
        inner.extend(ds_write_b32(2, 3, 0));
        inner.extend(ds_read_b32(4, 2, 0));
        inner.extend(store_uav(4));
        let body = if_v0_lt_s0(&inner);
        let native = make_cs(&body, [64, 1, 1], 1, 16);
        let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
        let st = structure::structure(&p).expect("structure");
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
        let io = io::raise(&p, &native).expect("io::raise");
        let StageIo::Cs(cs) = native.io else {
            unreachable!();
        };
        let all = lower::all_lane_blocks(&st).expect("all_lane_blocks");

        assert_eq!(
            super::analyze(&p, &st, &io, &res, cs, &all, &HashSet::new()),
            Err(super::Refusal::ReadWriteOverlap)
        );

        let mut unordered = HashSet::new();
        for (b, block) in p.blocks.iter().enumerate() {
            if all[b] {
                continue;
            }
            for (k, inst) in block.insts.iter().enumerate() {
                if matches!(inst.fields, crate::gfx10::Fields::Ds { .. }) {
                    unordered.insert((b, k));
                }
            }
        }
        let meet = super::analyze(&p, &st, &io, &res, cs, &all, &unordered)
            .expect("unordered accesses should not refuse or force meetings");
        assert!(meet.iter().all(|&(a, b)| !a && !b));
    }

    #[test]
    fn refinement_a_refuses_uniform_address_write_with_lane_varying_data() {
        // 1. Acyclic `!all` region with uniform address `[0]` written with lane-varying `v0`
        //    and disjoint read at `[16]` MUST be refused (Wave 0 & Wave 1 race writing different values).
        let mut inner = vec![v_mov_k(2, 0)];
        inner.extend(ds_write_b32(2, 0, 0)); // data = v0 (tid.x, lane-varying)
        inner.extend(ds_read_b32(4, 2, 16)); // disjoint [16, 20)
        inner.extend(store_uav(4));
        let body = if_v0_lt_s0(&inner);
        assert_eq!(
            run_analyze(&body, [64, 1, 1], 1),
            Err(super::Refusal::ReadWriteOverlap)
        );
        assert_eq!(lowered_wave(&body, [64, 1, 1], 1), 64);

        // 2. Dual write `ds_write2_b32` with uniform data0 (`v3=1`) and lane-varying data1 (`v0`)
        //    + disjoint read at `[16]` MUST also be refused.
        let mut dual_bad = vec![v_mov_k(2, 0), v_mov_k(3, 1)];
        dual_bad.extend(ds_write2_b32(2, 3, 0, 0, 1));
        dual_bad.extend(ds_read_b32(4, 2, 16));
        dual_bad.extend(store_uav(4));
        let dual_body = if_v0_lt_s0(&dual_bad);
        assert_eq!(
            run_analyze(&dual_body, [64, 1, 1], 1),
            Err(super::Refusal::ReadWriteOverlap)
        );
        assert_eq!(lowered_wave(&dual_body, [64, 1, 1], 1), 64);

        // 3. Positive control: uniform user SGPR data (`v3 = s1`) at `[0]` + disjoint read at `[16]`
        //    is accepted by Refinement A and recovers Wave32.
        let mut uniform_ok = vec![v_mov_k(2, 0), v_mov_s(3, 1)];
        uniform_ok.extend(ds_write_b32(2, 3, 0));
        uniform_ok.extend(ds_read_b32(4, 2, 16));
        uniform_ok.extend(store_uav(4));
        let ok_body = if_v0_lt_s0(&uniform_ok);
        assert!(run_analyze(&ok_body, [64, 1, 1], 1).is_ok());
        assert_eq!(lowered_wave(&ok_body, [64, 1, 1], 1), 32);
    }

    #[test]
    fn unproven_write_only_regions_refused_by_default_and_allowed_only_with_opt_in() {
        // (a) Unresolved dynamic WAW (`v9` uninitialized -> `expr == None`) across two writes in `!all`.
        let mut dyn_waw = vec![v_mov_k(3, 1), v_mov_k(4, 2)];
        dyn_waw.extend(ds_write_b32(9, 3, 0));
        dyn_waw.extend(ds_write_b32(9, 4, 0));
        dyn_waw.extend(store_uav(3));
        let dyn_waw_body = if_v0_lt_s0(&dyn_waw);
        with_write_only_ok(32, false, || {
            assert_eq!(
                run_analyze(&dyn_waw_body, [64, 1, 1], 1),
                Err(super::Refusal::UnprovenWrite)
            );
            assert_eq!(lowered_wave(&dyn_waw_body, [64, 1, 1], 1), 64);
        });
        with_write_only_ok(32, true, || {
            assert!(run_analyze(&dyn_waw_body, [64, 1, 1], 1).is_ok());
            assert_eq!(lowered_wave(&dyn_waw_body, [64, 1, 1], 1), 32);
        });
        with_write_only_ok(64, true, || {
            assert_eq!(
                run_analyze(&dyn_waw_body, [64, 1, 1], 1),
                Err(super::Refusal::UnprovenWrite)
            );
        });

        // (b) Unresolved dynamic cyclic write inside a `!all` loop (`expr == None`).
        let mut cyc_body = vec![
            v_mov_k(2, 0),
            v_mov_k(3, 7),
            0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
            0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
            0xBF88_0005, // s_cbranch_execz -> restore
            v_add_nc_sv(2, 132, 2),
            ds_write_b32(9, 3, 0)[0],
            ds_write_b32(9, 3, 0)[1],
            0x7D88_0401, // v_cmp_gt_u32 vcc, s1, v2
            0xBF87_FFFA, // s_cbranch_vccnz loop (-6 words)
            0xBEFE_040C, // s_mov_b64 exec, s[12:13]
        ];
        cyc_body.extend(store_uav(3));
        with_write_only_ok(32, false, || {
            assert_eq!(
                run_analyze(&cyc_body, [64, 1, 1], 1),
                Err(super::Refusal::CyclicSharedWrite)
            );
            assert_eq!(lowered_wave(&cyc_body, [64, 1, 1], 1), 64);
        });
        with_write_only_ok(32, true, || {
            assert!(run_analyze(&cyc_body, [64, 1, 1], 1).is_ok());
            assert_eq!(lowered_wave(&cyc_body, [64, 1, 1], 1), 32);
        });

        // (c) Overlapping stride-1 `ds_write_b32 [tid], 1` (stride 1 < width 4).
        let mut stride1 = vec![v_mov_k(3, 1)];
        stride1.extend(ds_write_b32(0, 3, 0));
        stride1.extend(store_uav(3));
        let stride1_body = if_v0_lt_s0(&stride1);
        with_write_only_ok(32, false, || {
            assert_eq!(
                run_analyze(&stride1_body, [64, 1, 1], 1),
                Err(super::Refusal::UnprovenWrite)
            );
            assert_eq!(lowered_wave(&stride1_body, [64, 1, 1], 1), 64);
        });
        with_write_only_ok(32, true, || {
            assert!(run_analyze(&stride1_body, [64, 1, 1], 1).is_ok());
            assert_eq!(lowered_wave(&stride1_body, [64, 1, 1], 1), 32);
        });

        // (d) Single uniform-address write with lane-varying data (`ds_write_b32 [0], v0`).
        let mut uniform_lane_data = vec![v_mov_k(2, 0)];
        uniform_lane_data.extend(ds_write_b32(2, 0, 0));
        uniform_lane_data.extend(store_uav(0));
        let uld_body = if_v0_lt_s0(&uniform_lane_data);
        with_write_only_ok(32, false, || {
            assert_eq!(
                run_analyze(&uld_body, [64, 1, 1], 1),
                Err(super::Refusal::UnprovenWrite)
            );
            assert_eq!(lowered_wave(&uld_body, [64, 1, 1], 1), 64);
        });
        with_write_only_ok(32, true, || {
            assert!(run_analyze(&uld_body, [64, 1, 1], 1).is_ok());
            assert_eq!(lowered_wave(&uld_body, [64, 1, 1], 1), 32);
        });
    }

    #[test]
    fn shared_dag_eval_val_stays_linear_time() {
        // VOP3 v_add3_u32 (op 0x36d): vdst = s0 + s1 + s2.
        let v_add3 = |vdst: u8, s0: u8, s1: u8, s2: u8| -> [u32; 2] {
            [
                (0xD4u32 << 24) | (0x36Du32 << 16) | u32::from(vdst),
                ((256 + u32::from(s2)) << 18)
                    | ((256 + u32::from(s1)) << 9)
                    | (256 + u32::from(s0)),
            ]
        };

        // 40-step shared DAG of v_add_nc_u32(v2, v2) (2^40 calls without memoization).
        let mut add2_inner = vec![v_mov_k(2, 0), v_mov_k(3, 5)];
        for _ in 0..40 {
            add2_inner.push(v_add_nc_vv(2, 2, 2));
        }
        add2_inner.extend(ds_write_b32(2, 3, 0));
        add2_inner.extend(ds_read_b32(4, 2, 4));
        add2_inner.extend(store_uav(4));
        let add2_body = if_v0_lt_s0(&add2_inner);
        let t0 = std::time::Instant::now();
        assert!(run_analyze(&add2_body, [64, 1, 1], 1).is_ok());
        assert!(
            t0.elapsed().as_secs() < 2,
            "40-step shared add2 DAG should evaluate in O(N) time, took {:?}",
            t0.elapsed()
        );

        // 32-step shared DAG of v_add3_u32(v2, v2, v2) (3^32 calls without memoization).
        let mut add3_inner = vec![v_mov_k(2, 0), v_mov_k(3, 5)];
        for _ in 0..32 {
            add3_inner.extend(v_add3(2, 2, 2, 2));
        }
        add3_inner.extend(ds_write_b32(2, 3, 0));
        add3_inner.extend(ds_read_b32(4, 2, 4));
        add3_inner.extend(store_uav(4));
        let add3_body = if_v0_lt_s0(&add3_inner);
        let t1 = std::time::Instant::now();
        assert!(run_analyze(&add3_body, [64, 1, 1], 1).is_ok());
        assert!(
            t1.elapsed().as_secs() < 2,
            "32-step shared add3 DAG should evaluate in O(N) time, took {:?}",
            t1.elapsed()
        );
    }
}
