# Troubleshooting

The questions asked most often in the issues, with the answers given there.
Before opening a new issue, update to the
[latest release](https://github.com/trustno1-coder/VIctory-NVIDIA/releases/latest)
and look for your error below. Many crashes reported on older releases are
already fixed.

## Before you report a problem

Attach **both** logs from the run that went wrong. Without them we can only
guess. Paste `%LOCALAPPDATA%` into the address bar of File Explorer to get
there.

- **The play log**, in `%LOCALAPPDATA%\victory\logs\`: the newest
  `victory-nvidia-play-<date>-<time>-<number>.log`. The window shows its path
  on the `Log: ...` line at the start.
- **The crash log**: `%LOCALAPPDATA%\VIctory\data\7952987A\crashes.log`, and
  the newest `victory-<date>-<time>-<number>.crash.log` in
  `%LOCALAPPDATA%\VIctory\data\7952987A\temp\` if there is one from that run.

Also say:

- which release you used (or which commit, if you built it yourself);
- your GPU, driver version, RAM, and Windows version (10 or 11, and its build);
- when it happens: right at the start, at the menu, while loading, or in play
  (and where).

Attach the log files themselves: a screenshot of the last lines is not enough.
Never attach anything from the game's folder.

If your crash is already reported (same error, or the same `+0x...` address
in `crashes.log`), add your logs to that issue rather than opening a new one.

## Crashes at start

### `0x887E0003`, "The D3D12 SDK version configuration of the host exe is invalid"

Often seen with `D3D12SerializeVersionedRootSignature` in the log. Windows
refused to start Direct3D 12 because `victory.exe` asked for a version of
Microsoft's Direct3D 12 runtime that was not there.

- **Update to v2026.11.27.6 or later.** Earlier releases always asked for
  version 1.614 (the fixes in .4 and .5 did not take effect). From .6 on, the
  exe asks for the version that is actually there: that of the `D3D12` folder
  beside it, or Windows' own.
- A `D3D12` folder from a repack or another guide (1.618, 1.620) is fine with
  .6 or later.

### Crash at `+0xb5a5f8` on Windows 10

Windows 10's own Direct3D 12 stops at shader model 6.5 and refuses some of
the game's shaders, which need 6.6. `victory.exe` uses Microsoft's newer
runtime (the D3D12 Agility SDK, `D3D12Core.dll`, about 22 MB from nuget.org)
in a `D3D12` folder beside it.

- With v2026.11.27.6 or later, the first start on Windows 10 offers to fetch
  it.
- To fetch it by hand, open a command prompt in the folder with `victory.exe`
  (type `cmd` in File Explorer's address bar there) and run
  `victory.exe get-d3d12`.

Windows 11 uses its own runtime and needs nothing.

### "Failed to allocate upload buffer" right after the loading screen

The full line is `D3D12 XDXR Error: Failed to allocate upload buffer`. Fixed
in v2026.11.27.4: the exe now reserves the memory range the game's ray
tracing needs as soon as it starts. Update to the latest release.

### GTX 10 or GTX 16 series card

The log shows `mesh shader support mesh=false`, or a bindless compute shader
refused with `E_INVALIDARG`. These cards can't run the game: it draws with mesh
shaders and bindless resources that they lack. No setting changes that. An RTX
20 series card or newer is needed.

### AMD and Intel Arc cards

This fork is built and tested on NVIDIA cards only. On AMD and Intel GPUs it
may crash where an NVIDIA card doesn't, often inside the GPU driver. Reports
with logs are still welcome.

### The game closes after about 20 seconds on the first start

That is on purpose. The first start of each new version runs the game for
about 20 seconds to warm up, says so at the top of the picture, and restarts
it. If it closes and doesn't come back, start `victory.exe` again by hand: the
second start goes straight in.

### Microsoft Defender reports `victory.exe` as a trojan

A false positive: `victory.exe` isn't signed, and it loads and patches the
game's exe in memory, which looks like what malware does. To be sure your copy
is the released one, compare its SHA-256 with the one listed on the release
page:

```
certutil -hashfile victory.exe SHA256
```

You can also build it yourself from the source (see the [README](../README.md)).

## Controls

### The game doesn't react to the mouse

Press **F1**, set **MOUSE CAPTURE** to **ON**, then save. The game gets the
mouse after that.

### The controller doesn't work

The game reads Xbox-style (XInput) controllers. A PlayStation or other
controller needs Steam Input or DS4Windows to show up as one. If an Xbox
controller doesn't work, open an issue with the play log.

### Stuck at the account picker

Press **Enter** on the keyboard (or **A** on an Xbox controller). It picks the
offline local user, and the game carries on.

## DLSS and frame generation

### Frame generation doesn't turn on

- **On an RTX 40 or 50:** DLSS frame generation needs Windows' **Hardware-accelerated
  GPU scheduling**. The log then says
  `DLSS Frame Generation is not supported here: OS hardware scheduling disabled`.
  Turn it on in Settings > System > Display > Graphics > Change default
  graphics settings, then restart the PC.
- **On an RTX 20 or 30:** NVIDIA doesn't offer DLSS frame generation on these
  cards, so AMD FSR 3 frame generation (2x at most) is used instead. It needs
  AMD's FidelityFX files: run `victory.exe get-fsr`.
- Frames are only generated while the game's window has the focus, and about
  a second passes after a change before they come.

### I pressed N when asked to download NVIDIA's or AMD's files

`victory.exe` remembers the answer and doesn't ask again. To get the files:

- run `victory.exe get-dlss` (NVIDIA's DLSS and frame generation files) or
  `victory.exe get-fsr` (AMD's FSR 3 files) from a command prompt in the
  folder with `victory.exe`;
- or delete the `declined` file in `%LOCALAPPDATA%\victory\nvidia-sdk\` or
  `%LOCALAPPDATA%\victory\amd-fidelityfx\`, and start again to be asked once
  more.

### `NGX evaluate feature failed 0xbad00005`

When this repeats every frame, DLSS is refusing its inputs. Update the NVIDIA
driver and try the latest release. Say in the issue whether the problem is
still there with DLSS off (F1 menu).

## Picture and speed

### "1440p output error"

The 1440p setting patches the game for a 1440p output. It only works with the
unmodified retail `game_scarlett_master.exe` of v1.0.2.1. Copying a file
doesn't change it, but some repacks ship a modified exe. Use the original exe
from your own copy of the game.

### Black areas, freezes or low-resolution textures on an 8 GB card

The log shows textures over the card's budget. Try the Series S profile (F1,
**CONSOLE**, **SERIES S (FASTER)**) and DLSS. Without NVIDIA's files (see
`victory.exe get-dlss` above), the game renders at full resolution with no
upscaling.

### Faces or objects missing on the first loads

These are shaders not built yet. They appear once the shader cache has them.
To build them up front, choose **PRELOAD SHADERS** in the F1 menu (it
restarts the game).

### Wrong lighting, neon-green or wrong colours, HUD size

These are known, and rendering is what's being worked on. No setting fixes
them yet. Watch the release notes.

### Memory grows during play and the frame rate drops

Fixed in v2026.11.27.4 (PR #58). Update to the latest release.

### C: drive full

Set `VICTORY_HOME` to another folder once, in a command prompt:

```
setx VICTORY_HOME D:\victory
```

Then start `victory.exe` from a new window. Settings, saves, caches, logs and
downloads go to that folder instead of `%LOCALAPPDATA%\victory`. To keep what
you have, move that folder's contents there first.

## Lines in the log that are normal

- `HTTP connection blocked destination=submit.backtrace.io:443`: the game's
  crash reporter is kept offline on purpose. It isn't a cause of crashes.
- `first-chance` exceptions: the game may catch and recover from them. The
  last lines of the log show what actually stopped the run.
- A couple of hundred shaders the translator doesn't handle yet, listed during
  the preload: the preload still finishes, and the game runs without them.
- `slot=0x1c8` "pipeline creation method not implemented": seen in runs that
  work fine too.

## Not supported

- **Linux** (Wine or Proton): not supported, and not planned for this fork
  right now.
- **GTX 10 and 16 series cards**: see above.
