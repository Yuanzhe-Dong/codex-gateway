@echo off
setlocal
set "GATEWAY_CONSOLE="
for %%F in ("%~dp0codex-gateway *.ps1") do set "GATEWAY_CONSOLE=%%~fF"
if not defined GATEWAY_CONSOLE (
  echo Gateway console script was not found.
  exit /b 1
)
"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "%GATEWAY_CONSOLE%" %*
set "GATEWAY_EXIT=%ERRORLEVEL%"
if not "%GATEWAY_EXIT%"=="0" (
  echo Gateway operation failed with exit code %GATEWAY_EXIT%.
  if "%~1"=="" pause
)
exit /b %GATEWAY_EXIT%
