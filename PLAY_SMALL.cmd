@echo off
rem One click for a small PC (8 GB VRAM, 16 GB RAM): scripts\play-small.ps1 with the tested set
rem (docs\small-config.md). Needs game_dir in victory.toml. Arguments go on to the script, e.g.:
rem   PLAY_SMALL.cmd -NoFrameGen
rem   PLAY_SMALL.cmd -SkipLevels 0
rem   PLAY_SMALL.cmd -Preload       (once per new build or driver: shaders and pipelines made first, no stutter)
setlocal
set "PSH=powershell"
where pwsh >nul 2>nul && set "PSH=pwsh"
%PSH% -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\play-small.ps1" %*
pause
