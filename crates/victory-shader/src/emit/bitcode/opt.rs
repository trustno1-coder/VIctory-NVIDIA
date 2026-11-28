//! Clean-up of a finished module before it is written: the lowering emits
//! one instruction after another as the console code names them, and
//! nothing stands between it and the driver's compiler. Every pass here is
//! exact: the module computes the same values and has the same effects for
//! every input.
//!
//! - [`DEAD`]: instructions without an effect whose result nothing reads
//!   are left out (the lowering computes every dword of every register a
//!   console instruction writes; a guarded write reads the register's old
//!   value whether anything reads the new one or not).
//! - [`FOLD`]: integer arithmetic on constants is done here, and identities
//!   (`x + 0`, `x & -1`, a select between a value and itself, a cast there
//!   and back, a join of one value) give their operand. Float arithmetic is
//!   left alone: `x + 0.0` and `x * 1.0` are not `x` for every `x`.
//! - [`REUSE`]: an instruction without an effect whose operands are those
//!   of an earlier one that every path to it has run gives that one's
//!   result (a descriptor handle made again in each block, an index clamped
//!   again, a register read as a float again).
//! - [`FACTS`]: in the blocks only one side of a branch leads to, the
//!   branch's condition is that side's constant, and so is what the
//!   condition was made of (both terms of an `and` that held, the opposite
//!   comparison). The lowering branches on a lane's condition and then
//!   guards each instruction behind the branch with the same condition.
//! - [`LOADS`]: within a block, a read of the same place as an earlier read
//!   gives that one's result while no aliasing store, atomic, barrier or other
//!   call with an effect stands between them (another thread's unsynchronised
//!   store may fall between any two reads or before both: the title's code
//!   cannot tell which). Pointer roots distinguish thread-private (`space 0`),
//!   shared (`space 3`), constant tables, and DXIL resource memory so
//!   non-aliasing writes preserve unrelated reads, forward thread-private
//!   stores to loads, reuse consecutive shared reads, and drop overwritten
//!   dead local/shared stores within a block.
//! - [`LICM`]: pure, non-trapping loop-invariant instructions are hoisted out
//!   of natural loops to their dominating preheader (splitting a conditional
//!   entry edge lazily when needed).
//! - [`BARRIERS`]: redundant `dx.op.barrier(mode)` calls within a block are
//!   dropped when an earlier barrier covers all of their mode bits — either
//!   immediately across pure ALU, or between consecutive non-escaping LDS
//!   (`space 3`) read phases bounded by a covering group+TGSM barrier before
//!   and either a covering barrier or shader return after, with no intervening
//!   stores, atomics, resource/lane calls, or control-flow edges.
//!
//! Which passes run is `emit::shader_opt()` (`VICTORY_SHADER_OPT`), part of
//! the translation and so of the shader cache's key.

use std::collections::HashMap;

use super::{
    Attr, Bin, Cast, Const, Func, MdDef, Module, Op, Term, Ty, TypeDef, V, for_each_operand, pred,
    successors,
};

/// Dead-code elimination.
pub const DEAD: u32 = 1;
/// Constants folded, identities and algebraic/bitwise/compare/select rules removed.
pub const FOLD: u32 = 2;
/// One instruction for one value.
pub const REUSE: u32 = 4;
/// What a branch taken says about its condition.
pub const FACTS: u32 = 8;
/// One read for one place within a block, thread-private store-to-load
/// forwarding, non-aliasing shared read reuse, and overwritten dead store elimination.
pub const LOADS: u32 = 16;
/// Not a pass over the module: the lowering of a first-lane read in a
/// wave64 program run as two host waves skips the exchange that asks which
/// half has a lane where every lane is in EXEC (`lower.rs`,
/// `v_readfirstlane_b32`).
pub const FIRST: u32 = 32;
/// Not a pass over the module either: the lowering reads the dwords of one
/// console load (`s_buffer_load_dwordx2` to `x16` from an SRV, a
/// `global_load_dwordx2` to `x4`) by one call for every four of them, not
/// by a call each (`lower.rs`, `raw_dwords`). The same bytes are read.
/// From fxceless/victory-fork c36971b2. On an RX 7900 XTX it is worth nothing:
/// the yard's frame replayed turn by turn, dispatches and draws 18.47 -> 18.45
/// ms (12 loops) and 18.23 -> 18.16 (5 loops): the driver joins the loads.
pub const WIDE: u32 = 64;
/// Pure loop-invariant code motion to dominating loop preheaders.
pub const LICM: u32 = 128;
/// Redundant adjacent and read-after-read `dx.op.barrier(mode)` elimination within a block.
pub const BARRIERS: u32 = 256;
/// The passes over the module.
pub const MODULE: u32 = DEAD | FOLD | REUSE | FACTS | LOADS | LICM | BARRIERS;
/// Every pass.
pub const ALL: u32 = MODULE | FIRST | WIDE;
/// The passes that run when nothing says which: every one but [`WIDE`],
/// which no card has shown a gain for yet (`VICTORY_SHADER_OPT=511` asks
/// for it).
pub const DEFAULT: u32 = ALL & !WIDE;

/// What the instructions a module writes use, of the features a shader
/// declares by flag (the validator's "flags must match usage").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Usage {
    /// A wave or quad operation.
    pub wave_ops: bool,
    /// A rasterizer ordered view.
    pub rovs: bool,
    /// A 64-bit integer, as a result or an operand.
    pub int64: bool,
    /// A typed load of several components from a UAV.
    pub typed_uav_loads: bool,
    /// A handle from the view heap.
    pub heap_views: bool,
    /// A handle from the sampler heap.
    pub heap_samplers: bool,
    /// A 64-bit atomic.
    pub atomic64: bool,
}

/// What makes two instructions without an effect the same value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Bin(u8, V, V),
    Cast(u8, Ty, V),
    Cmp(u8, V, V),
    Select(V, V, V),
    Extract(V, u32),
    Call(u32, Vec<V>),
    Gep(Ty, V, Vec<V>),
    Load(V),
}

/// The blocks reached from the entry, each after the blocks that dominate
/// it, and each block's children in the dominator tree.
struct Tree {
    order: Vec<usize>,
    children: Vec<Vec<usize>>,
    /// Per block: the reached blocks that branch to it, once per edge.
    preds: Vec<Vec<usize>>,
    /// Immediate dominator per block (`usize::MAX` if unreachable; `0` for entry).
    idom: Vec<usize>,
}

/// Whether block `a` dominates block `b` in the dominator tree `idom`.
fn dominates(idom: &[usize], a: usize, b: usize) -> bool {
    if a >= idom.len() || b >= idom.len() || idom[a] == usize::MAX || idom[b] == usize::MAX {
        return false;
    }
    let mut cur = b;
    loop {
        if cur == a {
            return true;
        }
        if cur == 0 {
            return false;
        }
        cur = idom[cur];
    }
}

/// The comparison that holds exactly when `p` does not (floats 0-15,
/// integers 32-41).
fn opposite(p: u8) -> u8 {
    match p {
        0..=15 => 15 - p,
        pred::EQ => pred::NE,
        pred::NE => pred::EQ,
        pred::IUGT => pred::IULE,
        pred::IULE => pred::IUGT,
        pred::IUGE => pred::IULT,
        pred::IULT => pred::IUGE,
        pred::SGT => pred::SLE,
        pred::SLE => pred::SGT,
        pred::SGE => pred::SLT,
        _ => pred::SGE,
    }
}

/// Canonical `(pred, a, b)` for comparison equivalence: symmetric predicates
/// order operands `a <= b`, and greater-than predicates flip to less-than by
/// swapping `(a, b)`. Preserves IEEE-754 NaN semantics for every float
/// predicate (ordered stays ordered, unordered stays unordered).
fn canonical_cmp(p: u8, a: V, b: V) -> (u8, V, V) {
    match p {
        pred::EQ
        | pred::NE
        | pred::OEQ
        | pred::UEQ
        | pred::ONE
        | pred::UNE
        | pred::ORD
        | pred::UNO => {
            if b < a {
                (p, b, a)
            } else {
                (p, a, b)
            }
        }
        pred::IUGT => (pred::IULT, b, a),
        pred::IUGE => (pred::IULE, b, a),
        pred::SGT => (pred::SLT, b, a),
        pred::SGE => (pred::SLE, b, a),
        pred::OGT => (pred::OLT, b, a),
        pred::OGE => (pred::OLE, b, a),
        pred::UGT => (pred::ULT, b, a),
        pred::UGE => (pred::ULE, b, a),
        _ => (p, a, b),
    }
}

/// Memory domain for a pointer traced through GEP chains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PtrRoot {
    /// Immutable constant global (`init.is_some()`).
    ConstGlobal(u32),
    /// Non-escaping thread-private global (`space == 0`, `init.is_none()`).
    PrivateGlobal(u32),
    /// Non-escaping thread-group shared global (`space == 3`, LDS).
    SharedGlobal(u32),
    /// Escaped global or non-global pointer.
    Unknown,
}

/// Constant canonical scalar-leaf vs dynamic GEP path from a [`PtrRoot`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum PtrPath {
    /// Canonical in-bounds root-to-scalar-leaf path with its exact leaf type.
    Const(Ty, Vec<u64>),
    Dynamic,
}

/// Fine-grained classification of `Effect::Writes` `dx.op.*` calls for
/// non-aliasing tracking and barrier elimination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteKind {
    /// Stage output sinks (`storeOutput*`, `emitIndices`, etc.): never alias
    /// buffers, textures, LDS, private memory, or wave lanes.
    OutputOnly,
    /// Resource buffer/texture stores and atomics: invalidate DXIL resource
    /// reads, never alias LDS, non-escaping private memory, or wave lanes.
    ResourceMemory,
    /// `dx.op.barrier`: synchronises shared/UAV memory according to mode bits;
    /// never aliases non-escaping thread-private memory.
    Barrier,
    /// `dx.op.discard` or any unrecognised side-effecting call.
    All,
}

fn write_kind(f: &Func) -> WriteKind {
    let name = f.name.strip_prefix("dx.op.").unwrap_or(&f.name);
    if name == "barrier" {
        WriteKind::Barrier
    } else if name.starts_with("storeOutput")
        || name.starts_with("storeVertexOutput")
        || name.starts_with("storePrimitiveOutput")
        || matches!(
            name,
            "emitIndices" | "setMeshOutputCounts" | "emitStream" | "cutStream"
        )
    {
        WriteKind::OutputOnly
    } else if name.starts_with("bufferStore")
        || name.starts_with("rawBufferStore")
        || name.starts_with("textureStore")
        || name.starts_with("atomicBinOp")
        || name.starts_with("atomicCompareExchange")
    {
        WriteKind::ResourceMemory
    } else {
        WriteKind::All
    }
}

/// Tracked values of non-escaping thread-private globals (`space == 0`).
#[derive(Default, Clone)]
struct PrivateVals {
    const_vals: HashMap<(u32, Vec<u64>), V>,
    dyn_vals: HashMap<(u32, V, Ty), V>,
}

/// Intra-block `dx.op.barrier` tracking for [`BARRIERS`]: eliminates both
/// immediately adjacent covered barriers and read-after-read barriers between
/// non-escaping LDS (`space == 3`) loads when bounded on the left by a covering
/// group+TGSM barrier (`mode & 9 == 9`) and on the right by either a covering
/// group+TGSM barrier or `Term::Ret` (when `mid_mode` has no UAV fence bits),
/// with no intervening stores, atomics, resource/wave calls, or branches.
#[derive(Default)]
struct BarrierState {
    /// Mode of the last barrier when no memory or lane op has run since it.
    adj: Option<u64>,
    rar: RarState,
}

#[derive(Default, Clone, Copy)]
enum RarState {
    #[default]
    None,
    /// Saw `B_anchor(anchor_mode)` with `(anchor_mode & 9) == 9`.
    AfterAnchor {
        anchor_mode: u64,
        saw_lds_load: bool,
    },
    /// Saw `B_anchor -> [>=1 LDS load] -> B_mid(mid_mode)` at `kept[mid_pos]`.
    AfterMid {
        anchor_mode: u64,
        mid_mode: u64,
        mid_pos: usize,
        saw_lds_load: bool,
    },
    /// Saw `B_anchor -> [>=1 LDS load] -> B_mid -> [>=1 LDS load]` where
    /// `(mid_mode & 1) != 0` and `mid_mode` has no UAV fence bits, followed
    /// only by non-atomic UAV or stage-output stores; drops `B_mid` if the
    /// block terminates in `Term::Ret`.
    TailBeforeRet {
        mid_pos: usize,
    },
}

impl BarrierState {
    fn reset(&mut self) {
        self.adj = None;
        self.rar = RarState::None;
    }

    fn on_load(&mut self, root: PtrRoot) {
        self.adj = None;
        match root {
            PtrRoot::ConstGlobal(_) | PtrRoot::PrivateGlobal(_) => {}
            PtrRoot::SharedGlobal(_) => match &mut self.rar {
                RarState::AfterAnchor { saw_lds_load, .. }
                | RarState::AfterMid { saw_lds_load, .. } => *saw_lds_load = true,
                RarState::TailBeforeRet { .. } | RarState::None => {}
            },
            PtrRoot::Unknown => self.rar = RarState::None,
        }
    }

    fn on_non_lds_sink_store(&mut self) {
        self.adj = None;
        self.rar = match self.rar {
            RarState::AfterMid {
                mid_mode,
                mid_pos,
                saw_lds_load: true,
                ..
            } if (mid_mode & 1) != 0 && mid_mode & !(1 | 8) == 0 => {
                RarState::TailBeforeRet { mid_pos }
            }
            RarState::TailBeforeRet { mid_pos } => RarState::TailBeforeRet { mid_pos },
            _ => RarState::None,
        };
    }
}

/// A natural loop detected via the dominator tree.
struct NaturalLoop {
    header: usize,
    in_loop: Vec<bool>,
    size: usize,
}

/// What holds in the block being walked: values that are a known bit there,
/// and comparisons (by their operands) with a known outcome.
#[derive(Default)]
struct Facts {
    bits: HashMap<V, bool>,
    compares: HashMap<(u8, V, V), bool>,
}

/// The facts a block added, to forget when its part of the tree is done.
#[derive(Default)]
struct Learned {
    bits: Vec<V>,
    compares: Vec<(u8, V, V)>,
}

fn mask(bits: u32) -> u64 {
    if bits >= 64 { !0 } else { (1 << bits) - 1 }
}

/// `x` of `bits` bits as a signed number.
fn signed(bits: u32, x: u64) -> i64 {
    if bits < 64 && x >> (bits - 1) & 1 != 0 {
        (x | !mask(bits)) as i64
    } else {
        x as i64
    }
}

/// What `v` stands for after the replacements so far.
fn resolve(repl: &[Option<V>], mut v: V) -> V {
    while let V::Inst(i) = v {
        match repl[i as usize] {
            Some(r) => v = r,
            None => break,
        }
    }
    v
}

/// Replaces every value `op` reads by `f` of it.
fn map_operands(op: &mut Op, mut f: impl FnMut(V) -> V) {
    match op {
        Op::Bin(_, a, b) | Op::Cmp(_, a, b) | Op::Store(a, b) | Op::AtomicRmw(_, a, b) => {
            *a = f(*a);
            *b = f(*b);
        }
        Op::Cast(_, v) | Op::Extract(v, _) | Op::Load(v) => *v = f(*v),
        Op::Select(a, b, c) | Op::CmpXchg(a, b, c) => {
            *a = f(*a);
            *b = f(*b);
            *c = f(*c);
        }
        Op::Call(_, args) => args.iter_mut().for_each(|a| *a = f(*a)),
        Op::Phi(inc) => inc.iter_mut().for_each(|x| x.0 = f(x.0)),
        Op::Gep(_, p, idx) => {
            *p = f(*p);
            idx.iter_mut().for_each(|i| *i = f(*i));
        }
    }
}

/// What a call does besides returning its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Effect {
    /// Nothing, and the result depends on the arguments alone for the whole
    /// run of the shader (arithmetic, handles, inputs, constant buffers,
    /// resource sizes).
    Pure,
    /// Nothing, but the result depends on memory a store may change
    /// (buffer and texture reads).
    Reads,
    /// Nothing, but the result depends on which lanes run the instruction
    /// (wave and quad operations).
    Lanes,
    /// Stores, atomics, barriers, outputs, discards: always kept.
    Writes,
}

pub(super) fn effect(f: &Func) -> Effect {
    let name = f.name.strip_prefix("dx.op.").unwrap_or(&f.name);
    match f.attr {
        Some(Attr::ReadNone) if name.starts_with("deriv") => Effect::Lanes,
        Some(Attr::ReadNone) => Effect::Pure,
        // A shader cannot write a constant buffer or resize a resource.
        Some(Attr::ReadOnly)
            if name.starts_with("cbufferLoadLegacy") || name.starts_with("getDimensions") =>
        {
            Effect::Pure
        }
        Some(Attr::ReadOnly) if name.starts_with("wave") || name.starts_with("quad") => {
            Effect::Lanes
        }
        Some(Attr::ReadOnly) => Effect::Reads,
        // Declared with the attribute of calls that have effects, and have
        // none: they read the other lanes' values.
        _ if name.starts_with("wave") || name.starts_with("quad") || name.starts_with("deriv") => {
            Effect::Lanes
        }
        _ => Effect::Writes,
    }
}

impl Module {
    /// Runs the clean-up passes named in `passes` ([`ALL`]: every one).
    pub(crate) fn optimise(&mut self, passes: u32) {
        if passes & (FOLD | REUSE | FACTS | LOADS | LICM | BARRIERS) != 0 {
            // Values merged in one round can make two joins the same in
            // the next (a loop's registers that only go round), a branch
            // that goes one way leaves blocks with one way in, and LICM
            // exposes hoisted pure values to REUSE/FOLD outside the loop.
            for _ in 0..4 {
                let mut changed = false;
                if passes & (FOLD | REUSE | FACTS | LOADS | BARRIERS) != 0 {
                    changed |= self.merge(passes);
                }
                if passes & FOLD != 0 {
                    changed |= self.prune();
                }
                if passes & LICM != 0 {
                    changed |= self.licm();
                }
                if !changed {
                    break;
                }
            }
        }
        if passes & DEAD != 0 {
            self.drop_dead();
        }
    }

    /// Per function: what a call of it does.
    pub(super) fn effects(&self) -> Vec<Effect> {
        self.funcs.iter().map(effect).collect()
    }

    /// The blocks reached from the entry.
    pub(super) fn reached(&self) -> Vec<bool> {
        let mut seen = vec![false; self.blocks.len()];
        if self.blocks.is_empty() {
            return seen;
        }
        let mut stack = vec![0usize];
        seen[0] = true;
        while let Some(b) = stack.pop() {
            for &s in successors(self.blocks[b].term).iter() {
                if !seen[s] {
                    seen[s] = true;
                    stack.push(s);
                }
            }
        }
        seen
    }

    fn dominator_tree(&self) -> Tree {
        let n = self.blocks.len();
        let succs = |b: usize| successors(self.blocks[b].term);
        // Reverse postorder, as `write` lays the blocks out.
        let mut seen = vec![false; n];
        let mut post = Vec::new();
        let mut stack = vec![(0usize, 0usize)];
        seen[0] = true;
        while let Some(top) = stack.last_mut() {
            let (b, k) = *top;
            top.1 += 1;
            match succs(b).get(k) {
                Some(&s) if !seen[s] => {
                    seen[s] = true;
                    stack.push((s, 0));
                }
                Some(_) => {}
                None => {
                    post.push(b);
                    stack.pop();
                }
            }
        }
        post.reverse();
        let order = post;
        let mut number = vec![usize::MAX; n];
        for (k, &b) in order.iter().enumerate() {
            number[b] = k;
        }
        let mut preds = vec![Vec::new(); n];
        for &b in &order {
            for &s in succs(b).iter() {
                preds[s].push(b);
            }
        }
        // Immediate dominators (Cooper, Harvey and Kennedy's iteration).
        let mut idom = vec![usize::MAX; n];
        idom[0] = 0;
        let mut changed = true;
        while changed {
            changed = false;
            for &b in &order[1..] {
                let mut new = usize::MAX;
                for &p in &preds[b] {
                    if idom[p] == usize::MAX {
                        continue;
                    }
                    if new == usize::MAX {
                        new = p;
                        continue;
                    }
                    let (mut x, mut y) = (p, new);
                    while x != y {
                        while number[x] > number[y] {
                            x = idom[x];
                        }
                        while number[y] > number[x] {
                            y = idom[y];
                        }
                    }
                    new = x;
                }
                if idom[b] != new {
                    idom[b] = new;
                    changed = true;
                }
            }
        }
        let mut children = vec![Vec::new(); n];
        for &b in &order[1..] {
            children[idom[b]].push(b);
        }
        Tree {
            order,
            children,
            preds,
            idom,
        }
    }

    /// Notes that `v` (an i1) is `holds` in the block being walked, and what
    /// follows from it for the values `v` was made of.
    fn learn(
        &self,
        repl: &[Option<V>],
        v: V,
        holds: bool,
        facts: &mut Facts,
        learned: &mut Learned,
        depth: u32,
    ) {
        let v = resolve(repl, v);
        let V::Inst(i) = v else { return };
        if facts.bits.contains_key(&v) {
            return;
        }
        facts.bits.insert(v, holds);
        learned.bits.push(v);
        if depth == 0 {
            return;
        }
        match self.insts[i as usize].op {
            // Both terms of an `and` that holds hold; neither of an `or`
            // that does not.
            Op::Bin(Bin::And, a, b) if holds => {
                self.learn(repl, a, true, facts, learned, depth - 1);
                self.learn(repl, b, true, facts, learned, depth - 1);
            }
            Op::Bin(Bin::Or, a, b) if !holds => {
                self.learn(repl, a, false, facts, learned, depth - 1);
                self.learn(repl, b, false, facts, learned, depth - 1);
            }
            Op::Bin(Bin::Xor, a, b) => {
                let (a, b) = (resolve(repl, a), resolve(repl, b));
                let (x, k) = match (self.int_k(a), self.int_k(b)) {
                    (None, Some((1, k))) => (a, k),
                    (Some((1, k)), None) => (b, k),
                    _ => return,
                };
                self.learn(repl, x, holds != (k != 0), facts, learned, depth - 1);
            }
            // The comparison itself and its opposite, both canonicalised so
            // flipped operands (`b > a` vs `a < b`) hit the same entry.
            Op::Cmp(p, a, b) => {
                let (a, b) = (resolve(repl, a), resolve(repl, b));
                for (key, val) in [
                    (canonical_cmp(p, a, b), holds),
                    (canonical_cmp(opposite(p), a, b), !holds),
                ] {
                    if let std::collections::hash_map::Entry::Vacant(e) =
                        facts.compares.entry(key)
                    {
                        e.insert(val);
                        learned.compares.push(key);
                    }
                }
            }
            _ => {}
        }
    }

    fn int_bits(&self, ty: Ty) -> Option<u32> {
        match self.types[ty.0 as usize] {
            TypeDef::Int(bits) => Some(bits),
            _ => None,
        }
    }

    /// An integer constant's width and value (the bits of that width).
    pub(crate) fn int_k(&self, v: V) -> Option<(u32, u64)> {
        let V::Const(i) = v else { return None };
        let (ty, Const::Int(x)) = &self.consts[i as usize] else {
            return None;
        };
        let bits = self.int_bits(*ty)?;
        Some((bits, *x as u64 & mask(bits)))
    }

    /// The bits of a 32-bit float that is a constant, given as one or as a
    /// cast of an integer constant.
    pub(crate) fn float_k(&self, v: V) -> Option<f32> {
        let (v, cast) = match v {
            V::Inst(i) => match self.insts[i as usize].op {
                Op::Cast(Cast::Bitcast, x) => (x, true),
                _ => return None,
            },
            _ => (v, false),
        };
        let V::Const(i) = v else { return None };
        match &self.consts[i as usize].1 {
            Const::Float(bits) if !cast => Some(f32::from_bits(*bits)),
            Const::Int(_) if cast => {
                let (bits, x) = self.int_k(v)?;
                (bits == 32).then_some(f32::from_bits(x as u32))
            }
            _ => None,
        }
    }

