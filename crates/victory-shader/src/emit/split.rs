//! Split waves: a console wave64 compute program on a host whose waves
//! have 32 lanes (NVIDIA). Each console wave runs as two host waves, its
//! low and high half (console lanes 0-31 and 32-63); a cross-lane operation
//! takes each half's part over the host wave and combines the two through
//! group-shared memory between thread-group barriers (see
//! `lower::Lower::exchange`).
//!
//! A barrier must be reached by every thread of the group, so exchanges are
//! only possible where the whole group runs together: in a block no lane
//! branch decides whether (or how often) a lane reaches. [`convergent`]
//! finds those blocks; elsewhere each half works alone, over its own 32
//! lanes.
//!
//! Ported from NVIctory (commits c41faed..61db46f) as a fallback only: the
//! lowering here keeps its own exchange (`Lower::exchange_halves`) and
//! meetings (`lds_meetings`); this module adds NVIctory's proofs that the
//! LDS accesses of a lane-branch region need no fence between them
//! ([`unfenced_lds`]), and what `emit` changes in a program they refuse
//! ([`skips_forward`], [`loop_exits_over`]). Only programs every other form
//! leaves needing 64-lane waves get there ([`with_fallback`]).

use crate::gfx10::{Fields, Operand};
use crate::ir::{Inst, Program, Reg, Value};
use crate::structure::{Atom, Bool, LaneExit, Structured};

/// Per block: whether every thread of the group that starts the program
/// reaches it, together and as often as the others.
///
/// A scalar branch can differ between host halves after a half-local read;
/// one on SCC that only same-in-both-halves scalars decide ([`half_local`])
/// sends the whole group one way and splits nothing.
/// Model watchdog returns as exits too: threads that return cannot reach
/// a later barrier, even if that block post-dominated the original loop.
/// A watchdog latch the whole group reaches together counts the same
/// iterations in every thread, so all return together.
///
/// Optimistic fixpoint: start with only lane branches splitting, then add
/// scalar branches and latches whose justification fails until none do.
/// Halves start with the same scalars, so they stay equal until a split.
/// A branch the wave takes together ([`Structured::wide`]: the halves
/// exchange the decision) splits nothing where the group reaches it together.
/// `exchanged`: the blocks where `Lower` exchanges a first-lane read between the halves (its
/// all-lane blocks).
pub(crate) fn convergent(
    p: &Program,
    st: &Structured,
    watchdog_return: impl Fn(usize, usize) -> bool,
    exchanged: &[bool],
) -> Vec<bool> {
    let exit = st.exits.len();
    let mut succs: Vec<Vec<usize>> = st
        .exits
        .iter()
        .enumerate()
        .map(|(b, e)| match *e {
            LaneExit::Jump(t) if watchdog_return(b, t) => vec![t, exit],
            LaneExit::Jump(t) => vec![t],
            LaneExit::Branch {
                taken, not_taken, ..
            } => vec![taken, not_taken],
            LaneExit::End | LaneExit::Indirect => Vec::new(),
        })
        .collect();
    succs.push(Vec::new());
    let scalar = |b: usize| match st.exits[b] {
        LaneExit::Branch { when, .. } => {
            let atoms = st.bools.support(when);
            (!atoms.is_empty() && atoms.iter().all(|a| matches!(a, Atom::Uniform(_)))).then_some(atoms)
        }
        _ => None,
    };
    let mut splits: Vec<bool> = (0..=exit).map(|b| b < exit && succs[b].len() > 1 && scalar(b).is_none()
        && !matches!(st.exits[b], LaneExit::Jump(_)) && !st.wide.contains(&b)).collect();
    // (no SCC here is a ballot over both halves: the fork's wide branches exchange their
    // decision themselves)
    let waves = std::collections::HashSet::new();
    loop {
        let out = reconverged(&succs, &splits);
        let dirty = half_local(p, &out, &waves, exchanged);
        let next: Vec<bool> = (0..=exit)
            .map(|b| {
                b < exit && succs[b].len() > 1 && match st.exits[b] {
                    LaneExit::Jump(_) => !out[b],
                    _ if st.wide.contains(&b) => !out[b],
                    _ => scalar(b).is_none_or(|atoms| {
                        atoms.iter().any(|a| matches!(a, Atom::Uniform(v) if dirty.contains(v)))
                    }),
                }
            })
            .collect();
        if next == splits {
            let mut out = out;
            out.pop();
            return out;
        }
        splits = next;
    }
}

