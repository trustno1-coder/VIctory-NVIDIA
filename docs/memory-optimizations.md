# RAM and VRAM optimizations

Base revision `df2daafeb1b2170931cf7a6eddff6de254841d9c`. The changes apply to the Windows D3D12 backend and the NVIDIA Streamline integration.

Run the standalone build with `victory-memory.exe play "<full path to game_scarlett_master.exe>" --set VICTORY_LEVELS_ON_DEMAND=1` to use the experimental streaming path. The earlier idle-pool changes apply to both modes.

## Texture streaming

The path uses the existing D3D12 tiled resource capability check. Render targets, depth textures, unsupported texture shapes and adapters without Tier 2 tiled resources retain their existing allocation paths.

1. **Create actual D3D12 reserved resources for sparse textures.** The ordinary texture pool previously ran first and could return a fully backed placed resource for a sparse request. The sparse creation path now bypasses that pool and reaches `CreateReservedResource`, which starts with no heap pages mapped. The regression test distinguishes the two resource types through `GetHeapProperties`. [Microsoft D3D12 documentation](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nf-d3d12-id3d12resource-getheapproperties)
2. **Use compact host textures alongside sparse textures.** When the title's registered views start at a coarser mip and that mip can legally start a D3D12 texture, the planner builds a compact resource and its smaller upload footprint. Previously, enabling sparse textures disabled compact textures globally, including for placements that could use the compact path.
3. **Defer physical backing until the first upload.** Eligible large placed textures now start sparse even when all title pages are mapped when the resource is created. The current pages and views are evaluated at upload time. The allocation plan can therefore stream a coarse mip first even when title memory is already full or changes shortly after creation.
4. **Admit sparse mips in byte-sized steps.** Use the texture budget and reusable space in the tile pool to estimate the available bytes for each upload. Map the smallest mips first. If the next mip exceeds the allowance, retry the finer mips as space becomes available. GPU writes retain their required tile allocation path. The limit applies to mapped mip bytes. Backing heaps are still acquired in 64 MiB blocks.
5. **Publish only initialized sparse mips.** A staging buffer may begin partway down the chain even if the CPU sees finer mips in title memory. The layer now maps only the mips covered by that upload and tracks initialization separately from physical tile ownership. That distinction is necessary for packed mip tails where several mips share one tile.

### In-game streaming comparison

I ran the previous Windows executable and the streaming build on the RTX 5060, with copied saves and caches, Series S, DLSS Performance, 1440p output and 2× DLSS Frame Generation. Both executables received the same `VICTORY_LEVELS_ON_DEMAND=1`, startup-movie skip and scripted local-player inputs. Both reached the world after about 43 seconds. The table uses the nine samples from each run that showed at least 1,000 draws per frame between 5 and 15 seconds after the first frame-rate sample at that threshold. The peak covers the entire 100-second run.

| Measurement | Previous build | Streaming build | Reduction |
| --- | ---: | ---: | ---: |
| Median dedicated VRAM | 5,913 MiB | 5,196 MiB | 717 MiB |
| Peak dedicated VRAM | 6,124.5 MiB | 5,665.2 MiB | 459.3 MiB |
| Median shared GPU memory | 4,541 MiB | 4,050 MiB | 491 MiB |
| Median private commit | 17,625 MiB | 15,967 MiB | 1,658 MiB |
| Median process working set | 3,339 MiB | 2,988 MiB | 351 MiB |
| Median base frame rate | 3 fps | 3 fps | — |

This was one short startup-to-world comparison against the previous build using the same sparse switch. Both runs remained slow. The normal compact-only launch was observed separately without scripted inputs. A controlled comparison against that mode, longer traversal and detailed image-quality checks remain open. The streaming path stays opt-in.

### Streaming validation

The Windows software-adapter fixture passed the sparse budget and partial-upload cases. It verified that a budget limited to the coarsest group defers finer mips, and that a later upload can make those mips available. The resource factory regression test verified that an ordinary texture has heap properties and a sparse texture reports the D3D12 reserved-resource result. The debug-layer hardware fixture was skipped because Windows Graphics Tools was unavailable. The live title run used the RTX 5060's Tier 4 tiled-resource path.

One late game sample counted 239 sparse textures created and 854 tile-mapping calls. Of 329 MiB for all live sparse mips, 263 MiB had mappings. The backing pool itself occupied 320 MiB. The approximately 65 MiB without mappings describes mip coverage. It differs from the process's dedicated GPU usage and includes mips still waiting for budget.

## Earlier allocator changes

