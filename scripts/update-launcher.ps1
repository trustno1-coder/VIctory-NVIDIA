# Updates this checkout from the official repository, then rebuilds and restarts the launcher. The launcher
# (apps/launcher) starts this when you choose "Update now", and closes itself so its file can be rebuilt.
#
#   scripts\update-launcher.ps1 -Remote upstream -Branch main -LauncherPid 1234 -LauncherExe <path>
#
# Only a fast-forward pull: nothing of yours is merged or overwritten. The layer (victory.exe) is rebuilt only if
# it was built before; otherwise the next Play builds it.
param(
    [Parameter(Mandatory)][string]$Remote,
    [Parameter(Mandatory)][string]$Branch,
    [Parameter(Mandatory)][int]$LauncherPid,
    [Parameter(Mandatory)][string]$LauncherExe
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
Set-Location $repo

function Stop-WithMessage([string]$text) {
    Write-Host ''
    Write-Host $text -ForegroundColor Red
    Read-Host 'Press Enter to close this window'
    exit 1
}

Write-Host 'Waiting for the launcher to close ...'
Wait-Process -Id $LauncherPid -ErrorAction SilentlyContinue

Write-Host "Updating from $Remote/$Branch ..."
git pull --ff-only $Remote $Branch
if ($LASTEXITCODE -ne 0) { Stop-WithMessage 'The update could not be pulled (see above). Nothing was changed.' }

# The layer, where the play script looks for it (its own target folder).
$victory = Join-Path $repo 'target\release\victory.exe'
if (Test-Path $victory) {
    Write-Host 'Rebuilding the layer (a few minutes) ...'
    $saved = $env:CARGO_TARGET_DIR
    Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
    cargo build --release --locked -p victory
    $built = $LASTEXITCODE -eq 0
    if ($saved) { $env:CARGO_TARGET_DIR = $saved }
    if (-not $built) { Stop-WithMessage 'The layer did not build (see above).' }
}

# The launcher, into the folder and profile it was built with: <target>\<profile>\victory-launcher.exe.
$profileDir = Split-Path $LauncherExe -Parent
$env:CARGO_TARGET_DIR = Split-Path $profileDir -Parent
Write-Host 'Rebuilding the launcher ...'
if ((Split-Path $profileDir -Leaf) -eq 'release') { cargo build --release --locked -p launcher }
else { cargo build --locked -p launcher }
if ($LASTEXITCODE -ne 0) { Stop-WithMessage 'The launcher did not build (see above).' }

Write-Host 'Done. Starting the launcher again.'
Start-Process -FilePath $LauncherExe
