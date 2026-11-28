<#
.SYNOPSIS
Runs the title unattended with the diagnostics build (through `victory play`,
the launcher's own settings), drives the menus into the world and summarises
frame times, so runs on different machines or with different switches can be
compared.

.DESCRIPTION
Each run gets a folder of its own (default: <cache_dir's parent>\bench\<time>-<label>)
holding the log, frames captured every -CaptureEvery seconds, the load
sampled every -SampleEvery seconds (samples.csv), the run's copy of the
title's data, summary.md and summary.json. The summary gives medians and
means of the frame lines, the p99 frame time (99th percentile of every
frame's time) and fps by draws per frame (median, interquartile range and
frame lines of each band). With a switch set to alt* the summary mixes the
two regimes: compare them band by band from the log instead.

The frame rates are of frames the title renders (presents): the frame line's
fps and the frame log's lengths count the title's own presents, so frames
made by DLSS frame generation are not in them (the summary says how many it
showed, and -FrameGen off, the default, keeps it off). The frame log
(`victory_gpu::framelog: frame times`) is a line every 5 s with every frame's
length in tenths of a millisecond, written by diagnostics builds only.

The title is started as `victory play <exe> --dlss .. --set ..`: the
launcher's settings (ray tracing, the pipeline book, ...) with the options
below on top. `play` clears every other VICTORY_* variable, so the run's own
(keys file, capture folder) go through --set. The F1 menu's saved choices
(%LOCALAPPDATA%\victory\victory-settings.txt) would override --dlss, the
frame generation options and the resolution: the script stops when they
differ from what it was asked for (-AllowMenuFile runs anyway); other saved
switches are listed in the summary's notes. What `play` finally used (its log
file's header) is in summary.json as `effective`.

Every run starts from the same saves: the title's data folder (the configured
data_dir, or -Seed) is copied for the run, so the run's autosaves never reach
it and two runs load the same checkpoint. The shader and pipeline caches are
shared: the first run of a build translates and compiles, later runs measure.

Until the world starts (a 5-second frame line with 500 draws or more) the
script looks at a captured frame every two seconds and presses what that
screen needs: Down to "Next" on the legal notices, A on each unticked box of
the terms and then A on "Confirm", and A (Enter) every few seconds on any
other screen, which passes the start and first-boot screens and continues
the save. With -FirstBoot the run starts with no saves at all (a fresh
profile), and its data folder then holds a save to start later runs from
(-Seed <run folder>\data).

Needs the diagnostics build (scripted keys, frame capture):
    cargo build --profile diag -p victory --features diagnostics
or pass -Build.

.EXAMPLE
.\scripts\bench.ps1 -Label baseline
.\scripts\bench.ps1 -Label no-opt -Switches @{ VICTORY_SHADER_OPT = '0' }
.\scripts\bench.ps1 -Compare <run folder>,<run folder>

.EXAMPLE
# The same walk in every run, from the world's first frame: 5 s ahead, turn
# right for 2 s, 6 s ahead (VICTORY_KEYS_FROM=world).
.\scripts\bench.ps1 -Label walk -Route '5-10:W,10-12:L,12-18:W'

.EXAMPLE
# No saves yet: make one, then measure from it.
.\scripts\bench.ps1 -FirstBoot -Label seed -Seconds 60
.\scripts\bench.ps1 -Seed <the seed run's folder>\data -Label baseline

.EXAMPLE
# The 0.5 baseline: ray tracing on and off, DLSS off and Quality (3 runs each).
.\scripts\bench.ps1 -Label rt1-dlssoff -RayTracing on -Dlss off -Seconds 360 -CaptureEvery 0
.\scripts\bench.ps1 -Label rt0-dlssq -RayTracing off -Dlss quality -Seconds 360 -CaptureEvery 0

.EXAMPLE
# Summarise an earlier run again, over another window.
.\scripts\bench.ps1 -From <run folder> -Warmup 60 -Seconds 120
#>
[CmdletBinding()]
param(
    # A name for the run folder and the summary.
    [string]$Label = 'run',
    # Switches for this run: @{ VICTORY_RELAY = '0' }, or 'VICTORY_RELAY=0'
    # (several as a list), which also passes through `powershell -File`.
    # A comma after a value, with no '=' in what follows, is part of the value ('A=1,2,3');
    # ';' always separates.
    # They go to `victory play --set`, after the options below (they win).
    [object]$Switches = @{},
    # The title's own ray tracing (VICTORY_BVH_BUILDER=1/0); default: as
    # `play` has it (on).
    [ValidateSet('default', 'on', 'off')][string]$RayTracing = 'default',
    # DLSS in place of the title's upscaling (`play --dlss`); off or quality
    # for the baseline. Needs NVIDIA's files (`victory get-dlss`) unless off.
    [ValidateSet('off', 'dlaa', 'quality', 'balanced', 'performance')][string]$Dlss = 'quality',
    # DLSS frame generation: off (not even made ready: `--no-framegen-ready`),
    # ready (made ready, off: `play`'s own start), or 2 to 6 (frames shown for
    # each one rendered; the fps reported stay the rendered ones).
    [ValidateSet('off', 'ready', '2', '3', '4', '5', '6')][string]$FrameGen = 'off',
    # VICTORY_PIPELINE_BOOK: write down the pipelines the title makes (for
    # `victory warm`).
    [ValidateSet('on', 'off')][string]$PipelineBook = 'on',
    # Run although the F1 menu's saved choices differ from these options.
    [switch]$AllowMenuFile,
    # Seconds measured, after -Warmup, from the world's first frame.
    [int]$Seconds = 180,
    # Seconds of world frames left out first (pipelines still being made).
    [int]$Warmup = 30,
    # Give up if the world has not started after this many seconds.
    [int]$BootTimeout = 900,
    # Keep pressing A in the world (skips cutscenes; runs then differ more).
    [switch]$SkipCutscenes,
    # A route walked in the world: a file of lines `<seconds>:<key>` or
    # `<from>-<to>:<key>` (`#` starts a comment), or such entries separated by
    # commas. It goes to VICTORY_KEYS with VICTORY_KEYS_FROM=world, so the
    # seconds count from the world's first frame (builds without that switch
    # count them from when input starts). Keys: W/A/S/D move, I/J/K/L turn the
    # camera, Enter is A. Its text's hash is in the summary.
    [string]$Route,
    # Seconds between captured frames (0: none).
    [int]$CaptureEvery = 20,
    # Seconds between samples of the load (processor, memory, card) in
    # samples.csv (0: none). The card's columns need nvidia-smi.
    [int]$SampleEvery = 2,
    # The data folder runs start from (default: the configured data_dir).
    [string]$Seed,
    # The run folder (default: <cache_dir's parent>\bench\<time>-<label>).
    [string]$Out,
    # The title's executable (default: the one .exe in game_dir).
    [string]$Exe,
    # Build the diagnostics build first.
    [switch]$Build,
    # Walk the first-boot screens of a profile with no saves.
    [switch]$FirstBoot,
    # Summarise this earlier run folder again instead of running.
    [string]$From,
    # Print a table of earlier runs' summaries instead of running.
    [string[]]$Compare
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$victory = Join-Path $root 'target\diag\victory.exe'
$invariant = [Globalization.CultureInfo]::InvariantCulture
if ($Switches -isnot [hashtable]) {
    $table = @{}
    $last = $null
    foreach ($pair in @($Switches) | ForEach-Object { "$_" -split '[,;]' } | Where-Object { $_ }) {
        $name, $value = $pair -split '=', 2
        if ($null -eq $value) {
            # No '=' in it: a comma inside the previous value ('A=1,2,3'), put back.
            if ($null -eq $last) { throw "a switch is NAME=VALUE, not '$pair'" }
            $table[$last] += ',' + $pair.Trim()
            continue
        }
        $last = $name.Trim()
        $table[$last] = $value.Trim()
    }
    $Switches = $table
}

# The summary's columns: the frame line's fields, per frame.
$metrics = @(
    @{ key = 'fps'; head = 'fps' },
    @{ key = 'ms'; head = 'ms/frame' },
    @{ key = 'prep'; head = 'preparing' },
    @{ key = 'gpu'; head = 'GPU' },
    @{ key = 'back'; head = 'copying back' },
    @{ key = 'draws'; head = 'draws' },
    @{ key = 'dropped'; head = 'dropped' },
    @{ key = 'making'; head = 'being made' }
)
$fields = 'fps', 'ms', 'prep', 'copyms', 'texms', 'gpu', 'back', 'draws', 'dropped', 'making'
# The 5-second frame line (victory_gpu::host, Meter::frame; its numbers are those of the last
# second's frames, and its text goes on after "being made)").
$lineRe = [regex]('^(?<ts>\S+Z)\s+INFO victory_gpu::host: (?<fps>[\d.]+) fps, (?<ms>[\d.]+) ms/frame ' +
    '\((?<prep>[\d.]+) preparing: copies (?<copyms>[\d.]+) ms [\d.]+ MB, textures (?<texms>[\d.]+) ms [\d.]+ MB; ' +
    '(?<gpu>[\d.]+) on the GPU; (?<back>[\d.]+) copying back [\d.]+ MB\), (?<draws>\d+) draws/frame, ' +
    'dropped (?<dropped>\d+) without a host pipeline \((?<making>\d+) being made\)')
$worldDraws = 500
# The frame log's line of every frame's length (diagnostics builds).
# (host.rs `FrameTimes::line`: every 5 s, lengths in tenths of a millisecond.)
$timesRe = [regex]'^(?<ts>\S+Z)\s+INFO victory_gpu::framelog: frame times: .*?; each \(0\.1 ms\): (?<each>[\d ]+)'
# DLSS and frame generation, once a second (streamline.rs `line`): to check the run had what was asked.
$dlssRe = [regex]'^(?<ts>\S+Z)\s+INFO victory_gpu::streamline: DLSS per frame: (?<up>[\d.]+) upscalings by DLSS'
$fgRe = [regex]('^(?<ts>\S+Z)\s+INFO victory_gpu::streamline: DLSS Frame Generation: (?<on>\d+) presents with it on, ' +
    '(?<off>\d+) off, (?<shown>\d+) frames shown by it')
# Frame lines are grouped by draws: scenes of different weight differ more
# than switches do, so runs are compared band by band.
$bands = @(@(500, 2000), @(2000, 3000), @(3000, 4500), @(4500, 7000), @(7000, [int]::MaxValue))
# The load sampled while the title runs (samples.csv, every -SampleEvery s).
$sampleHead = 'at,cpu_pct,private_gb,commit_gb,gpu_pct,vram_mb,power_w'

function Format-Number($value) {
    if ($null -eq $value) { return '-' }
    return ([double]$value).ToString('0.#', $invariant)
}

function Show-Compare([string[]]$dirs) {
    $runs = foreach ($d in $dirs) {
        $path = Join-Path $d 'summary.json'
        if (-not (Test-Path -LiteralPath $path)) { throw "no summary.json in $d" }
        Get-Content -Raw -LiteralPath $path | ConvertFrom-Json
    }
    $heads = ($metrics | ForEach-Object { $_.head }) -join ' | '
    ('| run | setup | frame lines | ' + $heads + ' | p99 frame time ms | 1 % low fps | 0.1 % low fps | GPU busy % | VRAM MB (peak) | not translated | refused | crashes |')
    ('|' + ('---|' * ($metrics.Count + 11)))
    foreach ($r in $runs) {
        $cells = ($metrics | ForEach-Object { Format-Number $r.median.($_.key) }) -join ' | '
        $setup = if ($r.config) { 'RT ' + $r.config.ray_tracing + ', DLSS ' + $r.config.dlss + ', FG ' + $r.config.frame_generation } else { '-' }
        ('| ' + $r.label + ' | ' + $setup + ' | ' + $r.lines + ' | ' + $cells + ' | ' + (Format-Number $r.p99_ms) + ' | ' +
            (Format-Number $r.low1_fps) + ' | ' + (Format-Number $r.low01_fps) + ' | ' + (Format-Number $r.load.gpu_pct) + ' | ' +
            (Format-Number $r.load.vram_mb) + ' (' + (Format-Number $r.load.vram_peak_mb) + ') | ' +
            $r.untranslated + ' | ' + $r.refused + ' | ' + $r.crashes + ' |')
    }
    ''
    'Medians of the 5-second frame lines in each run''s window; the ms columns are per frame; fps are frames the title renders.'
    'p99 frame time: the 99th percentile of every frame''s time in the window (frame log); 1 % low fps = 1000 / p99 ms.'
    'GPU busy % and VRAM: window means of nvidia-smi samples (a busy share says nothing about the limit by itself).'
    ''
    # The same runs band by band: fps of the median frame line in each.
    $bandHeads = ($bands | ForEach-Object { Format-Band $_ }) -join ' | '
    ('| run | ' + $bandHeads + ' |')
    ('|' + ('---|' * ($bands.Count + 1)))
    foreach ($r in $runs) {
        $cells = foreach ($b in $bands) {
            $hit = @($r.bands | Where-Object { $_.from -eq $b[0] })
            if ($hit.Count -and $hit[0].lines) {
                '{0} ({1}; IQR {2}-{3} ms)' -f (Format-Number $hit[0].fps), $hit[0].lines,
                    (Format-Number $hit[0].ms_p25), (Format-Number $hit[0].ms_p75)
            } else { '-' }
        }
        ('| ' + $r.label + ' | ' + ($cells -join ' | ') + ' |')
    }
    ''
    'fps of the median frame line by draws per frame (frame lines in the band; interquartile range of ms/frame).'
}

# A log line for the notes, with absolute paths (user and machine names) left out.
function Hide-Paths([string]$text) {
    return ($text -replace '[A-Za-z]:[\\/](?:[^\\/:\r\n"''<>|*?]*[\\/])*[^\\/:\s"''<>|*?,;()]*', '<path>')
}

function Format-Band($band) {
    if ($band[1] -eq [int]::MaxValue) { return ('{0}+ draws' -f $band[0]) }
    return ('{0}-{1} draws' -f $band[0], $band[1])
}

# The value below which a share `p` (0 to 1) of the values lie.
function Get-Percentile([double[]]$values, [double]$p) {
    if ($values.Count -eq 0) { return $null }
    $sorted = [double[]]($values | Sort-Object)
    return $sorted[[int][math]::Min($sorted.Count - 1, [math]::Ceiling($p * $sorted.Count) - 1)]
}

function Get-Median([double[]]$values) {
    if ($values.Count -eq 0) { return $null }
    $sorted = [double[]]($values | Sort-Object)
    $mid = [int][math]::Floor($sorted.Count / 2)
    if ($sorted.Count % 2) { return $sorted[$mid] }
    return ($sorted[$mid - 1] + $sorted[$mid]) / 2
}

# Reads a run folder's log and writes summary.json and summary.md.
function Write-Summary([string]$dir, [hashtable]$facts) {
    $log = Join-Path $dir 'victory.log'
    $rows = New-Object System.Collections.Generic.List[object]
    $untranslated = @{}
    $refused = @{}
    $notes = New-Object System.Collections.Generic.List[string]
    $times = New-Object System.Collections.Generic.List[object]
    $dlssLines = New-Object System.Collections.Generic.List[object]
    $fgLines = New-Object System.Collections.Generic.List[object]
    $first = $null
    foreach ($raw in [System.IO.File]::ReadLines($log)) {
        # A log written with colours (no NO_COLOR) reads the same.
        $line = $raw -replace "$([char]27)\[[0-9;]*m", ''
        if (-not $first -and $line -match '^(\S+Z)\s') { $first = [datetime]::Parse($Matches[1], $invariant).ToUniversalTime() }
        $t = $timesRe.Match($line)
        if ($t.Success) {
            $times.Add([pscustomobject]@{
                at   = [datetime]::Parse($t.Groups['ts'].Value, $invariant).ToUniversalTime()
                # Tenths of a millisecond in the line.
                each = [double[]]@($t.Groups['each'].Value.Trim() -split '\s+' | ForEach-Object { [double]::Parse($_, $invariant) / 10 })
            })
            continue
        }
        $d = $dlssRe.Match($line)
        if ($d.Success) {
            $dlssLines.Add([pscustomobject]@{
                at = [datetime]::Parse($d.Groups['ts'].Value, $invariant).ToUniversalTime()
                up = [double]::Parse($d.Groups['up'].Value, $invariant)
            })
            continue
        }
        $g = $fgRe.Match($line)
        if ($g.Success) {
            $fgLines.Add([pscustomobject]@{
                at    = [datetime]::Parse($g.Groups['ts'].Value, $invariant).ToUniversalTime()
                on    = [double]$g.Groups['on'].Value
                off   = [double]$g.Groups['off'].Value
                shown = [double]$g.Groups['shown'].Value
            })
            continue
        }
        $m = $lineRe.Match($line)
        if ($m.Success) {
            $row = [ordered]@{ at = [datetime]::Parse($m.Groups['ts'].Value, $invariant).ToUniversalTime() }
            foreach ($k in $fields) { $row[$k] = [double]::Parse($m.Groups[$k].Value, $invariant) }
            $rows.Add([pscustomobject]$row)
        } elseif ($line -match 'shader does not translate.*?([0-9a-f]{16})') {
            $untranslated[$Matches[1]] = 1
        } elseif ($line -match 'host (mesh )?pipeline refused.*shaders=(\[.*?\])') {
            $refused[$Matches[2]] = 1
        } elseif ($line -match 'device removed|DEVICE_REMOVED|DEVICE_HUNG|unhandled exception|slow driver compile') {
            if ($notes.Count -lt 40) { $notes.Add((Hide-Paths ($line -replace '^\S+\s+', ''))) }
        }
    }
    $world = $rows | Where-Object { $_.draws -ge $worldDraws } | Select-Object -First 1
    $window = @()
    $dlssWin = @()
    $fgWin = @()
    $frameTimes = [double[]]@()
    $load = [ordered]@{}
    if ($world) {
        $from = $world.at.AddSeconds($Warmup)
        $to = $from.AddSeconds($Seconds)
        $window = @($rows | Where-Object { $_.at -ge $from -and $_.at -le $to })
        # Every frame of the window, for the p99 frame time (the 99th percentile
        # of frame times; the 5-second lines average the long frames away).
        $frameTimes = [double[]]@($times | Where-Object { $_.at -ge $from -and $_.at -le $to } | ForEach-Object { $_.each })
        $dlssWin = @($dlssLines | Where-Object { $_.at -ge $from -and $_.at -le $to })
        $fgWin = @($fgLines | Where-Object { $_.at -ge $from -and $_.at -le $to })
        $samplesPath = Join-Path $dir 'samples.csv'
        if (Test-Path -LiteralPath $samplesPath) {
            $samples = @(Import-Csv -LiteralPath $samplesPath | Where-Object {
                $at = [datetime]::Parse($_.at, $invariant).ToUniversalTime(); $at -ge $from -and $at -le $to
            })
            foreach ($k in ($sampleHead -split ',' | Select-Object -Skip 1)) {
                # Cells that are not numbers (empty, or an older run's [N/A]) are left out.
                $values = [double[]]@($samples | ForEach-Object {
                    $v = 0.0
                    if ([double]::TryParse("$($_.$k)", [Globalization.NumberStyles]::Float, $invariant, [ref]$v)) { $v }
                })
                if ($k -eq 'vram_mb') {
                    # A counter that underflows reads as a number of terabytes (run 20261128-112011:
                    # 17592181866181 MB): over 1 TB (1048576 MB) is no card's memory, so the sample
                    # is left out of the mean and the peak, and the summary says how many.
                    $kept = [double[]]@($values | Where-Object { $_ -le 1048576 })
                    if ($kept.Count -lt $values.Count) {
                        $notes.Add(('video memory: ' + ($values.Count - $kept.Count) + ' of ' + $values.Count +
                            ' samples over 1 TB left out of the mean and the peak (a counter that underflowed)'))
                    }
                    $values = $kept
                }
                $load[$k] = if ($values.Count) { [math]::Round(($values | Measure-Object -Average).Average, 1) } else { $null }
                if ($k -eq 'vram_mb') {
                    $load['vram_peak_mb'] = if ($values.Count) { [math]::Round(($values | Measure-Object -Maximum).Maximum, 0) } else { $null }
                }
            }
        }
        # A frame line lands every 5 to 6 s (the log is due after 5 s and looked at once a second): fewer, or
        # none near the window's end, means the run stopped early.
        $lastAt = if ($window.Count) { $window[-1].at } else { $from }
        if ($window.Count -lt $Seconds / 7 -or $lastAt -lt $to.AddSeconds(-14)) {
            $notes.Insert(0, ('incomplete run: ' + $window.Count + ' frame lines in the window, about ' +
                (Format-Number ($Seconds / 5.5)) + ' expected'))
        }
    } else {
        $notes.Add('the world never started (no frame line with 500 draws or more)')
    }
    foreach ($n in @($facts.notes)) { if ($n) { $notes.Add($n) } }
    $alt = @($facts.switches.Keys | Where-Object { "$($facts.switches[$_])" -match '^alt' })
    if ($alt.Count) {
        $notes.Insert(0, ('switch ' + ($alt -join ', ') + ' alternates: this summary mixes the two regimes; ' +
            'compare them band by band from the log'))
    }
    $median = [ordered]@{}
    $mean = [ordered]@{}
    foreach ($k in $fields) {
        $values = [double[]]@($window | ForEach-Object { $_.$k })
        $median[$k] = Get-Median $values
        $mean[$k] = if ($values.Count) { ($values | Measure-Object -Average).Average } else { $null }
    }
    $p99 = Get-Percentile $frameTimes 0.99
    $p999 = Get-Percentile $frameTimes 0.999
    # What DLSS and frame generation did in the window: upscalings per frame (about 1 with DLSS on) and
    # the title's presents with frame generation on or off, and the frames it showed.
    $dlssUp = Get-Median ([double[]]@($dlssWin | ForEach-Object { $_.up }))
    $fgOn = ($fgWin | Measure-Object -Property on -Sum).Sum
    $fgOff = ($fgWin | Measure-Object -Property off -Sum).Sum
    $fgShown = ($fgWin | Measure-Object -Property shown -Sum).Sum
    $asked = $facts.config
    if ($asked -and $window.Count) {
        if ($asked.dlss -eq 'off' -and $dlssUp -gt 0.5) { $notes.Add('DLSS was asked off but the log shows it upscaling') }
        if ($asked.dlss -ne 'off' -and -not ($dlssUp -gt 0.5)) { $notes.Add('DLSS was asked on but the log shows (almost) no upscalings by it') }
        if ($asked.frame_generation -match '^[2-6]$' -and -not ($fgOn -gt 0)) { $notes.Add('frame generation was asked on but never presented with it on') }
        if ($asked.frame_generation -notmatch '^[2-6]$' -and $fgOn -gt 0) { $notes.Add('frame generation was asked off but presented with it on: the fps are rendered frames, the frames shown are more') }
    }
    $bandRows = foreach ($b in $bands) {
        $in = @($window | Where-Object { $_.draws -ge $b[0] -and $_.draws -lt $b[1] })
        $bandMs = [double[]]@($in | ForEach-Object { $_.ms })
        $ms = Get-Median $bandMs
        [ordered]@{
            from = $b[0]; to = $b[1]; lines = $in.Count; fps = if ($ms) { [math]::Round(1000 / $ms, 1) } else { $null }
            ms = $ms; ms_p25 = Get-Percentile $bandMs 0.25; ms_p75 = Get-Percentile $bandMs 0.75
        }
    }
    $crashes = Join-Path $dir 'data\crashes.log'
    $summary = [ordered]@{
        label           = $facts.label
        switches        = $facts.switches
        commit          = $facts.commit
        config          = $facts.config
        effective       = $facts.effective
        gpu             = $facts.gpu
        driver          = $facts.driver
        exit            = $facts.exit
        peak_private_gb = $facts.peak_private_gb
        world_after_s   = if ($world -and $first) { [math]::Round(($world.at - $first).TotalSeconds) } else { $null }
        window          = "$Warmup-$($Warmup + $Seconds) s after the world's first frame"
        lines           = $window.Count
        median          = $median
        mean            = $mean
        frames          = $frameTimes.Count
        p99_ms          = $p99
        low1_fps        = if ($p99) { [math]::Round(1000 / $p99, 1) } else { $null }
        p999_ms         = $p999
        low01_fps       = if ($p999) { [math]::Round(1000 / $p999, 1) } else { $null }
        dlss_upscalings_per_frame = $dlssUp
        framegen        = [ordered]@{ presents_on = $fgOn; presents_off = $fgOff; frames_shown = $fgShown }
        bands           = @($bandRows)
        load            = $load
        untranslated    = $untranslated.Count
        refused         = $refused.Count
        crashes         = if (Test-Path -LiteralPath $crashes) { @(Get-Content -LiteralPath $crashes).Count } else { 0 }
        notes           = $notes
    }
    $summary | ConvertTo-Json -Depth 5 | Set-Content -Encoding UTF8 -LiteralPath (Join-Path $dir 'summary.json')

    $switchText = if ($facts.switches.Count) {
        ($facts.switches.Keys | Sort-Object | ForEach-Object { '`' + $_ + '=' + $facts.switches[$_] + '`' }) -join ' '
    } else { 'none' }
    $heads = ($metrics | ForEach-Object { $_.head }) -join ' | '
    $md = New-Object System.Collections.Generic.List[string]
    $md.Add('## ' + $facts.label)
    $md.Add('')
    $md.Add('- commit ' + $facts.commit + ', GPU ' + $facts.gpu + ', driver ' + $facts.driver)
    $md.Add('- switches: ' + $switchText)
    if ($facts.config) {
        $c = $facts.config
        $md.Add('- setup: ray tracing ' + $c.ray_tracing + ', DLSS ' + $c.dlss + ', frame generation ' + $c.frame_generation +
            ', pipeline book ' + $c.pipeline_book + '; fps and frame times are frames the title renders (not generated ones)')
    }
    $md.Add('- world after ' + $summary.world_after_s + ' s; window ' + $summary.window + ': ' + $window.Count + ' frame lines')
    $md.Add('- shaders not translated: ' + $untranslated.Count + '; host pipelines refused: ' + $refused.Count +
        '; crashes.log lines: ' + $summary.crashes + '; exit: ' + $facts.exit + '; peak private memory: ' + $facts.peak_private_gb + ' GB')
    $md.Add('')
    $md.Add('| | ' + $heads + ' |')
    $md.Add('|---|' + ('---|' * $metrics.Count))
    $md.Add('| median | ' + (($metrics | ForEach-Object { Format-Number $median[$_.key] }) -join ' | ') + ' |')
    $md.Add('| mean | ' + (($metrics | ForEach-Object { Format-Number $mean[$_.key] }) -join ' | ') + ' |')
    $md.Add('')
    if ($frameTimes.Count) {
        $md.Add('- p99 frame time: ' + (Format-Number $p99) + ' ms (1 % low ' + (Format-Number $summary.low1_fps) +
            ' fps; 99th percentile of ' + $frameTimes.Count + ' frame times); p99.9 ' + (Format-Number $p999) +
            ' ms (0.1 % low ' + (Format-Number $summary.low01_fps) + ' fps)')
        if ($fgWin.Count) {
            $md.Add('- DLSS upscalings per frame (median) ' + (Format-Number $dlssUp) + '; frame generation: ' +
                (Format-Number $fgOn) + ' presents on, ' + (Format-Number $fgOff) + ' off, ' + (Format-Number $fgShown) + ' frames shown by it')
        }
    } else {
        $md.Add('- p99 frame time: - (no frame times in the log; the frame log is on by default in diagnostics builds)')
    }
    $md.Add('- by draws per frame (median frame line; IQR of ms/frame; frame lines): ' +
        (($bandRows | Where-Object { $_.lines } | ForEach-Object {
            (Format-Band @($_.from, $_.to)) + ' ' + (Format-Number $_.fps) + ' fps (' + (Format-Number $_.ms) +
                ' ms, IQR ' + (Format-Number $_.ms_p25) + '-' + (Format-Number $_.ms_p75) + ' ms; ' + $_.lines + ')' }) -join ', '))
    if ($load.Count) {
        $md.Add('- load, window mean: processor ' + (Format-Number $load.cpu_pct) + ' % of all cores, private ' +
            (Format-Number $load.private_gb) + ' GB, commit ' + (Format-Number $load.commit_gb) + ' GB, card ' +
            (Format-Number $load.gpu_pct) + ' % (busy share, not proof of a GPU limit), video memory ' + (Format-Number $load.vram_mb) +
            ' MB (peak ' + (Format-Number $load.vram_peak_mb) + '), ' + (Format-Number $load.power_w) + ' W')
    }
    if ($notes.Count) {
        $md.Add('')
        $md.Add('Notes:')
        $md.Add('')
        foreach ($n in $notes) { $md.Add('- ' + $n) }
    }
    $md | Set-Content -Encoding UTF8 -LiteralPath (Join-Path $dir 'summary.md')
    $md | ForEach-Object { Write-Host $_ }
    return [bool]$world
}

if ($Compare) {
    Show-Compare $Compare
    return
}

if ($From) {
    $old = Get-Content -Raw -LiteralPath (Join-Path $From 'summary.json') | ConvertFrom-Json
    $switchTable = @{}
    if ($old.switches) { $old.switches.PSObject.Properties | ForEach-Object { $switchTable[$_.Name] = $_.Value } }
    $facts = @{
        label = $old.label; switches = $switchTable; commit = $old.commit; gpu = $old.gpu; driver = $old.driver
        exit = $old.exit; peak_private_gb = $old.peak_private_gb; config = $old.config; effective = $old.effective
    }
    if (-not (Write-Summary $From $facts)) { exit 1 }
    return
}

if ($Build) {
    Push-Location $root
    try {
        cargo build --profile diag -p victory --features diagnostics
        if ($LASTEXITCODE -ne 0) { throw 'build failed' }
    } finally { Pop-Location }
}
if (-not (Test-Path $victory)) {
    throw "no diagnostics build at $victory (cargo build --profile diag -p victory --features diagnostics, or -Build)"
}

# The configuration as `victory config` resolves it (victory.toml and the
# VICTORY_*_DIR variables).
Push-Location $root
try { $config = & $victory config } finally { Pop-Location }
# A configured folder inside the repository (docs/asset-policy.md): stop.
$violation = @($config | Where-Object { "$_" -match 'POLICY VIOLATION' })
if ($violation.Count) { throw ('victory config: ' + ($violation -join '; ')) }
function Get-ConfigPath([string]$name) {
    foreach ($line in $config) {
        if ($line -match "^$name\s*:\s*(.+?)(\s+\(does not exist\))?$") { return $Matches[1] }
    }
    return $null
}
$gameDir = Get-ConfigPath 'game_dir'
$cacheDir = Get-ConfigPath 'cache_dir'
$dataDir = Get-ConfigPath 'data_dir'
if (-not $Exe) {
    if (-not $gameDir -or $gameDir -eq '(not set)') { throw 'no game_dir configured (victory.toml or VICTORY_GAME_DIR), and no -Exe' }
    $exes = @(Get-ChildItem -LiteralPath $gameDir -Filter '*.exe' -File)
    if ($exes.Count -ne 1) { throw "game_dir holds $($exes.Count) executables; name one with -Exe" }
    $Exe = $exes[0].FullName
}
if (-not $Seed) { $Seed = $dataDir }
if (-not $Out) {
    # Beside cache_dir, not in it: a run's data folder inside cache_dir is
    # refused by `victory` (the cache may be deleted at any time).
    $Out = Join-Path (Split-Path -Parent $cacheDir) ('bench\{0}-{1}' -f (Get-Date -Format 'yyyyMMdd-HHmmss'), ($Label -replace '[^\w.-]', '_'))
}
# The run folder holds the title's log, frames and saves: never in the repository.
$outFull = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Out).TrimEnd('\', '/') + '\'
$rootFull = [IO.Path]::GetFullPath($root).TrimEnd('\', '/') + '\'
if ($outFull.StartsWith($rootFull, [StringComparison]::OrdinalIgnoreCase)) {
    throw "the run folder $Out is inside the repository ($root); put it beside cache_dir or elsewhere (not inside cache_dir)"
}
$frames = Join-Path $Out 'frames'
$runData = Join-Path $Out 'data'
$keys = Join-Path $Out 'keys.txt'
# Logs go to stderr; stdout has the loader's few plain lines.
$log = Join-Path $Out 'victory.log'
$stdout = Join-Path $Out 'victory.out.txt'
$stdin = Join-Path $Out 'stdin.txt'
New-Item -ItemType Directory -Force -Path $frames | Out-Null
New-Item -ItemType File -Force -Path $keys | Out-Null
New-Item -ItemType File -Force -Path $stdin | Out-Null
if ($FirstBoot) {
    New-Item -ItemType Directory -Force -Path $runData | Out-Null
} elseif (Test-Path -LiteralPath $Seed) {
    Copy-Item -Recurse -Force -LiteralPath $Seed -Destination $runData
} else {
    throw "the seed data folder $Seed does not exist (no saves yet? run once with -FirstBoot)"
}

# What `victory play` is asked: the options, then -Switches (a later --set wins). `play` clears the other
# VICTORY_* variables, so the run's own go here (VICTORY_DATA_DIR is one it keeps: set below).
$playSet = [ordered]@{
    VICTORY_KEYS_FILE     = $keys
    VICTORY_CAPTURE       = $frames
    VICTORY_PIPELINE_BOOK = $(if ($PipelineBook -eq 'on') { '1' } else { '0' })
}
if ($RayTracing -ne 'default') { $playSet['VICTORY_BVH_BUILDER'] = $(if ($RayTracing -eq 'on') { '1' } else { '0' }) }
if ($FrameGen -eq 'off') {
    # (Over what the F1 menu's file may hold; the file itself is checked below.)
    $playSet['VICTORY_FRAMEGEN'] = '0'
    $playSet['VICTORY_FRAMEGEN_X'] = '0'
}
$routeHash = $null
if ($Route) {
    $text = if (Test-Path -LiteralPath $Route -PathType Leaf) { Get-Content -Raw -LiteralPath $Route } else { $Route }
    $entries = @($text -split "`r?`n" | ForEach-Object { $_ -replace '#.*$', '' } | ForEach-Object { $_ -split ',' } |
        ForEach-Object { $_.Trim() } | Where-Object { $_ })
    foreach ($e in $entries) {
        if ($e -notmatch '^\d+(\.\d+)?(-\d+(\.\d+)?)?:\w+$') { throw "-Route: '$e' is not <seconds>:<key> or <from>-<to>:<key>" }
    }
    $playSet['VICTORY_KEYS'] = $entries -join ','
    $playSet['VICTORY_KEYS_FROM'] = 'world'
    $sha = [Security.Cryptography.SHA256]::Create()
    $routeHash = -join ($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($playSet['VICTORY_KEYS'])) | Select-Object -First 8 | ForEach-Object { $_.ToString('x2') })
}
foreach ($k in $Switches.Keys) { $playSet[$k] = [string]$Switches[$k] }
$playArgs = @('play', "`"$Exe`"", '--dlss', $Dlss)
if ($FrameGen -eq 'off') { $playArgs += '--no-framegen-ready' }
elseif ($FrameGen -ne 'ready') { $playArgs += @('--framegen', $FrameGen) }
foreach ($k in $playSet.Keys) { $playArgs += @('--set', "`"$k=$($playSet[$k])`"") }
$config = [ordered]@{
    ray_tracing      = $(if ($RayTracing -eq 'default') { 'default (on)' } else { $RayTracing })
    dlss             = $Dlss
    frame_generation = $FrameGen
    pipeline_book    = $PipelineBook
    route            = $(if (-not $Route) { $null } elseif (Test-Path -LiteralPath $Route -PathType Leaf) { "$(Split-Path -Leaf $Route) ($routeHash)" } else { "inline ($routeHash)" })
}

# `play` reads the F1 menu's saved choices over its options (settings() in play.rs).
$menuNotes = New-Object System.Collections.Generic.List[string]
$menuFile = Join-Path $env:LOCALAPPDATA 'victory\victory-settings.txt'
if (Test-Path -LiteralPath $menuFile) {
    $saved = @{}
    foreach ($l in Get-Content -LiteralPath $menuFile) {
        $n, $v = $l -split '=', 2
        if ($null -ne $v -and $n.Trim() -cmatch '^VICTORY_[A-Z0-9_]+$') { $saved[$n.Trim()] = $v.Trim() }
    }
    $wantFg = if ($FrameGen -match '^[2-6]$') { $FrameGen } else { '0' }
    $clash = New-Object System.Collections.Generic.List[string]
    $expect = @{
        VICTORY_MENU_CONSOLE = 'series-x'; VICTORY_MENU_RES = '1440'; VICTORY_MENU_DLSS = $Dlss; VICTORY_MENU_FPS = '1000'
    }
    foreach ($k in $expect.Keys) {
        if ($saved.ContainsKey($k) -and $saved[$k] -ne $expect[$k]) { $clash.Add("$k=$($saved[$k]) (the run wants $($expect[$k]))") }
    }
    if ($saved.ContainsKey('VICTORY_MENU_FRAMEGEN')) {
        $fg = $saved['VICTORY_MENU_FRAMEGEN']
        if ($fg -eq '1') { $fg = '2' }
        if ($fg -ne $wantFg) { $clash.Add("VICTORY_MENU_FRAMEGEN=$($saved['VICTORY_MENU_FRAMEGEN']) (the run wants $wantFg)") }
    }
    if ($clash.Count -and -not $AllowMenuFile) {
        throw ("the F1 menu's saved choices in $menuFile would override the options: " + ($clash -join '; ') +
            '. Put them back in the menu, or pass -AllowMenuFile (the summary then records what play used).')
    }
    foreach ($c in $clash) { $menuNotes.Add("the F1 menu's file overrides the options: $c") }
    foreach ($k in $saved.Keys | Where-Object { $_ -notlike 'VICTORY_MENU_*' } | Sort-Object) {
        $menuNotes.Add("the F1 menu's file sets $k=$($saved[$k]) (overridden by the run's own --set where it has one)")
    }
}
# Without NVIDIA's files `play` asks on its console whether to fetch them (and waits): looked for here as it
# does (streamline_dir in play.rs), so an unattended run stops with a message instead.
if ($Dlss -ne 'off') {
    $sl = @()
    if ($env:VICTORY_STREAMLINE_DIR) { $sl += $env:VICTORY_STREAMLINE_DIR }
    $here = Split-Path -Parent $victory
    $sl += (Join-Path $env:LOCALAPPDATA 'victory\nvidia-sdk\Streamline\bin\x64'), (Join-Path $here 'nvidia-sdk\Streamline\bin\x64'), $here
    if (-not ($sl | Where-Object { Test-Path -LiteralPath (Join-Path $_ 'sl.interposer.dll') })) {
        throw "no NVIDIA files (sl.interposer.dll) for -Dlss ${Dlss}: run 'victory get-dlss' first, or use -Dlss off"
    }
}

function Send-Keys([string[]]$names) {
    # Appended through a shared stream, not Add-Content: Windows PowerShell's
    # Add-Content also opens the file to read it and fails ("stream was not
    # readable") when that lands on one of the title's reads of the file
    # (VICTORY_KEYS_FILE, every 50 ms). A clash left is retried.
    $bytes = [Text.Encoding]::ASCII.GetBytes((($names | ForEach-Object { "$_`r`n" }) -join ''))
    for ($try = 1; ; $try++) {
        try {
            $stream = [IO.File]::Open($keys, [IO.FileMode]::Append, [IO.FileAccess]::Write, [IO.FileShare]::ReadWrite)
            try { $stream.Write($bytes, 0, $bytes.Length) } finally { $stream.Dispose() }
            return
        } catch [IO.IOException] {
            # The title reads the file every 50 ms; past about 5 s of clashes
            # (a stalled title, say) the keys are dropped, not the run.
            if ($try -ge 200) { Write-Warning "keys not sent ($($names -join ' ')): $($_.Exception.Message)"; return }
            Start-Sleep -Milliseconds 25
        }
    }
}

# What a captured menu frame shows. The two screens that A alone does not
# pass are told apart by their layout, measured on the title's own frames
# (coordinates on a 1920x1080 capture, scaled to the capture's size): the
# legal notices (Copyright, Photosensitivity, a Next button) and the terms
# (two boxes to tick, a Confirm button). Returns `notices:<focus>`,
# `terms:<focus>:<privacy ticked>:<terms ticked>`, or `other`.
Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;

public static class BenchScreen {
    const double W0 = 1920, H0 = 1080;

    public static string Classify(string path) {
        using (var bmp = new Bitmap(path)) {
            int w = bmp.Width, h = bmp.Height;
            var data = bmp.LockBits(new Rectangle(0, 0, w, h), ImageLockMode.ReadOnly, PixelFormat.Format24bppRgb);
            var px = new byte[data.Stride * h];
            Marshal.Copy(data.Scan0, px, 0, px.Length);
            bmp.UnlockBits(data);
            Func<int, int, double> lum = (x, y) => {
                int i = y * data.Stride + x * 3;
                return 0.114 * px[i] + 0.587 * px[i + 1] + 0.299 * px[i + 2];
            };
            Func<double, int> X = v => (int)Math.Round(v / W0 * w);
            Func<double, int> Y = v => (int)Math.Round(v / H0 * h);
            // Mean brightness of a row segment.
            Func<int, int, int, double> row = (y, x0, x1) => {
                double s = 0; for (int x = x0; x < x1; x++) s += lum(x, y); return s / Math.Max(1, x1 - x0);
            };
            // A horizontal outline at y: the rows around it differ.
            Func<double, double> edge = y0 => {
                int y = Y(y0), x0 = X(25), x1 = X(470); double best = 0;
                for (int d = -1; d <= 1; d++) best = Math.Max(best, Math.Abs(row(y + d + 1, x0, x1) - row(y + d - 1, x0, x1)));
                return best;
            };
            // The menu's focused entry is drawn white.
            Func<double, double, bool> focused = (a, b) => {
                double s = 0; int n = 0;
                for (int y = Y(a); y < Y(b); y++) { s += row(y, X(20), X(300)); n++; }
                return n > 0 && s / n > 180;
            };
            // A box is ticked when its inside holds a mark that stands out.
            Func<double, double, bool> ticked = (y0, y1) => {
                int x0 = X(50), x1 = X(71), a = Y(y0), b = Y(y1);
                var v = new System.Collections.Generic.List<double>();
                for (int y = a; y < b; y++) for (int x = x0; x < x1; x++) v.Add(lum(x, y));
                if (v.Count == 0) return false;
                v.Sort(); double median = v[v.Count / 2]; int far = 0;
                foreach (var p in v) if (Math.Abs(p - median) > 90) far++;
                return far > 0.05 * v.Count;
            };
            if (edge(374.5) > 100 && edge(437.5) > 100 && edge(543) < 10) {
                string f = focused(380, 432) ? "next" : focused(248, 304) ? "photo" : focused(180, 236) ? "copyright" : "none";
                return "notices:" + f;
            }
            if (edge(543) > 25 && edge(606) > 25 && edge(484) > 25 && edge(374.5) < 10) {
                string f = focused(549, 601) ? "confirm" : focused(448, 505) ? "terms" : focused(245, 340) ? "privacy" : "none";
                return "terms:" + f + ":" + (ticked(261, 282) ? 1 : 0) + ":" + (ticked(465, 487) ? 1 : 0);
            }
            return "other";
        }
    }
}
'@

# The key a menu screen needs, or $null to leave it to the A every few
# seconds. Ticking is a toggle, so the terms are only pressed on what the
# frame shows unticked.
function Get-MenuKey([string]$screen) {
    $part = $screen.Split(':')
    switch ($part[0]) {
        'notices' { if ($part[1] -eq 'next') { return 'Enter' } else { return 'Down' } }
        'terms' {
            $privacy = $part[2] -eq '1'
            $terms = $part[3] -eq '1'
            switch ($part[1]) {
                'privacy' { if ($privacy) { return 'Down' } else { return 'Enter' } }
                'terms' { if ($terms) { return 'Down' } else { return 'Enter' } }
                'confirm' { if ($privacy -and $terms) { return 'Enter' } else { return 'Up' } }
                default { return 'Down' }
            }
        }
    }
    return $null
}

# Asks for the next presented frame and returns its file (or $null).
function Get-Frame([double]$timeout = 5) {
    $since = Get-Date
    New-Item -ItemType File -Force -Path (Join-Path $frames 'now') | Out-Null
    while (((Get-Date) - $since).TotalSeconds -lt $timeout) {
        Start-Sleep -Milliseconds 250
        $new = Get-ChildItem -LiteralPath $frames -Filter 'frame-*.png' |
            Where-Object { $_.LastWriteTime -ge $since } | Sort-Object LastWriteTime | Select-Object -Last 1
        if ($new) {
            # The frame is written in one call; give it a moment to finish.
            Start-Sleep -Milliseconds 200
            return $new.FullName
        }
    }
    return $null
}

# The variables the run sets (VICTORY_* and the switches), set inside the
# try below so its finally always puts the old values back.
$saved = @{}
$set = @{
    # (One of the variables `play` keeps; the others go through --set.)
    VICTORY_DATA_DIR  = $runData
    # A plain log, whatever started the script (the lines are matched).
    NO_COLOR          = '1'
}

# Watches the log for the world's first frame line (read as it grows).
$offset = 0
$pending = ''
$worldAt = $null
function Watch-Log {
    if ($script:worldAt -or -not (Test-Path -LiteralPath $log)) { return }
    $stream = [System.IO.File]::Open($log, 'Open', 'Read', 'ReadWrite')
    try {
        if ($stream.Length -le $script:offset) { return }
        [void]$stream.Seek($script:offset, 'Begin')
        $text = $script:pending + (New-Object System.IO.StreamReader($stream)).ReadToEnd()
        $script:offset = $stream.Length
    } finally { $stream.Dispose() }
    $end = $text.LastIndexOf("`n")
    $script:pending = $text.Substring($end + 1)
    if ($end -lt 0) { return }
    foreach ($line in $text.Substring(0, $end).Split("`n")) {
        $m = $lineRe.Match(($line.TrimEnd("`r") -replace "$([char]27)\[[0-9;]*m", ''))
        if ($m.Success -and [int]$m.Groups['draws'].Value -ge $worldDraws) {
            $script:worldAt = Get-Date
            return
        }
    }
}

Write-Host "run folder: $Out"
Write-Host "title:      $Exe"
Write-Host ('setup:      ' + (($config.Keys | ForEach-Object { "$_=$($config[$_])" }) -join ' '))
Write-Host ('switches:   ' + (($Switches.Keys | Sort-Object | ForEach-Object { "$_=$($Switches[$_])" }) -join ' '))
$launched = Get-Date
$proc = $null
$exit = 'stopped by the script'
$peak = 0
# The load, every -SampleEvery s: the title's process (processor share of
# all cores, private memory), the PC's commit charge, and the card (busy
# share, memory used, power) where nvidia-smi answers; empty elsewhere.
$samplesPath = Join-Path $Out 'samples.csv'
$smi = Get-Command nvidia-smi -ErrorAction SilentlyContinue
$cores = [Environment]::ProcessorCount
$lastSample = 0
$lastCpu = $null
function Add-Sample($title) {
    $now = Get-Date
    $cpu = ''
    if ($title) {
        $used = $title.TotalProcessorTime.TotalSeconds
        if ($script:lastCpu -and $script:lastCpu.id -eq $title.Id) {
            $cpu = (100 * ($used - $script:lastCpu.used) / (($now - $script:lastCpu.at).TotalSeconds * $cores)).ToString('0.0', $invariant)
        }
        $script:lastCpu = @{ id = $title.Id; used = $used; at = $now }
    }
    $private = if ($title) { ($title.PrivateMemorySize64 / 1GB).ToString('0.00', $invariant) } else { '' }
    $os = Get-CimInstance Win32_OperatingSystem
    $commit = (($os.TotalVirtualMemorySize - $os.FreeVirtualMemory) / 1MB).ToString('0.00', $invariant)
    $card = @('', '', '')
    if ($smi) {
        $answer = & $smi.Source --query-gpu=utilization.gpu,memory.used,power.draw --format=csv,noheader,nounits 2>$null |
            Select-Object -First 1
        if ($answer) {
            # Numbers only: a field the card does not report ([N/A], text) stays empty.
            $parts = @("$answer" -split ',')
            $card = @(for ($i = 0; $i -lt 3; $i++) {
                $v = 0.0
                if ($i -lt $parts.Count -and
                    [double]::TryParse($parts[$i].Trim(), [Globalization.NumberStyles]::Float, $invariant, [ref]$v)) {
                    $v.ToString('0.##', $invariant)
                } else { '' }
            })
        }
    }
    $row = @($now.ToUniversalTime().ToString('o', $invariant), $cpu, $private, $commit) + $card
    Add-Content -Encoding ASCII -LiteralPath $samplesPath -Value ($row -join ',')
}
if ($SampleEvery -gt 0) { Set-Content -Encoding ASCII -LiteralPath $samplesPath -Value $sampleHead }
try {
    foreach ($k in $set.Keys) {
        $saved[$k] = [Environment]::GetEnvironmentVariable($k, 'Process')
        [Environment]::SetEnvironmentVariable($k, $set[$k], 'Process')
    }
    $launched = Get-Date
    # (Standard input from an empty file: `play` must not wait for an answer on a console.)
    $proc = Start-Process -FilePath $victory -ArgumentList $playArgs -WorkingDirectory $root `
        -RedirectStandardInput $stdin -RedirectStandardOutput $stdout -RedirectStandardError $log -PassThru -NoNewWindow
    # Taking the handle now keeps the exit code readable after the process ends.
    $null = $proc.Handle
    $lastPress = 0
    $lastCapture = 0
    $lastLook = 0
    $lastScreen = ''
    while ($true) {
        Start-Sleep -Milliseconds 500
        Watch-Log
        $t = ((Get-Date) - $launched).TotalSeconds
        if ($proc.HasExited) { $exit = $proc.ExitCode; break }
        # The title runs in a process `victory run` starts: the largest one.
        $title = Get-Process -Name victory -ErrorAction SilentlyContinue | Sort-Object PrivateMemorySize64 -Descending | Select-Object -First 1
        $mem = if ($title) { $title.PrivateMemorySize64 } else { 0 }
        if ($mem -gt $peak) { $peak = $mem }
        if ($SampleEvery -gt 0 -and $t - $lastSample -ge $SampleEvery) { Add-Sample $title; $lastSample = $t }
        if (-not $worldAt) {
            if ($t -gt $BootTimeout) { Write-Warning "the world did not start within $BootTimeout s"; break }
            if ($t -lt 10 -or $t - $lastLook -lt 2) { continue }
            $lastLook = $t
            $frame = Get-Frame
            $screen = if ($frame) { [BenchScreen]::Classify($frame) } else { 'other' }
            if ($screen -ne $lastScreen) {
                Write-Host ('{0,5:0}s  {1}' -f $t, $screen)
                $lastScreen = $screen
            } elseif ($frame) {
                # One frame per screen is kept, to see where a run stopped.
                Remove-Item -LiteralPath $frame -ErrorAction SilentlyContinue
            }
            $key = Get-MenuKey $screen
            if ($key) {
                Send-Keys @($key); $lastPress = $t
            } elseif ($t - $lastPress -ge 4) {
                Send-Keys @('Enter'); $lastPress = $t
            }
            continue
        }
        if ($CaptureEvery -gt 0 -and $t - $lastCapture -ge $CaptureEvery) {
            New-Item -ItemType File -Force -Path (Join-Path $frames 'now') | Out-Null
            $lastCapture = $t
        }
        if ($SkipCutscenes -and $t - $lastPress -ge 8) { Send-Keys @('Enter'); $lastPress = $t }
        # A frame line lands every 5 to 6 s: run past the window's end for the last one.
        if (((Get-Date) - $worldAt).TotalSeconds -ge $Warmup + $Seconds + 9) { break }
    }
} finally {
    # Stopping the title must not keep the variables from being put back.
    try {
        if ($proc -and -not $proc.HasExited) {
            Get-Process -Name victory -ErrorAction SilentlyContinue |
                Where-Object { $_.Id -eq $proc.Id -or $_.StartTime -ge $launched } |
                Stop-Process -Force -ErrorAction SilentlyContinue
            if (-not $proc.WaitForExit(30000)) { Write-Warning 'the title did not exit within 30 s of being stopped' }
        }
    } catch {
        Write-Warning "stopping the title: $_"
    } finally {
        foreach ($k in $saved.Keys) { [Environment]::SetEnvironmentVariable($k, $saved[$k], 'Process') }
    }
}

# What `play` finally used: the header of its own log file (named on its console, `Log: <file>`).
$effective = [ordered]@{}
try {
    $named = @(Get-Content -LiteralPath $stdout -ErrorAction SilentlyContinue | Where-Object { $_ -match '^Log: (.+)$' })
    if ($named.Count) {
        $playLog = ($named[0] -replace '^Log: ', '').Trim()
        $header = Get-Content -LiteralPath $playLog -TotalCount 1
        foreach ($token in ($header -split '\s+')) {
            $kv = $token -split '=', 2
            # (Names of folders and files are left out: user and machine names.)
            if ($kv.Count -eq 2 -and $kv[0] -cmatch '^VICTORY_[A-Z0-9_]+$' -and $kv[1] -notmatch '[\\/:]' -and
                $kv[0] -notmatch '_(DIR|FILE|PLAY|ARGS)$') { $effective[$kv[0]] = $kv[1] }
        }
    }
} catch { Write-Warning "play's log header: $_" }
if ($effective.Count -eq 0) {
    $menuNotes.Add("play's switches could not be read from its log header")
} else {
    $bvh = $effective['VICTORY_BVH_BUILDER']
    if ($RayTracing -eq 'on' -and $bvh -ne '1') { $menuNotes.Add("ray tracing was asked on but play's VICTORY_BVH_BUILDER is '$bvh'") }
    if ($RayTracing -eq 'off' -and $bvh -eq '1') { $menuNotes.Add("ray tracing was asked off but play's VICTORY_BVH_BUILDER is 1") }
}
$cards = @(Get-CimInstance Win32_VideoController)
# `-dirty`: the build may hold changes that are not in the commit. No git (or
# an error on stderr, which 'Stop' turns into a throw) leaves it empty.
$commit = $null
try { $commit = git -C $root describe --always --dirty 2>$null } catch { }
$facts = @{
    label           = $Label
    switches        = $Switches
    commit          = $commit
    gpu             = ($cards | Select-Object -ExpandProperty Name) -join '; '
    driver          = ($cards | Select-Object -ExpandProperty DriverVersion) -join '; '
    exit            = $exit
    peak_private_gb = [math]::Round($peak / 1GB, 1)
    config          = $config
    effective       = $effective
    notes           = $menuNotes
}
if (-not (Write-Summary $Out $facts)) { exit 1 }
