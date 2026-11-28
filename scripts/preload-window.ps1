# The preload's progress window: the title's splash screen with the step the preload is at, how far it is, and
# about how long is left. play-foxhound.ps1 dot-sources this and passes it every line `victory preload` prints;
# the window runs on a thread of its own, so it stays responsive while the preload is quiet. Skip (or Esc) ends
# the preload (what it did is kept) and lets the game start; the window closes when the game's window opens.
# Nothing here is needed for a start: if the window cannot open, the preload goes on in the console alone.
#
#   . .\scripts\preload-window.ps1
#   $w = Start-PreloadWindow -Splash (Get-SplashImage $Exe '') -StateFile <file>
#   ... | ForEach-Object { Update-PreloadWindow $w "$_" }
#   Complete-PreloadWindow $w 'Preload done after 4.2 min.' $true

# The picture behind the window: -Splash when given (a file, or an http(s) URL fetched once into
# %LOCALAPPDATA%\victory\splash), else the title's own splash screen (SplashScreenImage in MicrosoftGame.config
# beside the executable). '' when there is none: the window is then plain.
function Get-SplashImage([string]$Exe, [string]$Splash) {
    if ($Splash -match '^https?://') {
        $dir = Join-Path $env:LOCALAPPDATA 'victory\splash'
        $name = [IO.Path]::GetFileName(([Uri]$Splash).AbsolutePath)
        if (-not $name) { $name = 'splash.jpg' }
        $file = Join-Path $dir $name
        if (-not (Test-Path -LiteralPath $file)) {
            try {
                New-Item -ItemType Directory -Force $dir | Out-Null
                $ProgressPreference = 'SilentlyContinue'
                Invoke-WebRequest -UseBasicParsing -Uri $Splash -OutFile "$file.part" -TimeoutSec 30
                Move-Item -LiteralPath "$file.part" $file -Force
            } catch {
                Remove-Item -LiteralPath "$file.part" -ErrorAction SilentlyContinue
                Write-Host "Could not get $Splash ($($_.Exception.Message)): the title's own splash instead."
            }
        }
        if (Test-Path -LiteralPath $file) { return $file }
    } elseif ($Splash) {
        if (Test-Path -LiteralPath $Splash) { return (Resolve-Path -LiteralPath $Splash).Path }
        Write-Host "No $Splash`: the title's own splash instead."
    }
    $dir = [IO.Path]::GetDirectoryName($Exe)
    $name = 'xbs_splash.png'
    $config = Join-Path $dir 'MicrosoftGame.config'
    if ((Test-Path -LiteralPath $config) -and (Get-Content -LiteralPath $config -Raw) -match 'SplashScreenImage="([^"]+)"') {
        $name = $Matches[1]
    }
    $file = Join-Path $dir $name
    if (Test-Path -LiteralPath $file) { return $file }
    return ''
}

# The title's square logo (Square150x150Logo in MicrosoftGame.config) for the window's taskbar icon, or ''.
function Get-SplashIcon([string]$Exe) {
    $dir = [IO.Path]::GetDirectoryName($Exe)
    $config = Join-Path $dir 'MicrosoftGame.config'
    if ((Test-Path -LiteralPath $config) -and (Get-Content -LiteralPath $config -Raw) -match 'Square150x150Logo="([^"]+)"') {
        $file = Join-Path $dir $Matches[1]
        if (Test-Path -LiteralPath $file) { return $file }
    }
    return ''
}

