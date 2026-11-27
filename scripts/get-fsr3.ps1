# Fetches AMD's FidelityFX SDK, builds FSR 3's bridge (tools\fsr3-bridge)
# against it, and puts fsr3_bridge.dll with AMD's amd_fidelityfx_dx12.dll
# into fsr3-sdk\ here (git-ignored), where scripts\play-foxhound.ps1 and
# `victory play` look for them. AMD's files are under AMD's licences
# (saved beside them: read them); this repository does not and may not
# carry them, so each person gets them from AMD directly. Get-FSR3.cmd in
# the repository's folder runs this. Nothing of AMD's is needed to build
# the layer: the bridge is loaded when the game runs, and the bridge in
# turn loads AMD's DLL from its own folder.
#
# SDK 1.1.4 is the pinned version: FSR 3.1, the single-DLL FidelityFX API
# (amd_fidelityfx_dx12.dll, five ffx* functions). A later SDK (2.0 and up)
# splits the API into several DLLs and is NOT what the bridge expects:
# if -SdkTag names one of those, the build step below fails and says so.
# If the tag does not exist on GitHub, the download fails loudly here
# rather than silently producing a half-built folder.
param(
    [string]$SdkTag = 'v1.1.4',
    # Another folder for the DLLs than fsr3-sdk here (`victory get-fsr3`
    # names one under %LOCALAPPDATA%\victory).
    [string]$InstallDir = '',
    # A FidelityFX SDK checkout already on this PC, instead of downloading
    # the archive (its folder must contain ffx-api\include).
    [string]$SdkDir = '',
    # The bridge's sources (tools\fsr3-bridge), when they are not beside
    # this script in the repository (`victory get-fsr3` writes them out).
    [string]$BridgeDir = ''
)
# The cmdlets this script uses (Join-Path, Test-Path, ...) come from
# Microsoft.PowerShell.Management. A session with module auto-loading
# off does not have them, and the script then fails on its first line
# ("Join-Path is not recognized") - as happened when it was run from
# inside another PowerShell window. Load the module explicitly, from
# its usual places if the module path itself is broken.
if (-not (Get-Command Join-Path -ErrorAction SilentlyContinue)) {
    Import-Module Microsoft.PowerShell.Management -ErrorAction SilentlyContinue
}
if (-not (Get-Command Join-Path -ErrorAction SilentlyContinue)) {
    foreach ($base in @("$env:SystemRoot\System32\WindowsPowerShell\v1.0\Modules", "$PSHOME\Modules")) {
        $psd1 = [System.IO.Path]::Combine($base, 'Microsoft.PowerShell.Management', 'Microsoft.PowerShell.Management.psd1')
        if ([System.IO.File]::Exists($psd1)) { Import-Module $psd1 -ErrorAction Stop; break }
    }
}
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'   # Invoke-WebRequest's progress bar makes it many times slower
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$install = if ($InstallDir) { $InstallDir } else { Join-Path $repo 'fsr3-sdk' }
New-Item -ItemType Directory -Force $install | Out-Null

# One file from a URL, with curl's progress line where Windows has curl (it has since Windows 10 1803).
function Get-File([string]$Url, [string]$To) {
    New-Item -ItemType Directory -Force (Split-Path $To) | Out-Null
    $part = "$To.part"
    if (Get-Command curl.exe -ErrorAction SilentlyContinue) {
        & curl.exe --location --fail --silent --show-error --progress-bar --output $part $Url
        if ($LASTEXITCODE -ne 0) { Remove-Item $part -ErrorAction SilentlyContinue; throw "could not get $Url" }
    } else {
        Invoke-WebRequest -Uri $Url -OutFile $part -UseBasicParsing -ErrorAction Stop
    }
    Move-Item $part $To -Force
}

