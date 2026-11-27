# DLSS and DLSS Frame Generation (NVIDIA Streamline)

Three switches, all off by default:

- `VICTORY_DLSS=1`: DLSS Super Resolution does the title's FidelityFX upscaling
  (its `ffxDispatch`), from the same colour, depth, motion vectors, exposure and
  reactive mask into the same output, at the same place of the title's command
  list.
- `VICTORY_FRAMEGEN=1`: DLSS Frame Generation (2x) on our present, from the depth
  and motion vectors of the last upscaling submitted before the present.
- `VICTORY_DLSS_RR=1` (with `VICTORY_DLSS=1`): DLSS Ray Reconstruction's plugin
  is loaded, checked and reported; it does not draw yet (see "Ray
  Reconstruction").

Nothing NVIDIA ships is in this repository. The library is loaded at run time
from a folder you name; if anything is missing (the folder, a signature, a
feature on this GPU, the device hand-over), the log says what, and the title's
own upscaling and our plain present run as before.

## Setup

1. Download NVIDIA's Streamline SDK release (NVIDIA RTX SDK license) from
   <https://github.com/NVIDIA-RTX/Streamline/releases> and unpack it outside the
   repository. Tested: `streamline-sdk-v2.14.16.zip`, unpacked to
   `C:\dev\sdk\streamline-sdk-v2.14.16`. Its `bin\x64` folder holds what is
   loaded:
   - `sl.interposer.dll` (loaded by us; its Authenticode signature is checked),
   - `sl.common.dll`, `sl.dlss.dll`, `sl.dlss_g.dll`, `sl.reflex.dll`,
     `sl.pcl.dll` (Streamline's plugins; it checks their NVIDIA signatures),
   - `nvngx_dlss.dll` (DLSS Super Resolution, 310.x), `nvngx_dlssg.dll` (DLSS
     Frame Generation),
   - `sl.dlss_d.dll` and `nvngx_dlssd.dll` (Ray Reconstruction, only with
     `VICTORY_DLSS_RR=1`; `scripts\get-nvidia-sdks.ps1 -RayReconstruction`
     fetches them, 46 MB).
   A newer `nvngx_dlss.dll` (the DLSS SDK, <https://github.com/NVIDIA/DLSS>,
   `lib\Windows_x86_64\rel`) can replace the one in that folder.
2. `VICTORY_STREAMLINE_DIR=<that bin\x64 folder>`; the default is
   `C:\dev\sdk\streamline-sdk-v2.14.16\bin\x64`.
3. Check the PC without the title (a few seconds):
   - `victory upscaler-check`: Streamline loads, and what this GPU supports.
   - `victory upscaler-check --device`: the host device is made and handed over
     to Streamline, DLSS's render sizes for the output are listed, and DLSS runs
     three times on stand-in textures.
   - `victory upscaler-check --present`: as `--device`, then three seconds of
     presents in a window of ours with frame generation on stand-in depth and
     motion vectors and a stand-in colour without the interface; it reports how
     many frames frame generation showed and how many presents were tagged
     with that colour. With a device, the check ends by switching frame
     generation off, letting the swap chain go and calling `slShutdown`
     (`VICTORY_CHECK_TEARDOWN=0`: not; the process then fails fast at its
     exit, code 0xC0000409, which some shells show as 9).
   - `VICTORY_STREAMLINE_PRESENT=1` with `--present`: DLSS alone, 120
     upscalings with a present after each through Streamline's proxy of our
     swap chain (no "presentCommon() was not observed" may follow).

Windows 10 2004 or later, a GeForce RTX GPU (frame generation: RTX 40 or
later), hardware-accelerated GPU scheduling on (frame generation needs it).

## Switches

| Variable | Values | Default | What |
|---|---|---|---|
| `VICTORY_DLSS` | `1` | off | DLSS in place of the title's FidelityFX upscaling |
| `VICTORY_FRAMEGEN` | `1` | off | DLSS Frame Generation on our present; needs `VICTORY_THIN=1` or `VICTORY_ASYNC_SUBMIT=1` (the line of submissions) |
| `VICTORY_DLSS_RR` | `1` | off | DLSS Ray Reconstruction's plugin is loaded, checked and reported, and does not draw; needs `VICTORY_DLSS=1` (without it a warning and no change); F1 item "RAY RECONSTRUCTION (LOADS ONLY)", applied at the next start |
| `VICTORY_RR_TARGETS` | `1`, or a comma list of upscaling numbers | off | diagnostics: logs the textures the title's passes write around the upscaling, lines start `RR targets:`; `1` samples the 1st, 300th and 3000th upscaling (so `1` cannot mean only the first; `1,2` can), a list such as `1200,5000` those; needs `VICTORY_DLSS=1`, not the Ray Reconstruction plugin; no F1 item (it costs a lock and a hash per draw and dispatch while on) |
| `VICTORY_DLSS_MODE` | `dlaa`, `quality`, `balanced`, `performance`, `ultraperformance` | `quality` (`dlaa` with frame generation alone) | the scene's size: 1, 2/3, 0.58, 1/2, 1/3 of the output; the DLSS mode follows the sizes the title then passes |
| `VICTORY_DLSS_PRESET` | `default`, `j`, `k`, `l`, `m` (`e`, `f` deprecated) | `default` | the DLSS model, for every mode |
| `VICTORY_STREAMLINE_DIR` | a folder | see above | where `sl.interposer.dll` and the rest are |
| `VICTORY_STREAMLINE_LOG` | a folder | none | Streamline's own log files there, verbose |
| `VICTORY_DLSS_JITTER` | `x,y` | `1,1` | multipliers for the jitter DLSS is given (`-1,1` flips x) |
| `VICTORY_DLSS_MV` | `x,y` | `1,1` | multipliers for the motion-vector scale |
| `VICTORY_DLSS_REACTIVE` | `0` | on | `0`: the title's reactive mask is not given to DLSS |
| `VICTORY_FSR_PATH` | `1` | off | the title's FidelityFX path at `VICTORY_DLSS_MODE`'s scale with its own FidelityFX (no Streamline): the comparison for DLSS |
| `VICTORY_FRAMEGEN_HUDLESS` | `0` | on (with `VICTORY_FRAMEGEN=1`) | `0`: no colour without the interface is copied or tagged on Series X (generated frames then warp the interface) |
| `VICTORY_STREAMLINE_PRESENT` | `1` | off | DLSS without frame generation: our swap chain is Streamline's proxy of it, so Streamline sees the presents (no "presentCommon() was not observed") |
| `VICTORY_REFLEX` | `on` (or `1`), `boost` | off | Reflex by itself, with or without frame generation, whenever Streamline is loaded (`VICTORY_DLSS=1` or `VICTORY_FRAMEGEN=1`): `slReflexSleep` where the title asks for the reading that opens a frame's simulation (where its frame begins while it reads no input), the latency markers at their own times, a line of Reflex's state every ten seconds. `on`: low latency; `boost`: low latency with boost. Unset: every call made of Streamline is as before the switch existed |

## How it works

**Where the title upscales.** The title imports `ffxCreateContext`,
`ffxDestroyContext`, `ffxQuery` and `ffxDispatch` from
`amd_fidelityfx_loader_xbox.dll`, which loads `amd_fidelityfx_upscaler_xbox.dll`
itself. With a switch on, the executable's imports of the loader are bound to
ours (`victory_hle::modules::fidelityfx::Interpose`), which call the title's own
DLL for everything; only an upscale `ffxDispatch` (type `0x10001`) is taken. The
title's upscaler context stays the title's own, so its upscaling runs again on
any frame DLSS does not take.

**Series X does not use FidelityFX by itself.** Run logs: the upscaler DLL is
loaded on every Series S run and on no Series X run (stock or 1440p). The
renderer selection at title+0x7e9421 picks renderer 7 (the FidelityFX path)
only when the console is a Series S (title+0x75140 tests the device type for
6, Lockhart) and a capability bit is set; Series X takes renderer 6, the
title's own temporal pass. So with DLSS or frame generation available, the app
patches the image (`native_1440p::prepare_upscaler`, from DexNom's
`prepare_fsr_4k`): renderer 7 always, and the FidelityFX path's render size
code at title+0x28ef69a notes the FidelityFX quality mode of `VICTORY_DLSS_MODE`
and takes its scaled branch, whose scale is the mode's. The title computes the
render size itself there (scale times the output size, truncated); it does not
ask `ffxQuery` for it, so a query answer cannot change it. The temporal scene
scale (title+0x28f1e7e) is set to the same scale, as the stock Series X image
pairs them (2/3 each). Every patched instruction is checked against the stock
bytes first (the test `patches_the_upscaler_path` does so with the retail
executable). Image caches: `images\native-1440p-upscaler-<mode>-sdr` (with
`VICTORY_NATIVE_1440P=1`), `images\upscaler-<mode>` (otherwise).

**At the dispatch.** On the title's thread the dispatch's textures are checked
(host copies, states a shader can read or write); then a step is put into the
title's list (`crate::list::external`): run at once, or with `VICTORY_THIN=1`
written into the list's stream and run where it is translated, in order. The
step prepares the inputs as a compute shader's would be prepared (aliased
views, uploads of title-written textures, depth twins), moves each into the
state the title gave FidelityFX, records `slEvaluateFeature` into the host list
with the textures as local tags in those states (Streamline moves them to what
it needs and back), marks the output as written on the host, and has the
title's draw state set again before its next draw. Viewport 1, frame index =
the upscaling's number.

Constants from the dispatch: jitter, motion-vector scale (FidelityFX's scale
over the render size), near, far, vertical field of view, reset; from the
context's flags: HDR, inverted depth, auto exposure (no exposure texture),
display-resolution motion vectors, jittered motion vectors. The title passes
no camera matrices: the projection is made of the field of view, the render
aspect and the depth range; camera motion is in the motion vectors, so the
clip-to-previous-clip matrices are the identity and the camera stands at the
origin. DLSS Super Resolution reads none of the matrices; frame generation may.

**Frame generation.** Streamline is used with manual hooking: nothing of D3D12
or DXGI is hooked globally. The host device is handed over (`slSetD3DDevice`);
the direct queue is made through Streamline's device proxy (the native queue
behind it runs everything); our swap chain is made through its factory proxy
on the queue proxy. A list that recorded an upscaling is noted; when it is
submitted, its depth and motion vectors become the latest. Each present (on
the line's thread, `VICTORY_THIN=1`) then gets a frame token of the title's
frame number, the latency markers (simulation, render submit, present), the
constants and the depth and motion-vector tags in the states the submissions
left them in (viewport 0), and frame generation is switched on; a present
without a new upscaling (menus, loads) switches it off and clears the tags.
Reflex low latency is on (frame generation needs it); without `VICTORY_REFLEX`
`slReflexSleep` is not called and the markers are all sent at the present. On
Series S, whose interface is a second plane, the scene plane is the colour
without the interface and the interface plane its colour and alpha
(premultiplied, as our composer blends them).

**Reflex by itself** (`VICTORY_REFLEX=on` or `boost`; code:
`streamline.rs` and `streamline/reflex.rs`). Reflex and the latency markers
(PCL) are asked for whenever Streamline is loaded, and `slReflexSetOptions` is
called once with the mode the switch names. A Reflex frame is one index:
every frame token of it is made from that index (`slGetNewFrameToken` with
the index, where the token is used), on whichever thread.

Where the sleep goes. The title begins its frames with a wait for the frame
origin (`frame.rs`) and hands that wait's token to the frame's submissions
and its present. The first runs with the sleep and the simulation start at
that wait (2026-11-26, two runs) showed the title reading its pad once a
frame and after the frame's first submission: in the world 101 of 108 frames
in ten seconds had their reading too late for an input sample, and none had
one. What a frame submits right after its origin wait is a simulation made
before it; the reading that follows is the input of a later frame's
simulation, and the sleep at the origin wait held back a finished frame.
Reflex's sleep belongs right before the input of the simulation it paces,
and the guide (`ProgrammingGuide.md` 2.10) gives a frame's token where its
simulation starts and the same token to its rendering and present. So now
the reading opens the simulation, and the next origin wait takes it:

| When | Thread | Call |
|---|---|---|
| the title asks GameInput for its current reading, before the reading is picked, when no simulation is open, the newest frame's simulation has ended at its submission, and the thread is the one that opens simulations | the reading's | `slReflexSleep` with a new index, then simulation start and controller input sample of that index |
| the next origin wait (the frame takes the open simulation: its token now names that index) | the title's | nothing |
| the origin wait of a frame with no simulation to take | the title's | a new index; simulation start at once while more than half of the last 8 frames took a simulation (a reading missed), else `slReflexSleep` first, as before |
| the title's first `ExecuteCommandLists` with the frame's token on a queue of the direct list type (a copy or compute queue's submission is not the frame's render submission) | the title's | simulation end, render submit start |
| our present of the frame begins | the presenting one | render submit end (and before it what the frame still owes), then present start |
| our present returned | the presenting one | present end |

So the sleep delays the title's reading to just in time, the reading's
simulation runs between the simulation start and end, and the latency Reflex
measures and reduces starts at the input. This takes the title to be one
frame deep (the simulation read after frame N's first submission is the one
frame N + 1 submits); from outside the title one frame deep and two look the
same. Further readings of an open simulation (other kinds, other devices)
send nothing and sleep no more; a reading on another thread than the one
that opens simulations never sleeps (that role passes on after 8 frames
without an opening); a reading before the newest frame's simulation ended
opens nothing (it is that simulation's, or that frame is in its sleep at the
origin wait, so two sleeps never overlap). The origin wait that comes takes
the open simulation however long after its reading (in the world's slow
stretches that is 80 to 370 ms). A reading that comes while a simulation has
waited longer than four of the last intervals between origin waits, and at
least 250 ms, lets it go and opens another (the title read input and began
no frame, as in a load). A menu that reads every tenth frame keeps the sleep
at the origin wait on the other nine: Reflex's guide has `slReflexSleep`
called always, and whether its pacing bears frames without one the guides on
disk do not say.

Each marker goes once a frame and in that order. The title's present is bound
to its frame when the title calls it (by the frame token in its first plane,
else by order), so the host's present finds the frame's index when its turn
comes on the line's thread, where the title may already be in its next frame.
A frame that never submits gets its simulation end and render submit start at
its present. The indexes the host presents keep rising (frame generation's
constants and present markers use them): a second present of one frame's
token gets that frame's index and sends no marker again; another present the
title began no frame for takes the simulation a reading opened, if one is
open, and completes it, and else (the check's) gets a frame of its own with
every marker there. With
frame generation the present's constants and tags use the same index: the
one the reading opened, so the constants, the tags, the present's markers and
the simulation's all name one frame. The DLSS upscaling's tokens are not
these (they count upscalings) and did not change.
`sl_pcl.h` 2.14.16 has no general input-sample marker any more (6 is struck
out as deprecated): the controller's (13) is the one sent, for a pad or a
keyboard reading. The latency ping (`ePCLatencyPing`, a window message) and
the flash indicator are not sent.

A token is Streamline's object and the headers do not say how many it keeps
(`sl_core_api.h`: no more than three frames in flight are expected; the guide
makes one a frame). With the switch about nine are made a frame on three or
more threads, so while one is in use no other is made: a lock is held from
`slGetNewFrameToken` until the call that reads the token has returned (a
frame's markers; `slSetConstants` and `slEvaluateFeature` of the upscaling;
`slSetConstants` and `slSetTagForFrame` of frame generation, whose token is
made after the lock of the calls is taken). A marker may therefore wait for
as long as an upscaling takes to record; the ten-second line has the wait.
The one token read without the lock is the sleep's: held across the sleep the
lock would stop the markers of the frame before. The lock of the frames'
bookkeeping is let go once a marker's token is made, before the markers are
sent: the next thread to decide waits for the token's lock and its markers
still leave after these, but a submission of the title's that owes nothing
does not wait for another thread's `slPCLSetMarker`. Without the switch there
is no lock.

The sleep has no bound in the header and runs on the title's thread (the one
that reads input, or the one that waits for the frame origin), so its length
is watched: the longest of each place is in the ten-second line, one over
250 ms is warned of once, and after one over 2 s no further sleep is asked for
in the run (one warning; the markers go on). A sleep that never returns
cannot be ended from our side. If the title holds a lock of its own while it
reads input, a thread that waits for that lock stands for the sleep too; the
longest sleep at a reading says how long that can be.

**The colour without the interface on Series X.** There the title keeps the
interface in a target of its own (3840x2160, RGBA8, with its own depth) and
blends it over the frame itself: one triangle with the shaders vertex
`0602c2384acbb3b2`, pixel `a78d088fa55aa495` (blend one, 1 - source alpha: the
interface target is premultiplied), the last draw into the texture it presents,
after the scene's composite into that texture. Every frame listing of
2026-11-26 has it once a frame (stock 3840x2160 output), and the 1440p runs
with DLSS bind the same pipeline from frame 3 on. With frame generation
(`crate::streamline::before_target_draw`, called by `draw::State::draw`): in
front of that draw, when its one target is a texture the title presented
before, the target is copied into a texture of ours (two, in turn; the back
buffer's size and format, always a copy's destination), recorded into the
title's list at that place. A list makes one copy. When the list is submitted
the copy is noted, and the present of that texture tags the first copy
submitted since its last present as `kBufferTypeHUDLessColor`. Other pipelines
that draw into a presented texture are named in the log once each: an interface
element drawn there before the composite would be in the copy, one drawn after
it is warped as before. The interface target is not tagged as the interface's
colour and alpha: its size is not the back buffer's at a 1440p output.

**The camera's planes.** The title names the far plane first (near 10000, far
0.15, inverted depth), which FidelityFX takes in either order. Frame
generation's constants get them in order (near 0.15, far 10000), and the
projection made of them; DLSS Super Resolution's are as before.

