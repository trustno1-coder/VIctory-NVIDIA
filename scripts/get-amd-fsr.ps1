# Official, pinned FidelityFX SDK v1.1.4 runtime (FSR 3.1.4).
param([string]$Destination = (Join-Path (Split-Path $PSScriptRoot -Parent) 'amd-sdk'))
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
$url = 'https://raw.githubusercontent.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK/v1.1.4/PrebuiltSignedDLL/amd_fidelityfx_dx12.dll'
$hash = '12A5081257EC95B0B53AD51B4A87FB3C03F97FE0BBB59F9496968F8D50EF93A6'
function Get-Sha256([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    $sha = [Security.Cryptography.SHA256]::Create()
    try { return [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '') }
    finally { $sha.Dispose(); $stream.Dispose() }
}
New-Item -ItemType Directory -Force -Path $Destination | Out-Null
$target = Join-Path $Destination 'amd_fidelityfx_dx12.dll'
if ((Test-Path $target) -and (Get-Sha256 $target) -eq $hash) {
    Write-Host "FSR runtime already installed: $target"
    return
}
$temp = "$target.download"
try {
    Invoke-WebRequest $url -OutFile $temp -UseBasicParsing
    if ((Get-Sha256 $temp) -ne $hash) { throw 'FSR runtime checksum mismatch.' }
    Move-Item -Force $temp $target
} finally {
    if (Test-Path $temp) { Remove-Item $temp }
}
Write-Host "Installed FSR 3.1.4: $target"
