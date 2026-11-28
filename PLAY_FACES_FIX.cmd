@echo off
setlocal
rem GTA VI with DLSS frame generation and facial mesh relocation enabled.
rem Usage: PLAY_FACES_FIX.cmd [mode 1-6], default mode 1.
set "PS1=%~dp0scripts\play-gta6-framegen.ps1"
set "PSH=pwsh"
set "MODE=%~1"
if "%MODE%"=="" set "MODE=1"
if exist "C:\Program Files\PowerShell\7\pwsh.exe" set "PSH=C:\Program Files\PowerShell\7\pwsh.exe"

if not exist "%PS1%" (
  echo Not found: %PS1%
  pause
  exit /b 1
)

"%PSH%" -NoProfile -ExecutionPolicy Bypass -File "%PS1%" -Mode %MODE% -FaceMeshFix
echo.
pause