**Notes on 32:9.** `-Res wide` (`VICTORY_DISPLAY_WIDE=5120x1440`) has been
written and compiled, not run on the title. A first run answers these, and the
log is where to look:

- How big is the interface target at 5120x1440? If it is 5120x1440 and not
  3840x2160, the middle band squashes it: set `VICTORY_UI_LETTERBOX=0`.
- Does the composite take its uv from interpolated vertices or from the pixel
  position? From vertices, the band shows the whole interface. From the pixel
  position, it shows only its middle half.
- Is the camera wide? If the world looks stretched, the title kept a 16:9
  camera.
- What does DLSS say is optimal? Read the `render sizes for [5120, 1440]` lines
  (Quality should come to 3413x960, the log has the real numbers).
- What does the frame generation line say at that size?

**Windowed and full screen.** With frame generation the system's Alt+Enter is
not taken for the window (`DXGI_MWA_NO_ALT_ENTER`): Streamline wants frame
generation off across such a change. When the presented size or format
changes, frame generation is switched off before the swap chain's buffers are
made again, and on again at the next present with an upscaling.

**Frame generation alone** (`VICTORY_FRAMEGEN=1` without `VICTORY_DLSS=1`)
leaves the upscaling to the title's own FidelityFX, which is made for the
Series S sizes only (1280x720 or 1920x1080 in, 1920x1080 out): at a 1440p
output its upscaler is not made (an error in the log says so) and the title
fails at its first upscaling. Use both switches there.

