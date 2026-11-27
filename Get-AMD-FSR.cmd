@echo off
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\get-amd-fsr.ps1" %*
if errorlevel 1 pause
