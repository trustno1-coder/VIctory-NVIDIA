param(
    [Parameter(Mandatory)][string]$SettingsFile,
    [Parameter(Mandatory)][System.Collections.IDictionary]$Menu
)
$ErrorActionPreference = 'Stop'

# Only expose fields the title actually wrote. The same choices validate the
# profile and populate the F1 page. An empty selection restores the user's value.
$controls = [ordered]@{
    grassLod = @('Grass detail', '0.50,0.75,1.00')
    lodScale = @('World draw distance', '0.60,0.80,1.00')
    vehicleHDCount = @('High detail vehicle count', '1,2,4')
    maxTexUpgradesPerFrame = @('Texture upgrades per frame', '2,3,5,8')
    treeTessellationEnabled = @('Tree tessellation', 'false,true')
    shadowParticleShadows = @('Particle shadows', 'false,true')
    waterReflectionSSR = @('Water reflections (SSR)', 'false,true')
    mapMSAA = @('Map anti-aliasing (MSAA)', 'false,true')
    motionBlur = @('Motion blur', 'false,true')
}
function Write-AtomicFile($path, [scriptblock]$write) {
    $temp = Join-Path ([IO.Path]::GetDirectoryName($path)) ([IO.Path]::GetRandomFileName())
    try {
        & $write $temp
        if ([IO.File]::Exists($path)) { [IO.File]::Replace($temp, $path, [System.Management.Automation.Language.NullString]::Value) }
        else { [IO.File]::Move($temp, $path) }
    } finally {
        if ([IO.File]::Exists($temp)) { [IO.File]::Delete($temp) }
    }
}

$path = [IO.Path]::GetFullPath($SettingsFile)
# Keep the original values per title and data directory, outside guest storage.
$title = [IO.Directory]::GetParent($path).Parent.Parent.FullName
$originalsFile = Join-Path $title 'victory-graphics-originals.json'
$originals = [ordered]@{}
$rows = @()
$restore = @()
$added = $false
$changed = $false
$mutex = [Threading.Mutex]::new($false, 'Local\VictoryNativeGraphicsSettings')
$held = $false
$file = $null
try {
    try { $held = $mutex.WaitOne(5000) } catch [Threading.AbandonedMutexException] { $held = $true }
    if (!$held) { throw 'Another launcher is applying graphics settings.' }
    $file = [IO.File]::Open($path, 'Open', 'Read', [IO.FileShare]::Read -bor [IO.FileShare]::Delete)
    $doc = [Xml.XmlDocument]::new()
    $doc.XmlResolver = $null
    $doc.PreserveWhitespace = $true
    $doc.Load($file)
    $section = $doc.SelectSingleNode('/rage__fwSystemSettingsCollection/advancedGraphics')
    if (!$section) { throw 'The title has no advancedGraphics section.' }
    if ([IO.File]::Exists($originalsFile)) {
        $stored = ConvertFrom-Json ([IO.File]::ReadAllText($originalsFile))
        if ($stored -isnot [pscustomobject]) { throw 'Invalid graphics originals file.' }
        foreach ($p in $stored.PSObject.Properties) {
            if ($p.Value -isnot [string]) { throw 'Invalid original graphics value.' }
            $originals[$p.Name] = $p.Value
        }
    }
    foreach ($name in $controls.Keys) {
        $attribute = $section.SelectSingleNode("$name/@value")
        if ($null -eq $attribute) { continue }
        $key = 'VICTORY_GAME_' + $name.ToUpperInvariant()
        $label, $choices = $controls[$name]
        $selected = [string]$Menu[$key]
        $original = if ($originals.Contains($name)) { $originals[$name] } else { $attribute.Value }
        if ($original -notmatch '^(true|false|-?\d+(\.\d+)?)$') { continue }
        if ($selected -and $selected -cnotin $choices.Split(',')) { throw "Invalid $name selection: $selected" }
        $rows += "$key|$label|$original|$choices"
        if ($selected) {
            if (!$originals.Contains($name)) { $originals[$name] = $original; $added = $true }
            $value = $selected
        } elseif ($originals.Contains($name)) {
            $value = $original
            $restore += $name
        } else { continue }
        if ($attribute.Value -cne $value) {
            $attribute.Value = $value
            $changed = $true
        }
    }
    $saveOriginals = { param($temp) [IO.File]::WriteAllText($temp, (ConvertTo-Json -InputObject $originals)) }
    # Save originals first. Interrupted writes can be retried at the next launch.
    if ($added) { Write-AtomicFile $originalsFile $saveOriginals }
    if ($changed) { Write-AtomicFile $path { param($temp) $doc.Save($temp) } }
    if ($restore.Count) {
        foreach ($name in $restore) { $originals.Remove($name) }
        if ($originals.Count) { Write-AtomicFile $originalsFile $saveOriginals }
        else { [IO.File]::Delete($originalsFile) }
    }
} finally {
    if ($file) { $file.Dispose() }
    if ($held) { $mutex.ReleaseMutex() }
    $mutex.Dispose()
}
$rows -join "`n"