## Ray Reconstruction

`VICTORY_DLSS_RR=1` (with `VICTORY_DLSS=1`; without it a warning and the
switch stays off) loads and reports DLSS Ray Reconstruction. It does not draw:
DLSS Super Resolution takes every upscaling exactly as without the switch.

**What runs.** Streamline loads the Ray Reconstruction plugin (feature 1001,
`sl.dlss_d.dll` and `nvngx_dlssd.dll`, from `VICTORY_STREAMLINE_DIR` beside
DLSS) and is asked whether this GPU supports it. The plugin's own list of
required buffer tags is read (`slGetFeatureRequirements`) and logged, so the
required set is what the plugin says and not what we guess. Its version is
logged. At the first upscaling, and again whenever the DLSS mode or the sizes
change, Ray Reconstruction's optimal render size and its smallest and largest
for the title's output are logged beside Super Resolution's. Once, one line says
that it is loaded and answering and not drawing.

**Why it does not draw.** Per its programming guide in the SDK
(`ProgrammingGuideDLSS_RR.md`), Ray Reconstruction takes, besides the colour,
depth, motion vectors and output Super Resolution has:

- the diffuse albedo (linear, three channels),
- the specular albedo (three channels; the guide gives `EnvBRDFApprox2` to make
  it from the material),
