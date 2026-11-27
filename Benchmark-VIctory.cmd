@echo off
setlocal EnableExtensions DisableDelayedExpansion
rem Frame-rate estimate without opening the game: replays the work a run's log
rem counted through this build's code and compares with that run's frame rate.
rem Usage: Benchmark-VIctory.cmd [path\to\play-*.log]   (default: newest run with gameplay)
set "runner=%~dp0target\release\victory.exe"
if not exist "%runner%" (
  echo Build first: cargo build --release --locked -p victory 1>&2
  exit /b 2
)
"%runner%" bench %*
exit /b %errorlevel%
