@echo off
setlocal EnableExtensions DisableDelayedExpansion
set "VICTORY_PREPARE_ONLY=1"
call "%~dp0Play-Radeon-1080p.cmd" "%~1"
set "result=%errorlevel%"
if not "%result%"=="0" goto failed
echo Cache preparation finished.
if defined VICTORY_NO_PAUSE exit /b 0
choice /c PX /n /m "Press P to play using the prepared cache, or X to exit. "
if not "%errorlevel%"=="1" exit /b 0
set "VICTORY_PREPARE_ONLY="
set "VICTORY_PLAY_PREPARED=1"
call "%~dp0Play-Radeon-1080p.cmd" "%~1"
exit /b %errorlevel%
:failed
echo Cache preparation exited with code %result%.
if not defined VICTORY_NO_PAUSE pause
exit /b %result%