- the normals (F16 or F32, view or world space) with the roughness, in the
  normals' alpha channel (`normalRoughnessMode` packed) or in a texture of its
  own,
- optionally the specular hit distance together with the world-to-view and
  view-to-world matrices, or specular motion vectors,
- and the colour must be HDR.

The FidelityFX dispatch carries the colour, depth, motion vectors and output
only (exposure, reactive and transparency masks are null in every run). The
rest would have to come from the title's own passes.

**What the tree knows and does not.** The title seems to have a deferred
G-buffer: the character shaders are named `PS_Deferred...` (`draw.rs`) and its
lighting reads the depth-stencil's stencil plane (`capture.rs`). Which target is
which, in which encoding (how normals and roughness are packed, whether the
albedo is sRGB or linear, view or world space) and where the specular colour
lives is not known. The title passes no camera matrices (see "How it works"),
so the world-to-view matrix in Ray Reconstruction's options would be left
invalid, not made up. A guide that is guessed would make a picture that looks
plausible and is wrong, so none is given.

**Whether it can be the denoiser is open.** Ray Reconstruction replaces a
denoiser that works on a noisy ray-traced image. The title's rays feed screen
probes (`CS_ScreenProbes_TemporalResolve`), an irradiance cache, shadows and
half-size reflections (`CS_ResolveScreenSpace`, `CS_RayMiss`), and all of it is
resolved inside the title before the colour reaches FidelityFX
(`docs/fork-ray-tracing.md`). If no pass writes a render-sized noisy image,
Ray Reconstruction could only be a guided upscaler fed an already resolved
colour, and whether the title's resolve passes could be left out is not known.
`VICTORY_RR_TARGETS` exists to answer it from a run.

