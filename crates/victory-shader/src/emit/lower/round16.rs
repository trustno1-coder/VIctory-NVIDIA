use crate::gfx10::{Fields, Operand};
use crate::ir::{Inst, Program, reverse_postorder};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// No MODE write: retain the fork's existing adapter-dependent conversion.
    Default,
    Explicit(u8),
    Unknown,
}

pub(super) fn after(inst: &Inst, mode: Mode) -> Mode {
    match (inst.name(), &inst.fields) {
        ("s_round_mode", Fields::Sopp { imm }) => Mode::Explicit((imm >> 2 & 3) as u8),
        (
            "s_setreg_imm32_b32" | "s_setreg_b32",
            Fields::Sop {
                src, imm: Some(h), ..
            },
        ) => {
            let (id, off, size) = (h & 0x3f, h >> 6 & 0x1f, (h >> 11 & 0x1f) + 1);
            if id != 1 || off >= 4 || off + size <= 2 {
                return mode;
            }
            match src.first() {
                Some(&Operand::Literal(x)) if off <= 2 && off + size >= 4 => {
                    Mode::Explicit((x >> (2 - off) & 3) as u8)
                }
                _ => Mode::Unknown,
            }
        }
        _ => mode,
    }
}

pub(super) fn block_entries(p: &Program) -> Option<Vec<Mode>> {
    if !p
        .blocks
        .iter()
        .flat_map(|block| &block.insts)
        .any(|inst| after(inst, Mode::Default) != Mode::Default)
    {
        return None;
    }
    let mut entries = vec![None; p.blocks.len()];
    entries[0] = Some(Mode::Default);
    let order = reverse_postorder(&p.blocks);
    loop {
        let mut changed = false;
        for &b in &order {
            let Some(mode) = entries[b] else { continue };
            let mode = p.blocks[b]
                .insts
                .iter()
                .fold(mode, |mode, inst| after(inst, mode));
            for s in p.blocks[b].exit.successors() {
                let next = match entries[s] {
                    None => mode,
                    Some(old) if old == mode => continue,
                    Some(_) => Mode::Unknown,
                };
                if entries[s] != Some(next) {
                    entries[s] = Some(next);
                    changed = true;
                }
            }
        }
        if !changed {
            return Some(
                entries
                    .into_iter()
                    .map(|mode| mode.unwrap_or(Mode::Unknown))
                    .collect(),
            );
        }
    }
}
