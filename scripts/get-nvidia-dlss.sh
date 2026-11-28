#!/usr/bin/env bash
# Linux stand-in for Get-NVIDIA-DLSS.cmd (scripts/get-nvidia-sdks.ps1), which needs Windows PowerShell.
# Downloads NVIDIA's Streamline SDK release from NVIDIA's own GitHub and puts the
# files the layer loads (and NVIDIA's licence files) into a Wine/Proton prefix,
# where victory.exe looks for them: %LOCALAPPDATA%\victory\nvidia-sdk\Streamline\bin\x64.
# NVIDIA's files stay under NVIDIA's licences; read the licence files it saves.
#
#   get-nvidia-dlss.sh <wine prefix> [streamline tag]
#
# The prefix is the one you run victory.exe in (with umu-run or Proton, the "pfx"
# folder). Needs curl and python3. Nothing is run from the download.
set -euo pipefail

prefix=${1:?usage: get-nvidia-dlss.sh <wine prefix> [streamline tag]}
tag=${2:-v2.14.16}
[[ $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "not a release tag: $tag" >&2; exit 2; }
[[ -d $prefix/drive_c ]] || { echo "$prefix has no drive_c: start victory.exe in it once, or make it with wineboot" >&2; exit 2; }

user=steamuser
[[ -d $prefix/drive_c/users/$user ]] || user=$(id -un)
dest=$prefix/drive_c/users/$user/AppData/Local/victory/nvidia-sdk/Streamline
bin=$dest/bin/x64
dlls=(sl.interposer.dll sl.common.dll sl.dlss.dll sl.dlss_g.dll sl.reflex.dll sl.pcl.dll nvngx_dlss.dll nvngx_dlssg.dll)

missing=0
for d in "${dlls[@]}"; do [[ -f $bin/$d ]] || missing=1; done
if (( !missing )); then echo "all there already: $bin"; exit 0; fi

url="https://github.com/NVIDIA-RTX/Streamline/releases/download/$tag/streamline-sdk-$tag.zip"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
echo "NVIDIA Streamline $tag from github.com/NVIDIA-RTX/Streamline (about 260 MB download, 70 MB kept):"
curl --fail --location --proto '=https' --tlsv1.2 --progress-bar --output "$work/sl.zip" "$url"
echo "sha256 $(sha256sum "$work/sl.zip" | cut -d' ' -f1)  streamline-sdk-$tag.zip"

mkdir -p "$bin"
# Only these names are taken, each written under its own base name, so nothing in
# the archive can pick where it goes. Release builds only (not bin/x64/development).
python3 -I - "$work/sl.zip" "$bin" "$dest" "${dlls[@]}" <<'PY'
import os, re, sys, zipfile
archive, bin_dir, dest, *want = sys.argv[1:]
want = set(want)
got = set()
with zipfile.ZipFile(archive) as z:
    for info in z.infolist():
        name = info.filename
        if info.is_dir() or "/development/" in name:
            continue
        base = os.path.basename(name)
        if re.search(r"(^|/)bin/x64/[^/]+$", name) and (base in want or re.search(r"license.*\.txt$", base, re.I)):
            out = os.path.join(bin_dir, base)
        elif re.fullmatch(r"([^/]+/)?license\.txt", name, re.I):
            out = os.path.join(dest, "license.txt")
        else:
            continue
        with z.open(info) as src, open(out, "wb") as dst:
            dst.write(src.read())
        got.add(base)
        print(f"  {os.path.relpath(out, dest)}  {info.file_size / 2**20:.1f} MB")
lost = sorted(want - got)
if lost:
    sys.exit("not in the archive: " + ", ".join(lost))
PY
echo "Done: $bin"
echo "Licences: $dest/license.txt (Streamline) and the *license*.txt files beside the DLLs (DLSS, Reflex)."
