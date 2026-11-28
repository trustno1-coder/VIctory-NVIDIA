# Working on this code

For anyone changing this repository, person or coding agent. It is short on
purpose. Read it before the first edit, and read `docs/architecture.md` and
`docs/asset-policy.md` after it.

This layer runs a console title's own machine code on a PC and stands in for
the console's graphics and system interfaces. The title was written for a
machine where the CPU and the GPU share one memory. A PC with a discrete card
does not have that. Nearly every hard problem here, and nearly every slow
frame, comes down to one question:

**Where are the bytes, and who moves them?**

If you cannot answer that for the code you are about to write, you are not
ready to write it.

## Know your copies

A buffer of the title's can exist in up to four places: the title's own
memory, a copy of ours in PC memory, an upload heap, and the card. Each hop
between them is a memcpy somebody pays for every frame, and the hop to the
card crosses the bus.

Before adding code that touches such bytes, write down (in the comment, not in
your head):

1. Which of those places the bytes are in when your code runs.
2. Which hops your change adds, removes or repeats, and how many bytes a
   frame go over each.
3. Who reads the result: a shader on the card, the title's CPU, or us.

Rules that follow from it:

- **One move per byte, at most.** A byte that streams in from the title goes
  to the card once. It does not go title -> ours -> staging -> upload -> card
  because four people each added a convenient buffer.
- **Data the card made stays on the card.** If a shader wrote it and a shader
  reads it, the CPU has no business in between. Reading card memory back is
  for the few bytes the title's own CPU code reads, nothing else.
- **Work on GPU data is GPU work.** Write a compute pass and record it into
  the title's command list (`ambient.rs` shows the pattern). Do not map,
  loop on the CPU and upload again.
- **The bus is the cost.** A heap in PC memory that shaders read looks like
  "no copy" and is the slowest option on a discrete card: every load crosses
  the bus. The tree has the measurement (one list went from 50.5 ms to
  4.5 ms when its copies moved onto the card). "In place" and "on the card"
  are different things; say which one you mean.
- **No staging buffers by habit.** A `Vec` you fill and then copy out of is
  a copy you chose. Write into the destination.

## Cast, do not copy

The title hands us structures by address. They already have a layout.

- Describe it once: `#[repr(C)]` struct, fields named, a `const` size
  assertion under it. Then read it where it lies. No field-by-field
  `from_le_bytes` over a byte copy, no intermediate `Vec<u8>`.
- Title pointers are untrusted and unaligned: `read_unaligned`, null checks,
  bounded counts. Where the memory may not be mapped at all, read it through
  the fault-safe readers (`host::read_into`, `host::read_there`), once, into
  the buffer that is kept.
- A layout you worked out from a run goes in the struct's doc comment with
  how it was worked out. A guess is labelled a guess.
- Every `unsafe` block gets one `// SAFETY:` line that says why it holds.

## Threads

- **Never stall the title's threads.** The title has a watchdog: a frame
  that stands for 30 seconds ends the run, deliberately. Anything that can
  take long (shader translation, host pipeline creation, big allocations)
  goes to workers, started as early as its inputs are known, in parallel.
  A loop that makes fifty pipelines one after another on a recording thread
  is how a run was lost; do not write the next one.
- Take a lock, do the small thing, release it. No translation, no D3D12
  object creation, no kernel calls in a loop under a lock other threads
  need every frame.
- Know which thread you are on: the title records on several, we submit and
  land on others. State it in the function's doc comment when it matters.
- Per-frame work is budgeted in milliseconds. If yours is not free, it gets
  a number in the periodic stats line of its module.

## Fix the cause

- Patch a value where it is produced, not a copy of it on its way somewhere.
  A fix applied to our copy of the title's data leaves the title's own CPU
  code (culling, level of detail, anything derived) working from the old
  value, and the two disagree from then on.
- Do not paper over a missing implementation by answering success. A method
  that is not implemented fails honestly and logs what it was given, so the
  next person can write it.
- No placeholder data, no "simulate for now", no TODO left behind. Finish
  it or leave the old behaviour in place and say what is missing.
- Smallest change that is correct. Do not reformat or reorganise what you
  did not need to touch.

## Switches

- New behaviour comes behind a `VICTORY_*` environment variable, off by
  default, read once. With it unset the layer must behave exactly as before
  your change.
- Describe the switch in a comment where the code reads it. If a player
  should be able to change it, add it to the F1 menu (`host/menu.rs`).
- The launcher clears every `VICTORY_*` variable of the shell before it
  applies its own: test through `-More @{ VICTORY_X = '1' }`, not by
  exporting.

## Measure, then claim

- "Should be faster" is not a result. Put the number before and after in
  the comment or the stats line, and say which run it came from.
- Build loop: `cargo check -p victory-gpu`, the crate's tests, then
  `cargo build --release` and a real run through `scripts\play-nvidia.ps1`.
  Each run writes a log under `%LOCALAPPDATA%\victory\logs`; read it.
- Nothing is done until it has run in the game. Say plainly what you
  verified in a run and what you only compiled.
- Tests use bytes you construct. See the next section for why.

## Nothing of the game in the repository

`docs/asset-policy.md` is not optional. No captured bytes, shader hashes
from a capture, disassembly, memory dumps or screenshots in the tree, in
tests, or in comments. Work on such data outside the repository and bring
back only what you learned, in your own words.

## Comments and names

- Plain lowercase prose that explains why, in the voice the files already
  have. No exclamation marks, no sales language, no banners.
- Comment where the reasoning is not obvious from the code, and at every
  layout, lock and thread boundary. A run that taught something gets a
  sentence where the code changed because of it.
- Before naming or adding anything, search for what exists. Do not invent
  an include path, a helper or a second type for something the tree
  already has.

## For coding agents in particular

- Read the files you are about to change, and the callers. Do not edit
  from a guess about what a function does.
- Do not launch the game, kill it, or run a release build while someone is
  playing; the running game holds the binary.
- Other people have uncommitted work in the tree. Do not revert, stash or
  check out over anything you did not write.
- Report what failed as plainly as what worked.
