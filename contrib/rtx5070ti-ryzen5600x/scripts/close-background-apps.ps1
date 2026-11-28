# Closes the user's programs before playing, so the game gets as much of the CPU as it can: on a
# Ryzen 5 5600X the layer is CPU-bound (the list-translation threads alone use ~6 cores' worth), and
# a sync client measured there took ~8 % of the whole CPU all the time.
#
# How it closes:
#   1. Programs with a window are asked to close (as clicking X). One with unsaved work may ask; it is
#      left alone (unless it is in the background list below, e.g. browsers, which restore their tabs).
#   2. Known background programs (FORCE) are ended, program by program: the main process first. When
#      the main process runs elevated and cannot be ended, the whole program is left as it is (ending
#      only its lower-integrity children left a miner's window blank).
#   3. Programs started by an elevated scheduled task (ELEVATED_TASKS) are ended through their task.
# Never touched: Windows (KEEP), system services (only this session's processes are looked at), the
# game (victory), controller helpers (DS4Windows, GameInput) and console windows (the launcher runs in one).
#
#   -WhatIf : only lists what it would close.   -Then <file> : starts <file> afterwards (e.g. a launcher).
# To keep a program, add its name (no .exe) to KEEP.
param([switch]$WhatIf, [string]$Then = '')
$ErrorActionPreference = 'Continue'

$KEEP = @(
    'System', 'Idle', 'Registry', 'smss', 'csrss', 'wininit', 'winlogon', 'services', 'lsass', 'LsaIso', 'svchost',
    'fontdrvhost', 'dwm', 'Memory Compression', 'Secure System', 'sihost', 'taskhostw', 'explorer', 'ctfmon',
    'TextInputHost', 'ShellExperienceHost', 'StartMenuExperienceHost', 'SearchHost', 'ShellHost', 'RuntimeBroker',
    'dllhost', 'conhost', 'WmiPrvSE', 'audiodg', 'spoolsv', 'dasHost', 'WUDFHost', 'unsecapp', 'LockApp',
    'SecurityHealthSystray', 'SecurityHealthService', 'MsMpEng', 'NisSrv', 'MpDefenderCoreService',
    'smartscreen', 'CHXSmartScreen', 'DefenderSessionHelper', 'SgrmBroker', 'UserOOBEBroker', 'DataExchangeHost',
    'ApplicationFrameHost', 'NVDisplay.Container', 'nvcontainer', 'igfxEM', 'RtkAudUService64', 'RtkAudUService',
    'victory', 'game_scarlett_master',
    'DS4Windows', 'DSX', 'GameInputRedistService', 'GameInputRawInputProxy', 'XboxService',
    'powershell', 'pwsh', 'cmd', 'WindowsTerminal', 'OpenConsole'
)
# Background programs that may be ended (* is a wildcard).
$FORCE = @(
    'OneDrive*', 'Kryptex*',
    'MSIAfterburner', 'RTSS*', 'EncoderServer',
    'chrome', 'msedge', 'msedgewebview2', 'firefox', 'opera*', 'brave',
    'ChatGPT', 'node', 'node_repl', 'python', 'pythonw', 'uv', 'uvx', 'mcp-*',
    'Spotify*', 'Discord*', 'steam*', 'EpicGamesLauncher', 'EADesktop', 'Battle.net', 'GalaxyClient*',
    'GameBar*', 'XboxGameBarWidgets', 'XboxPcAppFT', 'XboxPcApp', 'PhoneExperienceHost', 'CrossDevice*',
    'Widget*', 'MicrosoftStartFeedProvider', 'SnippingTool', 'TabTip', 'SystemSettings', 'Taskmgr',
    'tailscale-ipn', 'vgtray', 'sunshine', 'AUEPMaster', 'DTShellHlp', 'crashpad_handler',
    'Wacom_TabletUser', 'Wacom_TouchUser', 'Wacom_UpdateUtil', 'WacomHost', 'MoNotificationUx',
    'Teams', 'ms-teams', 'Zoom', 'Skype*'
)
# Scheduled tasks that run a program elevated (ended through the task; `schtasks /run` starts it again).
$ELEVATED_TASKS = @('KryptexElevationV2')

