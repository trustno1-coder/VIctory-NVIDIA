# Pipelines the title hands over in loose parts (device methods 0x1c8 and 0x218)

The title calls two console device methods as `(this, description, iid, out)` and
asks for an `ID3D12PipelineState`. Until these were read, VIctory answered
`E_NOTIMPL` and the title went without those pipelines, silently: the draws and
dispatches of every one of them were dropped (the "dropped N without a host
pipeline" figure of the frame line). `VICTORY_DUMP_PSO=<dir>` saves the first 600
calls of each method as `<slot>-<call>.bin` (the description's 128 bytes, then for
every (address, size) pair its address, size and bytes), and the log of an
unimplemented call shows the first bytes of each part (`pairs=`).

The description is a root signature pointer followed by (address, size) pairs of
parts the title keeps together in its own pipeline cache.

## 0x218: a compute pipeline (read)

Five pairs: registers (0x1c bytes), hash (0x14), GFX10 code, program metadata, then
the registers again. They are the entries of an `FSPX` part, kinds 0, 1, 6 and 7
(`victory_shader::native::fspx_program` documents them), so `create_loose_compute`
puts them into one, in a container with one `ACCC` part, and the pipeline goes on
as any other. `VICTORY_LOOSE_COMPUTE=0` goes back to answering `E_NOTIMPL`.

Seen in 600 dumps: every one reads. Their metadata has root constants (resource
type 2, root type 0, 4 dwords at `b500`...) which `fspx_program` had no place for;
they are root parameters of as many user SGPRs as the descriptor dwords field says.

Of the 600, 204 translate. What stops the others, by number of programs: an address
with two computed terms (132; `Address::dynamic` holds one), an image store of some
channels (65; refused on purpose, a store of two channels says nothing about the
view), an address not from a root parameter (52), a lane mask whose lanes depend on
its own value (42), a structured buffer of unknown stride (24), a typed buffer
store of some channels or at an offset (21).
(`VICTORY_TEST_PSO_DIR=<dir> cargo test --release -p victory-gpu --lib -- --ignored
--nocapture loose_compute_dumps_translate` counts them again.)

## 0x1c8: a graphics pipeline (not read)

Six pairs, in this order:

| pair | size | what it is |
|------|------|------------|
| 1 | 0x17c to 0x18c | the pipeline's registers, as kind 0 is for compute but larger (first dword `0x4005d808`-like) |
| 2 | 0x78 to 0xbc | the input layout: semantic names (`POSITION`, `COLOR`, `TEXCOORD`) with their offsets |
| 3 | 0x140 to 0x300, a multiple of 0x40 | GFX10 code of one stage, zero padded; a 0x20 byte head before the first instruction |
| 4 | 0x70 to 0x130 | its metadata: `u32 2, u32 size - 0xc, u32 code dwords, u8 0x40, u8 first user data dword, u8 count of 0x41 records, u8 0`, then 32 byte records from +0x10 with the tags of `FSPX` (0x41 resource, 0x42 view, 0x43 hash) |
| 5 | as 3 | GFX10 code of the other stage |
| 6 | as 4 | its metadata |

Pairs 1, 2, 4 and 6 lie one after the other in the title's memory; the code pairs
lie elsewhere. The code of pair 3 reads `v_interp` (a pixel program), pair 5 loads
through typed buffers (a vertex program).

Pair 1 (97 dwords, 0x184 bytes; 0x18c and 0x17c also seen) reads as GFX10 context
registers: across 281 dumps only 35 dwords vary, and those that do are what a
pipeline state varies in. Dwords 52 to 59 are eight values of `0x80000000` (the
`CB_BLENDn_CONTROL` of eight render targets with blending off, bit 31 being
`DISABLE_ROP3`) and dwords 64 to 71 eight of `0x06000600` (`SX_MRTn_BLEND_OPT`).
The order of the registers is not known yet, so the render target formats, depth
and stencil, rasteriser and topology cannot be read from it.

What is missing to build the two `NativeProgram`s: the stage data `FSSM` records
carry (a vertex program's inputs, with the VGPR of each, and outputs; a pixel
program's inputs with the `v_interp` attribute of each, `SPI_PS_INPUT_ADDR` and
`DB_SHADER_CONTROL`). Here they would have to come from pair 1's register values
and pair 2's layout, which are not decoded. Not done: a wrong guess draws garbage
or hangs the card, and there is no way to look at the result without playing.