**What a later change needs once a run has named the targets.**

- A registry the draw path fills for the guides.
- Compute passes of ours (the `ambient.rs` pattern, on the card) that decode the
  title's G-buffer into Ray Reconstruction's formats.
- `slDLSSDSetOptions` and `slEvaluateFeature` for `FEATURE_DLSS_RR` in place of
  Super Resolution's, for the same viewport and frame token (Super Resolution's
  options are still set, per the guide).
- `slFreeResources(FEATURE_DLSS_RR, viewport)` when the title's upscaler is
  destroyed.
- Ray Reconstruction has no dynamic resolution: a change of render size
  initialises it again.

None of this is written. Each item needs a fact a run has to show first.

## Log lines

At start: `Streamline: DLSS Super Resolution is supported on this GPU`
(requirements), `Streamline loaded (manual hooking, no downloads)`, the `display`
line with `through the title's FidelityFX path`. At the device: `Streamline: DLSS
Super Resolution loaded streamline=… ngx=…` (and frame generation, Reflex),
`the direct queue was made through the device proxy`, `our swap chain is
Streamline's proxy`. At the title: `FidelityFX: the title's import goes through
ours`, `FidelityFX: the title made its upscaler` (flags, sizes), `FidelityFX:
the title's first upscale dispatch` (every field), `DLSS: upscaling recorded in
place of the title's FidelityFX` (render and output sizes, DLSS mode, optimal
size, formats; again when they change). Every second: `DLSS per frame: …
upscalings by DLSS, … failed, … left to the title's FidelityFX` and `DLSS Frame
Generation: … presents with it on, … off, … frames shown by it (status …)`.
`FidelityFX: the title's own upscaling runs: <why>` names each reason once.

Ray Reconstruction (`VICTORY_DLSS_RR`): at start `Streamline: DLSS Ray
Reconstruction is supported on this GPU`, or `Streamline: DLSS Ray
Reconstruction says it needs these buffer tags` (the plugin's own required
list); at the device `Streamline: DLSS Ray Reconstruction loaded streamline=…
ngx=…`; at the first upscaling and at each change of mode or sizes `DLSS Ray
Reconstruction: optimal render size for this output` (beside Super Resolution's
line); once, the line that it is loaded and answering and not drawing.
`VICTORY_RR_TARGETS`: lines that start `RR targets:` at the chosen upscalings, a
header with the four known textures, one line per pass, and a summary of the
render-size textures.