/// Values that may differ between the two host halves, given which blocks
/// the whole group runs together (`together`): lane data, masks, anything
/// a vector instruction writes, scalar work outside `together` (a half
/// with no active lanes skips it), and whatever they reach. Phis join
/// equal values only where every predecessor ran together too.
/// A wave-level branch's SCC (`waves`) is a ballot over both halves where
/// the group runs together, the same in each.
fn half_local(
    p: &Program,
    together: &[bool],
    waves: &std::collections::HashSet<Value>,
    exchanged: &[bool],
) -> std::collections::HashSet<Value> {
    use crate::ir::{EXEC, VCC};
    let masks = |r: Reg| matches!(r, Reg::V(_) | Reg::Scc) || matches!(r, Reg::S(c) if c & !1 == EXEC || c & !1 == VCC);
    let mut dirty: std::collections::HashSet<Value> =
        p.inputs.iter().filter(|(r, _)| masks(*r)).map(|&(_, v)| v).collect();
    loop {
        let before = dirty.len();
        for (b, block) in p.blocks.iter().enumerate() {
            let joined = together[b] && block.preds.iter().all(|&q| together[q]);
            for phi in &block.phis {
                if !joined || phi.args.iter().any(|a| dirty.contains(a)) {
                    dirty.insert(phi.value);
                }
            }
            for i in &block.insts {
                // Clocks, hardware ids and atomics answer each host wave differently.
                let n = i.name();
                let scalar = matches!(i.fields, Fields::Sop { .. } | Fields::Smem { .. })
                    && !["s_memtime", "s_memrealtime", "s_getreg", "s_sendmsg", "s_atomic", "s_buffer_atomic"]
                        .iter().any(|x| n.starts_with(x));
                // (here a first-lane read where the group runs together is exchanged between
                // the halves: both get the console's value, `Lower` "a first-lane read")
                if n == "v_readfirstlane_b32" && together[b] && exchanged.get(b).copied().unwrap_or(false) {
                    continue;
                }
                if !scalar || !together[b] || i.reads.iter().any(|&(r, v)| masks(r) && r != Reg::Scc || dirty.contains(&v)) {
                    let ballot = |v: &Value| together[b] && waves.contains(v);
                    dirty.extend(i.writes.iter().map(|&(_, v)| v).filter(|v| !ballot(v)));
                }
            }
        }
        if dirty.len() == before {
            return dirty;
        }
    }
}

/// Whether the branch at byte `at` is a plain forward skip: its target is
/// after it and nothing it skips jumps back to at or before it (a loop's
/// exit is not: falling through it would loop on with no lane left).
pub(crate) fn skips_forward(p: &Program, at: usize) -> bool {
    let Some((b, taken)) = p.blocks.iter().find_map(|b| {
        let crate::ir::Exit::Branch { taken, .. } = b.exit else { return None };
        b.insts.last().filter(|i| i.machine.offset == at).map(|_| (b, taken))
    }) else {
        return false;
    };
    let target = p.blocks[taken].offset;
    target > b.offset
        && !p.blocks.iter().filter(|q| (at..target).contains(&q.offset)).any(|q| {
            q.exit.successors().iter().any(|&t| p.blocks[t].offset <= at)
        })
}

/// The byte offset of the innermost forward `s_cbranch_execz` whose skip
/// spans byte `offset` ([`skips_forward`]).
pub(crate) fn execz_skip_over(p: &Program, offset: usize) -> Option<usize> {
    p.blocks
        .iter()
        .filter_map(|b| {
            let crate::ir::Exit::Branch { taken, .. } = b.exit else { return None };
            let last = b.insts.last().filter(|i| i.name() == "s_cbranch_execz")?;
            let at = last.machine.offset;
            (at < offset && offset < p.blocks[taken].offset && skips_forward(p, at)).then_some(at)
        })
        .max()
}

