param(
    [Parameter(Mandatory=$true)][string]$Exe,
    [ValidateRange(1,3)][int]$MaxAttempts=2,
    [ValidateRange(10,300)][int]$TimeoutSeconds=120,
    [string]$FaultExe='',
    [switch]$PersistentFault,
    [string]$LogDir='./vn03e-recovery-logs'
)
$ErrorActionPreference='Stop'
$Exe=(Resolve-Path -LiteralPath $Exe).Path
if ($FaultExe) { $FaultExe=(Resolve-Path -LiteralPath $FaultExe).Path }
New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
$faultBefore=$env:VICTORY_TEST_FENCE_FAULT
try {
    for ($attempt=1; $attempt -le $MaxAttempts; $attempt++) {
        $inject=$FaultExe -and ($attempt -eq 1 -or $PersistentFault)
        $env:VICTORY_TEST_FENCE_FAULT=if ($inject) {'signal'} else {''}
        $info=New-Object Diagnostics.ProcessStartInfo
        $info.FileName=if ($inject) {$FaultExe} else {$Exe}
        $info.Arguments='upscaler-check --validate-framegen --combined --output 1920x1080 --frames 30 --focus-wait 60'
        $info.UseShellExecute=$false
        $info.RedirectStandardOutput=$true
        $info.RedirectStandardError=$true
        $child=New-Object Diagnostics.Process
        $child.StartInfo=$info
        try {
            if (!$child.Start()) { throw 'Child launch failed' }
            $out=$child.StandardOutput.ReadToEndAsync()
            $err=$child.StandardError.ReadToEndAsync()
            $finished=$child.WaitForExit($TimeoutSeconds*1000)
            if (!$finished) { $child.Kill(); $child.WaitForExit() }
            $text=$out.GetAwaiter().GetResult()+$err.GetAwaiter().GetResult()
            [IO.File]::WriteAllText((Join-Path (Resolve-Path $LogDir) "attempt-$attempt.log"),$text)
            $valid=$finished -and $child.ExitCode -eq 0 -and $text.Contains('Combined SR readback PASS:') -and $text.Contains('PASS=true') -and !$text.Contains('PASS=false') -and $text.Contains('Streamline shut down: ok (0)')
            Write-Output "ATTEMPT=$attempt PID=$($child.Id) EXIT=$($child.ExitCode) VALID=$valid TIMEOUT=$(!$finished)"
            if ($valid) { Write-Output 'PROCESS_RECOVERY=PASS'; exit 0 }
            if ($inject -and !$text.Contains('queue Signal')) { throw 'Fault control did not reach the intended Signal path' }
        } finally {
            if ($child.Id -and !$child.HasExited) { $child.Kill(); $child.WaitForExit() }
            $child.Dispose()
        }
    }
    Write-Output 'PROCESS_RECOVERY=EXHAUSTED'
    exit 1
} finally { $env:VICTORY_TEST_FENCE_FAULT=$faultBefore }
