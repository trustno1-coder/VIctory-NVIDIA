//! The pool hooks' places in game_scarlett_master.exe 1.0.2.1 (made by a script from the
//! executable; the analysis is in the module docs of `victory-gpu`'s `pool_hooks`).
//!
//! What is kept here is where each place is, how long the window compared there is, and what its
//! bytes hash to (BLAKE3-64: the low eight bytes of the BLAKE3-256 digest, little-endian), plus
//! the instruction recipes of the spans. The title's code is not in this file: `apply_pools`
//! reads the bytes from the mapped image and patches a place only where they hash to what is
//! recorded here, so another build of the title is refused. (The pool hooks themselves, which
//! would use these places, are not part of this fork yet — see `heap_hooks::hooked_heap`.)

use super::{Insn, Note, Site, Tls};

/// The copy into a fresh slot of a general pool (`call memcpy` at +0x4f3f877 in +0x4f3f7b0): the
/// window's place in the image, its length and BLAKE3-64, the TLS-array loads in it, and the
/// call's offset in the window.
pub(super) const COPY: (usize, u16, u64, &[Tls], usize) = (0x4f3f871, 15, 0xb3b83fa86945d529, &[], 6);

/// The free lists' links into a released slot: `mov [r14], ebp` in the general pools' flush,
/// `mov [rbx], ebp` in the special pool's, each with the `lea` after it.
#[rustfmt::skip]
pub(super) const LINKS: &[Site] = &[
    Site { name: "pool link (general)", window: 0x5244e6d, len: 17, digest: 0xe0c9cbbba29945e4, tls: &[], span: 0, insns: &[Insn::Store(3, Note::Link), Insn::Plain(8)], flags_live: true },
    Site { name: "pool link (special)", window: 0x5244f39, len: 16, digest: 0xca05c2522c67cc0e, tls: &[], span: 0, insns: &[Insn::Store(2, Note::Link), Insn::Plain(8)], flags_live: true },
];

/// The special pool's fresh slot filled with its template (+0x4f3fc13, +0x4f3fc20 in
/// +0x4f3fad0; the load between moves); an `add rsp` follows.
#[rustfmt::skip]
pub(super) const SPECIAL: &[Site] = &[
    Site { name: "special pool slot from its template", window: 0x4f3fc13, len: 21, digest: 0x821ad60555fe70d0, tls: &[], span: 0, insns: &[Insn::Store(5, Note::Record), Insn::Relative(8, 4), Insn::Store(4, Note::Record)], flags_live: false },
];

/// The switch in +0x4f3f640 that writes a property's new slot index into the special pool's fresh
/// slot (`mov [rsi+d], ax`, then back to +0x4f3f6ee): its table of 24 offsets from its own start
/// (`lea r14, [6758E10h]`; .rdata), and each case. The cases are too short for a jump: the
/// table's entries are made to point at the stubs, which go back to the case's own jump after
/// the store.
///
/// The table's entries are the title's own data: what is kept here is where they are, how many
/// bytes of them are compared, and their BLAKE3-64. The patched entries point at our stubs, so
/// their original values are read from the image and never written down here.
pub(super) const TABLE: (usize, u16, u64) = (0x6758e10, 96, 0x02d1f206fb786d9c);

/// The switch's cases, by index: each the window compared at its place and the store hooked in
/// it. Their jump and `mov` forms are what `heap_sites` recognizes; the tag is what says the
/// title at that place is this one.
#[rustfmt::skip]
pub(super) const CASES: &[(usize, Site)] = &[
    (0, Site { name: "special pool slot index 0", window: 0x4f3f707, len: 5, digest: 0x8f78294997ce2f08, tls: &[], span: 0, insns: &[Insn::Store(3, Note::Record)], flags_live: false }),
    (1, Site { name: "special pool slot index 1", window: 0x4f3f76e, len: 9, digest: 0x8df940216f33a7e0, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (2, Site { name: "special pool slot index 2", window: 0x4f3f780, len: 9, digest: 0x8f1c7efbb4084204, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (3, Site { name: "special pool slot index 3", window: 0x4f3f762, len: 6, digest: 0x0f6d2467df1ce499, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (4, Site { name: "special pool slot index 4", window: 0x4f3f738, len: 6, digest: 0xc540a2e9a9629574, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (5, Site { name: "special pool slot index 5", window: 0x4f3f712, len: 6, digest: 0x67f5b2596ad14fa2, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (6, Site { name: "special pool slot index 6", window: 0x4f3f71e, len: 6, digest: 0xad07227f6bc81410, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (7, Site { name: "special pool slot index 7", window: 0x4f3f750, len: 6, digest: 0xc5fb7c32d643b316, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (8, Site { name: "special pool slot index 8", window: 0x4f3f75c, len: 6, digest: 0xa83a747460bac29d, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (9, Site { name: "special pool slot index 9", window: 0x4f3f6ea, len: 4, digest: 0xbd713ecb8b53f220, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (10, Site { name: "special pool slot index 10", window: 0x4f3f701, len: 6, digest: 0xec3d791f1fc4b82b, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (11, Site { name: "special pool slot index 11", window: 0x4f3f744, len: 6, digest: 0x33f55b8dbf27709d, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (12, Site { name: "special pool slot index 12", window: 0x4f3f718, len: 6, digest: 0x99ab115518427f8e, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (13, Site { name: "special pool slot index 13", window: 0x4f3f7a4, len: 9, digest: 0xb1cf95fb11a02189, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (14, Site { name: "special pool slot index 14", window: 0x4f3f777, len: 9, digest: 0x629fcccfaecf32d4, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (15, Site { name: "special pool slot index 15", window: 0x4f3f792, len: 9, digest: 0x03dfea028e25d300, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (16, Site { name: "special pool slot index 16", window: 0x4f3f756, len: 6, digest: 0xb6f163af8d8d433d, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (17, Site { name: "special pool slot index 17", window: 0x4f3f768, len: 6, digest: 0xddef8f15da642f69, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (18, Site { name: "special pool slot index 18", window: 0x4f3f6fb, len: 6, digest: 0x8e4330c46edd94b1, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (19, Site { name: "special pool slot index 19", window: 0x4f3f789, len: 9, digest: 0x6c3a97a31a25559c, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (20, Site { name: "special pool slot index 20", window: 0x4f3f73e, len: 6, digest: 0x3a2deb89fd3341a7, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (21, Site { name: "special pool slot index 21", window: 0x4f3f70c, len: 6, digest: 0x1c1d7fbb443f285c, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (22, Site { name: "special pool slot index 22", window: 0x4f3f79b, len: 9, digest: 0x4acc6ebaa1351361, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
    (23, Site { name: "special pool slot index 23", window: 0x4f3f74a, len: 6, digest: 0xfa0925d940f04974, tls: &[], span: 0, insns: &[Insn::Store(4, Note::Record)], flags_live: false }),
];
