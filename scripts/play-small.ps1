# Starts your own copy of the title on a small PC: 8 GB of VRAM and 16 GB of RAM (tested on an RTX 4060 8 GB,
# Ryzen 5 7600, 16 GB, 1080p screen). Everything it sets, why, and what each step measured: docs/small-config.md.
#
#   .\scripts\play-small.ps1                       the tested set (below), the game folder from victory.toml
#   .\scripts\play-small.ps1 -Exe 'D:\MyDump\game_scarlett_master.exe'
#   .\scripts\play-small.ps1 -NoFrameGen           no frame generation (textures may take 85 % of the budget)
#   .\scripts\play-small.ps1 -SkipLevels 0         full-size textures (more VRAM; see the doc)
#
# The tested set: Series S profile, DLSS Performance, frame generation 2x, texture pool off, the largest level of
# block-compressed textures of 2048 texels or more left out, textures up to 82 % of the VRAM budget, 1 GB of title
# windows in BAR memory, a 1.5 GB reserve for render targets, borderless full screen.
#
# It goes on to play-foxhound.ps1 (which builds the layer the first time); the F1 menu's saved choices still go
# over these (delete victory-settings.txt to go back). Do not turn frame generation on or the console to Series X
# from the menu on such a PC.
param(
    # The title's executable; default: game_dir of victory.toml (or VICTORY_GAME_DIR).
    [string]$Exe = '',
    [ValidateSet('off', 'dlaa', 'quality', 'balanced', 'performance')][string]$Dlss = 'performance',
    # No frame generation (it takes 1.5 to 2 GB of VRAM); textures then get -RoomPct 85 unless given.
    [switch]$NoFrameGen,
    # VICTORY_TEXTURE_ROOM_PERCENT: the share of the VRAM budget textures may fill (default 82, 85 without FG).
    [int]$RoomPct = 0,
    # VICTORY_TEXTURE_SKIP_LEVELS / _MIN: largest levels left out of big block-compressed textures (0: none).
    [ValidateRange(0, 4)][int]$SkipLevels = 1,
    [int]$SkipMin = 2048,
    # VICTORY_TEXTURE_POOL=0 unless -Pool: textures in their own allocations, given back when let go.
    [switch]$Pool,
    # VICTORY_WINDOWS_BAR_MB: title windows in CPU-mapped card memory (resizable BAR), 0: off.
    [int]$BarMB = 1024,
    # VICTORY_CARD_ROOM_FOR_TARGETS_MB: VRAM the texture queue keeps for render targets (repo default 3072).
    [int]$TargetsMB = 1536,
    # Leave the game windowed (default: borderless full screen made by the layer, VICTORY_FULLSCREEN=1).
    [switch]$Windowed,
    # Raise the game process to High priority once it is up.
    [switch]$High,
    # Translate every shader and make the known pipelines ahead (`victory preload`, with these switches), then
    # play: once per new build or driver, so that the first run does not stutter (play-foxhound.ps1 -Preload).
    [switch]$Preload,
    # Any other switch, as play-foxhound.ps1's -More.
    [hashtable]$More = @{}
)
$repo = Split-Path $PSScriptRoot -Parent

if (-not $Exe) {
    $dir = $env:VICTORY_GAME_DIR
    if (-not $dir) {
        $toml = if ($env:VICTORY_CONFIG) { $env:VICTORY_CONFIG } else { Join-Path $repo 'victory.toml' }
        if (Test-Path -LiteralPath $toml) {
            $line = Get-Content -LiteralPath $toml | Where-Object { $_ -match "^\s*game_dir\s*=\s*['""](.+)['""]" } | Select-Object -First 1
            if ($line -match "^\s*game_dir\s*=\s*['""](.+)['""]") { $dir = $Matches[1] }
        }
    }
    if (-not $dir) { Write-Host 'No game folder: pass -Exe, or set game_dir in victory.toml (README).'; return }
    $Exe = Join-Path $dir 'game_scarlett_master.exe'
}
if ($RoomPct -le 0) { $RoomPct = if ($NoFrameGen) { 85 } else { 82 } }

$set = @{
    VICTORY_PLACED_ORDER = '1'
    VICTORY_CARD_ROOM_FOR_TARGETS_MB = [string]$TargetsMB
    VICTORY_TEXTURE_ROOM_PERCENT = [string]$RoomPct
}
if ($BarMB -gt 0) { $set.VICTORY_WINDOWS_BAR_MB = [string]$BarMB }
if (-not $Pool) { $set.VICTORY_TEXTURE_POOL = '0' }
if ($SkipLevels -gt 0) { $set.VICTORY_TEXTURE_SKIP_LEVELS = [string]$SkipLevels; $set.VICTORY_TEXTURE_SKIP_MIN = [string]$SkipMin }
# Borderless full screen made by the layer when it creates the title's window (lifecycle.rs `fullscreen`): unlike
# scripts\fullscreen.ps1, which waits at most 3 minutes for the window, it also holds after a long -Preload.
if (-not $Windowed) { $set.VICTORY_FULLSCREEN = '1' }
foreach ($k in $More.Keys) { $set[$k] = $More[$k] }

$fg = if ($NoFrameGen) { @{ NoFrameGenReady = $true } } else { @{ FrameGen = $true; FrameGenX = 2 } }

if ($High) {
    $null = Start-Job -ArgumentList $repo -ScriptBlock {
        param($r)
        $end = (Get-Date).AddMinutes(120)   # a -Preload comes first: the game may start much later
        while ((Get-Date) -lt $end) {
            $p = Get-Process victory -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$r*" }
            if ($p) { $p | ForEach-Object { $_.PriorityClass = 'High' }; break }
            Start-Sleep -Seconds 2
        }
    }
}
Write-Host ("Small PC: Series S, DLSS {0}, frame generation {1}, textures {2} % (skip {3} level(s) >= {4}), pool {5}, BAR {6} MB, targets {7} MB" -f `
        $Dlss, $(if ($NoFrameGen) { 'off' } else { '2x' }), $RoomPct, $SkipLevels, $SkipMin, $(if ($Pool) { 'on' } else { 'off' }), $BarMB, $TargetsMB)
if ($Preload) { $fg.Preload = $true }
& (Join-Path $PSScriptRoot 'play-foxhound.ps1') -Exe $Exe -SeriesS -Dlss $Dlss -More $set @fg
