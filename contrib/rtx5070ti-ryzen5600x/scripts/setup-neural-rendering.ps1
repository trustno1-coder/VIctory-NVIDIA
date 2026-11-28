# Prepares DLSS 5 Neural Rendering for the F1 menu's NEURAL RENDERING (INSERT) switch.
#
# The layer (apps/victory/src/play.rs, load_neural) loads the OptiScaler DLSS5 mod
# (https://github.com/Skynizz/optiscaler-dlss5, GPL-3.0, not part of this project) from an
# `optiscaler-dlss5` folder beside victory.exe or up to five folders above it (or VICTORY_NEURAL_DIR),
# before the device and Streamline are made. This script makes that folder at the repository root:
#
#   optiscaler-dlss5\winmm.dll             OptiScaler.dll of the DLSS5 release, under a proxy name
#   optiscaler-dlss5\nvngx.dll_dlssnr.dll  the forwarder of the same release
#   optiscaler-dlss5\nvngx_dlssnr.dll      NVIDIA's model (from a driver package; not redistributable)
#   optiscaler-dlss5\OptiScaler.ini        the release's ini with the keys below
#
# and links NVIDIA's nvngx_dlss / nvngx_dlssg / nvngx_dlssd from the Streamline folder beside
# victory.exe, where OptiScaler looks for them as in an ordinary game: without them it turned DLSS
# off, and Streamline turned DLSS Frame Generation off ("Failed to obtain DLSS-G min spec").
#
# Example:
#   .\setup-neural-rendering.ps1 -Release 'C:\Downloads\OptiScaler-DLSS5-1.3.0-win-x64.zip' `
#       -Model 'C:\...\nvngx_dlssnr.dll' -StreamlineDir 'C:\...\streamline\bin\x64'
param(
    [Parameter(Mandatory)][string]$Release,       # the release zip, or its extracted DLSS5-Optimised folder
    [Parameter(Mandatory)][string]$Model,         # nvngx_dlssnr.dll
    [Parameter(Mandatory)][string]$StreamlineDir  # the folder with sl.interposer.dll and nvngx_dlss*.dll
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path (Split-Path (Split-Path $PSScriptRoot -Parent) -Parent) -Parent
$dest = Join-Path $repo 'optiscaler-dlss5'
$release = Join-Path $repo 'target\release'

$src = $Release
if ($Release -like '*.zip') {
    $src = Join-Path ([IO.Path]::GetTempPath()) ('optiscaler-dlss5-' + [guid]::NewGuid())
    Expand-Archive -LiteralPath $Release -DestinationPath $src
}
$pkg = Get-ChildItem -LiteralPath $src -Recurse -Filter 'OptiScaler.dll' | Select-Object -First 1 | ForEach-Object DirectoryName
if (-not $pkg) { throw "No OptiScaler.dll in $Release." }
New-Item -ItemType Directory -Path $dest -Force | Out-Null
Copy-Item -LiteralPath (Join-Path $pkg 'OptiScaler.dll') -Destination (Join-Path $dest 'winmm.dll') -Force
Copy-Item -LiteralPath (Join-Path $pkg 'nvngx.dll_dlssnr.dll') -Destination $dest -Force
Copy-Item -LiteralPath $Model -Destination (Join-Path $dest 'nvngx_dlssnr.dll') -Force

# The keys set over the release's ini; everything else stays at its default (auto).
$keys = [ordered]@{
    'Upscalers' = [ordered]@{ Dx12Upscaler = 'dlss' }
    # Frame generation stays NVIDIA's, through the layer's own Streamline: OptiScaler makes no frames
    # and does not replace the multiplier.
    'FrameGen'  = [ordered]@{ Enabled = 'false'; FGInput = 'nofg'; FGOutput = 'nofg' }
    'Menu'      = [ordered]@{ ShortcutKey = '0x2D' }      # Insert
    'Libraries' = [ordered]@{ NvngxDlssPath = $StreamlineDir }
    'Log'       = [ordered]@{ LogToFile = 'true'; LogLevel = '2' }
    # The release's "Max quality" preset: the model after the upscaler, at 100 %, every frame. The CPU
    # is the limit on a Ryzen 5 5600X (the card is 30-45 % busy), so the model's cost does not show.
    # Pre-SR, 67 %, cache intervals and passes are in its menu (Insert, DLSS 5 tab).
    'DlssNr'    = [ordered]@{ Enabled = 'true'; CacheEnabled = 'true'; CacheInterval = '1'; PreSr = 'false'
                              WorkingScale = '1.0'; JbuUpsample = 'true'; CacheRefreshBlend = '1.0'; WhitePointSource = '3' }
}
$out = New-Object System.Collections.Generic.List[string]
$written = @{}
$section = $null
function Add-Missing($sec) {
    if (-not $sec -or -not $keys.Contains($sec)) { return }
    foreach ($k in $keys[$sec].Keys) { if (-not $written["$sec.$k"]) { $out.Add("$k=$($keys[$sec][$k])"); $written["$sec.$k"] = $true } }
}
foreach ($line in Get-Content -LiteralPath (Join-Path $pkg 'OptiScaler.ini')) {
    if ($line -match '^\[(.+)\]\s*$') { Add-Missing $section; $section = $Matches[1] }
    elseif ($section -and $keys.Contains($section) -and $line -match '^([A-Za-z0-9_]+)=' -and $keys[$section].Contains($Matches[1])) {
        $line = "$($Matches[1])=$($keys[$section][$Matches[1]])"; $written["$section.$($Matches[1])"] = $true
    }
    $out.Add($line)
}
Add-Missing $section
[IO.File]::WriteAllLines((Join-Path $dest 'OptiScaler.ini'), $out)

# NVIDIA's DLLs beside victory.exe (hard links where the volume allows, copies otherwise).
New-Item -ItemType Directory -Path $release -Force | Out-Null
foreach ($f in 'nvngx_dlss.dll', 'nvngx_dlssg.dll', 'nvngx_dlssd.dll') {
    $from = Join-Path $StreamlineDir $f; $to = Join-Path $release $f
    if (-not (Test-Path -LiteralPath $from)) { Write-Host "Not in the Streamline folder: $f"; continue }
    if (Test-Path -LiteralPath $to) { continue }
    try { New-Item -ItemType HardLink -Path $to -Target $from | Out-Null } catch { Copy-Item -LiteralPath $from -Destination $to }
}
Write-Host "Done: $dest. Turn on F1 > NEURAL RENDERING (INSERT) and restart; Insert opens OptiScaler's menu (DLSS 5 tab)."