if ($SdkDir) {
    $sdk = $SdkDir
} else {
    Write-Host "FidelityFX SDK $SdkTag, from github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK:"
    $url = "https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK/archive/refs/tags/$SdkTag.zip"
    $zip = Join-Path $install "fidelityfx-sdk-$SdkTag.zip"
    try { Get-File $url $zip } catch {
        throw "Could not download the FidelityFX SDK tag $SdkTag from $url. Check the tag exists (SDK 1.1.4 is the pinned one; -SdkTag names another): $($_.Exception.Message)"
    }
    $scratch = Join-Path $install 'sdk-src'
    Remove-Item $scratch -Recurse -Force -ErrorAction SilentlyContinue
    # Not Expand-Archive -Force: that calls the 3-argument
    # ExtractToDirectory, which an older .NET Framework does not have
    # ("Cannot find an overload ... argument count: 3"). The
    # 2-argument form has existed since .NET 4.5, and the folder was
    # just removed above, so nothing needs overwriting.
    Add-Type -AssemblyName System.IO.Compression.FileSystem -ErrorAction Stop
    [System.IO.Compression.ZipFile]::ExtractToDirectory($zip, $scratch)
    Remove-Item $zip -Force
    $sdk = Get-ChildItem $scratch -Directory | Where-Object { Test-Path (Join-Path $_.FullName 'ffx-api') } | Select-Object -First 1
    if (-not $sdk) { throw "The $SdkTag archive has no ffx-api folder; it is not the FidelityFX SDK this bridge expects." }
    $sdk = $sdk.FullName
}
if (-not (Test-Path (Join-Path $sdk 'ffx-api\include\ffx_api\ffx_api.h'))) {
    throw "No ffx-api\include\ffx_api\ffx_api.h in $sdk; that is not a FidelityFX SDK 1.1.4 checkout."
}
$amdDll = Get-ChildItem $sdk -Recurse -Filter 'amd_fidelityfx_dx12.dll' | Select-Object -First 1
if (-not $amdDll) {
    throw "No amd_fidelityfx_dx12.dll in $sdk (looked under PrebuiltSignedDLL). A later SDK splits the API into several DLLs; use SDK 1.1.4 (-SdkTag v1.1.4)."
}
Write-Host "  SDK: $sdk"
Write-Host "  AMD's DLL: $($amdDll.FullName)"

# Build the bridge. CMake and a C++ compiler (Visual Studio with the
# Desktop development with C++ workload) must be installed; the build
# fails loudly - and names the header or macro - if the SDK's API
# differs from what tools/fsr3-bridge/fsr3_bridge.cpp expects.
function Find-Cmake {
    # cmake on PATH first; then the copy Visual Studio bundles, which
    # is not on PATH (the Rust toolchain needs VS too, so it is
    # usually already there); then the well-known install folders.
    if (Get-Command cmake -ErrorAction SilentlyContinue) { return 'cmake' }
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path $vswhere) {
        $install = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null | Select-Object -First 1
        if ($install) {
            $bundled = Join-Path $install 'Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe'
            if (Test-Path $bundled) { return $bundled }
        }
    }
    foreach ($edition in 'Community', 'Professional', 'Enterprise', 'BuildTools') {
        foreach ($year in '2022', '2019') {
            $bundled = "C:\Program Files\Microsoft Visual Studio\$year\$edition\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe"
            if (Test-Path $bundled) { return $bundled }
        }
    }
    return $null
}
$cmake = Find-Cmake
if (-not $cmake) {
    throw 'cmake is not on PATH, and no Visual Studio copy of it was found. Install CMake (winget install Kitware.CMake) and Visual Studio with the Desktop development with C++ workload, then run this again.'
}
Write-Host "  cmake: $cmake"
$bridgeDir = if ($BridgeDir) { $BridgeDir } else { Join-Path $repo 'tools\fsr3-bridge' }
if (-not (Test-Path (Join-Path $bridgeDir 'fsr3_bridge.cpp'))) {
    throw "No fsr3_bridge.cpp in $bridgeDir; that is not the bridge's folder (tools\fsr3-bridge)."
}
$build = Join-Path $install 'build'
Remove-Item $build -Recurse -Force -ErrorAction SilentlyContinue
& $cmake -S $bridgeDir -B $build -DFSR3_SDK_DIR="$sdk" -DCMAKE_BUILD_TYPE=Release
if ($LASTEXITCODE -ne 0) { throw 'cmake configure failed (see above)' }
& $cmake --build $build --config Release
if ($LASTEXITCODE -ne 0) { throw 'cmake build failed (see above): the bridge did not compile against this SDK' }
$bridge = Get-ChildItem $build -Recurse -Filter 'fsr3_bridge.dll' | Select-Object -First 1
if (-not $bridge) { throw 'The build produced no fsr3_bridge.dll.' }
Copy-Item $bridge.FullName (Join-Path $install 'fsr3_bridge.dll') -Force
Copy-Item $amdDll.FullName (Join-Path $install 'amd_fidelityfx_dx12.dll') -Force
$license = Get-ChildItem $sdk -Filter 'LICENSE*' | Select-Object -First 1
if ($license) { Copy-Item $license.FullName (Join-Path $install $license.Name) -Force }
Remove-Item $build -Recurse -Force -ErrorAction SilentlyContinue
if ($scratch) { Remove-Item $scratch -Recurse -Force -ErrorAction SilentlyContinue }

$missing = @('fsr3_bridge.dll', 'amd_fidelityfx_dx12.dll') | Where-Object { -not (Test-Path (Join-Path $install $_)) }
if ($missing) { throw "Not there after all: $($missing -join ', ')" }
Write-Host ''
Write-Host "Done. In $install : fsr3_bridge.dll, amd_fidelityfx_dx12.dll."
Write-Host 'Next: .\scripts\play-foxhound.ps1 -Exe <path of your own copy''s executable> -Fsr3FrameGen'
Write-Host '  (or: victory play --fsr3-framegen). FSR 3 frame generation, 2x, with the title''s own upscaling.'
