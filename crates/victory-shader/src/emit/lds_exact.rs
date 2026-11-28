//! Lane-branch regions whose LDS accesses the two host waves of a console wave need not order.
//!
//! The console runs a wave64 as one wave, where every lane's LDS read sees every write that came
//! before it in program order. A host of 32-lane waves runs the wave as two, which meet at group
//! barriers on the edges of each lane-branch region ([`meetings`]), never inside it: a barrier
//! must be run by every thread. So a region that both reads and writes LDS is refused. Two kinds
//! of such region need no order between the halves, and [`exact_region`] proves them:
//!
//! - **One thread runs the region.** Every way into its blocks is the edge of a branch on
//!   `v_cmp_eq(thread id, k)` (a group of one dimension: thread `k` of the group and no other;
//!   with `k` a scalar, at most one thread per console wave), or from a block that thread alone
//!   runs. Its reads and writes are one thread's, in its own order: the thread that runs a
//!   `s_and_saveexec` on `tid == 0` around a read-modify-write of a counter, say. The edges of
//!   the region keep their group barriers, so what the region writes reaches the code after it
//!   as before.
//! - **What it writes it does not read.** Per LDS access the byte range it can touch is bounded
//!   from how its address is made (a constant, a masked or shifted value, such a value plus a
//!   constant, the access's own offsets). If no range read in the region meets a range written
//!   in it, and two atomics that meet are the same operation, a read cannot see a write of the
//!   region at all, from either half, so the order of the halves cannot show. The atomics
//!   commute; one that returns the old value (`ds_add_rtn_u32`: a wave's reservation of slots)
//!   gives each half the value it saw first, which only the allocation idiom of
//!   [`super::lower::with_local_waves`] tolerates, so only that form takes them.
//!
//! Two writes in one region are taken not to hit the same dword from both halves (the same
//! shortcut as [`meetings`]).
//!
//! `VICTORY_LDS_EXACT=1` turns this on (off by default until a game run has shown the programs
//! it takes draw right). Even then [`super::emit`] tries it only after every other form, so a
//! program that translates without it keeps its form.

use std::collections::{HashMap, HashSet};

use crate::gfx10::{Fields, Operand};
use crate::ir::{Inst, Program, Reg, Value};
use crate::structure::{Atom, Bool, LaneExit, Structured};

use super::lower::HOST_LDS_DWORDS;

type Why = &'static str;

/// Whether these regions may be taken at all (`VICTORY_LDS_EXACT=1`).
pub(super) fn switch() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_EXACT.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LDS_EXACT").is_some_and(|v| v == "1"))
}

