# 本地社区补丁栈 hefty/community-rendering（#33 #703 #280 #168 #814）
# 用法与 play-nvidia.ps1 完全相同，例如:
#   .\scripts\play-community-rendering.ps1 -Exe 'D:\Entertainment\GTA6\game_scarlett_master.exe' -Dlss off
$repo = Split-Path $PSScriptRoot -Parent
$branch = (git -C $repo branch --show-current 2>$null)
if ($branch -ne 'hefty/community-rendering') {
    Write-Warning "当前分支为 '$branch'，不是 hefty/community-rendering。请先: git checkout hefty/community-rendering; cargo build --release -p victory"
}
$victory = Join-Path $repo 'target\release\victory.exe'
if (-not (Test-Path $victory)) {
    Write-Error "未找到 $victory 。请先 cargo build --release -p victory"
    exit 1
}
& "$PSScriptRoot\play-nvidia.ps1" @args