1. **Release DLSS Frame Generation allocations on explicit Off.** After switching the viewport off, VIctory calls Streamline's `slFreeResources` for DLSS-G. This also covers the case where a loading screen had already paused frame generation with its resources retained. Temporary pauses continue retaining the allocations so they can resume quickly. During swap-chain resizing, the layer updates its allocation bookkeeping after Streamline accepts Off. The first present after re-enabling can take longer while the plugin reallocates. [NVIDIA Streamline DLSS-G guide](https://github.com/NVIDIA-RTX/Streamline/blob/main/docs/ProgrammingGuideDLSS_G.md#64-disabling-frame-generation)
2. **Fit new allocations into partially occupied blocks first.** Both the GPU texture and CPU upload pools prefer a suitable partially used block over an empty block that is equally eligible for reuse. That leaves more whole blocks available to return to Windows. Alignment, GPU submission ordering, reference ownership, and the 2.5-second reuse delay still govern eligibility.
3. **Scale the empty texture heap cache to the adapter.** Below 85% of the DXGI local-memory budget, the cache can retain one 64 MiB block per GiB of budget, bounded to 2–64 blocks. With a 6 GiB budget that sets a 384 MiB retention ceiling, down from 4 GiB. At high usage, under host pressure, or without a budget sample, the limit is two blocks. These limits apply to fully empty heaps. Actual savings depend on the number of heaps that become empty.
4. **Keep the staging pool's RAM limit active during allocations.** Up to two empty 64 MiB upload blocks are kept on systems with up to 32 GiB of RAM. Host memory pressure lowers the allowance to one block. Critical commit pressure lowers it to zero. Allocations read the staging pool's own limit, so the same policy applies if the periodic sweep cannot immediately acquire the pool lock. Active uploads continue to follow the existing allocation path.

## Existing run

The machine has an RTX 5060 with 8 GiB of VRAM and approximately 15.1 GiB of usable physical RAM. An existing game log from the installed build reported 6,121 MiB of dedicated GPU usage and 5,809 MiB of shared GPU usage late in a Series S, DLSS Performance, 1440p output run with 2× Frame Generation. Near the same point, the layer counted 1,083 MiB of texture-heap capacity without a live owner. That allocator estimate includes space inside partially occupied heaps and pieces still waiting out the reuse delay. It cannot be read as immediately releasable VRAM. The shared DXGI figure and the process working set measure different aspects of memory use.

## Verification

### RTX 5060 Frame Generation test

I ran the installed build (`29d8dbf7d9892ceb8bec378a9dbc3c4f2931b745`) and the patched build with Streamline 2.14.16 and the built-in 2560×1440 `upscaler-check --device --present`. `VICTORY_FRAMEGEN_CYCLE=45` requested 2×, 4×, Off, and 6× in successive 45-present intervals. I sampled per-process Windows GPU memory counters and matched samples to the toggle timestamps. These measurements are from stand-in textures.

| Build | Dedicated VRAM before Off | Dedicated VRAM after Off settled | Shared GPU memory after Off |
| --- | ---: | ---: | ---: |
| Installed | 776.2 MiB | 776.2 MiB | 66.7 MiB |
| Patched standalone | 776.2 MiB | 406.5 MiB | 66.9 MiB |

The patched standalone build released **369.7 MiB of dedicated VRAM**. It successfully re-enabled Frame Generation, returned to 776.2 MiB, and completed all 180 presents. Streamline reported status `0x0` and shut down successfully. The synthetic run also showed a drop in private commit from 1,110.1 MiB to 962.8 MiB while off. That is a commit measurement, and the shared GPU counter stayed flat within the run. A separate diagnostic build reproduced the dedicated VRAM result.

### Game launch smoke test

Both patched builds launched the supplied title with Series S, DLSS Performance, 1440p output and DLSS Frame Generation. They rendered frames past the startup sequence using copied saves and caches. Windows reported physical memory pressure, and D3D12 declined the same pipeline allocation with `E_OUTOFMEMORY` in both runs. Frame delivery was uneven. The texture-heap and upload-cache reductions in live gameplay are still unquantified.

### Build and GPU tests

The final streaming build's GPU suite rerun reported 425 passed, 35 ignored, and the same two failures as the clean checkout, `indirect::tests::host_list_budget_starts_empty_at_each_submission` and `indirect::tests::warp_batch_charges_every_execution`. One earlier full run also failed `views_off_their_elements_get_a_copy_of_their_bytes`. That test passed by itself and in the full rerun. All nine pool tests and the streaming resource factory test passed in the rerun.

Build on Windows with Rust 1.98.1 using `cargo build --locked --release -p victory`. For profiling, use `cargo build --locked --profile diag -p victory --features diagnostics` and set `VICTORY_LOG=info` for the run. Compare the same save, camera route, settings, driver and cache state. Record peak dedicated GPU usage, shared GPU usage, private commit, working set, and frame times for each run.