/// The `s_cbranch_scc*`, `vcc*` and `exec*` (byte offsets, not in
/// `taken`) that close or leave the innermost loop around byte `offset` that has any: a loop is a
/// back edge to at or before `offset` from a block ending at or after it.
pub(crate) fn loop_exits_over(
    p: &Program,
    offset: usize,
    taken: &std::collections::HashSet<usize>,
) -> Vec<usize> {
    let end = |b: &crate::ir::Block| b.insts.last().map_or(b.offset, |i| i.machine.offset);
    let mut loops: Vec<(usize, usize)> = p
        .blocks
        .iter()
        .flat_map(|b| {
            let at = end(b);
            b.exit.successors().into_iter().map(move |t| (p.blocks[t].offset, at))
        })
        .filter(|&(head, at)| head <= offset && offset <= at && head <= at)
        .collect();
    loops.sort_by_key(|&(head, at)| at - head);
    for (head, at) in loops {
        let exits: Vec<usize> = p
            .blocks
            .iter()
            .filter(|b| (head..=at).contains(&b.offset))
            .filter_map(|b| {
                let last = b.insts.last().filter(|i| {
                    ["s_cbranch_scc", "s_cbranch_vcc", "s_cbranch_exec"].iter().any(|k| i.name().starts_with(k))
                })?;
                let o = last.machine.offset;
                let leaves = b.exit.successors().iter().any(|&t| {
                    let t = p.blocks[t].offset;
                    t < head || t > at || o == at
                });
                (leaves && !taken.contains(&o)).then_some(o)
            })
            .collect();
        if !exits.is_empty() {
            return exits;
        }
    }
    Vec::new()
}

