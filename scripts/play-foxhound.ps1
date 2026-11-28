# Starts your own copy of the title on foxhound with the settings tested on an RTX 4070 SUPER (Series X profile,
# 2560x1440 output, DLSS Quality when NVIDIA Streamline is set up).
#
#   .\scripts\play-foxhound.ps1 -Exe 'D:\MyDump\game_scarlett_master.exe'
#   .\scripts\play-foxhound.ps1 -Exe ... -Dlss off            the title's own upscaling, no DLSS
#   .\scripts\play-foxhound.ps1 -Exe ... -Dlss balanced -FrameGen
#   .\scripts\play-foxhound.ps1 -Exe ... -SeriesS             Series S profile (1280x720 inside, faster, softer)
#   .\scripts\play-foxhound.ps1 -Exe ... -Build diag          diagnostics build (slower; captures, timelines)
#   .\scripts\play-foxhound.ps1 -Exe ... -More @{VICTORY_AMBIENT_SPECULAR='0'}   add or override switches
#
# In the game, F1 opens a settings menu (arrow keys pick and change, Enter does it). What it saves goes to
# victory-settings.txt in the repository's folder and is applied here at every start, over the options above;
# delete that file to go back to them.
#
# Build first (README). Run from a normal window, not as administrator. Each run writes a log to
# %LOCALAPPDATA%\victory\logs. Every VICTORY_... switch is described in a comment where the code reads it.
param(
    [Parameter(Mandatory)][string]$Exe,
    [ValidateSet('diag', 'release')][string]$Build = 'release',
    [switch]$SeriesS,
    # 1440: output and scene 2560x1440 (Series X, the image is patched); 1080to1440: scene 1920x1080 upscaled by
    # the title; 4k: the stock Series X output (3840x2160).
    [ValidateSet('1440', '1080', '1080stockui', '1080to1440', '4k')][string]$Res = '1440',
    # DLSS Super Resolution in place of the title's upscaling; needs NVIDIA Streamline (README).
    [ValidateSet('off', 'dlaa', 'quality', 'balanced', 'performance')][string]$Dlss = 'quality',
    # DLSS Frame Generation (RTX 40 or later, hardware-accelerated GPU scheduling on); needs DLSS.
    [switch]$FrameGen,
    # Frames shown for each one rendered with -FrameGen: 2 to 6 (more than 2 needs an RTX 50; the layer asks the
    # driver how many this card does and gives no more).
    [ValidateRange(2, 6)][int]$FrameGenX = 2,
    # Frame generation is made ready at every start with Streamline, on or off, so that the F1 menu switches it
    # and changes its amount while playing. Only with -FrameGenReady now: ready but off, Streamline's swap chain
    # held this PC's frames at 14 a second (20 without), so frame generation off is off unless asked.
    [switch]$FrameGenReady,
    # (The default now; still taken.)
    [switch]$NoFrameGenReady,
    # Which frame generation: auto (DLSS Frame Generation where it runs, else AMD's FSR 3), dlss, or fsr (FSR 3
    # even on an RTX 40/50; 2X only). FSR 3 needs AMD's FidelityFX DLLs (VICTORY_FIDELITYFX_DIR, else
    # amd-sdk\FidelityFX\bin here, where Get-AMD-FSR.cmd puts them) and, on Series X, DLSS for the upscaling (the title's own takes Series S sizes).
    [ValidateSet('auto', 'dlss', 'fsr')][string]$FgType = 'auto',
    # Frames begin every 1/Fps s whatever the title asks (it asks 1/30). 1000, the default: no cap, a frame
    # begins as soon as the last is done. 30: the title's own rate.
    [ValidateRange(1, 1000)][int]$Fps = 1000,
    # Every shader and the loading pipelines made first (`victory preload`), then the game; the F1 menu's
    # PRELOAD SHADERS starts this again with it.
    [switch]$Preload,
    # Finish after preparing the caches. FullPreload covers all recorded pipelines.
    [switch]$PrepareOnly,
    [switch]$FullPreload,
    # 1080p scene and window, stock UI, 60 rendered FPS target and preloading for a Radeon PC.
    [switch]$Radeon1080p,
    [ValidateSet('game','fsr-quality')][string]$Upscaler = 'game',
    [switch]$Recovery,
    [hashtable]$More = @{}
)
$repo = Split-Path $PSScriptRoot -Parent
if ($Radeon1080p) {
    if (-not $PSBoundParameters.ContainsKey('Res')) { $Res = '1080stockui' }
    if (-not $PSBoundParameters.ContainsKey('Fps')) { $Fps = 60 }
    if (-not $PSBoundParameters.ContainsKey('Dlss')) { $Dlss = 'off' }
    if (-not $PSBoundParameters.ContainsKey('NoFrameGenReady')) { $NoFrameGenReady = $true }
    if (-not $PSBoundParameters.ContainsKey('Preload')) { $Preload = $env:VICTORY_PLAY_PREPARED -ne '1' }
    if (-not $env:VICTORY_CACHE_DIR) { $env:VICTORY_CACHE_DIR = Join-Path (Split-Path $repo -Parent) 'Victory-cache-1080p' }
}
# The F1 menu's "PRELOAD SHADERS" restarts this with -Preload (and VICTORY_PRELOAD=1, read here before the
# switches below are cleared).
$preload = $PrepareOnly -or $Preload -or $env:VICTORY_PRELOAD -eq '1'
# What the F1 menu saved: its own choices (VICTORY_MENU_...) stand in for this script's options, and its
# switches go over the settings below. An empty value unsets a switch.
$menuFile = Join-Path $repo 'victory-settings.txt'
if ($Radeon1080p) { $menuFile = Join-Path $repo 'victory-settings.radeon1080.txt' }
$menu = [ordered]@{}
if (Test-Path -LiteralPath $menuFile) {
    foreach ($line in Get-Content -LiteralPath $menuFile) {
        if ($line -match '^\s*(VICTORY_[A-Z0-9_]+)\s*=(.*)$') { $menu[$Matches[1]] = $Matches[2].Trim() }
    }
}
if ($Recovery) {
    if (!$Radeon1080p) { throw 'Recovery requires the Radeon profile.' }
    if ([IO.File]::Exists($menuFile)) {
        $backup = Join-Path $repo "victory-settings.radeon-backup-$([guid]::NewGuid()).txt"
        [IO.File]::Move($menuFile, $backup)
        Write-Host "Previous graphics choices saved to $backup"
    }
    $menu.Clear()
}
if ($menu['VICTORY_MENU_UPSCALER'] -in 'game','fsr-quality') { $Upscaler = $menu['VICTORY_MENU_UPSCALER'] }
$fpsWanted = $Fps
if ($menu.Contains('VICTORY_MENU_CONSOLE')) { $SeriesS = $menu['VICTORY_MENU_CONSOLE'] -eq 'series-s' }
if ($menu['VICTORY_MENU_RES'] -in '1440', '1080', '1080stockui', '1080to1440', '4k') { $Res = $menu['VICTORY_MENU_RES'] }
if ($menu['VICTORY_MENU_DLSS'] -in 'off', 'dlaa', 'quality', 'balanced', 'performance') { $Dlss = $menu['VICTORY_MENU_DLSS'] }
$timesWanted = $FrameGenX
if ($menu.Contains('VICTORY_MENU_FRAMEGEN')) {
    # 0: off; 2 to 6: that many frames shown for each one rendered (1, from an older file: 2).
    $FrameGen = $menu['VICTORY_MENU_FRAMEGEN'] -match '^[1-6]$'
    if ($menu['VICTORY_MENU_FRAMEGEN'] -match '^[2-6]$') { $timesWanted = [int]$menu['VICTORY_MENU_FRAMEGEN'] }
}
if ($menu['VICTORY_MENU_FG_TYPE'] -in 'auto', 'dlss', 'fsr') { $FgType = $menu['VICTORY_MENU_FG_TYPE'] }
if ($menu['VICTORY_MENU_FPS'] -match '^\d+$' -and [int]$menu['VICTORY_MENU_FPS'] -ge 1 -and [int]$menu['VICTORY_MENU_FPS'] -le 1000) { $fpsWanted = [int]$menu['VICTORY_MENU_FPS'] }
if ($Radeon1080p -or $Upscaler -eq 'fsr-quality') {
    $Dlss = 'off'
    # The current frame generation integration requires DLSS as its input.
    $FrameGen = $false
    $FrameGenReady = $false
}
$settings = @{
    # Submission and copy machinery
    VICTORY_THIN = '1'; VICTORY_JOIN_PARTS = '8'; VICTORY_RELAY = '2'; VICTORY_RELAY_COPY = '1'; VICTORY_WIDE_BACK = '1'
    VICTORY_CARD_REHASH_RUNS = '1'; VICTORY_JOIN_COPIES = '1'; VICTORY_CARD_STEPS = '2'; VICTORY_CARD_AWAKE = '1'
    VICTORY_CARD_BUFFERS_MIN_KB = '64'; VICTORY_INDIRECT_WINDOW = '1'; VICTORY_INDIRECT_BATCH = '2'
    VICTORY_UPLOAD_AHEAD = '1'; VICTORY_SHOW_AFTER = '1'; VICTORY_MAP_THREADS = '1'
    # Shader writes into big write-combined copies stay on the card (-5 ms a frame)
    VICTORY_WC_NO_BACK = '1'; VICTORY_WC_NO_BACK_MIN_KB = '1024'
    # The title's page-mapping copies kept without Windows views (no cutscene freezes at scene cuts)
    VICTORY_SOFT_VIEWS = 'copies'
    # Textures: pool with no blocks made ahead, 16 ms a pass, no catch-up, levels not on demand (sharp)
    VICTORY_TEXTURE_POOL = '1'; VICTORY_RIPE_BLOCKS = '0'; VICTORY_TEXTURE_NO_WAIT = '1'; VICTORY_TEXTURE_CATCHUP = '0'
    VICTORY_TEXTURE_STAGE_MS = '16'; VICTORY_LEVELS_ON_DEMAND = '0'
    VICTORY_PRELUDE_UPLOADS = '1'; VICTORY_SHOW_NEW_LATER = '1'; VICTORY_POOL_SMALL = '0'; VICTORY_TABLED_REREAD = '1'
    VICTORY_QUEUE_HOST_TEXTURES = '0'
    # Descriptor heaps' records
    VICTORY_HEAP_SYNC_FIRST = '1'; VICTORY_SYNC_IN_PLACE = '1'; VICTORY_SPANS_QUICK = '1'; VICTORY_SYNC_FINE = '1'
    VICTORY_HEAP_SYNC_AT_SUBMIT = '1'
    # Light without the title's ray tracing: an estimate of indirect light and of reflections
    VICTORY_AMBIENT = '0.8'; VICTORY_AMBIENT_AIR = '1.5'; VICTORY_AMBIENT_SPECULAR = '1'
    # Pixel programs keep the console's early depth test
    VICTORY_EARLY_DEPTH = '1'
    # Left out: the decal heap's defragmentation (5.6 ms of GPU a frame) and the heat-haze pass (smeared blocks)
    VICTORY_LEAVE_OUT = '02384a60fda6f2d0,731a39171b568298'
    # Pipelines the title makes are written down, so `victory warm` can make them ahead
    VICTORY_PIPELINE_BOOK = '1'
    # Memory stack (skornuta): unused texture slots given back, old allocators taken anew, small buffers packed
    VICTORY_SLOT_EVICT = '1'
    VICTORY_ALLOCATOR_TRIM = '1'
    VICTORY_BUDGET_PACK = '1'
}
if ($Radeon1080p) {
    # Each line also passes through PowerShell and is flushed to disk.
    # Keep frame timings and warnings, and avoid logging every shader and view.
    $settings.VICTORY_LOG = if ($env:VICTORY_LOG) { $env:VICTORY_LOG } else { 'warn,victory_gpu::host=info,victory_hle::modules::fidelityfx=info' }
    $settings.VICTORY_MENU_RADEON = '1'
    $settings.VICTORY_WATER_LIGHT = '0'
    $settings.VICTORY_DISPLAY_WATCH = '1'
    $settings.VICTORY_RECORD_ASSET_READS = '1'
    # RX 9070 XT: the driver faulted compiling this tree-animation velocity variant
    # during prologue streaming. Other skinning and velocity shaders stay enabled.
    $settings.VICTORY_LEAVE_OUT += ',f16876929fa64f9f'
}
if (-not $SeriesS) {
    if ($Upscaler -eq 'fsr-quality') { $settings.VICTORY_NATIVE_1080P = '1' }
    elseif ($Res -eq '1440') { $settings.VICTORY_NATIVE_1440P = '1' }
    elseif ($Res -eq '1080') { $settings.VICTORY_NATIVE_1080P = '1' }
    elseif ($Res -eq '1080stockui') {
        $settings.VICTORY_1080P_STOCK_UI = '1'
        $settings.VICTORY_WINDOW_MAX = '1920x1080'
    }
    elseif ($Res -eq '1080to1440') { $settings.VICTORY_RENDER_1080P = '1' }
}
if ($Dlss -ne 'off') {
    # NVIDIA's files: where VICTORY_STREAMLINE_DIR says, else where Get-NVIDIA-DLSS.cmd puts them.
    $sl = if ($env:VICTORY_STREAMLINE_DIR) { $env:VICTORY_STREAMLINE_DIR } else { Join-Path $repo 'nvidia-sdk\Streamline\bin\x64' }
    if (Test-Path (Join-Path $sl 'sl.interposer.dll')) {
        $settings.VICTORY_DLSS = '1'; $settings.VICTORY_DLSS_MODE = $Dlss; $settings.VICTORY_STREAMLINE_DIR = $sl
        if ($FrameGen) { $settings.VICTORY_FRAMEGEN = '1'; $settings.VICTORY_FRAMEGEN_X = [string]$timesWanted }
        elseif ($FrameGenReady -and -not $NoFrameGenReady) { $settings.VICTORY_FRAMEGEN = '1'; $settings.VICTORY_FRAMEGEN_X = '0' }   # ready, off
        # Frame generation takes 1.5 to 2 GB of the card's memory, which the layer otherwise fills with textures up
        # to nine tenths of its budget (on a 12 GB card it then ran out: no generated frames). On from the start, it
        # is left a fifth of the budget; ready but off, a little less is held back.
        if ($settings.VICTORY_FRAMEGEN -eq '1') { $settings.VICTORY_TEXTURE_ROOM_PERCENT = if ($FrameGen) { '80' } else { '86' } }
    } else {
        Write-Host "No NVIDIA files in ${sl}: running without DLSS and frame generation. Get-NVIDIA-DLSS.cmd in the"
        Write-Host "repository folder fetches them from NVIDIA once (about 70 MB); VICTORY_STREAMLINE_DIR names another folder."
    }
}
# AMD FSR 3 frame generation: made ready as DLSS Frame Generation is (on, or ready and off for the F1 menu).
# auto: the layer uses it only where DLSS Frame Generation does not run; fsr: DLSS Frame Generation is not asked.
if ($FgType -ne 'dlss' -and ($FrameGen -or ($FrameGenReady -and -not $NoFrameGenReady))) {
    $fx = if ($env:VICTORY_FIDELITYFX_DIR) { $env:VICTORY_FIDELITYFX_DIR } else { Join-Path $repo 'amd-sdk\FidelityFX\bin' }
    # Asked whatever is here: the layer checks the files and the card, and the start's notice (on the picture for
    # its first 30 s, and in this window) says what runs and why not. On Series X without DLSS it stays off: the
    # title's own upscaling, which it needs, takes the Series S sizes only.
    $settings.VICTORY_FSR_FRAMEGEN = '1'; $settings.VICTORY_FIDELITYFX_DIR = $fx
    if (-not (Test-Path (Join-Path $fx 'amd_fidelityfx_framegeneration_dx12.dll'))) {
        Write-Host "No AMD FSR 3 files in ${fx}: Get-AMD-FSR.cmd in the repository's folder fetches them from AMD (about"
        Write-Host '30 MB, once). Needed only on cards without DLSS frame generation, or with FSR 3 chosen in F1.'
    }
    if ($FgType -eq 'fsr') { $settings.Remove('VICTORY_FRAMEGEN') }
    if (-not $settings.VICTORY_FRAMEGEN_X) { $settings.VICTORY_FRAMEGEN_X = if ($FrameGen) { [string]$timesWanted } else { '0' } }
    if (-not $settings.VICTORY_TEXTURE_ROOM_PERCENT) { $settings.VICTORY_TEXTURE_ROOM_PERCENT = if ($FrameGen) { '80' } else { '86' } }
}
if ($fpsWanted -gt 0) { $settings.VICTORY_FRAME_INTERVAL_US = [string][int][math]::Round(1000000 / $fpsWanted) }
foreach ($k in $menu.Keys) {
    if ($k -like 'VICTORY_MENU_*') { continue }
    if (($Radeon1080p -or $Upscaler -eq 'fsr-quality') -and ($k -like 'VICTORY_DLSS*' -or $k -in 'VICTORY_FRAMEGEN','VICTORY_FSR_FRAMEGEN','VICTORY_MFG_UNLOCKER','VICTORY_MFG_UNLOCKER_DLL','VICTORY_STREAMLINE_PRESENT')) { continue }
    if ($menu[$k]) { $settings[$k] = $menu[$k] } else { $settings.Remove($k) }
}
foreach ($k in $More.Keys) { $settings[$k] = $More[$k] }
$console = if ($SeriesS) { 'series-s' } else { 'series-x' }
# For the menu: what it shows as the current choices, where it saves, and how it starts the game again
# (-More is not carried over a restart from the menu).
$settings.VICTORY_MENU_UPSCALER = $Upscaler
$settings.VICTORY_MENU_CONSOLE = $console
$settings.VICTORY_MENU_RES = $Res
$settings.VICTORY_MENU_DLSS = if ($settings.VICTORY_DLSS -eq '1') { $Dlss } else { 'off' }
$settings.VICTORY_MENU_FRAMEGEN = if ($settings.VICTORY_FRAMEGEN -eq '1' -or $settings.VICTORY_FSR_FRAMEGEN -eq '1') { [string]$settings.VICTORY_FRAMEGEN_X } else { '0' }
$settings.VICTORY_MENU_FG_TYPE = $FgType
$settings.VICTORY_MENU_FPS = [string]$fpsWanted
$settings.VICTORY_MENU_FILE = $menuFile
$settings.VICTORY_MENU_PLAY = $MyInvocation.MyCommand.Path
$settings.VICTORY_MENU_ARGS = "-Exe '$($Exe -replace "'", "''")' -Build $Build -Upscaler '$Upscaler'$(if ($FrameGenReady -and -not $NoFrameGenReady) { ' -FrameGenReady' })$(if ($NoFrameGenReady) { ' -NoFrameGenReady' })$(if ($Radeon1080p) { ' -Radeon1080p' })"