# The window itself, run in a runspace of its own (STA, its own message loop). It reads $sync, which the
# launcher's thread writes.
$script:PreloadWindowUi = {
    param($sync)
    try {
        Add-Type -AssemblyName System.Windows.Forms, System.Drawing
        if (-not ('VictoryPreloadNative' -as [type])) {
            Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class VictoryPreloadNative {
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern bool ReleaseCapture();
    [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr h, int msg, IntPtr w, IntPtr l);
    [DllImport("dwmapi.dll")] public static extern int DwmSetWindowAttribute(IntPtr h, int attr, ref int value, int size);
}
'@
        }
        # Sharp text at any display scale (the console is another process: it is not affected).
        [void][VictoryPreloadNative]::SetProcessDPIAware()
        [Windows.Forms.Application]::EnableVisualStyles()

        $ink = [Drawing.Color]::FromArgb(255, 18, 7, 30)        # the splash's own dark purple
        $pink = [Drawing.Color]::FromArgb(255, 255, 77, 148)
        $orange = [Drawing.Color]::FromArgb(255, 255, 168, 64)
        $white = [Drawing.Color]::FromArgb(255, 255, 255, 255)
        $soft = [Drawing.Color]::FromArgb(205, 255, 255, 255)
        $dim = [Drawing.Color]::FromArgb(115, 255, 255, 255)

        # 16:9, 1120 wide at 100 % scale, never more than 62 % of the screen's width. $u: one layout pixel.
        $g0 = [Drawing.Graphics]::FromHwnd([IntPtr]::Zero); $dpi = $g0.DpiX / 96; $g0.Dispose()
        $wa = [Windows.Forms.Screen]::PrimaryScreen.WorkingArea
        $W = [int][Math]::Min(1120 * $dpi, $wa.Width * 0.62); $H = [int]($W * 9 / 16); $u = $W / 1120

        # The splash, cover-fitted, with the dark band the text sits on, drawn once.
        $back = New-Object Drawing.Bitmap($W, $H, [Drawing.Imaging.PixelFormat]::Format32bppPArgb)
        $g = [Drawing.Graphics]::FromImage($back)
        $g.Clear($ink)
        $g.InterpolationMode = 'HighQualityBicubic'; $g.PixelOffsetMode = 'HighQuality'
        if ($sync.Splash) {
            try {
                $img = [Drawing.Image]::FromFile($sync.Splash)
                $k = [Math]::Max($W / $img.Width, $H / $img.Height)
                $dw = $img.Width * $k; $dh = $img.Height * $k
                $g.DrawImage($img, [single](($W - $dw) / 2), [single](($H - $dh) / 2), [single]$dw, [single]$dh)
                $img.Dispose()
            } catch { }
        }
        $top = [int]($H * 0.58)
        $shade = New-Object Drawing.Drawing2D.LinearGradientBrush((New-Object Drawing.Rectangle(0, ($top - 1), $W, ($H - $top + 2))), $ink, $ink, [single]90)
        $blend = New-Object Drawing.Drawing2D.ColorBlend(3)
        $blend.Colors = [Drawing.Color[]]@([Drawing.Color]::FromArgb(0, $ink), [Drawing.Color]::FromArgb(222, $ink), [Drawing.Color]::FromArgb(246, $ink))
        $blend.Positions = [single[]]@(0, 0.42, 1)
        $shade.InterpolationColors = $blend
        $g.FillRectangle($shade, 0, $top, $W, $H - $top)
        $shade.Dispose(); $g.Dispose()

        $px = [Drawing.GraphicsUnit]::Pixel; $regular = [Drawing.FontStyle]::Regular
        $fEyebrow = New-Object Drawing.Font('Segoe UI Semibold', [single](12.5 * $u), $regular, $px)
        $fTitle = New-Object Drawing.Font('Segoe UI Semibold', [single](30 * $u), $regular, $px)
        $fPercent = New-Object Drawing.Font('Segoe UI Light', [single](30 * $u), $regular, $px)
        $fStats = New-Object Drawing.Font('Segoe UI', [single](15 * $u), $regular, $px)
        $fSkip = New-Object Drawing.Font('Segoe UI Semibold', [single](15 * $u), $regular, $px)
        $fDetail = New-Object Drawing.Font('Consolas', [single](11.5 * $u), $regular, $px)
        $sfL = [Drawing.StringFormat]::GenericTypographic.Clone()
        $sfL.FormatFlags = [Drawing.StringFormatFlags]::NoWrap -bor [Drawing.StringFormatFlags]::LineLimit
        $sfL.Trimming = [Drawing.StringTrimming]::EllipsisCharacter
        $sfR = $sfL.Clone(); $sfR.Alignment = [Drawing.StringAlignment]::Far

        # Layout, from the bottom up.
        $pad = 40 * $u; $wide = $W - 2 * $pad; $barH = 6 * $u
        $yDetail = $H - $pad - 14 * $u
        $yStats = $yDetail - 34 * $u
        $yBar = $yStats - 20 * $u
        $yTitle = $yBar - 50 * $u
        $yEyebrow = $yTitle - 20 * $u

        $view = @{ Shown = 0.0; SkipRect = $null; Hover = $false; WaitFrom = $null; NextCheck = [DateTime]::MinValue; Fading = $false }
        $clock = [Diagnostics.Stopwatch]::StartNew()

        function Write-Text($g, [string]$s, $font, $color, [single]$x, [single]$y, [single]$w, $sf) {
            $h = [single]($font.Height * 1.4)
            $shadow = New-Object Drawing.SolidBrush([Drawing.Color]::FromArgb(150, 0, 0, 0))
            $g.DrawString($s, $font, $shadow, (New-Object Drawing.RectangleF(($x + $u), ($y + $u), $w, $h)), $sf)
            $shadow.Dispose()
            $b = New-Object Drawing.SolidBrush($color)
            $g.DrawString($s, $font, $b, (New-Object Drawing.RectangleF($x, $y, $w, $h)), $sf)
            $b.Dispose()
        }
        function Get-Pill([single]$x, [single]$y, [single]$w, [single]$h) {
            if ($w -lt $h) { $w = $h }
            $p = New-Object Drawing.Drawing2D.GraphicsPath
            $p.AddArc($x, $y, $h, $h, 90, 180); $p.AddArc($x + $w - $h, $y, $h, $h, 270, 180); $p.CloseFigure()
            $p
        }
        function Format-Clock([double]$s) { '{0}:{1:00}' -f [int][Math]::Floor($s / 60), [int][Math]::Floor($s % 60) }
        function Format-Left([double]$s) {
            if ($s -lt 45) { 'less than a minute left' }
            elseif ($s -lt 90) { 'about a minute left' }
            elseif ($s -lt 5400) { 'about {0} min left' -f [int][Math]::Round($s / 60) }
            else { 'about {0:N1} h left' -f ($s / 3600) }
        }
        # How far the step is, 0 to 1, or -1 when it cannot be told.
        function Get-Fraction {
            if ($sync.Phase -ne 'preload') { return -1 }
            $total = [double]$sync.Total
            if ($total -le 0) { return -1 }
            [Math]::Min(1.0, [double]$sync.Done / $total)
        }

        $form = New-Object Windows.Forms.Form
        $form.FormBorderStyle = 'None'
        $form.StartPosition = 'CenterScreen'
        $form.ClientSize = New-Object Drawing.Size($W, $H)
        $form.Text = 'Victory: preloading shaders'
        $form.BackColor = $ink
        $form.KeyPreview = $true
        $form.GetType().GetProperty('DoubleBuffered', [Reflection.BindingFlags]'Instance,NonPublic').SetValue($form, $true, $null)
        if ($sync.Icon) {
            try {
                $logo = [Drawing.Image]::FromFile($sync.Icon)
                $small = New-Object Drawing.Bitmap($logo, 64, 64); $logo.Dispose()
                $form.Icon = [Drawing.Icon]::FromHandle($small.GetHicon())
            } catch { }
        }

        $skip = {
            if ($sync.Phase -ne 'preload' -or $sync.Skipped) { return }
            $answer = [Windows.Forms.MessageBox]::Show($form,
                "Stop the preload and start the game now?`n`nWhat is done so far is kept: the next preload goes on from there.",
                'Victory', 'YesNo', 'Question')
            if ($answer -ne 'Yes' -or $sync.Phase -ne 'preload') { return }
            $sync.Skipped = $true
            # `victory preload` is the launcher's child; /T takes the pipeline processes it started with it.
            Get-CimInstance Win32_Process -Filter "ParentProcessId=$($sync.HostPid) AND Name='victory.exe'" -ErrorAction SilentlyContinue |
                ForEach-Object { & taskkill.exe /T /F /PID $_.ProcessId 2>&1 | Out-Null }
            $form.Invalidate()
        }

        $form.Add_Paint({
            param($sender, $e)
            try {
                $g = $e.Graphics
                $g.DrawImageUnscaled($back, 0, 0)
                $g.SmoothingMode = 'AntiAlias'
                $g.TextRenderingHint = 'AntiAliasGridFit'
                $now = [DateTime]::UtcNow
                $phase = $sync.Phase; $step = [int]$sync.Step
                $fraction = Get-Fraction
                $view.SkipRect = $null

                if ($phase -eq 'preload') {
                    $eyebrow = 'PRELOADING SHADERS'
                    $title = switch ($step) { 1 { "Translating the game's shaders" } 2 { 'Building the pipelines from earlier runs' } default { 'Storing the pipelines for loading' } }
                    $done = [int]$sync.Done; $total = [int]$sync.Total
                    $parts = @()
                    $parts += switch ($step) {
                        1 { if ($total -gt 0) { '{0:N0} of about {1:N0} shaders' -f $done, $total } else { '{0:N0} shaders translated' -f $done } }
                        2 { if ($sync.Counted -and $total -eq 0) { 'none new to build' } elseif ($total -gt 0) { '{0:N0} of {1:N0} pipelines' -f $done, $total } else { 'counting the pipelines' } }
                        default { if ($total -gt 0) { '{0:N0} of {1:N0} pipelines stored' -f $done, $total } else { 'counting the pipelines' } }
                    }
                    if ($step -eq 2 -and [int]$sync.Failed -gt 0) { $parts += '{0:N0} failed' -f [int]$sync.Failed }
                    $inStep = ($now - $sync.StepBegan).TotalSeconds
                    if ($fraction -ge 0.01 -and $fraction -lt 1 -and $inStep -ge 8) { $parts += (Format-Left ($inStep * (1 - $fraction) / $fraction)) + ' in this step' }
                    $parts += (Format-Clock ($now - $sync.Began).TotalSeconds) + ' elapsed'
                    # (Characters past ASCII by their codes: Windows PowerShell reads a .ps1 without a BOM as ANSI.)
                    $stats = $parts -join "   $([char]0x00B7)   "
                    $detail = [string]$sync.Last
                    if ($sync.Skipped) { $title = 'Stopping the preload'; $stats = 'What is done so far is kept.' }
                } else {
                    $eyebrow = if ($sync.Ok) { 'SHADERS READY' } else { 'PRELOAD STOPPED' }
                    $title = 'Starting the game'
                    $stats = [string]$sync.Note
                    $detail = "This window closes when the game's window opens."
                }

                # Eyebrow, and the three steps on its right: done ones pink, this one white, the rest dim.
                Write-Text $g $eyebrow $fEyebrow $pink $pad $yEyebrow ($wide / 2) $sfL
                $cx = $W - $pad
                $names = 'SHADERS', 'PIPELINES', 'LIBRARY'
                for ($i = 2; $i -ge 0; $i--) {
                    $size = $g.MeasureString($names[$i], $fEyebrow, [Drawing.PointF]::Empty, $sfL)
                    $cx -= $size.Width
                    $color = if ($phase -ne 'preload' -or $i + 1 -lt $step) { $pink } elseif ($i + 1 -eq $step) { $white } else { $dim }
                    Write-Text $g $names[$i] $fEyebrow $color $cx $yEyebrow ($size.Width + 4 * $u) $sfL
                    $cx -= 22 * $u
                }

                # Title, and the step's percentage on its right.
                $percentW = 0
                if ($fraction -ge 0) {
                    $percent = '{0}%' -f [int][Math]::Floor(100 * $view.Shown)
                    $percentW = $g.MeasureString('100%', $fPercent, [Drawing.PointF]::Empty, $sfL).Width
                    Write-Text $g $percent $fPercent $white ($W - $pad - $percentW) $yTitle $percentW $sfR
                }
                Write-Text $g $title $fTitle $white $pad $yTitle ($wide - $percentW - 20 * $u) $sfL

                # The bar: the step's part, or a sweep when that cannot be told.
                $track = Get-Pill $pad $yBar $wide $barH
                $trackBrush = New-Object Drawing.SolidBrush([Drawing.Color]::FromArgb(48, 255, 255, 255))
                $g.FillPath($trackBrush, $track); $trackBrush.Dispose()
                $fill = New-Object Drawing.Drawing2D.LinearGradientBrush((New-Object Drawing.RectangleF(($pad - 1), $yBar, ($wide + 2), $barH)), $pink, $orange, [single]0)
                if ($fraction -ge 0) {
                    if ($view.Shown -gt 0.002) { $g.FillPath($fill, (Get-Pill $pad $yBar ($wide * $view.Shown) $barH)) }
                } else {
                    $seg = 0.22 * $wide
                    $t = ($clock.Elapsed.TotalSeconds / 1.6) % 1
                    $g.SetClip($track)
                    $g.FillPath($fill, (Get-Pill ($pad - $seg + $t * ($wide + $seg)) $yBar $seg $barH))
                    $g.ResetClip()
                }
                $fill.Dispose(); $track.Dispose()

                # Stats, and Skip on their right while the preload runs.
                $skipW = 0
                if ($phase -eq 'preload' -and -not $sync.Skipped) {
                    $label = "Skip and start the game  $([char]0x203A)"
                    $skipW = $g.MeasureString($label, $fSkip, [Drawing.PointF]::Empty, $sfL).Width + 4 * $u
                    $view.SkipRect = New-Object Drawing.RectangleF(($W - $pad - $skipW - 8 * $u), ($yStats - 6 * $u), ($skipW + 16 * $u), (32 * $u))
                    $color = if ($view.Hover) { $orange } else { $pink }
                    Write-Text $g $label $fSkip $color ($W - $pad - $skipW) $yStats $skipW $sfR
                }
                Write-Text $g $stats $fStats $soft $pad $yStats ($wide - $skipW - 24 * $u) $sfL
                Write-Text $g $detail $fDetail $dim $pad $yDetail $wide $sfL
            } catch { $sync.Error = "$_" }
        })

        $form.Add_MouseMove({
            param($sender, $e)
            $over = [bool]($view.SkipRect -and $view.SkipRect.Contains([single]$e.X, [single]$e.Y))
            $form.Cursor = if ($over) { [Windows.Forms.Cursors]::Hand } else { [Windows.Forms.Cursors]::Default }
            if ($over -ne $view.Hover) { $view.Hover = $over; $form.Invalidate() }
        })
        $form.Add_MouseDown({
            param($sender, $e)
            if ($e.Button -ne 'Left') { return }
            if ($view.SkipRect -and $view.SkipRect.Contains([single]$e.X, [single]$e.Y)) { & $skip; return }
            # Anywhere else drags the window.
            [void][VictoryPreloadNative]::ReleaseCapture()
            [void][VictoryPreloadNative]::SendMessage($form.Handle, 0xA1, [IntPtr]2, [IntPtr]::Zero)
        })
        $form.Add_KeyDown({ param($sender, $e) if ($e.KeyCode -eq 'Escape') { & $skip } })
        $form.Add_HandleCreated({
            # Windows 11's rounded corners (DWMWA_WINDOW_CORNER_PREFERENCE = round); refused, harmlessly, on 10.
            $round = 2
            [void][VictoryPreloadNative]::DwmSetWindowAttribute($form.Handle, 33, [ref]$round, 4)
        })
        # In front once when it opens (Windows may otherwise keep it behind the window in use), not after: a
        # long preload leaves the PC free for other things.
        $form.Add_Shown({
            $sync.Bounds = $form.Bounds; $sync.Hwnd = $form.Handle
            $form.TopMost = $true; $form.Activate(); $form.TopMost = $false
        })

        $timer = New-Object Windows.Forms.Timer
        $timer.Interval = 50
        $timer.Add_Tick({
            if ($sync.Close) { $timer.Stop(); $form.Close(); return }
            $target = Get-Fraction
            if ($target -ge 0) {
                # A step that went back to 0 starts again from there; otherwise ease toward it.
                if ($target -lt $view.Shown - 0.05) { $view.Shown = $target } else { $view.Shown += ($target - $view.Shown) * 0.15 }
            } else { $view.Shown = 0.0 }
            if ($sync.Phase -ne 'preload') {
                $now = [DateTime]::UtcNow
                if (-not $view.WaitFrom) { $view.WaitFrom = $now }
                if (-not $view.Fading -and $now -ge $view.NextCheck) {
                    $view.NextCheck = $now.AddSeconds(1)
                    $game = Get-Process victory -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
                    if ($game -or ($now - $view.WaitFrom).TotalSeconds -gt 180) { $view.Fading = $true }
                }
                if ($view.Fading) {
                    $o = $form.Opacity - 0.08
                    if ($o -le 0) { $timer.Stop(); $form.Close(); return }
                    $form.Opacity = $o
                }
            }
            $form.Invalidate()
        })
        $timer.Start()
        [void]$form.ShowDialog()
        $timer.Dispose(); $form.Dispose(); $back.Dispose()
    } catch {
        $sync.Error = "$_"
    }
}

