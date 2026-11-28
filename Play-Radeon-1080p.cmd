@echo off
setlocal EnableExtensions DisableDelayedExpansion
rem Accept a game folder as an argument, VICTORY_GAME_DIR, or a single sibling dump.
set "recovery="
if /i "%~1"=="--recover" (
  set "recovery=-Recovery"
  shift /1
)
set "game=%VICTORY_GAME_DIR%"
if not "%~1"=="" set "game=%~1"
if defined game goto launch
for /d %%D in ("%~dp0..\*") do if exist "%%~fD\game_scarlett_master.exe" (
  if defined game goto ambiguous
  set "game=%%~fD"
)
:launch
if not defined game goto missing
if not exist "%game%\game_scarlett_master.exe" goto missing
if "%VICTORY_PREPARE_ONLY%"=="1" goto prepare
rem Apply the requested UI correction once, when starting the normal player profile.
if defined VICTORY_DATA_DIR goto play
if defined VICTORY_CONFIG goto play
if exist "%~dp0victory.toml" goto play
if exist "%~dp0..\Victory-UI-Scale\Medium.pending" powershell.exe -NoProfile -ExecutionPolicy RemoteSigned -File "%~dp0..\Victory-UI-Scale\Apply-Medium-UI.ps1" -Once
:play
powershell.exe -NoProfile -ExecutionPolicy RemoteSigned -File "%~dp0scripts\play-foxhound.ps1" -Exe "%game%\game_scarlett_master.exe" -Radeon1080p %recovery%
exit /b %errorlevel%
:prepare
powershell.exe -NoProfile -ExecutionPolicy RemoteSigned -File "%~dp0scripts\play-foxhound.ps1" -Exe "%game%\game_scarlett_master.exe" -Radeon1080p -PrepareOnly -FullPreload
exit /b %errorlevel%
:ambiguous
echo Multiple game folders found. Pass your game folder as the first argument. 1>&2
exit /b 2
:missing
echo Pass the folder containing game_scarlett_master.exe as the first argument. 1>&2
exit /b 2
