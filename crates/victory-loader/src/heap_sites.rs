//! The title's stores into its shader-visible view heap, hooked
//! (`VICTORY_HEAP_HOOKS=1`; `victory-gpu`'s `heap_hooks` holds the switch, the
//! journal and the guard).
//!
//! The heap's memory stays read-only, as a guard that names any writer the
//! hooks miss. Each store of [`sites`] becomes a jump to code of ours that
//! writes the same bytes through our own view of the same pages, then notes
//! the 32-byte slots it wrote in a journal: the heap's sync reads those slots
//! and no others. What a store notes ([`Note`]): a record written; other
//! bytes (the frame allocator's own lists, which share its memory); or a
//! slot handed back to the title's pool of persistent slots, whose first
//! word is then the pool's link and no record.
//!
//! The cells the code reads ([`CELLS_LEN`] bytes, `#[repr(C)]` in
//! `victory-gpu`): the heap's first byte (64 KiB aligned) and its length in
//! bytes (0: nothing is hooked, every store is done as the title does it);
//! per 64 KiB block of the heap, what to add to a title address for ours (0:
//! not known yet, asked for by a call into `victory-gpu`; 1: there is none,
//! the store goes to the title's address); a byte per slot, the note; a mark
//! per block for the sync, and one for the write watch (the heap shares
//! memory with a buffer of the title's whose copy the watch keeps), each in a
//! cache line of its own ([`LINE`] bytes); a byte that says some block was
//! marked for the watch, written only when a block's mark was not set (every
//! recording thread writing one byte would pass its line around at every
//! store).
//!
//! Every site's window is compared before any is written. A table row keeps the
//! place, the length and the BLAKE3-64 of what was there when it was made — not
//! the title's code: [`Site::window`] reads the bytes from the mapped image and
//! gives them back only when they hash to the recorded ones. If one window
//! differs (another build of the title), none is written, and the log says which.
//!
//! What a row hashes is the image as it lies before our writes reach it. The title's own
//! `tls::redirect` is the one place where the running image is not the file's: it makes every
//! TLS-array load a call of its thunk, and that call's displacement is where the page it lives
//! on landed. So a row holds no bytes such a call would change: it names the load in `tls`,
//! which takes its nine whole bytes, or its window stops before the load begins. The one row
//! of `OWN` whose window ran into a load (+0x325bbf3) does the latter, and
//! `no_row_holds_bytes_the_tls_redirect_changes` holds the tables to it.

/// Bytes of the cells the stubs read.
pub const CELLS_LEN: usize = 56;

/// Bytes between two blocks' marks.
pub const LINE: usize = 64;

/// What a hooked store says of the slots it wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    /// A descriptor record (or bytes that may be one).
    Record = 1,
    /// Bytes that are no record: the frame allocator's own lists.
    Other = 2,
    /// The slot went back to the title's pool: its first word is a link.
    Freed = 3,
    /// A link of a pool's free list into a slot just released (the pool
    /// hooks): the store is a call to `victory-gpu`, which checks the slot is
    /// free and stores through our view; nothing is noted.
    Link = 4,
}

/// An instruction of a patched span: left as it is (`Plain`, which must not
/// depend on where it is), one whose 4 bytes at `.1` are a displacement from
/// its end (`Relative`, moved to point where it pointed), or a store that is
/// hooked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Insn {
    Plain(usize),
    Relative(usize, usize),
    Store(usize, Note),
}

impl Insn {
    fn len(self) -> usize {
        match self {
            Insn::Plain(n) | Insn::Relative(n, _) | Insn::Store(n, _) => n,
        }
    }
}

/// One of the title's TLS-array loads in a window: its offset in the window and
/// the register it loads into. `crate::tls` makes such a load a call of its
/// own thunk; these two numbers say what the title had there, so a window is
/// still recognized when the image shows the call, without the title's bytes
/// kept in the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tls {
    pub at: u16,
    pub reg: u8,
}

/// The title's load of its thread's TLS array into `reg` (`mov r, gs:[58h]`),
/// made from the register alone: an instruction of the kind every such image
/// has, which [`tls_load`] recognizes. Not a place in the title's code, so it
/// is ours to write down.
fn tls_load_insn(reg: u8) -> [u8; 9] {
    [
        0x65,
        if reg >= 8 { 0x4c } else { 0x48 },
        0x8b,
        0x04 | (reg & 7) << 3,
        0x25,
        0x58,
        0,
        0,
        0,
    ]
}

/// A place in the title's code: the window compared (its offset in the image,
/// its length and the BLAKE3-64 of its bytes), the span in it that is replaced
/// (from `span`, the lengths of `insns`), and whether the flags the span leaves
/// are read after it (then the stub keeps them). The title's bytes are not in
/// the table: [`Site::window`] reads them from the mapped image and gives them
/// back only when they are the recorded ones.
#[derive(Debug, Clone, Copy)]
pub struct Site {
    pub name: &'static str,
    pub window: usize,
    pub len: u16,
    pub digest: u64,
    pub tls: &'static [Tls],
    pub span: usize,
    pub insns: &'static [Insn],
    pub flags_live: bool,
}

impl Site {
    fn span_len(&self) -> usize {
        self.insns.iter().map(|i| i.len()).sum()
    }

    /// What a window is in these tables: the low eight bytes of its
    /// BLAKE3-256 digest, little-endian. Long enough to tell one build of the
    /// title from another, which is all a table is asked to tell.
    pub fn tag(bytes: &[u8]) -> u64 {
        let digest = blake3::hash(bytes);
        u64::from_le_bytes(digest.as_bytes()[..8].try_into().unwrap())
    }

    /// The bytes the row stands for, taken from `there` (the image's bytes at
    /// the site's window, as many as the row's `len`). None when the image
    /// shows anything else — a build of the title this table was not made for,
    /// of which nothing is patched.
    ///
    /// A load of the title's TLS array is the one place where the image may
    /// differ and still be the title's, because `crate::tls` turns those into
    /// calls of its thunk wherever it finds them. Such a call is put back as
    /// the load the row names, so what comes out here are the title's own
    /// bytes and the stubs are made of what they were made of before.
    pub fn window(&self, there: &[u8]) -> Option<Vec<u8>> {
        recorded(self.len, self.digest, self.tls, there)
    }
}

/// The bytes a table row records, read from `there`: the image's bytes at the row's window,
/// as many as its `len`. None when the image shows anything else. [`Site::window`]'s rules,
/// given the row's parts one by one, for the pool hooks' places — which are windows of their
/// own and not sites.
fn recorded(len: u16, digest: u64, tls: &[Tls], there: &[u8]) -> Option<Vec<u8>> {
    if there.len() != usize::from(len) {
        return None;
    }
    let mut bytes = there.to_vec();
    for t in tls {
        let (at, load) = (usize::from(t.at), tls_load_insn(t.reg));
        match bytes.get(at..at + load.len()) {
            Some(here) if here == load => {}
            Some(here) if tls_call(here, 0).is_some() => {
                bytes[at..at + load.len()].copy_from_slice(&load);
            }
            _ => return None,
        }
    }
    (Site::tag(&bytes) == digest).then_some(bytes)
}

const RECORD5: &[Insn] = &[Insn::Store(5, Note::Record)];
const RECORD6: &[Insn] = &[Insn::Store(6, Note::Record)];

/// The stores of game_scarlett_master.exe 1.0.2.1 (image offsets) that write
/// its view heap, as far as the guard has named them.
///
/// - The six record stores of the table fillers (+0x4d9efd0 graphics,
///   +0x4eec8e0 compute), as renderer port #2 has them (`descriptor_sites`):
///   `vmovups [table + slot], ymm0`, flags set again by the `cmp` after.
/// - +0x18849c and its copy +0x2474afc: a 24-byte node the frame allocator
///   (+0x4d9f400) just handed out is pushed on the list of the allocator's
///   page it lies in (`mov rdx, [header + 48h]; mov [node], rdx`). The node
///   lies in the heap's memory, as everything the frame allocator hands out
///   does; it is no record. The load before the store is part of the span.
/// - +0x4fb05b8: a slot of the pool of persistent slots (the first megabytes
///   of the heap, where views made by calls live) goes back to the pool: its
///   first word becomes the index of the next free one (`mov [slot], edx`),
///   then a `lock cmpxchg` makes it the head. The `mov rax, rdx` after is part
///   of the span; the `and` after that sets the flags.
/// - +0x4e9b4e2: a 24-byte node `{0, object, value}` of the frame
///   allocator's, in two spans: the first two stores (a call follows, after
///   which no flag is read), then the `add` and the third store (a jump
///   follows: the flags are kept).
/// - The writers the lane book saw open the heap's blocks next (four records
///   at once in the graphics table filler; objects, matrices and entries the
///   frame allocator's users fill): every store into the same object in a
///   span, each said at its site in [`OWN`].
pub fn sites() -> Vec<Site> {
    let mut all: Vec<Site> = crate::descriptor_sites::SITES
        .iter()
        .enumerate()
        .map(|(n, s)| Site {
            name: [
                "records, graphics 1",
                "records, graphics 2",
                "records, graphics 3",
                "records, compute 1",
                "records, compute 2",
                "records, compute 3",
            ][n],
            window: s.window,
            len: s.len,
            digest: s.digest,
            tls: s.tls,
            span: s.store,
            insns: if s.store_len == 5 { RECORD5 } else { RECORD6 },
            flags_live: false,
        })
        .collect();
    all.extend(OWN);
    all.extend(frame_users::sites());
    all
}

/// The stores of the frame allocator's users, made by a generator from the
/// title's code (see the file).
mod frame_users;

/// The pool hooks' places, made by a script from the title's code.
mod pool_users;

