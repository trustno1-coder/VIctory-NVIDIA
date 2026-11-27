@echo off
rem VIctory-NVIDIA-RT launcher. Pick resolution, DLSS, frame generation, HDR and more.
rem Put this file and victory.exe next to your own game_scarlett_master.exe. ASCII only.
rem   PLAY.cmd        menu (Enter on the first screen starts with your last choices)
rem   PLAY.cmd last   start with your last choices, no questions
setlocal EnableDelayedExpansion
set "VX=%~dp0victory.exe"
set "HOME_V=%LOCALAPPDATA%\victory"
set "LAST=%HOME_V%\launcher-last.cmd"
if not exist "%VX%" (
  echo victory.exe not found next to this file: %VX%
  pause & exit /b 1
)
if not exist "%~dp0game_scarlett_master.exe" (
  echo game_scarlett_master.exe not found next to this file.
  echo Put PLAY.cmd and victory.exe into the folder of your own dump of the game.
  pause & exit /b 1
)

rem ---- Windows 10: Microsoft's D3D12 Agility SDK ----
rem Windows 10's own D3D12 stops at shader model 6.5 and the game needs 6.6.
rem victory-win10.exe loads Microsoft's newer runtime from the D3D12 folder.
set "HERE=%~dp0"
set "WINBUILD=0"
for /f "tokens=2 delims=[]" %%V in ('ver') do for /f "tokens=4 delims=. " %%B in ("%%V") do set "WINBUILD=%%B"
if %WINBUILD% GTR 0 if %WINBUILD% LSS 22000 call :win10

rem ---- defaults (the recommended start) ----
set "C_CONSOLE=x"
set "C_RES=1440"
set "C_WIDE="
set "C_DLSS=quality"
set "C_FG=ready"
set "C_HDR=0"
set "C_FPS=1000"
set "C_FAST=1"
set "C_RT=0"
set "C_UI=band"
set "C_WIN=full"
set "C_REFLEX=boost"
if exist "%LAST%" call "%LAST%"

if /i "%~1"=="last" goto :go

cls
echo ============================================================
echo   VIctory-NVIDIA-RT
echo ============================================================
call :show
echo.
echo   Enter  start with these
echo   C      change them
echo.
set "A="
set /p A=Choice [Enter]:
if /i not "%A%"=="C" goto :go

rem ---- 1. resolution ----
cls
echo 1/10  Resolution (output)
echo   1. 2560x1440  16:9                        (recommended)
echo   2. 1920x1080 rendered, upscaled to 2560x1440
echo   3. 3840x2160  16:9  (4K)
echo   4. 5120x1440  32:9  super ultrawide
echo   5. 3440x1440  21:9  ultrawide
echo   6. other width, 1440 high (2560 to 7680, in steps of 8)
set "A="
set /p A=Choice [1]:
if "%A%"=="" set A=1
set "C_WIDE="
if "%A%"=="1" set "C_RES=1440"
if "%A%"=="2" set "C_RES=1080to1440"
if "%A%"=="3" set "C_RES=4k"
if "%A%"=="4" set "C_RES=wide"
if "%A%"=="5" set "C_RES=1440" & set "C_WIDE=3440x1440"
if "%A%"=="6" (
  set "C_RES=1440"
  set "C_W="
  set /p C_W=Width, for example 3840:
)
if "%A%"=="6" set "C_WIDE=!C_W!x1440"

rem ---- 2. window ----
cls
echo 2/10  Window
echo   1. Fullscreen, no border   (recommended; fills the whole screen)
echo   2. In a window
set "A="
set /p A=Choice [1]:
set "C_WIN=full"
if "%A%"=="2" set "C_WIN=window"

rem ---- 3. console profile ----
cls
echo 3/10  Console profile
echo   1. Series X   (default: the full-detail profile)
echo   2. Series S   (only for weaker cards: 1280x720 inside, much softer)
set "A="
set /p A=Choice [1]:
set "C_CONSOLE=x"
if "%A%"=="2" set "C_CONSOLE=s"

