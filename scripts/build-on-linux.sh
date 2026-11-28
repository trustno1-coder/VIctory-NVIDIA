#!/usr/bin/env bash
# Builds Windows victory.exe on Arch-based Linux (CachyOS and its
# relatives), to run under Wine like any other start of the program. Nothing
# here runs the title: the program stays Windows-only, this only builds its
# exe, and both routes are built on Linux by CI on every pull request.
#
#   scripts/build-on-linux.sh          MinGW build: what pacman has
#   scripts/build-on-linux.sh --msvc   MSVC build (cargo-xwin): the same ABI
#                                      Windows builds use, with the D3D12
#                                      Agility exports, which need Visual
#                                      Studio's linker and are left out of the
#                                      MinGW one (under Wine both run DXVK's
#                                      Direct3D 12 anyway)
#
# The exe lands in target/<target>/release/victory.exe; put it next to your
# own dump of the title, beside game_scarlett_master.exe, and start it as
# before (`wine victory.exe`). Not tested here: CI proves the build, not the
# run.

set -euo pipefail

cd "$(dirname "$0")/.."

route=gnu
for arg in "$@"; do
    case "$arg" in
        --msvc) route=msvc ;;
        --gnu) route=gnu ;;
        *) echo "unknown argument: $arg (this script knows --msvc, and nothing else)"; exit 2 ;;
    esac
done

jobs="${JOBS:-$(nproc 2>/dev/null || echo 4)}"
gnu_target=x86_64-pc-windows-gnu
msvc_target=x86_64-pc-windows-msvc
target=$gnu_target
if [ "$route" = msvc ]; then
    target=$msvc_target
fi

# Rust, at the version rust-toolchain.toml pins.
if ! command -v rustup >/dev/null 2>&1; then
    echo "no rustup in PATH: 'sudo pacman -S --needed rustup', then open a new terminal"
    exit 1
fi
echo "==> rustup toolchain install (the version rust-toolchain.toml pins)"
rustup toolchain install
if [ "$route" = gnu ]; then
    # The repository pins the MSVC target only; the MinGW one needs adding.
    rustup target add "$gnu_target"
fi

if [ "$route" = gnu ]; then
    if ! command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
        if command -v pacman >/dev/null 2>&1; then
            echo "==> pacman -S --needed mingw-w64-gcc (the cross compiler; Arch and CachyOS)"
            sudo pacman -S --needed mingw-w64-gcc
        else
            echo "no x86_64-w64-mingw32-gcc in PATH: install the mingw-w64 cross toolchain"
            echo "(Debian and Ubuntu: sudo apt-get install -y mingw-w64), then run this again"
            exit 1
        fi
    fi
    echo "==> cargo build --release --target $target"
    CARGO_BUILD_JOBS="$jobs" cargo build --release --locked --target "$target"
else
    # The cc crate compiles blake3's C with clang-cl; cargo-xwin fetches
    # Microsoft's own import libraries and headers, so lld-link can take the
    # MSVC linker arguments apps/victory/build.rs hands it.
    if ! command -v clang-cl >/dev/null 2>&1 || ! command -v llvm-ar >/dev/null 2>&1; then
        if command -v pacman >/dev/null 2>&1; then
            echo "==> pacman -S --needed clang llvm (clang-cl compiles blake3's C, llvm-ar archives it)"
            sudo pacman -S --needed clang llvm
        else
            echo "no clang-cl or llvm-ar in PATH: install clang and llvm"
            echo "(Debian and Ubuntu: sudo apt-get install -y clang llvm)"
            exit 1
        fi
    fi
    if ! cargo xwin --version >/dev/null 2>&1; then
        echo "==> cargo install cargo-xwin --locked (once)"
        cargo install cargo-xwin --locked
    fi
    echo "==> cargo xwin build --release --target $target"
    CARGO_BUILD_JOBS="$jobs" cargo xwin build --release --locked --target "$target"
fi

out="target/$target/release/victory.exe"
if [ ! -f "$out" ]; then
    echo "built nothing: no $out (the compiler output above says why)"
    exit 1
fi
echo "==> done: $out"
sha256sum "$out"
echo "Put it next to your own dump of the title (beside game_scarlett_master.exe)"
echo "and start it under Wine as before."
if [ "$route" = gnu ]; then
    echo "(MinGW build: no D3D12 Agility exports, so Windows runs its own Direct3D 12"
    echo "runtime; --msvc builds with them. Under Wine both run DXVK's.)"
fi
