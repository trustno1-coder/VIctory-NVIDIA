# Starts the title with the settings this fork is played with (README.md, "How to run it").
#   scripts\play.ps1 -Exe <path of your own copy's executable>
# -Safe         without the newer, faster ways (the first thing to try when something is wrong)
# -RayTracing   the title's own ray-traced indirect light: lit interiors, about a third slower
# -Build        target\diag (the default, the build all measurements are of) or target\release
# -More         other settings: -More @{VICTORY_PANEL='1'}
# -LogDir       the folder this run's log goes to (default: %LOCALAPPDATA%\victory\logs, or VICTORY_LOG_DIR)
# -LogTag       a word added to the log's name, to tell runs apart
# -LogLevel     error, warn, info, debug or trace (debug and trace: the diag build only); -LogFilter a whole
#               VICTORY_LOG filter instead ('victory_gpu::host=debug,info'). A VICTORY_LOG set in this window stays.
# It builds target\<Build>\victory.exe from this checkout first, at every start (cargo: README; without cargo it
# starts the one there with a warning). The log says which commit that build is of.
# Run it from a normal window, not as administrator: the driver then keeps its shader cache elsewhere.
param(
    [Parameter(Mandatory)][string]$Exe,
    [switch]$Safe,
    [switch]$RayTracing,
    [ValidateSet('diag', 'release')][string]$Build = 'diag',
    [hashtable]$More = @{},
    [string]$LogDir = '',
    [ValidatePattern('^[A-Za-z0-9_-]*$')][string]$LogTag = '',
    [ValidateSet('off', 'error', 'warn', 'info', 'debug', 'trace')][string]$LogLevel = '',
    [string]$LogFilter = ''
)
# (Read before the settings left over in this window are cleared below.)
if (-not $LogDir) { $LogDir = if ($env:VICTORY_LOG_DIR) { $env:VICTORY_LOG_DIR } else { Join-Path $env:LOCALAPPDATA 'victory\logs' } }
$logFilter = if ($LogFilter) { $LogFilter } elseif ($LogLevel) { $LogLevel } elseif ($env:VICTORY_LOG) { $env:VICTORY_LOG } else { '' }
$settings = @{
    VICTORY_TEXTURE_NO_WAIT = '1'; VICTORY_RELAY = '2'; VICTORY_WIDE_BACK = '1'; VICTORY_CARD_REHASH_RUNS = '1'
    VICTORY_JOIN_COPIES = '1'; VICTORY_INDIRECT_WINDOW = '1'; VICTORY_RELAY_COPY = '1'; VICTORY_CARD_STEPS = '2'
    VICTORY_INDIRECT_BATCH = '2'; VICTORY_MAP_THREADS = '1'; VICTORY_TITLE_PRIORITIES = '0'; VICTORY_CARD_AWAKE = '1'
    VICTORY_SHOW_AFTER = '1'; VICTORY_AMBIENT = '0.8'; VICTORY_D3D12_DRED = 'light'; VICTORY_SKIP_PIPELINES = 'none'
}
if ($Safe) {
    $settings.VICTORY_UNDER_WIDE = '0'; $settings.VICTORY_LAND_ECHO = '0'
} else {
    $settings.VICTORY_THIN = '1'; $settings.VICTORY_JOIN_PARTS = '8'; $settings.VICTORY_UPLOAD_AHEAD = '1'
}
if ($RayTracing) { $settings.VICTORY_BVH_BUILDER = '1' }
if ($logFilter) { $settings.VICTORY_LOG = $logFilter }
foreach ($k in $More.Keys) { $settings[$k] = $More[$k] }

$repo = Split-Path $PSScriptRoot -Parent
$victory = Join-Path $repo "target\$Build\victory.exe"
# Built from this checkout at every start, not only when missing (victory-build.ps1 says why); seconds when
# nothing changed.
. (Join-Path $PSScriptRoot 'victory-build.ps1')
$built = Build-Victory $repo $Build
if (-not $built) { return }
if (-not (Test-Path $Exe)) { Write-Host "No $Exe."; return }
# (Other settings left over in this window go; where your files are stays.)
$paths = 'VICTORY_CONFIG', 'VICTORY_GAME_DIR', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR'
Get-ChildItem Env:VICTORY_* | Where-Object { $_.Name -notin $paths } | ForEach-Object { Remove-Item "Env:$($_.Name)" }
foreach ($k in $settings.Keys) { Set-Item "Env:$k" $settings[$k] }
$Exe = (Resolve-Path $Exe).Path
# The layer writes every line it shows into the log too (VICTORY_LOG_FILE), after these first lines.
New-Item -ItemType Directory -Force $LogDir | Out-Null
$stamp = '{0:yyyyMMdd-HHmmss}' -f (Get-Date)
$log = Join-Path $LogDir $(if ($LogTag) { "victory-$stamp-$LogTag.log" } else { "victory-$stamp.log" })
$header = "# build=$Build switches: " + (($settings.GetEnumerator() | Sort-Object Name | ForEach-Object { "$($_.Name)=$($_.Value)" }) -join ' ')
# What victory.exe was built from and what the checkout is at, with a warning when they differ.
$source = Get-VictorySource $repo $victory ($built -eq 'stale')
$sourceLines = ($source.Keys | ForEach-Object { "# run: ${_}: $($source[$_])`n" }) -join ''
[IO.File]::WriteAllText($log, "$header`n# run: started: $((Get-Date).ToString('o'))`n$sourceLines", [Text.UTF8Encoding]::new($false))
$env:VICTORY_LOG_FILE = $log
Write-Host "Log: $log"
Push-Location $repo   # victory.toml is looked for here
try { & $victory run $Exe --execute } finally { Pop-Location }
