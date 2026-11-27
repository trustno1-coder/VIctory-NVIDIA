# SPDX-License-Identifier: GPL-3.0-or-later
[CmdletBinding()]
param(
    [int]$ProcessId = 0,
    [ValidateRange(1,3600)][int]$Seconds = 60,
    [string]$OutputDir = (Join-Path $env:LOCALAPPDATA ('victory\lowspec-audit-' + (Get-Date -Format 'yyyyMMdd-HHmmss')))
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$cpu = @(Get-CimInstance Win32_Processor | Select-Object Name, NumberOfCores, NumberOfLogicalProcessors, MaxClockSpeed)
$gpu = @(Get-CimInstance Win32_VideoController | Select-Object Name, DriverVersion, PNPDeviceID)
$os = Get-CimInstance Win32_OperatingSystem
$computer = Get-CimInstance Win32_ComputerSystem
$nvsmi = Get-Command nvidia-smi -ErrorAction SilentlyContinue
$nvidia = if ($nvsmi) {
    & $nvsmi.Source --query-gpu=name,driver_version,memory.total,pci.bus_id,pcie.link.gen.max,pcie.link.width.max,power.limit,power.max_limit --format=csv 2>&1 | Out-String
} else { 'BLOCKED: nvidia-smi unavailable' }
$storage = try { @(Get-PhysicalDisk | Select-Object MediaType, BusType, Size, HealthStatus) } catch { @() }
$power = & powercfg /getactivescheme 2>&1 | Out-String
$revision = if (Get-Command git -ErrorAction SilentlyContinue) { & git -C (Split-Path $PSScriptRoot -Parent) rev-parse HEAD 2>$null }
[ordered]@{
    utc = (Get-Date).ToUniversalTime().ToString('o'); revision = $revision
    cpu = $cpu; gpu = $gpu; nvidia = $nvidia; storage = $storage; powerScheme = $power
    os = $os.Caption; build = $os.BuildNumber; physicalBytes = $computer.TotalPhysicalMemory
    pagefiles = @(Get-CimInstance Win32_PageFileUsage | Select-Object AllocatedBaseSize, CurrentUsage, PeakUsage)
    dxgi = 'Use in-process engine DXGI logs; external NVML memory is not the process budget.'
    avx2 = 'Not measured by this script.'
} | ConvertTo-Json -Depth 6 | Set-Content -Encoding UTF8 (Join-Path $OutputDir 'hardware.json')
$rows = for ($i=0; $i -lt $Seconds; $i++) {
    $m = Get-CimInstance Win32_PerfFormattedData_PerfOS_Memory
    $p = if ($ProcessId) { Get-Process -Id $ProcessId -ErrorAction SilentlyContinue }
    $sys = Get-CimInstance Win32_PerfFormattedData_PerfOS_Processor -Filter "Name='_Total'"
    [pscustomobject]@{
        utc=(Get-Date).ToUniversalTime().ToString('o')
        availableBytes=$m.AvailableBytes; committedBytes=$m.CommittedBytes; commitLimit=$m.CommitLimit
        systemPageReadsPerSec=$m.PageReadsPersec; systemPagesInputPerSec=$m.PagesInputPersec
        workingSetBytes=if($p){$p.WorkingSet64}; privateBytes=if($p){$p.PrivateMemorySize64}
        processId=if($p){$p.Id}; cpuPercent=$sys.PercentProcessorTime
    }
    Start-Sleep -Seconds 1
}
$rows | Export-Csv -NoTypeInformation -Encoding UTF8 (Join-Path $OutputDir 'memory.csv')
Write-Host "Audit written to $OutputDir"
