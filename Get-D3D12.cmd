@echo off
rem Downloads Microsoft's newer Direct3D 12 runtime (the D3D12 Agility SDK, D3D12Core.dll, about 22 MB)
rem from nuget.org and puts it where the layer looks for it (target\release\D3D12 in this folder). Only
rem Windows 10 needs it: its own Direct3D 12 stops at shader model 6.5, and the translated shaders are 6.6.
rem Run it once. It is Microsoft's, under Microsoft's licence, which is saved beside it; this repository
rem does not contain it. victory.exe get-d3d12 does the same beside the victory.exe it is run as.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\get-d3d12.ps1" %*
echo.
pause