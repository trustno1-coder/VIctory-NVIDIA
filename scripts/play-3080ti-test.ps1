# Conservative first-run profile for the RTX 3080 Ti (12 GB).
# This wrapper does not download SDKs, alter game files, or enable any frame generation.
# Run from the repository root in PowerShell:
#   .\scripts\play-3080ti-test.ps1 -Exe 'D:\Path\To\game_scarlett_master.exe'
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateNotNullOrEmpty()][string]$Exe,
    [ValidateSet('release', 'diag')][string]$Build = 'release',
    [ValidateSet('1440', '1080to1440')][string]$Res = '1440',
    [ValidateSet('quality', 'balanced', 'performance')][string]$Dlss = 'quality',
    [ValidateRange(30, 120)][int]$Fps = 60
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
$runner = Join-Path $PSScriptRoot 'play-foxhound.ps1'

if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) {
    throw "Executable not found: $Exe"
}
if (-not (Test-Path -LiteralPath $runner -PathType Leaf)) {
    throw "Existing launcher not found: $runner"
}

$ram = Get-CimInstance Win32_ComputerSystem
$ramGB = [math]::Round($ram.TotalPhysicalMemory / 1GB, 1)
$gpu = Get-CimInstance Win32_VideoController |
    Where-Object { $_.Name -match 'RTX 3080 Ti' } |
    Select-Object -First 1

Write-Host "RTX 3080 Ti first-test profile"
Write-Host "System RAM: $ramGB GB (project documentation asks for 32 GB; this PC has less)."
if (-not $gpu) {
    Write-Warning 'Could not confirm an RTX 3080 Ti from Windows device information.'
}
if ($ramGB -lt 32) {
    Write-Warning '16 GB RAM may cause paging, stutter, or crashes. This profile cannot remove that limitation.'
}

# Explicitly disable both frame-generation paths for the first baseline.
# FgType=dlss prevents the launcher from preparing AMD FSR frame generation.
# NoFrameGenReady prevents DLSS frame generation from being prepared.
& $runner -Exe $Exe -Build $Build -Res $Res -Dlss $Dlss -Fps $Fps -FgType dlss -NoFrameGenReady
exit $LASTEXITCODE