# Opens the window and returns what Update-/Complete-/Close-PreloadWindow take, or $null when it cannot open.
# $StateFile keeps how many shaders the last preload translated: the first step's total next time.
function Start-PreloadWindow([string]$Splash, [string]$Icon, [string]$StateFile) {
    try {
        $now = [DateTime]::UtcNow
        $sync = [hashtable]::Synchronized(@{
                Splash = $Splash; Icon = $Icon; StateFile = $StateFile; HostPid = $PID
                Began = $now; StepBegan = $now; Phase = 'preload'; Step = 1; Done = 0; Total = 0; Counted = $false
                Failed = 0; Last = ''; Note = ''; Ok = $false; Skipped = $false; Close = $false; Error = ''
            })
        if ($StateFile -and (Test-Path -LiteralPath $StateFile) -and (Get-Content -LiteralPath $StateFile -Raw) -match 'shaders=(\d+)') {
            $sync.Total = [int]$Matches[1]
        }
        $rs = [runspacefactory]::CreateRunspace()
        $rs.ApartmentState = 'STA'; $rs.ThreadOptions = 'ReuseThread'
        $rs.Open()
        $ps = [powershell]::Create(); $ps.Runspace = $rs
        [void]$ps.AddScript($script:PreloadWindowUi.ToString()).AddArgument($sync)
        $sync.Async = $ps.BeginInvoke()
        $sync.PowerShell = $ps
        return $sync
    } catch {
        Write-Host "No progress window ($($_.Exception.Message)); the preload's progress is below."
        return $null
    }
}

