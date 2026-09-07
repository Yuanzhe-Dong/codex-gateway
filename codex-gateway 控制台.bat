@echo off
setlocal
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0codex-gateway 控制台.ps1" %*
if errorlevel 1 (
  echo.
  echo [错误] 控制台脚本运行失败，错误码 %errorlevel%
  echo 请把窗口里的错误信息完整截图反馈。
  echo.
  pause
)
exit /b %errorlevel%
