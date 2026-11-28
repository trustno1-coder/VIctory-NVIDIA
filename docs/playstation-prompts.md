# PlayStation pads and button prompts

The Xbox build of the title ships only Xbox button art, so on a PlayStation
pad the game asks for A, B, X and Y. Two rows of the F1 menu are for the pad
and for the prompts. Both are read once at start, so a change applies at the
next start ("SAVE AND RESTART NOW" does that for you).

## Switches

| Variable | Values | Default | What |
|---|---|---|---|
| `VICTORY_DUALSENSE` | `0`, `1` | not set: AUTO | whether PlayStation pads are read: AUTO only while XInput lists no pad, `1` beside XInput's pads, `0` none (F1: PLAYSTATION PAD) |
| `VICTORY_PS_PROMPTS` | `1` | off | the game's button prompts are PlayStation's, built once at the next start from the copy being played (F1: PLAYSTATION BUTTON ICONS) |
| `VICTORY_PS_PICTURE` | a PNG file | `ps5-controller.png` in the home folder | the controller picture at Settings, Controls |
| `VICTORY_FILE_PATCH` | a list file | none | byte ranges of the game's files served from other files at read time (below) |

## PlayStation pads

A DualShock 4 (both revisions, and Sony's wireless adapter), a DualSense and
a DualSense Edge are read straight from HID, over USB or Bluetooth. They are
not XInput devices, so without a remapper (Steam Input, DS4Windows) XInput
never lists them; here none is needed. Their buttons go to the game in the
Xbox layout (cross is A, circle B, square X, triangle Y, L1 is LB, L2 is LT,
and so on) and the motors run at the speeds the game asks for. The lights and
the DualSense's adaptive triggers are left alone.

- AUTO (the variable not set, or any other value): XInput has priority.
  PlayStation pads are read only while XInput lists no pad, because a
  remapper presents the same pad twice.
- WITH XBOX PADS (`1`): PlayStation pads are read beside XInput's pads, so a
  PlayStation pad and an Xbox pad work at the same time.
- OFF (`0`): no PlayStation pad is read.

`VICTORY_DUALSENSE` is the variable's own name: it was given before the
DualShock 4 was read.

## PlayStation button prompts

With PLAYSTATION BUTTON ICONS on, the next start builds the prompts once from
the copy being played, and from then on the game shows them:

- In the HUD and the menus the face buttons are cross, circle, square and
  triangle; LB, LT, RB and RT are L1, L2, R1 and R2; View is the touchpad.
  The symbols are Kenney's Input Prompts (CC0), drawn inside the game's own
  button frames. Menu (the same icon as Options), the sticks and the D-pad
  stay as they are.
- The controller picture at Settings, Controls is a picture of your own, when
  you supply one (below).

No file in the game's folder is written. The patches go to
`<cache>\file-patches\playstation\patches.txt`, with the new bytes in files
beside it. `<cache>` is `%LOCALAPPDATA%\victory\cache` unless
`VICTORY_CACHE_DIR`, `VICTORY_HOME` or `cache_dir` in `victory.toml` names
another folder. The prompt movie is looked for in `data.rpf`, so that start
takes longer than usual; the log says what was built and how long it took. If
the build fails, the log says why and the game starts with the Xbox prompts.

The list is used again at every later start and not built again: before a
patch is applied, the game's bytes at its range are checked against the hash
in the list, so after an update of the game it is not applied (the log says
so). Delete the folder `file-patches\playstation` to have it built again. It
is built again by itself when a picture is there and the list has none yet,
or when the picture file is newer than the list. With `VICTORY_FILE_PATCH`
set too, that list is used and nothing is built.

The same by hand, or for another copy:

```
victory glyphs [--game <game folder>] [--picture <png>] [--out <folder>]
```

`--game` names the game folder (else the one `victory.toml` or
`VICTORY_GAME_DIR` configures). It writes the patches and the list to
`<cache>\file-patches\playstation` (else the folder `--out` names) and prints
the path of the list; set `VICTORY_FILE_PATCH` to it.

**The controller picture.** `VICTORY_PS_PICTURE` is a PNG of your own: the
DualSense as you want it shown, of any size, transparent where the menu is to
show through. Not set, it is `ps5-controller.png` in victory's home folder
(`%LOCALAPPDATA%\victory`, or the folder `VICTORY_HOME` names). With no
picture there, the prompts are built alone. A picture that cannot be read
(not a PNG with transparency) is said in the log and left out, until the
file changes. The picture is drawn into a 1024x512 texture, which the menu
shows at 0.75 of its width, so the picture is drawn wider in it: one of
1536x1024 comes out in its own proportions.

## Mods: `VICTORY_FILE_PATCH`

The prompts are one use of a general mechanism. `VICTORY_FILE_PATCH=<list>`
names a text file that lists byte ranges of the game's files to serve from
other files. Every file the title opens through DirectStorage is checked
against the list, archives and loose files alike, and the bytes of a listed
range are replaced as they are read. Nothing on disk is written, so the game's
folder stays as its maker wrote it.

One patch a line:

```
# <file name> <offset> <blake3 of the original range> <file with the new bytes>
data.rpf <offset> <blake3> <new bytes>
```

- `#` starts a comment; blank lines are skipped.
- `<file name>` is the name alone of the file the title opens; the case is
  ignored.
- `<offset>` is where the range starts in that file: decimal, or `0x` hex.
- `<blake3 of the original range>` is the BLAKE3 hash of the file's bytes at
  the range, 64 hex digits. The range is as long as the new bytes.
- `<file with the new bytes>` holds the replacement; a relative path is from
  the list's folder. It is the rest of the line, so it may contain spaces.

A patch is applied only to a file whose name matches and whose bytes at the
range still have that hash. A file of another version of the game is left as
it is, and a line that is not of this form is skipped; the log says so in
both cases. The list is read once per process, before the title opens its
first file.

Both launchers keep `VICTORY_FILE_PATCH` when they clear the other
`VICTORY_*` variables, so it can be set once where the launcher is started
from: in a command prompt, `setx VICTORY_FILE_PATCH "<path of patches.txt>"`,
then open a new window. The script launcher also takes it in `-More`.

The repository holds no patches: a list and its files are the user's own,
made from their copy ([asset policy](asset-policy.md)).

## How it works

- **The prompts** are one zlib entry in `data.rpf` that inflates to the game's
  prompt movie; only the label shapes change (`victory_formats::swf`,
  `victory_game::prompt_glyphs`). The archive's table is encrypted and is not
  touched, so the rebuilt movie keeps the old one's length (zeros after its
  `End` tag) and its stream the old stream's exact length: compressed with
  zopfli, then filled with stored blocks (`victory_formats::zlib_exact`). The
  game reads exactly the stream's length.
