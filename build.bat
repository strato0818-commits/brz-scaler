@echo off
setlocal
cd /d "%~dp0"

cargo build --release
if errorlevel 1 (
    echo.
    echo Build failed.
    pause
    exit /b 1
)

echo.
echo Built target\release\brz-scaler-gui.exe
pause
