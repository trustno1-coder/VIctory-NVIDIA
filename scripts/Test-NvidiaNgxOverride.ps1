# Detects NVIDIA App / NGX per-game DLSS override plugins (sl_*_override_0).
# When active, Streamline loads override DLLs instead of VICTORY_STREAMLINE_DIR and
# DLSS evaluate often fails with 0xbad00005 (see issue #449, #99).
param([switch]$Quiet)
$roots = @(
    Join-Path $env:ProgramData 'NVIDIA\NGX\models'
    'C:\ProgramData\NVIDIA\NGX\models'
)
$hits = [System.Collections.Generic.List[string]]::new()
foreach ($root in $roots) {
    if (-not (Test-Path -LiteralPath $root)) { continue }
    Get-ChildItem -LiteralPath $root -Directory -Filter 'sl_*_override_*' -ErrorAction SilentlyContinue |
        ForEach-Object { $hits.Add($_.FullName) }
}
if ($hits.Count -eq 0) {
    if (-not $Quiet) { Write-Host 'NGX DLSS override: not detected under ProgramData\NVIDIA\NGX\models' }
    return 0
}
if (-not $Quiet) {
    Write-Warning 'NVIDIA NGX DLSS override is present. Streamline may load override DLLs instead of nvidia-sdk.'
    Write-Warning 'If cutscenes are black or DLSS logs 0xbad00005, disable DLSS Override for victory.exe in NVIDIA App.'
    $hits | ForEach-Object { Write-Warning "  $_" }
}
return 1