thread_local! {
    /// See [`with_fallback`].
    static FALLBACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The byte offset of the access [`unfenced_lds`] last refused on this thread (with
    /// [`REFUSED`]), for the retry in `emit`.
    pub(crate) static REFUSED_AT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    /// The byte offsets of the lane branches (`s_cbranch_scc*`, `vcc*`, `exec*`) that split the
    /// group at blocks it runs together, found by the last [`admit`] on this thread.
    pub(crate) static SPLITTING: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
    /// The byte offsets of the last instructions of the region [`unfenced_lds`]
    /// last refused on this thread, its root's first, for the retry in
    /// `emit_with`.
    // shortcut: a side channel past `ShaderError`, which carries one offset;
    // give the error a field if a second caller needs it.
    pub(crate) static REFUSED: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Runs `f` with LDS read and written in one lane-branch region admitted where
/// [`unfenced_lds`] proves the accesses need no fence between them (`Lower::new`):
/// the last resort for a console wave64 program on 32-lane hosts, after every
/// other form of it still needed 64-lane waves.
pub(crate) fn with_fallback<R>(f: impl FnOnce() -> R) -> R {
    let was = FALLBACK.replace(true);
    let r = f();
    FALLBACK.set(was);
    r
}

/// Whether [`with_fallback`] is in force on this thread.
pub(crate) fn fallback() -> bool {
    FALLBACK.get()
}

/// The two halves may run different arms of a region at the same time.
/// Another access after a write needs a barrier unless the compiler can
/// prove that the accesses touch different host dwords.
pub(super) fn unfenced_lds(
    p: &Program,
    order: &[usize],
    uniform: &[bool],
    access: impl Fn(&Inst) -> bool,
    write: impl Fn(&Inst) -> bool,
    independent: impl Fn(usize, usize, &Inst, &Inst) -> bool,
    singleton: impl Fn(usize, usize) -> bool,
    alone: impl Fn(usize) -> bool,
) -> Option<usize> {
    let mut seen = vec![usize::MAX; p.blocks.len()];
    for &root in order {
        if !uniform[root] || alone(root) {
            continue;
        }
        let mut work = p.blocks[root].exit.successors();
        let mut previous = None;
        let mut multiple = false;
        let mut multiple_blocks = false;
        let mut written = false;
        while let Some(b) = work.pop() {
            if uniform[b] || seen[b] == root {
                continue;
            }
            seen[b] = root;
            for inst in p.blocks[b].insts.iter().filter(|i| access(i)) {
                let writes = write(inst);
                if (written || previous.is_some() && writes)
                    && !(previous.is_some_and(|(block, first)| {
                        block == b && ((!multiple && independent(root, b, first, inst))
                            || (!multiple_blocks && singleton(root, b)))
                    }))
                {
                    let mut region = vec![root];
                    let mut next = p.blocks[root].exit.successors();
                    while let Some(b) = next.pop() {
                        if !uniform[b] && !region.contains(&b) {
                            region.push(b);
                            next.extend(p.blocks[b].exit.successors());
                        }
                    }
                    REFUSED.set(region.iter().filter_map(|&b| p.blocks[b].insts.last()).map(|i| i.machine.offset).collect());
                    return Some(inst.machine.offset);
                }
                written |= writes;
                multiple_blocks |= previous.is_some_and(|(block, _)| block != b);
                multiple |= previous.is_some();
                previous = Some((b, inst));
            }
            work.extend(p.blocks[b].exit.successors());
        }
    }
    None
}

fn lds_access(i: &Inst) -> bool {
    let n = i.name();
    matches!(i.fields, Fields::Ds { .. }) && !n.starts_with("ds_swizzle") && !n.contains("permute")
}

/// An LDS instruction that writes (stores and atomics).
fn lds_write(i: &Inst) -> bool {
    lds_access(i) && !i.name().starts_with("ds_read")
}

/// With [`with_fallback`]: the blocks the whole group runs together (`all`, the
/// fork's all-lane blocks, that [`convergent`] also finds so), if [`unfenced_lds`]
/// admits every lane-branch region's LDS accesses between them; else the byte offset
/// of the access it refused ([`REFUSED`] holds its region). `threads`: the group;
/// `bytes`: the console's LDS.
pub(super) fn admit(p: &Program, st: &Structured, all: &[bool], threads: [u32; 3], bytes: u32) -> Result<Vec<bool>, usize> {
    let order = crate::ir::reverse_postorder(&p.blocks);
    let mut rpo = vec![usize::MAX; p.blocks.len()];
    for (n, &b) in order.iter().enumerate() {
        rpo[b] = n;
    }
    // A lane at the loop limit leaves the program on a counted back edge (`abort_block`).
    let together = convergent(p, st, |from, to| rpo[to] <= rpo[from], all);
    let uniform: Vec<bool> = (0..st.exits.len())
        .map(|b| all.get(b).copied().unwrap_or(false) && together.get(b).copied().unwrap_or(false))
        .collect();
    SPLITTING.set(
        p.blocks
            .iter()
            .enumerate()
            .filter(|&(b, block)| {
                uniform[b]
                    && matches!(st.exits[b], LaneExit::Branch { when, .. }
                        if st.bools.support(when).iter().any(|a| matches!(a, Atom::Lane(_))))
                    && block.exit.successors().iter().any(|&t| !uniform[t])
            })
            .filter_map(|(_, block)| block.insts.last())
            .filter(|i| ["s_cbranch_scc", "s_cbranch_vcc", "s_cbranch_exec"].iter().any(|k| i.name().starts_with(k)))
            .map(|i| i.machine.offset)
            .collect(),
    );
    if std::env::var_os("VICTORY_WAVE_WHY").is_some() {
        let n = |v: &[bool]| v.iter().filter(|&&x| x).count();
        eprintln!("WAVE64-SPLIT blocks {}: all-lane {}, convergent {}, both {}", uniform.len(), n(all), n(&together), n(&uniform));
        for (b, block) in p.blocks.iter().enumerate() {
            if !uniform[b] || st.exits[b] == LaneExit::End {
                continue;
            }
            let succ = block.exit.successors();
            if succ.iter().any(|&t| !uniform[t]) {
                let last = block.insts.last().map_or(("-", 0), |i| (i.name(), i.machine.offset));
                let kind = match st.exits[b] {
                    LaneExit::Jump(_) => "jump".to_string(),
                    LaneExit::Branch { when, .. } => format!("{:?}{}", st.bools.support(when), if st.wide.contains(&b) { " wide" } else { "" }),
                    _ => "?".into(),
                };
                eprintln!("WAVE64-SPLIT   split at B{b} {} {:#x}: {kind}", last.0, last.1);
            }
        }
    }
    // Only a group of one console wave shares LDS without barriers (one wave needs none, so
    // the console compiler leaves them out); a larger group's program has its own, as on
    // NVIctory, which checks one-wave groups alone.
    if threads.iter().product::<u32>() > 64 {
        return Ok(uniform);
    }
    let one_wave = threads == [64, 1, 1];
    let one_dimensional = threads[1] == 1 && threads[2] == 1;
    match unfenced_lds(
        p,
        &order,
        &uniform,
        lds_access,
        lds_write,
        |root, block, first, second| independent_lds(p, st, root, block, first, second, bytes, one_wave),
        |root, block| singleton_lds(p, st, &uniform, root, block, one_wave),
        |root| thread_zero(p, st, &uniform, root, one_dimensional),
    ) {
        Some(offset) => Err(offset),
        None => Ok(uniform),
    }
}

/// A region only thread zero enters: `root` branches on `v_cmp(x)_eq_u32
/// 0, v0` of a one-dimensional group and the other arm is the convergent
/// join, so no other thread runs until the join's barrier. One thread
/// cannot race itself, however its accesses are ordered.
pub(super) fn thread_zero(p: &Program, st: &Structured, uniform: &[bool], root: usize, one_dimensional: bool) -> bool {
    let proof = || {
        let LaneExit::Branch { when, taken, not_taken } = st.exits[root] else { return None };
        let (zero, other) = match st.bools.node(when)? {
            (Atom::Lane(v), Bool::FALSE, Bool::TRUE) => (v, (taken, not_taken)),
            (Atom::Lane(v), Bool::TRUE, Bool::FALSE) => (v, (not_taken, taken)),
            _ => return None,
        };
        let (_, join) = other;
        let cmp = p.blocks[root].insts.iter().find(|i| i.writes.iter().any(|&(_, v)| v == zero))?;
        let Fields::Valu(v) = &cmp.fields else { return None };
        let id = p.inputs.iter().find(|(r, _)| *r == Reg::V(0))?.1;
        let plain = v.neg == 0 && v.abs == 0 && v.opsel == 0 && v.sdwa.is_none() && v.dpp.is_none();
        let operands = match v.src.as_slice() {
            [Operand::Const(128), Operand::Vector { reg: 0, dwords: 1 }]
            | [Operand::Vector { reg: 0, dwords: 1 }, Operand::Const(128)] => true,
            _ => false,
        };
        (one_dimensional && uniform[join] && plain && operands
            && matches!(cmp.name(), "v_cmpx_eq_u32" | "v_cmp_eq_u32")
            && cmp.read(Reg::V(0)) == Some(id))
            .then_some(())
    };
    proof().is_some()
}

/// A derived flag can name thread zero alone: entry initializes it to zero,
/// only that thread writes -1, and the next branch tests for -1.
pub(super) fn singleton_lds(
    p: &Program, st: &Structured, uniform: &[bool], root: usize, block: usize, one_dimensional: bool,
) -> bool {
    fn branch(st: &Structured, block: usize) -> Option<(Value, usize, usize)> {
        let LaneExit::Branch { when, taken, not_taken } = st.exits[block] else { return None };
        match st.bools.node(when)? {
            (Atom::Lane(v), Bool::FALSE, Bool::TRUE) => Some((v, taken, not_taken)),
            (Atom::Lane(v), Bool::TRUE, Bool::FALSE) => Some((v, not_taken, taken)),
            _ => None,
        }
    }
    fn plain_move(p: &Program, st: &Structured, b: usize, reg: Reg, value: Value, constant: Operand) -> bool {
        p.blocks[b].insts.iter().zip(&st.guards[b]).any(|(i, guard)| {
            let Fields::Valu(v) = &i.fields else { return false };
            guard.is_none() && i.write(reg) == Some(value) && i.name() == "v_mov_b32"
                && v.src.as_slice() == [constant] && v.neg == 0 && v.abs == 0
                && v.opsel == 0 && v.sdwa.is_none() && v.dpp.is_none()
        })
    }
    fn eq_input(i: &Inst, mask: Value, left: Operand, right: Operand) -> bool {
        let Fields::Valu(v) = &i.fields else { return false };
        i.writes.iter().any(|&(_, value)| value == mask)
            && matches!(i.name(), "v_cmpx_eq_u32" | "v_cmp_eq_u32")
            && v.src.as_slice() == [left, right] && v.neg == 0 && v.abs == 0
            && v.opsel == 0 && v.sdwa.is_none() && v.dpp.is_none()
    }
    let proof = || {
        if !one_dimensional || !uniform[root] || !p.blocks[0].preds.is_empty() { return None; }
        let (test, yes, join) = branch(st, root)?;
        if yes != block || !uniform[join] || p.blocks[block].preds != [root]
            || !matches!(st.exits[block], LaneExit::Jump(t) if t == join)
        {
            return None;
        }
        let cmp = p.blocks[root].insts.iter().zip(&st.guards[root])
            .find(|(i, guard)| guard.is_none() && i.writes.iter().any(|&(_, v)| v == test))?.0;
        let Fields::Valu(v) = &cmp.fields else { return None };
        let [Operand::Const(193), Operand::Vector { reg, dwords: 1 }] = v.src.as_slice() else { return None };
        if !eq_input(cmp, test, Operand::Const(193), Operand::Vector { reg: *reg, dwords: 1 }) {
            return None;
        }
        let reg = Reg::V(*reg);
        let value = cmp.read(reg)?;
        let phi = p.blocks[root].phis.iter().find(|phi| phi.reg == reg && phi.value == value)?;
        if p.blocks[root].preds.len() != 2 || phi.args.len() != 2 { return None; }
        let (entry_test, arm, entry_join) = branch(st, 0)?;
        if entry_join != root || p.blocks[arm].preds != [0]
            || !matches!(st.exits[arm], LaneExit::Jump(t) if t == root)
        {
            return None;
        }
        p.blocks[0].insts.iter().zip(&st.guards[0]).find(|(i, guard)| {
            guard.is_none()
                && eq_input(i, entry_test, Operand::Const(128), Operand::Vector { reg: 0, dwords: 1 })
                && i.read(Reg::V(0)).is_some_and(|v| p.inputs.contains(&(Reg::V(0), v)))
        })?;
        for (&pred, &arg) in p.blocks[root].preds.iter().zip(&phi.args) {
            let (b, constant) = if pred == 0 { (0, Operand::Const(128)) }
                else if pred == arm { (arm, Operand::Const(193)) }
                else { return None };
            if !plain_move(p, st, b, reg, arg, constant) { return None; }
        }
        Some(())
    };
    proof().is_some()
}

/// Two accesses in one short lane-branch arm can use separate LDS ranges.
/// Fixed addresses additionally require at most one active thread in the group.
pub(super) fn independent_lds(
    p: &Program, st: &Structured, root: usize, block: usize, first: &Inst, second: &Inst, bytes: u32,
    one_dimensional: bool,
) -> bool {
    let proof = || {
        let LaneExit::Branch { when, taken, not_taken } = st.exits[root] else { return None };
        let Some((Atom::Lane(test), lo, hi)) = st.bools.node(when) else { return None };
        let join = match (lo, hi) {
            (Bool::FALSE, Bool::TRUE) if taken == block => not_taken,
            (Bool::TRUE, Bool::FALSE) if not_taken == block => taken,
            _ => return None,
        };
        if p.blocks[block].preds != [root] || !matches!(st.exits[block], LaneExit::Jump(t) if t == join) {
            return None;
        }
        let plain = |b: usize, value: Value| {
            p.blocks[b].insts.iter().zip(&st.guards[b])
                .find(|(i, guard)| guard.is_none() && i.writes.iter().any(|&(_, v)| v == value))
                .map(|(i, _)| i)
        };
        let operands = |inst: &Inst| {
            let Fields::Valu(v) = &inst.fields else { return None };
            let &[a, b] = v.src.as_slice() else { return None };
            (v.neg == 0 && v.abs == 0 && v.opsel == 0 && v.sdwa.is_none() && v.dpp.is_none()).then_some((a, b))
        };
        let value = |inst: &Inst, op| match op {
            Operand::Vector { reg, dwords: 1 } => inst.read(Reg::V(reg)),
            _ => None,
        };
        let imm = |op| match op {
            Operand::Const(c @ 128..=192) => Some(u32::from(c - 128)),
            Operand::Literal(v) => Some(v),
            _ => None,
        };
        let fixed_range = |i: &Inst| {
            let Fields::Ds { addr: Some(addr), offset0, offset1, gds: false, .. } = i.fields else { return None };
            let def = plain(block, value(i, addr)?)?;
            let Fields::Valu(v) = &def.fields else { return None };
            if def.name() != "v_mov_b32" || v.neg != 0 || v.abs != 0 || v.opsel != 0
                || v.sdwa.is_some() || v.dpp.is_some() || v.src.as_slice() != [Operand::Const(128)]
            {
                return None;
            }
            let (start, end) = match i.name() {
                "ds_read_b32" | "ds_write_b32" => {
                    let start = u32::from(offset0) | u32::from(offset1) << 8;
                    (start, start + 4)
                }
                "ds_read2_b32" | "ds_write2_b32" => {
                    (u32::from(offset0.min(offset1)) * 4, (u32::from(offset0.max(offset1)) + 1) * 4)
                }
                _ => return None,
            };
            (start % 4 == 0 && end <= bytes).then_some((start, end))
        };
        if let (Some((a, b)), Some((c, d))) = (fixed_range(first), fixed_range(second)) {
            let cmp = p.blocks.iter().enumerate().find_map(|(b, block)| {
                block.insts.iter().zip(&st.guards[b])
                    .find(|(i, guard)| guard.is_none() && i.writes.iter().any(|&(_, v)| v == test))
                    .map(|(i, _)| i)
            })?;
            let (zero, thread) = operands(cmp)?;
            let thread = value(cmp, thread)?;
            return Some(one_dimensional
                && matches!(cmp.name(), "v_cmpx_eq_u32" | "v_cmp_eq_u32")
                && imm(zero) == Some(0) && p.inputs.contains(&(Reg::V(0), thread))
                && (b <= c || d <= a));
        }
        if !first.name().starts_with("ds_write") || !second.name().starts_with("ds_write") {
            return None;
        }
        // The comparison itself may narrow EXEC; only the lane-id
        // definitions must execute unconditionally in its block.
        let cmp = p.blocks[root].insts.iter().find(|i| i.writes.iter().any(|&(_, v)| v == test))?;
        if !matches!(cmp.name(), "v_cmpx_gt_i32" | "v_cmp_gt_u32") {
            return None;
        }
        let (limit, lane) = operands(cmp)?;
        let count = imm(limit)?;
        if !(1..=64).contains(&count) {
            return None;
        }
        let lane = value(cmp, lane)?;
        let high = plain(root, lane)?;
        let (mask, low) = operands(high)?;
        let low = plain(root, value(high, low)?)?;
        if high.name() != "v_mbcnt_hi_u32_b32" || mask != Operand::Const(193)
            || low.name() != "v_mbcnt_lo_u32_b32" || operands(low)? != (Operand::Const(193), Operand::Const(128))
        {
            return None;
        }
        let range = |i: &Inst| {
            let Fields::Ds { addr: Some(addr), offset0, offset1, gds: false, .. } = i.fields else { return None };
            let def = plain(block, value(i, addr)?)?;
            let (shift, source) = operands(def)?;
            let shift = imm(shift)?;
            if def.name() != "v_lshlrev_b32" || value(def, source)? != lane || shift > 5 {
                return None;
            }
            let (start, end, align) = match i.name() {
                "ds_write_b16" => (u32::from(offset0) | u32::from(offset1) << 8, 2, 2),
                "ds_write_b32" => (u32::from(offset0) | u32::from(offset1) << 8, 4, 4),
                "ds_write2_b32" if offset0.abs_diff(offset1) == 1 => (u32::from(offset0.min(offset1)) * 4, 8, 4),
                _ => return None,
            };
            let last = (start + ((count - 1) << shift) + end).next_multiple_of(4);
            (start % align == 0 && (1 << shift) >= end && last <= bytes).then_some((start & !3, last))
        };
        let (a, b) = range(first)?;
        let (c, d) = range(second)?;
        Some(b <= c || d <= a)
    };
    proof().unwrap_or(false)
}

fn reconverged(succs: &[Vec<usize>], splits: &[bool]) -> Vec<bool> {
    let n = succs.len();
    let ipdom = post_dominators(succs);
    let mut out = vec![true; n];
    for (d, edges) in succs.iter().enumerate() {
        if edges.len() < 2 || !splits[d] {
            continue;
        }
        let stop = ipdom[d];
        let mut work = edges.clone();
        let mut seen = vec![false; n];
        while let Some(b) = work.pop() {
            if Some(b) == stop || seen[b] {
                continue;
            }
            seen[b] = true;
            out[b] = false;
            work.extend(&succs[b]);
        }
    }
    out
}

/// Immediate post-dominator per block (`None`: the virtual exit every
/// block without successors leads to, or a block that never exits).
fn post_dominators(succs: &[Vec<usize>]) -> Vec<Option<usize>> {
    let n = succs.len();
    let words = (n + 1).div_ceil(64);
    let exit = n;
    let full = {
        let mut f = vec![u64::MAX; words];
        let extra = words * 64 - (n + 1);
        if extra > 0 {
            f[words - 1] >>= extra;
        }
        f
    };
    // pdom[b]: blocks on every path from b to the exit, b included.
    let mut pdom = vec![full.clone(); n];
    let set = |s: &mut Vec<u64>, b: usize| s[b / 64] |= 1 << (b % 64);
    let has = |s: &[u64], b: usize| s[b / 64] >> (b % 64) & 1 != 0;
    let mut changed = true;
    while changed {
        changed = false;
        for b in (0..n).rev() {
            let mut next = if succs[b].is_empty() {
                let mut s = vec![0; words];
                set(&mut s, exit);
                s
            } else {
                let mut s = full.clone();
                for &t in &succs[b] {
                    for (w, x) in s.iter_mut().zip(&pdom[t]) {
                        *w &= x;
                    }
                }
                s
            };
            set(&mut next, b);
            if next != pdom[b] {
                pdom[b] = next;
                changed = true;
            }
        }
    }
    // The immediate one: the strict post-dominator that every other strict
    // post-dominator post-dominates.
    (0..n)
        .map(|b| {
            if has(&pdom[b], exit) && pdom[b] == full {
                // Never exits: no useful post-dominator.
                return None;
            }
            let strict: Vec<usize> = (0..n).filter(|&c| c != b && has(&pdom[b], c)).collect();
            strict
                .iter()
                .copied()
                .find(|&c| strict.iter().all(|&o| o == c || has(&pdom[c], o)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::post_dominators;

    #[test]
    fn post_dominators_of_an_if_and_a_loop() {
        // 0 -> {1, 2} -> 3 -> {3, 4}; 4 ends.
        let succs = vec![vec![1, 2], vec![3], vec![3], vec![3, 4], vec![]];
        let ipdom = post_dominators(&succs);
        assert_eq!(ipdom, vec![Some(3), Some(3), Some(3), Some(4), None]);
    }

    #[test]
    fn watchdog_return_prevents_reconvergence_after_the_loop() {
        // 0 hands control to a loop; 2 can take its watchdog exit at 4.
        let graph = [vec![1], vec![2, 3], vec![1, 4], vec![], vec![]];
        assert_eq!(super::reconverged(&graph, &[true; 6]), [true, false, false, false, false]);
    }
}
