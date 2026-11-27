# Running under Linux (Wine / Proton)

`victory.exe` is a Windows program, but it runs under Wine with vkd3d-proton
(Direct3D 12 on Vulkan). Three opt-in switches make that work. Each one does
nothing unless it is set, so Windows runs are unchanged:

- `VICTORY_NO_WATCHDOG=1`: the title's watchdog ends the process when its main
  thread has not advanced for about 30 seconds. Wine's first frames are slow
  enough to trip it. The hook works only on the Series X 1.0.2.1 build, and
  only where the bytes are exactly the expected ones (`crates/victory-loader/src/hooks.rs`).
- `VICTORY_DXC=<folder with dxcompiler.dll and dxil.dll>`: the layer compiles
  a few HLSL shaders of its own. Wine's `d3dcompiler_47` refuses some of them,
  and for others it drops the root signature, and the world then stays black.
  With this set, those shaders are compiled with Microsoft's DXC first
  (`crates/victory-gpu/src/dxc.rs`). DXC 1.9.2609.5 was used.
- An empty pipeline library is now made from a null blob. This one is not a
  switch. vkd3d-proton refuses a non-null blob of length zero, so before this
  fix no pipeline was kept between runs under Wine.

## Tested setup

These steps were tested on one PC only: an RTX 5070 Ti 16 GB (NVIDIA 620.73.04
open modules), a Ryzen 7 9800X3D, 32 GB of RAM and CachyOS (Linux 7.3), with
GE-Proton 11-14's Wine and vkd3d-proton.

The title was Series X 1.0.2.1 at native 1440p with DLSS quality through
Streamline (`docs/dlss.md`). Once its pipelines were made, it averaged about
15 fps at 6,000 to 11,000 draws a frame, and about 12 fps above 11,000. Other
cards and drivers have not been tried.

## Building

Cross-build with [cargo-xwin](https://github.com/rust-cross/cargo-xwin):

```sh
rustup target add x86_64-pc-windows-msvc
cargo install --locked cargo-xwin
cargo xwin build --release -p victory --target x86_64-pc-windows-msvc
```

The program is `target/x86_64-pc-windows-msvc/release/victory.exe`.

## Running

Use the `wine` of a GE-Proton release directly. Give it a prefix that Proton
has made, so that vkd3d-proton, DXVK's `dxgi` and dxvk-nvapi are in it. Paths
handed to the program are Windows paths: Wine maps `/` to `Z:\`.

```sh
export WINEPREFIX=/path/to/prefix/pfx
export WINEDLLOVERRIDES="d3d12,d3d12core,dxgi=n,b;nvapi64=n"
export DXVK_ENABLE_NVAPI=1
# NVIDIA: with raw access chains, the layer's shaders faulted the GPU (Xid 13).
export VKD3D_DISABLE_EXTENSIONS=VK_NV_raw_access_chains
# Upload heaps in cached system memory: the per-frame copies went from 2.7 to 3.8 GB/s.
export VKD3D_CONFIG=no_upload_hvv,force_host_cached
export VICTORY_NO_WATCHDOG=1
export VICTORY_DXC='Z:\path\to\dxc'
# A few pipelines are never made under vkd3d-proton (below): keep the wait short.
export VICTORY_PIPELINE_WAIT_MS=24
/path/to/GE-Proton/files/bin/wine target/x86_64-pc-windows-msvc/release/victory.exe \
  run 'Z:\path\to\dump\game_scarlett_master.exe' --execute --console series-x
```

The tested runs used `run`, with the variables that `victory play` sets (its
`SETTINGS` in `apps/victory/src/play.rs`) exported by hand. `victory play`
itself has not been tried under Wine.

## Known problems

- About 7 of the title's compute pipelines are never made. Creating one
  overflows the stack right after vkd3d-proton takes the shader, even on a
  4 GB stack, so the recursion is endless. Most likely it happens in its
  shader translation, but no stack trace was taken. The thread that makes the
  pipeline dies, and the pipeline stays "being made", so its work is dropped.
- After a GPU fault, every later start died within the first frames until
  vkd3d-proton's pipeline cache (`vkd3d-proton.cache` in the working folder)
  was moved aside.
- Memory: on 32 GB of RAM, the free memory fell to about 4 GB during play.