/// The sites [`sites`] adds to renderer port #2's.
const OWN: [Site; 31] = [
    // The frame allocator's node functions (+0x188430 and its copy +0x2474a90,
    // with their callees): every store into the node, the objects it names
    // and the page headers, hooked from what each register holds, not from
    // what faulted. A store that lands outside the heap is done as before.
    // +0x188493..: the node's page header noted in the caller's object, the node pushed on the header's list, the header's head set (`[rdi+10h]`, `[rbx]`, `[rcx+rax+48h]`); a `shl` sets the flags after.
    Site {
        name: "frame allocator node pushed on its page's list",
        window: 0x18848f,
        len: 33, digest: 0x3968367a553edc70, tls: &[],
        span: 4,
        insns: &[
            Insn::Store(4, Note::Other),
            Insn::Plain(5),
            Insn::Store(3, Note::Other),
            Insn::Store(5, Note::Other),
        ],
        flags_live: false,
    },
    // +0x2474af3..: the same in the copy of the function (allocator +0x4e32440).
    Site {
        name: "frame allocator node pushed on its page's list (copy)",
        window: 0x2474aef,
        len: 33, digest: 0x3968367a553edc70, tls: &[],
        span: 4,
        insns: &[
            Insn::Store(4, Note::Other),
            Insn::Plain(5),
            Insn::Store(3, Note::Other),
            Insn::Store(5, Note::Other),
        ],
        flags_live: false,
    },
    // +0x188504: the node's second word (`[rbx+8]`), after a virtual call returns; a call follows.
    Site {
        name: "frame allocator node: its object",
        window: 0x1884fe,
        len: 17, digest: 0xfe98c4f1cd697e6a, tls: &[],
        span: 3,
        insns: &[Insn::Plain(3), Insn::Store(4, Note::Other)],
        flags_live: false,
    },
    // +0x2474b64: the same in the copy.
    Site {
        name: "frame allocator node: its object (copy)",
        window: 0x2474b5e,
        len: 17, digest: 0x00f0eb99a8bee67c, tls: &[],
        span: 3,
        insns: &[Insn::Plain(3), Insn::Store(4, Note::Other)],
        flags_live: false,
    },
    // +0x18850d: the node's third word and the object noted in the caller's (`[rbx+10h]`, `[rdi+8]`), where the call returns; the span ends at a branch target.
    Site {
        name: "frame allocator node: its value",
        window: 0x18850d,
        len: 7, digest: 0x577fd2a4ebb25fba, tls: &[],
        span: 0,
        insns: &[Insn::Store(3, Note::Other), Insn::Store(4, Note::Other)],
        flags_live: false,
    },
    // +0x2474b6d: the same in the copy.
    Site {
        name: "frame allocator node: its value (copy)",
        window: 0x2474b6d,
        len: 7, digest: 0x577fd2a4ebb25fba, tls: &[],
        span: 0,
        insns: &[Insn::Store(3, Note::Other), Insn::Store(4, Note::Other)],
        flags_live: false,
    },
    // +0x188549: what +0x2474be0 or +0x2474c50 returned, noted in the caller's object (`[rdi+50h]`, `[rdi+48h]`), from the branch target where both meet; a call follows.
    Site {
        name: "frame allocator user noted",
        window: 0x18853e,
        len: 21, digest: 0x4fa3e2d367296710, tls: &[],
        span: 5,
        insns: &[
            Insn::Plain(3),
            Insn::Plain(3),
            Insn::Store(4, Note::Other),
            Insn::Store(4, Note::Other),
        ],
        flags_live: false,
    },
    // +0x2474ba9: the same in the copy.
    Site {
        name: "frame allocator user noted (copy)",
        window: 0x2474b9e,
        len: 21, digest: 0xcc2fc61c5f31d946, tls: &[],
        span: 5,
        insns: &[
            Insn::Plain(3),
            Insn::Plain(3),
            Insn::Store(4, Note::Other),
            Insn::Store(4, Note::Other),
        ],
        flags_live: false,
    },
    // +0x18855b: the record +0x4f202c0 returned, filled (`[rax]` to `[rax+28h]`); an `add rsp` follows.
    Site {
        name: "frame allocator record filled",
        window: 0x188554,
        len: 29, digest: 0x332512198c13c683, tls: &[],
        span: 5,
        insns: &[
            Insn::Plain(2),
            Insn::Store(2, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Store(3, Note::Other),
        ],
        flags_live: false,
    },
    // +0x2474bbb: the same in the copy (+0x52234f0).
    Site {
        name: "frame allocator record filled (copy)",
        window: 0x2474bb4,
        len: 29, digest: 0x0beb99769d1c7117, tls: &[],
        span: 5,
        insns: &[
            Insn::Plain(2),
            Insn::Store(2, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Store(3, Note::Other),
        ],
        flags_live: false,
    },
    // +0x2474a48: a loop that links the allocator's objects into a list and fills them (`[r8+8]`, `[rdi]` .. `[rdi+28h]`), from its top; a `test` follows.
    Site {
        name: "frame allocator objects linked",
        window: 0x2474a42,
        len: 34, digest: 0x6827685c1c7c0bb1, tls: &[],
        span: 0,
        insns: &[
            Insn::Plain(3),
            Insn::Plain(3),
            Insn::Store(4, Note::Other),
            Insn::Store(3, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Store(3, Note::Other),
            Insn::Store(3, Note::Other),
        ],
        flags_live: false,
    },
    // +0x2474a69: the list's last object pointed at its global head (`[rdx+8]`; the head itself is a global); an `add rsp` follows.
    Site {
        name: "frame allocator objects' list ended",
        window: 0x2474a62,
        len: 29, digest: 0x37a0f0ca42b45663, tls: &[],
        span: 0,
        insns: &[
            Insn::Relative(7, 3),
            Insn::Store(4, Note::Other),
            Insn::Relative(7, 3),
        ],
        flags_live: false,
    },
    // +0x2474c8e: +0x2474c50's next index (`[rdi+20h]`); an `add rsp` follows.
    Site {
        name: "frame allocator user's index",
        window: 0x2474c88,
        len: 9, digest: 0xee48ee3ac3926b32, tls: &[],
        span: 3,
        insns: &[Insn::Plain(3), Insn::Store(3, Note::Other)],
        flags_live: false,
    },
    // +0x4e9b4d2: the page's head written out (`[r14]`) and the `test` after.
    Site {
        name: "frame allocator page's head handed out",
        window: 0x4e9b4cb,
        len: 18, digest: 0x6b801a11a0b92580, tls: &[],
        span: 7,
        insns: &[Insn::Store(3, Note::Other), Insn::Plain(3)],
        flags_live: false,
    },
    // +0x4e9b4ff: the node (or none) noted in the page (`[rdi+48h]`) and the size left (`[rsi]`), from the branch target; the span ends at the next one.
    Site {
        name: "frame allocator page's node noted",
        window: 0x4e9b4fa,
        len: 18, digest: 0x767d5849a6c28ce9, tls: &[],
        span: 5,
        insns: &[Insn::Store(4, Note::Other), Insn::Store(3, Note::Other)],
        flags_live: false,
    },
    // +0x4e9b585: the count of the object a node names (`[rsi+0Ch]`), where a call returns; a `shl` sets the flags after.
    Site {
        name: "frame allocator object's count",
        window: 0x4e9b580,
        len: 18, digest: 0x6e701cbb1a07d9e4, tls: &[],
        span: 5,
        insns: &[Insn::Store(3, Note::Other), Insn::Plain(3)],
        flags_live: false,
    },
    Site {
        name: "persistent slot given back to its pool",
        window: 0x4fb05b5,
        len: 13, digest: 0x47803eed5cda4493, tls: &[],
        span: 3,
        insns: &[Insn::Store(2, Note::Freed), Insn::Plain(3)],
        flags_live: false,
    },
    Site {
        name: "frame allocator node filled, first two words",
        window: 0x4e9b4df,
        len: 21, digest: 0x5f1caf01e7f4926f, tls: &[],
        span: 3,
        insns: &[Insn::Store(7, Note::Other), Insn::Store(4, Note::Other)],
        flags_live: false,
    },
    Site {
        name: "frame allocator node filled, third word",
        window: 0x4e9b4f2,
        len: 10, digest: 0x249c6ee6a4c7dfc6, tls: &[],
        span: 0,
        insns: &[Insn::Plain(4), Insn::Store(4, Note::Other)],
        flags_live: true,
    },
    // +0x4d9f25f..+0x4d9f26e: four records of a table of 128 bytes, copied at
    // once by the graphics table filler (`vmovups [rbx + 60h, 40h, 20h, 0], ymm3..0`);
    // the movs after it are followed by an `xor`.
    Site {
        name: "records, graphics, four at once",
        window: 0x4d9f258,
        len: 37, digest: 0xade59691b91ea467, tls: &[],
        span: 7,
        insns: &[
            Insn::Store(5, Note::Record),
            Insn::Store(5, Note::Record),
            Insn::Store(5, Note::Record),
            Insn::Store(4, Note::Record),
        ],
        flags_live: false,
    },
    // +0x1228f1: an object the call before handed out, filled with constants
    // and floats (two loads relative to the instruction are moved with the
    // span), to its end at +60h.
    Site {
        name: "object filled after +0x555e570",
        window: 0x1228e4,
        len: 199, digest: 0x9eec84ed96a10080, tls: &[],
        span: 5,
        insns: &[
            Insn::Relative(8, 4),
            Insn::Store(4, Note::Other),
            Insn::Relative(8, 4),
            Insn::Store(5, Note::Other),
            Insn::Store(5, Note::Other),
            Insn::Plain(4),
            Insn::Store(5, Note::Other),
            Insn::Plain(5),
            Insn::Store(5, Note::Other),
            Insn::Plain(5),
            // +0x12291f on: the next entries of the object, of 1Ch bytes each.
            Insn::Store(5, Note::Other),
            Insn::Plain(5),
            Insn::Store(5, Note::Other),
            Insn::Relative(8, 4),
            Insn::Store(5, Note::Other),
            Insn::Store(5, Note::Other),
            Insn::Plain(4),
            Insn::Store(5, Note::Other),
            Insn::Plain(5),
            Insn::Store(5, Note::Other),
            Insn::Plain(5),
            Insn::Store(5, Note::Other),
            Insn::Plain(5),
            Insn::Store(5, Note::Other),
            Insn::Relative(8, 4),
            Insn::Store(5, Note::Other),
            Insn::Store(5, Note::Other),
            Insn::Plain(4),
            // To the object's end at +60h; a `cmp` follows.
            Insn::Store(5, Note::Other),
            Insn::Plain(5),
            Insn::Store(5, Note::Other),
            Insn::Plain(5),
            Insn::Store(5, Note::Other),
            Insn::Plain(5),
            Insn::Store(5, Note::Other),
        ],
        flags_live: false,
    },
    // +0x325bbf9: three rows of a matrix (`vmovaps [rsi + 0, 10h, 20h]`); a
    // jump follows, and after it the title's load of its TLS array at +0x325bc08, of which the
    // window keeps nothing: `tls::redirect` makes it a call of a thunk of ours.
    Site {
        name: "matrix rows",
        window: 0x325bbf3,
        len: 25, digest: 0xf4b4e569ad380912, tls: &[],
        span: 6,
        insns: &[
            Insn::Store(4, Note::Other),
            Insn::Store(5, Note::Other),
            Insn::Store(5, Note::Other),
        ],
        flags_live: true,
    },
    // +0x325bd6c: an entry of 12 bytes (`mov [rcx + r8*8 + 0, 4, 8]`); a
    // `shl` sets the flags after.
    Site {
        name: "entry of 12 bytes",
        window: 0x325bd69,
        len: 30, digest: 0x1169209db9d79132, tls: &[],
        span: 3,
        insns: &[
            Insn::Store(4, Note::Other),
            Insn::Store(5, Note::Other),
            Insn::Store(9, Note::Other),
        ],
        flags_live: false,
    },
    // +0x325be12: a loop's word (`mov [rax + r8*4], r9d`) and its `inc`; the
    // loop's `and` and `cmp` set the flags before its `jb`.
    Site {
        name: "words of a loop",
        window: 0x325be0c,
        len: 22, digest: 0x5b48da1c84deec80, tls: &[],
        span: 6,
        insns: &[Insn::Store(4, Note::Other), Insn::Plain(3)],
        flags_live: false,
    },
    // +0x325bef7: a loop's entry of 24 bytes (`mov [r9 + 0 .. 14h]`); an
    // `add` follows.
    Site {
        name: "entries of 24 bytes",
        window: 0x325bef5,
        len: 31, digest: 0xadd54ff805561dc7, tls: &[],
        span: 2,
        insns: &[
            Insn::Store(3, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Store(4, Note::Other),
        ],
        flags_live: false,
    },
    // +0x399ca0: four words of an entry (`mov [r15 + 0, 4, 20h, 24h]`); a
    // `test` follows the movs after.
    Site {
        name: "entry at r15",
        window: 0x399c96,
        len: 34, digest: 0x8d8ee598a60be55e, tls: &[],
        span: 10,
        insns: &[
            Insn::Store(3, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Store(4, Note::Other),
        ],
        flags_live: false,
    },
    // +0x5574df6: 96 bytes of a block the call before handed out, zeroed
    // (`vmovups [rax + 0, 20h, 40h], ymm0`); no flag is read before the
    // `imul` and `shr` after.
    Site {
        name: "block after +0x517a440 zeroed",
        window: 0x5574df1,
        len: 25, digest: 0xafbfddef190d0202, tls: &[],
        span: 5,
        insns: &[
            Insn::Store(4, Note::Other),
            Insn::Store(5, Note::Other),
            Insn::Store(5, Note::Other),
        ],
        flags_live: false,
    },
    // +0x1263ad: an object of 48h bytes taken from a run of them
    // (`rdx = head[rax]`, `head[rax] = rdx + 48h`), filled; the store of the
    // head is hooked too (it is no heap memory as far as known: then it is
    // done as before).
    Site {
        name: "object of 48h bytes, from +0x126380",
        window: 0x12639f,
        len: 83, digest: 0x769409dc808f2a0e, tls: &[],
        span: 14,
        insns: &[
            Insn::Store(3, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Plain(4),
            Insn::Store(7, Note::Other),
            Insn::Store(5, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Store(5, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Plain(3),
            Insn::Store(5, Note::Other),
            // The rest of the object (`vmovlps [rdx+40h]` last); a call follows.
            Insn::Store(5, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Store(5, Note::Other),
        ],
        flags_live: false,
    },
    // +0x13b3df3: the same kind of object, from +0x13b3dc0, and the store of
    // its run's head.
    Site {
        name: "object of 48h bytes, from +0x13b3dc0",
        window: 0x13b3def,
        len: 112, digest: 0x61119196c478e2ab, tls: &[],
        span: 4,
        insns: &[
            Insn::Store(4, Note::Other),
            Insn::Store(5, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Store(5, Note::Other),
            Insn::Store(5, Note::Other),
            Insn::Store(5, Note::Other),
            Insn::Store(5, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Store(5, Note::Other),
            // The rest of the object, the two loads of saved registers between
            // and the store of the run's head; an `add rsp` follows.
            Insn::Store(5, Note::Other),
            Insn::Plain(6),
            Insn::Plain(6),
            Insn::Store(4, Note::Other),
            Insn::Store(5, Note::Other),
            Insn::Store(5, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Store(5, Note::Other),
            Insn::Store(5, Note::Other),
        ],
        flags_live: false,
    },
    // +0x555e3b1: the pool of persistent slots made (+0x555e340, once, when
    // the heap is): each slot's first word the index of the next, all given
    // back. The span is the store and the `inc` after the loop's target; the
    // loop's `cmp` sets the flags.
    Site {
        name: "persistent slots chained into their pool",
        window: 0x555e3ad,
        len: 18, digest: 0xa30b2de104063922, tls: &[],
        span: 4,
        insns: &[Insn::Store(3, Note::Freed), Insn::Plain(3)],
        flags_live: false,
    },
    // +0x555e3c2: the last slot's word, the end of the chain (-1); an `add
    // rsp` follows.
    Site {
        name: "persistent slots' chain ended",
        window: 0x555e3c0,
        len: 14, digest: 0xaa666f8c4c4c1c1b, tls: &[],
        span: 2,
        insns: &[Insn::Store(6, Note::Freed)],
        flags_live: false,
    },
];

/// A store of a register or a constant to memory: where its parts are, so
/// that it can be made to another address.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Store<'a> {
    code: &'a [u8],
    /// The REX byte, or the first byte of a three-byte VEX prefix.
    rex: Option<usize>,
    vex3: Option<usize>,
    /// The ModRM byte; past the address (SIB and displacement); past the constant.
    modrm: usize,
    address_end: usize,
    /// The address's index and base registers are of the upper eight.
    x: bool,
    b: bool,
    /// Bytes written.
    size: usize,
    /// The general register stored, if it is one.
    source: Option<u8>,
}

/// The store at the start of `code`, of the forms the hooks take: `mov` of a
/// register or a constant (1 to 8 bytes), and the vector stores of 16 and 32
/// bytes (`movups`, `movaps`, `movdqu`, `movdqa`, `movss`, `movsd` and their
/// VEX forms). Not an address relative to the instruction, nor one of the
/// stack, nor a segment's.
fn decode(code: &[u8]) -> Option<Store<'_>> {
    let mut i = 0;
    let mut legacy = None;
    while let Some(&p @ (0x66 | 0xf2 | 0xf3)) = code.get(i) {
        if legacy.replace(p).is_some() {
            return None;
        }
        i += 1;
    }
    let (rex, vex3, x, b, size, source, modrm, constant);
    match *code.get(i)? {
        first @ (0xc4 | 0xc5) => {
            if legacy.is_some() {
                return None;
            }
            let three = first == 0xc4;
            let one = *code.get(i + 1)?;
            let last = if three { *code.get(i + 2)? } else { one };
            let map = if three { one & 0x1f } else { 1 };
            if last >> 3 & 15 != 15 {
                return None;
            }
            let op_at = i + if three { 3 } else { 2 };
            let (op, pp) = (*code.get(op_at)?, last & 3);
            // vmovups/upd/ss/sd, vmovlps/lpd, vmovaps/apd, vmovdqa/dqu,
            // vmovq, vextractps (with its constant), vmovd/vmovq from a
            // vector register (VEX.W: 8 bytes).
            let (n, imm) = match (map, op, pp) {
                (1, 0x7e, 1) => (if three && last & 0x80 != 0 { 8 } else { 4 }, 0),
                (1, 0x11, 2) | (3, 0x17, 1) => (4, usize::from(map == 3)),
                (1, 0x11, 3) | (1, 0x13, 0 | 1) | (1, 0xd6, 1) => (8, 0),
                (1, 0x11, _) | (1, 0x29, 0 | 1) | (1, 0x7f, 1 | 2) => (16 << (last >> 2 & 1), 0),
                _ => return None,
            };
            (rex, vex3) = (None, three.then_some(i));
            (x, b) = (three && one & 0x40 == 0, three && one & 0x20 == 0);
            (size, source, modrm, constant) = (n, None, op_at + 1, imm);
        }
        _ => {
            let mut prefix = 0u8;
            rex = (code.get(i)? & 0xf0 == 0x40).then_some(i);
            if rex.is_some() {
                prefix = code[i];
                i += 1;
            }
            vex3 = None;
            (x, b) = (prefix & 2 != 0, prefix & 1 != 0);
            let wide = prefix & 8 != 0;
            let reg = |at: usize| code.get(at).map(|m| m >> 3 & 7);
            let (n, register, at, imm) = match (*code.get(i)?, legacy) {
                (0x88, None) => (1, true, i + 1, 0),
                (0x89, None) => (if wide { 8 } else { 4 }, true, i + 1, 0),
                (0x89, Some(0x66)) => (2, true, i + 1, 0),
                (0xc6, None) if reg(i + 1)? == 0 => (1, false, i + 1, 1),
                (0xc7, None) if reg(i + 1)? == 0 => (if wide { 8 } else { 4 }, false, i + 1, 4),
                (0xc7, Some(0x66)) if reg(i + 1)? == 0 => (2, false, i + 1, 2),
                (0x0f, _) => match (*code.get(i + 1)?, legacy) {
                    (0x11 | 0x29, None | Some(0x66)) | (0x7f, Some(0x66 | 0xf3)) => {
                        (16, false, i + 2, 0)
                    }
                    (0x11, Some(0xf3)) => (4, false, i + 2, 0),
                    (0x11, Some(0xf2)) => (8, false, i + 2, 0),
                    _ => return None,
                },
                _ => return None,
            };
            let r = reg(at)?;
            // A byte register without a prefix: 4 to 7 are the high bytes of the first four.
            source = register.then(|| {
                if n == 1 && rex.is_none() && r >= 4 {
                    r - 4
                } else {
                    r | (prefix & 4) << 1
                }
            });
            (size, modrm, constant) = (n, at, imm);
        }
    }
    let m = *code.get(modrm)?;
    let (mode, rm) = (m >> 6, m & 7);
    let mut end = modrm + 1;
    if mode == 3 {
        return None;
    }
    if rm == 4 {
        let sib = *code.get(end)?;
        end += 1;
        if sib & 7 == 4 && !b {
            return None; // the stack
        }
        if sib & 7 == 5 && mode == 0 {
            end += 4; // no base, an address of 32 bits
        }
    } else if rm == 5 && mode == 0 {
        return None; // relative to the instruction
    }
    end += match mode {
        1 => 1,
        2 => 4,
        _ => 0,
    };
    let code = code.get(..end + constant)?;
    Some(Store {
        code,
        rex,
        vex3,
        modrm,
        address_end: end,
        x,
        b,
        size,
        source,
    })
}

impl Store<'_> {
    /// The registers the address is made of: its base and its index (by
    /// number).
    fn regs(&self) -> (Option<u8>, Option<u8>) {
        let m = self.code[self.modrm];
        let (mode, rm) = (m >> 6, m & 7);
        if rm != 4 {
            return (Some(rm | u8::from(self.b) << 3), None);
        }
        let sib = self.code[self.modrm + 1];
        let base = (!(sib & 7 == 5 && mode == 0)).then_some(sib & 7 | u8::from(self.b) << 3);
        let index = sib >> 3 & 7 | u8::from(self.x) << 3;
        (base, (index != 4).then_some(index))
    }

    /// `lea reg, [the store's address]` (`reg` one of the first eight).
    fn lea(&self, reg: u8) -> Vec<u8> {
        let m = self.code[self.modrm];
        let mut out = vec![
            0x48 | u8::from(self.x) << 1 | u8::from(self.b),
            0x8d,
            m & 0xc7 | reg << 3,
        ];
        out.extend(&self.code[self.modrm + 1..self.address_end]);
        out
    }

    /// The same store to `[reg]` (`reg` one of the first eight, not the stack
    /// pointer nor rbp).
    fn to(&self, reg: u8) -> Vec<u8> {
        let mut out = self.code[..self.modrm].to_vec();
        if let Some(at) = self.rex {
            out[at] &= !3;
        }
        if let Some(at) = self.vex3 {
            out[at + 1] |= 0x60;
        }
        out.push(self.code[self.modrm] & 0x38 | reg);
        out.extend(&self.code[self.address_end..]);
        out
    }
}

/// Code put together for a known address, with jumps to labels.
struct Asm {
    at: usize,
    code: Vec<u8>,
    labels: Vec<Option<usize>>,
    fixups: Vec<(usize, usize)>,
}

const JAE: u8 = 3;
const JNE: u8 = 5;

impl Asm {
    fn new(at: usize) -> Asm {
        Asm {
            at,
            code: Vec::new(),
            labels: Vec::new(),
            fixups: Vec::new(),
        }
    }

    fn put(&mut self, bytes: &[u8]) {
        self.code.extend(bytes);
    }

    fn label(&mut self) -> usize {
        self.labels.push(None);
        self.labels.len() - 1
    }

    fn here(&mut self, label: usize) {
        self.labels[label] = Some(self.code.len());
    }

    fn branch(&mut self, op: &[u8], label: usize) {
        self.code.extend(op);
        self.fixups.push((self.code.len(), label));
        self.code.extend([0; 4]);
    }

    fn jcc(&mut self, cc: u8, label: usize) {
        self.branch(&[0x0f, 0x80 | cc], label);
    }

    fn jmp(&mut self, label: usize) {
        self.branch(&[0xe9], label);
    }

    /// `op` (ending in a ModRM of `[rip + disp32]`) of the address `to`, if it
    /// reaches; no constant follows.
    fn rip(&mut self, op: &[u8], to: usize) -> Option<()> {
        self.code.extend(op);
        let from = self.at + self.code.len() + 4;
        self.code
            .extend(i32::try_from(to as i64 - from as i64).ok()?.to_le_bytes());
        Some(())
    }

    /// `op rel32` to the address `to`, if it reaches.
    fn rel(&mut self, op: &[u8], to: usize) -> Option<()> {
        self.code.extend(op);
        let from = self.at + self.code.len() + 4;
        self.code
            .extend(i32::try_from(to as i64 - from as i64).ok()?.to_le_bytes());
        Some(())
    }

    fn done(mut self) -> Option<Vec<u8>> {
        for (at, label) in std::mem::take(&mut self.fixups) {
            let to = self.labels[label]?;
            let rel = i32::try_from(to as i64 - (at as i64 + 4)).ok()?;
            self.code[at..at + 4].copy_from_slice(&rel.to_le_bytes());
        }
        Some(self.code)
    }
}

/// `op r/m64, r64` between two of the first eight registers (`to` the r/m).
fn rr(op: u8, from: u8, to: u8) -> [u8; 3] {
    [0x48, op, 0xc0 | from << 3 | to]
}

/// `op r64, [base + disp8]` (`base` not the stack pointer nor rbp).
fn rm(op: u8, reg: u8, base: u8, disp: u8) -> Vec<u8> {
    if disp == 0 {
        vec![0x48, op, reg << 3 | base]
    } else {
        vec![0x48, op, 0x40 | reg << 3 | base, disp]
    }
}

/// `shl` (4) or `shr` (5) of a register by `n`.
fn shift(kind: u8, reg: u8, n: u8) -> [u8; 4] {
    [0x48, 0xc1, 0xc0 | kind << 3 | reg, n]
}

const SHL: u8 = 4;
const SHR: u8 = 5;
const MOV: u8 = 0x89;
const MOV_LOAD: u8 = 0x8b;
const ADD: u8 = 0x01;
const ADD_LOAD: u8 = 0x03;
const SUB_LOAD: u8 = 0x2b;
const CMP_LOAD: u8 = 0x3b;
const XOR: u8 = 0x31;
const TEST: u8 = 0x85;

/// The cells' fields ([`CELLS_LEN`]).
const BASE: u8 = 0;
const DELTAS: u8 = 16;
const SLOTS: u8 = 24;
const BLOCKS: u8 = 32;
const TOLD: u8 = 40;
const ANY: u8 = 48;

/// The hooked form of `store` into `asm`: its address `a`; inside the heap,
/// the same store through our view, then the notes; outside, the store as it
/// was. `slow` is the code that asks for a block's way to our view. Every
/// register is put back; the flags too if `flags`.
#[allow(clippy::too_many_arguments)] // the cells, the slow call and the mirror each come from the caller
fn hooked(
    asm: &mut Asm,
    store: &Store,
    note: Note,
    flags: bool,
    cells: usize,
    slow: usize,
    mirror: usize,
    link: usize,
) -> Option<()> {
    if note == Note::Link {
        return link_call(asm, store, link);
    }
    // Three of rax, rcx, rdx, rbx, rsi, rdi that the store does not read.
    let free: Vec<u8> = [0u8, 1, 2, 3, 6, 7]
        .into_iter()
        .filter(|&r| Some(r) != store.source)
        .collect();
    let (a, c, d) = (free[0], free[1], free[2]);
    debug_assert!(a < 2);
    let (outside, have, add, end) = (asm.label(), asm.label(), asm.label(), asm.label());
    // Outside the heap (most of the title's stores at these places), one
    // register and the mirror of the heap's place by the stubs: compared and
    // gone before the rest is saved.
    if flags {
        asm.put(&[0x9c]); // pushfq
    }
    asm.put(&[0x50 | a]);
    asm.put(&store.lea(a));
    asm.rip(&[0x48, SUB_LOAD, 0x05 | a << 3], mirror)?; // sub a, [heap's first byte]: the offset
    asm.rip(&[0x48, CMP_LOAD, 0x05 | a << 3], mirror + 8)?; // cmp a, [its length]
    asm.jcc(JAE, outside);
    asm.put(&[0x50 | c, 0x50 | d]);
    asm.put(&[0x48, 0xb8 | c]); // mov c, cells
    asm.put(&(cells as u64).to_le_bytes());
    let pop = |asm: &mut Asm| {
        asm.put(&[0x58 | d, 0x58 | c]);
    };
    let pop_a = |asm: &mut Asm| {
        asm.put(&[0x58 | a]);
        if flags {
            asm.put(&[0x9d]); // popfq
        }
    };
    // d: what to add, from the block's entry.
    asm.put(&rr(MOV, a, d));
    asm.put(&shift(SHR, d, 16));
    asm.put(&shift(SHL, d, 3));
    asm.put(&rm(ADD_LOAD, d, c, DELTAS));
    asm.put(&rm(MOV_LOAD, d, d, 0));
    asm.put(&rr(TEST, d, d));
    asm.jcc(JNE, have);
    // Not known yet: asked for, the block's number in and the answer out on the stack.
    asm.put(&rr(MOV, a, d));
    asm.put(&shift(SHR, d, 16));
    asm.put(&[0x50 | d]);
    asm.rel(&[0xe8], slow)?;
    asm.put(&[0x58 | d]);
    asm.here(have);
    // 1: no view of ours, the title's address.
    asm.put(&[0x48, 0x83, 0xf8 | d, 1]); // cmp d, 1
    asm.jcc(JNE, add);
    asm.put(&rr(XOR, d, d));
    asm.here(add);
    asm.put(&rr(ADD, a, d));
    asm.put(&rm(ADD_LOAD, d, c, BASE));
    asm.put(&store.to(d));
    // The notes, after the bytes: the slot of the first byte and of the last.
    for last in [0, store.size - 1] {
        asm.put(&rr(MOV, a, d));
        if last != 0 {
            asm.put(&[0x48, 0x83, 0xc0 | d, last as u8]); // add d, last
        }
        asm.put(&shift(SHR, d, 5));
        asm.put(&rm(ADD_LOAD, d, c, SLOTS));
        asm.put(&[0xc6, d, note as u8]); // mov byte [d], note
    }
    // The block's marks: for the sync; for the watch by an exchange (a full
    // barrier: the bytes are seen before the mark is), and if that was not
    // set, the flag that some is.
    let told = asm.label();
    for table in [BLOCKS, TOLD] {
        asm.put(&rr(MOV, a, d));
        asm.put(&shift(SHR, d, 16));
        asm.put(&shift(SHL, d, LINE.trailing_zeros() as u8));
        asm.put(&rm(ADD_LOAD, d, c, table));
        if table == BLOCKS {
            asm.put(&[0xc6, d, 1]);
        }
    }
    // (`a` is rax or rcx, whose low byte needs no prefix.)
    asm.put(&[0xb8 | a, 1, 0, 0, 0]); // mov a32, 1
    asm.put(&[0x86, a << 3 | d]); // xchg [d], a8
    asm.put(&[0x84, 0xc0 | a << 3 | a]); // test a8, a8
    asm.jcc(JNE, told);
    asm.put(&[0xc6, 0x40 | c, ANY, 1]);
    asm.here(told);
    pop(asm);
    pop_a(asm);
    asm.jmp(end);
    asm.here(outside);
    pop_a(asm);
    asm.put(store.code);
    asm.here(end);
    Some(())
}

/// `vmovdqu` of ymm0 to ymm15 to (`op` 7Fh) or from (6Fh) `[rsp + 32 + 32k]`.
fn ymm_all(asm: &mut Asm, op: u8) {
    for k in 0..16u8 {
        asm.put(&[
            0xc5,
            if k < 8 { 0xfe } else { 0x7e },
            op,
            0x84 | (k & 7) << 3,
            0x24,
        ]);
        asm.put(&(32 + 32 * u32::from(k)).to_le_bytes());
    }
}

/// A store of 4 bytes made a call of `link(address, value)` (`extern "system"`),
/// every register, the vector registers whole and the flags put back. Not
/// for an address made with rdx, rbp or the stack pointer (the call's frame
/// changes them before the address is taken).
fn link_call(asm: &mut Asm, store: &Store, link: usize) -> Option<()> {
    let source = store.source?;
    let (base, index) = store.regs();
    if store.size != 4
        || link == 0
        || [base, index]
            .into_iter()
            .flatten()
            .any(|r| matches!(r, 2 | 4 | 5))
    {
        return None;
    }
    asm.put(&[0x9c]); // pushfq
    asm.put(&[0x55, 0x48, 0x89, 0xe5]); // push rbp ; mov rbp, rsp
    asm.put(&[
        0x50, 0x51, 0x52, 0x41, 0x50, 0x41, 0x51, 0x41, 0x52, 0x41, 0x53,
    ]); // push rax, rcx, rdx, r8-r11
    asm.put(&[0x48, 0x83, 0xe4, 0xe0]); // and rsp, -32
    asm.put(&[0x48, 0x81, 0xec, 0x20, 0x02, 0, 0]); // sub rsp, 32 + 16 * 32
    ymm_all(asm, 0x7f);
    // The value in edx (rbp's from where it was pushed), then the address in rcx.
    if source == 5 {
        asm.put(&[0x8b, 0x55, 0x00]); // mov edx, [rbp]
    } else {
        let rex = if source >= 8 { vec![0x44] } else { vec![] };
        asm.put(&rex);
        asm.put(&[0x89, 0xc0 | (source & 7) << 3 | 2]); // mov edx, source
    }
    asm.put(&store.lea(1));
    asm.put(&[0x48, 0xb8]); // mov rax, link
    asm.put(&(link as u64).to_le_bytes());
    asm.put(&[0xff, 0xd0]); // call rax
    ymm_all(asm, 0x6f);
    asm.put(&[0x48, 0x8d, 0x65, 0xc8]); // lea rsp, [rbp-56]
    asm.put(&[
        0x41, 0x5b, 0x41, 0x5a, 0x41, 0x59, 0x41, 0x58, 0x5a, 0x59, 0x58,
    ]); // pop r11-r8, rdx, rcx, rax
    asm.put(&[0x5d, 0x9d]); // pop rbp ; popfq
    Some(())
}

/// The code the stubs call for a block's way to our view: the block's number
/// is on the stack above the return address, and what to add comes back in
/// its place. It calls `resolve` (`extern "system" fn(u64) -> u64`) with every
/// register but the flags put back, the vector registers whole.
fn slow(at: usize, resolve: usize) -> Option<Vec<u8>> {
    let mut asm = Asm::new(at);
    asm.put(&[0x55]); // push rbp
    asm.put(&[0x48, 0x89, 0xe5]); // mov rbp, rsp
    asm.put(&[
        0x50, 0x51, 0x52, 0x41, 0x50, 0x41, 0x51, 0x41, 0x52, 0x41, 0x53,
    ]); // push rax, rcx, rdx, r8-r11
    asm.put(&[0x48, 0x83, 0xe4, 0xe0]); // and rsp, -32
    asm.put(&[0x48, 0x81, 0xec, 0x20, 0x02, 0, 0]); // sub rsp, 32 + 16 * 32
    let ymm = |asm: &mut Asm, op: u8| {
        for k in 0..16u8 {
            // vmovdqu [rsp + 32 + 32k], ymmk (or back)
            asm.put(&[
                0xc5,
                if k < 8 { 0xfe } else { 0x7e },
                op,
                0x84 | (k & 7) << 3,
                0x24,
            ]);
            asm.put(&(32 + 32 * u32::from(k)).to_le_bytes());
        }
    };
    ymm(&mut asm, 0x7f);
    asm.put(&[0x48, 0x8b, 0x4d, 0x10]); // mov rcx, [rbp+16]
    asm.put(&[0x48, 0xb8]); // mov rax, resolve
    asm.put(&(resolve as u64).to_le_bytes());
    asm.put(&[0xff, 0xd0]); // call rax
    asm.put(&[0x48, 0x89, 0x45, 0x10]); // mov [rbp+16], rax
    ymm(&mut asm, 0x6f);
    asm.put(&[0x48, 0x8d, 0x65, 0xc8]); // lea rsp, [rbp-56]
    asm.put(&[
        0x41, 0x5b, 0x41, 0x5a, 0x41, 0x59, 0x41, 0x58, 0x5a, 0x59, 0x58,
    ]); // pop r11-r8, rdx, rcx, rax
    asm.put(&[0x5d, 0xc3]); // pop rbp ; ret
    asm.done()
}

/// The code at `to` for `site`, whose span the title has at `at`: the span's
/// instructions, each store hooked, then back to the title's code. `mirror`:
/// the heap's first byte and length as the stubs compare them (16 bytes near
/// the stubs, kept by `victory-gpu` beside the cells). (The tests'; `apply` makes them from
/// the bytes it finds.)
#[cfg(test)]
fn stub(
    site: &Site,
    window: &[u8],
    at: usize,
    to: usize,
    cells: usize,
    slow: usize,
    mirror: usize,
) -> Option<Vec<u8>> {
    stub_with(site, window, window, at, to, cells, slow, mirror, 0)
}

/// The register a `mov r64, gs:[58h]` at the start of `code` loads: the title's load of its
/// thread's TLS array, as `crate::tls` finds them (the same bytes).
fn tls_load(code: &[u8]) -> Option<u8> {
    let c = code.get(..9)?;
    let reg = c[3] >> 3 & 7 | (c[1] & 4) << 1;
    (c[0] == 0x65
        && matches!(c[1], 0x48 | 0x4c)
        && c[2] == 0x8b
        && c[3] & 0xc7 == 0x04
        && c[4..] == [0x25, 0x58, 0, 0, 0]
        && reg != 4)
        .then_some(reg)
}

/// What `crate::tls` makes of such a load (when the image's TLS index is not 0): `call thunk;
/// nop dword [rax]`. The thunk's address, `code` being at `at`.
fn tls_call(code: &[u8], at: usize) -> Option<usize> {
    let c = code.get(..9)?;
    (c[0] == 0xe8 && c[5..] == [0x0f, 0x1f, 0x40, 0x00]).then(|| {
        (at + 5).wrapping_add_signed(i32::from_le_bytes(c[1..5].try_into().unwrap()) as isize)
    })
}

/// [`stub`], with `table` the window as the site's row describes it (its bytes, read back by
/// [`Site::window`]) and `found` the image's bytes at the same place (the title's, but for the
/// TLS-array loads `crate::tls` redirected, which the stub calls the same thunk for), and
/// `link` the function [`Note::Link`] stores call.
#[allow(clippy::too_many_arguments)] // `stub`'s, and what the pool hooks and the TLS add
fn stub_with(
    site: &Site,
    table: &[u8],
    found: &[u8],
    at: usize,
    to: usize,
    cells: usize,
    slow: usize,
    mirror: usize,
    link: usize,
) -> Option<Vec<u8>> {
    let span = table.get(site.span..site.span + site.span_len())?;
    let now = found.get(site.span..site.span + site.span_len())?;
    let mut asm = Asm::new(to);
    let mut offset = 0;
    for insn in site.insns {
        match *insn {
            Insn::Plain(n) => match (
                n,
                tls_load(&span[offset..]),
                tls_call(&now[offset..], at + offset),
            ) {
                // A redirected TLS-array load: the same thunk, called through the register it
                // loads (which the thunk sets without reading); the flags and the stack as
                // the call in the image leaves them.
                (9, Some(reg), Some(thunk)) => {
                    asm.put(&[0x48 | reg >> 3, 0xb8 | reg & 7]);
                    asm.put(&(thunk as u64).to_le_bytes());
                    if reg >= 8 {
                        asm.put(&[0x41]);
                    }
                    asm.put(&[0xff, 0xd0 | reg & 7]);
                }
                _ => asm.put(span.get(offset..offset + n)?),
            },
            Insn::Relative(n, disp) => {
                let code = span.get(offset..offset + n)?;
                let old = i32::from_le_bytes(code.get(disp..disp + 4)?.try_into().ok()?);
                let target = (at + offset + n) as i64 + i64::from(old);
                let new = i32::try_from(target - (asm.at + asm.code.len() + n) as i64).ok()?;
                asm.put(&code[..disp]);
                asm.put(&new.to_le_bytes());
                asm.put(&code[disp + 4..]);
            }
            Insn::Store(n, note) => {
                let store = decode(span.get(offset..offset + n)?)?;
                if store.code.len() != n {
                    return None;
                }
                hooked(
                    &mut asm,
                    &store,
                    note,
                    site.flags_live,
                    cells,
                    slow,
                    mirror,
                    link,
                )?;
            }
        }
        offset += insn.len();
    }
    asm.rel(&[0xe9], at + offset)?;
    asm.done()
}

/// What became of a site that was hooked: its span in the image, and its
/// stub's code (address, bytes).
#[derive(Debug, Clone, Copy)]
pub struct Hooked {
    pub name: &'static str,
    pub span: usize,
    pub stub: usize,
    pub len: usize,
}

/// Hooks every site of `sites` in the image at `base` (`size` bytes), or
/// none: the stubs read the cells at `cells` and ask `resolve` for a block's
/// way to our view. What each became, or why none was hooked (a site whose
/// window differs is named with its place, its length and the tag found there,
/// never with the title's bytes).
///
/// # Safety
/// The image stays mapped and no thread runs its code yet; `cells` and
/// `resolve` stay valid for as long as the image runs.
#[cfg(windows)]
pub unsafe fn apply(
    base: usize,
    size: usize,
    cells: usize,
    resolve: usize,
    sites: &[Site],
) -> Result<(Vec<Hooked>, usize), String> {
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut differ = Vec::new();
    for site in sites {
        let len = usize::from(site.len);
        if site.window.checked_add(len).is_none_or(|end| end > size)
            || site.span + site.span_len() > len
            || site.span_len() < 5
        {
            return Err(format!(
                "{} (title+{:#x}) is not a span of the image",
                site.name, site.window
            ));
        }
        // SAFETY: inside the mapped image (checked above).
        let there = unsafe { std::slice::from_raw_parts((base + site.window) as *const u8, len) };
        if site.window(there).is_none() {
            differ.push(format!(
                "{} (title+{:#x}: {} bytes whose BLAKE3-64 is {:016x}, not the recorded {:016x})",
                site.name,
                site.window,
                len,
                Site::tag(there),
                site.digest
            ));
        }
    }
    if !differ.is_empty() {
        return Err(format!(
            "the title's bytes differ at {} of {} places (another build?): {}",
            differ.len(),
            sites.len(),
            differ.join(", ")
        ));
    }
    if !std::is_x86_feature_detected!("avx") {
        return Err("the CPU has no AVX, which the stubs keep the vector registers with".into());
    }
    // No two spans meet. (Named with their places: the generator's sites share one name, and
    // it is the place that says which store is meant.)
    let mut spans: Vec<(usize, usize, &str, usize)> = sites
        .iter()
        .map(|s| (s.window + s.span, s.window + s.span + s.span_len(), s.name, s.window))
        .collect();
    spans.sort_unstable();
    if let Some(w) = spans.windows(2).find(|w| w[1].0 < w[0].1) {
        return Err(format!(
            "{} (title+{:#x}) and {} (title+{:#x}) overlap",
            w[0].2, w[0].3, w[1].2, w[1].3
        ));
    }
    // The mirror of the heap's place (16 bytes), the call, then the stubs.
    let place = room(base, AREA).ok_or("no room for code near the title's")?;
    let mirror = place;
    let slow_at = place + 64;
    let slow = slow(slow_at, resolve).ok_or("the call's code could not be made")?;
    let mut used = (64 + slow.len()).next_multiple_of(16);
    let mut made: Vec<(Hooked, Vec<u8>, [u8; 5])> = Vec::new();
    for site in sites {
        let at = base + site.window + site.span;
        let to = place + used;
        // SAFETY: inside the mapped image (checked above).
        let there = unsafe {
            std::slice::from_raw_parts((base + site.window) as *const u8, usize::from(site.len))
        };
        // The window again: a stub is made of the title's own bytes, which the table no
        // longer carries, and they are only taken when they are what its tag stands for.
        let Some(table) = site.window(there) else {
            return Err(format!(
                "{} (title+{:#x}): the image's bytes there are not the recorded ones",
                site.name, site.window
            ));
        };
        let (Some(body), Some(jump)) = (
            stub_with(site, &table, there, at, to, cells, slow_at, mirror, 0),
            crate::console_insn::jump(at, to),
        ) else {
            return Err(format!(
                "{} (title+{:#x}): not a store the stubs take, or out of reach",
                site.name,
                site.window + site.span
            ));
        };
        if used + body.len() > AREA {
            return Err("no room for code near the title's".into());
        }
        used += body.len().next_multiple_of(16);
        made.push((
            Hooked {
                name: site.name,
                span: site.window + site.span,
                stub: to,
                len: body.len(),
            },
            body,
            jump,
        ));
    }
    // SAFETY: our own code memory, which nothing runs yet.
    unsafe { std::ptr::copy_nonoverlapping(slow.as_ptr(), slow_at as *mut u8, slow.len()) };
    for (hooked, body, _) in &made {
        // SAFETY: as above.
        unsafe { std::ptr::copy_nonoverlapping(body.as_ptr(), hooked.stub as *mut u8, body.len()) };
    }
    for (site, (hooked, _, jump)) in sites.iter().zip(&made) {
        let (at, len) = (base + hooked.span, site.span_len());
        let mut old = 0;
        // SAFETY: the span, in the title's code, which nothing runs yet; made
        // writable for this and put back.
        unsafe {
            if VirtualProtect(at as *const _, len, PAGE_EXECUTE_READWRITE, &mut old) == 0 {
                // (Those before stay hooked: the image is not run.)
                return Err(format!(
                    "{}: Windows would not make the title's code writable",
                    site.name
                ));
            }
            std::ptr::write_bytes(at as *mut u8, 0x90, len);
            std::ptr::copy_nonoverlapping(jump.as_ptr(), at as *mut u8, jump.len());
            VirtualProtect(at as *const _, len, old, &mut old);
            FlushInstructionCache(GetCurrentProcess(), at as *const _, len);
        }
    }
    tracing::info!(
        sites = made.len(),
        stores = sites
            .iter()
            .flat_map(|s| s.insns)
            .filter(|i| matches!(i, Insn::Store(..)))
            .count(),
        code = used,
        "heap hooks: the title's stores into its view heap go through our view and are noted (VICTORY_HEAP_HOOKS)"
    );
    Ok((made.into_iter().map(|m| m.0).collect(), mirror))
}

/// What the pool hooks' places became: the stubs (as [`apply`]'s), and the
/// mirror of the special pool's place the stubs compare first.
#[derive(Debug, Clone)]
pub struct PoolHooked {
    pub stubs: Vec<Hooked>,
    pub mirror: usize,
}

/// The pool hooks (`VICTORY_POOL_HOOKS=1`; `victory-gpu`'s `pool_hooks`), all
/// of them or none: the general pools' fill (`memcpy` at +0x4f3f877) becomes a
/// call of `copy` (`extern "system" fn(dest, src, len) -> dest`); the two free
/// lists' links calls of `link(address, value)`; the special pool's stores
/// stores through our view, noted in the cells at `special` (whose way to our
/// view `resolve` gives), the switch's cases by its table's entries. What became
/// of them, or why none was hooked (a place whose window differs is named with
/// the tag found there, never with the title's bytes).
///
/// # Safety
/// As [`apply`]'s; `copy`, `link` and `resolve` stay valid while the image runs.
#[cfg(windows)]
pub unsafe fn apply_pools(
    base: usize,
    size: usize,
    special: usize,
    resolve: usize,
    link: usize,
    copy: usize,
) -> Result<PoolHooked, String> {
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{
        PAGE_EXECUTE_READWRITE, PAGE_READWRITE, VirtualProtect,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let there = |at: usize, len: usize| -> Option<&[u8]> {
        // SAFETY: inside the mapped image (checked).
        (at.checked_add(len)? <= size)
            .then(|| unsafe { std::slice::from_raw_parts((base + at) as *const u8, len) })
    };
    let mut differ = Vec::new();
    let mut check = |name: &str, at: usize, len: u16, digest: u64, tls: &[Tls]| {
        match there(at, usize::from(len)) {
            Some(found) if recorded(len, digest, tls, found).is_some() => {}
            Some(found) => differ.push(format!(
                "{name} (title+{at:#x}: {len} bytes whose BLAKE3-64 is {:016x}, not the recorded {digest:016x})",
                Site::tag(found)
            )),
            None => differ.push(format!("{name} (title+{at:#x}: outside the image)")),
        }
    };
    let (copy_at, copy_len, copy_digest, copy_tls, call) = pool_users::COPY;
    check("pool copy", copy_at, copy_len, copy_digest, copy_tls);
    for s in pool_users::LINKS.iter().chain(pool_users::SPECIAL) {
        check(s.name, s.window, s.len, s.digest, s.tls);
    }
    let (table_at, table_len, table_digest) = pool_users::TABLE;
    check("pool switch table", table_at, table_len, table_digest, &[]);
    for (_, s) in pool_users::CASES {
        check(s.name, s.window, s.len, s.digest, s.tls);
    }
    if !differ.is_empty() {
        return Err(format!(
            "the title's bytes differ at {} places (another build?): {}",
            differ.len(),
            differ.join(", ")
        ));
    }
    if !std::is_x86_feature_detected!("avx") {
        return Err("the CPU has no AVX, which the stubs keep the vector registers with".into());
    }
    let place = room(base, 1 << 20).ok_or("no room for code near the title's")?;
    let mirror = place;
    let slow_at = place + 64;
    let slow = slow(slow_at, resolve).ok_or("the call's code could not be made")?;
    let tramp = (slow_at + slow.len()).next_multiple_of(16);
    // mov rax, copy ; jmp rax
    let mut jump_copy = vec![0x48, 0xb8];
    jump_copy.extend((copy as u64).to_le_bytes());
    jump_copy.extend([0xff, 0xe0]);
    let mut used = (tramp - place + jump_copy.len()).next_multiple_of(16);
    let mut code: Vec<(usize, Vec<u8>)> = vec![(slow_at, slow), (tramp, jump_copy)];
    let mut stubs = Vec::new();
    let mut make =
        |s: &Site, used: &mut usize, code: &mut Vec<(usize, Vec<u8>)>| -> Result<usize, String> {
            let (at, to) = (base + s.window + s.span, place + *used);
            let here = there(s.window, usize::from(s.len)).ok_or("outside the image")?;
            // As in the heap's own sites: the title's bytes, from the image, once the
            // tag says they are the ones this row stands for.
            let table = s.window(here).ok_or("not the recorded bytes")?;
            let body =
                stub_with(s, &table, here, at, to, special, slow_at, mirror, link).ok_or_else(|| {
                    format!(
                        "{} (title+{:#x}): not a store the stubs take",
                        s.name, s.window
                    )
                })?;
            *used += body.len().next_multiple_of(16);
            stubs.push(Hooked {
                name: s.name,
                span: s.window + s.span,
                stub: to,
                len: body.len(),
            });
            code.push((to, body));
            Ok(to)
        };
    let mut spans = Vec::new();
    for s in pool_users::LINKS.iter().chain(pool_users::SPECIAL) {
        let to = make(s, &mut used, &mut code)?;
        let jump = crate::console_insn::jump(base + s.window + s.span, to).ok_or("out of reach")?;
        spans.push((base + s.window + s.span, s.span_len(), jump));
    }
    let table = base + pool_users::TABLE.0;
    let mut entries = Vec::new();
    for (index, s) in pool_users::CASES {
        let to = make(s, &mut used, &mut code)?;
        let rel = i32::try_from(to as i64 - table as i64)
            .map_err(|_| "the switch's table cannot reach the stubs")?;
        entries.push((table + 4 * index, rel));
    }
    let call_at = base + copy_at + call;
    let rel = i32::try_from(tramp as i64 - (call_at + 5) as i64)
        .map_err(|_| "the copy's call cannot reach its stub")?;
    if used > 1 << 20 {
        return Err("no room for code near the title's".into());
    }
    for (at, body) in &code {
        // SAFETY: our own code memory, which nothing runs yet.
        unsafe { std::ptr::copy_nonoverlapping(body.as_ptr(), *at as *mut u8, body.len()) };
    }
    // SAFETY (all below): the title's code and its table, which nothing runs
    // yet, each made writable for this and put back.
    let write = |at: usize, bytes: &[u8], protect: u32| -> Result<(), String> {
        let mut old = 0;
        unsafe {
            if VirtualProtect(at as *const _, bytes.len(), protect, &mut old) == 0 {
                return Err(format!(
                    "Windows would not make title+{:#x} writable",
                    at - base
                ));
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), at as *mut u8, bytes.len());
            VirtualProtect(at as *const _, bytes.len(), old, &mut old);
            FlushInstructionCache(GetCurrentProcess(), at as *const _, bytes.len());
        }
        Ok(())
    };
    for (at, len, jump) in spans {
        let mut bytes = vec![0x90; len];
        bytes[..5].copy_from_slice(&jump);
        write(at, &bytes, PAGE_EXECUTE_READWRITE)?;
    }
    for (at, rel) in entries {
        write(at, &rel.to_le_bytes(), PAGE_READWRITE)?;
    }
    let mut call_bytes = vec![0xe8];
    call_bytes.extend(rel.to_le_bytes());
    write(call_at, &call_bytes, PAGE_EXECUTE_READWRITE)?;
    tracing::info!(
        stubs = stubs.len(),
        code = used,
        "pool hooks: the pools' fill, their free lists' links and the special pool's stores are hooked (VICTORY_POOL_HOOKS)"
    );
    Ok(PoolHooked { stubs, mirror })
}

/// Bytes of code memory for the stubs.
#[cfg(windows)]
const AREA: usize = 4 << 20;

/// `len` bytes of memory for code within reach of a `jmp rel32` at `at`.
#[cfg(windows)]
fn room(at: usize, len: usize) -> Option<usize> {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc,
    };
    let at = at & !0xffff;
    (0x100_0000..0x6000_0000)
        .step_by(0x100_0000)
        .flat_map(|d| [at.checked_sub(d), at.checked_add(d)])
        .flatten()
        .find_map(|place| {
            // SAFETY: reserves free memory at that address or fails.
            let got = unsafe {
                VirtualAlloc(
                    place as *const _,
                    len,
                    MEM_COMMIT | MEM_RESERVE,
                    PAGE_EXECUTE_READWRITE,
                )
            } as usize;
            (got != 0).then_some(got)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a row holds now that it holds no bytes: a window big enough for its span and the
    /// jump into the stub, no two spans meeting, one window to a site, and its TLS loads inside
    /// the window. The tables are pinned by their size as well — the same sites, over the same
    /// bytes, as the port they come from.
    ///
    /// (That each `Store` really is a store of the length its recipe says, and that a whole span
    /// decodes, is the site generator's check, made against the title's bytes; `apply` reads a
    /// window from the mapped image and refuses the whole run when its bytes are not what the row
    /// hashes to, so a row that says otherwise costs the hooks and never the title.)
    #[test]
    fn the_sites_hold_windows_that_fit() {
        let all = sites();
        assert_eq!(all.len(), 340, "the sites of the three tables");
        assert_eq!(
            all.iter().map(|s| usize::from(s.len)).sum::<usize>(),
            11_188,
            "the bytes of the windows they name"
        );
        let mut spans: Vec<(usize, usize, &str)> = all
            .iter()
            .map(|s| (s.window + s.span, s.window + s.span + s.span_len(), s.name))
            .collect();
        spans.sort_unstable();
        for w in spans.windows(2) {
            assert!(w[1].0 >= w[0].1, "{} and {} overlap", w[0].2, w[1].2);
        }
        let mut windows: Vec<usize> = all.iter().map(|s| s.window).collect();
        windows.sort_unstable();
        assert_eq!(
            windows.windows(2).filter(|w| w[0] == w[1]).count(),
            0,
            "one window named by two sites"
        );
        for site in &all {
            let len = usize::from(site.len);
            assert!(site.span + site.span_len() <= len, "{}", site.name);
            assert!(site.span_len() >= 5, "{}: room for the jump", site.name);
            assert_ne!(site.digest, 0, "{}", site.name);
            for t in site.tls {
                assert!(
                    usize::from(t.at) + 9 <= len,
                    "{}: a TLS load past its window",
                    site.name
                );
            }
        }
    }

    /// A site of the tests' and the window its tag stands for, made of code written here. The
    /// tables say the same thing about the title's places, from the title's bytes — of which this
    /// repository keeps offsets, lengths and hashes only.
    fn made(
        name: &'static str,
        window: &[u8],
        tls: &'static [Tls],
        span: usize,
        insns: &'static [Insn],
        flags_live: bool,
    ) -> Site {
        Site {
            name,
            window: 0x1000,
            len: window.len() as u16,
            digest: Site::tag(window),
            tls,
            span,
            insns,
            flags_live,
        }
    }

    /// The shapes the tables patch, as code we wrote: a record store of the table fillers, the
    /// frame allocator's own stores, a slot given back to its pool, a pool's free-list link (whose
    /// flags the title reads after), a span with a branch to move, a window with a TLS-array load,
    /// and a case too short for the jump: test material for `Site::window` and for the stubs.
    pub(super) fn fixtures() -> Vec<(Site, Vec<u8>)> {
        let mut all = Vec::new();
        let mut put = |window: &[u8], tls, span, insns: &'static [Insn], flags, name| {
            all.push((made(name, window, tls, span, insns, flags), window.to_vec()));
        };
        // and ecx, 7FE0h ; vmovups ymm0, [rdx] ; vmovups [rcx+rsi], ymm0 ; movzx eax, byte
        // [rbx+10h] ; cmp eax, [rbp]  — a record store, flags set again by the cmp.
        put(
            &[
                0x81, 0xe1, 0xe0, 0x7f, 0x00, 0x00, 0xc5, 0xfc, 0x10, 0x02, 0xc5, 0xfc, 0x11,
                0x04, 0x31, 0x0f, 0xb6, 0x43, 0x10, 0x3b, 0x45, 0x00,
            ],
            &[],
            10,
            &[Insn::Store(5, Note::Record)],
            false,
            "record store",
        );
        // lea rdx, [rcx+rax] ; mov [rdi+10h], rdx ; mov rdx, [rcx+rax+48h] ; mov [rbx], rdx ;
        // mov [rcx+rax+48h], rbx — the allocator's own lists, not records.
        put(
            &[
                0x48, 0x8d, 0x14, 0x01, 0x48, 0x89, 0x57, 0x10, 0x48, 0x8b, 0x54, 0x01, 0x48,
                0x48, 0x89, 0x13, 0x48, 0x89, 0x5c, 0x01, 0x48,
            ],
            &[],
            4,
            &[
                Insn::Store(4, Note::Other),
                Insn::Plain(5),
                Insn::Store(3, Note::Other),
                Insn::Store(5, Note::Other),
            ],
            false,
            "lists of the allocator",
        );
        // mov r11, rdx ; mov [rcx], edx ; mov rax, rdx ; and r11, r13 — a slot back to the pool
        // of persistent ones: its first word is the pool's link.
        put(
            &[0x49, 0x89, 0xd3, 0x89, 0x11, 0x48, 0x89, 0xd0, 0x4d, 0x21, 0xcb],
            &[],
            3,
            &[Insn::Store(2, Note::Freed), Insn::Plain(3)],
            false,
            "slot given back",
        );
        // mov [r14], ebp ; lea rdx, [r12+r*8+200h] — a free list's link, and the flags the title
        // reads after it.
        put(
            &[0x41, 0x89, 0x2e, 0x49, 0x8d, 0x94, 0x14, 0x00, 0x00, 0x02, 0x00],
            &[],
            0,
            &[Insn::Store(3, Note::Link), Insn::Plain(8)],
            true,
            "free list's link",
        );
        // mov [rbx], rdx ; jmp rel32 — a store and a branch that moves with the stub.
        put(
            &[0x48, 0x89, 0x13, 0xe9, 0x00, 0x01, 0x00, 0x00],
            &[],
            0,
            &[Insn::Store(3, Note::Other), Insn::Relative(5, 1)],
            false,
            "store and branch",
        );
        // mov rax, gs:[58h] ; mov [rbx], rdx ; mov [rbx+8], rcx — the title's thread's TLS array
        // read, which `crate::tls` may have made a call of its thunk, and two stores after it.
        put(
            &[
                0x65, 0x48, 0x8b, 0x04, 0x25, 0x58, 0x00, 0x00, 0x00, 0x48, 0x89, 0x13, 0x48,
                0x89, 0x4b, 0x08,
            ],
            &[Tls { at: 0, reg: 0 }],
            0,
            &[
                Insn::Plain(9),
                Insn::Store(3, Note::Other),
                Insn::Store(4, Note::Other),
            ],
            false,
            "tls window",
        );
        // mov word ptr [rsi+2], ax — a case too short for a jump of its own (the special pool's
        // switch reaches it through its table).
        put(
            &[0x66, 0x89, 0x46, 0x02],
            &[],
            0,
            &[Insn::Store(4, Note::Record)],
            false,
            "case of the switch",
        );
        // mov qword ptr [r14], 0 ; mov [r14+8], rbx — a node's first two words, filled.
        put(
            &[0x49, 0xc7, 0x06, 0x00, 0x00, 0x00, 0x00, 0x49, 0x89, 0x5e, 0x08],
            &[],
            0,
            &[Insn::Store(7, Note::Other), Insn::Store(4, Note::Other)],
            false,
            "node filled",
        );
        all
    }

    /// The title's bytes, where a private copy of them is at hand: one window per line, as its
    /// offset in the image and its bytes in hex (`VICTORY_HEAP_WINDOWS`). They are the title's
    /// code, so they are not in this repository; the tests over the tables run only with them.
    pub(super) fn title_windows() -> Option<std::collections::HashMap<usize, Vec<u8>>> {
        let text = std::fs::read_to_string(std::env::var_os("VICTORY_HEAP_WINDOWS")?).ok()?;
        let mut out = std::collections::HashMap::new();
        for line in text.lines() {
            let (at, hex) = line.trim_end().split_once(' ')?;
            let at = usize::from_str_radix(at, 16).ok()?;
            let hex = hex.as_bytes();
            if hex.len() % 2 != 0 {
                return None;
            }
            let mut window = Vec::with_capacity(hex.len() / 2);
            for c in hex.chunks(2) {
                window.push(u8::from_str_radix(std::str::from_utf8(c).ok()?, 16).ok()?);
            }
            out.insert(at, window);
        }
        (!out.is_empty()).then_some(out)
    }

    /// What `crate::tls` takes is a whole `mov r64, gs:[58h]`, wherever it lies: a window that
    /// reached into one — to its first bytes, where the image no longer shows them, or past
    /// them, where the row could not name it — would hash bytes the running title does not
    /// have. Only the title's own windows show a load to look for (`VICTORY_HEAP_WINDOWS`).
    #[test]
    fn no_row_holds_bytes_the_tls_redirect_changes() {
        let Some(windows) = title_windows() else {
            return;
        };
        let mut met = 0;
        for site in sites() {
            let Some(here) = windows.get(&site.window) else {
                continue;
            };
            let len = usize::from(site.len);
            assert_eq!(here.len(), len, "{}", site.name);
            for at in 0..len {
                if !begins_a_tls_load(&here[at..]) {
                    continue;
                }
                met += 1;
                assert!(
                    site.tls.iter().any(|t| usize::from(t.at) == at) && at + 9 <= len,
                    "{}: a TLS-array load at +{at:#x} the row neither names nor stops before",
                    site.name
                );
            }
        }
        assert!(
            met >= 1,
            "a TLS-array load among the title's windows of the tables"
        );
    }

    /// A row whose window ends before a TLS-array load is the title's row however far the
    /// loader's call reaches: `tls::redirect` writes `call rel32` and four nops over the load's
    /// nine bytes, and that displacement is the distance to a page `near()` allocated, which no
    /// build gives twice. A window that hashed even the call's first byte would hash the
    /// allocation instead of the title — this is the shape of the row at +0x325bbf3, whose window
    /// stops with the jump before its load, and it is what the row looked like before it stopped.
    #[test]
    fn a_window_that_stops_before_a_load_hashes_the_title_not_the_thunk() {
        // mov [rbx], rdx ; mov [rbx+8], rcx ; jmp rel32 ; mov rax, gs:[58h]
        let code: Vec<u8> = [
            &[0x48, 0x89, 0x13, 0x48, 0x89, 0x4b, 0x08][..],
            &[0xe9, 0x00, 0x01, 0x00, 0x00][..],
            &tls_load_insn(0)[..],
        ]
        .concat();
        let insns: &[Insn] = &[
            Insn::Store(3, Note::Other),
            Insn::Store(4, Note::Other),
            Insn::Relative(5, 1),
        ];
        let (base, place) = (0x1_4000_0000usize, 0x1_3ff0_0000usize);
        let stops = made("stops before the load", &code[..12], &[], 0, insns, false);
        let cuts = made("cuts into the load", &code[..14], &[], 0, insns, false);
        let at = base + 0x1000; // `made` places its window there
        let mut made_stub: Option<Vec<u8>> = None;
        for thunk in [0x1_3fe0_0000usize, 0x1_1000_0000] {
            // The image as `crate::tls` left it, with the thunk page here.
            let mut image = code.clone();
            let rel = (thunk as i64 - (at + 12 + 5) as i64) as i32;
            image[12] = 0xe8;
            image[13..17].copy_from_slice(&rel.to_le_bytes());
            image[17..21].copy_from_slice(&[0x0f, 0x1f, 0x40, 0x00]);
            let there = stops
                .window(&image[..12])
                .unwrap_or_else(|| panic!("thunk {thunk:#x}: the title's bytes, stopped short"));
            assert_eq!(there, code[..12], "the same bytes either way");
            let code_of_stub = stub(
                &stops,
                &there,
                at,
                place + 0x100,
                0x7ff6_1234_5678,
                place,
                place + 0x80,
            )
            .unwrap();
            if let Some(first) = &made_stub {
                assert_eq!(first, &code_of_stub, "the stub does not reach the thunk");
            } else {
                made_stub = Some(code_of_stub);
            }
            // The row that hashed into the load: the title's own image is its recorded one, and
            // either redirect is not.
            assert_eq!(cuts.window(&code[..14]).unwrap(), code[..14]);
            assert!(cuts.window(&image[..14]).is_none(), "thunk {thunk:#x}");
        }
    }

    /// A load a row names in `tls` is one its stub can make a call of: either it lies outside
    /// the span the stub replays, where the name only says what the window's tag stands for, or
    /// it is a `Plain(9)` of the span, which is the shape `stub_with` turns into a call of the
    /// same thunk. Any other shape would go into the stub as the image has it, and the image's
    /// call reaches its thunk from the title's place, not from the stub's.
    #[test]
    fn named_tls_loads_are_whole_instructions_of_the_span() {
        let mut met = 0;
        for site in sites() {
            let end = site.span + site.insns.iter().map(|i| i.len()).sum::<usize>();
            for t in site.tls {
                let at = usize::from(t.at);
                met += 1;
                if at < site.span || at >= end {
                    continue; // outside the span: only the window's tag holds its bytes
                }
                let mut offset = site.span;
                let whole = site.insns.iter().any(|insn| {
                    let here = offset == at && matches!(*insn, Insn::Plain(9));
                    offset += insn.len();
                    here
                });
                assert!(
                    whole,
                    "{}: a TLS load at +{at:#x} of its span that no Plain(9) is",
                    site.name
                );
            }
        }
        assert!(met >= 1, "the tables name TLS loads of their own");
    }

    /// The beginning of a `mov r64, gs:[58h]`, judged from as much of it as a window shows.
    /// Whole, this is what `tls_load` takes; cut short by the window's end it is the title's
    /// load still, and `crate::tls` takes it no less. A load into rsp it leaves alone.
    fn begins_a_tls_load(code: &[u8]) -> bool {
        if code.len() >= 9 {
            return tls_load(code).is_some();
        }
        let nth = |i: usize| code.get(i).copied();
        if nth(0) != Some(0x65) || !matches!(nth(1), Some(0x48) | Some(0x4c)) {
            return false;
        }
        if nth(2).is_some_and(|b| b != 0x8b) {
            return false;
        }
        // The ModRM says which register loads, and only that: 04h-form, and not into rsp.
        if let Some(m) = nth(3) {
            let reg = m >> 3 & 7 | (nth(1).unwrap_or(0x48) & 4) << 1;
            if m & 0xc7 != 0x04 || reg == 4 {
                return false;
            }
        }
        let tail = [0x25, 0x58, 0, 0, 0];
        [4, 5, 6, 7, 8]
            .into_iter()
            .all(|i| nth(i).is_none_or(|b| b == tail[i - 4]))
    }

    /// A window is the recorded one and only itself: every byte changed at every place, a byte
    /// too few, and a byte too many are refused; what fits comes back as the image's own bytes.
    #[test]
    fn a_window_that_differs_is_not_the_recorded_one() {
        for (site, window) in fixtures() {
            assert_eq!(
                site.window(&window).unwrap(),
                window,
                "{}: the recorded bytes come back",
                site.name
            );
            for at in 0..window.len() {
                let mut other = window.clone();
                other[at] ^= 0x01;
                assert!(
                    site.window(&other).is_none(),
                    "{}: byte {at} changed",
                    site.name
                );
            }
            assert!(
                site.window(&window[..window.len() - 1]).is_none(),
                "{}: a byte too few",
                site.name
            );
            let mut longer = window.clone();
            longer.push(0x90);
            assert!(
                site.window(&longer).is_none(),
                "{}: a byte too many",
                site.name
            );
        }
    }

    #[test]
    fn stores_are_taken_apart_and_made_to_a_register() {
        // vmovups [rcx+rsi], ymm0
        let s = decode(&[0xc5, 0xfc, 0x11, 0x04, 0x31]).unwrap();
        assert_eq!((s.size, s.source), (32, None));
        assert_eq!(s.lea(0), [0x48, 0x8d, 0x04, 0x31]); // lea rax, [rcx+rsi]
        assert_eq!(s.to(2), [0xc5, 0xfc, 0x11, 0x02]); // vmovups [rdx], ymm0
        // vmovups [r9+rsi], ymm0
        let s = decode(&[0xc4, 0xc1, 0x7c, 0x11, 0x04, 0x31]).unwrap();
        assert_eq!(s.lea(0), [0x49, 0x8d, 0x04, 0x31]);
        assert_eq!(s.to(2), [0xc4, 0xe1, 0x7c, 0x11, 0x02]);
        // vmovups [r10+r8], ymm0
        let s = decode(&[0xc4, 0x81, 0x7c, 0x11, 0x04, 0x02]).unwrap();
        assert_eq!(s.lea(0), [0x4b, 0x8d, 0x04, 0x02]);
        assert_eq!(s.to(1), [0xc4, 0xe1, 0x7c, 0x11, 0x01]);
        // mov [rbx], rdx
        let s = decode(&[0x48, 0x89, 0x13]).unwrap();
        assert_eq!((s.size, s.source), (8, Some(2)));
        assert_eq!(s.lea(0), [0x48, 0x8d, 0x03]);
        assert_eq!(s.to(1), [0x48, 0x89, 0x11]);
        // mov [rcx], edx
        let s = decode(&[0x89, 0x11]).unwrap();
        assert_eq!((s.size, s.source), (4, Some(2)));
        assert_eq!(s.to(3), [0x89, 0x13]);
        // mov qword [r14], 0
        let s = decode(&[0x49, 0xc7, 0x06, 0, 0, 0, 0]).unwrap();
        assert_eq!((s.size, s.source), (8, None));
        assert_eq!(s.lea(0), [0x49, 0x8d, 0x06]);
        assert_eq!(s.to(2), [0x48, 0xc7, 0x02, 0, 0, 0, 0]);
        // mov [r14+8], rbx
        let s = decode(&[0x49, 0x89, 0x5e, 0x08]).unwrap();
        assert_eq!((s.size, s.source), (8, Some(3)));
        assert_eq!(s.lea(0), [0x49, 0x8d, 0x46, 0x08]);
        assert_eq!(s.to(2), [0x48, 0x89, 0x1a]);
        // mov [r14+10h], eax
        let s = decode(&[0x41, 0x89, 0x46, 0x10]).unwrap();
        assert_eq!((s.size, s.source), (4, Some(0)));
        assert_eq!(s.lea(1), [0x49, 0x8d, 0x4e, 0x10]);
        assert_eq!(s.to(2), [0x40, 0x89, 0x02]);
        // mov [r12+rax*8+100h], r9d
        let s = decode(&[0x45, 0x89, 0x8c, 0xc4, 0x00, 0x01, 0x00, 0x00]).unwrap();
        assert_eq!((s.size, s.source), (4, Some(9)));
        assert_eq!(s.lea(0), [0x49, 0x8d, 0x84, 0xc4, 0x00, 0x01, 0x00, 0x00]);
        // mov [rcx], ah: the high byte of rax.
        assert_eq!(decode(&[0x88, 0x21]).unwrap().source, Some(0));
        // Not taken: the stack, an address relative to the instruction, a
        // load, a register, a lock.
        assert!(decode(&[0x48, 0x89, 0x04, 0x24]).is_none());
        assert!(decode(&[0x48, 0x89, 0x05, 0, 0, 0, 0]).is_none());
        assert!(decode(&[0x48, 0x8b, 0x13]).is_none());
        assert!(decode(&[0x48, 0x89, 0xd3]).is_none());
        assert!(decode(&[0xf0, 0x48, 0x89, 0x13]).is_none());
        assert!(decode(&[0xc5, 0xfc, 0x10, 0x04, 0x31]).is_none());
    }

    /// A pool's free-list link, as `victory-gpu` gives it to the pool hooks. Only its place is
    /// of use here: the stubs of the fixtures are made, never run.
    extern "system" fn no_link(_: u64, _: u32) {}

    /// Each fixture's span becomes a stub, and the stub goes back to where the span ends. (The
    /// store given back to a pool is made a call of the pool's link, so a link is named.)
    #[test]
    fn every_fixture_site_has_a_stub() {
        let (base, place) = (0x1_4000_0000usize, 0x1_3ff0_0000usize);
        for (site, window) in fixtures() {
            let at = base + site.window + site.span;
            let code = stub_with(
                &site,
                &window,
                &window,
                at,
                place + 0x100,
                0x7ff6_1234_5678,
                place,
                place + 0x80,
                no_link as *const () as usize,
            )
            .unwrap_or_else(|| panic!("{}", site.name));
            // Back to the title's code after the span.
            let back = i32::from_le_bytes(code[code.len() - 4..].try_into().unwrap()) as i64;
            assert_eq!(
                (place + 0x100 + code.len()) as i64 + back,
                (at + site.span_len()) as i64,
                "{}",
                site.name
            );
        }
        assert!(slow(place, 0x7ff6_0000_1000).is_some());
    }

    /// The tables themselves, over the title's bytes: a row's tag is what the bytes at its place
    /// hash to, they come back unchanged, and its span makes a stub. Only with a private copy of
    /// the windows (`VICTORY_HEAP_WINDOWS`), which are the title's code and so not here; the
    /// fixtures above are what a stub is asked of without them.
    #[test]
    fn every_site_of_the_tables_has_a_stub() {
        let Some(windows) = title_windows() else {
            return;
        };
        let (base, place) = (0x1_4000_0000usize, 0x1_3ff0_0000usize);
        for site in sites() {
            let window = windows.get(&site.window).unwrap_or_else(|| {
                panic!("{}: no window of the title's at {:#x}", site.name, site.window)
            });
            assert_eq!(window.len(), usize::from(site.len), "{}", site.name);
            assert_eq!(&site.window(window).unwrap(), window, "{}", site.name);
            for at in 0..window.len() {
                let mut other = window.clone();
                other[at] ^= 0x01;
                assert!(site.window(&other).is_none(), "{}: byte {at}", site.name);
            }
            let at = base + site.window + site.span;
            let code = stub(&site, window, at, place + 0x100, 0x7ff6_1234_5678, place, place + 0x80)
                .unwrap_or_else(|| panic!("{}", site.name));
            let back = i32::from_le_bytes(code[code.len() - 4..].try_into().unwrap()) as i64;
            assert_eq!(
                (place + 0x100 + code.len()) as i64 + back,
                (at + site.span_len()) as i64,
                "{}",
                site.name
            );
        }
    }

    /// A window whose TLS-array load `crate::tls` redirected (the title's `mov rax, gs:[58h]`
    /// made `call thunk; nop4`) is the title's still: its stub calls the same thunk through the
    /// register the load loaded, and is otherwise the stub of the title's bytes. Over the
    /// fixtures, and over the tables wherever a private copy of the title's bytes is at hand.
    #[test]
    fn tls_redirected_loads_are_the_titles_and_call_the_same_thunk() {
        let mut met = 0;
        for (site, window) in fixtures() {
            met += redirected(&site, &window) as usize;
        }
        for (site, window) in table_fixtures() {
            met += redirected(&site, &window) as usize;
        }
        assert!(
            met >= 1,
            "a window with a TLS-array load among the fixtures (the tables have them at +0x399cdf,
             +0x399d03 and +0x399e04)"
        );
    }

    /// The title's windows of the tables, where they are at hand (`VICTORY_HEAP_WINDOWS`): empty
    /// without them, so a test over them runs the fixtures alone.
    fn table_fixtures() -> Vec<(Site, Vec<u8>)> {
        let Some(windows) = title_windows() else {
            return Vec::new();
        };
        sites()
            .into_iter()
            .filter_map(|s| windows.get(&s.window).map(|w| (s, w.clone())))
            .collect()
    }

    /// `site` with its window made the way `crate::tls` leaves it (every TLS-array load a call of
    /// its thunk, 32 bytes apart): that it is the title's window still, that a window which is
    /// not its redirect is not, and that its stub is the stub of the title's bytes with the load
    /// made a call of the same thunk. Whether the window had a load to redirect at all.
    fn redirected(site: &Site, window: &[u8]) -> bool {
        let (base, place, thunks) = (0x1_4000_0000usize, 0x1_3ff0_0000usize, 0x1_3fe0_0000usize);
        // The image as `crate::tls` leaves it (thunk per register, 32 bytes each).
        let mut found = window.to_vec();
        let mut loads = vec![];
        let mut i = 0;
        while i + 9 <= found.len() {
            if let Some(reg) = tls_load(&found[i..]) {
                let thunk = thunks + 32 * usize::from(reg);
                let rel = (thunk as i64 - (base + site.window + i + 5) as i64) as i32;
                found[i] = 0xe8;
                found[i + 1..i + 5].copy_from_slice(&rel.to_le_bytes());
                found[i + 5..i + 9].copy_from_slice(&[0x0f, 0x1f, 0x40, 0x00]);
                loads.push((i, reg, thunk));
                i += 9;
            } else {
                i += 1;
            }
        }
        if loads.is_empty() {
            return false;
        }
        assert_eq!(&site.window(&found).unwrap(), window, "{}", site.name);
        let mut other = found.clone();
        other[loads[0].0 + 5] = 0x90;
        assert!(
            site.window(&other).is_none(),
            "{}: not the redirect's nop",
            site.name
        );
        let at = base + site.window + site.span;
        let (to, cells, mirror) = (place + 0x100, 0x7ff6_1234_5678, place + 0x80);
        let plain = stub_with(site, window, window, at, to, cells, place, mirror, 0).unwrap();
        let redirected = stub_with(site, window, &found, at, to, cells, place, mirror, 0)
            .unwrap_or_else(|| panic!("{}", site.name));
        // A call of the thunk is longer than the load it replaces, and the stub's own branches
        // reach across it: what the two stubs owe each other is the sequences, and the length.
        let holds = |code: &[u8], seq: &[u8]| code.windows(seq.len()).any(|w| w == seq);
        let mut longer = 0;
        for &(i, reg, thunk) in &loads {
            if !(site.span..site.span + site.span_len()).contains(&i) {
                continue; // after the span: neither stub holds the load
            }
            let mut call = vec![0x48 | reg >> 3, 0xb8 | reg & 7];
            call.extend((thunk as u64).to_le_bytes());
            if reg >= 8 {
                call.push(0x41);
            }
            call.extend([0xff, 0xd0 | reg & 7]);
            assert!(
                holds(&plain, &tls_load_insn(reg)[..]),
                "{}: the plain stub holds the title's load",
                site.name
            );
            assert!(
                holds(&redirected, &call),
                "{}: the redirected stub calls the same thunk",
                site.name
            );
            assert!(
                !holds(&plain, &call),
                "{}: the plain stub does not",
                site.name
            );
            longer += call.len() - 9;
        }
        assert_eq!(redirected.len(), plain.len() + longer, "{}", site.name);
        for (code, what) in [(&plain, "as the title has it"), (&redirected, "redirected")] {
            // Back to the title's code after the span, from each stub's own end.
            let back = i32::from_le_bytes(code[code.len() - 4..].try_into().unwrap()) as i64;
            assert_eq!(
                (to + code.len()) as i64 + back,
                (at + site.span_len()) as i64,
                "{}: {} jumps back",
                site.name,
                what
            );
        }
        true
    }

    /// Each hooked site run as code in memory of a test's, twice, once as the
    /// title has it and once hooked: what the hooked one writes goes to the
    /// other copy of the heap that the cells name, the notes are made, the
    /// registers and the flags come out the same.
    #[cfg(windows)]
    #[test]
    fn hooked_stores_write_through_our_view_and_note_their_slots() {
        // A run needs the code itself, which is not in this repository.
        if title_windows().is_none() {
            return;
        }
        stub_tests::run();
    }
}

/// Executed tests of the stubs (Windows: the calls use its conventions).
#[cfg(all(test, windows))]
mod stub_tests {
    use super::*;

    /// The cells, as `victory-gpu` lays them out.
    #[repr(C)]
    struct Cells {
        base: u64,
        len: u64,
        deltas: *mut u64,
        slots: *mut u8,
        blocks: *mut u8,
        told: *mut u8,
        any: u8,
        _pad: [u8; 7],
    }

    const _: () = assert!(size_of::<Cells>() == CELLS_LEN);

    /// What the stubs' call was told last, and what it answers.
    static ASKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
    static ANSWER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    extern "system" fn resolve(block: u64) -> u64 {
        ASKED.store(block, std::sync::atomic::Ordering::Relaxed);
        ANSWER.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The pool hooks' link, as `victory-gpu` makes it when the slot lies in
    /// none of its regions: the store at the title's address.
    extern "system" fn link(at: u64, value: u32) {
        // SAFETY: the test's title memory, which the site's store writes.
        unsafe { (at as *mut u32).write_unaligned(value) };
    }

    /// The registers a run starts with and ends with (by number; the stack
    /// pointer's place unused), the flags, and ymm0 to ymm15.
    #[repr(C)]
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    struct State {
        regs: [u64; 16],
        flags: u64,
        ymm: [u8; 512],
    }

    fn alloc(len: usize, exec: bool) -> usize {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE, PAGE_READWRITE, VirtualAlloc,
        };
        // SAFETY: fresh memory of the test's.
        let at = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                len,
                MEM_COMMIT | MEM_RESERVE,
                if exec {
                    PAGE_EXECUTE_READWRITE
                } else {
                    PAGE_READWRITE
                },
            )
        } as usize;
        assert_ne!(at, 0);
        at
    }

    /// `fn(state: *mut State)` at `at`: loads the state, runs `span` (which
    /// is placed where the returned offset says), and stores the state back.
    fn harness(span: &[u8]) -> (Vec<u8>, usize) {
        let mut code = vec![
            0x53, 0x56, 0x57, 0x55, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57,
        ]; // push rbx..r15
        // xmm6-15 are the caller's (Windows' convention): kept, as the
        // registers above are. Loading ymm6-9 for the span changes them.
        code.extend([0x48, 0x81, 0xec, 0xa0, 0, 0, 0]); // sub rsp, 160
        for k in 6u8..16 {
            // vmovdqu [rsp + 16(k-6)], xmmk
            code.extend([
                0xc5,
                if k < 8 { 0xfa } else { 0x7a },
                0x7f,
                0x84 | (k & 7) << 3,
                0x24,
            ]);
            code.extend((16 * u32::from(k - 6)).to_le_bytes());
        }
        code.push(0x51); // push rcx
        code.extend([0xff, 0xb1, 0x80, 0, 0, 0, 0x9d]); // push qword [rcx+128] ; popfq
        for k in 0u8..16 {
            // vmovdqu ymmk, [rcx + 136 + 32k]
            code.extend([
                0xc5,
                if k < 8 { 0xfe } else { 0x7e },
                0x6f,
                0x81 | (k & 7) << 3,
            ]);
            code.extend((136 + 32 * u32::from(k)).to_le_bytes());
        }
        for reg in (0u8..16).filter(|&r| r != 1 && r != 4) {
            // mov reg, [rcx + 8*reg]
            code.extend([0x48 | (reg >> 3) << 2, 0x8b, 0x81 | (reg & 7) << 3]);
            code.extend((8 * u32::from(reg)).to_le_bytes());
        }
        code.extend([0x48, 0x8b, 0x49, 0x08]); // mov rcx, [rcx+8]
        // 128 bytes of zeros on the stack, for spans that load from it.
        for _ in 0..16 {
            code.extend([0x6a, 0x00]); // push 0
        }
        let at = code.len();
        code.extend(span);
        code.extend([0x48, 0x8d, 0xa4, 0x24, 0x80, 0, 0, 0]); // lea rsp, [rsp+128]
        code.extend([0x51, 0x48, 0x8b, 0x4c, 0x24, 0x08]); // push rcx ; mov rcx, [rsp+8]
        for reg in (0u8..16).filter(|&r| r != 1 && r != 4) {
            // mov [rcx + 8*reg], reg
            code.extend([0x48 | (reg >> 3) << 2, 0x89, 0x81 | (reg & 7) << 3]);
            code.extend((8 * u32::from(reg)).to_le_bytes());
        }
        code.extend([0x9c, 0x58, 0x48, 0x89, 0x81, 0x80, 0, 0, 0]); // pushfq ; pop rax ; mov [rcx+128], rax
        code.extend([0x58, 0x48, 0x89, 0x41, 0x08]); // pop rax ; mov [rcx+8], rax
        for k in 0u8..16 {
            // vmovdqu [rcx + 136 + 32k], ymmk
            code.extend([
                0xc5,
                if k < 8 { 0xfe } else { 0x7e },
                0x7f,
                0x81 | (k & 7) << 3,
            ]);
            code.extend((136 + 32 * u32::from(k)).to_le_bytes());
        }
        code.push(0x59); // pop rcx
        for k in 6u8..16 {
            // vmovdqu xmmk, [rsp + 16(k-6)]
            code.extend([
                0xc5,
                if k < 8 { 0xfa } else { 0x7a },
                0x6f,
                0x84 | (k & 7) << 3,
                0x24,
            ]);
            code.extend((16 * u32::from(k - 6)).to_le_bytes());
        }
        code.extend([0x48, 0x8d, 0xa4, 0x24, 0xa0, 0, 0, 0]); // lea rsp, [rsp+160]
        code.extend([
            0x41, 0x5f, 0x41, 0x5e, 0x41, 0x5d, 0x41, 0x5c, 0x5d, 0x5f, 0x5e, 0x5b,
        ]); // pop r15..rbx
        code.extend([0xc5, 0xf8, 0x77, 0xc3]); // vzeroupper ; ret
        (code, at)
    }

    /// 32 blocks of title memory, as many of ours, and the cells over them.
    struct Heap {
        title: usize,
        ours: usize,
        slots: Vec<u8>,
        blocks: Vec<u8>,
        told: Vec<u8>,
        deltas: Vec<u64>,
        cells: Box<Cells>,
        /// The heap's first byte and length as the stubs compare them.
        mirror: Box<[u64; 2]>,
        /// What the stubs' call was told in the run.
        asked: u64,
    }

    const LEN: usize = 32 << 16;
    const PLAIN_LEN: usize = 0x80000;

    /// The title's blocks and ours, the same for every run: registers and
    /// stored values that are addresses in them are then the same in each;
    /// and memory for the bases of a generated span's plain instructions.
    fn memory() -> (usize, usize, usize) {
        static MEMORY: std::sync::OnceLock<(usize, usize, usize)> = std::sync::OnceLock::new();
        *MEMORY.get_or_init(|| {
            (
                alloc(LEN, false),
                alloc(LEN, false),
                alloc(PLAIN_LEN, false),
            )
        })
    }

    /// Whether `site` is the generator's (`frame_users`).
    fn generated(site: &Site) -> bool {
        site.name.starts_with("frame user")
    }

    impl Heap {
        /// The cells over [`memory`], whose bytes are made as at the start of a
        /// run: zeros and a word a node site loads, or (`pointers`, for the
        /// generated sites) a pointer into the title's memory everywhere, which
        /// a load of an object pointer finds.
        fn new(len: u64, pointers: bool) -> Heap {
            let (title, ours, plain) = memory();
            let middle = (title + LEN / 2) as u64;
            for at in [title, ours] {
                // SAFETY: the test's memory.
                unsafe { std::ptr::write_bytes(at as *mut u8, 0, LEN) };
                if pointers {
                    // SAFETY: as above.
                    unsafe { std::slice::from_raw_parts_mut(at as *mut u64, LEN / 8) }.fill(middle);
                } else {
                    // A word the node site loads (rcx + rax + 48h).
                    // SAFETY: inside the test's memory.
                    unsafe {
                        ((at + 0x10040 + 0x60 + 0x48) as *mut u64).write(0x0bad_cafe_f00d_d00d)
                    };
                }
            }
            // SAFETY: as above.
            unsafe { std::slice::from_raw_parts_mut(plain as *mut u64, PLAIN_LEN / 8) }
                .fill(middle);
            let mut heap = Heap {
                title,
                ours,
                slots: vec![0; LEN / 32 + 1],
                blocks: vec![0; (LEN / 0x10000 + 1) * LINE],
                told: vec![0; (LEN / 0x10000 + 1) * LINE],
                // The even blocks known, the odd ones asked for.
                deltas: (0..=LEN / 0x10000)
                    .map(|b| {
                        if b % 2 == 0 {
                            ours.wrapping_sub(title) as u64
                        } else {
                            0
                        }
                    })
                    .collect(),
                cells: Box::new(Cells {
                    base: title as u64,
                    len,
                    deltas: std::ptr::null_mut(),
                    slots: std::ptr::null_mut(),
                    blocks: std::ptr::null_mut(),
                    told: std::ptr::null_mut(),
                    any: 0,
                    _pad: [0; 7],
                }),
                mirror: Box::new([0; 2]),
                asked: u64::MAX,
            };
            heap.cells.deltas = heap.deltas.as_mut_ptr();
            heap.cells.slots = heap.slots.as_mut_ptr();
            heap.cells.blocks = heap.blocks.as_mut_ptr();
            heap.cells.told = heap.told.as_mut_ptr();
            heap.mirror = Box::new([heap.cells.base, heap.cells.len]);
            heap
        }

        fn bytes(at: usize) -> Vec<u8> {
            // SAFETY: the test's memory.
            unsafe { std::slice::from_raw_parts(at as *const u8, LEN) }.to_vec()
        }

        /// The registers `site`'s stores land in the title's two blocks with.
        fn state(&self, site: &Site) -> State {
            let t = self.title as u64;
            let mut regs = [0u64; 16];
            regs[0] = 0x60; // rax
            regs[1] = t + 0x10040; // rcx: block 1
            regs[2] = 0x1122_3344_5566_7788; // rdx
            regs[3] = t + 0x308; // rbx
            regs[5] = 0x55; // rbp
            regs[6] = 0x60; // rsi
            regs[7] = t + 0x200; // rdi
            regs[8] = 0x20;
            regs[9] = t + 0x40;
            regs[10] = t + 0x10100;
            regs[11] = 0xabcdef;
            regs[12] = 0x12;
            regs[13] = 0x13;
            regs[14] = t + 0x418;
            regs[15] = 0x1000;
            // The registers some sites' stores go through.
            let over: &[(usize, u64)] = match site.window {
                0x1228e4 => &[(0, 0x10200)],
                0x325bbf3 => &[(6, 0x600)],
                0x325be0c => &[(0, 0x700)],
                0x399c96 => &[(15, 0x800)],
                0x5574df1 => &[(0, 0x10400)],
                0x12639f => &[(2, 0x900)],
                0x13b3def => &[(2, 0xa00)],
                0x555e3ad | 0x555e3c0 => &[(0, 0x10000)],
                0x18853e | 0x2474b9e => &[(0, 0xb00)],
                0x188554 | 0x2474bb4 => &[(0, 0xc00)],
                0x2474a42 | 0x2474a62 => &[(2, 0xd00)],
                0x4e9b4fa | 0x4e9b580 => &[(6, 0xe00)],
                _ => &[],
            };
            for &(reg, offset) in over {
                regs[reg] = t + offset;
            }
            // The special pool's switch: its fresh slot in rsi.
            if site.name.starts_with("special pool slot index") {
                regs[6] = t + 0x600;
            }
            if generated(site) {
                let setup = frame_users::SETUP
                    .iter()
                    .find(|s| s.0 == site.window)
                    .map_or(&[][..], |s| s.1);
                for &(reg, kind, value) in setup {
                    regs[reg as usize] = match kind {
                        1 => t + u64::from(value),
                        2 => memory().2 as u64 + u64::from(value),
                        _ => u64::from(value),
                    };
                }
            }
            State {
                regs,
                flags: 0xad7,
                ymm: std::array::from_fn(|k| (k as u8).wrapping_mul(7) ^ 0xa5),
            }
        }
    }

    type Run = unsafe extern "system" fn(*mut State);

    /// Fails with `what` and the first offsets where `a` and `b` differ, with
    /// their bytes (the arrays are 128 KiB, nearly all zeros).
    fn same(a: &[u8], b: &[u8], what: &str) {
        let differ: Vec<String> = (0..a.len().max(b.len()))
            .filter(|&k| a.get(k) != b.get(k))
            .take(12)
            .map(|k| format!("{k:#x}: {:02x?} / {:02x?}", a.get(k), b.get(k)))
            .collect();
        assert!(differ.is_empty(), "{what}: differ at {}", differ.join(", "));
    }

    /// `window` with the site's loads relative to the instruction pointing at `zeros`, when
    /// its span lies in the harness placed at `place`: the bytes the row's tag stands for,
    /// moved to where this test runs them.
    fn retarget(site: &Site, window: &[u8], place: usize, zeros: usize) -> Vec<u8> {
        let (_, at) = harness(&window[site.span..site.span + site.span_len()]);
        let mut bytes = window.to_vec();
        let mut offset = site.span;
        for insn in site.insns {
            if let Insn::Relative(n, disp) = *insn {
                let end = place + at + (offset - site.span) + n;
                let rel = i32::try_from(zeros as i64 - end as i64).unwrap();
                bytes[offset + disp..offset + disp + 4].copy_from_slice(&rel.to_le_bytes());
            }
            offset += insn.len();
        }
        bytes
    }

    /// Runs `site`'s span (over `window`, the title's bytes there) as the title has it, and
    /// hooked with the heap's cells on and off: (state, title bytes, our bytes) of each.
    fn runs(
        site: &Site,
        window: &[u8],
        code: usize,
        slow_at: usize,
    ) -> [(State, Vec<u8>, Vec<u8>, Heap); 3] {
        std::array::from_fn(|n| {
            let mut heap = Heap::new(if n == 2 { 0 } else { LEN as u64 }, generated(site));
            // (Room for the longest stubs, some kilobytes, after each.)
            let place = code + 0x8000 * (n + 1);
            // The window's bytes, as `apply` would have read them from the image.
            let window = &retarget(site, window, place, code + 0x3f000);
            let span = &window[site.span..site.span + site.span_len()];
            // (A span too short for the jump, a case of the special pool's switch, which the
            // title reaches through its table: five nops before it, where the hooked run jumps
            // to the stub, which goes back after the span.)
            let short = span.len() < 5;
            let padded: Vec<u8> = if short {
                [0x90; 5].iter().chain(span).copied().collect()
            } else {
                span.to_vec()
            };
            let (body, at) = harness(&padded);
            let at_span = at + if short { 5 } else { 0 };
            // SAFETY: the test's code memory, written before it runs.
            unsafe { std::ptr::copy_nonoverlapping(body.as_ptr(), place as *mut u8, body.len()) };
            if n > 0 {
                let to = place + 0x800;
                let stub = stub_with(
                    site,
                    window,
                    window,
                    place + at_span,
                    to,
                    &raw const *heap.cells as usize,
                    slow_at,
                    heap.mirror.as_ptr() as usize,
                    link as *const () as usize,
                )
                .unwrap();
                let jump = crate::console_insn::jump(place + at, to).unwrap();
                // SAFETY: as above.
                unsafe {
                    std::ptr::copy_nonoverlapping(stub.as_ptr(), to as *mut u8, stub.len());
                    std::ptr::write_bytes((place + at) as *mut u8, 0x90, padded.len());
                    std::ptr::copy_nonoverlapping(jump.as_ptr(), (place + at) as *mut u8, 5);
                }
            }
            ANSWER.store(
                heap.ours.wrapping_sub(heap.title) as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
            ASKED.store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
            let mut state = heap.state(site);
            // SAFETY: the code just written, of the harness's signature.
            unsafe { std::mem::transmute::<usize, Run>(place)(&mut state) };
            heap.asked = ASKED.load(std::sync::atomic::Ordering::Relaxed);
            let (title, ours) = (Heap::bytes(heap.title), Heap::bytes(heap.ours));
            (state, title, ours, heap)
        })
    }

    pub(super) fn run() {
        let code = alloc(0x40000, true);
        let slow_at = code + 0x20000;
        let body = slow(slow_at, resolve as *const () as usize).unwrap();
        // SAFETY: the test's code memory.
        unsafe { std::ptr::copy_nonoverlapping(body.as_ptr(), slow_at as *mut u8, body.len()) };
        // (The generated sites the generator's harness could run with its setup.)
        // (And the pool hooks' places.)
        let pools = pool_users::LINKS
            .iter()
            .chain(pool_users::SPECIAL)
            .copied()
            .chain(pool_users::CASES.iter().map(|c| c.1));
        let windows = super::tests::title_windows().expect("the title's windows, for this test");
        let mut missing = 0;
        for site in sites()
            .into_iter()
            .filter(|s| !generated(s) || frame_users::TESTED.contains(&s.window))
            .chain(pools)
        {
            let Some(window) = windows.get(&site.window) else {
                missing += 1; // (a window the private copy of them does not have)
                continue;
            };
            let [
                (plain, title_plain, _, before),
                (hooked, title_hooked, ours_hooked, heap),
                (off, title_off, ours_off, quiet),
            ] = runs(&site, window, code, slow_at);
            let name = site.name;
            let initial = Heap::bytes(Heap::new(0, generated(&site)).title);
            // (Every run has the same title memory; an address in it is
            // compared as its offset there all the same.)
            let status = |s: &State, h: &Heap| State {
                regs: s.regs.map(|r| {
                    let off = r.wrapping_sub(h.title as u64);
                    if off < 2 * LEN as u64 {
                        off | 1 << 63
                    } else {
                        r
                    }
                }),
                flags: if site.flags_live { s.flags & 0x8d5 } else { 0 },
                ..*s
            };
            // Registers, ymm0-9 and (where they are read after) the flags as the title's.
            assert_eq!(
                status(&hooked, &heap),
                status(&plain, &before),
                "{name}: hooked"
            );
            if site
                .insns
                .iter()
                .any(|i| matches!(i, Insn::Store(_, Note::Link)))
            {
                // A free list's link: the call, as the title's store, nothing noted.
                same(
                    &title_hooked,
                    &title_plain,
                    &format!("{name}: the link at the title's address"),
                );
                assert!(
                    heap.slots.iter().all(|&b| b == 0) && heap.cells.any == 0,
                    "{name}: nothing noted"
                );
                continue;
            }
            assert_eq!(
                status(&off, &quiet),
                status(&plain, &before),
                "{name}: hooked, no heap"
            );
            // Hooked: the title's bytes untouched, ours as the title's would be.
            same(
                &title_hooked,
                &initial,
                &format!("{name}: the title's memory is not written"),
            );
            same(
                &ours_hooked,
                &title_plain,
                &format!("{name}: our view has what the title's would"),
            );
            // With no heap in the cells: the store as before, nothing noted.
            same(
                &title_off,
                &title_plain,
                &format!("{name}: no heap, the title's address"),
            );
            same(&ours_off, &initial, &format!("{name}: no heap, not ours"));
            assert!(
                quiet
                    .slots
                    .iter()
                    .chain(&quiet.blocks)
                    .chain(&quiet.told)
                    .all(|&b| b == 0)
                    && quiet.cells.any == 0,
                "{name}: no heap, no notes"
            );
            // The notes: every slot whose bytes changed, its block, the flag; each with the site's note.
            for slot in (0..LEN)
                .filter(|&k| title_plain[k] != initial[k])
                .map(|k| k / 32)
            {
                assert_ne!(heap.slots[slot], 0, "{name}: slot {slot} noted");
                assert_eq!(
                    (
                        heap.blocks[((slot * 32) >> 16) * LINE],
                        heap.told[((slot * 32) >> 16) * LINE]
                    ),
                    (1, 1),
                    "{name}: its block"
                );
            }
            assert_eq!(heap.cells.any, 1, "{name}");
            let stores = site
                .insns
                .iter()
                .filter(|i| matches!(i, Insn::Store(..)))
                .count();
            let notes: Vec<u8> = site
                .insns
                .iter()
                .filter_map(|i| match *i {
                    Insn::Store(_, note) => Some(note as u8),
                    Insn::Plain(_) | Insn::Relative(..) => None,
                })
                .collect();
            let noted: Vec<u8> = heap.slots.iter().copied().filter(|&s| s != 0).collect();
            assert!(
                !noted.is_empty()
                    && noted.len() <= 2 * stores
                    && noted.iter().all(|s| notes.contains(s)),
                "{name}: {noted:?}"
            );
            // Block 1's way to our view was asked for where a store went there.
            let second = (0x10000..0x20000).any(|k| title_plain[k] != initial[k]);
            if !generated(&site) {
                assert_eq!(
                    heap.asked,
                    if second { 1 } else { u64::MAX },
                    "{name}: asked"
                );
            }
        }
        assert_eq!(missing, 0, "windows of the tables the private copy does not have");
    }
}