    /// For a float that is a number and not an infinity whatever the program
    /// reads: a power of two its magnitude does not pass.
    pub(crate) fn bounded(&self, v: V, depth: u32) -> Option<f32> {
        if let Some(k) = self.float_k(v) {
            return k.is_finite().then(|| k.abs().max(1e-30).log2().ceil());
        }
        let V::Inst(i) = v else { return None };
        let depth = depth.checked_sub(1)?;
        let of = |v: V| self.bounded(v, depth);
        // (A power that overflows, or might with rounding, is no bound.)
        let held = |b: f32| (b < 126.0).then_some(b);
        match &self.insts[i as usize].op {
            Op::Cast(Cast::UiToFp | Cast::SiToFp, _) => Some(32.0),
            Op::Cast(Cast::Bitcast, x) => match *x {
                V::Inst(j) => match self.insts[j as usize].op {
                    Op::Cast(Cast::Bitcast, y) if self.type_of(y) == self.type_of(v) => of(y),
                    _ => None,
                },
                _ => None,
            },
            Op::Bin(Bin::Mul, a, b) => held(of(*a)? + of(*b)?),
            Op::Bin(Bin::Add | Bin::Sub, a, b) => held(of(*a)?.max(of(*b)?) + 1.0),
            Op::Select(_, a, b) => Some(of(*a)?.max(of(*b)?)),
            Op::Call(_, args) => {
                let (_, code) = self.int_k(*args.first()?)?;
                match (code, &args[1..]) {
                    // Saturated, a fraction, a cosine or sine (of a number).
                    (7, [_]) => Some(0.0),
                    (6, [x]) => of(*x),
                    (22 | 12 | 13, [x]) => of(*x).map(|_| 0.0),
                    (26..=29, [x]) => held(of(*x)? + 1.0),
                    // Of two numbers; or one of them the other way round: the
                    // minimum (36) of a constant and a maximum of another.
                    (35 | 36, [a, b]) => match (of(*a), of(*b)) {
                        (Some(x), Some(y)) => Some(x.max(y)),
                        _ => {
                            let other = if code == 35 { 36 } else { 35 };
                            let (inner, k) = if self.float_k(*b).is_some() { (*a, *b) } else { (*b, *a) };
                            let k = self.float_k(k).and_then(|_| of(k))?;
                            let V::Inst(j) = inner else { return None };
                            let Op::Call(_, inner) = &self.insts[j as usize].op else { return None };
                            let (_, code) = self.int_k(*inner.first()?)?;
                            let low = inner[1..].iter().find(|x| self.float_k(**x).is_some())?;
                            (code == other).then_some(())?;
                            Some(k.max(of(*low)?))
                        }
                    },
                    (46, [a, b, c]) => held((of(*a)? + of(*b)?).max(of(*c)?) + 1.0),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// The constant `x` of the integer type `ty`, kept sign-extended as
    /// [`Module::i32`] and [`Module::bool`] keep theirs.
    fn k(&mut self, ty: Ty, bits: u32, x: u64) -> V {
        self.int_const(ty, signed(bits, x & mask(bits)))
    }

    /// If `v` is `xor(x, true)` or `xor(true, x)` on `i1`, returns `Some(x)`.
    fn is_bool_not(&self, v: V) -> Option<V> {
        let V::Inst(j) = v else { return None };
        let inst = &self.insts[j as usize];
        if self.int_bits(inst.ty) != Some(1) {
            return None;
        }
        let Op::Bin(Bin::Xor, a, b) = inst.op else {
            return None;
        };
        match (self.int_k(a), self.int_k(b)) {
            (None, Some((1, 1))) => Some(a),
            (Some((1, 1)), None) => Some(b),
            _ => None,
        }
    }

    /// Simplifies `xor(x, true)` for an `i1` instruction `i`: returns a direct
    /// replacement value when possible, or rewrites `self.insts[i].op` in place
    /// to an inverted comparison or `xor(x, true)`.
    fn negate_bool_in_place(&mut self, i: u32, x: V, changed: &mut bool) -> Option<V> {
        if let Some((1, k)) = self.int_k(x) {
            return Some(self.bool(k == 0));
        }
        if let Some(orig) = self.is_bool_not(x) {
            return Some(orig);
        }
        if let V::Inst(j) = x
            && let Op::Cmp(p, a, b) = self.insts[j as usize].op
            && (p <= pred::TRUE || (pred::EQ..=pred::SLE).contains(&p))
        {
            let opp = opposite(p);
            if opp == pred::FALSE {
                return Some(self.bool(false));
            }
            if opp == pred::TRUE {
                return Some(self.bool(true));
            }
            // Never emit `fcmp one` (`pred::ONE`): the AMD D3D12 driver
            // treats it as `une`.
            if opp != pred::ONE {
                self.insts[i as usize].op = Op::Cmp(opp, a, b);
                *changed = true;
                return None;
            }
        }
        let yes = self.bool(true);
        let already = matches!(
            self.insts[i as usize].op,
            Op::Bin(Bin::Xor, a, b) if (a == x && b == yes) || (a == yes && b == x)
        );
        if !already {
            self.insts[i as usize].op = Op::Bin(Bin::Xor, x, yes);
            *changed = true;
        }
        None
    }

    /// If `v` is an integer instruction of form `(x op c)` (with either operand
    /// order for commutative `op`), returns `Some((x, c))`.
    fn bin_with_const(&self, v: V, want: Bin) -> Option<(V, u64)> {
        let V::Inst(j) = v else { return None };
        let Op::Bin(op, a, b) = self.insts[j as usize].op else {
            return None;
        };
        if op != want {
            return None;
        }
        match (self.int_k(a), self.int_k(b)) {
            (None, Some((_, k))) => Some((a, k)),
            (Some((_, k)), None)
                if matches!(op, Bin::Add | Bin::Mul | Bin::And | Bin::Or | Bin::Xor) =>
            {
                Some((b, k))
            }
            _ => None,
        }
    }

    /// If `v` is a two-valued integer driven by an `i1` condition `c`, returns
    /// `(c, value_when_true, value_when_false)` masked to `bits`.
    fn two_valued_int(&self, v: V, bits: u32) -> Option<(V, u64, u64)> {
        if bits == 1 && self.int_k(v).is_none() {
            return Some((v, 1, 0));
        }
        let V::Inst(j) = v else { return None };
        match self.insts[j as usize].op {
            Op::Select(c, t, f) => {
                let (_, k1) = self.int_k(t)?;
                let (_, k2) = self.int_k(f)?;
                Some((c, k1, k2))
            }
            Op::Cast(Cast::ZExt, b) if self.int_bits(self.type_of(b)) == Some(1) => {
                Some((b, 1, 0))
            }
            Op::Cast(Cast::SExt, b) if self.int_bits(self.type_of(b)) == Some(1) => {
                Some((b, mask(bits), 0))
            }
            _ => None,
        }
    }

    /// The value instruction `i` is known to have without it, if any: a
    /// constant or an earlier value. May instead give `i` simpler operands
    /// (`changed` says so).
    fn simplify(&mut self, i: u32, changed: &mut bool) -> Option<V> {
        let ty = self.insts[i as usize].ty;
        match self.insts[i as usize].op {
            Op::Bin(op, a, b) => {
                // Floats: `x + 0.0`, `x * 1.0` and the like are not `x` for
                // every `x` (a negative zero, a signalling NaN).
                let bits = self.int_bits(ty)?;
                let (ka, kb) = (self.int_k(a).map(|k| k.1), self.int_k(b).map(|k| k.1));
                let all = mask(bits);
                if let (Some(x), Some(y)) = (ka, kb) {
                    // A shift by the width or more and a division by zero
                    // (or signed MIN / -1) have no defined result: left as they are.
                    let amount = u32::try_from(y).ok().filter(|&y| y < bits);
                    let r = match op {
                        Bin::Add => x.wrapping_add(y),
                        Bin::Sub => x.wrapping_sub(y),
                        Bin::Mul => x.wrapping_mul(y),
                        Bin::And => x & y,
                        Bin::Or => x | y,
                        Bin::Xor => x ^ y,
                        Bin::Shl => x << amount?,
                        Bin::LShr => x >> amount?,
                        Bin::AShr => (signed(bits, x) >> amount?) as u64,
                        Bin::UDiv if y != 0 => x / y,
                        Bin::SDiv
                            if y != 0
                                && !(signed(bits, y) == -1 && x == (1u64 << (bits - 1))) =>
                        {
                            (signed(bits, x) / signed(bits, y)) as u64
                        }
                        Bin::URem if y != 0 => x % y,
                        _ => return None,
                    };
                    return Some(self.k(ty, bits, r));
                }
                // Direct single-instruction identities.
                match (op, ka, kb) {
                    (
                        Bin::Add | Bin::Sub | Bin::Or | Bin::Xor | Bin::Shl | Bin::LShr | Bin::AShr,
                        _,
                        Some(0),
                    ) => return Some(a),
                    (Bin::Add | Bin::Or | Bin::Xor, Some(0), _) => return Some(b),
                    (Bin::Mul | Bin::UDiv, _, Some(1)) => return Some(a),
                    (Bin::SDiv, _, Some(1)) if bits > 1 => return Some(a),
                    (Bin::Mul, Some(1), _) => return Some(b),
                    (Bin::Mul | Bin::And, _, Some(0)) => return Some(b),
                    (Bin::Mul | Bin::And, Some(0), _) => return Some(a),
                    (Bin::URem, _, Some(1)) => return Some(self.k(ty, bits, 0)),
                    (Bin::And, _, Some(m)) if m == all => return Some(a),
                    (Bin::And, Some(m), _) if m == all => return Some(b),
                    (Bin::Or, _, Some(m)) if m == all => return Some(b),
                    (Bin::Or, Some(m), _) if m == all => return Some(a),
                    (Bin::And | Bin::Or, _, _) if a == b => return Some(a),
                    (Bin::Sub | Bin::Xor, _, _) if a == b => return Some(self.k(ty, bits, 0)),
                    _ => {}
                }
                // Algebraic, bitwise, shift-mask, and power-of-two strength reduction.
                match op {
                    Bin::Xor => {
                        if bits == 1 {
                            match (ka, kb) {
                                (None, Some(1)) => return self.negate_bool_in_place(i, a, changed),
                                (Some(1), None) => return self.negate_bool_in_place(i, b, changed),
                                _ => {}
                            }
                        }
                        let const_pair = match (ka, kb) {
                            (None, Some(c2)) => Some((a, c2)),
                            (Some(c2), None) => Some((b, c2)),
                            _ => None,
                        };
                        if let Some((other, c2)) = const_pair
                            && let Some((x, c1)) = self.bin_with_const(other, Bin::Xor)
                        {
                            let c = (c1 ^ c2) & all;
                            if c == 0 {
                                return Some(x);
                            }
                            let ck = self.k(ty, bits, c);
                            self.insts[i as usize].op = Op::Bin(Bin::Xor, x, ck);
                            *changed = true;
                            return None;
                        }
                        // `(x ^ y) ^ y => x` and symmetric forms.
                        if let V::Inst(j) = a
                            && let Op::Bin(Bin::Xor, x, y) = self.insts[j as usize].op
                        {
                            if y == b {
                                return Some(x);
                            }
                            if x == b {
                                return Some(y);
                            }
                        }
                        if let V::Inst(j) = b
                            && let Op::Bin(Bin::Xor, x, y) = self.insts[j as usize].op
                        {
                            if y == a {
                                return Some(x);
                            }
                            if x == a {
                                return Some(y);
                            }
                        }
                        None
                    }
                    Bin::Add => {
                        let const_pair = match (ka, kb) {
                            (None, Some(c2)) => Some((a, c2)),
                            (Some(c2), None) => Some((b, c2)),
                            _ => None,
                        };
                        if let Some((other, c2)) = const_pair {
                            if let Some((x, c1)) = self.bin_with_const(other, Bin::Add) {
                                let c = c1.wrapping_add(c2) & all;
                                if c == 0 {
                                    return Some(x);
                                }
                                let ck = self.k(ty, bits, c);
                                self.insts[i as usize].op = Op::Bin(Bin::Add, x, ck);
                                *changed = true;
                                return None;
                            }
                            if let Some((x, c1)) = self.bin_with_const(other, Bin::Sub) {
                                let d = c2.wrapping_sub(c1) & all;
                                if d == 0 {
                                    return Some(x);
                                }
                                let dk = self.k(ty, bits, d);
                                self.insts[i as usize].op = Op::Bin(Bin::Add, x, dk);
                                *changed = true;
                                return None;
                            }
                        }
                        // `(x - y) + y => x` and `y + (x - y) => x`.
                        if let V::Inst(j) = a
                            && let Op::Bin(Bin::Sub, x, y) = self.insts[j as usize].op
                            && y == b
                        {
                            return Some(x);
                        }
                        if let V::Inst(j) = b
                            && let Op::Bin(Bin::Sub, x, y) = self.insts[j as usize].op
                            && y == a
                        {
                            return Some(x);
                        }
                        None
                    }
                    Bin::Sub => {
                        if let Some(c2) = kb {
                            if let Some((x, c1)) = self.bin_with_const(a, Bin::Sub) {
                                let c = c1.wrapping_add(c2) & all;
                                if c == 0 {
                                    return Some(x);
                                }
                                let ck = self.k(ty, bits, c);
                                self.insts[i as usize].op = Op::Bin(Bin::Sub, x, ck);
                                *changed = true;
                                return None;
                            }
                            if let Some((x, c1)) = self.bin_with_const(a, Bin::Add) {
                                let d = c1.wrapping_sub(c2) & all;
                                if d == 0 {
                                    return Some(x);
                                }
                                let dk = self.k(ty, bits, d);
                                self.insts[i as usize].op = Op::Bin(Bin::Add, x, dk);
                                *changed = true;
                                return None;
                            }
                        }
                        // `(x + y) - y => x` and `(y + x) - y => x`.
                        if let V::Inst(j) = a
                            && let Op::Bin(Bin::Add, x, y) = self.insts[j as usize].op
                        {
                            if y == b {
                                return Some(x);
                            }
                            if x == b {
                                return Some(y);
                            }
                        }
                        None
                    }
                    Bin::And => {
                        let const_pair = match (ka, kb) {
                            (None, Some(c2)) => Some((a, c2)),
                            (Some(c2), None) => Some((b, c2)),
                            _ => None,
                        };
                        if let Some((other, c2)) = const_pair {
                            if let Some((x, c1)) = self.bin_with_const(other, Bin::And) {
                                let c = c1 & c2;
                                if c == 0 {
                                    return Some(self.k(ty, bits, 0));
                                }
                                if c == c1 {
                                    return Some(other);
                                }
                                let ck = self.k(ty, bits, c);
                                self.insts[i as usize].op = Op::Bin(Bin::And, x, ck);
                                *changed = true;
                                return None;
                            }
                            // `and(zext i1 b, c)` is `zext i1 b` when bit 0 is set, else 0.
                            if let V::Inst(z) = other
                                && let Op::Cast(Cast::ZExt, src) = self.insts[z as usize].op
                                && self.int_bits(self.type_of(src)) == Some(1)
                            {
                                return Some(if c2 & 1 != 0 {
                                    other
                                } else {
                                    self.k(ty, bits, 0)
                                });
                            }
                        }
                        None
                    }
                    Bin::Or => {
                        let const_pair = match (ka, kb) {
                            (None, Some(c2)) => Some((a, c2)),
                            (Some(c2), None) => Some((b, c2)),
                            _ => None,
                        };
                        if let Some((other, c2)) = const_pair
                            && let Some((x, c1)) = self.bin_with_const(other, Bin::Or)
                        {
                            let c = (c1 | c2) & all;
                            if c == all {
                                return Some(self.k(ty, bits, all));
                            }
                            if c == c1 {
                                return Some(other);
                            }
                            let ck = self.k(ty, bits, c);
                            self.insts[i as usize].op = Op::Bin(Bin::Or, x, ck);
                            *changed = true;
                            return None;
                        }
                        None
                    }
                    Bin::Shl | Bin::LShr | Bin::AShr => {
                        if let Some(c2) = kb
                            && c2 < u64::from(bits)
                            && let Some((x, c1)) = self.bin_with_const(a, op)
                            && c1 < u64::from(bits)
                            && let Some(sum) = c1.checked_add(c2)
                        {
                            if sum < u64::from(bits) {
                                let sk = self.k(ty, bits, sum);
                                self.insts[i as usize].op = Op::Bin(op, x, sk);
                                *changed = true;
                                return None;
                            }
                            if matches!(op, Bin::Shl | Bin::LShr) {
                                return Some(self.k(ty, bits, 0));
                            }
                            let max_s = u64::from(bits - 1);
                            if c1 == max_s {
                                return Some(a);
                            }
                            let sk = self.k(ty, bits, max_s);
                            self.insts[i as usize].op = Op::Bin(Bin::AShr, x, sk);
                            *changed = true;
                            return None;
                        }
                        None
                    }
                    Bin::Mul => {
                        let pow2 = match (ka, kb) {
                            (None, Some(c)) if c > 1 && c.is_power_of_two() => Some((a, c)),
                            (Some(c), None) if c > 1 && c.is_power_of_two() => Some((b, c)),
                            _ => None,
                        };
                        if let Some((x, c)) = pow2 {
                            let sh = self.k(ty, bits, u64::from(c.trailing_zeros()));
                            self.insts[i as usize].op = Op::Bin(Bin::Shl, x, sh);
                            *changed = true;
                        }
                        None
                    }
                    Bin::UDiv => {
                        if let Some(c) = kb
                            && c > 1
                            && c.is_power_of_two()
                        {
                            let sh = self.k(ty, bits, u64::from(c.trailing_zeros()));
                            self.insts[i as usize].op = Op::Bin(Bin::LShr, a, sh);
                            *changed = true;
                        }
                        None
                    }
                    Bin::URem => {
                        if let Some(c) = kb
                            && c > 1
                            && c.is_power_of_two()
                        {
                            let m = self.k(ty, bits, c - 1);
                            self.insts[i as usize].op = Op::Bin(Bin::And, a, m);
                            *changed = true;
                        }
                        None
                    }
                    _ => None,
                }
            }
            Op::Cmp(p, a, b) if p <= pred::TRUE => {
                if p == pred::FALSE {
                    return Some(self.bool(false));
                }
                if p == pred::TRUE {
                    return Some(self.bool(true));
                }
                // Two float constants compare one way (`v_mul_legacy` asks
                // whether its constant factor is zero).
                let (V::Const(x), V::Const(y)) = (a, b) else {
                    return None;
                };
                let (Const::Float(x), Const::Float(y)) =
                    (&self.consts[x as usize].1, &self.consts[y as usize].1)
                else {
                    return None;
                };
                let (x, y) = (f32::from_bits(*x), f32::from_bits(*y));
                let unordered = x.is_nan() || y.is_nan();
                // Bits 0-2 of a predicate: equal, greater, less; bit 3:
                // or unordered.
                let holds = if unordered {
                    p & 8 != 0
                } else {
                    (p & 1 != 0 && x == y) || (p & 2 != 0 && x > y) || (p & 4 != 0 && x < y)
                };
                Some(self.bool(holds))
            }
            Op::Cmp(p, a, b) => {
                if !(pred::EQ..=pred::SLE).contains(&p) {
                    return None;
                }
                let holds = |x: u64, y: u64, bits: u32| {
                    let (sx, sy) = (signed(bits, x), signed(bits, y));
                    match p {
                        pred::EQ => x == y,
                        pred::NE => x != y,
                        pred::IUGT => x > y,
                        pred::IUGE => x >= y,
                        pred::IULT => x < y,
                        pred::IULE => x <= y,
                        pred::SGT => sx > sy,
                        pred::SGE => sx >= sy,
                        pred::SLT => sx < sy,
                        _ => sx <= sy,
                    }
                };
                if let (Some((bits, x)), Some((_, y))) = (self.int_k(a), self.int_k(b)) {
                    return Some(self.bool(holds(x, y, bits)));
                }
                if a == b {
                    return Some(self.bool(holds(0, 0, 32)));
                }
                // Comparing `select(c, k1, k2)`, `zext i1 c`, `sext i1 c`, or
                // `i1` against an integer constant reduces directly to a
                // constant, `c`, or `!c`.
                let reduced = if let Some((bits, k)) = self.int_k(b)
                    && let Some((c, k1, k2)) = self.two_valued_int(a, bits)
                {
                    Some((c, holds(k1, k, bits), holds(k2, k, bits)))
                } else if let Some((bits, k)) = self.int_k(a)
                    && let Some((c, k1, k2)) = self.two_valued_int(b, bits)
                {
                    Some((c, holds(k, k1, bits), holds(k, k2, bits)))
                } else {
                    None
                };
                if let Some((c, wt, wf)) = reduced {
                    return match (wt, wf) {
                        (true, true) => Some(self.bool(true)),
                        (false, false) => Some(self.bool(false)),
                        (true, false) => Some(c),
                        (false, true) => self.negate_bool_in_place(i, c, changed),
                    };
                }
                None
            }
            Op::Select(mut c, mut t, mut f) => {
                let mut inner = false;
                while let Some(not_c) = self.is_bool_not(c) {
                    c = not_c;
                    std::mem::swap(&mut t, &mut f);
                    inner = true;
                }
                if let Some((_, c)) = self.int_k(c) {
                    return Some(if c != 0 { t } else { f });
                }
                // Under the same condition an inner select is one of its
                // operands (two guarded writes of one register in a row).
                while let V::Inst(x) = t
                    && let Op::Select(c2, t2, _) = self.insts[x as usize].op
                    && c2 == c
                {
                    (t, inner) = (t2, true);
                }
                while let V::Inst(x) = f
                    && let Op::Select(c2, _, f2) = self.insts[x as usize].op
                    && c2 == c
                {
                    (f, inner) = (f2, true);
                }
                if t == f {
                    return Some(t);
                }
                if self.int_bits(ty) == Some(1) {
                    let (kt, kf) = (self.int_k(t).map(|k| k.1), self.int_k(f).map(|k| k.1));
                    match (kt, kf) {
                        (Some(1), Some(0)) => return Some(c),
                        (Some(0), Some(1)) => return self.negate_bool_in_place(i, c, changed),
                        (Some(1), _) => {
                            if f == c {
                                return Some(c);
                            }
                            self.insts[i as usize].op = Op::Bin(Bin::Or, c, f);
                            *changed = true;
                            return None;
                        }
                        (_, Some(0)) => {
                            if t == c {
                                return Some(c);
                            }
                            self.insts[i as usize].op = Op::Bin(Bin::And, c, t);
                            *changed = true;
                            return None;
                        }
                        _ => {}
                    }
                    if t == c {
                        self.insts[i as usize].op = Op::Bin(Bin::Or, c, f);
                        *changed = true;
                        return None;
                    }
                    if f == c {
                        self.insts[i as usize].op = Op::Bin(Bin::And, c, t);
                        *changed = true;
                        return None;
                    }
                }
                if inner {
                    self.insts[i as usize].op = Op::Select(c, t, f);
                    *changed = true;
                }
                None
            }
            Op::Cast(op, x) => {
                let from = self.type_of(x);
                let through = match x {
                    V::Inst(y) => match self.insts[y as usize].op {
                        Op::Cast(op2, z) => Some((op2, z)),
                        _ => None,
                    },
                    _ => None,
                };
                match op {
                    Cast::Bitcast => {
                        if from == ty {
                            return Some(x);
                        }
                        // There and back: the bits are the same.
                        if let Some((Cast::Bitcast, z)) = through
                            && self.type_of(z) == ty
                        {
                            return Some(z);
                        }
                        let V::Const(c) = x else { return None };
                        let to_float = matches!(self.types[ty.0 as usize], TypeDef::Float);
                        match self.consts[c as usize].1 {
                            Const::Int(bits) if to_float && self.int_bits(from) == Some(32) => {
                                Some(self.f32(bits as u32))
                            }
                            Const::Float(bits) if self.int_bits(ty) == Some(32) => {
                                Some(self.int_const(ty, i64::from(bits as i32)))
                            }
                            _ => None,
                        }
                    }
                    Cast::ZExt | Cast::SExt | Cast::Trunc => {
                        let to = self.int_bits(ty)?;
                        if let Some((bits, k)) = self.int_k(x) {
                            let k = if op == Cast::SExt {
                                signed(bits, k) as u64
                            } else {
                                k
                            };
                            return Some(self.k(ty, to, k));
                        }
                        // Widened and narrowed again.
                        if op == Cast::Trunc
                            && let Some((Cast::ZExt | Cast::SExt, z)) = through
                            && self.type_of(z) == ty
                        {
                            return Some(z);
                        }
                        None
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Whether `ty` is an indivisible scalar leaf (never a struct/array aggregate).
    fn is_scalar_ty(&self, ty: Ty) -> bool {
        matches!(
            self.types[ty.0 as usize],
            TypeDef::Int(_) | TypeDef::Float | TypeDef::Double | TypeDef::Ptr(..)
        )
    }

    /// Traces pointer `v` through arbitrarily deep GEP chains to its root
    /// global and canonical, provably in-bounds, non-overlapping scalar-leaf
    /// path (or [`PtrPath::Dynamic`] for any dynamic, out-of-bounds, negative,
    /// aggregate, or non-canonical pointer).
    fn trace_ptr(&self, mut v: V, escaped: &[bool]) -> (PtrRoot, PtrPath) {
        let mut hops: Vec<(Ty, Ty, Vec<V>)> = Vec::new();
        for _ in 0..=self.insts.len() {
            match v {
                V::Global(g) => {
                    let global = &self.globals[g as usize];
                    let root = if global.init.is_some() {
                        PtrRoot::ConstGlobal(g)
                    } else if escaped.get(g as usize).copied().unwrap_or(true) {
                        PtrRoot::Unknown
                    } else if global.space == 0 {
                        PtrRoot::PrivateGlobal(g)
                    } else if global.space == 3 {
                        PtrRoot::SharedGlobal(g)
                    } else {
                        PtrRoot::Unknown
                    };
                    let mut cur_ty = global.ty;
                    let g_space = global.space;
                    let mut path: Vec<u64> = Vec::new();
                    let mut parent_array_len: Option<u32> = None;
                    for (gep_ty, res_ty, idx) in hops.into_iter().rev() {
                        if gep_ty != cur_ty || idx.is_empty() {
                            return (root, PtrPath::Dynamic);
                        }
                        let mut consts = Vec::with_capacity(idx.len());
                        for ix in idx {
                            let Some((bits, k)) = self.int_k(ix) else {
                                return (root, PtrPath::Dynamic);
                            };
                            if bits == 0 {
                                return (root, PtrPath::Dynamic);
                            }
                            consts.push(signed(bits, k));
                        }
                        let s0 = consts[0];
                        if s0 != 0 {
                            let (Some(len), Some(last)) = (parent_array_len, path.last_mut())
                            else {
                                return (root, PtrPath::Dynamic);
                            };
                            let next = i128::from(*last) + i128::from(s0);
                            if next < 0 || next >= i128::from(len) {
                                return (root, PtrPath::Dynamic);
                            }
                            *last = next as u64;
                        }
                        for &s in &consts[1..] {
                            if s < 0 {
                                return (root, PtrPath::Dynamic);
                            }
                            match &self.types[cur_ty.0 as usize] {
                                TypeDef::Array(len, elem) if (s as u64) < u64::from(*len) => {
                                    path.push(s as u64);
                                    parent_array_len = Some(*len);
                                    cur_ty = *elem;
                                }
                                TypeDef::Struct(_, fields) if (s as u64) < fields.len() as u64 => {
                                    path.push(s as u64);
                                    parent_array_len = None;
                                    cur_ty = fields[s as usize];
                                }
                                _ => return (root, PtrPath::Dynamic),
                            }
                        }
                        if !matches!(
                            self.types[res_ty.0 as usize],
                            TypeDef::Ptr(pt, sp) if pt == cur_ty && sp == g_space
                        ) {
                            return (root, PtrPath::Dynamic);
                        }
                    }
                    if !self.is_scalar_ty(cur_ty) {
                        return (root, PtrPath::Dynamic);
                    }
                    return (root, PtrPath::Const(cur_ty, path));
                }
                V::Inst(id) => {
                    let inst = &self.insts[id as usize];
                    let Op::Gep(gep_ty, base, idx) = &inst.op else {
                        return (PtrRoot::Unknown, PtrPath::Dynamic);
                    };
                    hops.push((*gep_ty, inst.ty, idx.clone()));
                    v = *base;
                }
                _ => return (PtrRoot::Unknown, PtrPath::Dynamic),
            }
        }
        (PtrRoot::Unknown, PtrPath::Dynamic)
    }

    /// Per global: whether its address (or any GEP derived from it) escapes
    /// beyond direct `Gep`/`Load`/`Store`/`AtomicRmw`/`CmpXchg` pointer slots.
    fn escaped_globals(&self, order: &[usize]) -> Vec<bool> {
        let mut escaped = vec![false; self.globals.len()];
        if escaped.is_empty() {
            return escaped;
        }
        let mut gep_root: Vec<Option<u32>> = vec![None; self.insts.len()];
        let root_of = |gep_root: &[Option<u32>], v: V| match v {
            V::Global(g) => Some(g),
            V::Inst(i) => gep_root[i as usize],
            _ => None,
        };
        // Collect GEP roots in RPO order (bases dominate derived GEPs).
        for &b in order {
            for &i in &self.blocks[b].body {
                if let Op::Gep(_, base, _) = self.insts[i as usize].op {
                    gep_root[i as usize] = root_of(&gep_root, base);
                }
            }
        }
        let mark = |escaped: &mut [bool], gep_root: &[Option<u32>], v: V| {
            if let Some(g) = root_of(gep_root, v) {
                escaped[g as usize] = true;
            }
        };
        for &b in order {
            for &i in self.blocks[b].phis.iter().chain(&self.blocks[b].body) {
                match &self.insts[i as usize].op {
                    Op::Gep(_, _, idx) => {
                        for &ix in idx {
                            mark(&mut escaped, &gep_root, ix);
                        }
                    }
                    Op::Load(_) => {}
                    Op::Store(_, val) | Op::AtomicRmw(_, _, val) => {
                        mark(&mut escaped, &gep_root, *val);
                    }
                    Op::CmpXchg(_, exp, new) => {
                        mark(&mut escaped, &gep_root, *exp);
                        mark(&mut escaped, &gep_root, *new);
                    }
                    other => {
                        for_each_operand(other, |v| mark(&mut escaped, &gep_root, v));
                    }
                }
            }
        }
        escaped
    }

    /// Whether `p` is a provably in-bounds constant pointer to a scalar of
    /// type `val_ty` inside an immutable constant global (safe to load
    /// speculatively during LICM).
    fn is_in_bounds_const_global_ptr(&self, p: V, val_ty: Ty) -> bool {
        matches!(
            self.trace_ptr(p, &[]),
            (PtrRoot::ConstGlobal(_), PtrPath::Const(leaf_ty, _)) if leaf_ty == val_ty
        )
    }

    /// What kind of instruction `i` is for merging.
    fn class(&self, effects: &[Effect], i: u32) -> Effect {
        match &self.insts[i as usize].op {
            Op::Bin(..)
            | Op::Cast(..)
            | Op::Cmp(..)
            | Op::Select(..)
            | Op::Extract(..)
            | Op::Gep(..) => Effect::Pure,
            Op::Load(p) => {
                // A constant table reads the same whatever is stored (walk
                // arbitrarily deep GEP chains).
                let mut cur = *p;
                for _ in 0..=self.insts.len() {
                    match cur {
                        V::Global(g) if self.globals[g as usize].init.is_some() => {
                            return Effect::Pure;
                        }
                        V::Inst(g) => match &self.insts[g as usize].op {
                            Op::Gep(_, base, _) => cur = *base,
                            _ => break,
                        },
                        _ => break,
                    }
                }
                Effect::Reads
            }
            Op::Store(..) | Op::AtomicRmw(..) | Op::CmpXchg(..) => Effect::Writes,
            Op::Phi(_) => Effect::Lanes,
            Op::Call(f, _) => effects[*f as usize],
        }
    }

    fn key(&self, i: u32) -> Option<Key> {
        let inst = &self.insts[i as usize];
        Some(match &inst.op {
            Op::Bin(op, a, b) => {
                // Integer sums, products and bit operations whichever way
                // round; float ones as written (which NaN an operation of
                // two NaNs gives is the hardware's choice by operand).
                let commutes = matches!(op, Bin::Add | Bin::Mul | Bin::And | Bin::Or | Bin::Xor)
                    && self.int_bits(inst.ty).is_some();
                let (a, b) = if commutes && b < a {
                    (*b, *a)
                } else {
                    (*a, *b)
                };
                Key::Bin(*op as u8, a, b)
            }
            Op::Cast(op, v) => Key::Cast(*op as u8, inst.ty, *v),
            Op::Cmp(p, a, b) => {
                let (cp, ca, cb) = canonical_cmp(*p, *a, *b);
                Key::Cmp(cp, ca, cb)
            }
            Op::Select(c, t, f) => Key::Select(*c, *t, *f),
            Op::Extract(a, n) => Key::Extract(*a, *n),
            Op::Call(f, args) => Key::Call(*f, args.clone()),
            Op::Gep(ty, p, idx) => Key::Gep(*ty, *p, idx.clone()),
            Op::Load(p) => Key::Load(*p),
            _ => return None,
        })
    }

    /// [`FOLD`], [`REUSE`], [`FACTS`], [`LOADS`] and [`BARRIERS`]: one walk
    /// over the dominator tree. Returns whether anything changed.
    fn merge(&mut self, passes: u32) -> bool {
        if self.blocks.is_empty() {
            return false;
        }
        let effects = self.effects();
        let tree = self.dominator_tree();
        let escaped = self.escaped_globals(&tree.order);
        let mut repl: Vec<Option<V>> = vec![None; self.insts.len()];
        let mut changed = false;
        // Joins of one value: until none is left (one gone can leave
        // another with one value).
        if passes & FOLD != 0 {
            let mut again = true;
            while again {
                again = false;
                for &b in &tree.order {
                    for &p in &self.blocks[b].phis {
                        if repl[p as usize].is_some() {
                            continue;
                        }
                        let Op::Phi(incoming) = &self.insts[p as usize].op else {
                            continue;
                        };
                        let mut one = None;
                        let mut same = true;
                        for &(v, _) in incoming {
                            let v = resolve(&repl, v);
                            if v == V::Inst(p) {
                                continue;
                            }
                            match one {
                                None => one = Some(v),
                                Some(o) if o == v => {}
                                Some(_) => same = false,
                            }
                        }
                        if let (true, Some(v)) = (same, one) {
                            repl[p as usize] = Some(v);
                            again = true;
                            changed = true;
                        }
                    }
                }
            }
        }
        // The values every path to the block being walked has computed
        // (those of the blocks above it in the tree, and its own so far).
        let mut known: HashMap<Key, V> = HashMap::new();
        // Per-block resource memory reads (`dx.op.*` with `Effect::Reads`).
        let mut read_mem: HashMap<Key, V> = HashMap::new();
        // Per-block wave/quad lane ops (`Effect::Lanes`).
        let mut read_lanes: HashMap<Key, V> = HashMap::new();
        // Per-block shared memory (LDS, space 3) tracked values.
        let mut shared_const: HashMap<(u32, Vec<u64>), V> = HashMap::new();
        let mut shared_dyn: HashMap<(u32, V, Ty), V> = HashMap::new();
        // Per-block escaped/unknown pointer loads.
        let mut unknown_loads: HashMap<V, V> = HashMap::new();
        // Per-block un-observed local/shared stores eligible for overwritten DSE.
        let mut pending_const_stores: HashMap<(u32, Vec<u64>), usize> = HashMap::new();
        let mut pending_dyn_stores: HashMap<(u32, V, Ty), usize> = HashMap::new();
        // Exit state of non-escaping thread-private globals for single-pred children.
        let mut exit_private: Vec<Option<PrivateVals>> = vec![None; self.blocks.len()];
        let mut facts = Facts::default();
        let (yes, no) = (self.bool(true), self.bool(false));
        enum Step {
            Enter(usize),
            Leave(usize, Vec<Key>, Learned),
        }
        let mut steps = vec![Step::Enter(0)];
        while let Some(step) = steps.pop() {
            let b = match step {
                Step::Enter(b) => b,
                Step::Leave(b, keys, learned) => {
                    for k in keys {
                        known.remove(&k);
                    }
                    for v in learned.bits {
                        facts.bits.remove(&v);
                    }
                    for k in learned.compares {
                        facts.compares.remove(&k);
                    }
                    exit_private[b] = None;
                    continue;
                }
            };
            let mut added = Vec::new();
            // Reached from one block alone, by one side of its branch: the
            // condition is that side's, here and in every block below.
            let mut learned = Learned::default();
            if passes & FACTS != 0
                && let [p] = tree.preds[b][..]
                && let Some(Term::CondBr(c, t, f)) = self.blocks[p].term
                && t != f
            {
                self.learn(&repl, c, b == t, &mut facts, &mut learned, 8);
            }
            let known_bit = |facts: &Facts, v: V| match facts.bits.get(&v) {
                Some(true) => yes,
                Some(false) => no,
                None => v,
            };
            read_mem.clear();
            read_lanes.clear();
            shared_const.clear();
            shared_dyn.clear();
            unknown_loads.clear();
            pending_const_stores.clear();
            pending_dyn_stores.clear();
            let mut priv_vals = if passes & LOADS != 0
                && let [p] = tree.preds[b][..]
            {
                exit_private[p].clone().unwrap_or_default()
            } else {
                PrivateVals::default()
            };
            let mut barriers = BarrierState::default();
            let mut dropped_kept = false;
            // Two joins of the same values from the same blocks are one.
            if passes & REUSE != 0 && self.blocks[b].phis.len() > 1 {
                let mut joins: HashMap<Vec<(usize, V)>, V> = HashMap::new();
                for &p in &self.blocks[b].phis {
                    if repl[p as usize].is_some() {
                        continue;
                    }
                    let ty = self.insts[p as usize].ty;
                    let Op::Phi(incoming) = &self.insts[p as usize].op else {
                        continue;
                    };
                    let mut key: Vec<(usize, V)> = incoming
                        .iter()
                        .map(|&(v, from)| (from, resolve(&repl, v)))
                        .collect();
                    key.sort();
                    // The type too: an undefined value of one type is not
                    // that of another.
                    key.push((ty.0 as usize, V::Global(u32::MAX)));
                    match joins.get(&key) {
                        Some(&first) => {
                            repl[p as usize] = Some(first);
                            changed = true;
                        }
                        None => {
                            joins.insert(key, V::Inst(p));
                        }
                    }
                }
            }
            let body = std::mem::take(&mut self.blocks[b].body);
            let mut kept = Vec::with_capacity(body.len());
            for i in body {
                map_operands(&mut self.insts[i as usize].op, |v| {
                    let v = resolve(&repl, v);
                    let bit = known_bit(&facts, v);
                    changed |= bit != v;
                    bit
                });
                if passes & FACTS != 0
                    && let Op::Cmp(p, x, y) = self.insts[i as usize].op
                    && let Some(&holds) = facts.compares.get(&canonical_cmp(p, x, y))
                {
                    repl[i as usize] = Some(if holds { yes } else { no });
                    changed = true;
                    continue;
                }
                if passes & FOLD != 0
                    && let Some(v) = self.simplify(i, &mut changed)
                {
                    repl[i as usize] = Some(v);
                    changed = true;
                    continue;
                }
                if passes & FACTS != 0
                    && let Op::Cmp(p, x, y) = self.insts[i as usize].op
                    && let Some(&holds) = facts.compares.get(&canonical_cmp(p, x, y))
                {
                    repl[i as usize] = Some(if holds { yes } else { no });
                    changed = true;
                    continue;
                }
                let class = self.class(&effects, i);
                if passes & REUSE != 0
                    && class == Effect::Pure
                    && let Some(key) = self.key(i)
                {
                    if let Some(&first) = known.get(&key) {
                        repl[i as usize] = Some(first);
                        changed = true;
                        continue;
                    }
                    known.insert(key.clone(), V::Inst(i));
                    added.push(key);
                }
                match self.insts[i as usize].op.clone() {
                    Op::Load(_) if class == Effect::Pure => {
                        barriers.on_load(PtrRoot::ConstGlobal(0));
                    }
                    Op::Load(ptr) => {
                        if passes & (LOADS | BARRIERS) != 0 {
                            let val_ty = self.insts[i as usize].ty;
                            let (root, raw_path) = self.trace_ptr(ptr, &escaped);
                            if passes & LOADS != 0 {
                                let path = match raw_path {
                                    PtrPath::Const(leaf_ty, cp) if leaf_ty == val_ty => {
                                        PtrPath::Const(leaf_ty, cp)
                                    }
                                    _ => PtrPath::Dynamic,
                                };
                                match root {
                                    PtrRoot::PrivateGlobal(g) => {
                                        let hit = match &path {
                                            PtrPath::Const(_, cp) => {
                                                priv_vals.const_vals.get(&(g, cp.clone())).copied()
                                            }
                                            PtrPath::Dynamic => {
                                                priv_vals.dyn_vals.get(&(g, ptr, val_ty)).copied()
                                            }
                                        };
                                        if let Some(v) = hit {
                                            repl[i as usize] = Some(v);
                                            changed = true;
                                            continue;
                                        }
                                        match path {
                                            PtrPath::Const(_, cp) => {
                                                pending_const_stores
                                                    .retain(|(pg, pp), _| !(*pg == g && *pp == cp));
                                                pending_dyn_stores.retain(|(pg, _, _), _| *pg != g);
                                                priv_vals.const_vals.insert((g, cp), V::Inst(i));
                                            }
                                            PtrPath::Dynamic => {
                                                pending_const_stores.retain(|(pg, _), _| *pg != g);
                                                pending_dyn_stores.retain(|(pg, _, _), _| *pg != g);
                                                priv_vals
                                                    .dyn_vals
                                                    .insert((g, ptr, val_ty), V::Inst(i));
                                            }
                                        }
                                    }
                                    PtrRoot::SharedGlobal(g) => {
                                        let hit = match &path {
                                            PtrPath::Const(_, cp) => {
                                                shared_const.get(&(g, cp.clone())).copied()
                                            }
                                            PtrPath::Dynamic => {
                                                shared_dyn.get(&(g, ptr, val_ty)).copied()
                                            }
                                        };
                                        if let Some(v) = hit {
                                            repl[i as usize] = Some(v);
                                            changed = true;
                                            continue;
                                        }
                                        match path {
                                            PtrPath::Const(_, cp) => {
                                                pending_const_stores
                                                    .retain(|(pg, pp), _| !(*pg == g && *pp == cp));
                                                pending_dyn_stores.retain(|(pg, _, _), _| *pg != g);
                                                shared_const.insert((g, cp), V::Inst(i));
                                            }
                                            PtrPath::Dynamic => {
                                                pending_const_stores.retain(|(pg, _), _| *pg != g);
                                                pending_dyn_stores.retain(|(pg, _, _), _| *pg != g);
                                                shared_dyn.insert((g, ptr, val_ty), V::Inst(i));
                                            }
                                        }
                                    }
                                    PtrRoot::Unknown => {
                                        if let Some(&first) = unknown_loads.get(&ptr)
                                            && self.type_of(first) == val_ty
                                        {
                                            repl[i as usize] = Some(first);
                                            changed = true;
                                            continue;
                                        }
                                        unknown_loads.insert(ptr, V::Inst(i));
                                    }
                                    PtrRoot::ConstGlobal(_) => {}
                                }
                            }
                            barriers.on_load(root);
                        } else {
                            barriers.reset();
                        }
                    }
                    Op::Store(ptr, val) => {
                        if passes & LOADS != 0 {
                            let val_ty = self.type_of(val);
                            let (root, raw_path) = self.trace_ptr(ptr, &escaped);
                            let path = match raw_path {
                                PtrPath::Const(leaf_ty, cp) if leaf_ty == val_ty => {
                                    PtrPath::Const(leaf_ty, cp)
                                }
                                _ => PtrPath::Dynamic,
                            };
                            match root {
                                PtrRoot::PrivateGlobal(g) => match path {
                                    PtrPath::Const(_, cp) => {
                                        let slot = (g, cp);
                                        if priv_vals.const_vals.get(&slot) == Some(&val) {
                                            changed = true;
                                            continue;
                                        }
                                        if let Some(prev) = pending_const_stores.remove(&slot) {
                                            kept[prev] = u32::MAX;
                                            dropped_kept = true;
                                            changed = true;
                                        }
                                        priv_vals.dyn_vals.retain(|(pg, _, _), _| *pg != g);
                                        pending_dyn_stores.retain(|(pg, _, _), _| *pg != g);
                                        priv_vals.const_vals.insert(slot.clone(), val);
                                        let pos = kept.len();
                                        kept.push(i);
                                        pending_const_stores.insert(slot, pos);
                                        barriers.reset();
                                        continue;
                                    }
                                    PtrPath::Dynamic => {
                                        let slot = (g, ptr, val_ty);
                                        if priv_vals.dyn_vals.get(&slot) == Some(&val) {
                                            changed = true;
                                            continue;
                                        }
                                        if let Some(prev) = pending_dyn_stores.remove(&slot) {
                                            kept[prev] = u32::MAX;
                                            dropped_kept = true;
                                            changed = true;
                                        }
                                        pending_const_stores.retain(|(pg, _), _| *pg != g);
                                        pending_dyn_stores.retain(|(pg, _, _), _| *pg != g);
                                        priv_vals.const_vals.retain(|(pg, _), _| *pg != g);
                                        priv_vals.dyn_vals.retain(|(pg, _, _), _| *pg != g);
                                        priv_vals.dyn_vals.insert(slot, val);
                                        let pos = kept.len();
                                        kept.push(i);
                                        pending_dyn_stores.insert(slot, pos);
                                        barriers.reset();
                                        continue;
                                    }
                                },
                                PtrRoot::SharedGlobal(g) => match path {
                                    PtrPath::Const(_, cp) => {
                                        let slot = (g, cp);
                                        if let Some(prev) = pending_const_stores.remove(&slot) {
                                            kept[prev] = u32::MAX;
                                            dropped_kept = true;
                                            changed = true;
                                        }
                                        shared_dyn.retain(|(pg, _, _), _| *pg != g);
                                        pending_dyn_stores.retain(|(pg, _, _), _| *pg != g);
                                        // Do not forward space-3 store -> load: without a
                                        // barrier, another lane in the same wave can write the
                                        // same LDS slot (`lds[bucket] = lane; if (lds[bucket] == lane)`).
                                        shared_const.remove(&slot);
                                        let pos = kept.len();
                                        kept.push(i);
                                        pending_const_stores.insert(slot, pos);
                                        barriers.reset();
                                        continue;
                                    }
                                    PtrPath::Dynamic => {
                                        let slot = (g, ptr, val_ty);
                                        if let Some(prev) = pending_dyn_stores.remove(&slot) {
                                            kept[prev] = u32::MAX;
                                            dropped_kept = true;
                                            changed = true;
                                        }
                                        pending_const_stores.retain(|(pg, _), _| *pg != g);
                                        pending_dyn_stores.retain(|(pg, _, _), _| *pg != g);
                                        shared_const.retain(|(pg, _), _| *pg != g);
                                        shared_dyn.retain(|(pg, _, _), _| *pg != g);
                                        // Do not insert into `shared_dyn` on store (same
                                        // intra-wave LDS race as const-indexed stores).
                                        let pos = kept.len();
                                        kept.push(i);
                                        pending_dyn_stores.insert(slot, pos);
                                        barriers.reset();
                                        continue;
                                    }
                                },
                                PtrRoot::Unknown | PtrRoot::ConstGlobal(_) => {
                                    unknown_loads.clear();
                                }
                            }
                        }
                        barriers.reset();
                    }
                    Op::AtomicRmw(_, ptr, _) | Op::CmpXchg(ptr, _, _) => {
                        if passes & LOADS != 0 {
                            let (root, _) = self.trace_ptr(ptr, &escaped);
                            match root {
                                PtrRoot::PrivateGlobal(g) => {
                                    priv_vals.const_vals.retain(|(pg, _), _| *pg != g);
                                    priv_vals.dyn_vals.retain(|(pg, _, _), _| *pg != g);
                                    pending_const_stores.retain(|(pg, _), _| *pg != g);
                                    pending_dyn_stores.retain(|(pg, _, _), _| *pg != g);
                                }
                                PtrRoot::SharedGlobal(g) => {
                                    shared_const.retain(|(pg, _), _| *pg != g);
                                    shared_dyn.retain(|(pg, _, _), _| *pg != g);
                                    pending_const_stores.retain(|(pg, _), _| *pg != g);
                                    pending_dyn_stores.retain(|(pg, _, _), _| *pg != g);
                                }
                                PtrRoot::Unknown | PtrRoot::ConstGlobal(_) => {
                                    unknown_loads.clear();
                                }
                            }
                        }
                        barriers.reset();
                    }
                    Op::Call(f, args) => match class {
                        Effect::Reads => {
                            if passes & LOADS != 0
                                && let Some(key) = self.key(i)
                            {
                                if let Some(&first) = read_mem.get(&key) {
                                    repl[i as usize] = Some(first);
                                    changed = true;
                                    continue;
                                }
                                read_mem.insert(key, V::Inst(i));
                            }
                            barriers.reset();
                        }
                        Effect::Lanes => {
                            if passes & LOADS != 0
                                && let Some(key) = self.key(i)
                            {
                                if let Some(&first) = read_lanes.get(&key) {
                                    repl[i as usize] = Some(first);
                                    changed = true;
                                    continue;
                                }
                                read_lanes.insert(key, V::Inst(i));
                            }
                            barriers.reset();
                        }
                        Effect::Writes => match write_kind(&self.funcs[f as usize]) {
                            WriteKind::Barrier => {
                                let mode = if args.len() == 2
                                    && self.int_k(args[0]).is_some_and(|(_, op)| op == 80)
                                {
                                    self.int_k(args[1]).map(|(_, m)| m)
                                } else {
                                    None
                                };
                                if passes & BARRIERS != 0
                                    && self.is_void(self.insts[i as usize].ty)
                                {
                                    if let (Some(prev), Some(m)) = (barriers.adj, mode)
                                        && (prev & m) == m
                                    {
                                        changed = true;
                                        continue;
                                    }
                                    if let Some(m) = mode {
                                        let next_pos = kept.len();
                                        barriers.rar = match barriers.rar {
                                            RarState::AfterAnchor {
                                                anchor_mode,
                                                saw_lds_load: true,
                                            } if (m & 9) != 0 && (anchor_mode & m) == m => {
                                                RarState::AfterMid {
                                                    anchor_mode,
                                                    mid_mode: m,
                                                    mid_pos: next_pos,
                                                    saw_lds_load: false,
                                                }
                                            }
                                            RarState::AfterMid {
                                                anchor_mode,
                                                mid_mode,
                                                mid_pos,
                                                saw_lds_load: true,
                                            } => {
                                                if (m & 9) == 9 && (m & mid_mode) == mid_mode {
                                                    kept[mid_pos] = u32::MAX;
                                                    dropped_kept = true;
                                                    changed = true;
                                                    if (anchor_mode & m) == m {
                                                        RarState::AfterMid {
                                                            anchor_mode,
                                                            mid_mode: m,
                                                            mid_pos: next_pos,
                                                            saw_lds_load: false,
                                                        }
                                                    } else {
                                                        RarState::AfterAnchor {
                                                            anchor_mode: m,
                                                            saw_lds_load: false,
                                                        }
                                                    }
                                                } else if (mid_mode & 9) == 9
                                                    && (m & 9) != 0
                                                    && (mid_mode & m) == m
                                                {
                                                    RarState::AfterMid {
                                                        anchor_mode: mid_mode,
                                                        mid_mode: m,
                                                        mid_pos: next_pos,
                                                        saw_lds_load: false,
                                                    }
                                                } else if (m & 9) == 9 {
                                                    RarState::AfterAnchor {
                                                        anchor_mode: m,
                                                        saw_lds_load: false,
                                                    }
                                                } else {
                                                    RarState::None
                                                }
                                            }
                                            _ if (m & 9) == 9 => RarState::AfterAnchor {
                                                anchor_mode: m,
                                                saw_lds_load: false,
                                            },
                                            _ => RarState::None,
                                        };
                                        barriers.adj = Some(m);
                                    } else {
                                        barriers.reset();
                                    }
                                } else {
                                    barriers.reset();
                                }
                                if passes & LOADS != 0 {
                                    let touches_lds = mode.is_none_or(|m| m & (1 | 8) != 0);
                                    let touches_uav = mode.is_none_or(|m| m & (1 | 2 | 4) != 0);
                                    if touches_lds {
                                        shared_const.clear();
                                        shared_dyn.clear();
                                        unknown_loads.clear();
                                        pending_const_stores.retain(|(pg, _), _| {
                                            self.globals[*pg as usize].space == 0
                                        });
                                        pending_dyn_stores.retain(|(pg, _, _), _| {
                                            self.globals[*pg as usize].space == 0
                                        });
                                    }
                                    if touches_uav {
                                        read_mem.clear();
                                    }
                                }
                            }
                            WriteKind::OutputOnly => {
                                barriers.on_non_lds_sink_store();
                            }
                            WriteKind::ResourceMemory => {
                                read_mem.clear();
                                let name = self.funcs[f as usize]
                                    .name
                                    .strip_prefix("dx.op.")
                                    .unwrap_or(&self.funcs[f as usize].name);
                                if name.starts_with("atomic") {
                                    barriers.reset();
                                } else {
                                    barriers.on_non_lds_sink_store();
                                }
                            }
                            WriteKind::All => {
                                let is_discard = self.funcs[f as usize]
                                    .name
                                    .strip_prefix("dx.op.")
                                    .unwrap_or(&self.funcs[f as usize].name)
                                    == "discard";
                                read_mem.clear();
                                read_lanes.clear();
                                shared_const.clear();
                                shared_dyn.clear();
                                unknown_loads.clear();
                                pending_const_stores.clear();
                                pending_dyn_stores.clear();
                                if !is_discard {
                                    priv_vals = PrivateVals::default();
                                }
                                barriers.reset();
                            }
                        },
                        Effect::Pure => {
                            if self.funcs[f as usize].attr != Some(Attr::ReadNone) {
                                barriers.reset();
                            }
                        }
                    },
                    Op::Phi(_) => barriers.reset(),
                    _ => {}
                }
                kept.push(i);
            }
            if passes & BARRIERS != 0 && matches!(self.blocks[b].term, Some(Term::Ret)) {
                match barriers.rar {
                    RarState::AfterMid {
                        mid_mode,
                        mid_pos,
                        saw_lds_load: true,
                        ..
                    } if (mid_mode & 1) != 0 && mid_mode & !(1 | 8) == 0 => {
                        kept[mid_pos] = u32::MAX;
                        dropped_kept = true;
                        changed = true;
                    }
                    RarState::TailBeforeRet { mid_pos } => {
                        kept[mid_pos] = u32::MAX;
                        dropped_kept = true;
                        changed = true;
                    }
                    _ => {}
                }
            }
            if dropped_kept {
                kept.retain(|&id| id != u32::MAX);
            }
            self.blocks[b].body = kept;
            if let Some(Term::CondBr(c, t, f)) = self.blocks[b].term {
                let c = known_bit(&facts, resolve(&repl, c));
                self.blocks[b].term = Some(Term::CondBr(c, t, f));
            }
            if passes & LOADS != 0
                && (!priv_vals.const_vals.is_empty() || !priv_vals.dyn_vals.is_empty())
                && tree.children[b].iter().any(|&c| tree.preds[c].len() == 1)
            {
                exit_private[b] = Some(priv_vals);
            }
            steps.push(Step::Leave(b, added, learned));
            for &c in tree.children[b].iter().rev() {
                steps.push(Step::Enter(c));
            }
        }
        if !changed {
            return false;
        }
        // Everything that read a replaced value reads its replacement
        // (joins read values from blocks walked after them).
        for blk in &mut self.blocks {
            blk.phis.retain(|&p| repl[p as usize].is_none());
            if let Some(Term::CondBr(c, t, f)) = blk.term {
                blk.term = Some(Term::CondBr(resolve(&repl, c), t, f));
            }
        }
        let (blocks, insts) = (&self.blocks, &mut self.insts);
        for blk in blocks {
            for &i in blk.phis.iter().chain(&blk.body) {
                map_operands(&mut insts[i as usize].op, |v| resolve(&repl, v));
            }
        }
        true
    }

    /// Whether a `dx.op.*` name is a pure value-only math intrinsic safe to
    /// execute speculatively (never accesses descriptors, resources, inputs,
    /// or memory, and never traps on any bit pattern).
    fn is_speculatable_pure_call(name: &str) -> bool {
        let base = name.split('.').next().unwrap_or(name);
        matches!(
            base,
            "unary"
                | "unaryBits"
                | "binary"
                | "tertiary"
                | "quaternary"
                | "isSpecialFloat"
                | "legacyF16ToF32"
                | "legacyF32ToF16"
                | "makeDouble"
                | "splitDouble"
                | "dot2"
                | "dot3"
                | "dot4"
        )
    }

    /// Whether instruction `i` inside a loop is pure, non-trapping, and safe to
    /// hoist to the loop preheader (`guaranteed`: `i` executes on every entry
    /// to the loop with the exact same active lanes as the preheader, before
    /// any `dx.op.discard` or unknown side-effecting call).
    fn can_hoist(&self, effects: &[Effect], i: u32, guaranteed: bool) -> bool {
        match &self.insts[i as usize].op {
            Op::Bin(Bin::UDiv | Bin::URem, _, rhs) => {
                self.int_k(*rhs).is_some_and(|(_, k)| k != 0)
            }
            Op::Bin(Bin::SDiv, _, rhs) => self
                .int_k(*rhs)
                .is_some_and(|(bits, k)| k != 0 && signed(bits, k) != -1),
            Op::Bin(..)
            | Op::Cast(..)
            | Op::Cmp(..)
            | Op::Select(..)
            | Op::Extract(..)
            | Op::Gep(..) => true,
            Op::Load(p) => {
                self.class(effects, i) == Effect::Pure
                    && (guaranteed
                        || self.is_in_bounds_const_global_ptr(*p, self.insts[i as usize].ty))
            }
            Op::Call(f, _) => {
                if effects[*f as usize] != Effect::Pure {
                    return false;
                }
                if guaranteed {
                    return true;
                }
                let name = self.funcs[*f as usize].name.as_str();
                let name = name.strip_prefix("dx.op.").unwrap_or(name);
                Self::is_speculatable_pure_call(name)
            }
            Op::Store(..) | Op::AtomicRmw(..) | Op::CmpXchg(..) | Op::Phi(_) => false,
        }
    }

    /// Whether every operand of instruction `i` is defined outside the loop
    /// `in_loop` and dominates `dom_target`.
    fn operands_invariant(
        &self,
        i: u32,
        in_loop: &[bool],
        def_block: &[usize],
        idom: &[usize],
        dom_target: usize,
    ) -> bool {
        let mut ok = true;
        for_each_operand(&self.insts[i as usize].op, |v| {
            if let V::Inst(op) = v {
                let b = def_block[op as usize];
                if b == usize::MAX || in_loop[b] || !dominates(idom, b, dom_target) {
                    ok = false;
                }
            }
        });
        ok
    }

    /// Splits conditional entry edge `entry_pred -> header` into
    /// `entry_pred -> ph -> header`, updating header phis, `tree`, and outer
    /// loop memberships.
    fn split_preheader(
        &mut self,
        entry_pred: usize,
        header: usize,
        tree: &mut Tree,
        loops: &mut [NaturalLoop],
        cur_idx: usize,
    ) -> usize {
        let ph = self.new_block();
        self.blocks[ph].term = Some(Term::Br(header));
        match self.blocks[entry_pred].term {
            Some(Term::CondBr(c, t, f)) => {
                let t = if t == header { ph } else { t };
                let f = if f == header { ph } else { f };
                self.blocks[entry_pred].term = Some(Term::CondBr(c, t, f));
            }
            _ => unreachable!("split_preheader called on non-CondBr predecessor"),
        }
        for n in 0..self.blocks[header].phis.len() {
            let p = self.blocks[header].phis[n];
            if let Op::Phi(incoming) = &mut self.insts[p as usize].op {
                for (_, from) in incoming {
                    if *from == entry_pred {
                        *from = ph;
                    }
                }
            }
        }
        tree.idom.push(entry_pred);
        tree.idom[header] = ph;
        tree.preds.push(vec![entry_pred]);
        for p in &mut tree.preds[header] {
            if *p == entry_pred {
                *p = ph;
            }
        }
        tree.children.push(vec![header]);
        for c in &mut tree.children[entry_pred] {
            if *c == header {
                *c = ph;
            }
        }
        if let Some(pos) = tree.order.iter().position(|&b| b == header) {
            tree.order.insert(pos, ph);
        }
        for (k, lp) in loops.iter_mut().enumerate() {
            let in_outer = k != cur_idx && lp.in_loop[entry_pred] && lp.in_loop[header];
            lp.in_loop.push(in_outer);
            if in_outer {
                lp.size += 1;
            }
        }
        ph
    }

    /// [`LICM`]: hoists pure, non-trapping loop-invariant instructions out of
    /// natural loops (innermost first) to their dominating preheader.
    fn licm(&mut self) -> bool {
        if self.blocks.is_empty() {
            return false;
        }
        let effects = self.effects();
        let mut tree = self.dominator_tree();
        // Group all backedges `(latch -> header)` by header.
        let mut latches_by_header: HashMap<usize, Vec<usize>> = HashMap::new();
        for &b in &tree.order {
            for &s in successors(self.blocks[b].term).iter() {
                if dominates(&tree.idom, s, b) {
                    latches_by_header.entry(s).or_default().push(b);
                }
            }
        }
        if latches_by_header.is_empty() {
            return false;
        }
        let n_blocks = self.blocks.len();
        let mut loops: Vec<NaturalLoop> = Vec::with_capacity(latches_by_header.len());
        for (header, latches) in latches_by_header {
            if header == 0 {
                continue;
            }
            let mut in_loop = vec![false; n_blocks];
            in_loop[header] = true;
            let mut work = Vec::new();
            for latch in latches {
                if !in_loop[latch] {
                    in_loop[latch] = true;
                    work.push(latch);
                }
            }
            while let Some(b) = work.pop() {
                for &p in &tree.preds[b] {
                    if !in_loop[p] {
                        in_loop[p] = true;
                        work.push(p);
                    }
                }
            }
            let size = in_loop.iter().filter(|&&x| x).count();
            loops.push(NaturalLoop {
                header,
                in_loop,
                size,
            });
        }
        // Innermost loops first, deterministically ordered by header.
        loops.sort_by_key(|l| (l.size, l.header));

        let mut def_block = vec![usize::MAX; self.insts.len()];
        for &b in &tree.order {
            for &i in self.blocks[b].phis.iter().chain(&self.blocks[b].body) {
                def_block[i as usize] = b;
            }
        }

        let mut any_changed = false;
        for idx in 0..loops.len() {
            let header = loops[idx].header;
            let outside_preds: Vec<usize> = tree.preds[header]
                .iter()
                .copied()
                .filter(|&p| !loops[idx].in_loop[p])
                .collect();
            if outside_preds.len() != 1 {
                continue;
            }
            let entry_pred = outside_preds[0];
            let mut preheader = match self.blocks[entry_pred].term {
                Some(Term::Br(t)) if t == header => Some(entry_pred),
                Some(Term::CondBr(_, t, f)) if (t == header) != (f == header) => None,
                _ => continue,
            };

            // Blocks reached unconditionally from `header` along a single-pred /
            // single-succ `Term::Br` chain without any intervening
            // `WriteKind::All` call (e.g. `dx.op.discard`). Instructions in
            // these blocks (before any `WriteKind::All` in the block itself)
            // run on every loop entry with the exact same active lanes as the
            // preheader, preserving dynamic uniformity and preventing
            // speculation past conditional branches or discards.
            let mut guaranteed_blocks = vec![false; self.blocks.len()];
            let mut cur = header;
            loop {
                if guaranteed_blocks[cur] {
                    break;
                }
                guaranteed_blocks[cur] = true;
                let has_barrier_exit = self.blocks[cur].body.iter().any(|&inst_id| {
                    matches!(self.insts[inst_id as usize].op, Op::Call(f, _)
                        if effects[f as usize] == Effect::Writes
                            && write_kind(&self.funcs[f as usize]) == WriteKind::All)
                });
                if has_barrier_exit {
                    break;
                }
                match self.blocks[cur].term {
                    Some(Term::Br(next))
                        if loops[idx].in_loop[next]
                            && next != header
                            && tree.preds[next].len() == 1
                            && tree.preds[next][0] == cur =>
                    {
                        cur = next;
                    }
                    _ => break,
                }
            }
            let loop_blocks: Vec<usize> = tree
                .order
                .iter()
                .copied()
                .filter(|&b| loops[idx].in_loop[b])
                .collect();

            loop {
                let mut round_changed = false;
                for &b in &loop_blocks {
                    let mut guaranteed = guaranteed_blocks[b];
                    let body = std::mem::take(&mut self.blocks[b].body);
                    let mut kept = Vec::with_capacity(body.len());
                    for i in body {
                        if let Op::Call(f, _) = self.insts[i as usize].op
                            && effects[f as usize] == Effect::Writes
                            && write_kind(&self.funcs[f as usize]) == WriteKind::All
                        {
                            guaranteed = false;
                        }
                        let dom_target = preheader.unwrap_or(entry_pred);
                        if self.can_hoist(&effects, i, guaranteed)
                            && self.operands_invariant(
                                i,
                                &loops[idx].in_loop,
                                &def_block,
                                &tree.idom,
                                dom_target,
                            )
                        {
                            let ph = match preheader {
                                Some(ph) => ph,
                                None => {
                                    let ph = self.split_preheader(
                                        entry_pred,
                                        header,
                                        &mut tree,
                                        &mut loops,
                                        idx,
                                    );
                                    preheader = Some(ph);
                                    ph
                                }
                            };
                            self.blocks[ph].body.push(i);
                            def_block[i as usize] = ph;
                            round_changed = true;
                            any_changed = true;
                        } else {
                            kept.push(i);
                        }
                    }
                    self.blocks[b].body = kept;
                }
                if !round_changed {
                    break;
                }
            }
        }
        any_changed
    }

    /// [`FOLD`]: a branch on a constant goes one way. The side not taken
    /// loses the branching block's values at its joins, and so do the joins
    /// of blocks that nothing reaches any more. Returns whether a branch
    /// changed.
    fn prune(&mut self) -> bool {
        let mut changed = false;
        for b in 0..self.blocks.len() {
            let Some(Term::CondBr(c, t, f)) = self.blocks[b].term else {
                continue;
            };
            // Both sides one block: two edges into it, left as they are.
            if t == f {
                continue;
            }
            let Some((_, k)) = self.int_k(c) else {
                continue;
            };
            let (taken, not) = if k != 0 { (t, f) } else { (f, t) };
            self.blocks[b].term = Some(Term::Br(taken));
            changed = true;
            for n in 0..self.blocks[not].phis.len() {
                let p = self.blocks[not].phis[n];
                if let Op::Phi(incoming) = &mut self.insts[p as usize].op {
                    incoming.retain(|&(_, from)| from != b);
                }
            }
        }
        if !changed {
            return false;
        }
        let reached = self.reached();
        let (blocks, insts) = (&self.blocks, &mut self.insts);
        for (_, blk) in blocks.iter().enumerate().filter(|(b, _)| reached[*b]) {
            for &p in &blk.phis {
                if let Op::Phi(incoming) = &mut insts[p as usize].op {
                    incoming.retain(|&(_, from)| reached[from]);
                }
            }
        }
        true
    }

    /// Whether instruction `i` must stay whatever reads it.
    fn has_effect(&self, effects: &[Effect], i: u32) -> bool {
        match &self.insts[i as usize].op {
            Op::Store(..) | Op::AtomicRmw(..) | Op::CmpXchg(..) => true,
            Op::Call(f, _) => effects[*f as usize] == Effect::Writes,
            _ => false,
        }
    }

    /// [`DEAD`]: keeps the instructions with an effect, the branch
    /// conditions, and what they read, transitively; the rest leave their
    /// blocks. Blocks no branch reaches are emptied (they are not written).
    fn drop_dead(&mut self) {
        let effects = self.effects();
        let reached = self.reached();
        let mut live = vec![false; self.insts.len()];
        let mut work: Vec<u32> = Vec::new();
        let mark = |live: &mut [bool], work: &mut Vec<u32>, v: V| {
            if let V::Inst(i) = v
                && !live[i as usize]
            {
                live[i as usize] = true;
                work.push(i);
            }
        };
        for (b, blk) in self.blocks.iter().enumerate() {
            if !reached[b] {
                continue;
            }
            for &i in &blk.body {
                if self.has_effect(&effects, i) {
                    mark(&mut live, &mut work, V::Inst(i));
                }
            }
            if let Some(Term::CondBr(c, ..)) = blk.term {
                mark(&mut live, &mut work, c);
            }
        }
        while let Some(i) = work.pop() {
            for_each_operand(&self.insts[i as usize].op, |v| {
                mark(&mut live, &mut work, v)
            });
        }
        for (b, blk) in self.blocks.iter_mut().enumerate() {
            if reached[b] {
                blk.body.retain(|&i| live[i as usize]);
                blk.phis.retain(|&i| live[i as usize]);
            } else {
                blk.body.clear();
                blk.phis.clear();
            }
        }
        self.drop_unused_functions();
        self.drop_unused_tables();
    }

    /// See [`Usage`]: over the blocks reached from the entry.
    pub(crate) fn usage(&self) -> Usage {
        let mut u = Usage::default();
        let reached = self.reached();
        let wide = |ty: Ty| self.types[ty.0 as usize] == TypeDef::Int(64);
        // The first field of a handle's annotation: its kind, with the UAV
        // bit; the second: the component type and count.
        let annotation = |h: V| -> Option<(i64, i64)> {
            let V::Inst(a) = h else { return None };
            let Op::Call(f, args) = &self.insts[a as usize].op else {
                return None;
            };
            if self.funcs[*f as usize].name != "dx.op.annotateHandle" {
                return None;
            }
            let V::Const(props) = *args.get(2)? else {
                return None;
            };
            let Const::Aggregate(fields) = &self.consts[props as usize].1 else {
                return None;
            };
            match (
                &self.consts[fields[0] as usize].1,
                &self.consts[fields[1] as usize].1,
            ) {
                (Const::Int(kind), Const::Int(components)) => Some((*kind, *components)),
                _ => None,
            }
        };
        for (_, blk) in self.blocks.iter().enumerate().filter(|(b, _)| reached[*b]) {
            for &i in blk.phis.iter().chain(&blk.body) {
                let inst = &self.insts[i as usize];
                u.int64 |= wide(inst.ty);
                for_each_operand(&inst.op, |v| u.int64 |= wide(self.type_of(v)));
                let Op::Call(f, args) = &inst.op else {
                    continue;
                };
                let name = self.funcs[*f as usize].name.as_str();
                let name = name.strip_prefix("dx.op.").unwrap_or(name);
                if name.starts_with("wave") || name.starts_with("quad") {
                    u.wave_ops = true;
                } else if name == "createHandleFromHeap" {
                    match args.get(2).and_then(|&s| self.int_k(s)) {
                        Some((_, 0)) => u.heap_views = true,
                        _ => u.heap_samplers = true,
                    }
                } else if name == "annotateHandle" {
                    u.rovs |= annotation(V::Inst(i)).is_some_and(|(kind, _)| kind & 0x2000 != 0);
                } else if name.starts_with("atomic") && name.ends_with(".i64") {
                    u.atomic64 = true;
                } else if name.starts_with("textureLoad") || name.starts_with("bufferLoad") {
                    u.typed_uav_loads |= args.get(1).and_then(|&h| annotation(h)).is_none_or(
                        |(kind, components)| kind & 0x1000 != 0 && components >> 8 & 0xff > 1,
                    );
                }
            }
        }
        u
    }

    /// A constant table no remaining load reads leaves the module (the
    /// validator refuses an internal declaration nothing uses). Shared
    /// memory stays declared whether it is read or not.
    fn drop_unused_tables(&mut self) {
        let mut used: Vec<bool> = self.globals.iter().map(|g| g.init.is_none()).collect();
        if used.iter().all(|&u| u) {
            return;
        }
        for blk in &self.blocks {
            for &i in blk.phis.iter().chain(&blk.body) {
                for_each_operand(&self.insts[i as usize].op, |v| {
                    if let V::Global(g) = v {
                        used[g as usize] = true;
                    }
                });
            }
        }
        for md in &self.mds {
            if let MdDef::Value(_, V::Global(g)) = md {
                used[*g as usize] = true;
            }
        }
        if used.iter().all(|&u| u) {
            return;
        }
        let mut new = vec![u32::MAX; used.len()];
        for (next, (old, _)) in used.iter().enumerate().filter(|u| *u.1).enumerate() {
            new[old] = next as u32;
        }
        let mut old = 0;
        self.globals.retain(|_| {
            old += 1;
            used[old - 1]
        });
        let renumber = |v: V| match v {
            V::Global(g) if used[g as usize] => V::Global(new[g as usize]),
            v => v,
        };
        for inst in &mut self.insts {
            map_operands(&mut inst.op, renumber);
        }
        for md in &mut self.mds {
            if let MdDef::Value(_, v) = md {
                *v = renumber(*v);
            }
        }
    }

    /// Declarations no remaining call names leave the module (the validator
    /// refuses an external function nothing uses); the others keep their
    /// order, so `main` stays function 0.
    fn drop_unused_functions(&mut self) {
        let mut used: Vec<bool> = self.funcs.iter().map(|f| f.body).collect();
        for blk in &self.blocks {
            for &i in blk.phis.iter().chain(&blk.body) {
                if let Op::Call(f, _) = self.insts[i as usize].op {
                    used[f as usize] = true;
                }
            }
        }
        for md in &self.mds {
            if let MdDef::Value(_, V::Func(f)) = md {
                used[*f as usize] = true;
            }
        }
        if used.iter().all(|&u| u) {
            return;
        }
        let mut new = vec![u32::MAX; used.len()];
        for (next, (old, _)) in used.iter().enumerate().filter(|u| *u.1).enumerate() {
            new[old] = next as u32;
        }
        let mut old = 0;
        self.funcs.retain(|_| {
            old += 1;
            used[old - 1]
        });
        self.func_ids = self
            .funcs
            .iter()
            .enumerate()
            .map(|(i, f)| (f.name.clone(), i as u32))
            .collect();
        // Instructions that left their block keep a stale number: nothing
        // reads them again.
        for inst in &mut self.insts {
            if let Op::Call(f, _) = &mut inst.op
                && used[*f as usize]
            {
                *f = new[*f as usize];
            }
        }
        for md in &mut self.mds {
            if let MdDef::Value(_, V::Func(f)) = md {
                *f = new[*f as usize];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `main` with one block and a function of each kind of effect.
    struct Kit {
        m: Module,
        b: usize,
        pure: V,
        reads: V,
        writes: V,
    }

    fn kit() -> Kit {
        let mut m = Module::default();
        let i32t = m.int(32);
        let void = m.void();
        let fty = m.func_ty(i32t, vec![i32t, i32t]);
        m.ptr(fty);
        let pure = m.function("dx.op.binary.i32", fty, Some(Attr::ReadNone), false);
        let reads = m.function("dx.op.bufferLoad.i32", fty, Some(Attr::ReadOnly), false);
        let writes = m.function("dx.op.bufferStore.i32", fty, Some(Attr::NoUnwind), false);
        let main_ty = m.func_ty(void, vec![]);
        m.ptr(main_ty);
        m.function("main", main_ty, None, true);
        let b = m.new_block();
        Kit {
            m,
            b,
            pure,
            reads,
            writes,
        }
    }

    fn body(m: &Module, b: usize) -> usize {
        m.blocks[b].body.len()
    }

    #[test]
    fn unread_results_without_effect_are_dropped() {
        let Kit {
            mut m,
            b,
            pure,
            reads,
            writes,
        } = kit();
        let (one, two) = (m.i32(1), m.i32(2));
        let used = m.call(b, reads, vec![one, two]);
        let dead = m.call(b, reads, vec![two, one]);
        let chain = m.bin(b, Bin::Add, dead, one);
        let _unread = m.call(b, pure, vec![chain, two]);
        // A store's result is unread and stays, with what it reads.
        let kept = m.bin(b, Bin::Add, used, two);
        m.call(b, writes, vec![kept, one]);
        m.terminate(b, Term::Ret);
        m.optimise(DEAD);
        assert_eq!(body(&m, b), 3, "the load, the add and the store");
        m.write();
    }

    #[test]
    fn branch_conditions_and_phi_cycles() {
        let Kit {
            mut m,
            b,
            reads,
            writes,
            ..
        } = kit();
        let i32t = m.int(32);
        let (zero, one) = (m.i32(0), m.i32(1));
        let (head, exit) = (m.new_block(), m.new_block());
        m.terminate(b, Term::Br(head));
        // A counter that decides the branch, and a register that only
        // feeds itself round the loop.
        let count = m.phi(head, i32t);
        let idle = m.phi(head, i32t);
        let next = m.bin(head, Bin::Add, count, one);
        let idle_next = m.call(head, reads, vec![idle, one]);
        let more = m.cmp(head, pred::IULT, next, one);
        m.terminate(head, Term::CondBr(more, head, exit));
        m.set_phi(count, vec![(zero, b), (next, head)]);
        m.set_phi(idle, vec![(zero, b), (idle_next, head)]);
        m.call(exit, writes, vec![zero, one]);
        m.terminate(exit, Term::Ret);
        m.optimise(DEAD);
        assert_eq!(m.blocks[head].phis.len(), 1, "the counter");
        assert_eq!(body(&m, head), 2, "the step and the test");
        assert_eq!(body(&m, exit), 1);
        m.write();
    }

    #[test]
    fn wave_operations_have_no_effect_of_their_own() {
        let mut k = kit();
        let i32t = k.m.int(32);
        let fty = k.m.func_ty(i32t, vec![i32t, i32t]);
        let first = k.m.function(
            "dx.op.waveReadLaneFirst.i32",
            fty,
            Some(Attr::NoUnwind),
            false,
        );
        let barrier =
            k.m.function("dx.op.barrier", fty, Some(Attr::NoDuplicate), false);
        let one = k.m.i32(1);
        k.m.call(k.b, first, vec![one, one]);
        k.m.call(k.b, barrier, vec![one, one]);
        k.m.terminate(k.b, Term::Ret);
        k.m.optimise(DEAD);
        assert_eq!(body(&k.m, k.b), 1, "the barrier");
        // The declarations nothing calls any more are gone, and the call
        // that stayed names its function by the new number.
        let names: Vec<&str> = k.m.funcs.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["main", "dx.op.barrier"]);
        let Op::Call(f, _) = k.m.insts[k.m.blocks[k.b].body[0] as usize].op else {
            panic!("a call");
        };
        assert_eq!(k.m.funcs[f as usize].name, "dx.op.barrier");
        k.m.write();
    }

    /// The operands of each store left in block `b`.
    fn stores(m: &Module, b: usize) -> Vec<Vec<V>> {
        m.blocks[b]
            .body
            .iter()
            .filter_map(|&i| match &m.insts[i as usize].op {
                Op::Call(g, args) if m.funcs[*g as usize].name.contains("Store") => {
                    Some(args.clone())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn constants_and_identities_fold() {
        let Kit {
            mut m,
            b,
            reads,
            writes,
            ..
        } = kit();
        let (i32t, f32t) = (m.int(32), m.float());
        let (zero, one, all) = (m.i32(0), m.i32(1), m.i32(u32::MAX));
        let x = m.call(b, reads, vec![one, one]);
        // x + 0, -1 & x, x >> 0, x * 1, x | x: all x.
        let a = m.bin(b, Bin::Add, x, zero);
        let a = m.bin(b, Bin::And, all, a);
        let a = m.bin(b, Bin::LShr, a, zero);
        let a = m.bin(b, Bin::Mul, a, one);
        let a = m.bin(b, Bin::Or, a, a);
        // There and back.
        let f = m.cast(b, Cast::Bitcast, a, f32t);
        let a = m.cast(b, Cast::Bitcast, f, i32t);
        // Constants: (262168 >> 16) & 127 = 4, 7 & 31 = 7, -8 >>s 1 = -4.
        let k = m.i32(262168);
        let sixteen = m.i32(16);
        let k = m.bin(b, Bin::LShr, k, sixteen);
        let k127 = m.i32(127);
        let k = m.bin(b, Bin::And, k, k127);
        let minus8 = m.i32(-8i32 as u32);
        let s = m.bin(b, Bin::AShr, minus8, one);
        // A shift by the width is left alone.
        let k32 = m.i32(32);
        let wide = m.bin(b, Bin::Shl, one, k32);
        // A select between a value and itself, and on a constant.
        let c = m.cmp(b, pred::IULT, zero, one);
        let sel = m.select(b, c, a, wide);
        let same = m.select(b, c, sel, sel);
        m.call(b, writes, vec![same, k]);
        m.call(b, writes, vec![s, wide]);
        m.terminate(b, Term::Ret);
        m.optimise(FOLD | DEAD);
        assert_eq!(body(&m, b), 4, "the read, the wide shift and two stores");
        let four = m.i32(4);
        let minus4 = m.i32(-4i32 as u32);
        let stores = stores(&m, b);
        assert_eq!(stores, [vec![x, four], vec![minus4, wide]]);
        m.write();
    }

    #[test]
    fn float_arithmetic_is_not_folded() {
        let Kit {
            mut m, b, writes, ..
        } = kit();
        let f32t = m.float();
        let fty = m.func_ty(f32t, vec![f32t, f32t]);
        let input = m.function("dx.op.loadInput.f32", fty, Some(Attr::ReadNone), false);
        let (zero, one, two) = (m.f32(0), m.f32(1.0f32.to_bits()), m.f32(2.0f32.to_bits()));
        let x = m.call(b, input, vec![zero, zero]);
        let y = m.call(b, input, vec![one, one]);
        let a = m.bin(b, Bin::Add, x, zero);
        let p = m.bin(b, Bin::Mul, a, one);
        // The same sum the other way round is another instruction.
        let xy = m.bin(b, Bin::Add, x, y);
        let yx = m.bin(b, Bin::Add, y, x);
        // Constants compare here: 1 < 2, and nothing equals a NaN.
        let nan = m.f32(f32::NAN.to_bits());
        let lt = m.cmp(b, pred::OLT, one, two);
        let eq = m.cmp(b, pred::OEQ, nan, nan);
        let une = m.cmp(b, pred::UNE, nan, one);
        let sel = m.select(b, lt, xy, yx);
        let sel = m.select(b, eq, p, sel);
        let sel = m.select(b, une, sel, p);
        m.call(b, writes, vec![sel, p]);
        m.terminate(b, Term::Ret);
        m.optimise(ALL);
        // Two inputs, x + 0.0, * 1.0, x + y and the store: y + x and the
        // selects are gone.
        assert_eq!(body(&m, b), 6);
        assert_eq!(stores(&m, b), [[xy, p]]);
        m.write();
    }

    /// entry: `c = x < y`, branch to `then` or `other`, which join.
    struct Diamond {
        k: Kit,
        x: V,
        y: V,
        c: V,
        then: usize,
        other: usize,
        join: usize,
    }

    fn diamond() -> Diamond {
        let mut k = kit();
        let (one, two) = (k.m.i32(1), k.m.i32(2));
        let x = k.m.call(k.b, k.reads, vec![one, one]);
        let y = k.m.call(k.b, k.reads, vec![two, two]);
        let c = k.m.cmp(k.b, pred::IULT, x, y);
        let (then, other, join) = (k.m.new_block(), k.m.new_block(), k.m.new_block());
        k.m.terminate(k.b, Term::CondBr(c, then, other));
        k.m.terminate(then, Term::Br(join));
        k.m.terminate(other, Term::Br(join));
        k.m.terminate(join, Term::Ret);
        Diamond {
            k,
            x,
            y,
            c,
            then,
            other,
            join,
        }
    }

    #[test]
    fn a_value_is_reused_below_its_block_and_not_beside_it() {
        let Diamond {
            k:
                Kit {
                    mut m,
                    b,
                    pure,
                    writes,
                    ..
                },
            x,
            y,
            then,
            other,
            join,
            ..
        } = diamond();
        // The entry's value again in a branch; a branch's value again in
        // the other branch and in the join (neither is on every path).
        let top = m.call(b, pure, vec![x, y]);
        m.call(b, writes, vec![top, top]);
        let again = m.call(then, pure, vec![x, y]);
        let side = m.bin(then, Bin::Add, y, x);
        m.call(then, writes, vec![again, side]);
        let beside = m.bin(other, Bin::Add, x, y);
        m.call(other, writes, vec![beside, beside]);
        let after = m.bin(join, Bin::Add, x, y);
        m.call(join, writes, vec![after, after]);
        m.optimise(REUSE);
        assert_eq!(stores(&m, then), [[top, side]]);
        assert_eq!(body(&m, then), 2);
        assert_eq!(body(&m, other), 2);
        assert_eq!(body(&m, join), 2);
        m.write();
    }

    #[test]
    fn a_branch_taken_says_what_its_condition_is() {
        let Diamond {
            k:
                Kit {
                    mut m,
                    b,
                    reads,
                    writes,
                    ..
                },
            x,
            y,
            c,
            then,
            other,
            join,
        } = diamond();
        let three = m.i32(3);
        let d = m.call(b, reads, vec![three, three]);
        let d = m.cmp(b, pred::NE, d, three);
        let both = m.bin(b, Bin::And, c, d);
        // Behind the branch: the condition again (a guard), its opposite
        // comparison, and a guarded write.
        let guard = m.bin(then, Bin::And, c, d);
        let old = m.select(then, guard, x, y);
        let not = m.cmp(then, pred::IUGE, x, y);
        let new = m.select(then, not, y, old);
        m.call(then, writes, vec![new, new]);
        let kept = m.select(other, c, x, y);
        m.call(other, writes, vec![kept, kept]);
        // After the join nothing is known.
        let open = m.select(join, c, x, y);
        m.call(join, writes, vec![open, open]);
        m.optimise(FOLD | FACTS);
        // then: select(d, x, y) and the store; other: y is stored.
        assert_eq!(body(&m, then), 2);
        let V::Inst(sel) = stores(&m, then)[0][0] else {
            panic!()
        };
        assert!(matches!(m.insts[sel as usize].op, Op::Select(g, a, z) if (g, a, z) == (d, x, y)));
        assert_eq!(stores(&m, other), [[y, y]]);
        assert_eq!(body(&m, join), 2);
        let _ = both;
        m.write();

        // Both terms of an `and` that held hold behind it.
        let Diamond {
            k:
                Kit {
                    mut m,
                    b,
                    reads,
                    writes,
                    ..
                },
            x,
            y,
            c,
            ..
        } = diamond();
        let three = m.i32(3);
        let d = m.call(b, reads, vec![three, three]);
        let d = m.cmp(b, pred::NE, d, three);
        let both = m.bin(b, Bin::And, c, d);
        let (inner, out) = (m.new_block(), m.new_block());
        m.terminate(b, Term::CondBr(both, inner, out));
        let a = m.select(inner, c, x, y);
        let z = m.select(inner, d, a, y);
        m.call(inner, writes, vec![z, z]);
        m.terminate(inner, Term::Br(out));
        // Where the `and` did not hold either term may.
        let w = m.select(out, d, x, y);
        m.call(out, writes, vec![w, w]);
        m.terminate(out, Term::Ret);
        m.optimise(FOLD | FACTS);
        assert_eq!(stores(&m, inner), [[x, x]]);
        assert_eq!(body(&m, out), 2);
        m.write();
    }

    #[test]
    fn a_branch_on_a_constant_goes_one_way() {
        let Diamond {
            k: Kit {
                mut m, b, writes, ..
            },
            x,
            y,
            c,
            then,
            other,
            join,
        } = diamond();
        let i32t = m.int(32);
        // Behind the branch the same test again: its other side is never
        // run, and the join behind it has one value.
        let (deep, never, meet) = (m.new_block(), m.new_block(), m.new_block());
        m.terminate(then, Term::CondBr(c, deep, never));
        m.terminate(deep, Term::Br(meet));
        m.call(never, writes, vec![y, y]);
        m.terminate(never, Term::Br(meet));
        let p = m.phi(meet, i32t);
        m.set_phi(p, vec![(x, deep), (y, never)]);
        m.call(meet, writes, vec![p, p]);
        m.terminate(meet, Term::Br(join));
        let q = m.phi(join, i32t);
        m.set_phi(q, vec![(p, meet), (y, other)]);
        m.call(join, writes, vec![q, q]);
        m.optimise(ALL);
        assert!(matches!(m.blocks[then].term, Some(Term::Br(t)) if t == deep));
        assert_eq!(stores(&m, meet), [[x, x]]);
        assert!(m.blocks[meet].phis.is_empty());
        assert_eq!(body(&m, never), 0, "emptied: nothing reaches it");
        let Op::Phi(incoming) = &m.insts[m.blocks[join].phis[0] as usize].op else {
            panic!()
        };
        assert_eq!(incoming, &vec![(x, meet), (y, other)]);
        let _ = b;
        m.write();
    }

    #[test]
    fn a_read_is_reused_within_its_block_until_a_store() {
        let Diamond {
            k:
                Kit {
                    mut m,
                    b,
                    reads,
                    writes,
                    ..
                },
            x,
            y,
            then,
            ..
        } = diamond();
        let i32t = m.int(32);
        // The entry reads (1, 1) for x: again in the entry it is x; in the
        // next block it is read again.
        let one = m.i32(1);
        let again = m.call(b, reads, vec![one, one]);
        m.call(b, writes, vec![again, y]);
        let after = m.call(b, reads, vec![one, one]);
        let twice = m.call(b, reads, vec![one, one]);
        let sum = m.bin(b, Bin::Add, after, twice);
        m.call(b, writes, vec![sum, sum]);
        let below = m.call(then, reads, vec![one, one]);
        m.call(then, writes, vec![below, below]);
        // A constant buffer cannot change: one load for the function.
        let cfty = m.func_ty(i32t, vec![i32t, i32t]);
        let cb = m.function(
            "dx.op.cbufferLoadLegacy.i32",
            cfty,
            Some(Attr::ReadOnly),
            false,
        );
        let top = m.call(b, cb, vec![one, one]);
        m.call(b, writes, vec![top, top]);
        let under = m.call(then, cb, vec![one, one]);
        m.call(then, writes, vec![under, under]);
        m.optimise(REUSE | LOADS);
        let stores = stores(&m, b);
        assert_eq!(stores[0], [x, y], "the second read is the first");
        let V::Inst(sum) = stores[1][0] else { panic!() };
        assert!(
            matches!(m.insts[sum as usize].op, Op::Bin(Bin::Add, a, z) if a == after && z == after),
            "read again after the store, once"
        );
        assert_eq!(stores[2], [top, top]);
        assert_eq!(
            super::tests::stores(&m, then),
            [vec![below, below], vec![top, top]]
        );
        m.write();
    }

    #[test]
    fn a_join_of_one_value_and_two_equal_joins() {
        let Diamond {
            k: Kit { mut m, writes, .. },
            x,
            y,
            then,
            other,
            join,
            ..
        } = diamond();
        let i32t = m.int(32);
        let one = m.phi(join, i32t);
        m.set_phi(one, vec![(x, then), (x, other)]);
        let p = m.phi(join, i32t);
        m.set_phi(p, vec![(x, then), (y, other)]);
        let q = m.phi(join, i32t);
        m.set_phi(q, vec![(y, other), (x, then)]);
        m.call(join, writes, vec![one, p]);
        let sum = m.bin(join, Bin::Add, p, q);
        m.call(join, writes, vec![sum, q]);
        m.optimise(FOLD | REUSE);
        assert_eq!(
            m.blocks[join].phis,
            [match p {
                V::Inst(i) => i,
                _ => unreachable!(),
            }]
        );
        let stores = stores(&m, join);
        assert_eq!(stores[0], [x, p]);
        assert_eq!(stores[1][1], p);
        m.write();
    }

    // ---- the passes against an interpreter, on random programs ----

    /// Runs `m` on `inputs`: the stores it makes, in order (and how it
    /// ended). Reads a value before its definition ran: a panic.
    fn run(m: &Module, inputs: &[u32]) -> Vec<(String, Vec<u64>)> {
        let mut vals: Vec<Option<u64>> = vec![None; m.insts.len()];
        let mut memory = [0u64; 16];
        let mut trace = Vec::new();
        let eval = |vals: &[Option<u64>], v: V| -> u64 {
            match v {
                V::Inst(i) => vals[i as usize].expect("a value defined before its use"),
                V::Const(c) => {
                    match (&m.consts[c as usize].1, m.int_bits(m.consts[c as usize].0)) {
                        (Const::Int(x), Some(bits)) => *x as u64 & mask(bits),
                        (Const::Float(bits), _) => u64::from(*bits),
                        _ => 0,
                    }
                }
                _ => panic!("a global or function as a value"),
            }
        };
        let (mut b, mut from) = (0usize, usize::MAX);
        for _ in 0..4000 {
            let blk = &m.blocks[b];
            // Joins take their values together, before any is assigned.
            let joined: Vec<(u32, u64)> = blk
                .phis
                .iter()
                .map(|&p| {
                    let Op::Phi(incoming) = &m.insts[p as usize].op else {
                        panic!("a join")
                    };
                    let v = incoming
                        .iter()
                        .find(|x| x.1 == from)
                        .expect("a value for the edge taken");
                    (p, eval(&vals, v.0))
                })
                .collect();
            for (p, x) in joined {
                vals[p as usize] = Some(x);
            }
            for &i in &blk.body {
                let inst = &m.insts[i as usize];
                let bits = m.int_bits(inst.ty);
                let float = |x: u64| f32::from_bits(x as u32);
                let r = match &inst.op {
                    Op::Bin(op, x, y) => {
                        let (x, y) = (eval(&vals, *x), eval(&vals, *y));
                        match bits {
                            Some(bits) => {
                                let r = match op {
                                    Bin::Add => x.wrapping_add(y),
                                    Bin::Sub => x.wrapping_sub(y),
                                    Bin::Mul => x.wrapping_mul(y),
                                    Bin::And => x & y,
                                    Bin::Or => x | y,
                                    Bin::Xor => x ^ y,
                                    Bin::Shl => x << y,
                                    Bin::LShr => x >> y,
                                    Bin::AShr => (signed(bits, x) >> y) as u64,
                                    _ => panic!("no divisions here"),
                                };
                                r & mask(bits)
                            }
                            None => {
                                let (x, y) = (float(x), float(y));
                                let r = match op {
                                    Bin::Add => x + y,
                                    Bin::Sub => x - y,
                                    _ => x * y,
                                };
                                u64::from(r.to_bits())
                            }
                        }
                    }
                    Op::Cmp(p, x, y) => {
                        let wide = m.int_bits(m.type_of(*x));
                        let (x, y) = (eval(&vals, *x), eval(&vals, *y));
                        let holds = match wide {
                            Some(bits) => {
                                let (sx, sy) = (signed(bits, x), signed(bits, y));
                                match *p {
                                    pred::EQ => x == y,
                                    pred::NE => x != y,
                                    pred::IUGT => x > y,
                                    pred::IUGE => x >= y,
                                    pred::IULT => x < y,
                                    pred::IULE => x <= y,
                                    pred::SGT => sx > sy,
                                    pred::SGE => sx >= sy,
                                    pred::SLT => sx < sy,
                                    _ => sx <= sy,
                                }
                            }
                            None => {
                                let (x, y) = (float(x), float(y));
                                if x.is_nan() || y.is_nan() {
                                    p & 8 != 0
                                } else {
                                    (p & 1 != 0 && x == y)
                                        || (p & 2 != 0 && x > y)
                                        || (p & 4 != 0 && x < y)
                                }
                            }
                        };
                        u64::from(holds)
                    }
                    Op::Select(c, t, f) => {
                        if eval(&vals, *c) != 0 {
                            eval(&vals, *t)
                        } else {
                            eval(&vals, *f)
                        }
                    }
                    Op::Cast(op, x) => {
                        let from_bits = m.int_bits(m.type_of(*x));
                        let x = eval(&vals, *x);
                        match (op, from_bits, bits) {
                            (Cast::Bitcast, _, _) => x,
                            (Cast::ZExt, _, _) => x,
                            (Cast::SExt, Some(f), Some(t)) => signed(f, x) as u64 & mask(t),
                            (Cast::Trunc, _, Some(t)) => x & mask(t),
                            _ => panic!("a cast the generator does not make"),
                        }
                    }
                    Op::Call(f, args) => {
                        let a: Vec<u64> = args.iter().map(|&v| eval(&vals, v)).collect();
                        let name = m.funcs[*f as usize].name.as_str();
                        match name {
                            "dx.op.loadInput.i32" => {
                                u64::from(inputs[(a[0] ^ a[1]) as usize % inputs.len()])
                            }
                            "dx.op.binary.i32" => a[0].min(a[1]),
                            "dx.op.cbufferLoadLegacy.i32" => u64::from(
                                inputs[a[0] as usize % inputs.len()].rotate_left(a[1] as u32 & 31),
                            ),
                            "dx.op.bufferLoad.i32" => memory[(a[0] ^ a[1]) as usize % 16],
                            "dx.op.waveReadLaneFirst.i32" => a[0] ^ a[1],
                            _ => {
                                memory[a[0] as usize % 16] = a[1];
                                trace.push((name.to_owned(), a));
                                0
                            }
                        }
                    }
                    _ => panic!("an instruction the generator does not make"),
                };
                vals[i as usize] = Some(r);
            }
            from = b;
            b = match blk.term.expect("a terminator") {
                Term::Ret => {
                    trace.push(("ret".into(), Vec::new()));
                    return trace;
                }
                Term::Br(t) => t,
                Term::CondBr(c, t, f) => {
                    if eval(&vals, c) != 0 {
                        t
                    } else {
                        f
                    }
                }
            };
        }
        trace.push(("still running".into(), Vec::new()));
        trace
    }

    /// A random structured program over a few registers, as the lowering
    /// makes them: every register joined at every join, conditions tested
    /// again behind their branch, values computed again.
    struct Gen {
        k: Kit,
        rng: u64,
        regs: Vec<V>,
        conds: Vec<V>,
        cur: usize,
        input: V,
        table: V,
        wave: V,
        /// The operands of the block's last read (to read the place again,
        /// and to store to it).
        last: Option<(V, V)>,
    }

    impl Gen {
        fn next(&mut self, n: u64) -> u64 {
            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 7;
            self.rng ^= self.rng << 17;
            (self.rng >> 11) % n
        }

        fn reg(&mut self) -> V {
            let n = self.next(self.regs.len() as u64) as usize;
            self.regs[n]
        }

        /// A register or a small constant.
        fn value(&mut self) -> V {
            match self.next(4) {
                0 => {
                    let k = [0u32, 1, 2, 31, u32::MAX, 0x8000_0000, 7][self.next(7) as usize];
                    self.k.m.i32(k)
                }
                _ => self.reg(),
            }
        }

        fn cond(&mut self) -> V {
            let c = self.cur;
            let made = match self.next(6) {
                0 if !self.conds.is_empty() => {
                    let n = self.next(self.conds.len() as u64) as usize;
                    return self.conds[n];
                }
                1 if self.conds.len() > 1 => {
                    let (a, b) = (
                        self.next(self.conds.len() as u64),
                        self.next(self.conds.len() as u64),
                    );
                    let op = [Bin::And, Bin::Or, Bin::Xor][self.next(3) as usize];
                    self.k
                        .m
                        .bin(c, op, self.conds[a as usize], self.conds[b as usize])
                }
                2 if !self.conds.is_empty() => {
                    let n = self.next(self.conds.len() as u64) as usize;
                    let t = self.k.m.bool(true);
                    self.k.m.bin(c, Bin::Xor, self.conds[n], t)
                }
                _ => {
                    let p = pred::EQ + self.next(10) as u8;
                    let (x, y) = (self.reg(), self.value());
                    self.k.m.cmp(c, p, x, y)
                }
            };
            self.conds.push(made);
            made
        }

        fn statement(&mut self) {
            let c = self.cur;
            let d = self.next(self.regs.len() as u64) as usize;
            let i32t = self.k.m.int(32);
            let f32t = self.k.m.float();
            let r = match self.next(14) {
                0..=3 => {
                    let op = [Bin::Add, Bin::Sub, Bin::Mul, Bin::And, Bin::Or, Bin::Xor]
                        [self.next(6) as usize];
                    let (x, y) = (self.value(), self.value());
                    self.k.m.bin(c, op, x, y)
                }
                4 => {
                    let op = [Bin::Shl, Bin::LShr, Bin::AShr][self.next(3) as usize];
                    let x = self.value();
                    let by = self.next(32) as u32;
                    let by = self.k.m.i32(by);
                    self.k.m.bin(c, op, x, by)
                }
                5 | 6 => {
                    // A guarded write: the old value where the guard fails.
                    let g = self.cond();
                    let x = self.value();
                    self.k.m.select(self.cur, g, x, self.regs[d])
                }
                7 => {
                    let (x, y) = (self.value(), self.value());
                    let f = [self.k.pure, self.input, self.table, self.wave][self.next(4) as usize];
                    self.k.m.call(c, f, vec![x, y])
                }
                8 | 9 => {
                    let again = self.next(2) == 0;
                    let (x, y) = match self.last {
                        Some(last) if again => last,
                        _ => (self.value(), self.value()),
                    };
                    self.last = Some((x, y));
                    self.k.m.call(c, self.k.reads, vec![x, y])
                }
                10 => {
                    // A store, half the time to the place read last.
                    let (mut x, y) = (self.value(), self.value());
                    let there = self.next(2) == 0;
                    if let Some((a, b)) = self.last
                        && there
                    {
                        x = self.k.m.bin(c, Bin::Xor, a, b);
                    }
                    self.k.m.call(c, self.k.writes, vec![x, y]);
                    return;
                }
                11 => {
                    // Float arithmetic, read back as bits.
                    let (x, y) = (self.value(), self.value());
                    let (x, y) = (
                        self.k.m.cast(c, Cast::Bitcast, x, f32t),
                        self.k.m.cast(c, Cast::Bitcast, y, f32t),
                    );
                    let op = [Bin::Add, Bin::Sub, Bin::Mul][self.next(3) as usize];
                    let f = self.k.m.bin(c, op, x, y);
                    if self.next(2) == 0 {
                        let p = self.next(16) as u8;
                        let t = self.k.m.cmp(c, p, f, y);
                        self.conds.push(t);
                    }
                    self.k.m.cast(c, Cast::Bitcast, f, i32t)
                }
                12 => {
                    let g = self.cond();
                    self.k.m.cast(self.cur, Cast::ZExt, g, i32t)
                }
                _ => {
                    let x = self.value();
                    let f = self.k.m.cast(c, Cast::Bitcast, x, f32t);
                    self.k.m.cast(c, Cast::Bitcast, f, i32t)
                }
            };
            self.regs[d] = r;
        }

        fn code(&mut self, depth: u32) {
            for _ in 0..1 + self.next(6) {
                match self.next(8) {
                    0 | 1 if depth > 0 => self.branch(depth - 1),
                    2 if depth > 0 => self.repeat(depth - 1),
                    _ => self.statement(),
                }
            }
        }

        /// if / else, every register joined.
        fn branch(&mut self, depth: u32) {
            let c = self.cond();
            let i32t = self.k.m.int(32);
            let (then, other, join) = (
                self.k.m.new_block(),
                self.k.m.new_block(),
                self.k.m.new_block(),
            );
            self.k.m.terminate(self.cur, Term::CondBr(c, then, other));
            let (before, known) = (self.regs.clone(), self.conds.len());
            self.cur = then;
            self.last = None;
            self.code(depth);
            let (then_regs, then_end) = (std::mem::replace(&mut self.regs, before), self.cur);
            self.k.m.terminate(then_end, Term::Br(join));
            self.conds.truncate(known);
            self.cur = other;
            self.last = None;
            if self.next(3) != 0 {
                self.code(depth);
            }
            let other_end = self.cur;
            self.k.m.terminate(other_end, Term::Br(join));
            self.conds.truncate(known);
            for (r, &t) in then_regs.iter().enumerate() {
                let p = self.k.m.phi(join, i32t);
                self.k
                    .m
                    .set_phi(p, vec![(t, then_end), (self.regs[r], other_end)]);
                self.regs[r] = p;
            }
            self.cur = join;
            self.last = None;
        }

        /// A loop that runs at most a few times, every register joined at
        /// its head.
        fn repeat(&mut self, depth: u32) {
            let i32t = self.k.m.int(32);
            let (head, exit) = (self.k.m.new_block(), self.k.m.new_block());
            let before = self.cur;
            self.k.m.terminate(before, Term::Br(head));
            let entering = self.regs.clone();
            let joins: Vec<V> = (0..self.regs.len())
                .map(|_| self.k.m.phi(head, i32t))
                .collect();
            let count = self.k.m.phi(head, i32t);
            self.regs = joins.clone();
            let known = self.conds.len();
            self.cur = head;
            self.last = None;
            self.code(depth);
            let one = self.k.m.i32(1);
            let next = self.k.m.bin(self.cur, Bin::Add, count, one);
            let limit = 1 + self.next(3) as u32;
            let limit = self.k.m.i32(limit);
            let mut more = self.k.m.cmp(self.cur, pred::IULT, next, limit);
            if self.next(2) == 0 {
                let c = self.cond();
                more = self.k.m.bin(self.cur, Bin::And, more, c);
            }
            let latch = self.cur;
            self.k.m.terminate(latch, Term::CondBr(more, head, exit));
            for (r, &p) in joins.iter().enumerate() {
                self.k
                    .m
                    .set_phi(p, vec![(entering[r], before), (self.regs[r], latch)]);
            }
            let zero = self.k.m.i32(0);
            self.k.m.set_phi(count, vec![(zero, before), (next, latch)]);
            self.conds.truncate(known);
            self.cur = exit;
            self.last = None;
        }
    }

    fn random_program(seed: u64) -> Module {
        let mut k = kit();
        let i32t = k.m.int(32);
        let fty = k.m.func_ty(i32t, vec![i32t, i32t]);
        let input =
            k.m.function("dx.op.loadInput.i32", fty, Some(Attr::ReadNone), false);
        let table = k.m.function(
            "dx.op.cbufferLoadLegacy.i32",
            fty,
            Some(Attr::ReadOnly),
            false,
        );
        let wave = k.m.function(
            "dx.op.waveReadLaneFirst.i32",
            fty,
            Some(Attr::NoUnwind),
            false,
        );
        let b = k.b;
        let mut g = Gen {
            k,
            rng: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
            regs: Vec::new(),
            conds: Vec::new(),
            cur: b,
            input,
            table,
            wave,
            last: None,
        };
        for r in 0..5 {
            let (x, y) = (g.k.m.i32(r), g.k.m.i32(r * 3));
            let v = g.k.m.call(b, input, vec![x, y]);
            g.regs.push(v);
        }
        g.code(3);
        g.code(2);
        // Every register is read at the end.
        for r in 0..g.regs.len() {
            let at = g.k.m.i32(r as u32);
            g.k.m.call(g.cur, g.k.writes, vec![at, g.regs[r]]);
        }
        g.k.m.terminate(g.cur, Term::Ret);
        g.k.m
    }

    #[test]
    fn every_pass_leaves_what_random_programs_store() {
        let mut removed = 0;
        for seed in 1..=400u64 {
            let plain = random_program(seed);
            let inputs: Vec<Vec<u32>> = (0..6u32)
                .map(|n| {
                    (0..7u32)
                        .map(|i| match n {
                            0 => 0,
                            1 => i,
                            2 => u32::MAX - i,
                            _ => {
                                (seed as u32 ^ n)
                                    .wrapping_mul(0x9E37_79B9)
                                    .rotate_left(i * 5)
                                    ^ (i << 28)
                            }
                        })
                        .collect()
                })
                .collect();
            let expected: Vec<_> = inputs.iter().map(|i| run(&plain, i)).collect();
            let written: usize = plain
                .blocks
                .iter()
                .map(|b| b.body.len() + b.phis.len())
                .sum();
            for passes in [
                DEAD,
                FOLD,
                REUSE,
                FACTS,
                LOADS,
                LICM,
                BARRIERS,
                FOLD | FACTS,
                FOLD | REUSE | DEAD,
                LOADS | LICM | BARRIERS,
                DEFAULT,
                ALL,
            ] {
                let mut m = random_program(seed);
                m.optimise(passes);
                for (i, want) in inputs.iter().zip(&expected) {
                    assert_eq!(&run(&m, i), want, "seed {seed}, passes {passes}");
                }
                if passes == ALL {
                    let left: usize = m.blocks.iter().map(|b| b.body.len() + b.phis.len()).sum();
                    removed += written - left;
                }
                m.write();
            }
        }
        // The programs are of the wasteful kind: the passes find work.
        assert!(removed > 4000, "{removed} instructions removed");
    }

    #[test]
    fn extended_fold_algebraic_bitwise_shifts_and_strength_reduction() {
        let Kit {
            mut m,
            b,
            reads,
            writes,
            ..
        } = kit();
        let (k0, k1, k2, k3, k4, k5, k6, k8, k16, k20, k32) = (
            m.i32(0),
            m.i32(1),
            m.i32(2),
            m.i32(3),
            m.i32(4),
            m.i32(5),
            m.i32(6),
            m.i32(8),
            m.i32(16),
            m.i32(20),
            m.i32(32),
        );
        let x = m.call(b, reads, vec![k1, k1]);
        let y = m.call(b, reads, vec![k2, k2]);

        // x - x => 0, x ^ x => 0, (x ^ c) ^ c => x
        let sub_self = m.bin(b, Bin::Sub, x, x);
        let xor_self = m.bin(b, Bin::Xor, x, x);
        let xc = m.bin(b, Bin::Xor, x, k5);
        let xcc = m.bin(b, Bin::Xor, xc, k5);
        // (x + c1) + c2 => x + (c1 + c2), (x + c) - c => x, (x - c1) - c2 => x - (c1 + c2)
        let add1 = m.bin(b, Bin::Add, x, k3);
        let add2 = m.bin(b, Bin::Add, add1, k4);
        let add_cancel = m.bin(b, Bin::Sub, add1, k3);
        let sub1 = m.bin(b, Bin::Sub, x, k3);
        let sub2 = m.bin(b, Bin::Sub, sub1, k4);
        let wrap_hi = m.i32(u32::MAX);
        let wrap1 = m.bin(b, Bin::Add, x, wrap_hi);
        let wrap_zero = m.bin(b, Bin::Add, wrap1, k1);
        // (x & c1) & c2 => x & (c1 & c2), (x | c1) | c2 => x | (c1 | c2)
        let m0f = m.i32(0x0F);
        let m3c = m.i32(0x3C);
        let and1 = m.bin(b, Bin::And, x, m0f);
        let and2 = m.bin(b, Bin::And, and1, m3c);
        let or1 = m.bin(b, Bin::Or, x, m0f);
        let or2 = m.bin(b, Bin::Or, or1, m3c);
        // Shifts: (x << 3) << 4 => x << 7, (x >>u 5) >>u 6 => x >>u 11, (x << 20) << 20 => 0
        let shl1 = m.bin(b, Bin::Shl, x, k3);
        let shl2 = m.bin(b, Bin::Shl, shl1, k4);
        let lshr1 = m.bin(b, Bin::LShr, x, k5);
        let lshr2 = m.bin(b, Bin::LShr, lshr1, k6);
        let shl_ov1 = m.bin(b, Bin::Shl, x, k20);
        let shl_ov2 = m.bin(b, Bin::Shl, shl_ov1, k20);
        // Negative control: shift by >= 32 must NOT fold.
        let bad_sh = m.bin(b, Bin::Shl, x, k32);
        let bad_sh2 = m.bin(b, Bin::Shl, bad_sh, k1);
        // Strength reduction: x * 8 => x << 3, x /u 8 => x >>u 3, x %u 16 => x & 15, x %u 1 => 0
        let mul_p2 = m.bin(b, Bin::Mul, x, k8);
        let udiv_p2 = m.bin(b, Bin::UDiv, x, k8);
        let urem_p2 = m.bin(b, Bin::URem, x, k16);
        let urem_1 = m.bin(b, Bin::URem, x, k1);
        // Negative controls: x * 6, x /s 8, x /u 0, x %u 0 must stay as written.
        let mul_6 = m.bin(b, Bin::Mul, y, k6);
        let sdiv_8 = m.bin(b, Bin::SDiv, y, k8);
        let udiv_0 = m.bin(b, Bin::UDiv, y, k0);
        let urem_0 = m.bin(b, Bin::URem, y, k0);

        for (p, q) in [
            (sub_self, xor_self),
            (xcc, add_cancel),
            (add2, sub2),
            (wrap_zero, and2),
            (or2, shl2),
            (lshr2, shl_ov2),
            (bad_sh2, mul_p2),
            (udiv_p2, urem_p2),
            (urem_1, mul_6),
            (sdiv_8, udiv_0),
            (urem_0, k0),
        ] {
            m.call(b, writes, vec![p, q]);
        }
        m.terminate(b, Term::Ret);
        m.optimise(FOLD | DEAD);

        let st = stores(&m, b);
        assert_eq!(st[0], [k0, k0], "x - x and x ^ x fold to 0");
        assert_eq!(st[1], [x, x], "(x ^ c) ^ c and (x + c) - c fold to x");
        assert_eq!(st[3][0], x, "(x + 0xFFFF_FFFF) + 1 wraps to x");
        assert_eq!(st[5][1], k0, "(x << 20) << 20 folds to 0");
        assert_eq!(st[8][0], k0, "x %u 1 folds to 0");

        let k7 = m.i32(7);
        let k11 = m.i32(11);
        let k15 = m.i32(15);
        let k0c = m.i32(0x0C);
        let k3f = m.i32(0x3F);
        let inst_op = |v: V| match v {
            V::Inst(id) => m.insts[id as usize].op.clone(),
            other => panic!("expected inst, got {other:?}"),
        };
        assert!(matches!(inst_op(st[2][0]), Op::Bin(Bin::Add, a, c) if a == x && c == k7));
        assert!(matches!(inst_op(st[2][1]), Op::Bin(Bin::Sub, a, c) if a == x && c == k7));
        assert!(matches!(inst_op(st[3][1]), Op::Bin(Bin::And, a, c) if a == x && c == k0c));
        assert!(matches!(inst_op(st[4][0]), Op::Bin(Bin::Or, a, c) if a == x && c == k3f));
        assert!(matches!(inst_op(st[4][1]), Op::Bin(Bin::Shl, a, c) if a == x && c == k7));
        assert!(matches!(inst_op(st[5][0]), Op::Bin(Bin::LShr, a, c) if a == x && c == k11));
        assert!(matches!(inst_op(st[6][0]), Op::Bin(Bin::Shl, a, c) if a == bad_sh && c == k1));
        assert!(matches!(inst_op(st[6][1]), Op::Bin(Bin::Shl, a, c) if a == x && c == k3));
        assert!(matches!(inst_op(st[7][0]), Op::Bin(Bin::LShr, a, c) if a == x && c == k3));
        assert!(matches!(inst_op(st[7][1]), Op::Bin(Bin::And, a, c) if a == x && c == k15));
        assert!(matches!(inst_op(st[8][1]), Op::Bin(Bin::Mul, a, c) if a == y && c == k6));
        assert!(matches!(inst_op(st[9][0]), Op::Bin(Bin::SDiv, a, c) if a == y && c == k8));
        assert!(matches!(inst_op(st[9][1]), Op::Bin(Bin::UDiv, a, c) if a == y && c == k0));
        assert!(matches!(inst_op(st[10][0]), Op::Bin(Bin::URem, a, c) if a == y && c == k0));
        m.write();
    }

    #[test]
    fn compare_select_zext_fold_and_nan_safety() {
        let Kit {
            mut m,
            b,
            reads,
            writes,
            ..
        } = kit();
        let (i32t, f32t) = (m.int(32), m.float());
        let (k0, k1, k2, k10, k20, k30) = (
            m.i32(0),
            m.i32(1),
            m.i32(2),
            m.i32(10),
            m.i32(20),
            m.i32(30),
        );
        let (t, f) = (m.bool(true), m.bool(false));
        let x = m.call(b, reads, vec![k1, k1]);
        let y = m.call(b, reads, vec![k2, k2]);
        let xf = m.cast(b, Cast::Bitcast, x, f32t);
        let yf = m.cast(b, Cast::Bitcast, y, f32t);

        // xor(icmp(IULT, x, y), true) => icmp(IUGE, x, y)
        let ic = m.cmp(b, pred::IULT, x, y);
        let nic = m.bin(b, Bin::Xor, ic, t);
        // Canonical REUSE: `icmp(IULE, y, x)` equals `icmp(IUGE, x, y)`!
        let mir_ic = m.cmp(b, pred::IULE, y, x);
        // xor(fcmp(OLT, xf, yf), true) => fcmp(UGE, xf, yf)
        let fc = m.cmp(b, pred::OLT, xf, yf);
        let nfc = m.bin(b, Bin::Xor, fc, t);
        // Canonical float REUSE: `fcmp(ULE, yf, xf)` equals `fcmp(UGE, xf, yf)`!
        let mir_fc = m.cmp(b, pred::ULE, yf, xf);
        // Negative control (AMD driver): xor(fcmp(UEQ, xf, yf), true) MUST NOT become fcmp(ONE)!
        let ueq = m.cmp(b, pred::UEQ, xf, yf);
        let nueq = m.bin(b, Bin::Xor, ueq, t);
        // Negative control (NaN): fcmp(OEQ, xf, xf) MUST NOT fold to true!
        let nan_eq = m.cmp(b, pred::OEQ, xf, xf);
        let nan_une = m.cmp(b, pred::UNE, xf, xf);

        // select(xor(opaque_bool, true), x, y) => select(opaque_bool, y, x)
        let i1t = m.int(1);
        let opaque_b = m.cast(b, Cast::Trunc, x, i1t);
        let not_opaque = m.bin(b, Bin::Xor, opaque_b, t);
        let sel_flip = m.select(b, not_opaque, x, y);
        // Boolean selects:
        let c2 = m.cmp(b, pred::EQ, x, k10);
        let b2 = m.cmp(b, pred::EQ, y, k20);
        let sel_or = m.select(b, c2, t, b2);
        let sel_and = m.select(b, c2, b2, f);
        let sel_not = m.select(b, c2, f, t);
        // icmp (select(c, k10, k20), k)
        let sel_k = m.select(b, c2, k10, k20);
        let eq_10 = m.cmp(b, pred::EQ, sel_k, k10);
        let eq_20 = m.cmp(b, pred::EQ, sel_k, k20);
        let eq_30 = m.cmp(b, pred::EQ, sel_k, k30);
        // icmp (zext i1 c2, 0/1) & and(zext i1 c2, 1/2)
        let z = m.cast(b, Cast::ZExt, c2, i32t);
        let z_eq0 = m.cmp(b, pred::EQ, z, k0);
        let z_eq1 = m.cmp(b, pred::EQ, z, k1);
        let z_ne1 = m.cmp(b, pred::NE, k1, z);
        let z_and1 = m.bin(b, Bin::And, z, k1);
        let z_and2 = m.bin(b, Bin::And, z, k2);

        let w = |m: &mut Module, v: V| m.cast(b, Cast::ZExt, v, i32t);
        let vals = [
            (w(&mut m, nic), w(&mut m, mir_ic)),
            (w(&mut m, nfc), w(&mut m, mir_fc)),
            (w(&mut m, nueq), w(&mut m, nan_eq)),
            (w(&mut m, nan_une), sel_flip),
            (w(&mut m, sel_or), w(&mut m, sel_and)),
            (w(&mut m, sel_not), w(&mut m, eq_10)),
            (w(&mut m, eq_20), w(&mut m, eq_30)),
            (w(&mut m, z_eq0), w(&mut m, z_eq1)),
            (w(&mut m, z_ne1), z_and1),
            (z_and2, k0),
        ];
        for (p, q) in vals {
            m.call(b, writes, vec![p, q]);
        }
        m.terminate(b, Term::Ret);
        m.optimise(FOLD | REUSE | DEAD);

        // Verify exact semantic equivalence with NaN, -0.0, +0.0, finite inputs via interpreter.
        let st = stores(&m, b);
        assert_eq!(st[0][0], st[0][1], "icmp IUGE(x,y) merges with IULE(y,x)");
        assert_eq!(st[1][0], st[1][1], "fcmp UGE(xf,yf) merges with ULE(yf,xf)");
        // Check that `pred::ONE` never appears anywhere in the module.
        for &id in &m.blocks[b].body {
            if let Op::Cmp(p, _, _) = m.insts[id as usize].op {
                assert_ne!(p, pred::ONE, "must never emit fcmp ONE (AMD driver bug)");
            }
        }
        // Check `nan_eq` and `nan_une` did NOT fold to constants.
        assert!(matches!(st[2][1], V::Inst(_)), "fcmp OEQ(xf, xf) must stay");
        assert!(matches!(st[3][0], V::Inst(_)), "fcmp UNE(xf, xf) must stay");
        // Check `sel_flip` became `select(opaque_b, y, x)` (not using `not_opaque`).
        let V::Inst(sf) = st[3][1] else { panic!() };
        assert!(
            matches!(m.insts[sf as usize].op, Op::Select(c, a, z) if (c, a, z) == (opaque_b, y, x))
        );
        // Check boolean selects lowered to `Or`, `And`, and `Cmp(NE, x, k10)`.
        let unwrap_zext = |v: V| match v {
            V::Inst(id) => match m.insts[id as usize].op {
                Op::Cast(Cast::ZExt, inner) => inner,
                _ => panic!("expected zext, got {:?}", m.insts[id as usize].op),
            },
            _ => panic!("expected inst, got {v:?}"),
        };
        let V::Inst(or_id) = unwrap_zext(st[4][0]) else { panic!() };
        assert!(matches!(m.insts[or_id as usize].op, Op::Bin(Bin::Or, a, b) if (a, b) == (c2, b2)));
        let V::Inst(and_id) = unwrap_zext(st[4][1]) else { panic!() };
        assert!(matches!(m.insts[and_id as usize].op, Op::Bin(Bin::And, a, b) if (a, b) == (c2, b2)));
        let V::Inst(not_id) = unwrap_zext(st[5][0]) else { panic!() };
        assert!(matches!(m.insts[not_id as usize].op, Op::Cmp(pred::NE, a, k) if (a, k) == (x, k10)));
        assert_eq!(unwrap_zext(st[5][1]), c2, "eq(select(c2,10,20),10) => c2");
        assert_eq!(unwrap_zext(st[6][0]), V::Inst(not_id), "eq(select(c2,10,20),20) => !c2");
        assert_eq!(st[6][1], k0, "eq(select(c2,10,20),30) => false");
        assert_eq!(unwrap_zext(st[7][0]), V::Inst(not_id), "eq(zext c2, 0) => !c2");
        assert_eq!(unwrap_zext(st[7][1]), c2, "eq(zext c2, 1) => c2");
        assert_eq!(unwrap_zext(st[8][0]), V::Inst(not_id), "ne(1, zext c2) => !c2");
        assert_eq!(st[8][1], z, "and(zext c2, 1) => zext c2");
        assert_eq!(st[9][0], k0, "and(zext c2, 2) => 0");
        m.write();
    }

    #[test]
    fn fcmp_opposite_matches_ieee754_for_all_predicates_including_nans() {
        let samples = [
            0.0f32.to_bits(),
            (-0.0f32).to_bits(),
            1.0f32.to_bits(),
            (-2.5f32).to_bits(),
            f32::INFINITY.to_bits(),
            f32::NEG_INFINITY.to_bits(),
            f32::NAN.to_bits(),
            0x7F80_0001u32, // sNaN
        ];
        for p in 0..=15u8 {
            let build = || {
                let mut k = kit();
                let (i32t, f32t) = (k.m.int(32), k.m.float());
                let fty = k.m.func_ty(i32t, vec![i32t, i32t]);
                let inp = k.m.function("dx.op.loadInput.i32", fty, Some(Attr::ReadNone), false);
                let (k0, k1) = (k.m.i32(0), k.m.i32(1));
                let a = k.m.call(k.b, inp, vec![k0, k0]);
                let b = k.m.call(k.b, inp, vec![k1, k0]);
                let af = k.m.cast(k.b, Cast::Bitcast, a, f32t);
                let bf = k.m.cast(k.b, Cast::Bitcast, b, f32t);
                let c = k.m.cmp(k.b, p, af, bf);
                let yes = k.m.bool(true);
                let nc = k.m.bin(k.b, Bin::Xor, c, yes);
                let z = k.m.cast(k.b, Cast::ZExt, nc, i32t);
                k.m.call(k.b, k.writes, vec![k0, z]);
                k.m.terminate(k.b, Term::Ret);
                k.m
            };
            let plain = build();
            let mut opt = build();
            opt.optimise(FOLD | REUSE | DEAD);
            for &x in &samples {
                for &y in &samples {
                    assert_eq!(
                        run(&opt, &[x, y]),
                        run(&plain, &[x, y]),
                        "pred {p}, x={x:#x}, y={y:#x}"
                    );
                }
            }
        }
    }

    #[test]
    fn canonical_facts_propagate_flipped_comparisons() {
        let Diamond {
            k: Kit { mut m, writes, .. },
            x,
            y,
            then,
            other,
            ..
        } = diamond();
        // Entry branches on `x <u y`.
        // In `then`: `y >u x` is true, `y <=u x` is false, `x >=u y` is false.
        let flipped_true = m.cmp(then, pred::IUGT, y, x);
        let flipped_false = m.cmp(then, pred::IULE, y, x);
        let s1 = m.select(then, flipped_true, x, y);
        let s2 = m.select(then, flipped_false, x, y);
        m.call(then, writes, vec![s1, s2]);
        // In `other`: `y <=u x` is true, `y >u x` is false.
        let o1 = m.cmp(other, pred::IULE, y, x);
        let o2 = m.cmp(other, pred::IUGT, y, x);
        let s3 = m.select(other, o1, x, y);
        let s4 = m.select(other, o2, x, y);
        m.call(other, writes, vec![s3, s4]);
        m.optimise(FOLD | FACTS | DEAD);
        assert_eq!(stores(&m, then), [[x, y]]);
        assert_eq!(stores(&m, other), [[x, y]]);
        m.write();
    }

    #[test]
    fn licm_hoists_pure_invariants_splits_cond_entry_and_respects_traps() {
        // Part 1: conditional entry + nested invariant chain + safe vs unsafe div/rem + reads/lanes.
        let build = || {
            let mut k = kit();
            let i32t = k.m.int(32);
            let fty = k.m.func_ty(i32t, vec![i32t, i32t]);
            let inp = k.m.function("dx.op.loadInput.i32", fty, Some(Attr::ReadNone), false);
            let cb = k.m.function("dx.op.cbufferLoadLegacy.i32", fty, Some(Attr::ReadOnly), false);
            let wave = k.m.function("dx.op.waveReadLaneFirst.i32", fty, Some(Attr::NoUnwind), false);
            let (k0, k1, k3, k7, km1) = (
                k.m.i32(0),
                k.m.i32(1),
                k.m.i32(3),
                k.m.i32(7),
                k.m.i32(u32::MAX),
            );
            let n = k.m.call(k.b, inp, vec![k0, k0]);
            let a = k.m.call(k.b, inp, vec![k1, k0]);
            let d = k.m.call(k.b, inp, vec![k1, k1]); // potentially 0!
            let enter = k.m.cmp(k.b, pred::IUGT, n, k0);
            let (head, exit) = (k.m.new_block(), k.m.new_block());
            k.m.terminate(k.b, Term::CondBr(enter, head, exit));

            let idx = k.m.phi(head, i32t);
            let acc = k.m.phi(head, i32t);
            // Pure loop-invariant chain: must hoist into lazily split preheader.
            let inv1 = k.m.bin(head, Bin::Mul, a, k3);
            let inv2 = k.m.bin(head, Bin::Add, inv1, k7);
            let inv_div_safe = k.m.bin(head, Bin::UDiv, inv2, k3);
            let inv_cb = k.m.call(head, cb, vec![k0, k1]);
            // Negative controls: MUST NOT hoist!
            let unsafe_udiv = k.m.bin(head, Bin::UDiv, a, d);
            let unsafe_sdiv_m1 = k.m.bin(head, Bin::SDiv, a, km1);
            let mem_read = k.m.call(head, k.reads, vec![k0, k1]);
            let lane_op = k.m.call(head, wave, vec![a, k1]);
            // Combine everything into loop accumulator.
            let t0 = k.m.bin(head, Bin::Add, acc, inv_div_safe);
            let t1 = k.m.bin(head, Bin::Add, t0, inv_cb);
            let t2 = k.m.bin(head, Bin::Add, t1, mem_read);
            let t3 = k.m.bin(head, Bin::Add, t2, lane_op);
            let t4 = k.m.bin(head, Bin::Xor, t3, unsafe_udiv);
            let next_acc = k.m.bin(head, Bin::Xor, t4, unsafe_sdiv_m1);
            let next_idx = k.m.bin(head, Bin::Add, idx, k1);
            let more = k.m.cmp(head, pred::IULT, next_idx, n);
            k.m.terminate(head, Term::CondBr(more, head, exit));
            k.m.set_phi(idx, vec![(k0, k.b), (next_idx, head)]);
            k.m.set_phi(acc, vec![(k0, k.b), (next_acc, head)]);

            let final_acc = k.m.phi(exit, i32t);
            k.m.set_phi(final_acc, vec![(k0, k.b), (next_acc, head)]);
            k.m.call(exit, k.writes, vec![k0, final_acc]);
            k.m.terminate(exit, Term::Ret);
            (k.m, k.b, head, inv1, inv2, inv_div_safe, inv_cb, unsafe_udiv, unsafe_sdiv_m1, mem_read, lane_op)
        };

        let (mut m, entry, head, inv1, inv2, inv_div_safe, inv_cb, unsafe_udiv, unsafe_sdiv_m1, mem_read, lane_op) =
            build();
        m.optimise(LICM);
        // Edge `entry -> head` must have been split into a new preheader block.
        let Some(Term::CondBr(_, ph, _)) = m.blocks[entry].term else {
            panic!("entry should stay CondBr");
        };
        assert_ne!(ph, head, "conditional entry must split a dedicated preheader");
        assert!(matches!(m.blocks[ph].term, Some(Term::Br(h)) if h == head));
        let inst_id = |v: V| match v {
            V::Inst(i) => i,
            _ => unreachable!(),
        };
        // Invariant chain hoisted to `ph` in definition order!
        for v in [inv1, inv2, inv_div_safe, inv_cb] {
            assert!(m.blocks[ph].body.contains(&inst_id(v)), "{v:?} should be hoisted to preheader");
            assert!(!m.blocks[head].body.contains(&inst_id(v)), "{v:?} should leave loop header");
        }
        // Negative controls stayed inside `head`!
        for v in [unsafe_udiv, unsafe_sdiv_m1, mem_read, lane_op] {
            assert!(m.blocks[head].body.contains(&inst_id(v)), "{v:?} must NOT be hoisted");
            assert!(!m.blocks[ph].body.contains(&inst_id(v)), "{v:?} must NOT be in preheader");
        }
        // Header phis updated to reference `ph` instead of `entry`!
        for &p in &m.blocks[head].phis {
            let Op::Phi(inc) = &m.insts[p as usize].op else { panic!() };
            assert!(inc.iter().any(|&(_, from)| from == ph), "phi must name split preheader");
            assert!(!inc.iter().any(|&(_, from)| from == entry), "phi must not name stale entry");
        }
        m.write();

        // Part 2: nested loop hoists doubly-invariant expression out of BOTH loops.
        let mut k = kit();
        let i32t = k.m.int(32);
        let (k0, k1, k4) = (k.m.i32(0), k.m.i32(1), k.m.i32(4));
        let x = k.m.call(k.b, k.reads, vec![k0, k1]);
        let (outer, inner, outer_latch, exit) = (
            k.m.new_block(),
            k.m.new_block(),
            k.m.new_block(),
            k.m.new_block(),
        );
        k.m.terminate(k.b, Term::Br(outer));
        let oi = k.m.phi(outer, i32t);
        k.m.terminate(outer, Term::Br(inner));
        let ii = k.m.phi(inner, i32t);
        let dbl_inv = k.m.bin(inner, Bin::Add, x, k4);
        let use_it = k.m.bin(inner, Bin::Add, dbl_inv, ii);
        k.m.call(inner, k.writes, vec![ii, use_it]);
        let next_ii = k.m.bin(inner, Bin::Add, ii, k1);
        let in_more = k.m.cmp(inner, pred::IULT, next_ii, k4);
        k.m.terminate(inner, Term::CondBr(in_more, inner, outer_latch));
        k.m.set_phi(ii, vec![(k0, outer), (next_ii, inner)]);
        let next_oi = k.m.bin(outer_latch, Bin::Add, oi, k1);
        let out_more = k.m.cmp(outer_latch, pred::IULT, next_oi, k4);
        k.m.terminate(outer_latch, Term::CondBr(out_more, outer, exit));
        k.m.set_phi(oi, vec![(k0, k.b), (next_oi, outer_latch)]);
        k.m.terminate(exit, Term::Ret);
        k.m.optimise(LICM);
        assert!(
            k.m.blocks[k.b].body.contains(&inst_id(dbl_inv)),
            "doubly-invariant op must hoist all the way to entry"
        );
        assert!(!k.m.blocks[outer].body.contains(&inst_id(dbl_inv)));
        assert!(!k.m.blocks[inner].body.contains(&inst_id(dbl_inv)));
        k.m.write();
    }

    #[test]
    fn local_and_shared_memory_forwarding_non_aliasing_and_dse() {
        let mut k = kit();
        let (i32t, void) = (k.m.int(32), k.m.void());
        let arr = k.m.array(4, i32t);
        let priv_a = k.m.global("priv_a", arr, 0);
        let priv_b = k.m.global("priv_b", arr, 0);
        let lds = k.m.global("lds", arr, 3);
        let bty = k.m.func_ty(void, vec![i32t, i32t]);
        k.m.ptr(bty);
        let barrier = k.m.function("dx.op.barrier", bty, Some(Attr::NoDuplicate), false);
        let (k0, k1, k2, k9, k80) = (k.m.i32(0), k.m.i32(1), k.m.i32(2), k.m.i32(9), k.m.i32(80));
        let v1 = k.m.call(k.b, k.reads, vec![k0, k0]);
        let v2 = k.m.call(k.b, k.reads, vec![k1, k1]);
        let dyn_idx = k.m.call(k.b, k.reads, vec![k2, k2]);

        // 1) Dead overwritten store on priv_a[0] before any read, plus non-aliasing
        // forwarding across priv_b store, LDS store, UAV bufferStore, and dx.op.barrier!
        let pa0 = k.m.element(k.b, priv_a, k0);
        let pa1 = k.m.element(k.b, priv_a, k1);
        let pb0 = k.m.element(k.b, priv_b, k0);
        let lds0 = k.m.element(k.b, lds, k0);
        let lds1 = k.m.element(k.b, lds, k1);
        k.m.store(k.b, pa0, v1); // dead: overwritten below!
        k.m.store(k.b, pa1, v2);
        k.m.store(k.b, pa0, v2); // overwrites pa0 -> keeps this, drops first!
        k.m.store(k.b, pb0, v1); // unrelated private global
        k.m.store(k.b, lds0, v1); // unrelated LDS
        // Space-3 store -> load MUST NOT forward (wave-ordered LDS exchange);
        // space-3 load -> load DOES reuse across non-aliasing UAV bufferStore.
        let lds_after_st = k.m.load(k.b, lds0);
        k.m.call(k.b, k.writes, vec![v1, v2]); // unrelated UAV bufferStore
        let lds_reuse = k.m.load(k.b, lds0); // reuses lds_after_st across UAV store!
        // Barrier invalidates LDS, but MUST NOT invalidate priv_a!
        k.m.call(k.b, barrier, vec![k80, k9]);
        let pa0_after_bar = k.m.load(k.b, pa0); // forwards v2 across barrier!
        let lds0_after_bar = k.m.load(k.b, lds0); // MUST NOT reuse across barrier!
        // Dynamic-indexed store to priv_a invalidates priv_a[0] & priv_a[1],
        // but NOT priv_b[0]!
        let pa_dyn = k.m.element(k.b, priv_a, dyn_idx);
        k.m.store(k.b, pa_dyn, v1);
        let pa0_after_dyn = k.m.load(k.b, pa0); // MUST NOT forward (invalidated)!
        let pb0_after_dyn = k.m.load(k.b, pb0); // forwards v1 (different global)!
        // LDS dead overwritten store with non-aliasing const load lds1 in between:
        k.m.store(k.b, lds0, v1); // dead: overwritten after non-aliasing load lds1
        let lds1_read = k.m.load(k.b, lds1);
        k.m.store(k.b, lds0, v2);
        // Negative control: dynamic LDS load between two LDS stores preserves earlier store!
        let lds_dyn = k.m.element(k.b, lds, dyn_idx);
        k.m.store(k.b, lds1, v1);
        let lds_dyn_read = k.m.load(k.b, lds_dyn);
        k.m.store(k.b, lds1, v2);

        k.m.call(k.b, k.writes, vec![lds_after_st, lds_reuse]);
        k.m.call(k.b, k.writes, vec![pa0_after_bar, lds0_after_bar]);
        k.m.call(k.b, k.writes, vec![pa0_after_dyn, pb0_after_dyn]);
        k.m.call(k.b, k.writes, vec![lds1_read, lds_dyn_read]);
        k.m.terminate(k.b, Term::Ret);
        k.m.optimise(LOADS | DEAD);

        let st = stores(&k.m, k.b);
        // st[0] is the earlier UAV store (v1, v2)
        assert_eq!(
            st[1],
            [lds_after_st, lds_after_st],
            "lds0 store->load must not forward; second load reuses first across UAV store"
        );
        assert_eq!(st[2][0], v2, "pa0 forwarded across barrier");
        assert_eq!(st[2][1], lds0_after_bar, "lds0 NOT reused across barrier");
        assert_eq!(st[3][0], pa0_after_dyn, "pa0 NOT forwarded across dynamic store to priv_a");
        assert_eq!(st[3][1], v1, "pb0 forwarded across dynamic store to priv_a");

        // Count remaining Op::Store instructions on pa0, lds0, lds1.
        let count_stores_to = |ptr: V| -> usize {
            k.m.blocks[k.b]
                .body
                .iter()
                .filter(|&&id| matches!(k.m.insts[id as usize].op, Op::Store(p, _) if p == ptr))
                .count()
        };
        assert_eq!(count_stores_to(pa0), 1, "first dead store to pa0 eliminated");
        assert_eq!(count_stores_to(lds0), 2, "pre-barrier lds0 kept; dead post-barrier lds0 dropped");
        assert_eq!(count_stores_to(lds1), 2, "intervening dynamic lds load preserves both lds1 stores");
        k.m.write();
    }

    #[test]
    fn escaped_private_global_and_loop_header_do_not_misforward() {
        // Escaped global passed to a call disables private-global forwarding.
        let mut k = kit();
        let i32t = k.m.int(32);
        let arr = k.m.array(2, i32t);
        let p = k.m.global("esc", arr, 0);
        let ptr_ty = k.m.ptr(i32t);
        let cty = k.m.func_ty(i32t, vec![ptr_ty]);
        k.m.ptr(cty);
        let escape_fn = k.m.function("dx.op.unknownMutate", cty, Some(Attr::NoUnwind), false);
        let (k0, k1, k2) = (k.m.i32(0), k.m.i32(1), k.m.i32(2));
        let p0 = k.m.element(k.b, p, k0);
        k.m.store(k.b, p0, k1);
        k.m.call(k.b, escape_fn, vec![p0]);
        let after = k.m.load(k.b, p0);
        k.m.call(k.b, k.writes, vec![after, k2]);
        k.m.terminate(k.b, Term::Ret);
        k.m.optimise(LOADS | DEAD);
        assert_eq!(stores(&k.m, k.b), [[after, k2]], "escaped global must reload after call");
        k.m.write();

        // Loop header (2 preds) must NOT inherit preheader private global value across backedge!
        let mut k = kit();
        let i32t = k.m.int(32);
        let arr = k.m.array(2, i32t);
        let priv_g = k.m.global("loop_priv", arr, 0);
        let (k0, k1, k4) = (k.m.i32(0), k.m.i32(1), k.m.i32(4));
        let p0 = k.m.element(k.b, priv_g, k0);
        k.m.store(k.b, p0, k0);
        let (head, exit) = (k.m.new_block(), k.m.new_block());
        k.m.terminate(k.b, Term::Br(head));
        let idx = k.m.phi(head, i32t);
        let cur = k.m.load(head, p0); // changes every iteration!
        let inc = k.m.bin(head, Bin::Add, cur, k1);
        k.m.store(head, p0, inc);
        let next = k.m.bin(head, Bin::Add, idx, k1);
        let more = k.m.cmp(head, pred::IULT, next, k4);
        k.m.terminate(head, Term::CondBr(more, head, exit));
        k.m.set_phi(idx, vec![(k0, k.b), (next, head)]);
        let final_v = k.m.load(exit, p0);
        k.m.call(exit, k.writes, vec![final_v, k0]);
        k.m.terminate(exit, Term::Ret);
        k.m.optimise(ALL);
        assert!(
            k.m.blocks[head]
                .body
                .iter()
                .any(|&id| matches!(k.m.insts[id as usize].op, Op::Load(_))),
            "load inside loop header must NOT be forwarded from preheader"
        );
        k.m.write();
    }

    #[test]
    fn adjacent_redundant_barrier_elimination_and_negative_controls() {
        let mut k = kit();
        let (i32t, void) = (k.m.int(32), k.m.void());
        let bty = k.m.func_ty(void, vec![i32t, i32t]);
        k.m.ptr(bty);
        let barrier = k.m.function("dx.op.barrier", bty, Some(Attr::NoDuplicate), false);
        let arr = k.m.array(4, i32t);
        let lds = k.m.global("lds", arr, 3);
        let (k0, k1, k8, k9, k11, k80) = (
            k.m.i32(0),
            k.m.i32(1),
            k.m.i32(8),
            k.m.i32(9),
            k.m.i32(11),
            k.m.i32(80),
        );
        let x = k.m.call(k.b, k.reads, vec![k0, k1]);
        // 1) barrier(11) then pure arithmetic then barrier(9): 11 covers 9 -> drop second!
        k.m.call(k.b, barrier, vec![k80, k11]);
        let pure_sum = k.m.bin(k.b, Bin::Add, x, k1);
        k.m.call(k.b, barrier, vec![k80, k9]);
        // 2) LDS load resets adjacency -> next barrier(9) MUST stay!
        let p0 = k.m.element(k.b, lds, k0);
        let lv = k.m.load(k.b, p0);
        k.m.call(k.b, barrier, vec![k80, k9]);
        // 3) barrier(9) then barrier(11): 9 does NOT cover bit 2 -> keep barrier(11)!
        k.m.call(k.b, barrier, vec![k80, k11]);
        // 4) barrier(11) again immediately -> dropped!
        k.m.call(k.b, barrier, vec![k80, k11]);
        // 5) Negative control: fence-only (8) then sync-only (1) does NOT cover (9)!
        k.m.store(k.b, p0, pure_sum);
        k.m.call(k.b, barrier, vec![k80, k8]);
        k.m.call(k.b, barrier, vec![k80, k1]);
        k.m.call(k.b, barrier, vec![k80, k9]);
        k.m.call(k.b, k.writes, vec![lv, pure_sum]);
        let next = k.m.new_block();
        k.m.terminate(k.b, Term::Br(next));
        // 6) Barrier in a different block MUST stay even with no ops between!
        k.m.call(next, barrier, vec![k80, k9]);
        k.m.terminate(next, Term::Ret);

        k.m.optimise(BARRIERS);
        let barrier_modes = |b: usize| -> Vec<u64> {
            k.m.blocks[b]
                .body
                .iter()
                .filter_map(|&id| match &k.m.insts[id as usize].op {
                    Op::Call(f, args) if k.m.funcs[*f as usize].name == "dx.op.barrier" => {
                        k.m.int_k(args[1]).map(|(_, m)| m)
                    }
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            barrier_modes(k.b),
            vec![11, 9, 11, 8, 1, 9],
            "only strictly covered adjacent barriers should drop"
        );
        assert_eq!(barrier_modes(next), vec![9], "cross-block barrier must stay");
        k.m.write();
    }

    #[test]
    fn read_after_read_barrier_coalescing_and_soundness_negatives() {
        let trace_lds_ops = |m: &Module, b: usize| -> String {
            m.blocks[b]
                .body
                .iter()
                .filter_map(|&id| match &m.insts[id as usize].op {
                    Op::Call(f, _) if m.funcs[*f as usize].name == "dx.op.barrier" => Some('B'),
                    Op::Load(_) => Some('R'),
                    Op::Store(..) => Some('W'),
                    Op::AtomicRmw(..) | Op::CmpXchg(..) => Some('A'),
                    _ => None,
                })
                .collect()
        };

        // 1) Positive: `write -> read -> read -> overwrite -> read` cuts 5 -> 4 barriers
        //    under both `BARRIERS` and `ALL` (`BWBRBRBWBR` -> `BWBRRBWBR`).
        for passes in [BARRIERS, ALL] {
            let mut k = kit();
            let (i32t, void) = (k.m.int(32), k.m.void());
            let bty = k.m.func_ty(void, vec![i32t, i32t]);
            k.m.ptr(bty);
            let barrier = k.m.function("dx.op.barrier", bty, Some(Attr::NoDuplicate), false);
            let arr = k.m.array(8, i32t);
            let lds = k.m.global("lds", arr, 3);
            let (k0, k1, k9, k80) = (k.m.i32(0), k.m.i32(1), k.m.i32(9), k.m.i32(80));
            let v0 = k.m.call(k.b, k.reads, vec![k0, k1]);
            let p0 = k.m.element(k.b, lds, k0);
            let p1 = k.m.element(k.b, lds, k1);

            k.m.call(k.b, barrier, vec![k80, k9]);
            k.m.store(k.b, p0, v0);
            k.m.call(k.b, barrier, vec![k80, k9]);
            let r1 = k.m.load(k.b, p1);
            k.m.call(k.b, barrier, vec![k80, k9]);
            let r2 = k.m.load(k.b, p0);
            let next_v = k.m.bin(k.b, Bin::Add, r1, k1);
            k.m.call(k.b, barrier, vec![k80, k9]);
            k.m.store(k.b, p0, next_v);
            k.m.call(k.b, barrier, vec![k80, k9]);
            let r3 = k.m.load(k.b, p1);
            k.m.call(k.b, k.writes, vec![r1, r2]);
            k.m.call(k.b, k.writes, vec![r3, k0]);
            k.m.terminate(k.b, Term::Ret);

            assert_eq!(trace_lds_ops(&k.m, k.b), "BWBRBRBWBR");
            k.m.optimise(passes);
            assert_eq!(
                trace_lds_ops(&k.m, k.b),
                "BWBRRBWBR",
                "middle read-after-read barrier must drop while write barriers stay (passes={passes})"
            );
            k.m.write();
        }

        // 2) Positive: 3 consecutive reads separated by pure ALU, `ReadNone` call,
        //    and non-escaping `PrivateGlobal` load coalesce 5 -> 3 barriers (`BWBRRRBW`).
        {
            let mut k = kit();
            let (i32t, void) = (k.m.int(32), k.m.void());
            let bty = k.m.func_ty(void, vec![i32t, i32t]);
            k.m.ptr(bty);
            let barrier = k.m.function("dx.op.barrier", bty, Some(Attr::NoDuplicate), false);
            let arr = k.m.array(8, i32t);
            let lds = k.m.global("lds", arr, 3);
            let priv_g = k.m.global("priv_g", arr, 0);
            let (k0, k1, k2, k9, k80) = (
                k.m.i32(0),
                k.m.i32(1),
                k.m.i32(2),
                k.m.i32(9),
                k.m.i32(80),
            );
            let v0 = k.m.call(k.b, k.reads, vec![k0, k1]);
            let p0 = k.m.element(k.b, lds, k0);
            let p1 = k.m.element(k.b, lds, k1);
            let p2 = k.m.element(k.b, lds, k2);
            let priv0 = k.m.element(k.b, priv_g, k0);
            k.m.store(k.b, priv0, v0);

            k.m.call(k.b, barrier, vec![k80, k9]);
            k.m.store(k.b, p0, v0);
            k.m.call(k.b, barrier, vec![k80, k9]);
            let r1 = k.m.load(k.b, p0);
            let alu = k.m.bin(k.b, Bin::Xor, r1, k2);
            let pure_c = k.m.call(k.b, k.pure, vec![alu, k1]);
            let priv_ld = k.m.load(k.b, priv0);
            k.m.call(k.b, barrier, vec![k80, k9]);
            let r2 = k.m.load(k.b, p1);
            let alu2 = k.m.bin(k.b, Bin::Add, pure_c, priv_ld);
            k.m.call(k.b, barrier, vec![k80, k9]);
            let r3 = k.m.load(k.b, p2);
            let sum = k.m.bin(k.b, Bin::Add, r2, r3);
            let sum = k.m.bin(k.b, Bin::Add, sum, alu2);
            k.m.call(k.b, barrier, vec![k80, k9]);
            k.m.store(k.b, p0, sum);
            k.m.terminate(k.b, Term::Ret);

            k.m.optimise(BARRIERS);
            // Includes initial priv0 store ('W') and priv_ld ('R') between r1 and second barrier.
            assert_eq!(trace_lds_ops(&k.m, k.b), "WBWBRRRRBW");
            k.m.write();
        }

        // 3) Positive: end-of-shader `write -> read -> read -> bufferStore -> Ret`
        //    (no trailing LDS overwrite) drops the middle barrier (`3 -> 2`, `BWBRR`).
        {
            let mut k = kit();
            let (i32t, void) = (k.m.int(32), k.m.void());
            let bty = k.m.func_ty(void, vec![i32t, i32t]);
            k.m.ptr(bty);
            let barrier = k.m.function("dx.op.barrier", bty, Some(Attr::NoDuplicate), false);
            let arr = k.m.array(4, i32t);
            let lds = k.m.global("lds", arr, 3);
            let (k0, k1, k9, k80) = (k.m.i32(0), k.m.i32(1), k.m.i32(9), k.m.i32(80));
            let v0 = k.m.call(k.b, k.reads, vec![k0, k1]);
            let p0 = k.m.element(k.b, lds, k0);
            let p1 = k.m.element(k.b, lds, k1);

            k.m.call(k.b, barrier, vec![k80, k9]);
            k.m.store(k.b, p0, v0);
            k.m.call(k.b, barrier, vec![k80, k9]);
            let r1 = k.m.load(k.b, p0);
            k.m.call(k.b, barrier, vec![k80, k9]);
            let r2 = k.m.load(k.b, p1);
            k.m.call(k.b, k.writes, vec![r1, r2]);
            k.m.terminate(k.b, Term::Ret);

            assert_eq!(trace_lds_ops(&k.m, k.b), "BWBRBR");
            k.m.optimise(BARRIERS);
            assert_eq!(trace_lds_ops(&k.m, k.b), "BWBRR");
            k.m.write();
        }

        // 4) Negative: `write -> read -> write -> read` MUST keep all 4 barriers (`BWBRBWBR`).
        {
            let mut k = kit();
            let (i32t, void) = (k.m.int(32), k.m.void());
            let bty = k.m.func_ty(void, vec![i32t, i32t]);
            k.m.ptr(bty);
            let barrier = k.m.function("dx.op.barrier", bty, Some(Attr::NoDuplicate), false);
            let arr = k.m.array(4, i32t);
            let lds = k.m.global("lds", arr, 3);
            let (k0, k1, k9, k80) = (k.m.i32(0), k.m.i32(1), k.m.i32(9), k.m.i32(80));
            let v0 = k.m.call(k.b, k.reads, vec![k0, k1]);
            let p0 = k.m.element(k.b, lds, k0);
            let p1 = k.m.element(k.b, lds, k1);

            k.m.call(k.b, barrier, vec![k80, k9]);
            k.m.store(k.b, p0, v0);
            k.m.call(k.b, barrier, vec![k80, k9]);
            let r1 = k.m.load(k.b, p1);
            k.m.call(k.b, barrier, vec![k80, k9]);
            k.m.store(k.b, p0, r1);
            k.m.call(k.b, barrier, vec![k80, k9]);
            let r2 = k.m.load(k.b, p1);
            k.m.call(k.b, k.writes, vec![r1, r2]);
            k.m.terminate(k.b, Term::Ret);

            k.m.optimise(ALL);
            assert_eq!(trace_lds_ops(&k.m, k.b), "BWBRBWBR");
            k.m.write();
        }

        // 5) Negative: intervening LDS `AtomicRmw`, `CmpXchg`, UAV `atomicBinOp`,
        //    `bufferLoad`, `bufferStore`, or `waveReadLaneFirst` between reads MUST keep barriers.
        for kind in 0..6 {
            let mut k = kit();
            let (i32t, void) = (k.m.int(32), k.m.void());
            let bty = k.m.func_ty(void, vec![i32t, i32t]);
            k.m.ptr(bty);
            let barrier = k.m.function("dx.op.barrier", bty, Some(Attr::NoDuplicate), false);
            let fty_i32 = k.m.func_ty(i32t, vec![i32t, i32t]);
            k.m.ptr(fty_i32);
            let uav_atomic =
                k.m.function("dx.op.atomicBinOp.i32", fty_i32, Some(Attr::NoUnwind), false);
            let wave_op = k.m.function(
                "dx.op.waveReadLaneFirst.i32",
                fty_i32,
                Some(Attr::NoUnwind),
                false,
            );
            let arr = k.m.array(4, i32t);
            let lds = k.m.global("lds", arr, 3);
            let (k0, k1, k2, k9, k80) = (
                k.m.i32(0),
                k.m.i32(1),
                k.m.i32(2),
                k.m.i32(9),
                k.m.i32(80),
            );
            let v0 = k.m.call(k.b, k.reads, vec![k0, k1]);
            let p0 = k.m.element(k.b, lds, k0);
            let p1 = k.m.element(k.b, lds, k1);
            let p2 = k.m.element(k.b, lds, k2);

            k.m.call(k.b, barrier, vec![k80, k9]);
            k.m.store(k.b, p0, v0);
            k.m.call(k.b, barrier, vec![k80, k9]);
            let r1 = k.m.load(k.b, p1);
            let side = match kind {
                0 => k.m.atomic(k.b, crate::emit::bitcode::Rmw::Add, p2, k1),
                1 => k.m.cmpxchg(k.b, p2, k0, k1),
                2 => k.m.call(k.b, uav_atomic, vec![r1, k1]),
                3 => k.m.call(k.b, k.reads, vec![r1, k2]),
                4 => {
                    k.m.call(k.b, k.writes, vec![r1, k2]);
                    r1
                }
                _ => k.m.call(k.b, wave_op, vec![r1, k0]),
            };
            k.m.call(k.b, barrier, vec![k80, k9]);
            let r2 = k.m.load(k.b, p0);
            k.m.call(k.b, barrier, vec![k80, k9]);
            k.m.store(k.b, p0, side);
            k.m.call(k.b, k.writes, vec![r1, r2]);
            k.m.terminate(k.b, Term::Ret);

            let count_barriers = |m: &Module| -> usize {
                m.blocks[k.b]
                    .body
                    .iter()
                    .filter(|&&id| {
                        matches!(
                            &m.insts[id as usize].op,
                            Op::Call(f, _) if m.funcs[*f as usize].name == "dx.op.barrier"
                        )
                    })
                    .count()
            };
            assert_eq!(count_barriers(&k.m), 4);
            k.m.optimise(ALL);
            assert_eq!(
                count_barriers(&k.m),
                4,
                "intervening side-effect/lane op kind={kind} must preserve all 4 barriers"
            );
            k.m.write();
        }

        // 6) Negative: WAR hazard across a branch (`W1 -> B1(9) -> R1(lds[0]) -> B2(9) -> R2(lds[1]) -> CondBr`
        //    into `then` block storing `lds[0]` WITHOUT a barrier) MUST keep `B2(9)`!
        //    Also verifies cross-block reads (`Br`) and fence-only `barrier(8)` keep barriers.
        {
            let mut k = kit();
            let (i32t, void) = (k.m.int(32), k.m.void());
            let bty = k.m.func_ty(void, vec![i32t, i32t]);
            k.m.ptr(bty);
            let barrier = k.m.function("dx.op.barrier", bty, Some(Attr::NoDuplicate), false);
            let arr = k.m.array(4, i32t);
            let lds = k.m.global("lds", arr, 3);
            let (k0, k1, k8, k9, k80) = (
                k.m.i32(0),
                k.m.i32(1),
                k.m.i32(8),
                k.m.i32(9),
                k.m.i32(80),
            );
            let v0 = k.m.call(k.b, k.reads, vec![k0, k1]);
            let cond = k.m.cmp(k.b, pred::EQ, v0, k1);
            let p0 = k.m.element(k.b, lds, k0);
            let p1 = k.m.element(k.b, lds, k1);

            // Fence-only mode 8 without thread-group sync bit 1: must keep all three `barrier(8)`.
            k.m.call(k.b, barrier, vec![k80, k8]);
            let f1 = k.m.load(k.b, p0);
            k.m.call(k.b, barrier, vec![k80, k8]);
            let f2 = k.m.load(k.b, p1);
            k.m.call(k.b, barrier, vec![k80, k8]);

            // WAR across branch: B2 protects R1(p0) from the un-barriered store to p0 in `then_blk`!
            k.m.store(k.b, p0, v0);
            k.m.call(k.b, barrier, vec![k80, k9]);
            let r1 = k.m.load(k.b, p0);
            k.m.call(k.b, barrier, vec![k80, k9]);
            let r2 = k.m.load(k.b, p1);
            let (then_blk, exit_blk) = (k.m.new_block(), k.m.new_block());
            k.m.terminate(k.b, Term::CondBr(cond, then_blk, exit_blk));

            let sum = k.m.bin(then_blk, Bin::Add, r1, r2);
            k.m.store(then_blk, p0, sum);
            k.m.terminate(then_blk, Term::Br(exit_blk));

            let out_f = k.m.bin(exit_blk, Bin::Add, f1, f2);
            k.m.call(exit_blk, k.writes, vec![out_f, r2]);
            k.m.terminate(exit_blk, Term::Ret);

            k.m.optimise(ALL);
            assert_eq!(
                trace_lds_ops(&k.m, k.b),
                "BRBRBWBRBR",
                "fence-only barriers and branch-exiting WAR barrier must stay intact"
            );
            k.m.write();
        }
    }

    #[test]
    fn ptr_path_aliasing_scalar_aggregate_signed_multidim_and_deep_gep() {
        // 1a-i: Scalar global `V::Global(g)` vs `gep ty, g, 0` MUST canonicalize to the same slot.
        {
            let mut k = kit();
            let i32t = k.m.int(32);
            let ptr_i32 = k.m.ptr(i32t);
            let g = k.m.global("scalar_g", i32t, 0);
            let (k0, k1, k2) = (k.m.i32(0), k.m.i32(1), k.m.i32(2));
            let v1 = k.m.call(k.b, k.reads, vec![k0, k0]);
            let v2 = k.m.call(k.b, k.reads, vec![k1, k1]);
            let g_gep0 = k.m.push(k.b, ptr_i32, Op::Gep(i32t, g, vec![k0]));
            k.m.store(k.b, g, v1);
            let read_via_gep = k.m.load(k.b, g_gep0); // must see v1!
            k.m.store(k.b, g_gep0, v2);
            let read_via_global = k.m.load(k.b, g); // must see v2, NOT stale v1!
            k.m.call(k.b, k.writes, vec![read_via_gep, read_via_global]);
            k.m.terminate(k.b, Term::Ret);
            k.m.optimise(LOADS | DEAD);
            assert_eq!(
                stores(&k.m, k.b),
                [[v1, v2]],
                "scalar V::Global(g) and gep(i32, g, 0) must share canonical slot"
            );
            // Also verify `Store(g, v1)` is NOT unsafely deleted when `Load(gep(g,0))`
            // cannot be forwarded (e.g., after an unknown read or with SharedGlobal).
            let mut k2m = kit();
            let i32t = k2m.m.int(32);
            let ptr_lds = k2m.m.ptr_in(i32t, 3);
            let sg = k2m.m.global("scalar_lds", i32t, 3);
            let (z0, z1) = (k2m.m.i32(0), k2m.m.i32(1));
            let a = k2m.m.call(k2m.b, k2m.reads, vec![z0, z0]);
            let b = k2m.m.call(k2m.b, k2m.reads, vec![z1, z1]);
            let sg_gep0 = k2m.m.push(k2m.b, ptr_lds, Op::Gep(i32t, sg, vec![z0]));
            k2m.m.store(k2m.b, sg, a);
            k2m.m.store(k2m.b, sg_gep0, b);
            let out = k2m.m.load(k2m.b, sg);
            let out_reuse = k2m.m.load(k2m.b, sg_gep0);
            k2m.m.call(k2m.b, k2m.writes, vec![out, out_reuse]);
            k2m.m.terminate(k2m.b, Term::Ret);
            k2m.m.optimise(LOADS | DEAD);
            assert_eq!(
                stores(&k2m.m, k2m.b),
                [[out, out]],
                "LDS scalar store->load must not forward; second load reuses canonical alias"
            );
            let rem_stores = k2m.m.blocks[k2m.b]
                .body
                .iter()
                .filter(|&&id| matches!(k2m.m.insts[id as usize].op, Op::Store(..)))
                .count();
            assert_eq!(rem_stores, 1, "overwritten store via equivalent scalar GEP drops first");
            let _ = k2;
        }

        // 1a-iii: Wave-ordered LDS exchange (`lds[bucket] = lane; if (lds[bucket] == lane)`)
        // must never forward space-3 stores into subsequent loads (neither const nor dynamic),
        // while consecutive loads still reuse the first load.
        {
            let mut k = kit();
            let i32t = k.m.int(32);
            let arr = k.m.array(64, i32t);
            let lds = k.m.global("lds_wave_exch", arr, 3);
            let (k0, k1) = (k.m.i32(0), k.m.i32(1));
            let bucket = k.m.call(k.b, k.reads, vec![k0, k0]);
            let lane = k.m.call(k.b, k.reads, vec![k1, k1]);
            let p_dyn = k.m.element(k.b, lds, bucket);
            let p_c0 = k.m.element(k.b, lds, k0);
            k.m.store(k.b, p_dyn, lane);
            let rd_dyn_1 = k.m.load(k.b, p_dyn);
            let rd_dyn_2 = k.m.load(k.b, p_dyn);
            k.m.store(k.b, p_c0, lane);
            let rd_c0_1 = k.m.load(k.b, p_c0);
            let rd_c0_2 = k.m.load(k.b, p_c0);
            k.m.call(k.b, k.writes, vec![rd_dyn_1, rd_dyn_2]);
            k.m.call(k.b, k.writes, vec![rd_c0_1, rd_c0_2]);
            k.m.terminate(k.b, Term::Ret);
            k.m.optimise(LOADS | DEAD);
            assert_eq!(
                stores(&k.m, k.b),
                [[rd_dyn_1, rd_dyn_1], [rd_c0_1, rd_c0_1]],
                "space-3 store->load must never forward (const or dynamic), while load->load reuses"
            );
        }

        // 1a-ii: Whole-aggregate store/load vs field access MUST invalidate each other.
        {
            let mut k = kit();
            let i32t = k.m.int(32);
            let st_ty = k.m.named_struct("pair", vec![i32t, i32t]);
            let ptr_i32 = k.m.ptr(i32t);
            let g = k.m.global("agg_g", st_ty, 0);
            let (k0, k1) = (k.m.i32(0), k.m.i32(1));
            let v1 = k.m.call(k.b, k.reads, vec![k0, k0]);
            let v2 = k.m.call(k.b, k.reads, vec![k1, k1]);
            let agg_k = k.m.aggregate(st_ty, &[k1, k1]);
            let f0 = k.m.push(k.b, ptr_i32, Op::Gep(st_ty, g, vec![k0, k0]));
            // Store field -> whole-aggregate load -> overwrite field: first field store MUST stay!
            k.m.store(k.b, f0, v1);
            let whole = k.m.load(k.b, g);
            let whole_f0 = k.m.extract(k.b, whole, 0);
            k.m.store(k.b, f0, v2);
            // Whole-aggregate store invalidates cached field value -> next field load MUST reload!
            k.m.store(k.b, g, agg_k);
            let f0_after_agg = k.m.load(k.b, f0);
            k.m.call(k.b, k.writes, vec![whole_f0, f0_after_agg]);
            k.m.terminate(k.b, Term::Ret);
            k.m.optimise(LOADS | DEAD);
            assert_eq!(
                stores(&k.m, k.b),
                [[whole_f0, f0_after_agg]],
                "whole-aggregate store must invalidate field load (no stale v2)"
            );
            let f0_stores = k.m.blocks[k.b]
                .body
                .iter()
                .filter(|&&id| matches!(k.m.insts[id as usize].op, Op::Store(p, _) if p == f0))
                .count();
            assert_eq!(f0_stores, 2, "whole-aggregate load must preserve preceding field store");
        }

        // 1b: Signed negative GEP index (`gep i32, (gep [4 x i32], g, 0, 1), -1` == `[0]`,
        //     and OOB `gep i32, (gep [4 x i32], g, 0, 0), -1` falls back to Dynamic).
        {
            let mut k = kit();
            let i32t = k.m.int(32);
            let ptr_i32 = k.m.ptr(i32t);
            let arr = k.m.array(4, i32t);
            let g = k.m.global("signed_g", arr, 0);
            let (k0, k1, km1) = (k.m.i32(0), k.m.i32(1), k.m.i32(u32::MAX));
            let v1 = k.m.call(k.b, k.reads, vec![k0, k0]);
            let v2 = k.m.call(k.b, k.reads, vec![k1, k1]);
            let p0 = k.m.element(k.b, g, k0);
            let p1 = k.m.element(k.b, g, k1);
            let p1_m1 = k.m.push(k.b, ptr_i32, Op::Gep(i32t, p1, vec![km1]));
            let p0_m1 = k.m.push(k.b, ptr_i32, Op::Gep(i32t, p0, vec![km1]));

            // In-bounds negative step `1 + (-1) == 0` aliases `p0`: must forward `v2`!
            k.m.store(k.b, p0, v1);
            k.m.store(k.b, p1_m1, v2);
            let after_inbounds = k.m.load(k.b, p0);

            // OOB negative step `0 + (-1) == -1` falls back to Dynamic:
            // Load must prevent DSE of `p0`, and Store must invalidate `p0`!
            k.m.store(k.b, p0, v1);
            let oob_rd = k.m.load(k.b, p0_m1);
            k.m.store(k.b, p0, v2);
            k.m.store(k.b, p0_m1, v1);
            let after_oob_st = k.m.load(k.b, p0);

            k.m.call(k.b, k.writes, vec![after_inbounds, oob_rd]);
            k.m.call(k.b, k.writes, vec![after_oob_st, k0]);
            k.m.terminate(k.b, Term::Ret);
            k.m.optimise(LOADS | DEAD);
            let st = stores(&k.m, k.b);
            assert_eq!(st[0][0], v2, "gep(p1, -1) must alias p0 and forward v2");
            assert_eq!(st[1][0], after_oob_st, "OOB negative GEP store must invalidate p0");
            let p0_stores = k.m.blocks[k.b]
                .body
                .iter()
                .filter(|&&id| matches!(k.m.insts[id as usize].op, Op::Store(p, _) if p == p0))
                .count();
            assert_eq!(p0_stores, 2, "OOB negative GEP load must preserve earlier p0 store");
        }

        // 1c: Multi-dimensional array flattening `gep i32, (gep [2 x [2 x i32]], g, 0, 0, 1), 1`
        //     aliases `gep g, 0, 1, 0` -> must fall back to Dynamic (no stale forward, no DSE).
        {
            let mut k = kit();
            let i32t = k.m.int(32);
            let ptr_i32 = k.m.ptr(i32t);
            let inner = k.m.array(2, i32t);
            let outer = k.m.array(2, inner);
            let g = k.m.global("md_g", outer, 0);
            let (k0, k1) = (k.m.i32(0), k.m.i32(1));
            let v1 = k.m.call(k.b, k.reads, vec![k0, k0]);
            let v2 = k.m.call(k.b, k.reads, vec![k1, k1]);
            let p10 = k.m.push(k.b, ptr_i32, Op::Gep(outer, g, vec![k0, k1, k0]));
            let p01 = k.m.push(k.b, ptr_i32, Op::Gep(outer, g, vec![k0, k0, k1]));
            let p01_flat = k.m.push(k.b, ptr_i32, Op::Gep(i32t, p01, vec![k1]));

            k.m.store(k.b, p10, v1);
            let rd_flat = k.m.load(k.b, p01_flat); // reads [1, 0] via flattened GEP!
            k.m.store(k.b, p10, v2);
            k.m.store(k.b, p01_flat, v1); // overwrites [1, 0] via flattened GEP!
            let rd_p10 = k.m.load(k.b, p10); // MUST NOT forward stale v2!

            k.m.call(k.b, k.writes, vec![rd_flat, rd_p10]);
            k.m.terminate(k.b, Term::Ret);
            k.m.optimise(LOADS | DEAD);
            assert_eq!(
                stores(&k.m, k.b),
                [[rd_flat, rd_p10]],
                "flattened multi-dim GEP store must invalidate [1, 0]"
            );
            let p10_stores = k.m.blocks[k.b]
                .body
                .iter()
                .filter(|&&id| matches!(k.m.insts[id as usize].op, Op::Store(p, _) if p == p10))
                .count();
            assert_eq!(
                p10_stores, 2,
                "flattened multi-dim GEP load must prevent DSE of [1, 0] store"
            );
        }

        // 1d: Deep GEP chains (>32 hops) MUST agree with `escaped_globals` and invalidate `g`.
        {
            let mut k = kit();
            let i32t = k.m.int(32);
            let ptr_i32 = k.m.ptr(i32t);
            let arr = k.m.array(4, i32t);
            let g = k.m.global("deep_g", arr, 0);
            let (k0, k1, k2) = (k.m.i32(0), k.m.i32(1), k.m.i32(2));
            let v1 = k.m.call(k.b, k.reads, vec![k0, k0]);
            let v2 = k.m.call(k.b, k.reads, vec![k1, k1]);
            let dyn_idx = k.m.call(k.b, k.reads, vec![k2, k2]);
            let p0 = k.m.element(k.b, g, k0);
            let mut p_deep_const = p0;
            let mut p_deep_dyn = k.m.element(k.b, g, dyn_idx);
            for _ in 0..35 {
                p_deep_const = k.m.push(k.b, ptr_i32, Op::Gep(i32t, p_deep_const, vec![k0]));
                p_deep_dyn = k.m.push(k.b, ptr_i32, Op::Gep(i32t, p_deep_dyn, vec![k0]));
            }

            // 36-hop const chain aliases p0 -> forwards v2 (not stale v1).
            k.m.store(k.b, p0, v1);
            k.m.store(k.b, p_deep_const, v2);
            let after_deep_const = k.m.load(k.b, p0);

            // 36-hop dynamic chain must invalidate p0 on Store, Load (prevent DSE), and AtomicRmw.
            k.m.store(k.b, p0, v1);
            let deep_rd = k.m.load(k.b, p_deep_dyn);
            k.m.store(k.b, p0, v2);
            k.m.store(k.b, p_deep_dyn, v1);
            let after_deep_st = k.m.load(k.b, p0);
            k.m.store(k.b, p0, v2);
            let deep_rmw = k.m.atomic(k.b, crate::emit::bitcode::Rmw::Add, p_deep_dyn, k1);
            let after_deep_rmw = k.m.load(k.b, p0);

            k.m.call(k.b, k.writes, vec![after_deep_const, deep_rd]);
            k.m.call(k.b, k.writes, vec![after_deep_st, deep_rmw]);
            k.m.call(k.b, k.writes, vec![after_deep_rmw, k0]);
            k.m.terminate(k.b, Term::Ret);
            k.m.optimise(LOADS | DEAD);
            let st = stores(&k.m, k.b);
            assert_eq!(st[0][0], v2, "36-hop constant GEP must alias p0");
            assert_eq!(st[1][0], after_deep_st, "36-hop dynamic GEP store must invalidate p0");
            assert_eq!(st[2][0], after_deep_rmw, "36-hop dynamic GEP atomic must invalidate p0");
            let p0_stores = k.m.blocks[k.b]
                .body
                .iter()
                .filter(|&&id| matches!(k.m.insts[id as usize].op, Op::Store(p, _) if p == p0))
                .count();
            assert_eq!(
                p0_stores, 3,
                "36-hop dynamic load/store/atomic must preserve all 3 live p0 stores"
            );
        }
    }

    #[test]
    fn licm_negative_controls_speculative_resources_uniformity_discard_and_signed_const_gep() {
        let inst_id = |v: V| match v {
            V::Inst(i) => i,
            _ => panic!("expected V::Inst, got {v:?}"),
        };

        // 2a: Conditional `if (valid)` inside loop MUST NOT speculatively hoist
        //     createHandleFromHeap (even with nonUniform=true or const idx),
        //     annotateHandle, cbufferLoadLegacy, or getDimensions, while pure
        //     math (`unary.f32`) in the same conditional block still hoists.
        {
            let mut k = kit();
            let (i32t, i1t, f32t) = (k.m.int(32), k.m.int(1), k.m.float());
            let hty = k.m.func_ty(i32t, vec![i32t, i32t, i1t, i1t]);
            let aty = k.m.func_ty(i32t, vec![i32t, i32t, i32t]);
            let fty = k.m.func_ty(i32t, vec![i32t, i32t]);
            let uty = k.m.func_ty(f32t, vec![i32t, f32t]);
            let heap =
                k.m.function("dx.op.createHandleFromHeap", hty, Some(Attr::ReadNone), false);
            let ann = k.m.function("dx.op.annotateHandle", aty, Some(Attr::ReadNone), false);
            let cb = k.m.function("dx.op.cbufferLoadLegacy.i32", fty, Some(Attr::ReadOnly), false);
            let dims = k.m.function("dx.op.getDimensions", fty, Some(Attr::ReadOnly), false);
            let unary = k.m.function("dx.op.unary.f32", uty, Some(Attr::ReadNone), false);
            let (k0, k1, k4, k13, k59, k72, k216, k218) = (
                k.m.i32(0),
                k.m.i32(1),
                k.m.i32(4),
                k.m.i32(13),
                k.m.i32(59),
                k.m.i32(72),
                k.m.i32(216),
                k.m.i32(218),
            );
            let (yes, no) = (k.m.bool(true), k.m.bool(false));
            let slot = k.m.call(k.b, k.reads, vec![k0, k0]);
            let cond_val = k.m.call(k.b, k.reads, vec![k1, k0]);
            let xf = k.m.cast(k.b, Cast::Bitcast, slot, f32t);

            let (head, guarded, latch, exit) = (
                k.m.new_block(),
                k.m.new_block(),
                k.m.new_block(),
                k.m.new_block(),
            );
            k.m.terminate(k.b, Term::Br(head));
            let idx = k.m.phi(head, i32t);
            let has_valid = k.m.cmp(head, pred::NE, cond_val, k0);
            k.m.terminate(head, Term::CondBr(has_valid, guarded, latch));

            let h_dyn_nu = k.m.call(guarded, heap, vec![k218, slot, no, yes]);
            let h_const = k.m.call(guarded, heap, vec![k218, k0, no, no]);
            let h_ann = k.m.call(guarded, ann, vec![k216, h_dyn_nu, k0]);
            let cb_v = k.m.call(guarded, cb, vec![k59, h_ann]);
            let dim_v = k.m.call(guarded, dims, vec![k72, h_const]);
            let pure_sin = k.m.call(guarded, unary, vec![k13, xf]);
            let sin_i = k.m.cast(guarded, Cast::Bitcast, pure_sin, i32t);
            let sum = k.m.bin(guarded, Bin::Add, cb_v, dim_v);
            let sum2 = k.m.bin(guarded, Bin::Add, sum, sin_i);
            k.m.call(guarded, k.writes, vec![idx, sum2]);
            k.m.terminate(guarded, Term::Br(latch));

            let next = k.m.bin(latch, Bin::Add, idx, k1);
            let more = k.m.cmp(latch, pred::IULT, next, k4);
            k.m.terminate(latch, Term::CondBr(more, head, exit));
            k.m.set_phi(idx, vec![(k0, k.b), (next, latch)]);
            k.m.terminate(exit, Term::Ret);

            k.m.optimise(LICM);
            for v in [h_dyn_nu, h_const, h_ann, cb_v, dim_v] {
                assert!(
                    k.m.blocks[guarded].body.contains(&inst_id(v)),
                    "guarded resource/query call {v:?} MUST NOT hoist speculatively"
                );
                assert!(!k.m.blocks[k.b].body.contains(&inst_id(v)));
            }
            assert!(
                k.m.blocks[k.b].body.contains(&inst_id(pure_sin)),
                "pure math unary.f32 in guarded block is safe to hoist"
            );
            k.m.write();
        }

        // 2b-i: `header: condbr %c, body, header; body: ... condbr %done, exit, header`
        //       (`body` dominates all `exits` yet is NOT guaranteed on iter 0):
        //       MUST NOT hoist uniform `createHandleFromHeap`, `cbufferLoadLegacy`, or
        //       dynamic `Op::Load(ConstGlobal)` from `body`.
        {
            let mut k = kit();
            let (i32t, i1t) = (k.m.int(32), k.m.int(1));
            let arr4 = k.m.array(4, i32t);
            let c_init = k.m.undef(arr4);
            let cg = k.m.constant_global("tbl", c_init);
            let hty = k.m.func_ty(i32t, vec![i32t, i32t, i1t, i1t]);
            let fty = k.m.func_ty(i32t, vec![i32t, i32t]);
            let heap =
                k.m.function("dx.op.createHandleFromHeap", hty, Some(Attr::ReadNone), false);
            let cb = k.m.function("dx.op.cbufferLoadLegacy.i32", fty, Some(Attr::ReadOnly), false);
            let (k0, k1, k4, k59, k218) = (
                k.m.i32(0),
                k.m.i32(1),
                k.m.i32(4),
                k.m.i32(59),
                k.m.i32(218),
            );
            let no = k.m.bool(false);
            let dyn_idx = k.m.call(k.b, k.reads, vec![k0, k0]);
            let ptr_dyn = k.m.element(k.b, cg, dyn_idx);

            let (head, body, exit) = (k.m.new_block(), k.m.new_block(), k.m.new_block());
            k.m.terminate(k.b, Term::Br(head));
            let iter = k.m.phi(head, i32t);
            let next_h = k.m.bin(head, Bin::Add, iter, k1);
            let ready = k.m.cmp(head, pred::EQ, iter, dyn_idx);
            k.m.terminate(head, Term::CondBr(ready, body, head));

            let h_unif = k.m.call(body, heap, vec![k218, dyn_idx, no, no]);
            let cb_v = k.m.call(body, cb, vec![k59, h_unif]);
            let dyn_cg_load = k.m.load(body, ptr_dyn);
            let sum = k.m.bin(body, Bin::Add, cb_v, dyn_cg_load);
            k.m.call(body, k.writes, vec![iter, sum]);
            let done = k.m.cmp(body, pred::IUGE, next_h, k4);
            k.m.terminate(body, Term::CondBr(done, exit, head));
            k.m.set_phi(iter, vec![(k0, k.b), (next_h, head), (next_h, body)]);
            k.m.terminate(exit, Term::Ret);

            k.m.optimise(LICM);
            for v in [h_unif, cb_v, dyn_cg_load] {
                assert!(
                    k.m.blocks[body].body.contains(&inst_id(v)),
                    "{v:?} in continue-skipped exit-dominating body MUST NOT hoist"
                );
                assert!(!k.m.blocks[k.b].body.contains(&inst_id(v)));
            }
            k.m.write();
        }

        // 2b-ii: `dx.op.discard` inside `header` stops `guaranteed` for subsequent ops
        //        in `header` and any `Term::Br` successor block, while ops BEFORE `discard`
        //        and clean `header -> Br(b1)` chains without `discard` DO hoist.
        {
            let mut k = kit();
            let (i32t, i1t, void) = (k.m.int(32), k.m.int(1), k.m.void());
            let dty = k.m.func_ty(void, vec![i32t, i1t]);
            let fty = k.m.func_ty(i32t, vec![i32t, i32t]);
            k.m.ptr(dty);
            let discard = k.m.function("dx.op.discard", dty, Some(Attr::NoUnwind), false);
            let cb = k.m.function("dx.op.cbufferLoadLegacy.i32", fty, Some(Attr::ReadOnly), false);
            let arr4 = k.m.array(4, i32t);
            let c_init = k.m.undef(arr4);
            let cg = k.m.constant_global("tbl_d", c_init);
            let (k0, k1, k4, k59, k82) = (
                k.m.i32(0),
                k.m.i32(1),
                k.m.i32(4),
                k.m.i32(59),
                k.m.i32(82),
            );
            let dyn_idx = k.m.call(k.b, k.reads, vec![k0, k0]);
            let bad = k.m.cmp(k.b, pred::IUGE, dyn_idx, k4);
            let ptr_dyn = k.m.element(k.b, cg, dyn_idx);

            let (head, b1, exit) = (k.m.new_block(), k.m.new_block(), k.m.new_block());
            k.m.terminate(k.b, Term::Br(head));
            let iter = k.m.phi(head, i32t);
            let cb_before = k.m.call(head, cb, vec![k59, k0]);
            k.m.call(head, discard, vec![k82, bad]);
            let cb_after = k.m.call(head, cb, vec![k59, k1]);
            let load_after = k.m.load(head, ptr_dyn);
            k.m.terminate(head, Term::Br(b1));

            let cb_in_b1 = k.m.call(b1, cb, vec![k59, k4]);
            let s0 = k.m.bin(b1, Bin::Add, cb_before, cb_after);
            let s1 = k.m.bin(b1, Bin::Add, s0, load_after);
            let s2 = k.m.bin(b1, Bin::Add, s1, cb_in_b1);
            k.m.call(b1, k.writes, vec![iter, s2]);
            let next = k.m.bin(b1, Bin::Add, iter, k1);
            let more = k.m.cmp(b1, pred::IULT, next, k4);
            k.m.terminate(b1, Term::CondBr(more, head, exit));
            k.m.set_phi(iter, vec![(k0, k.b), (next, b1)]);
            k.m.terminate(exit, Term::Ret);

            k.m.optimise(LICM);
            assert!(
                k.m.blocks[k.b].body.contains(&inst_id(cb_before)),
                "cbufferLoadLegacy BEFORE discard in header must hoist"
            );
            assert!(
                k.m.blocks[head].body.contains(&inst_id(cb_after)),
                "cbufferLoadLegacy AFTER discard in header must NOT hoist"
            );
            assert!(
                k.m.blocks[head].body.contains(&inst_id(load_after)),
                "dynamic ConstGlobal load AFTER discard in header must NOT hoist"
            );
            assert!(
                k.m.blocks[b1].body.contains(&inst_id(cb_in_b1)),
                "cbufferLoadLegacy in block after discard must NOT hoist"
            );
            k.m.write();
        }

        // Positive control for 2b: clean `header -> Br(b1)` without `discard` DOES hoist from `b1`.
        {
            let mut k = kit();
            let i32t = k.m.int(32);
            let fty = k.m.func_ty(i32t, vec![i32t, i32t]);
            let cb = k.m.function("dx.op.cbufferLoadLegacy.i32", fty, Some(Attr::ReadOnly), false);
            let (k0, k1, k4, k59) = (k.m.i32(0), k.m.i32(1), k.m.i32(4), k.m.i32(59));
            let (head, b1, exit) = (k.m.new_block(), k.m.new_block(), k.m.new_block());
            k.m.terminate(k.b, Term::Br(head));
            let iter = k.m.phi(head, i32t);
            k.m.terminate(head, Term::Br(b1));
            let cb_b1 = k.m.call(b1, cb, vec![k59, k0]);
            k.m.call(b1, k.writes, vec![iter, cb_b1]);
            let next = k.m.bin(b1, Bin::Add, iter, k1);
            let more = k.m.cmp(b1, pred::IULT, next, k4);
            k.m.terminate(b1, Term::CondBr(more, head, exit));
            k.m.set_phi(iter, vec![(k0, k.b), (next, b1)]);
            k.m.terminate(exit, Term::Ret);
            k.m.optimise(LICM);
            assert!(
                k.m.blocks[k.b].body.contains(&inst_id(cb_b1)),
                "unconditional single-pred Br chain from header with no discard must hoist"
            );
            k.m.write();
        }

        // 2c: `is_in_bounds_const_global_ptr` rejects signed `i8(-1)` into `[512 x i32]`
        //     and rejects mismatched GEP `arr_ty`, while allowing provably in-bounds `i32(7)`.
        {
            let mut k = kit();
            let i32t = k.m.int(32);
            let ptr_i32 = k.m.ptr(i32t);
            let arr512 = k.m.array(512, i32t);
            let arr4 = k.m.array(4, i32t);
            let c_init = k.m.undef(arr512);
            let cg = k.m.constant_global("big_tbl", c_init);
            let (k0, k1, k4, k7, km1_i8) = (
                k.m.i32(0),
                k.m.i32(1),
                k.m.i32(4),
                k.m.i32(7),
                k.m.i8(u8::MAX),
            );
            let flag = k.m.call(k.b, k.reads, vec![k0, k0]);
            let p_neg_i8 = k.m.push(k.b, ptr_i32, Op::Gep(arr512, cg, vec![k0, km1_i8]));
            let p_mismatch = k.m.push(k.b, ptr_i32, Op::Gep(arr4, cg, vec![k0, k1]));
            let p_ok = k.m.push(k.b, ptr_i32, Op::Gep(arr512, cg, vec![k0, k7]));

            let (head, guarded, latch, exit) = (
                k.m.new_block(),
                k.m.new_block(),
                k.m.new_block(),
                k.m.new_block(),
            );
            k.m.terminate(k.b, Term::Br(head));
            let iter = k.m.phi(head, i32t);
            let cond = k.m.cmp(head, pred::NE, flag, k0);
            k.m.terminate(head, Term::CondBr(cond, guarded, latch));

            let ld_neg = k.m.load(guarded, p_neg_i8);
            let ld_mis = k.m.load(guarded, p_mismatch);
            let ld_ok = k.m.load(guarded, p_ok);
            let s0 = k.m.bin(guarded, Bin::Add, ld_neg, ld_mis);
            let s1 = k.m.bin(guarded, Bin::Add, s0, ld_ok);
            k.m.call(guarded, k.writes, vec![iter, s1]);
            k.m.terminate(guarded, Term::Br(latch));

            let next = k.m.bin(latch, Bin::Add, iter, k1);
            let more = k.m.cmp(latch, pred::IULT, next, k4);
            k.m.terminate(latch, Term::CondBr(more, head, exit));
            k.m.set_phi(iter, vec![(k0, k.b), (next, latch)]);
            k.m.terminate(exit, Term::Ret);

            k.m.optimise(LICM);
            assert!(
                k.m.blocks[guarded].body.contains(&inst_id(ld_neg)),
                "i8(-1) ConstGlobal GEP is OOB and MUST NOT hoist speculatively"
            );
            assert!(
                k.m.blocks[guarded].body.contains(&inst_id(ld_mis)),
                "mismatched arr_ty ConstGlobal GEP MUST NOT hoist speculatively"
            );
            assert!(
                k.m.blocks[k.b].body.contains(&inst_id(ld_ok)),
                "provably in-bounds ConstGlobal GEP load must hoist"
            );
            k.m.write();
        }
    }

    // ---- dxil.dll validation, hardware GPU parity & GPU timestamps ----

    #[cfg(windows)]
    mod hw_tests {
        use super::*;
        use crate::emit::host::{Dispatch, Host, Root, View};
        use crate::emit::shader::Shader;
        use crate::emit::{HostShader, lower, validator, with_shader_opt};
        use crate::native::{
            ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo,
            UsedResource,
        };
        use crate::{io, ir, resource, structure};

        fn hw_host() -> Option<Host> {
            match Host::new(false) {
                Ok(h) => Some(h),
                Err(e) => {
                    if std::env::var("VICTORY_REQUIRE_HW_GPU").is_ok_and(|v| v == "1") {
                        panic!("VICTORY_REQUIRE_HW_GPU=1 but no SM 6.6 hardware GPU: {e}");
                    }
                    eprintln!("no SM 6.6 hardware device; skipped ({e})");
                    None
                }
            }
        }

        fn validate_dxil(dxil: &[u8]) {
            if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
                let mut copy = dxil.to_vec();
                let ours = copy[4..20].to_vec();
                v.validate(&mut copy).expect("dxil.dll validation failed");
                assert_eq!(copy[4..20], ours[..], "DXIL container hash mismatch");
            }
        }

        /// Builds a realistic SM 6.6 compute shader with a conditional-entry loop
        /// containing loop-invariant CBV loads, heap handle creation, transcendental +
        /// algebraic/shift/div-by-pow2/select/zext chains, a dynamic-divisor negative
        /// control, and IEEE-754 NaN compare negation.
        fn build_loop_licm_fold_cs(
            passes: u32,
            iters: u32,
        ) -> (HostShader, usize, usize, std::time::Duration) {
            let t0 = std::time::Instant::now();
            let (dxil, inst_count, loop_insts) = with_shader_opt(passes, || {
                let mut s = Shader::new(false);
                s.threads = Some([64, 1, 1]);
                s.user_data = 4;
                let ud = s.user_data_handle();
                let (e, i32t, f32t) = (s.entry, s.t.i32, s.t.f32);
                let (k0, k1, k2, k3, k4, k5, k7, k8, k16, k31) = (
                    s.m.i32(0),
                    s.m.i32(1),
                    s.m.i32(2),
                    s.m.i32(3),
                    s.m.i32(4),
                    s.m.i32(5),
                    s.m.i32(7),
                    s.m.i32(8),
                    s.m.i32(16),
                    s.m.i32(31),
                );
                let yes = s.m.bool(true);
                let no = s.m.bool(false);
                // Dispatch thread id x.
                let tid = s.op(e, "threadId.i32", Attr::ReadNone, i32t, 93, &[k0]);
                let cb0 = s.op(e, "cbufferLoadLegacy.i32", Attr::ReadOnly, s.t.cbuf_ret, 59, &[ud, k0]);
                let seed = s.m.extract(e, cb0, 0);
                let limit = s.m.extract(e, cb0, 1);
                let dyn_div = s.m.extract(e, cb0, 2); // non-zero when loop runs; 0 when limit == 0
                let fbits = s.m.extract(e, cb0, 3);
                let enter = s.m.cmp(e, pred::IUGT, limit, k0);
                let (head, exit) = (s.m.new_block(), s.m.new_block());
                s.m.terminate(e, Term::CondBr(enter, head, exit));

                let i_phi = s.m.phi(head, i32t);
                let acc_phi = s.m.phi(head, i32t);
                // Loop-invariant CBV + pure math + algebraic/shift/compare/select chains!
                let cb_in = s.op(head, "cbufferLoadLegacy.i32", Attr::ReadOnly, s.t.cbuf_ret, 59, &[ud, k0]);
                let s_in = s.m.extract(head, cb_in, 0);
                let base = s.m.bin(head, Bin::Add, tid, s_in);
                // Heavy loop-invariant transcendental chain (big GPU speedup when hoisted!).
                let bf = s.m.cast(head, Cast::UiToFp, base, f32t);
                let s1 = s.op(head, "unary.f32", Attr::ReadNone, f32t, 13, &[bf]); // sin
                let c1 = s.op(head, "unary.f32", Attr::ReadNone, f32t, 12, &[bf]); // cos
                let s1_sq = s.m.bin(head, Bin::Mul, s1, s1);
                let c1_sq = s.m.bin(head, Bin::Mul, c1, c1);
                let sc_sum = s.m.bin(head, Bin::Add, s1_sq, c1_sq);
                let sq = s.op(head, "unary.f32", Attr::ReadNone, f32t, 24, &[sc_sum]);
                let inv_bits = s.m.cast(head, Cast::Bitcast, sq, i32t);
                // Algebraic + shift + strength-reduction + select/cmp/zext chains (loop-invariant!).
                let m8 = s.m.bin(head, Bin::Mul, base, k8);
                let sh = s.m.bin(head, Bin::Shl, m8, k2);
                let d8 = s.m.bin(head, Bin::UDiv, inv_bits, k8);
                let r16 = s.m.bin(head, Bin::URem, d8, k16);
                let a1 = s.m.bin(head, Bin::Add, r16, k3);
                let a2 = s.m.bin(head, Bin::Add, a1, k5);
                let a3 = s.m.bin(head, Bin::Sub, a2, k5);
                let x1 = s.m.bin(head, Bin::Xor, a3, k7);
                let x2 = s.m.bin(head, Bin::Xor, x1, k7);
                let c_lt = s.m.cmp(head, pred::IULT, x2, k16);
                let c_not = s.m.bin(head, Bin::Xor, c_lt, yes);
                let c_mir = s.m.cmp(head, pred::IULE, k16, x2); // matches c_not via canonical REUSE!
                let z = s.m.cast(head, Cast::ZExt, c_mir, i32t);
                let z1 = s.m.bin(head, Bin::And, z, k1);
                let z_ne = s.m.cmp(head, pred::EQ, z1, k0);
                let b_or = s.m.select(head, z_ne, yes, c_not);
                let b_and = s.m.select(head, b_or, c_lt, no);
                let pick = s.m.select(head, b_and, sh, x2);
                // Loop-invariant IEEE-754 float compare negation (tested with NaN lane!).
                let xf = s.m.cast(head, Cast::Bitcast, fbits, f32t);
                let yf = s.m.f32(1.0f32.to_bits());
                let f_lt = s.m.cmp(head, pred::OLT, xf, yf);
                let f_not = s.m.bin(head, Bin::Xor, f_lt, yes);
                let f_sel = s.m.select(head, f_not, k31, k4);
                // Dynamic-divisor negative control (safe only when loop runs since dyn_div != 0).
                let safe_in_loop = s.m.bin(head, Bin::UDiv, i_phi, dyn_div);
                // Loop-invariant heap handle creation inside loop (guaranteed executed in header).
                let h_uav = s.heap_handle(head, k0, false, false, (11 | 0x1000, 0));
                let _ = h_uav;

                let step = s.m.bin(head, Bin::Add, pick, f_sel);
                let step = s.m.bin(head, Bin::Add, step, safe_in_loop);
                let step = s.m.bin(head, Bin::Xor, step, i_phi);
                let next_acc = s.m.bin(head, Bin::Add, acc_phi, step);
                let next_i = s.m.bin(head, Bin::Add, i_phi, k1);
                let more = s.m.cmp(head, pred::IULT, next_i, limit);
                s.m.terminate(head, Term::CondBr(more, head, exit));
                s.m.set_phi(i_phi, vec![(k0, e), (next_i, head)]);
                s.m.set_phi(acc_phi, vec![(seed, e), (next_acc, head)]);

                let res = s.m.phi(exit, i32t);
                s.m.set_phi(res, vec![(seed, e), (next_acc, head)]);
                let uav = s.heap_handle(exit, k0, false, false, (11 | 0x1000, 0));
                let byte_off = s.m.bin(exit, Bin::Shl, tid, k2);
                let undef = s.m.undef(i32t);
                let mask1 = s.m.i8(1);
                s.op(
                    exit,
                    "rawBufferStore.i32",
                    Attr::NoUnwind,
                    s.t.void,
                    140,
                    &[uav, byte_off, undef, res, undef, undef, undef, mask1, k4],
                );
                s.m.terminate(exit, Term::Ret);
                // Run the selected passes before measuring census & finishing.
                let module_passes = passes & MODULE;
                if module_passes != 0 {
                    s.m.optimise(module_passes);
                }
                let insts = s.m.census().counts[0] as usize;
                let loop_insts = s.m.blocks[head].body.len();
                // Finish with `passes = 0` so we don't optimise twice!
                let dxil = with_shader_opt(0, || s.finish());
                let _ = iters;
                (dxil, insts, loop_insts)
            });
            let elapsed = t0.elapsed();
            let shader = HostShader {
                stage: HwStage::Cs,
                dxil,
                user_data: (0..4)
                    .map(|d| crate::emit::UserData {
                        index: d,
                        slot: 0,
                        root: RootType::Constants,
                        dword: d,
                    })
                    .collect(),
                samplers: Vec::new(),
                static_samplers: Vec::new(),
                root_cbvs: Vec::new(),
                root_srvs: Vec::new(),
                root_uavs: Vec::new(),
                root_indexed_views: Vec::new(),
            };
            (shader, inst_count, loop_insts, elapsed)
        }

        /// Builds a realistic SM 6.6 LDS + redundant barrier + non-aliasing UAV store compute shader.
        fn build_lds_barrier_cs(passes: u32) -> (HostShader, usize, usize, std::time::Duration) {
            let t0 = std::time::Instant::now();
            let (dxil, inst_count, loop_insts) = with_shader_opt(passes, || {
                let mut s = Shader::new(false);
                s.threads = Some([64, 1, 1]);
                s.user_data = 4;
                s.lds_dwords = 64;
                let ud = s.user_data_handle();
                let lds = s.lds();
                let (e, i32t, void) = (s.entry, s.t.i32, s.t.void);
                let (k0, k1, k2, k4, k9, k11, k63, k80) = (
                    s.m.i32(0),
                    s.m.i32(1),
                    s.m.i32(2),
                    s.m.i32(4),
                    s.m.i32(9),
                    s.m.i32(11),
                    s.m.i32(63),
                    s.m.i32(80),
                );
                let tid = s.op(e, "threadId.i32", Attr::ReadNone, i32t, 93, &[k0]);
                let lane = s.m.bin(e, Bin::And, tid, k63);
                let cb0 = s.op(e, "cbufferLoadLegacy.i32", Attr::ReadOnly, s.t.cbuf_ret, 59, &[ud, k0]);
                let k = s.m.extract(e, cb0, 0);
                let iters = s.m.extract(e, cb0, 1);
                let uav = s.heap_handle(e, k0, false, false, (11 | 0x1000, 0));
                let undef = s.m.undef(i32t);
                let mask1 = s.m.i8(1);

                let (head, exit) = (s.m.new_block(), s.m.new_block());
                s.m.terminate(e, Term::Br(head));
                let i_phi = s.m.phi(head, i32t);
                let acc_phi = s.m.phi(head, i32t);
                let my_ptr = s.m.element(head, lds, lane);
                let peer = s.m.bin(head, Bin::Xor, lane, k1);
                let peer_ptr = s.m.element(head, lds, peer);
                let val1 = s.m.bin(head, Bin::Add, acc_phi, k);
                let val2 = s.m.bin(head, Bin::Add, val1, lane);
                // Dead overwritten LDS store followed by real store and reload (no space-3 store->load forwarding).
                s.m.store(head, my_ptr, val1);
                s.m.store(head, my_ptr, val2);
                let self_fwd = s.m.load(head, my_ptr);
                // Real barrier(11) + redundant adjacent barrier(9) + redundant barrier(9) (eliminated!).
                s.op(head, "barrier", Attr::NoDuplicate, void, 80, &[k11]);
                let mix = s.m.bin(head, Bin::Xor, self_fwd, i_phi);
                s.op(head, "barrier", Attr::NoDuplicate, void, 80, &[k9]);
                s.op(head, "barrier", Attr::NoDuplicate, void, 80, &[k9]);
                // Cross-lane LDS read after barrier (must NOT be forwarded across barrier!).
                let neighbor = s.m.load(head, peer_ptr);
                // Another real barrier before next iteration's write to my_ptr.
                s.op(head, "barrier", Attr::NoDuplicate, void, 80, &[k9]);
                let next_acc = s.m.bin(head, Bin::Add, mix, neighbor);
                let next_i = s.m.bin(head, Bin::Add, i_phi, k1);
                let more = s.m.cmp(head, pred::IULT, next_i, iters);
                s.m.terminate(head, Term::CondBr(more, head, exit));
                s.m.set_phi(i_phi, vec![(k0, e), (next_i, head)]);
                s.m.set_phi(acc_phi, vec![(tid, e), (next_acc, head)]);

                let byte_off = s.m.bin(exit, Bin::Shl, tid, k2);
                s.op(
                    exit,
                    "rawBufferStore.i32",
                    Attr::NoUnwind,
                    void,
                    140,
                    &[uav, byte_off, undef, next_acc, undef, undef, undef, mask1, k4],
                );
                let _ = k80;
                s.m.terminate(exit, Term::Ret);
                let module_passes = passes & MODULE;
                if module_passes != 0 {
                    s.m.optimise(module_passes);
                }
                let insts = s.m.census().counts[0] as usize;
                let loop_insts = s.m.blocks[head].body.len();
                let dxil = with_shader_opt(0, || s.finish());
                (dxil, insts, loop_insts)
            });
            let elapsed = t0.elapsed();
            let shader = HostShader {
                stage: HwStage::Cs,
                dxil,
                user_data: (0..4)
                    .map(|d| crate::emit::UserData {
                        index: d,
                        slot: 0,
                        root: RootType::Constants,
                        dword: d,
                    })
                    .collect(),
                samplers: Vec::new(),
                static_samplers: Vec::new(),
                root_cbvs: Vec::new(),
                root_srvs: Vec::new(),
                root_uavs: Vec::new(),
                root_indexed_views: Vec::new(),
            };
            (shader, inst_count, loop_insts, elapsed)
        }

        /// Lowers a real synthetic RDNA2/GFX10 compute loop shader (with loop-invariant
        /// VALU arithmetic, selects, and UAV store) using `passes`.
        fn build_gfx10_loop_cs(passes: u32) -> (HostShader, std::time::Duration) {
            // RDNA2 wave32 compute shader (32 threads):
            //   s0..s3 = root constants (seed, iters, unused, unused)
            //   s4 = descriptor table address (raw UAV at heap index 0)
            //   v0 = thread id x
            //   v1 = seed, v2 = 0 (acc), s12 = 0 (counter), s13 = iters
            // loop:
            //   v3 = (v0 + s0) * 8 + 7  (loop-invariant!)
            //   v4 = (v3 ^ 5) ^ 5       (loop-invariant algebraic!)
            //   v2 = v2 + v4 + s12
            //   s12 = s12 + 1
            //   s_cmp_lt_u32 s12, s13
            //   s_cbranch_scc1 loop
            //   buffer_store_dword v2, v0*4, s[8:11], 0 offen
            //   s_endpgm
            let words: [u32; 22] = [
                0xBE85_0380, // s_mov_b32 s5, 0
                0xF408_0202,
                0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0
                0xBE8C_0380, // s_mov_b32 s12, 0 (i = 0)
                0xBE8D_0301, // s_mov_b32 s13, s1 (iters)
                0x7E04_0280, // v_mov_b32 v2, 0 (acc = 0)
                // loop body (pc = 6):
                0x4A06_0000, // v_add_nc_u32 v3, s0, v0
                0x3406_0683, // v_lshlrev_b32 v3, 3, v3
                0x4A06_0687, // v_add_nc_u32 v3, 7, v3
                0x3A08_0685, // v_xor_b32 v4, 5, v3
                0x3A08_0885, // v_xor_b32 v4, 5, v4
                0x4A04_0802, // v_add_nc_u32 v2, v2, v4  (VOP2: src0=v2? 0x102 -> use v4 + v2)
                0x4A04_040C, // v_add_nc_u32 v2, s12, v2
                0x800C_810C, // s_add_u32 s12, s12, 1
                0xBF0A_0D0C, // s_cmp_lt_u32 s12, s13
                0xBF85_FFF6, // s_cbranch_scc1 -10 (back to pc=6)
                // exit:
                0x340A_0082, // v_lshlrev_b32 v5, 2, v0
                0xE070_1000,
                0x8002_0205, // buffer_store_dword v2, v5, s[8:11], 0 offen
                0xBF81_0000, // s_endpgm
                0xBF81_0000, // padding
                0xBF81_0000,
            ];
            let mut words = words[..20].to_vec();
            // Fix VOP2 v_add_nc_u32 v2, v2, v4: src0 = 256 + 2 = 0x102, vsrc1 = 4 -> 0x4A04_0902.
            words[11] = 0x4A04_0902;
            let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
            let native = NativeProgram {
                stage: HwStage::Cs,
                wave64: false,
                vgprs: 6,
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
                    rsrc2: 5 << 1,
                    threads: [32, 1, 1],
                }),
                code,
            };
            let t0 = std::time::Instant::now();
            let lowered = with_shader_opt(passes, || {
                let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
                let st = structure::structure(&p).expect("structure");
                let res = resource::track(&p, &native.layout, native.stage, Some(&st))
                    .expect("resource::track");
                let io = io::raise(&p, &native).expect("io::raise");
                lower::lower(&lower::Parts {
                    native: &native,
                    p: &p,
                    st: &st,
                    io: &io,
                    res: &res,
                    inputs: Vec::new(),
                    outputs: Vec::new(),
                    prims: Vec::new(),
                    mesh_stride: 0,
                })
                .expect("lower")
            });
            let elapsed = t0.elapsed();
            let shader = HostShader {
                stage: HwStage::Cs,
                dxil: lowered.dxil,
                user_data: lowered.user_data,
                samplers: lowered.samplers,
                static_samplers: Vec::new(),
                root_cbvs: lowered.root_cbvs,
                root_srvs: lowered.root_srvs,
                root_uavs: lowered.root_uavs,
                root_indexed_views: Vec::new(),
            };
            (shader, elapsed)
        }

        /// Executes a compute pipeline with D3D12 hardware GPU timestamp queries and
        /// returns `(uav_output_first_run, median_gpu_us)`. When `extra_srvs > 0`,
        /// places that many 32-dword structured SRVs at view-heap slots `0..extra_srvs`
        /// and the raw UAV at slot `extra_srvs`.
        #[allow(clippy::too_many_arguments)]
        fn dispatch_timed(
            host: &Host,
            shader: &HostShader,
            root_consts: &[u32],
            uav_dwords: u32,
            extra_srvs: usize,
            groups: [u32; 3],
            warmups: usize,
            samples: usize,
        ) -> Result<(Vec<u32>, f64), String> {
            use windows::Win32::Graphics::Direct3D12::*;
            use windows::Win32::Graphics::Dxgi::Common::*;
            use windows::core::Interface;

            let err = |e: windows::core::Error| e.message().to_string();
            let (root, pso) = host.compute_pipeline(shader)?;
            let dev = &host.device;
            // SAFETY: standard D3D12 resource/command/query lifetime management;
            // GPU completion is fenced before every map/readback.
            unsafe {
                let queue: ID3D12CommandQueue = dev
                    .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                        Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                        ..Default::default()
                    })
                    .map_err(err)?;
                let freq = queue.GetTimestampFrequency().map_err(err)?;
                let alloc: ID3D12CommandAllocator = dev
                    .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                    .map_err(err)?;
                let list: ID3D12GraphicsCommandList = dev
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, &pso)
                    .map_err(err)?;
                list.Close().map_err(err)?;

                let make_buf = |heap: D3D12_HEAP_TYPE, bytes: u64, flags, state| {
                    let mut r: Option<ID3D12Resource> = None;
                    let desc = D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                        Width: bytes,
                        Height: 1,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                        Flags: flags,
                        ..Default::default()
                    };
                    dev.CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES { Type: heap, ..Default::default() },
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        state,
                        None,
                        &mut r,
                    )
                    .map_err(err)?;
                    r.ok_or_else(|| "no buffer".to_owned())
                };

                let uav_bytes = u64::from(uav_dwords) * 4;
                let uav = make_buf(
                    D3D12_HEAP_TYPE_DEFAULT,
                    uav_bytes,
                    D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_COMMON,
                )?;
                let uav_rb = make_buf(
                    D3D12_HEAP_TYPE_READBACK,
                    uav_bytes,
                    D3D12_RESOURCE_FLAG_NONE,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                )?;
                let ts_rb = make_buf(
                    D3D12_HEAP_TYPE_READBACK,
                    16,
                    D3D12_RESOURCE_FLAG_NONE,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                )?;
                let view_heap: ID3D12DescriptorHeap = dev
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                        NumDescriptors: (extra_srvs + 1) as u32,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        ..Default::default()
                    })
                    .map_err(err)?;
                let inc = dev.GetDescriptorHandleIncrementSize(
                    D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                ) as usize;
                let mut cpu = view_heap.GetCPUDescriptorHandleForHeapStart();
                let mut keep_srvs = Vec::with_capacity(extra_srvs);
                for _ in 0..extra_srvs {
                    let srv = make_buf(
                        D3D12_HEAP_TYPE_UPLOAD,
                        256,
                        D3D12_RESOURCE_FLAG_NONE,
                        D3D12_RESOURCE_STATE_GENERIC_READ,
                    )?;
                    let srv_desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                        Format: DXGI_FORMAT_UNKNOWN,
                        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                            Buffer: D3D12_BUFFER_SRV {
                                FirstElement: 0,
                                NumElements: 32,
                                StructureByteStride: 4,
                                Flags: D3D12_BUFFER_SRV_FLAG_NONE,
                            },
                        },
                    };
                    dev.CreateShaderResourceView(&srv, Some(&srv_desc), cpu);
                    cpu.ptr += inc;
                    keep_srvs.push(srv);
                }
                let uav_desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                    Format: DXGI_FORMAT_R32_TYPELESS,
                    ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                    Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_UAV {
                            NumElements: uav_dwords,
                            Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                            ..Default::default()
                        },
                    },
                };
                dev.CreateUnorderedAccessView(&uav, None, Some(&uav_desc), cpu);
                let _ = &keep_srvs;
                let mut qheap: Option<ID3D12QueryHeap> = None;
                dev.CreateQueryHeap(
                    &D3D12_QUERY_HEAP_DESC {
                        Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                        Count: 2,
                        NodeMask: 0,
                    },
                    &mut qheap,
                )
                .map_err(err)?;
                let qheap = qheap.ok_or_else(|| "no query heap".to_owned())?;
                let fence: ID3D12Fence =
                    dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(err)?;
                let mut fence_val = 0u64;
                let mut times_us = Vec::with_capacity(samples);
                let mut first_out = Vec::new();
                let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
                for u in &shader.user_data {
                    ud[u.index as usize] = match (u.root, u.slot) {
                        (RootType::Constants, 0) => root_consts[u.dword as usize],
                        (RootType::Table, _) => 0,
                        _ => 0,
                    };
                }

                for run_idx in 0..(warmups + samples) {
                    alloc.Reset().map_err(err)?;
                    list.Reset(&alloc, &pso).map_err(err)?;
                    list.SetDescriptorHeaps(&[Some(view_heap.clone())]);
                    list.SetComputeRootSignature(&root);
                    list.SetComputeRoot32BitConstants(0, ud.len() as u32, ud.as_ptr().cast(), 0);
                    list.EndQuery(&qheap, D3D12_QUERY_TYPE_TIMESTAMP, 0);
                    list.Dispatch(groups[0], groups[1], groups[2]);
                    list.EndQuery(&qheap, D3D12_QUERY_TYPE_TIMESTAMP, 1);
                    list.ResolveQueryData(&qheap, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, &ts_rb, 0);
                    if run_idx == 0 {
                        let barrier = D3D12_RESOURCE_BARRIER {
                            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                                Transition: std::mem::ManuallyDrop::new(
                                    D3D12_RESOURCE_TRANSITION_BARRIER {
                                        pResource: std::mem::transmute_copy(&uav),
                                        Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                                        StateBefore: D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                        StateAfter: D3D12_RESOURCE_STATE_COPY_SOURCE,
                                    },
                                ),
                            },
                        };
                        list.ResourceBarrier(&[barrier]);
                        list.CopyBufferRegion(&uav_rb, 0, &uav, 0, uav_bytes);
                        let back = D3D12_RESOURCE_BARRIER {
                            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                                Transition: std::mem::ManuallyDrop::new(
                                    D3D12_RESOURCE_TRANSITION_BARRIER {
                                        pResource: std::mem::transmute_copy(&uav),
                                        Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                                        StateBefore: D3D12_RESOURCE_STATE_COPY_SOURCE,
                                        StateAfter: D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                    },
                                ),
                            },
                        };
                        list.ResourceBarrier(&[back]);
                    }
                    list.Close().map_err(err)?;
                    queue.ExecuteCommandLists(&[Some(list.cast().map_err(err)?)]);
                    fence_val += 1;
                    queue.Signal(&fence, fence_val).map_err(err)?;
                    while fence.GetCompletedValue() < fence_val {
                        if let Err(e) = dev.GetDeviceRemovedReason() {
                            return Err(format!("device removed: {}", err(e)));
                        }
                        std::thread::yield_now();
                    }
                    if run_idx == 0 {
                        let mut p = std::ptr::null_mut();
                        uav_rb.Map(0, None, Some(&mut p)).map_err(err)?;
                        first_out =
                            std::slice::from_raw_parts(p.cast::<u32>(), uav_dwords as usize)
                                .to_vec();
                        uav_rb.Unmap(0, None);
                    }
                    if run_idx >= warmups {
                        let mut p = std::ptr::null_mut();
                        ts_rb.Map(0, None, Some(&mut p)).map_err(err)?;
                        let ts = *p.cast::<[u64; 2]>();
                        ts_rb.Unmap(0, None);
                        let us = (ts[1].saturating_sub(ts[0]) as f64) * 1e6 / (freq as f64);
                        times_us.push(us);
                    }
                }
                times_us.sort_by(|a, b| a.total_cmp(b));
                let median = times_us[times_us.len() / 2];
                Ok((first_out, median))
            }
        }

        #[test]
        fn dxil_opt_hw_gpu_bit_exact_parity_and_validation() {
            let _one = crate::emit::valu_tests::ONE_AT_A_TIME
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let (loop_none, insts_loop_0, loop_body_0, _) = build_loop_licm_fold_cs(0, 64);
            let (loop_all, insts_loop_all, loop_body_all, _) = build_loop_licm_fold_cs(ALL, 64);
            assert!(
                insts_loop_all < insts_loop_0 && loop_body_all < loop_body_0,
                "expected instruction & loop-body reduction: total {insts_loop_all}/{insts_loop_0}, loop {loop_body_all}/{loop_body_0}"
            );
            validate_dxil(&loop_none.dxil);
            validate_dxil(&loop_all.dxil);

            let (lds_none, insts_lds_0, lds_body_0, _) = build_lds_barrier_cs(0);
            let (lds_all, insts_lds_all, lds_body_all, _) = build_lds_barrier_cs(ALL);
            assert!(
                insts_lds_all < insts_lds_0 && lds_body_all < lds_body_0,
                "expected LDS/barrier reduction: total {insts_lds_all}/{insts_lds_0}, loop {lds_body_all}/{lds_body_0}"
            );
            validate_dxil(&lds_none.dxil);
            validate_dxil(&lds_all.dxil);

            let (gfx_none, _) = build_gfx10_loop_cs(0);
            let (gfx_all, _) = build_gfx10_loop_cs(DEFAULT);
            assert!(gfx_all.dxil.len() < gfx_none.dxil.len());
            validate_dxil(&gfx_none.dxil);
            validate_dxil(&gfx_all.dxil);

            let Some(host) = hw_host() else { return };

            // 1) Loop + LICM + FOLD + NaN + guarded dynamic divisor parity:
            //    Test both `limit = 64, dyn_div = 5, NaN` AND `limit = 0, dyn_div = 0`
            //    (proves LICM did NOT speculatively hoist `udiv` by 0 out of the loop!).
            for consts in [
                [17, 64, 5, f32::NAN.to_bits()],
                [42, 64, 3, 0.5f32.to_bits()],
                [99, 0, 0, 2.0f32.to_bits()],
            ] {
                let (out0, _) =
                    dispatch_timed(&host, &loop_none, &consts, 256, 0, [4, 1, 1], 1, 1).unwrap();
                let (out_all, _) =
                    dispatch_timed(&host, &loop_all, &consts, 256, 0, [4, 1, 1], 1, 1).unwrap();
                assert_eq!(out_all, out0, "loop shader parity mismatch for {consts:?}");
            }

            // 2) LDS forwarding + redundant barrier elimination parity:
            let lds_consts = [13, 32, 0, 0];
            let (lds_out0, _) =
                dispatch_timed(&host, &lds_none, &lds_consts, 256, 0, [4, 1, 1], 1, 1).unwrap();
            let (lds_out_all, _) =
                dispatch_timed(&host, &lds_all, &lds_consts, 256, 0, [4, 1, 1], 1, 1).unwrap();
            assert_eq!(lds_out_all, lds_out0, "LDS/barrier shader parity mismatch");

            // 3) Real GFX10 translated compute loop parity:
            let (root0, pso0) = host.compute_pipeline(&gfx_none).unwrap();
            let (root_a, pso_a) = host.compute_pipeline(&gfx_all).unwrap();
            let ud = vec![11u32, 32, 0, 0, 0, 0, 0, 0];
            let g0 = host
                .dispatch(
                    &root0,
                    &pso0,
                    &Dispatch {
                        roots: vec![(0, Root::Constants(ud.clone()))],
                        views: &[View::RawUav(32)],
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap();
            let ga = host
                .dispatch(
                    &root_a,
                    &pso_a,
                    &Dispatch {
                        roots: vec![(0, Root::Constants(ud))],
                        views: &[View::RawUav(32)],
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap();
            assert_eq!(ga[0], g0[0], "GFX10 translated compute loop parity mismatch");
            // Verify against analytical result: for lane l, each iter adds ((l+11)*8 + 7) + iter.
            for (l, &got) in ga[0].iter().enumerate() {
                let inv = (l as u32 + 11).wrapping_mul(8).wrapping_add(7);
                let want: u32 = (0..32u32).map(|it| inv.wrapping_add(it)).fold(0u32, u32::wrapping_add);
                assert_eq!(got, want, "lane {l} analytical mismatch");
            }

            // 4) Real GFX10 waterfall compute shader parity (`opt=0` vs `opt=ALL`):
            let wf_build = |passes: u32| -> HostShader {
                let words = crate::resource::tests::WATERFALL_JOINED_BUFFER_WAVE32;
                let table = |user_data: u8| RootParam {
                    root: RootType::Table,
                    table_offset_dwords: 0,
                    sgprs: 1,
                    base_register: 0,
                    user_data,
                    slot: user_data,
                };
                let range = |kind, table_offset_dwords, dwords, stride, user_data| UsedResource {
                    kind,
                    root: RootType::Table,
                    table_offset_dwords,
                    lower_bound: 0,
                    stride,
                    dwords,
                    user_data,
                    slot: user_data,
                };
                let native = NativeProgram {
                    stage: HwStage::Cs,
                    wave64: false,
                    vgprs: 32,
                    layout: vec![table(0), table(1)],
                    used: vec![
                        range(ResourceType::Srv, 0, 4, 4, 0),
                        range(ResourceType::Uav, 16, 4, 0, 0),
                        range(ResourceType::Sampler, 0, 4, 0, 1),
                    ],
                    io: StageIo::Cs(ComputeIo {
                        rsrc2: 2 << 1,
                        threads: [32, 1, 1],
                    }),
                    code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
                };
                let lowered = with_shader_opt(passes, || {
                    let p = ir::build(&native.code, native.wave64, native.vgprs).unwrap();
                    let st = structure::structure(&p).unwrap();
                    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
                    let io = io::raise(&p, &native).unwrap();
                    lower::lower(&lower::Parts {
                        native: &native,
                        p: &p,
                        st: &st,
                        io: &io,
                        res: &res,
                        inputs: Vec::new(),
                        outputs: Vec::new(),
                        prims: Vec::new(),
                        mesh_stride: 0,
                    })
                    .unwrap()
                });
                let shader = HostShader {
                    stage: HwStage::Cs,
                    dxil: lowered.dxil,
                    user_data: lowered.user_data,
                    samplers: lowered.samplers,
                    static_samplers: Vec::new(),
                    root_cbvs: lowered.root_cbvs,
                    root_srvs: lowered.root_srvs,
                    root_uavs: lowered.root_uavs,
                    root_indexed_views: Vec::new(),
                };
                validate_dxil(&shader.dxil);
                shader
            };
            let wf0 = wf_build(0);
            let wfa = wf_build(DEFAULT);
            assert!(wfa.dxil.len() < wf0.dxil.len(), "waterfall shader should shrink");
            let first: Vec<u32> = (0..32).map(|i| 100 + i).collect();
            let second: Vec<u32> = (0..32).map(|i| 200 + i).collect();
            let wf_views = [
                View::Buffer {
                    data: &first,
                    stride: crate::emit::host::Stride::Structured(4),
                },
                View::Buffer {
                    data: &second,
                    stride: crate::emit::host::Stride::Structured(4),
                },
                View::RawUav(32),
            ];
            let (wf_r0, wf_p0) = host.compute_pipeline(&wf0).unwrap();
            let (wf_ra, wf_pa) = host.compute_pipeline(&wfa).unwrap();
            let wf_ud0 = vec![0u32; wf0.user_data_dwords().max(1) as usize];
            let wf_uda = vec![0u32; wfa.user_data_dwords().max(1) as usize];
            let out_wf0 = host
                .dispatch(
                    &wf_r0,
                    &wf_p0,
                    &Dispatch {
                        roots: vec![(0, Root::Constants(wf_ud0))],
                        views: &wf_views,
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap();
            let out_wfa = host
                .dispatch(
                    &wf_ra,
                    &wf_pa,
                    &Dispatch {
                        roots: vec![(0, Root::Constants(wf_uda))],
                        views: &wf_views,
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap();
            assert_eq!(out_wfa[0], out_wf0[0], "waterfall compute shader parity mismatch");
        }

        #[test]
        fn split_lds_wave32_read_after_read_barrier_coalescing_and_parity() {
            let _one = crate::emit::valu_tests::ONE_AT_A_TIME
                .lock()
                .unwrap_or_else(|e| e.into_inner());

            let dump_lds_pattern = |container: &[u8]| -> Option<String> {
                let dxc = match std::env::var_os("VICTORY_PC_DXC") {
                    Some(p) => std::path::PathBuf::from(p),
                    None => validator::Validator::default_path()?.with_file_name("dxc.exe"),
                };
                if !dxc.exists() {
                    return None;
                }
                let file = std::env::temp_dir().join(format!(
                    "victory-rar-{}-{:?}.dxil",
                    std::process::id(),
                    std::thread::current().id()
                ));
                std::fs::write(&file, container).ok()?;
                let out = std::process::Command::new(dxc)
                    .arg("-dumpbin")
                    .arg(&file)
                    .output()
                    .ok()?;
                let _ = std::fs::remove_file(&file);
                assert!(
                    out.status.success(),
                    "dxc -dumpbin failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                let text = String::from_utf8_lossy(&out.stdout);
                Some(
                    text.lines()
                        .filter_map(|line| {
                            let t = line.trim_start();
                            if t.contains("call void @dx.op.barrier(") {
                                Some('B')
                            } else if line.contains("load i32, i32 addrspace(3)*") {
                                Some('R')
                            } else if t.starts_with("store i32") && line.contains("addrspace(3)*") {
                                Some('W')
                            } else if t.contains("atomicrmw ") && line.contains("addrspace(3)*") {
                                Some('A')
                            } else {
                                None
                            }
                        })
                        .collect(),
                )
            };

            let lower_split_lds_cs = |body: &[u32], passes: u32| -> HostShader {
                let mut words = vec![
                    0xBE85_0380, // s_mov_b32 s5, 0
                    0x3D << 26 | 0x02 << 18 | 8 << 6 | 2,
                    125 << 25, // s_load_dwordx4 s[8:11], s[4:5], 0
                ];
                words.extend_from_slice(body);
                words.push(0xBF81_0000); // s_endpgm
                let native = NativeProgram {
                    stage: HwStage::Cs,
                    wave64: true,
                    vgprs: 16,
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
                        rsrc2: (5 << 1) | (1 << 15), // 5 user SGPRs + 64 dwords LDS
                        threads: [64, 1, 1],
                    }),
                    code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
                };
                let lowered = with_shader_opt(passes, || {
                    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
                    let st = structure::structure(&p).expect("structure");
                    let res = resource::track(&p, &native.layout, native.stage, Some(&st))
                        .expect("resource::track");
                    let io = io::raise(&p, &native).expect("io::raise");
                    let was = lower::TEST_HOST_WAVE.replace(Some(32));
                    let res_lower = lower::lower(&lower::Parts {
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
                    res_lower.expect("lower split_lds Wave32")
                });
                assert_eq!(lowered.wave_size, Some(32), "expected Wave32 split_lds lowering");
                let shader = HostShader {
                    stage: HwStage::Cs,
                    dxil: lowered.dxil,
                    user_data: lowered.user_data,
                    samplers: lowered.samplers,
                    static_samplers: Vec::new(),
                    root_cbvs: lowered.root_cbvs,
                    root_srvs: lowered.root_srvs,
                    root_uavs: lowered.root_uavs,
                    root_indexed_views: Vec::new(),
                };
                validate_dxil(&shader.dxil);
                shader
            };

            // 1) Adjacent `write -> read -> read -> overwrite -> read`:
            //    Thread `t` writes `t`, reads partner `t ^ 32` (`v4`) and own `t` (`v5`),
            //    overwrites own slot with `v4 + 25`, then reads partner `t + 25` (`v6`).
            let rar_words: [u32; 20] = [
                0x3404_0082, // v2 = t * 4
                0x3A06_00A0, // v3 = t ^ 32
                0x3406_0682, // v3 = (t ^ 32) * 4
                0xD834_0000,
                0x0000_0002, // ds_write_b32 v2, v0
                0xD8D8_0000,
                0x0400_0003, // ds_read_b32 v4, v3
                0xD8D8_0000,
                0x0500_0002, // ds_read_b32 v5, v2
                0x4A0E_0899, // v7 = v4 + 25
                0xD834_0000,
                0x0000_0702, // ds_write_b32 v2, v7
                0xD8D8_0000,
                0x0600_0003, // ds_read_b32 v6, v3
                0xE070_1000,
                0x8002_0402, // out[t] = v4
                0xE070_1100,
                0x8002_0502, // out[64 + t] = v5
                0xE070_1200,
                0x8002_0602, // out[128 + t] = v6
            ];
            let mut alu_between_words = rar_words.to_vec();
            // Move pure VALU `v_mov_b32 v8, 7` between the two `ds_read_b32` instructions.
            alu_between_words.insert(7, 0x7E10_0287);

            // End-of-shader `write -> read -> read -> UAV stores -> s_endpgm` (3 -> 2 barriers).
            let rar_end_words: [u32; 13] = [
                0x3404_0082, // v2 = t * 4
                0x3A06_00A0, // v3 = t ^ 32
                0x3406_0682, // v3 = (t ^ 32) * 4
                0xD834_0000,
                0x0000_0002, // ds_write_b32 v2, v0
                0xD8D8_0000,
                0x0400_0003, // ds_read_b32 v4, v3
                0xD8D8_0000,
                0x0500_0002, // ds_read_b32 v5, v2
                0xE070_1000,
                0x8002_0402, // out[t] = v4
                0xE070_1100,
                0x8002_0502, // out[64 + t] = v5
            ];

            // Negative 1: `write -> read -> write -> read` (keeps all 4 barriers).
            let neg_wrwr_words: [u32; 16] = [
                0x3404_0082, // v2 = t * 4
                0x3A06_00A0, // v3 = t ^ 32
                0x3406_0682, // v3 = (t ^ 32) * 4
                0xD834_0000,
                0x0000_0002, // ds_write_b32 v2, v0
                0xD8D8_0000,
                0x0400_0003, // ds_read_b32 v4, v3
                0x4A0E_0899, // v7 = v4 + 25
                0xD834_0000,
                0x0000_0702, // ds_write_b32 v2, v7
                0xD8D8_0000,
                0x0600_0003, // ds_read_b32 v6, v3
                0xE070_1000,
                0x8002_0402, // out[t] = v4
                0xE070_1200,
                0x8002_0602, // out[128 + t] = v6
            ];

            // Negative 2: LDS atomic (`ds_add_u32`) between two reads keeps all 4 barriers.
            let neg_atomic_words: [u32; 16] = [
                0x3404_0082, // v2 = t * 4
                0x3A06_00A0, // v3 = t ^ 32
                0x3406_0682, // v3 = (t ^ 32) * 4
                0x7E10_0287, // v8 = 7
                0xD834_0000,
                0x0000_0002, // ds_write_b32 v2, v0
                0xD8D8_0000,
                0x0400_0003, // ds_read_b32 v4, v3
                0xD800_0000,
                0x0000_0802, // ds_add_u32 v2, v8
                0xD8D8_0000,
                0x0500_0002, // ds_read_b32 v5, v2 (t + 7)
                0xE070_1000,
                0x8002_0402, // out[t] = v4
                0xE070_1100,
                0x8002_0502, // out[64 + t] = v5
            ];

            // Negative 3: UAV `buffer_load_dword` call between two reads keeps all 5 barriers.
            let mut neg_uav_load_words = rar_words.to_vec();
            neg_uav_load_words.splice(7..7, [0xE030_1000, 0x8002_0802]); // buffer_load_dword v8, v2, s[8:11], 0 offen

            let hw = hw_host();
            let warp = Host::new(true).ok();

            let check_192 = |out: &[u32], ctx: &str| {
                for t in 0..64u32 {
                    let i = t as usize;
                    assert_eq!(
                        [out[i], out[64 + i], out[128 + i]],
                        [t ^ 32, t, t + 25],
                        "{ctx}, thread {t}"
                    );
                }
            };

            for (label, body, unopt_pat, opt_pat) in [
                ("adjacent_rar", rar_words.as_slice(), "BWBRBRBWBR", "BWBRRBWBR"),
                (
                    "pure_alu_between_reads",
                    alu_between_words.as_slice(),
                    "BWBRBRBWBR",
                    "BWBRRBWBR",
                ),
                ("rar_before_ret", rar_end_words.as_slice(), "BWBRBR", "BWBRR"),
                (
                    "neg_write_between_reads",
                    neg_wrwr_words.as_slice(),
                    "BWBRBWBR",
                    "BWBRBWBR",
                ),
                (
                    "neg_atomic_between_reads",
                    neg_atomic_words.as_slice(),
                    "BWBRBABR",
                    "BWBRBABR",
                ),
                (
                    "neg_uav_load_between_reads",
                    neg_uav_load_words.as_slice(),
                    "BWBRBRBWBR",
                    "BWBRBRBWBR",
                ),
            ] {
                let s0 = lower_split_lds_cs(body, 0);
                let sb = lower_split_lds_cs(body, BARRIERS);
                let sa = lower_split_lds_cs(body, DEFAULT);
                assert!(
                    sa.dxil.len() <= s0.dxil.len(),
                    "{label}: ALL passes should not grow DXIL ({} vs {})",
                    sa.dxil.len(),
                    s0.dxil.len()
                );
                if let Some(p0) = dump_lds_pattern(&s0.dxil) {
                    let pb = dump_lds_pattern(&sb.dxil).expect("BARRIERS dump");
                    let pa = dump_lds_pattern(&sa.dxil).expect("ALL dump");
                    assert_eq!(p0, unopt_pat, "{label} mask=0 pattern");
                    assert_eq!(pb, opt_pat, "{label} mask=BARRIERS pattern");
                    assert_eq!(pa, opt_pat, "{label} mask=ALL pattern");
                    eprintln!("{label}: opt=0 `{p0}` -> BARRIERS `{pb}`, ALL `{pa}`");
                }
                for (host_name, host_opt) in [("hw", hw.as_ref()), ("warp", warp.as_ref())] {
                    let Some(h) = host_opt else { continue };
                    let Ok((r0, p0)) = h.compute_pipeline(&s0) else {
                        // Older Windows 10 WARP refuses explicit `WaveSize(32)`;
                        // WARP execution is additionally covered below without `WaveSize`.
                        assert_ne!(
                            host_name, "hw",
                            "{label}: hardware Wave32 GPU rejected PSO"
                        );
                        continue;
                    };
                    let (rb, pb) = h.compute_pipeline(&sb).expect("BARRIERS PSO");
                    let (ra, pa) = h.compute_pipeline(&sa).expect("ALL PSO");
                    let run_one = |r: &_, p: &_, sh: &HostShader| -> Vec<u32> {
                        let ud = vec![0u32; sh.user_data_dwords().max(1) as usize];
                        h.dispatch(
                            r,
                            p,
                            &Dispatch {
                                roots: vec![(0, Root::Constants(ud))],
                                views: &[View::RawUav(192)],
                                samplers: &[],
                                groups: [1, 1, 1],
                            },
                        )
                        .expect("dispatch")[0]
                            .clone()
                    };
                    let out0 = run_one(&r0, &p0, &s0);
                    let outb = run_one(&rb, &pb, &sb);
                    let outa = run_one(&ra, &pa, &sa);
                    assert_eq!(outb, out0, "{label} ({host_name}): BARRIERS vs 0 mismatch");
                    assert_eq!(outa, out0, "{label} ({host_name}): ALL vs 0 mismatch");
                    if label == "adjacent_rar"
                        || label == "pure_alu_between_reads"
                        || label == "neg_uav_load_between_reads"
                    {
                        check_192(&outa, &format!("{label} ({host_name})"));
                    } else if label == "rar_before_ret" {
                        for t in 0..64u32 {
                            let i = t as usize;
                            assert_eq!([outa[i], outa[64 + i]], [t ^ 32, t], "{label} thread {t}");
                        }
                    } else if label == "neg_atomic_between_reads" {
                        for t in 0..64u32 {
                            let i = t as usize;
                            assert_eq!(
                                [outa[i], outa[64 + i]],
                                [t ^ 32, t + 7],
                                "{label} thread {t}"
                            );
                        }
                    }
                }
            }

            // Unconditional WARP execution (wave-width-neutral SM 6.6 `write -> read -> read -> overwrite -> read`,
            // 5 -> 4 barriers) so even Windows 10 WARP without `WaveSize(32)` executes every lane & dword.
            let warp = Host::new(true).expect("Microsoft D3D12 WARP adapter required");
            let build_warp_rar_cs = |passes: u32| -> (HostShader, usize) {
                let (dxil, barriers_left): (Vec<u8>, usize) = with_shader_opt(passes, || {
                    let mut s = Shader::new(false);
                    s.threads = Some([64, 1, 1]);
                    s.user_data = 1;
                    s.lds_dwords = 64;
                    let lds = s.lds();
                    let (e, i32t, void) = (s.entry, s.t.i32, s.t.void);
                    let (k0, k2, k4, k9, k25, k32, k64, k128, k80) = (
                        s.m.i32(0),
                        s.m.i32(2),
                        s.m.i32(4),
                        s.m.i32(9),
                        s.m.i32(25),
                        s.m.i32(32),
                        s.m.i32(64),
                        s.m.i32(128),
                        s.m.i32(80),
                    );
                    let _ = k80;
                    let tid = s.op(e, "threadId.i32", Attr::ReadNone, i32t, 93, &[k0]);
                    let partner = s.m.bin(e, Bin::Xor, tid, k32);
                    let self_ptr = s.m.element(e, lds, tid);
                    let peer_ptr = s.m.element(e, lds, partner);

                    s.op(e, "barrier", Attr::NoDuplicate, void, 80, &[k9]);
                    s.m.store(e, self_ptr, tid);
                    s.op(e, "barrier", Attr::NoDuplicate, void, 80, &[k9]);
                    let r_peer = s.m.load(e, peer_ptr);
                    s.op(e, "barrier", Attr::NoDuplicate, void, 80, &[k9]);
                    let r_self = s.m.load(e, self_ptr);
                    let overwritten = s.m.bin(e, Bin::Add, r_peer, k25);
                    s.op(e, "barrier", Attr::NoDuplicate, void, 80, &[k9]);
                    s.m.store(e, self_ptr, overwritten);
                    s.op(e, "barrier", Attr::NoDuplicate, void, 80, &[k9]);
                    let r_after = s.m.load(e, peer_ptr);

                    let uav = s.heap_handle(e, k0, false, false, (11 | 0x1000, 0));
                    let undef = s.m.undef(i32t);
                    let mask1 = s.m.i8(1);
                    for (base, val) in [(k0, r_peer), (k64, r_self), (k128, r_after)] {
                        let idx = s.m.bin(e, Bin::Add, tid, base);
                        let off = s.m.bin(e, Bin::Shl, idx, k2);
                        s.op(
                            e,
                            "rawBufferStore.i32",
                            Attr::NoUnwind,
                            void,
                            140,
                            &[uav, off, undef, val, undef, undef, undef, mask1, k4],
                        );
                    }
                    s.m.terminate(e, Term::Ret);
                    let module_passes = passes & MODULE;
                    if module_passes != 0 {
                        s.m.optimise(module_passes);
                    }
                    let b_idx = crate::emit::census::KINDS
                        .iter()
                        .position(|&n| n == "barrier")
                        .unwrap();
                    let barriers_left = s.m.census().counts[b_idx] as usize;
                    let dxil = with_shader_opt(0, || s.finish());
                    (dxil, barriers_left)
                });
                validate_dxil(&dxil);
                let shader = HostShader {
                    stage: HwStage::Cs,
                    dxil,
                    user_data: Vec::new(),
                    samplers: Vec::new(),
                    static_samplers: Vec::new(),
                    root_cbvs: Vec::new(),
                    root_srvs: Vec::new(),
                    root_uavs: Vec::new(),
                    root_indexed_views: Vec::new(),
                };
                (shader, barriers_left)
            };

            let (w0, b_cnt0) = build_warp_rar_cs(0);
            let (wb, b_cntb) = build_warp_rar_cs(BARRIERS);
            let (wa, b_cnta) = build_warp_rar_cs(ALL);
            assert_eq!(b_cnt0, 5, "unoptimized fixture must have 5 barriers");
            assert_eq!(b_cntb, 4, "BARRIERS fixture must coalesce 5 -> 4 barriers");
            assert_eq!(b_cnta, 4, "ALL fixture must coalesce 5 -> 4 barriers");
            for (lbl, sh) in [("warp_0", &w0), ("warp_BARRIERS", &wb), ("warp_ALL", &wa)] {
                let (r, p) = warp.compute_pipeline(sh).expect("WARP compute_pipeline");
                let out = warp
                    .dispatch(
                        &r,
                        &p,
                        &Dispatch {
                            roots: vec![(0, Root::Constants(vec![0]))],
                            views: &[View::RawUav(192)],
                            samplers: &[],
                            groups: [1, 1, 1],
                        },
                    )
                    .expect("WARP dispatch");
                check_192(&out[0], lbl);
            }
        }

        #[test]
        #[ignore = "GPU benchmark: cargo test --release -p victory-shader --locked bench_dxil_opt_hw_gpu -- --ignored --nocapture"]
        fn bench_dxil_opt_hw_gpu() {
            let _one = crate::emit::valu_tests::ONE_AT_A_TIME
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let host = hw_host().expect("hardware SM 6.6 GPU required for GPU benchmark");

            let old_mask = DEAD | FOLD | REUSE | FACTS | LOADS | FIRST;
            let configs = [
                ("opt=0 (none)", 0u32),
                ("opt=old_bits (no LICM/BARRIERS)", old_mask),
                ("opt=DEFAULT (new)", DEFAULT),
            ];

            println!("\n=== Benchmark 1: Loop + LICM + FOLD + Select/ZExt (4096 groups x 64 threads, 256 iters) ===");
            let loop_consts = [17u32, 256, 5, 0.5f32.to_bits()];
            let mut ref_out = None;
            for (label, mask) in configs {
                let (shader, insts, loop_body, compile_t) = build_loop_licm_fold_cs(mask, 256);
                validate_dxil(&shader.dxil);
                let (out, gpu_us) = dispatch_timed(
                    &host,
                    &shader,
                    &loop_consts,
                    4096 * 64,
                    0,
                    [4096, 1, 1],
                    5,
                    25,
                )
                .unwrap();
                match &ref_out {
                    None => ref_out = Some(out),
                    Some(r) => assert_eq!(&out, r, "parity failed for {label}"),
                }
                println!(
                    "{label:<32} | insts={insts:4} (loop={loop_body:2}) | dxil={:5} B | build={:7.1} us | gpu_median={:8.2} us ({:.3} ms)",
                    shader.dxil.len(),
                    compile_t.as_secs_f64() * 1e6,
                    gpu_us,
                    gpu_us / 1e3,
                );
            }

            println!("\n=== Benchmark 2: LDS Forwarding + Redundant Barriers (4096 groups x 64 threads, 128 iters) ===");
            let lds_consts = [13u32, 128, 0, 0];
            let mut ref_out = None;
            for (label, mask) in configs {
                let (shader, insts, loop_body, compile_t) = build_lds_barrier_cs(mask);
                validate_dxil(&shader.dxil);
                let (out, gpu_us) = dispatch_timed(
                    &host,
                    &shader,
                    &lds_consts,
                    4096 * 64,
                    0,
                    [4096, 1, 1],
                    5,
                    25,
                )
                .unwrap();
                match &ref_out {
                    None => ref_out = Some(out),
                    Some(r) => assert_eq!(&out, r, "parity failed for {label}"),
                }
                println!(
                    "{label:<32} | insts={insts:4} (loop={loop_body:2}) | dxil={:5} B | build={:7.1} us | gpu_median={:8.2} us ({:.3} ms)",
                    shader.dxil.len(),
                    compile_t.as_secs_f64() * 1e6,
                    gpu_us,
                    gpu_us / 1e3,
                );
            }

            println!("\n=== Benchmark 3: Real GFX10 Translated Compute Loop (8192 groups x 32 threads, 512 iters) ===");
            let gfx_consts = [11u32, 512, 0, 0];
            let mut ref_out = None;
            for (label, mask) in configs {
                let (shader, compile_t) = build_gfx10_loop_cs(mask);
                let insts = crate::emit::census::take(&shader.dxil).map(|c| c.counts[0]);
                validate_dxil(&shader.dxil);
                let (out, gpu_us) = dispatch_timed(
                    &host,
                    &shader,
                    &gfx_consts,
                    8192 * 32,
                    0,
                    [8192, 1, 1],
                    5,
                    25,
                )
                .unwrap();
                match &ref_out {
                    None => ref_out = Some(out),
                    Some(r) => assert_eq!(&out, r, "parity failed for {label}"),
                }
                println!(
                    "{label:<32} | insts={insts:?} | dxil={:5} B | translate={:7.1} us | gpu_median={:8.2} us ({:.3} ms)",
                    shader.dxil.len(),
                    compile_t.as_secs_f64() * 1e6,
                    gpu_us,
                    gpu_us / 1e3,
                );
            }

            println!("\n=== Benchmark 4: Real GFX10 Waterfall Compute Shader (4096 groups x 32 threads) ===");
            let wf_words = crate::resource::tests::WATERFALL_JOINED_BUFFER_WAVE32;
            let table = |user_data: u8| RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data,
                slot: user_data,
            };
            let range = |kind, table_offset_dwords, dwords, stride, user_data| UsedResource {
                kind,
                root: RootType::Table,
                table_offset_dwords,
                lower_bound: 0,
                stride,
                dwords,
                user_data,
                slot: user_data,
            };
            let wf_native = NativeProgram {
                stage: HwStage::Cs,
                wave64: false,
                vgprs: 32,
                layout: vec![table(0), table(1)],
                used: vec![
                    range(ResourceType::Srv, 0, 4, 4, 0),
                    range(ResourceType::Uav, 16, 4, 0, 0),
                    range(ResourceType::Sampler, 0, 4, 0, 1),
                ],
                io: StageIo::Cs(ComputeIo {
                    rsrc2: 2 << 1,
                    threads: [32, 1, 1],
                }),
                code: wf_words.iter().flat_map(|w| w.to_le_bytes()).collect(),
            };
            for (label, mask) in configs {
                let t0 = std::time::Instant::now();
                let lowered = with_shader_opt(mask, || {
                    let p = ir::build(&wf_native.code, wf_native.wave64, wf_native.vgprs).unwrap();
                    let st = structure::structure(&p).unwrap();
                    let res =
                        resource::track(&p, &wf_native.layout, wf_native.stage, Some(&st)).unwrap();
                    let io = io::raise(&p, &wf_native).unwrap();
                    lower::lower(&lower::Parts {
                        native: &wf_native,
                        p: &p,
                        st: &st,
                        io: &io,
                        res: &res,
                        inputs: Vec::new(),
                        outputs: Vec::new(),
                        prims: Vec::new(),
                        mesh_stride: 0,
                    })
                    .unwrap()
                });
                let compile_t = t0.elapsed();
                let insts = crate::emit::census::take(&lowered.dxil).map(|c| c.counts[0]);
                validate_dxil(&lowered.dxil);
                let wf_shader = HostShader {
                    stage: HwStage::Cs,
                    dxil: lowered.dxil,
                    user_data: lowered.user_data,
                    samplers: lowered.samplers,
                    static_samplers: Vec::new(),
                    root_cbvs: lowered.root_cbvs,
                    root_srvs: lowered.root_srvs,
                    root_uavs: lowered.root_uavs,
                    root_indexed_views: Vec::new(),
                };
                let (_, gpu_us) =
                    dispatch_timed(&host, &wf_shader, &[0; 4], 32, 2, [4096, 1, 1], 5, 25).unwrap();
                println!(
                    "{label:<32} | insts={insts:?} | dxil={:5} B | translate={:7.1} us | gpu_median={:8.2} us ({:.3} ms)",
                    wf_shader.dxil.len(),
                    compile_t.as_secs_f64() * 1e6,
                    gpu_us,
                    gpu_us / 1e3,
                );
            }
        }
    }
}
