@echo off
rem B-stage TAP DLL build (Git Bash: cmd //c build_tap.cmd). ASCII only.
rem Increment TAPVER to force explorer to load a fresh copy (old module stays resident until explorer restarts).
setlocal
if "%TAPVER%"=="" set TAPVER=16
set MSVC=D:\environment\VsBuildTools\VC\Tools\MSVC\14.44.35207
set KIT=D:\environment\WindowsKits\10
set INCLUDE=%MSVC%\include;%KIT%\Include\10.0.26100.0\um;%KIT%\Include\10.0.26100.0\shared;%KIT%\Include\10.0.26100.0\ucrt;%KIT%\Include\10.0.26100.0\winrt;%KIT%\Include\10.0.26100.0\cppwinrt
set LIB=%MSVC%\lib\x64;%KIT%\Lib\10.0.26100.0\um\x64;%KIT%\Lib\10.0.26100.0\ucrt\x64
set PATH=%MSVC%\bin\Hostx64\x64;%PATH%

cd /d %~dp0
cl /nologo /LD /MT /EHsc /O2 /W3 /std:c++17 /utf-8 /DTAPVER=%TAPVER% tap.cpp /Fe:..\bin\lical_clock_tap%TAPVER%.dll /link /nologo /DLL /DEF:tap.def User32.lib ole32.lib OleAut32.lib Runtimeobject.lib
if errorlevel 1 exit /b 1
del /q ..\bin\lical_clock_tap%TAPVER%.exp ..\bin\lical_clock_tap%TAPVER%.lib tap.obj 2>nul
exit /b 0