Frame generation: `DLSS Frame Generation switched frame=… on=… code=…` at each
change; `DLSS Frame Generation: first frame with depth and motion vectors
tagged` (sizes, the states of the two textures, near, far, jitter,
motion-vector scale, `interface_plane`, `interface_free_copy`); `video memory
as the system counts it, right before the first present with frame generation
on` and `… 240 presents with frame generation on later`; `DLSS Frame
Generation status changed` (anything but 0 is a fault); `DLSS Frame Generation:
a present failed hresult=…`. Series X: `DLSS Frame Generation: the colour
without the interface is copied in front of the title's interface composite
(first time)` (size, format, pipeline), `DLSS Frame Generation: this pipeline
draws into the texture the title presents` (once per pipeline), and every ten
frame-rate lines `DLSS Frame Generation, colour without the interface, last ten
lines: … copies in front of the title's interface composite, … presents tagged
with one, … presents with frame generation on had none; … interface composite
draws into a presented texture (… after a copy in their list), … draws of other
pipelines into one`.

Reflex by itself (`VICTORY_REFLEX`): at start `Streamline: Reflex by itself is
asked for (VICTORY_REFLEX) and supported`; at the device `Streamline: Reflex
options set … code=0 mode=…` and `Reflex by itself is on`; then about every
ten seconds `Reflex state: low latency available, …; latency report over N
frames: … from simulation start to the end of GPU rendering on average …;
since the last line: N frames begun: N with an input sample, N took the
simulation a reading opened, N had none to take and no sleep (most of the
last 8 frames took one), N slept at the origin wait (fewer did);
readings asked for: N opened a simulation, N were part of an open one, N came
before the newest frame's simulation ended, N on another thread than the one
that opens them; N simulations let go at a reading because no origin wait
took them within … ms (the bound now), N input samples
lost (…), N frames whose first submission came before their simulation
started; sleeps at a reading N of … ms on average (the longest … ms), at the
origin wait N of …; on the frame's origin-wait thread and on another:
openings N and N, first submissions N and N, title presents N and N; from the
origin wait to the first submission … ms on average over N (the longest …
ms), from the first submission to the next opening …, from an opening to the
origin wait that took it …; N simulations ended by a submission with the
title's token and N by one found by order, N presented without a submission
before, N presents with no frame begun, N presents bound by the title's token
and N by order, N frames never presented, … ms waited for a frame token in
use (the longest wait … ms)`. The total is
the difference of two time stamps of the report (`gpuRenderEndTime` less
`simStartTime`); the header names no unit for them. `Reflex stays off` or
`Reflex by itself stays off` with the reason, if it does not run. Once each,
if it happens: `Reflex: one slReflexSleep took … ms, more than the 250 ms …`
and `Reflex: one slReflexSleep took … ms, more than 2000 ms: no further sleep
is asked for in this run …` (the line then says `NO sleep is asked for any
more`).

## The first run with Reflex by itself in the title

Run twice in the title (2026-11-26) with the sleep at the origin wait; not run
yet with the sleep at the reading. Through the launcher: `-More @{
VICTORY_REFLEX = 'on' }` with DLSS or frame generation on. Before it, without
the title: `victory upscaler-check --present` with `VICTORY_REFLEX=on` goes
through both sleeps, every marker and the state on stand-in frames (each
begins, submits, takes the reading of the next simulation and presents) and
ends with a `Reflex state:` line with `180 frames begun: 180 with an input
sample, 179 took the simulation a reading opened`, `1 slept at the origin
wait` (the first frame) and 180 sleeps at a reading (the input samples count
simulations: the one the last reading opened is never taken). In the title,
the
ten-second line should show:

1. `low latency available` on a GeForce card, and after the first line a
   `latency report over N frames` with a total that is not 0. The total (game
   to render) should be larger than in the runs with the sleep at the origin
   wait (about 1.9 GPU frame times there) by about the simulation's time: the
   report now starts at the input.
2. In the world: `with an input sample` and `took the simulation a reading
   opened` near the frames begun, `slept at the origin wait` near 0, `sleeps
   at a reading` near the frames begun, and `came before the newest frame's
   simulation ended`, `on another thread`, `let go at a reading` and `lost` near 0.
   That is the sleep at the input. `slept at the origin wait` large with the
   pad in use instead: the title's readings come before its frames'
   submissions, nothing opens, and the sleep stays where it was.
3. The threads (which the runs before could not show): `openings N and N`
   mostly in the second number: the title reads on another thread than the one
   that waits for the frame origin. Mostly in the first: that thread reads
   itself, after its frame's submission went to another thread (then `first
   submissions` are mostly in the second number too).
4. The gaps: `from the origin wait to the first submission` well under a
   millisecond in the world (the frame submits a finished simulation at once);
   `from an opening to the origin wait that took it` about a frame's
   simulation. `frames whose first submission came before their simulation
   started` above a few: the title is deeper than one frame there, and the
   simulation's end is marked a frame late.
5. As many frames begun as the frame-rate line counts frames. Many more begun
   than presented (`frames never presented` large): the title waits for the
   frame origin more than once a frame.
6. `presents bound by the title's token` for nearly all presents. `by order`
   instead: the plane's token is not the origin wait's, and in a title that
   begins its next frame before it presents the last one the present's
   markers would name the wrong frame.
7. `presented without a submission before` and `presents with no frame
   begun` near 0 in gameplay.
8. The frame rate with and without the switch: Reflex should cost a few
   percent at most when it is on (more with boost), and the frame-pacing
   line (`VICTORY_TIMER_LOG=1`) should stay even. Our own clock still paces
   the title's flip wait; if the two fight, the pacing line shows it.