rem ---- 3. DLSS ----
cls
echo 4/10  DLSS upscaling
echo   1. Quality       (recommended)
echo   2. Balanced
echo   3. Performance
echo   4. DLAA          (native resolution, best looking, slowest)
echo   5. Off           (the game's own upscaling)
set "A="
set /p A=Choice [1]:
set "C_DLSS=quality"
if "%A%"=="2" set "C_DLSS=balanced"
if "%A%"=="3" set "C_DLSS=performance"
if "%A%"=="4" set "C_DLSS=dlaa"
if "%A%"=="5" set "C_DLSS=off"

rem ---- 4. frame generation ----
cls
echo 5/10  DLSS frame generation   (needs DLSS on; RTX 40 or later; 3x-6x need an RTX 50)
echo   1. Ready, off   (switch it on in the F1 menu while playing; recommended)
echo   2. 2x
echo   3. 3x
echo   4. 4x
echo   5. 5x
echo   6. 6x
echo   7. Not loaded at all   (a little more memory for textures)
set "A="
set /p A=Choice [1]:
set "C_FG=ready"
if "%A%"=="2" set "C_FG=2"
if "%A%"=="3" set "C_FG=3"
if "%A%"=="4" set "C_FG=4"
if "%A%"=="5" set "C_FG=5"
if "%A%"=="6" set "C_FG=6"
if "%A%"=="7" set "C_FG=none"

rem ---- reflex ----
cls
echo 6/10  NVIDIA Reflex   (lower input lag; always on with frame generation)
echo   1. On + boost   (recommended)
echo   2. On
echo   3. Off          (frame generation still turns it on)
set "A="
set /p A=Choice [1]:
set "C_REFLEX=boost"
if "%A%"=="2" set "C_REFLEX=on"
if "%A%"=="3" set "C_REFLEX=off"

rem ---- 5. HDR ----
cls
echo 7/10  HDR10   (turn Windows HDR on first)
echo   1. Off
echo   2. On
set "A="
set /p A=Choice [1]:
set "C_HDR=0"
if "%A%"=="2" set "C_HDR=1"

rem ---- 6. frame rate cap ----
cls
echo 8/10  Frame rate cap
echo   1. None        (recommended)
echo   2. 30   (the game's own rate)
echo   3. 60
echo   4. 120
echo   5. 144
echo   6. other
set "A="
set /p A=Choice [1]:
set "C_FPS=1000"
if "%A%"=="2" set "C_FPS=30"
if "%A%"=="3" set "C_FPS=60"
if "%A%"=="4" set "C_FPS=120"
if "%A%"=="5" set "C_FPS=144"
if "%A%"=="6" set /p C_FPS=Frames a second, 1 to 1000:

rem ---- 7. interface on wide screens ----
cls
echo 9/10  Menus and interface on ultrawide screens   (16:9 screens: no difference)
echo   1. Kept 16:9 in the middle, black bars   (recommended)
echo   2. Stretched over the whole width
set "A="
set /p A=Choice [1]:
set "C_UI=band"
if "%A%"=="2" set "C_UI=stretch"

rem ---- 8. speed and experiments ----
cls
echo 10/10  Speed settings
echo   1. Fast: pipelines from pieces, parallel submitting thread   (recommended)
echo   2. Plain: the official release's settings only
echo   3. Fast + the game's own ray tracing   (EXPERIMENTAL: works, but 5-8 fps)
set "A="
set /p A=Choice [1]:
set "C_FAST=1" & set "C_RT=0"
if "%A%"=="2" set "C_FAST=0"
if "%A%"=="3" set "C_RT=1"

rem ---- remember the choices ----
if not exist "%HOME_V%" mkdir "%HOME_V%" >nul 2>nul
(
  echo set "C_CONSOLE=%C_CONSOLE%"
  echo set "C_RES=%C_RES%"
  echo set "C_WIDE=%C_WIDE%"
  echo set "C_DLSS=%C_DLSS%"
  echo set "C_FG=%C_FG%"
  echo set "C_HDR=%C_HDR%"
  echo set "C_FPS=%C_FPS%"
  echo set "C_FAST=%C_FAST%"
  echo set "C_RT=%C_RT%"
  echo set "C_UI=%C_UI%"
  echo set "C_WIN=%C_WIN%"
  echo set "C_REFLEX=%C_REFLEX%"
) > "%LAST%" 2>nul

:go
rem ---- build the command ----
set "ARGS=--res %C_RES% --dlss %C_DLSS% --fps %C_FPS%"
if "%C_CONSOLE%"=="s" set "ARGS=!ARGS! --series-s"
if "%C_FG%"=="none" set "ARGS=!ARGS! --no-framegen-ready"
if not "%C_FG%"=="ready" if not "%C_FG%"=="none" set "ARGS=!ARGS! --framegen %C_FG%"
if not "%C_WIDE%"=="" set "ARGS=!ARGS! --set VICTORY_DISPLAY_WIDE=%C_WIDE%"
set "ARGS=!ARGS! --set VICTORY_HDR=%C_HDR%"
if "%C_UI%"=="stretch" set "ARGS=!ARGS! --set VICTORY_UI_LETTERBOX=0"
if not "%C_REFLEX%"=="off" set "ARGS=!ARGS! --set VICTORY_REFLEX=%C_REFLEX%"
if "%C_WIN%"=="full" (set "ARGS=!ARGS! --set VICTORY_FULLSCREEN=1") else (set "ARGS=!ARGS! --set VICTORY_FULLSCREEN=0")
if "%C_RT%"=="1" set "ARGS=!ARGS! --set VICTORY_RT_NATIVE=1"
rem the official release's loading fix, always
set "ARGS=!ARGS! --set VICTORY_PIPELINE_SAMPLER_TABLE=1"
if "%C_FAST%"=="1" call :fast

rem The F1 menu saves its own resolution, DLSS, frame generation and frame
rem rate choices, and those would win over this launcher's: drop them, keep
rem every other F1 choice.
set "SETS=%HOME_V%\victory-settings.txt"
if exist "%SETS%" (
  findstr /v /b /i /c:"VICTORY_MENU_RES=" /c:"VICTORY_MENU_DLSS=" /c:"VICTORY_MENU_FRAMEGEN=" /c:"VICTORY_MENU_FPS=" /c:"VICTORY_MENU_CONSOLE=" /c:"VICTORY_HDR=" /c:"VICTORY_DISPLAY_WIDE=" /c:"VICTORY_RT_NATIVE=" /c:"VICTORY_UI_LETTERBOX=" /c:"VICTORY_FULLSCREEN=" /c:"VICTORY_REFLEX=" "%SETS%" > "%SETS%.tmp" 2>nul
  if exist "%SETS%.tmp" move /y "%SETS%.tmp" "%SETS%" >nul
)

echo.
call :show
echo.
echo Starting...  F1 settings menu, F3 stats.
rem a crash leaves crash-<run>.log and crash-<run>.dmp next to this file
set "BEFORE="
for /f "delims=" %%F in ('dir /b /o-d "%~dp0crash-*.log" 2^>nul') do if not defined BEFORE set "BEFORE=%%F"
"%VX%" play !ARGS!
set "AFTER="
for /f "delims=" %%F in ('dir /b /o-d "%~dp0crash-*.log" 2^>nul') do if not defined AFTER set "AFTER=%%F"
if defined AFTER if not "!AFTER!"=="!BEFORE!" (
  echo.
  echo The game crashed. Send these two files from this folder:
  echo   !AFTER!
  echo   !AFTER:.log=.dmp!
  echo.
  pause
)
endlocal
exit /b 0

:fast
set "ARGS=!ARGS! --set VICTORY_PIPELINE_WAIT_ONCE=1 --set VICTORY_PIPELINE_WAIT_BACKLOG=64 --set VICTORY_PIPELINE_SLOW_WORKERS=8"
set "ARGS=!ARGS! --set VICTORY_TEXTURE_LOOK_ONCE=1 --set VICTORY_TEXTURE_READ_AHEAD=1 --set VICTORY_STAGING_NO_ZERO=1"
set "ARGS=!ARGS! --set VICTORY_SEND_COPIED_FIND=1 --set VICTORY_FILES_BUFFERED=1"
set "ARGS=!ARGS! --set VICTORY_MEMORY_FRESH=1 --set VICTORY_MEMORY_ARENAS_AHEAD=1 --set VICTORY_MEMORY_QUIET=1"
set "ARGS=!ARGS! --set VICTORY_MEMORY_LAND_SENT=1 --set VICTORY_MEMORY_ASK_RUNS=1 --set VICTORY_MEMORY_LEAN=1"
set "ARGS=!ARGS! --set VICTORY_MEMORY_DIRTY=1 --set VICTORY_MEMORY_BUFFER_FAULTS=1 --set VICTORY_MEMORY_FAULT_PAGES=40000"
set "ARGS=!ARGS! --set VICTORY_REFRESH_PROBE_US=300 --set VICTORY_DECLARE_ACROSS_LISTS=1 --set VICTORY_REFRESH_PROBE_TEXTURES=1"
set "ARGS=!ARGS! --set VICTORY_REFRESH_SPLIT=1 --set VICTORY_WAIT_SPLIT=1 --set VICTORY_DSTORAGE_STATS=1"
set "ARGS=!ARGS! --set VICTORY_PIPELINE_PIECES=1 --set VICTORY_PIPELINE_PIECES_REFUSE_UNTRANSLATED=1 --set VICTORY_PIPELINE_PIECES_ONLY=graphics"
set "ARGS=!ARGS! --set VICTORY_REFRESH_SET=1 --set VICTORY_TITLE_WRITE_RANGE=1 --set VICTORY_UNDER_PARTS=1 --set VICTORY_SEND_THREAD=1"
exit /b 0

:show
set "S_RES=%C_RES%"
if "%C_RES%"=="1440" set "S_RES=2560x1440"
if "%C_RES%"=="1080to1440" set "S_RES=1920x1080 upscaled to 2560x1440"
if "%C_RES%"=="4k" set "S_RES=3840x2160"
if "%C_RES%"=="wide" set "S_RES=5120x1440 (32:9)"
if not "%C_WIDE%"=="" set "S_RES=%C_WIDE%"
set "S_FG=%C_FG%x"
if "%C_FG%"=="ready" set "S_FG=ready, switch on in F1"
if "%C_FG%"=="none" set "S_FG=not loaded"
set "S_FPS=%C_FPS%"
if "%C_FPS%"=="1000" set "S_FPS=none"
set "S_REFLEX=on + boost"
if "%C_REFLEX%"=="on" set "S_REFLEX=on"
if "%C_REFLEX%"=="off" set "S_REFLEX=off (on with frame generation)"
set "S_HDR=off"
if "%C_HDR%"=="1" set "S_HDR=on"
set "S_SPEED=plain"
if "%C_FAST%"=="1" set "S_SPEED=fast"
if "%C_RT%"=="1" set "S_SPEED=fast + ray tracing (experimental)"
set "S_WIN=fullscreen, no border"
if "%C_WIN%"=="window" set "S_WIN=in a window"
set "S_UI=16:9 in the middle"
if "%C_UI%"=="stretch" set "S_UI=stretched"
echo   Resolution      !S_RES!
echo   Window          !S_WIN!
if /i "%C_CONSOLE%"=="s" (echo   Console         Series S) else (echo   Console         Series X)
echo   DLSS            %C_DLSS%
echo   Frame gen       !S_FG!
echo   Reflex          !S_REFLEX!
echo   HDR10           !S_HDR!
echo   Frame cap       !S_FPS!
echo   Wide interface  !S_UI!
echo   Speed           !S_SPEED!
exit /b 0

:win10
if not exist "%HERE%victory-win10.exe" (
  echo Windows 10: victory-win10.exe is missing next to this file; the game may not start.
  goto :eof
)
if not exist "%HERE%D3D12\D3D12Core.dll" (
  cls
  echo Windows 10 needs Microsoft's newer D3D12 runtime for this game
  echo ^(the D3D12 Agility SDK, about 35 MB^), from Microsoft's own NuGet:
  echo   https://www.nuget.org/packages/Microsoft.Direct3D.D3D12/1.620.2
  echo It goes into a D3D12 folder next to this file, once.
  set "A="
  set /p A=Download it now? [Y/n]:
  if /i "!A!"=="n" goto :eof
  set "PKG=%TEMP%\victory-d3d12"
  if exist "!PKG!" rd /s /q "!PKG!"
  mkdir "!PKG!"
  "%SystemRoot%\System32\curl.exe" -L --fail -o "!PKG!\d3d12.nupkg" "https://www.nuget.org/api/v2/package/Microsoft.Direct3D.D3D12/1.620.2"
  "%SystemRoot%\System32\tar.exe" -xf "!PKG!\d3d12.nupkg" -C "!PKG!" build/native/bin/x64/D3D12Core.dll
  rem only Microsoft's file goes in: its SHA-256 is checked first
  set "SUM="
  for /f "skip=1 delims=" %%H in ('certutil -hashfile "!PKG!\build\native\bin\x64\D3D12Core.dll" SHA256 2^>nul') do if not defined SUM set "SUM=%%H"
  set "SUM=!SUM: =!"
  if /i "!SUM!"=="70a6d47d335b4a15180544c01a35b84474522a369dc582d85fe057313a3aeaa0" (
    if not exist "%HERE%D3D12" mkdir "%HERE%D3D12"
    copy /y "!PKG!\build\native\bin\x64\D3D12Core.dll" "%HERE%D3D12\" >nul
    echo Done.
  ) else (
    echo The download did not work or is not Microsoft's file. Starting without it.
    pause
  )
  rd /s /q "!PKG!" 2>nul
)
if exist "%HERE%D3D12\D3D12Core.dll" set "VX=%HERE%victory-win10.exe"
goto :eof
