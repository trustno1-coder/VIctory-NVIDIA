# The title's ray-tracing structures and how they are built

Format memo for the native ray-tracing work (`VICTORY_RT_NATIVE=1`). It
collects in one place what the tree already knows, with where each fact
lives, and what the runs of 2026-11-26 added. Nothing here is taken from the
game's files: layouts are of interfaces and of what our own code reads.

## Who does what

- The title keeps its acceleration structures in plain title memory and
  walks them in its own compute programs. The only GPU help is one
  instruction that tests a ray against one node; the stack, instance
  transforms and hit logic are the program's code
  (`crates/victory-shader/src/bvh.rs:8-11`).
- The node test is lowered to DXIL arithmetic (`crates/victory-shader/src/emit/intersect.rs`)
  and has a CPU reference that matches it dword for dword
  (`crates/victory-shader/src/bvh.rs:1-6`).
- Structures are built by the console's builder library, which the title
  calls through seven functions of a DXR-shaped class. Its programs are
  compute shaders with a spilled user-data table
  (`crates/victory-shader/src/spill.rs`, `crates/victory-gpu/src/pipeline.rs`
  `of_builder`), 53 host pipelines in the runs here.
- With `VICTORY_RT_NATIVE=1` those seven calls pass through
  `crates/victory-hle/src/modules/bvh_builder.rs` (bound by
  `apps/victory/src/run.rs`, after the FidelityFX interposer). Today they are
  counted, timed and forwarded; this is the seam the native builder replaces
  call by call.

## Nodes

All from `crates/victory-shader/src/bvh.rs` (module documentation, lines
13-56):

| item | layout |
|---|---|
| node pointer | low 3 bits the kind, the rest the byte offset in 8-byte units; node at `(pointer >> 3) << 6` past the descriptor's base |
| kinds | 0-3 triangle 0-3 of a triangle node, 4 box node of 16-bit floats (64 B), 5 box node of 32-bit floats (128 B), 6 instance, 7 procedural |
| box node | four child pointers at bytes 0-15 (`0xffff_ffff` none), then four boxes min xyz, max xyz from byte 16 |
| triangle node | five vertices of three floats at bytes 0-59, a dword at 60 with one byte a triangle naming which vertices the barycentrics weigh; triangle `k` uses `TRIANGLE_VERTICES[k]` (line 76) |
| descriptor | 40-bit base in 256-byte units; bit 63 box sorting; bit 120 barycentric return; type 8 in dword 3 bits 31:28 (lines 78-85) |

## Structure header and instances

From `docs/fork-ray-tracing.md` (lines 69-81): dword 0 holds flags (0x8000
bottom level, 0x4 compacted, 0x2 rehydrated, 0x100 still packed), dword 1 the
size in 64-byte nodes, dword 3 the number of elements, 4-9 the bounds, 10 the
offset of the box nodes, 12 the root's node pointer, 13 the offset of the
elements, 14 the offset of the parent table. A top-level instance is 128
bytes at offset 13.

The static world's structures ship packed and are unpacked in place on the
GPU, once. Dynamic ones are built every frame or refitted, their compacted
size is stored into a small title buffer, and the title copies each to a
place of that size.

## The builder's interface

Signatures as the imports name them; structs in
`crates/victory-hle/src/modules/bvh_builder.rs`.

| call | arguments |
|---|---|
| constructor | device, one flag (0 in the runs here) |
| build, batched | command list, count, array of `BatchedDesc` |
| rehydrate, batched | command list, count, array of `BuildDesc`, array of one 64-bit value a structure |
| copy | command list, destination, source, mode |
| prebuild info | `Inputs`, a stream description (null in the calls seen), `PrebuildInfo` out |
| Morton width | 64-bit or 32-bit codes; set around every batch |

`Inputs` (24 B), `BuildDesc` (48 B) and `PrebuildInfo` (24 B) are Direct3D
12's own, with title addresses where a PC has GPU addresses.

`BatchedDesc` is not public. Read off traced calls (`VICTORY_RT_TRACE=1`,
run rt-1, every batch seen):

| offset | field |
|---|---|
| 0x00 | `BuildDesc`: destination, inputs, source (0 for a fresh build), scratch |
| 0x30 | u32 count of post-build facts asked (1), u32 zero |
| 0x38 | pointer to that many 16-byte `PostbuildDesc` (title address to store at, u32 kind) |
| 0x40 | two zero qwords |

What the calls looked like:

- Builds are of bottom levels with flags 0x0a, 0x0b or 0x06 (compaction
  allowed, plus fast build, update, or fast trace), 1 to 33 geometries, in
  batches of 16.
- Unpackings name the same address as destination and source (in place),
  flags 0x14, scratch 0. The second array's value is the destination's low
  32 bits under a different high part; what it names is not known yet.
- Prebuild sizes are computed on the CPU by the library (no command list).

The post-build pointer is the address the title later reads a compacted
size from. Knowing it per build, at the call, is what a fix of the size
race (`crates/victory-gpu/src/memory.rs`, `sizes_wait`) can stand on: the
call can be tied to its slot without guessing from a dispatch's root views.

## What the runs showed

- rt-1: the first builder dispatch made every builder pipeline one after
  another on the recording thread; the frame stood 32 s and the title's
  watchdog ended the run. Fixed: they go to the workers at creation
  (`pipeline/make.rs` `ahead`).
- rt-2: the builder ran (53 pipelines; some 215 structures built and 92
  unpacked a second while the world streamed in; 1.2 GiB held in 19 arenas).
  Frames then took 8 to 20 s and the watchdog ended the run again. The
  recording threads spent 36 s a frame against 95 ms without the builder;
  the fault budget (2,048 pages for a run) was used up while some 2,500
  pages a frame were still found without a copy. The cause, found in rt-4
  with the by-address line's time split by section: the twentieth arena
  could not be had (no hole of 64 MiB left in [4 GiB, 2^37), where the
  title's own memory lies) and every builder dispatch asked for it again,
  5 to 18 s of every frame under the module's mutex. Fixed: arenas fall
  back to anywhere below 2^40 and a failure backs off (`memory.rs`
  `another`, `upload.rs` `reserve_out_of_reach`).
- rt-3: the same settings, and the address space happened to have room: 25
  arenas, no failure, 75 ms a frame in the world at the end.
- rt-4: the builder's calls size the structures (`structure_extent`); the
  run ended as rt-2 did, at 19 arenas.
- rt-5: with the arena fix, 23 arenas and no failure. Root views sized by
  the builder's calls 150 to 430 a frame, by a guess 6 to 100; pages found
  without a copy 250 to 430 a frame (830 in rt-3), cause not found yet.
  About 300 ms a frame while the world's host pipelines were being made,
  66 to 79 ms after.

## Not known yet

- The geometry descriptions `Inputs.descs` points at on the console (the PC
  layout is assumed, not checked).
- The stream description of the prebuild call, and the copy modes used.
- The packed form of the static structures: until it is decoded their
  unpacking stays with the library's own programs.
- How the console rounds in the node test and orders equal entries
  (`crates/victory-shader/src/bvh.rs:65-69`).
