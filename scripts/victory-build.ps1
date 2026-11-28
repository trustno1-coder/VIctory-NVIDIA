# Shared by the launchers (play-foxhound.ps1, play.ps1), dot-sourced: victory.exe built from the checkout at every
# start, and the log's lines on what it was built from. A test run logged as "hcx-in-progress at dec535f" was of a
# victory.exe built hours before any of that branch's changes (2026-11-28): the launcher built only when the program
# was missing, and its log named the checkout, not the build.

# The program's sources, as apps\victory\build.rs counts changes not committed: an edited note is none.
$victoryProgramPaths = 'apps', 'crates', 'Cargo.toml', 'Cargo.lock'

# Builds target\<Build>\victory.exe in $Repo: cargo is done in a few seconds when nothing changed. 'built' when it
# did; 'stale' when there is no cargo but a victory.exe is there (started with a warning); $null when the start
# should stop (the reason is shown). A failed build stops the start: an older victory.exe's run would pass for
# this checkout's.
function Build-Victory([string]$Repo, [string]$Build) {
    $victory = Join-Path $Repo "target\$Build\victory.exe"
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        if (Test-Path $victory) {
            Write-Host "No cargo to build with: starting $victory as it is, which may be older than this checkout." -ForegroundColor Yellow
            return 'stale'
        }
        Write-Host "No $victory, and no cargo to build it with: install Rust (README), then start this again."
        return $null
    }
    Write-Host $(if (Test-Path $victory) { 'Building the layer (only what changed since the last build) ...' } else { 'Building the layer (a few minutes the first time) ...' })
    Push-Location $Repo
    try {
        if ($Build -eq 'diag') { cargo build --locked --profile diag -p victory --features diagnostics | Out-Host }
        else { cargo build --release --locked -p victory | Out-Host }
        $built = $LASTEXITCODE -eq 0
    } finally { Pop-Location }
    if (-not $built) { Write-Host "The build failed (see above), so the game is not started with an older $victory."; return $null }
    if (-not (Test-Path $victory)) { Write-Host "The build did not make $victory."; return $null }
    'built'
}

# The log's lines on the program, in order: 'built from' (the commit build.rs put into `victory --version`),
# 'checkout' (the commit $Repo is at) and, when they differ, 'warning' (also shown here). apps\victory\src\play.rs
# (`victory play`) says the same.
function Get-VictorySource([string]$Repo, [string]$Victory, [bool]$Stale) {
    $lines = [ordered]@{}
    $builtFrom = $null; $builtDirty = $false
    $version = & $Victory --version 2>$null | Where-Object { "$_" -match 'built from' } | Select-Object -First 1
    if ("$version" -match 'built from ([0-9a-f]{40})') {
        $builtFrom = $Matches[1]; $builtDirty = "$version" -match 'not committed'
        $lines['built from'] = "$builtFrom$(if ($builtDirty) { ', with changes not committed' })"
    } else { $lines['built from'] = 'not known' }
    $warning = $null; $commit = $null
    if (Get-Command git -ErrorAction SilentlyContinue) {
        $commit = git -C $Repo rev-parse HEAD 2>$null
        if ($commit) {
            $branch = git -C $Repo rev-parse --abbrev-ref HEAD 2>$null
            $dirty = [bool](git -C $Repo status --porcelain --untracked-files=no -- @victoryProgramPaths 2>$null)
            $lines.checkout = "$branch at $commit$(if ($dirty) { ', with changes not committed' })"
            $short = { param($c) $c.Substring(0, [Math]::Min(10, $c.Length)) }
            if (-not $builtFrom) { $warning = "this victory.exe does not know the commit it was built from; the checkout is at $(& $short $commit)" }
            elseif ($builtFrom -ne $commit) {
                $warning = "this victory.exe was built from $(& $short $builtFrom), but the checkout is at $(& $short $commit): this run is of the build, not the checkout"
            } elseif ($dirty -and -not $builtDirty) { $warning = 'the checkout has changes not committed that this victory.exe was built without' }
        }
    }
    if (-not $warning -and $Stale -and -not $commit) { $warning = 'victory.exe was not built again before this run (no cargo)' }
    if ($warning) { $lines.warning = $warning; Write-Host "Warning: $warning." -ForegroundColor Yellow }
    $lines
}