$victory = Join-Path $repo "target\$Build\victory.exe"
# A release built during play can be staged beside the running executable.
# Install it at the usual path so the next launch can use the warmed driver cache.
$staged = "$victory.pending"
if (Test-Path -LiteralPath $staged) {
    try {
        if (-not [IO.File]::Exists($victory)) {
            [IO.File]::Move($staged, $victory)
        } elseif ([IO.File]::GetLastWriteTimeUtc($staged) -ge [IO.File]::GetLastWriteTimeUtc($victory)) {
            [IO.File]::Replace($staged, $victory, [System.Management.Automation.Language.NullString]::Value)
        }
        if (-not [IO.File]::Exists($staged) -and [IO.File]::Exists("$staged.pdb")) {
            Move-Item -LiteralPath "$staged.pdb" -Destination ([IO.Path]::ChangeExtension($victory, 'pdb')) -Force -ErrorAction Stop
        }
    } catch {
        if (Test-Path -LiteralPath $staged) { Write-Warning "Player update is pending. $($_.Exception.GetBaseException().Message)" }
        elseif (Test-Path -LiteralPath "$staged.pdb") { Write-Warning "Could not install player debug symbols. $($_.Exception.GetBaseException().Message)" }
    }
}
if (-not (Test-Path $victory)) {
    # Not built yet: built here, once.
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { Write-Host "No $victory, and no cargo to build it with: install Rust (README), then start this again."; exit 1 }
    Write-Host 'Building the layer (a few minutes the first time) ...'
    Push-Location $repo
    try {
        if ($Build -eq 'diag') { cargo build --locked --profile diag -p victory --features diagnostics }
        else { cargo build --release --locked -p victory }
    } finally { Pop-Location }
    if (-not (Test-Path $victory)) { Write-Host "The build did not make $victory."; exit 1 }
}
if (-not (Test-Path -LiteralPath $Exe)) { Write-Host "No $Exe."; exit 1 }
# Switches left over in this window are cleared; path settings stay (victory.toml gives them otherwise).
$paths = 'VICTORY_CONFIG', 'VICTORY_GAME_DIR', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR', 'VICTORY_STREAMLINE_DIR', 'VICTORY_FIDELITYFX_DIR'
Get-ChildItem Env:VICTORY_* -ErrorAction SilentlyContinue | Where-Object { $_.Name -notin $paths } | ForEach-Object { Remove-Item "Env:$($_.Name)" }
foreach ($k in $settings.Keys) { Set-Item "Env:$k" $settings[$k] }