thread_local! {
    /// [`with_exact`] is running.
    static ACTIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
thread_local! {
    /// A test's choice for [`switch`], for its thread only.
    pub(super) static TEST_EXACT: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Whether the regions are taken now: inside [`with_exact`], the last forms `emit` tries.
pub(super) fn on() -> bool {
    switch() && ACTIVE.get()
}

/// Runs `f` with these regions taken, and the allocation idiom knowing the reservation by an
/// LDS atomic ([`super::lower::wave_local`]).
pub(super) fn with_exact<R>(f: impl FnOnce() -> R) -> R {
    let was = ACTIVE.replace(true);
    let r = f();
    ACTIVE.set(was);
    r
}

/// Whether `inst` is, in the last forms, an LDS atomic that the allocation idiom of
/// `lower::wave_local` may take as the wave's first lane reserving slots on a counter in
/// group-shared memory; with `total`, the operation of the wave's total that goes in, whether
/// it applies that one (with or without the old value back).
pub(super) fn reserves(inst: &Inst, total: Option<super::wave::Op>) -> bool {
    use super::wave::Op;
    if !on() || !matches!(inst.fields, Fields::Ds { gds: false, .. }) {
        return false;
    }
    let name = inst.name().replacen("_rtn", "", 1);
    let want: &[&str] = match total {
        None => &[
            "ds_add_u32",
            "ds_min_u32",
            "ds_max_u32",
            "ds_min_i32",
            "ds_max_i32",
            "ds_or_b32",
            "ds_and_b32",
            "ds_xor_b32",
        ],
        Some(Op::AddU) => &["ds_add_u32"],
        Some(Op::MinU) => &["ds_min_u32"],
        Some(Op::MaxU) => &["ds_max_u32"],
        Some(Op::MinI) => &["ds_min_i32"],
        Some(Op::MaxI) => &["ds_max_i32"],
        Some(Op::Or) => &["ds_or_b32"],
        Some(Op::And) => &["ds_and_b32"],
        Some(Op::Xor) => &["ds_xor_b32"],
        Some(_) => &[],
    };
    want.contains(&name.as_str())
}

/// Whether `lds_dwords` of group-shared memory pass the host's 32 KiB in the last forms, which
/// then refuse the program (or leave out what they need not have) rather than make one the
/// host cannot create.
pub(super) fn past(lds_dwords: u32) -> bool {
    on() && lds_dwords > HOST_LDS_DWORDS
}

/// With `VICTORY_LDS_EXACT=1`: a group of one dimension whose own LDS leaves no dword for
/// `wave_base`'s counter (all of D3D12's 32 KiB) takes its console threads from the flat id, as
/// a host does that gives each wave a run of consecutive ids; the counter is for the other
/// shapes, whose host waves are tiles.
pub(super) fn no_room(lds_dwords: u32, threads: [u32; 3]) -> bool {
    switch() && lds_dwords >= HOST_LDS_DWORDS && threads[1] == 1 && threads[2] == 1
}

/// Where each value is written: (block, instruction).
fn definitions(p: &Program) -> HashMap<Value, (usize, usize)> {
    let mut def = HashMap::new();
    for (b, block) in p.blocks.iter().enumerate() {
        for (i, inst) in block.insts.iter().enumerate() {
            for &(_, v) in &inst.writes {
                def.insert(v, (b, i));
            }
        }
    }
    def
}

/// The compare results (every dword they write) that hold in at most one thread of each console
/// wave of a group of one dimension: `v_cmp_eq` of the thread id (the program's `v0` on entry)
/// with a constant or a scalar.
fn single_thread_compares(p: &Program, threads: [u32; 3]) -> HashSet<Value> {
    let mut out = HashSet::new();
    if threads[1] != 1 || threads[2] != 1 {
        return out;
    }
    let Some(&(_, tid)) = p.inputs.iter().find(|(r, _)| *r == Reg::V(0)) else {
        return out;
    };
    for inst in p.blocks.iter().flat_map(|b| &b.insts) {
        if !matches!(
            inst.name(),
            "v_cmp_eq_u32" | "v_cmp_eq_i32" | "v_cmpx_eq_u32" | "v_cmpx_eq_i32"
        ) {
            continue;
        }
        let Fields::Valu(v) = &inst.fields else {
            continue;
        };
        if v.sdwa.is_some() || v.dpp.is_some() || v.src.len() != 2 {
            continue;
        }
        let is_tid = |o: &Operand| match *o {
            Operand::Vector { reg, .. } => inst.read(Reg::V(reg)) == Some(tid),
            _ => false,
        };
        // (a scalar is the same in every lane of a console wave: `tid == s` holds in one of
        // its lanes at most)
        let uniform = |o: &Operand| match *o {
            Operand::Const(_) | Operand::Literal(_) => true,
            Operand::Scalar { code, .. } => code < crate::ir::PARKED,
            _ => false,
        };
        let (a, b) = (&v.src[0], &v.src[1]);
        if (is_tid(a) && uniform(b)) || (is_tid(b) && uniform(a)) {
            out.extend(
                inst.writes
                    .iter()
                    .filter(|w| matches!(w.0, Reg::S(_)))
                    .map(|w| w.1),
            );
        }
    }
    out
}

/// Whether at most one lane has `b` equal to `want`, given the atoms that hold in one lane at
/// most. A mask joined at a loop or a merge (`Atom::Phi`) holds in at most one lane when every
/// edge into the join brings nothing, the join itself, or the one same condition that does.
fn at_most_one(
    st: &Structured,
    singles: &HashSet<Value>,
    b: Bool,
    want: bool,
    seen: &mut Vec<Value>,
) -> bool {
    let none = if want { Bool::FALSE } else { Bool::TRUE };
    if b == none {
        return true;
    }
    let Some((atom, lo, hi)) = st.bools.node(b) else {
        return false;
    };
    // (lanes with the atom false take `lo`, the others `hi`)
    let single = match atom {
        Atom::Lane(v) => singles.contains(&v),
        Atom::Phi(id) if seen.contains(&id) => {
            // (around a loop: the edges that are not this one decide)
            true
        }
        Atom::Phi(id) => {
            seen.push(id);
            let own = |e: Bool| st.bools.node(e) == Some((Atom::Phi(id), Bool::FALSE, Bool::TRUE));
            let mut brought: Vec<Bool> = Vec::new();
            for &e in st.phi_masks.get(&id).map_or(&[][..], |v| &v[..]) {
                if e != Bool::FALSE && !own(e) && !brought.contains(&e) {
                    brought.push(e);
                }
            }
            let ok = st.phi_masks.contains_key(&id)
                && match brought[..] {
                    [] => true,
                    [e] => at_most_one(st, singles, e, true, seen),
                    _ => false,
                };
            seen.pop();
            ok
        }
        _ => false,
    };
    if lo == none {
        // (the lanes with `want` are among those that have the atom: at most one when it holds
        // in one lane at most)
        single || at_most_one(st, singles, hi, want, seen)
    } else if hi == none {
        at_most_one(st, singles, lo, want, seen)
    } else {
        false
    }
}

/// Per IR block: whether at most one thread of each console wave runs it (the module's first
/// kind of region). The greatest set in which every way into a block is the edge of a branch
/// on such a condition, or comes from a block of the set. None in a group of more than one
/// dimension.
pub(super) fn single_thread_blocks(p: &Program, st: &Structured, threads: [u32; 3]) -> Vec<bool> {
    let n = st.exits.len().min(p.blocks.len());
    let singles = single_thread_compares(p, threads);
    let mut s = vec![false; n];
    if singles.is_empty() || n == 0 {
        return s;
    }
    // edges into each block: (from, the condition and its value that take a lane along the
    // edge when it is a branch's)
    let mut into: Vec<Vec<(usize, Option<(Bool, bool)>)>> = vec![Vec::new(); n];
    for (q, e) in st.exits.iter().enumerate().take(n) {
        match *e {
            LaneExit::Jump(t) if t < n => into[t].push((q, None)),
            LaneExit::Branch {
                when,
                taken,
                not_taken,
            } => {
                if taken == not_taken {
                    if taken < n {
                        into[taken].push((q, None));
                    }
                    continue;
                }
                if taken < n {
                    into[taken].push((q, Some((when, true))));
                }
                if not_taken < n {
                    into[not_taken].push((q, Some((when, false))));
                }
            }
            _ => {}
        }
    }
    // (the entry, which every thread runs, and what nothing reaches are in no set)
    s.iter_mut().skip(1).for_each(|x| *x = true);
    loop {
        let mut changed = false;
        for b in 1..n {
            if !s[b] {
                continue;
            }
            let ok = !into[b].is_empty()
                && into[b].iter().all(|&(q, edge)| {
                    s[q] || edge.is_some_and(|(w, want)| {
                        at_most_one(st, &singles, w, want, &mut Vec::new())
                    })
                });
            if !ok {
                s[b] = false;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    s
}

/// A byte range of LDS, both ends included.
type Range = (u64, u64);

/// The values `v` can hold, as an inclusive range: what the instruction that made it and its
/// operands say; anything else the whole 32 bits.
fn bounds(p: &Program, def: &HashMap<Value, (usize, usize)>, v: Value, depth: u32) -> Range {
    const ALL: Range = (0, u32::MAX as u64);
    let Some(&(b, i)) = def.get(&v) else {
        return ALL;
    };
    if depth == 0 {
        return ALL;
    }
    let inst = &p.blocks[b].insts[i];
    let Fields::Valu(f) = &inst.fields else {
        return ALL;
    };
    if f.sdwa.is_some()
        || f.dpp.is_some()
        || f.abs != 0
        || f.neg != 0
        || f.opsel != 0
        || f.clamp
        || f.omod != 0
    {
        return ALL;
    }
    let of = |o: &Operand| -> Range {
        match *o {
            Operand::Const(c) => {
                crate::resource::inline(c).map_or(ALL, |k| (u64::from(k), u64::from(k)))
            }
            Operand::Literal(k) => (u64::from(k), u64::from(k)),
            Operand::Vector { reg, .. } => inst
                .read(Reg::V(reg))
                .map_or(ALL, |x| bounds(p, def, x, depth - 1)),
            _ => ALL,
        }
    };
    let sources: Vec<Range> = f.src.iter().map(of).collect();
    let max = u64::from(u32::MAX);
    match (inst.name(), &sources[..]) {
        ("v_mov_b32", [x]) => *x,
        // not above either operand
        ("v_and_b32", [a, b]) => (0, a.1.min(b.1)),
        ("v_lshrrev_b32", [s, x]) if s.0 == s.1 && s.0 < 32 => (x.0 >> s.0, x.1 >> s.0),
        ("v_lshlrev_b32", [s, x]) if s.0 == s.1 && s.0 < 32 && x.1 << s.0 <= max => {
            (x.0 << s.0, x.1 << s.0)
        }
        ("v_add_nc_u32", [a, b]) if a.1 + b.1 <= max => (a.0 + b.0, a.1 + b.1),
        _ => ALL,
    }
}

/// How an LDS access touches memory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Read,
    Write,
    /// An atomic of this operation; whether it returns the old value.
    Atomic(&'static str, bool),
}

/// The bytes an LDS instruction can touch, per dword access of it, and how: `None` for one this
/// module does not know the reach of.
fn accesses(
    p: &Program,
    def: &HashMap<Value, (usize, usize)>,
    inst: &Inst,
) -> Option<Vec<(Range, Kind)>> {
    let Fields::Ds {
        addr,
        offset0,
        offset1,
        gds,
        ..
    } = &inst.fields
    else {
        return None;
    };
    if *gds {
        return None;
    }
    let base = match addr {
        Some(Operand::Vector { reg, .. }) => inst
            .read(Reg::V(*reg))
            .map_or((0, u64::from(u32::MAX)), |v| bounds(p, def, v, 6)),
        _ => return None,
    };
    let (op, size) = match inst.name().rsplit_once('_') {
        Some((op, "b32" | "u32" | "i32")) => (op, 4u64),
        Some((op, "b64" | "u64" | "i64")) => (op, 8),
        Some((op, "u8" | "i8" | "b8")) => (op, 1),
        Some((op, "u16" | "i16" | "b16")) => (op, 2),
        // (float atomics that add round: their order shows)
        Some((op, "f32")) if !op.contains("_add") && !op.contains("_sub") => (op, 4),
        _ => return None,
    };
    let (o0, o1) = (u64::from(*offset0), u64::from(*offset1));
    // (byte offsets, the kind)
    let (offsets, kind): (Vec<u64>, Kind) = if op.starts_with("ds_read2st64") {
        (vec![o0 * 64 * size, o1 * 64 * size], Kind::Read)
    } else if op.starts_with("ds_read2") {
        (vec![o0 * size, o1 * size], Kind::Read)
    } else if op.starts_with("ds_write2st64") {
        (vec![o0 * 64 * size, o1 * 64 * size], Kind::Write)
    } else if op.starts_with("ds_write2") {
        (vec![o0 * size, o1 * size], Kind::Write)
    } else if op == "ds_read" {
        (vec![o0 | o1 << 8], Kind::Read)
    } else if op == "ds_write" {
        (vec![o0 | o1 << 8], Kind::Write)
    } else {
        let returns = op.contains("_rtn");
        let atomic = match op.replace("_rtn", "").as_str() {
            "ds_add" => "add",
            "ds_sub" => "sub",
            "ds_min" => "min",
            "ds_max" => "max",
            "ds_and" => "and",
            "ds_or" => "or",
            "ds_xor" => "xor",
            _ => return None,
        };
        (vec![o0 | o1 << 8], Kind::Atomic(atomic, returns))
    };
    offsets
        .into_iter()
        .map(|off| {
            let (lo, hi) = (base.0 + off, base.1 + off + size - 1);
            // (an address past the 32 bits wraps: no range then)
            (hi <= u64::from(u32::MAX)).then_some(((lo, hi), kind))
        })
        .collect()
}

/// Whether the lane-branch region `blocks` (of [`meetings`], which reads and writes LDS)
/// needs no order between the two host waves, as the module's text says; else why not.
/// `single` is [`single_thread_blocks`]; `rtn_ok`: the form takes atomics that return the old
/// value (the allocation idiom's local waves).
pub(super) fn exact_region(
    p: &Program,
    blocks: &[usize],
    single: &[bool],
    rtn_ok: bool,
) -> Result<(), Why> {
    if !on() {
        return Err("exact LDS regions are off");
    }
    if blocks
        .iter()
        .all(|&b| single.get(b).copied().unwrap_or(false))
    {
        return Ok(());
    }
    let def = definitions(p);
    let mut all: Vec<(Range, Kind)> = Vec::new();
    for &b in blocks {
        for inst in &p.blocks[b].insts {
            let name = inst.name();
            if !matches!(inst.fields, Fields::Ds { .. })
                || name.starts_with("ds_swizzle")
                || name.contains("permute")
            {
                continue;
            }
            let Some(list) = accesses(p, &def, inst) else {
                return Err("an LDS access whose reach is not known");
            };
            all.extend(list);
        }
    }
    let meets = |a: &Range, b: &Range| a.0 <= b.1 && b.0 <= a.1;
    for (n, (ra, ka)) in all.iter().enumerate() {
        if let Kind::Atomic(_, true) = ka
            && !rtn_ok
        {
            return Err("an atomic that returns the old value");
        }
        for (rb, kb) in &all[n + 1..] {
            if !meets(ra, rb) {
                continue;
            }
            match (ka, kb) {
                (Kind::Read, Kind::Read) | (Kind::Write, Kind::Write) => {}
                (Kind::Atomic(x, _), Kind::Atomic(y, _)) if x == y => {}
                _ => return Err("an LDS range read and written in one region"),
            }
        }
    }
    Ok(())
}

/// LDS accesses of a console wave run as two host waves, where some sit in blocks that only
/// some lanes run (`all` is `lower.rs`'s `all_lane_blocks`): per IR block, whether the host
/// waves meet (a group barrier) at its start and before its exit. `None` if barriers cannot
/// keep the console's order.
///
/// The console runs the wave's lanes in step, so an LDS write is seen by every later read. Two
/// host waves keep that order if they meet between any two accesses of which one writes. A
/// barrier must be run by every thread, so it cannot stand in a lane branch's region; but
/// every way into and out of such a region passes a block all lanes run, and the waves meet
/// there: before the exit of an all-lane block into a region with LDS accesses, and at the
/// start of an all-lane block entered from one. Accesses in all-lane blocks get their own
/// barrier as before. What that cannot order is a write and a read inside one region (blocks
/// joined without an all-lane block between them): refused, unless [`exact_region`] takes it
/// (in the last forms; `threads` is the group's size, `rtn_ok` as there). The accesses in
/// `unordered` (`lower.rs`'s `LdsOrder`) need no meeting and are left out. `why`: a
/// `WAVE64-LDS-REGION` line per refused region, with its code offsets and the reason.
/// shortcut: two writes in one region are taken not to hit the same dword from both halves.
pub(super) fn meetings(
    p: &Program,
    st: &Structured,
    all: &[bool],
    unordered: &HashSet<(usize, usize)>,
    threads: [u32; 3],
    rtn_ok: bool,
    why: bool,
) -> Option<Vec<(bool, bool)>> {
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
    // regions: blocks not every lane runs, joined by the edges between them
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
    // per region: LDS reads, LDS writes
    let mut reads = vec![false; n];
    let mut writes = vec![false; n];
    for (b, block) in p.blocks.iter().enumerate().take(n) {
        if all[b] {
            continue;
        }
        let r = find(&mut region, b);
        for (k, i) in block.insts.iter().enumerate() {
            let name = i.name();
            if !matches!(i.fields, Fields::Ds { .. })
                || name.starts_with("ds_swizzle")
                || name.contains("permute")
            {
                continue;
            }
            if unordered.contains(&(b, k)) {
                continue; // (no meeting: `LdsOrder`)
            }
            if name.starts_with("ds_read") {
                reads[r] = true;
            } else {
                writes[r] = true;
            }
        }
    }
    let single = if on() {
        single_thread_blocks(p, st, threads)
    } else {
        Vec::new()
    };
    let mut refused = false;
    for r in (0..n).filter(|&r| reads[r] && writes[r]) {
        let blocks: Vec<usize> = (0..n)
            .filter(|&b| !all[b] && find(&mut region, b) == r)
            .collect();
        let Err(what) = exact_region(p, &blocks, &single, rtn_ok) else {
            continue;
        };
        refused = true;
        if why {
            let (lo, hi) = blocks.iter().fold((usize::MAX, 0), |(lo, hi), &b| {
                (lo.min(p.blocks[b].offset), hi.max(p.blocks[b].offset))
            });
            eprintln!(
                "WAVE64-LDS-REGION B{} ({} blocks, {lo:#x}..{hi:#x}): {what}",
                blocks.first().copied().unwrap_or(0),
                blocks.len(),
            );
        }
    }
    if refused {
        return None;
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
    Some(meet)
}