- **The controller picture** is the one 1024x512 BC7 texture in
  `data\ttdb\ui\platform.ttpf`, tiled as 64 KiB `D` blocks in one zlib
  stream (`victory_game::settings_diagram`, and the BC7 encoder
  `victory_formats::bc7`).
- **Served at read time.** `file_patch` in
  `crates\victory-hle\src\modules\dstorage.rs` puts a patch's bytes over
  every DirectStorage read that meets its range. With `VICTORY_PS_PROMPTS=1`
  and `VICTORY_FILE_PATCH` not set, the launcher (`apps\victory\src\run.rs`)
  makes the list in the cache folder when it is not there or a newer picture
  asks for it again (looking for the movie reads the whole of `data.rpf`: its
  table is encrypted, so the entry's place is not known), then hands it to
  the layer.
- **Everything written is derived from your copy.** It goes to the cache
  folder, `file-patches\playstation`, never into the repository.

## Why the archives are not repacked

The plain way to change what is inside an archive is to repack it with the
changed entry. Two things stand against that here, and neither has been
settled by a run in the game:

- No repacked RPF9 archive has been shown to load in the game. forge9's
  repacks are read back correctly by a reader forge9 did not write, and an
  unchanged archive repacks byte-identically, but its own
  [repack_validation.md](https://github.com/nineforge0/forge9/blob/main/docs/repack_validation.md)
  says so; a static read of the game's executable found no signature check on
  the load path, which does not prove a live load either.
- openrage states that the game checks a signature over the table when it
  mounts an archive, so only its maker can sign a rebuilt one, and that no
  file it wrote has been run in the game
  ([GTA_VI_Xbox_Build.md](https://github.com/openragee/openrage/blob/main/docs/features/GTA_VI_Xbox_Build.md)).

A file patch leaves each archive as its maker wrote it: table, header and
signature stay in place, and only the bytes the title reads from the patched
range differ. That is the route PR #3399 (below) checked in game.

## Log lines

The logs are in `%LOCALAPPDATA%\victory\logs`. At a start with PLAYSTATION
BUTTON ICONS on, `playstation prompts:` lines say what happened:

- `playstation prompts: building the patches from this copy of the game, once
  ...`: the build began; `playstation prompts: the patches are built, in ...
  s` ends it.
- `playstation prompts: the list made earlier is used as it is`: nothing was
  built at this start.
- `playstation prompts: not built (...)`: a warning; the title starts with
  its Xbox prompts.

The file patches:

- `file patch: applies to this file`: a patch is used for a file that was
  opened.
- `file patch: first read served from it`: the first time the game reads from
  a patch.
- `file patch: the file's bytes there are not the ones the patch was made
  for; not applied`: the range no longer has the listed hash.

Other `file patch:` lines are warnings that say what was not used: a list
that could not be read, a line that is not of the form above (it is skipped),
or a patch's new bytes that could not be read.

## Credits

Ported from [Marvisak/VIctory PR #3399](https://github.com/Marvisak/VIctory/pull/3399)
by anoyarou (GPL-3.0-or-later), where the author checked the prompts and the
controller picture in game, on Linux under Wine (Proton, RX 9070 XT). Prompt
symbols: Kenney's [Input Prompts](https://kenney.nl/assets/input-prompts)
(CC0). The controller picture is the user's own; nothing of it is in this
repository. PlayStation and DualSense are trademarks of Sony Interactive
Entertainment.