9. With frame generation (`VICTORY_FRAMEGEN_X=2`; both runs so far had 0):
   no `DLSS Frame Generation status changed` (2 is "Reflex not seen") and no
   `common constants cannot be found for frame N` in Streamline's log (the
   present's markers and the constants name one frame index).
10. With DLSS, frame generation and Reflex on together (the most tokens a
    frame): still no `common constants cannot be found`, no `DLSS: the
    upscaling was not recorded: slEvaluateFeature …` and no `frame
    generation: slSetConstants` or `slSetTagForFrame` warning. One of them
    with the switch and none without: a token named another frame by the
    time it was read.
11. `simulations ended by a submission with the title's token` for nearly all
    frames. `by one found by order` instead: the title does not tag its
    submissions (or tags them with something else than the origin wait's
    token), the newest running frame is taken, and a frame still submitting
    after the title began the next one ends the next one's simulation early.
12. `the longest … ms` of both kinds of sleep under a frame's time, and
    neither of the two `one slReflexSleep took` warnings, also with the
    window minimised or without focus for a while (whether the sleep stands
    then is not in the headers). A frame rate that drops with the switch
    while the sleeps at a reading stay short: the title holds a lock while it
    reads input that its other threads need.
13. `ms waited for a frame token in use` small against ten seconds of frames
    (it is a sum), and its longest wait about what an upscaling takes to
    record (the frame generation time line has that number).

## The first run with frame generation in the title

Switches: `VICTORY_DLSS=1 VICTORY_DLSS_MODE=quality VICTORY_FRAMEGEN=1
VICTORY_THIN=1 VICTORY_NATIVE_1440P=1`, Series X (the launcher: `-FrameGen`).
Before it, once: `victory upscaler-check --present` (below, line 1).

1. Without the title: `frame generation: 180 presents, 180 with it on, 360
   frames shown by it, 180 presents tagged with a colour without the
   interface, status 0x0`, then `Streamline shut down: ok (0)`, exit code 0.
   (Fewer than 360 shown, 291 in one run of 2026-11-27, when the PC is busy:
   frame generation drops a generated frame that comes too late.)
2. It started: `Streamline loaded … dlss=true framegen=true`, `Streamline: DLSS
   Frame Generation loaded`, `Streamline: Reflex low latency on … code=0`,
   `the direct queue was made through the device proxy`, `our swap chain is
   Streamline's proxy`. `frame generation stays off` or `not running (see
   above)` anywhere: it does not run; the line before says why.
3. It switched on in the world: `DLSS Frame Generation switched … on=true
   code=0` at the first frame with an upscaling, then `first frame with depth
   and motion vectors tagged` with `near=0.15 far=10000`, `depth_inverted=true`,
   `interface_free_copy=true`.
4. Generated frames are shown: each second `DLSS Frame Generation: N presents
   with it on, 0 off, 2N frames shown by it (status 0x0)`. `frames shown`
   equal to `presents`: none was generated. Our caption and frame-rate line
   count the title's frames only; NVIDIA's overlay or PresentMon shows twice
   that. (`DLSSG_IndicatorText`, a DWORD of 2 under
   `HKLM\SOFTWARE\NVIDIA Corporation\Global\NGXCore`, makes frame generation
   write its state on the picture.)
5. No fault: no `DLSS Frame Generation status changed`, no `a present failed`,
   no line of target `streamline` at ERROR, no `presentCommon() was not
   observed`. A pink tint over the picture is frame generation's own sign of
   an invalid state.
6. Off and on: at the pause menu, a load or a video, `DLSS Frame Generation
   switched … on=false` when the title stops upscaling and `on=true` when it
   upscales again; the per-second line then counts presents `off`. If the
   pause menu keeps upscaling, frame generation stays on behind it.
7. The interface: the ten-second line has as many `copies` as `presents tagged
   with one`, about one per present with frame generation on, and `had none`
   near 0 in gameplay; the map, the subtitles and the prompts stand still and
   sharp while the camera turns. `this pipeline draws into the texture the
   title presents` should name one pipeline, the scene's composite (blend
   false); any other with `blend true` is an interface element drawn outside
   the interface target. Compare with `VICTORY_FRAMEGEN_HUDLESS=0`: the
   interface wobbles at the edges of moving things behind it.
