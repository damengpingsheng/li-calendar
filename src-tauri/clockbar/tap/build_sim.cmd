@echo off
setlocal
set MSVC=D:\environment\VsBuildTools\VC\Tools\MSVC\14.44.35207
set KIT=D:\environment\WindowsKits\10
set INCLUDE=%MSVC%\include;%KIT%\Include\10.0.26100.0\um;%KIT%\Include\10.0.26100.0\shared;%KIT%\Include\10.0.26100.0\ucrt;%KIT%\Include\10.0.26100.0\winrt;%KIT%\Include\10.0.26100.0\cppwinrt
set LIB=%MSVC%\lib\x64;%KIT%\Lib\10.0.26100.0\um\x64;%KIT%\Lib\10.0.26100.0\ucrt\x64
set PATH=%MSVC%\bin\Hostx64\x64;%PATH%
cl /nologo /MT /EHsc /O2 /std:c++17 /utf-8 sim.cpp /Fe:sim.exe
