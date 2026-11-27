//! Guest-wave state across lane-divergent branches.

use super::*;

/// Joins of a compare's mask past a per-lane branch ([`Self::skipped`]): the per-lane branches'
/// regions by branch block ([`lane_regions`]), and where each value is written.
pub(super) struct CompareJoins {
    regions: HashMap<usize, Vec<bool>>,
    defs: HashMap<Value, (usize, Def)>,
}

/// How a value is written: by the join of that index in its block, by a compare (`v_cmp_*`;
/// not `v_cmpx_*`, which writes EXEC) or otherwise.
#[derive(Clone, Copy)]
enum Def {
    Join(usize),
    Compare,
    Other,
}

impl CompareJoins {
    /// None with an indirect exit.
    pub(super) fn new(p: &Program, st: &Structured) -> Option<Self> {
        let regions = lane_regions(st)?.into_iter().collect();
        let mut defs = HashMap::new();
        for (b, block) in p.blocks.iter().enumerate() {
            for (i, phi) in block.phis.iter().enumerate() {
                defs.insert(phi.value, (b, Def::Join(i)));
            }
            for inst in &block.insts {
                let name = inst.name();
                let def = if name.starts_with("v_cmp_") && !name.starts_with("v_cmpx_") {
                    Def::Compare
                } else {
                    Def::Other
                };
                for &(_, v) in &inst.writes {
                    defs.insert(v, (b, def));
                }
            }
        }
        Some(CompareJoins { regions, defs })
    }

    /// A compare inside a per-lane branch whose mask a join after the branch's region reads:
    /// the blocks of that region through which the compare's mask reaches the phi at `(b, i)`,
    /// while the branch's other lanes bring their old mask past the region. On the console the
    /// wave runs the region when any lane does, and a compare (`v_cmp_*` writing VCC or an SGPR
    /// pair) writes the whole mask with 0 for the lanes EXEC leaves out (RDNA2 ISA, VOPC), so
    /// after the join every lane has the region's mask: its own bit 0 where it skipped, the
    /// region's number as data. Per lane, a lane that skipped keeps the old mask. Taking those
    /// branches wave-wide ([`branches_around`], `emit`'s retry) is what the console does.
    /// `skip_bits`, for a join read as lane bits: each edge's incoming bit
    /// ([`Structured::phi_masks`]); an edge whose lanes all bring 0 is right per lane. Empty
    /// where per lane is right. Found by the RDNA2 oracle (`oracle_tests`).
    pub(super) fn skipped(&self, p: &Program, b: usize, i: usize, skip_bits: Option<&[Bool]>) -> Vec<usize> {
        let block = &p.blocks[b];
        let phi = &block.phis[i];
        let mut out = Vec::new();
        for (n, &q) in block.preds.iter().enumerate() {
            if skip_bits.is_some_and(|bits| bits.get(n) == Some(&Bool::FALSE)) {
                continue;
            }
            // (an edge from a per-lane branch past its region, not into it)
            let Some(region) = self.regions.get(&q).filter(|region| !region[b]) else {
                continue;
            };
            for (m, &r) in block.preds.iter().enumerate() {
                if region[r]
                    && phi.args[m] != phi.args[n]
                    && self.compare_in(p, region, phi.args[m], &mut HashSet::new())
                    && !out.contains(&r)
                {
                    out.push(r);
                }
            }
        }
        out
    }

    /// Whether `v` is a compare's mask written in `region`, or a join in `region` of one.
    fn compare_in(
        &self,
        p: &Program,
        region: &[bool],
        v: Value,
        seen: &mut HashSet<Value>,
    ) -> bool {
        if !seen.insert(v) {
            return false;
        }
        match self.defs.get(&v) {
            Some(&(b, Def::Compare)) => region[b],
            Some(&(b, Def::Join(i))) if region[b] => p.blocks[b].phis[i]
                .args
                .iter()
                .any(|&a| self.compare_in(p, region, a, seen)),
            _ => false,
        }
    }
}

/// The per-lane branches (by block) around a scalar the console's wave writes once for every
/// lane: a scalar instruction (or `v_readfirstlane`, `v_readlane`, an SCC) in the region of a
/// per-lane branch whose result a join after the region mixes with another value that is
/// read. The console runs such a region when any lane needs it (scalar instructions ignore
/// EXEC), so after the join every lane has the region's value; lowered per lane, only the
/// lanes that ran the region would. Taking those branches wave-wide
/// ([`crate::structure::structure_wide`]) is what the console does. A copy (`s_mov`) of a
/// value from before the region counts as that value. Branches whose wave-wide meaning is
/// not known ([`Structured::wave_any`]) are left out. None with an indirect exit.
/// Found by the RDNA2 oracle (`oracle_tests`, kind `salu_in_branch`).
pub(in crate::emit) fn scalar_splits(p: &Program, st: &Structured) -> Option<Vec<usize>> {
    let scalar = |reg: Reg, v: Value| match reg {
        Reg::Scc => true,
        Reg::S(code) => {
            code != EXEC && code != EXEC + 1 && !st.masks.contains_key(&v) && !st.highs.contains_key(&v)
        }
        Reg::V(_) => false,
    };
    let mut copy: HashMap<Value, Value> = HashMap::new();
    let mut read: HashSet<Value> = HashSet::new();
    for block in &p.blocks {
        for phi in &block.phis {
            read.extend(phi.args.iter().copied());
        }
        for inst in &block.insts {
            read.extend(inst.reads.iter().map(|r| r.1));
            if matches!(inst.name(), "s_mov_b32" | "s_mov_b64") {
                let from: Vec<Value> = inst.reads.iter().filter(|r| matches!(r.0, Reg::S(_))).map(|r| r.1).collect();
                let to: Vec<Value> = inst.writes.iter().filter(|w| matches!(w.0, Reg::S(_))).map(|w| w.1).collect();
                if from.len() == to.len() {
                    for (t, f) in to.into_iter().zip(from) {
                        copy.insert(t, f);
                    }
                }
            }
        }
    }
    let origin = |mut v: Value| {
        for _ in 0..64 {
            match copy.get(&v) {
                Some(&u) => v = u,
                None => break,
            }
        }
        v
    };
    let mut out = Vec::new();
    for (d, region) in lane_regions(st)? {
        if st.wave_any.get(d).copied().flatten().is_none() {
            continue;
        }
        let mixes = p.blocks.iter().enumerate().filter(|&(j, _)| !region[j]).any(|(_, block)| {
            block.phis.iter().any(|phi| {
                if !scalar(phi.reg, phi.value) || !read.contains(&phi.value) {
                    return false;
                }
                // Which value a lane gets depends on its way through the region: the join is
                // reached through it, and its values (copies seen through) are not one.
                let first = phi.args.first().map(|&a| origin(a));
                let differ = phi.args.iter().any(|&a| Some(origin(a)) != first);
                let through = block.preds.iter().any(|&q| q == d || region.get(q).copied().unwrap_or(false));
                differ && through
            })
        });
        // WRITELANE ignores EXEC, including for its selected destination lane.
        // Execute the containing branch wave-wide so that an inactive target lane
        // is present. Keep the branch's wave-any test: an empty branch does not run.
        let writes_lane = p.blocks.iter().enumerate().any(|(b, block)| {
            region[b] && block.insts.iter().any(|inst| inst.name() == "v_writelane_b32")
        });
        if mixes || writes_lane {
            out.push(d);
        }
    }
    Some(out)
}
