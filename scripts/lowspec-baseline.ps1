# SPDX-License-Identifier: GPL-3.0-or-later
[CmdletBinding()]
param([string]$Repo = (Split-Path $PSScriptRoot -Parent))
$ErrorActionPreference = 'Stop'
$sha = '64d4931b3dd90305134512b805fc98f04119fb6e'
$dest = Join-Path ([IO.Path]::GetTempPath()) ('victory-baseline-' + [guid]::NewGuid().ToString('N'))
& git -C $Repo worktree add --detach $dest $sha
if ($LASTEXITCODE -ne 0) { throw 'Baseline worktree failed' }
Push-Location $dest
try {
    foreach ($argsList in @(
        @{Args=@('fmt','--all','--','--check')},
        @{Args=@('clippy','--workspace','--all-targets','--locked','--','-D','warnings')},
        @{Args=@('test','--workspace','--locked')},
        @{Args=@('build','--release','--locked','-p','victory')}
    )) {
        $a = $argsList.Args
        & cargo @a
        if ($LASTEXITCODE -ne 0) { throw "Baseline cargo $a failed with $LASTEXITCODE" }
    }
    Write-Host "Unchanged baseline built at $dest\target\release\victory.exe"
} finally { Pop-Location; Write-Host "Baseline worktree retained: $dest" }
