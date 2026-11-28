# Launch helper behind the RTX 5070 Ti + Ryzen 5 5600X results (contrib/rtx5070ti-ryzen5600x).
# It is not part of the layer: it wraps this repository's scripts\play-foxhound.ps1 with the options
# played with, and adds three things that made starts dependable on that PC:
#
#  - A one-time warm-up start for every new victory.exe (identified by its SHA-256). A new build's
#    first run stays at a flat ~14 fps (upstream finding, the driver's program cache); the in-game
#    warm-up restart starts play-foxhound again without this script's options, so here the game is
#    closed after 60 s and started again with the same options.
#  - An automatic reopen (up to 3 times) when the title fails at start with its known write to 0 at
#    game_scarlett_master.exe+0x2215c35 (about one start in three on this PC, every build) and, with
#    -NeuralRendering, when the title's watchdog (+0x23cf683) ends a start that stalled in the intro.
#  - -Seconds N: closes the game after N seconds (fixed-spot measurements; compare-logs.py).
#
# Example (the settings of the screenshots; the F1 menu's saved choices go over these):
#   .\play.ps1 -GameExe 'D:\Games\<game>\game_scarlett_master.exe' -FrameGenX 3 -DlssModel k -Fullscreen -NeuralRendering
param(
    [Parameter(Mandatory)][string]$GameExe,
    [string]$DataDir = $env:VICTORY_DATA_DIR,
    [string]$CacheDir = $env:VICTORY_CACHE_DIR,
    [string]$StreamlineDir = $env:VICTORY_STREAMLINE_DIR,
    [ValidateSet('series-s', 'series-x')][string]$Console = 'series-x',
    [ValidateRange(0, 6)][int]$FrameGenX = 3,
    [ValidateSet('', 'dlaa', 'quality', 'balanced', 'performance', 'off')][string]$Dlss = '',
    [ValidateSet('', 'k', 'j', 'l', 'm')][string]$DlssModel = 'k',
    [switch]$Fullscreen,
    [switch]$NeuralRendering,
    [int]$Seconds = 0
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path (Split-Path (Split-Path $PSScriptRoot -Parent) -Parent) -Parent
$victory = Join-Path $repo 'target\release\victory.exe'
$play = Join-Path $repo 'scripts\play-foxhound.ps1'
if (-not (Test-Path -LiteralPath $victory)) { throw "Not built yet: $victory (cargo build --release -p victory)." }
if (-not (Test-Path -LiteralPath $GameExe)) { throw "No game executable at $GameExe." }
if (Get-Process victory -ErrorAction SilentlyContinue) { throw 'A game is already running: close it first.' }

Get-ChildItem Env:VICTORY_* -ErrorAction SilentlyContinue | ForEach-Object { Remove-Item -LiteralPath ('Env:' + $_.Name) }
$env:VICTORY_GAME_DIR = Split-Path -Parent $GameExe
if ($DataDir) { $env:VICTORY_DATA_DIR = $DataDir }
if ($CacheDir) { $env:VICTORY_CACHE_DIR = $CacheDir }
if ($StreamlineDir) { $env:VICTORY_STREAMLINE_DIR = $StreamlineDir }

function Stop-Timers {
    Get-CimInstance Win32_Process -Filter "Name = 'powershell.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -like '*victory-contrib-timer*' } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
}
# Closes this repository's victory.exe after $n seconds, from a hidden process of its own.
function Start-Timer([int]$n) {
    $kill = "`$m = 'victory-contrib-timer'; Start-Sleep -Seconds $n; Get-Process victory -ErrorAction SilentlyContinue | Where-Object { `$_.Path -eq '$victory' } | Stop-Process -Force"
    Start-Process powershell.exe -WindowStyle Hidden -ArgumentList @('-NoProfile', '-Command', $kill) | Out-Null
}
# Waits until the last victory.exe has left memory (~20 GB takes a few seconds), and 5 s more:
# a start while the previous one is still closing makes the start failure more likely.
function Wait-Closed {
    $until = (Get-Date).AddSeconds(60)
    while ((Get-Process victory -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $victory }) -and (Get-Date) -lt $until) {
        Start-Sleep -Milliseconds 500
    }
    Start-Sleep -Seconds 5
}

# play-foxhound.ps1 merges the game's error output (its log lines) into its own output; with 'Stop'
# Windows PowerShell 5.1 took the first line for an error and ended the game.
$ErrorActionPreference = 'Continue'
$opts = @{ Exe = $GameExe; Fast = $true; More = @{ VICTORY_LOG = 'info' } }
if ($Console -eq 'series-s') { $opts.SeriesS = $true }
if ($FrameGenX -ge 2) { $opts.FrameGen = $true; $opts.FrameGenX = $FrameGenX }
if ($Dlss) { $opts.Dlss = $Dlss }
if ($DlssModel) { $opts.More.VICTORY_DLSS_PRESET = $DlssModel }
if ($Fullscreen) { $opts.More.VICTORY_FULLSCREEN = '1' }
if ($NeuralRendering) { $opts.More.VICTORY_NEURAL = '1' }

function Start-Game([int]$timer) {
    for ($try = 1; $try -le 4; $try++) {
        Stop-Timers
        if ($timer -gt 0) { Start-Timer $timer }
        $began = Get-Date
        & $play @opts
        $lasted = ((Get-Date) - $began).TotalSeconds
        $log = Get-ChildItem -LiteralPath (Join-Path $env:LOCALAPPDATA 'victory\logs') -Filter '*.log' -ErrorAction SilentlyContinue |
            Where-Object { $_.CreationTime -ge $began.AddSeconds(-2) } | Sort-Object CreationTime | Select-Object -Last 1
        $startFailure = $lasted -lt 150 -and $log -and (Select-String -LiteralPath $log.FullName -SimpleMatch 'unhandled exception 0xc0000005 at game_scarlett_master.exe+0x2215c35' -Quiet)
        $watchdog = $NeuralRendering -and $lasted -lt 300 -and $log -and (Select-String -LiteralPath $log.FullName -SimpleMatch 'unhandled exception 0xc0000005 at game_scarlett_master.exe+0x23cf683' -Quiet)
        if (-not ($startFailure -or $watchdog)) { return }
        if ($try -eq 4) { Write-Host 'The game failed at start four times in a row: start this again.'; return }
        if ($watchdog) { Write-Host "The start stalled in the intro with DLSS 5 and the title's watchdog ended it: starting again ($try of 3)..." }
        else { Write-Host "The title's known start failure (+0x2215c35): starting again ($try of 3)..." }
        Wait-Closed
    }
}

$marks = if ($CacheDir) { Join-Path $CacheDir 'warmed' } else { Join-Path $env:LOCALAPPDATA 'victory\warmed-contrib' }
$mark = Join-Path $marks (Get-FileHash -LiteralPath $victory -Algorithm SHA256).Hash.Substring(0, 16)
if (-not (Test-Path -LiteralPath $mark)) {
    Write-Host '=== WARM-UP (first start of this victory.exe only): the game CLOSES BY ITSELF in about 60 s.'
    Write-Host '=== It is not a crash: it starts again right after, with the same options.'
    Start-Game 60
    New-Item -ItemType Directory -Path $marks -Force | Out-Null
    Set-Content -LiteralPath $mark -Encoding ascii -Value 'warmed up: first run of this victory.exe done'
    Wait-Closed
}
Write-Host "$Console, frame generation ${FrameGenX}x$(if ($NeuralRendering) { ', DLSS 5 neural rendering (Insert: its menu)' }). F1: settings, F3: numbers."
Start-Game $Seconds