$logs = Join-Path $env:LOCALAPPDATA 'victory\logs'; New-Item -ItemType Directory -Force $logs | Out-Null
$log = Join-Path $logs ('victory-nvidia-{0:yyyyMMdd-HHmmss}.log' -f (Get-Date))
$writer = [IO.StreamWriter]::new($log, $false, [Text.UTF8Encoding]::new($false)); $writer.AutoFlush = $true
$writer.WriteLine("# console=$console build=$Build preload=$preload switches: " + (($settings.GetEnumerator() | Sort-Object Name | ForEach-Object { "$($_.Name)=$($_.Value)" }) -join ' '))
Write-Host "Log: $log"
Push-Location $repo   # victory.toml is read from here
try {
    if ($preload) {
        # With this start's switches (the translations depend on them); its lines go to the log too.
        Write-Host 'Preparing the shader and pipeline caches. Progress follows.'
        $began = Get-Date
        # (Split-Path -LiteralPath with -Parent: no such parameter set in Windows PowerShell 5.1.)
        $gameFolder = [IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($Exe))
        $preloadArgs = @($gameFolder)
        if ($Radeon1080p) {
            # This retail build stores its shader pack and embedded programs here.
            # Scanning audio, movies and level archives adds tens of GB of reads.
            $preloadArgs = @($Exe)
            if (!$FullPreload) { $preloadArgs = @('--used', '--extra', '4096') + $preloadArgs }
            $preloadArgs += @(Get-ChildItem -LiteralPath $gameFolder -File | Where-Object {
                $_.Extension -eq '.dll' -or $_.Name -eq 'shaders.rpf'
            } | ForEach-Object { $_.FullName })
        }
        if ($PrepareOnly) { $preloadArgs = @('--asset-dir', $gameFolder) + @($preloadArgs) }
        & $victory preload @preloadArgs 2>&1 | ForEach-Object { $line = "$_"; $writer.WriteLine("# preload: $line"); $line }
        $preloadExit = $LASTEXITCODE
        $how = if ($preloadExit -eq 0) { 'done' } else { "ended with code $preloadExit" }
        $said = 'Preload {0} after {1:N1} min.' -f $how, ((Get-Date) - $began).TotalMinutes
        if (!$PrepareOnly) { $said += ' Starting the game.' }
        $writer.WriteLine("# $said"); Write-Host $said
        if ($PrepareOnly) { $writer.Dispose(); exit $preloadExit }
    }
}
finally { Pop-Location }
Push-Location $repo
try {
    $stockCanvas = $settings.VICTORY_1080P_STOCK_UI -eq '1'
    if ($Radeon1080p) {
        $customConfig = $env:VICTORY_CONFIG -or (Test-Path -LiteralPath 'victory.toml')
        $dataDir = $env:VICTORY_DATA_DIR
        if (!$dataDir -and $customConfig) {
            # Let Victory resolve [paths].data_dir and its normal defaults.
            $config = @(& $victory config)
            if ($LASTEXITCODE -eq 0) {
                foreach ($line in $config) { if ($line -match '^data_dir\s*:\s*(.+)$') { $dataDir = $Matches[1]; break } }
            }
        } elseif (!$dataDir) { $dataDir = Join-Path $env:LOCALAPPDATA 'victory\data' }
        $videoFile = if ($dataDir) { Join-Path $dataDir '7952987A\storage\settings\settings.xml' }
        if ($videoFile -and (Test-Path -LiteralPath $videoFile)) {
            if (($stockCanvas -or $settings.VICTORY_NATIVE_1080P -eq '1' -or $Upscaler -eq 'fsr-quality') -and !$customConfig) {
                try {
                    & (Join-Path $PSScriptRoot 'set-radeon-ui-resolution.ps1') -SettingsFile $videoFile -StockCanvas:$stockCanvas
                } catch { Write-Warning "Could not update the UI canvas. $($_.Exception.Message)" }
            }
            try {
                $fields = & (Join-Path $PSScriptRoot 'game-graphics.ps1') -SettingsFile $videoFile -Menu $menu
                if ($fields) {
                    $env:VICTORY_MENU_GAME_FIELDS = $fields
                    $env:VICTORY_MENU_SAMPLE_FILE = Join-Path $dataDir '7952987A\victory-graphics-samples.tsv'
                }
            } catch {
                $message = "Could not apply game graphics. $($_.Exception.GetBaseException().Message)"
                $writer.WriteLine("# $message")
                Write-Warning $message
            }
        }
    }
    & $victory run $Exe --console $console --execute 2>&1 | ForEach-Object { $line = "$_"; $writer.WriteLine($line); $line }
    $runExit = $LASTEXITCODE
    $ended = 'Player exited with code {0} (0x{0:X8}).' -f $runExit
    $writer.WriteLine("# $ended")
    if ($runExit -ne 0) { Write-Host "$ended Log: $log" }
}
finally { Pop-Location; $writer.Dispose() }
exit $runExit