8. Motion vectors (`VICTORY_DLSS_MV`, default `1,1`): turn the camera slowly
   and evenly past upright edges (poles, door frames) and look at a still
   interface element's neighbourhood and at the edges. Right: edges stay
   single in every frame. Wrong sign or scale: every other frame shows the
   edge doubled or pulled the wrong way (a judder at half the shown rate), and
   DLSS itself smears a trail behind moving edges. Try `-1,1` (x flipped; turn
   left and right), `1,-1` (y flipped; look up and down), then `2,2` and
   `0.5,0.5` (scale): if `1,1` is right, each of them is visibly worse. The
   `DLSS: upscaling recorded` line has `mvec_scale` (1/1706, 1/960 at Quality:
   the title's vectors are in render pixels).
9. Jitter (`VICTORY_DLSS_JITTER`, default `1,1`): stand still and look at fine
   detail (wire, lettering on signs, distant railings). Right: it settles
   within a second to a sharp, steady picture. Wrong sign: it stays soft and
   crawls. Try `-1,1`, `1,-1`, `-1,-1`; take the sharpest steady one. The
   jitter changes nothing for frame generation itself.
10. Video memory: the two `video memory … frame generation on` lines; the
   difference of `in use` is what frame generation took (NVIDIA: about 489 MB
   at 1440p). `in use` over the budget of the card's own memory, or the PC's
   shared memory growing by hundreds of MB in the memory line, means it does
   not fit: use a smaller DLSS mode.
11. A removed device: `host GPU device removed after …`, `; device removed:
   …`, `DLSS Frame Generation: a present failed hresult=0x887a0005` (removed),
   `0x887a0006` (hung) or `0x887a0007` (reset), the window gone with exit code
   0x887a0005. With `VICTORY_STREAMLINE_LOG=<folder>` Streamline's own log has
   the plugin's last words.

## The first run with VICTORY_RR_TARGETS

Not run yet. Through the launcher, in the world: `-More @{ VICTORY_DLSS = '1';
VICTORY_DLSS_RR = '1'; VICTORY_RR_TARGETS = '1' }` (with the usual DLSS
switches), and once more with `VICTORY_RT_NATIVE = '1'` added. Read the
`RR targets:` lines:

1. The header: the four textures the upscaling is given (colour, depth, motion
   vectors, output), with address, size and format.
2. Passes marked `[binds the upscaling's depth]` (a depth view bound, not a
   proven write, so forward and sky passes show too): the render targets bound
   together with that depth are the G-buffer set; four or more is the filter. Note their formats and sizes.
3. Which pass writes `[colour]`, and which passes sit between the G-buffer and
   it, by program name.
4. Whether any trace, resolve or denoise-like pass (by name) writes a texture of
   the render size, or only smaller ones (probe grids, half size). That says
   whether Ray Reconstruction could replace a denoiser here.
5. The summary line of render-size textures.

Also read `Streamline: DLSS Ray Reconstruction says it needs these buffer tags`
(the plugin's own required list) and the line for Ray Reconstruction's optimal
size at the output size in use: a 32:9 output would show whether it takes that
shape.

Then in a diagnostics build (`--profile diag --features diagnostics`),
`VICTORY_CAPTURE=<folder> VICTORY_CAPTURE_FRAMES=<n> VICTORY_DUMP_TARGETS=1`
saves the same textures as pictures named by the same addresses, to read the
encodings. The pictures stay outside the repository (`docs/asset-policy.md`).

## Limits

- Ray Reconstruction (`VICTORY_DLSS_RR`) loads and reports only. It takes no
  upscale, adds no video memory beyond the plugin's own load, and is set at the
  start only (the F1 item applies at the next start).
- Motion vectors: blocks of wrong motion inside character silhouettes (likely
  the heat-haze pass `731a39171b568298`) show as smearing with DLSS and as
  warped shapes in generated frames.
- The colour without the interface on Series X is a copy in front of one
  known draw (the interface composite by its shaders, this build's); not yet
  seen running in the title. Interface drawn into the frame by another
  pipeline is not told apart. The interface's own colour and alpha are not
  tagged there.
- Camera matrices are made, not the title's (see above).
- "presentCommon() was not observed" (Streamline, once a run, with DLSS and no
  frame generation): our swap chain is not Streamline's then, so its common
  plugin sees no present; it frees nothing it retired (what DLSS let go of when
  the title's upscaler is destroyed stays allocated until the process ends).
  `VICTORY_STREAMLINE_PRESENT=1` gives it the presents.
- HDR output as a 16-bit float (scRGB) swap chain stays unsupported by frame
  generation (status 4). `VICTORY_HDR=1` presents HDR10 instead (R10G10B10A2,
  G2084 with Rec.2020 primaries), which Streamline's guide supports. With it,
  the planes this layer converts or blends (an interface plane, SDR planes) are
  not in the back buffer's colour space, so they are not tagged as hudless or
  interface and frame generation reads the composed frame. Not run yet.
- Video memory: the title's FidelityFX context stays allocated beside DLSS's;
  frame generation needs about 0.5 GB more at 1440p.
- Latency: frame generation holds a frame back; it is meant for a base frame
  rate of 60 and up.
- Reflex by itself (`VICTORY_REFLEX`) is set at the start only: no line in the
  F1 menu, no change while the title runs. It does not load Streamline alone
  (it needs `VICTORY_DLSS=1` or `VICTORY_FRAMEGEN=1`). No latency ping and no
  flash indicator are sent, so NVIDIA's tools measure no input sampling
  latency from them.
- `slShutdown` is not called in the title; the process ends with the title.
  Where the title leaves through the C runtime's exit with frame generation
  on, the process may fail fast at its very end as the check did without its
  shutdown (exit code 0xC0000409): no fault of the run before it.

Credits: the FidelityFX description layouts and the image patch of the
FidelityFX path are DexNom's (DexNom/VIctory, GPL-3.0-or-later).
