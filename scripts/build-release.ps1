<#
.SYNOPSIS
  Builds victory.exe so that the same commit gives the same bytes wherever it
  is built, and prints its SHA-256: a release can then be checked against
  this repository by building it again.

.DESCRIPTION
  What made two builds of one commit differ, and what this does about it:
  - The checkout's path is compiled in (`env!("CARGO_MANIFEST_DIR")` in
    victory-core, victory-pipeline, the shader build script; panic
    locations): the build runs from a drive letter mapped onto the checkout
    (`subst`), so every build sees the same `V:\`, and no builder's user
    folder ends up in the exe.
  - Cargo's registry is under the builder's home: its path is remapped.
  - The linker writes a time stamp and a random PDB GUID: `/Brepro` makes
    both a hash of the output, and `/PDBALTPATH` keeps the PDB's own path
    out.
  Measured on 48ad785: two checkouts in different folders built to the same
  SHA-256 with this, and to three different ones (with the release's) without.

  The same bytes also need the same Rust (pinned in rust-toolchain.toml) and
  the same MSVC tools and static C runtime: the script prints the versions
  it used, to publish with the hash.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1 -Out dist
#>
param(
    # Every build must use the same letter: it is part of the paths compiled in.
    [string]$Drive = 'V:',
    # Where victory.exe and victory.exe.sha256 are copied (relative to the checkout).
    [string]$Out = 'dist'
)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$outDir = Join-Path $root $Out
if (Test-Path "$Drive\") {
    throw "$Drive is already in use. Free it (subst $Drive /D) or pass -Drive; the hash then differs from builds made on V:."
}
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$flags = @(
    '-C target-feature=+crt-static',
    "--remap-path-prefix=$cargoHome=/cargo",
    "--remap-path-prefix=$Drive\=/victory",
    '-C link-arg=/Brepro',
    '-C link-arg=/PDBALTPATH:victory.pdb'
) -join ' '

$savedFlags = $env:RUSTFLAGS
subst $Drive $root
if ($LASTEXITCODE) { throw "subst $Drive failed" }
try {
    Push-Location "$Drive\"
    $env:RUSTFLAGS = $flags
    cargo build --release --locked -p victory
    if ($LASTEXITCODE) { throw 'cargo build failed' }
    New-Item -ItemType Directory -Force -Path $outDir | Out-Null
    Copy-Item "$Drive\target\release\victory.exe" $outDir -Force
} finally {
    Pop-Location
    $env:RUSTFLAGS = $savedFlags
    subst $Drive /D
}

$exe = Join-Path $outDir 'victory.exe'
$hash = (Get-FileHash $exe -Algorithm SHA256).Hash
"$hash  victory.exe" | Set-Content -Encoding ascii (Join-Path $outDir 'victory.exe.sha256')

$commit = git -C $root rev-parse HEAD
$rustc = rustc -V
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$msvc = 'unknown'
if (Test-Path $vswhere) {
    $vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    $tools = Join-Path $vs 'VC\Auxiliary\Build\Microsoft.VCToolsVersion.default.txt'
    if ($vs -and (Test-Path $tools)) { $msvc = (Get-Content $tools -TotalCount 1).Trim() }
}
''
"commit  $commit"
"rustc   $rustc"
"MSVC    $msvc"
"SHA-256 $hash  ($exe)"