# Reads one line of `victory preload` (apps/victory/src/translate.rs `precompile`, warm.rs `run` and
# `into_library` print them).
function Update-PreloadWindow($sync, [string]$Line) {
    if (-not $sync) { return }
    $t = $Line.Trim()
    if (-not $t) { return }
    $step = {
        param([int]$n, [int]$total)
        $sync.Step = $n; $sync.Done = 0; $sync.Total = $total; $sync.Counted = $true; $sync.StepBegan = [DateTime]::UtcNow
    }
    switch -regex ($t) {
        '^(\d+) shaders in the cache, (\d+) do not translate' {
            $n = [int]$Matches[1]; $sync.Done = $n; $sync.Total = $n
            if ($sync.StateFile) { try { Set-Content -LiteralPath $sync.StateFile -Value "shaders=$n" -Encoding ascii } catch { } }
            break
        }
        '^(\d+) shaders in the cache, \d+ s$' {
            $n = [int]$Matches[1]; $sync.Done = $n
            if ($n -gt [int]$sync.Total) { $sync.Total = 0 }   # more than last time: no telling how many more
            break
        }
        '^\d+ pipelines known, .*; (\d+) to make' { & $step 2 ([int]$Matches[1]); break }
        '^(\d+) of (\d+), \d+ s$' { $sync.Done = [int]$Matches[1]; $sync.Total = [int]$Matches[2]; break }
        '^(\d+) made in \d+ s' { if ($sync.Step -eq 2) { $sync.Done = $sync.Total }; break }
        '^pipeline \d+ .*: ' { $sync.Failed = [int]$sync.Failed + 1; break }
        '^into the pipeline library: (\d+) pipelines' { & $step 3 ([int]$Matches[1]); break }
        '^(\d+) of (\d+) in the library, ' { $sync.Done = [int]$Matches[1]; $sync.Total = [int]$Matches[2]; break }
        '^(\d+) of (\d+) in the pipeline library in' { $sync.Done = $sync.Total; break }
    }
    # The log's time and level are left out.
    $sync.Last = $t -replace '^\S+Z\s+(TRACE|DEBUG|INFO|WARN|ERROR)\s+', ''
}

# The preload is over: the window says so, then fades when the game's window opens.
function Complete-PreloadWindow($sync, [string]$Note, [bool]$Ok) {
    if (-not $sync) { return }
    $sync.Note = $Note; $sync.Ok = $Ok; $sync.Phase = 'starting'
}

function Close-PreloadWindow($sync) {
    if ($sync) { $sync.Close = $true }
}
