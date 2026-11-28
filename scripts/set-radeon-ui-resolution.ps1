param(
    [Parameter(Mandatory)][string]$SettingsFile,
    [switch]$StockCanvas
)
$ErrorActionPreference = 'Stop'

# Read through a handle that excludes writers until the atomic replacement.
$path = [IO.Path]::GetFullPath($SettingsFile)
$file = [IO.File]::Open($path, 'Open', 'Read', [IO.FileShare]::Read -bor [IO.FileShare]::Delete)
$temporary = $null
try {
    $xml = [Xml.XmlDocument]::new()
    $xml.XmlResolver = $null
    $xml.PreserveWhitespace = $true
    $xml.Load($file)
    $video = $xml.SelectSingleNode('/rage__fwSystemSettingsCollection/video')
    if (!$video) { throw 'The settings file has no video section.' }
    $screen = $video.SelectSingleNode('resolutionFullscreen')
    $ui = $video.SelectSingleNode('uiResolution')
    if (!$screen -or !$ui) { throw 'The settings file has no fullscreen or UI resolution.' }
    $screenSize = "$($screen.width.value)x$($screen.height.value)"
    $uiSize = "$($ui.width.value)x$($ui.height.value)"
    if ($screenSize -notin '1920x1080','3840x2160' -or $uiSize -notin '0x0','1920x1080') {
        Write-Host "Keeping custom game dimensions ($screenSize fullscreen, $uiSize UI)."
        return
    }
    if ($StockCanvas) {
        $screenWidth, $screenHeight, $uiWidth, $uiHeight = '3840', '2160', '0', '0'
    } else {
        $screenWidth, $screenHeight, $uiWidth, $uiHeight = '1920', '1080', '1920', '1080'
    }
    if ($screenSize -eq "${screenWidth}x${screenHeight}" -and $uiSize -eq "${uiWidth}x${uiHeight}") { return }
    $screen.width.SetAttribute('value', $screenWidth)
    $screen.height.SetAttribute('value', $screenHeight)
    $ui.width.SetAttribute('value', $uiWidth)
    $ui.height.SetAttribute('value', $uiHeight)
    $temporary = Join-Path ([IO.Path]::GetDirectoryName($path)) ([IO.Path]::GetRandomFileName())
    $xml.Save($temporary)
    # PowerShell 5.1 marshals $null as an empty string for File.Replace.
    [IO.File]::Replace($temporary, $path, [System.Management.Automation.Language.NullString]::Value)
    Write-Host "Updated game video dimensions to ${screenWidth}x${screenHeight} fullscreen, ${uiWidth}x${uiHeight} UI."
} finally {
    $file.Dispose()
    if ($temporary -and [IO.File]::Exists($temporary)) { [IO.File]::Delete($temporary) }
}
