@echo off
rem Fetches AMD's FidelityFX SDK (1.1.4), builds FSR 3's bridge against it, and puts fsr3_bridge.dll
rem with AMD's amd_fidelityfx_dx12.dll where the launcher looks for them (fsr3-sdk\ in this folder).
rem Run it once. They are AMD's files under AMD's licences, which are saved beside them; this repository
rem does not contain them. Without them the game still runs, with its own upscaling and no frame generation.
rem CMake and Visual Studio with C++ are needed for the one-off build of the bridge.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\get-fsr3.ps1" %*
echo.
pause