function Test-Name([string]$name, [string[]]$list) { foreach ($p in $list) { if ($name -like $p) { return $true } }; return $false }
function Get-CpuUse {
    # % of the whole CPU, a 2-second average (independent of the Windows language).
    $v = 1..2 | ForEach-Object { (Get-CimInstance Win32_PerfFormattedData_PerfOS_Processor -Filter "Name='_Total'").PercentProcessorTime; Start-Sleep -Seconds 1 }
    [math]::Round(($v | Measure-Object -Average).Average, 0)
}
$session = (Get-Process -Id $PID).SessionId
$mine = @($PID, (Get-CimInstance Win32_Process -Filter "ProcessId=$PID").ParentProcessId)
function Get-Candidates { Get-Process | Where-Object { $_.SessionId -eq $session -and $_.Id -notin $mine -and -not (Test-Name $_.Name $KEEP) } }
$names = { param($ps) ($ps | Select-Object -ExpandProperty Name -Unique | Sort-Object) -join ', ' }

$before = Get-CpuUse
$all = @(Get-Candidates)
$windowed = @($all | Where-Object { $_.MainWindowHandle -ne [IntPtr]::Zero })
if ($WhatIf) {
    Write-Host "Nothing closed. CPU now: $before%."
    Write-Host "Would ask to close (windows): $(& $names $windowed)"
    Write-Host "Would end (background): $(& $names @($all | Where-Object { Test-Name $_.Name $FORCE }))"
    return
}
Write-Host "CPU before: $before%. Closing programs with a window..."
foreach ($p in $windowed) { try { [void]$p.CloseMainWindow() } catch {} }
$until = (Get-Date).AddSeconds(8)
while ((Get-Date) -lt $until -and @($windowed | Where-Object { -not $_.HasExited -and -not (Test-Name $_.Name $FORCE) }).Count) { Start-Sleep -Milliseconds 500 }

Write-Host 'Ending background programs...'
foreach ($t in $ELEVATED_TASKS) {
    $task = Get-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue
    if ($task -and $task.State -eq 'Running') { schtasks /end /tn $t | Out-Null; Write-Host "Ended through its task: $t (schtasks /run /tn $t starts it again)." }
}
$parents = @{}; Get-CimInstance Win32_Process | ForEach-Object { $parents[[int]$_.ProcessId] = [int]$_.ParentProcessId }
$left = @()
foreach ($g in @(Get-Candidates | Where-Object { Test-Name $_.Name $FORCE } | Group-Object Name)) {
    $ids = @($g.Group | ForEach-Object { $_.Id })
    $main = @($g.Group | Where-Object { $ids -notcontains $parents[[int]$_.Id] })
    $denied = $false
    foreach ($p in $main) {
        try { Stop-Process -Id $p.Id -Force -ErrorAction Stop } catch { if (Get-Process -Id $p.Id -ErrorAction SilentlyContinue) { $denied = $true } }
    }
    if ($denied) { $left += $g.Name; continue }
    foreach ($p in $g.Group) { try { Stop-Process -Id $p.Id -Force -ErrorAction Stop } catch {} }
}
if ($left.Count) { Write-Host "Left whole (elevated): $(($left | Sort-Object -Unique) -join ', ')" }
Start-Sleep -Seconds 2
$stillOpen = @(Get-Candidates | Where-Object { $_.MainWindowHandle -ne [IntPtr]::Zero })
if ($stillOpen.Count) { Write-Host "Still open (maybe asking to save; close them yourself): $(& $names $stillOpen)" }
Write-Host "Done. CPU: $before% before, $(Get-CpuUse)% now. Nothing was uninstalled or changed: open them again to go back."
if ($Then) { Start-Process -FilePath $Then -WorkingDirectory (Split-Path -Parent $Then) }
