# UTF-8 BOM 供 Windows PowerShell 5.1 正确读取中文。
param(
  [ValidateSet('', 'start', 'stop', 'restart', 'status', 'setup', 'log', 'uninstall', 'repair-sessions', 'refresh-models', 'test-deepseek')]
  [string]$Action = '',
  [switch]$KeepOpen,
  [switch]$ElevationAttempt
)
$ErrorActionPreference = 'Stop'
try { [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false) } catch {}
try { [Console]::InputEncoding = New-Object System.Text.UTF8Encoding($false) } catch {}
$OutputEncoding = [Console]::OutputEncoding
try { $Host.UI.RawUI.WindowTitle = 'Codex 网关控制台' } catch {}

$root = $PSScriptRoot
# 同步软件可能改变修改时间；按程序自身版本选择，避免新源码配上旧服务。
function Resolve-GatewayExecutable([string[]]$Candidates) {
  $available = foreach ($path in $Candidates) {
    if (Test-Path -LiteralPath $path -PathType Leaf) {
      $file = Get-Item -LiteralPath $path
      $versionText = (& $path --version | Out-String).Trim()
      $version = [version]'0.0.0'
      if ($versionText -match '^codex-gateway (\d+\.\d+\.\d+)') { $version = [version]$Matches[1] }
      [pscustomobject]@{ Path=$file.FullName; Version=$version; Modified=$file.LastWriteTimeUtc }
    }
  }
  $best = $available | Sort-Object -Property @{Expression='Version';Descending=$true}, @{Expression='Modified';Descending=$true} | Select-Object -First 1
  if ($best) { return $best.Path }
  return $Candidates[0]
}
$exe = Resolve-GatewayExecutable @((Join-Path $root 'codex-gateway.exe'), (Join-Path $root 'target\release\codex-gateway.exe'))
$gwDir = if ([string]::IsNullOrWhiteSpace($env:CODEX_GATEWAY_HOME)) {
  Join-Path $env:USERPROFILE '.codex-gateway'
} else { [IO.Path]::GetFullPath($env:CODEX_GATEWAY_HOME) }
$log = Join-Path $gwDir 'gateway.log'
$codexDir = if ([string]::IsNullOrWhiteSpace($env:CODEX_HOME)) {
  Join-Path $env:USERPROFILE '.codex'
} else { [IO.Path]::GetFullPath($env:CODEX_HOME) }
$gatewayVersion = if (Test-Path -LiteralPath $exe -PathType Leaf) {
  (& $exe --version | Out-String).Trim()
} else { '未找到网关程序' }

function ConvertTo-PowerShellLiteral([string]$Value) {
  return "'" + $Value.Replace("'", "''") + "'"
}

function Invoke-Gateway([string]$Command) {
  if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
    Write-Host "未找到网关程序：$exe"
    Write-Host '源码使用者请先编译：cargo build --release --locked'
    return 1
  }
  & $exe $Command | Out-Host
  $code = [int]$LASTEXITCODE
  if ($code -ne 5 -or $ElevationAttempt -or $Command -notin @('start', 'stop', 'restart')) {
    return $code
  }
  Write-Host 'Windows 拒绝了网关控制权限；将通过系统权限提示重试当前操作。'
  Write-Host '完成后，在新窗口按回车返回此控制台。'
  # 用完整路径保留当前实例，单引号转义后编码，路径中的空格和引号不会变成命令。
  # 仅传数据目录路径，不传 API Key 或登录信息；已提权的重试不会再次提权。
  $elevatedCommand = @(
    ('$env:CODEX_GATEWAY_HOME = ' + (ConvertTo-PowerShellLiteral $gwDir)),
    ('$env:CODEX_HOME = ' + (ConvertTo-PowerShellLiteral $codexDir)),
    ('Set-Location -LiteralPath ' + (ConvertTo-PowerShellLiteral $root)),
    ('& ' + (ConvertTo-PowerShellLiteral $PSCommandPath) + ' -Action ' +
      (ConvertTo-PowerShellLiteral $Command) + ' -KeepOpen -ElevationAttempt')
  ) -join "`r`n"
  $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($elevatedCommand))
  try {
    $elevated = Start-Process -FilePath "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" `
      -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-EncodedCommand', $encoded) `
      -Verb RunAs -WindowStyle Normal -Wait -PassThru
    return [int]$elevated.ExitCode
  } catch {
    Write-Host "系统权限重试未完成：$($_.Exception.Message)"
    return 5
  }
}
function Invoke-Action([string]$Command) {
  if ($Command -eq 'log') {
    if (-not (Test-Path -LiteralPath $log -PathType Leaf)) {
      Write-Host "日志不存在：$log"
      return 1
    }
    & "$env:WINDIR\System32\notepad.exe" $log
    return 0
  }
  if ($Command -eq 'uninstall') {
    Write-Host "将停止当前实例、恢复直连配置并删除网关密钥和日志：$gwDir"
    Write-Host 'Codex 对话历史会保留。'
    if ((Read-Host '确认卸载？输入 yes 后回车') -ne 'yes') {
      Write-Host '已取消。'
      return 0
    }
  }
  return (Invoke-Gateway $Command)
}

if ($Action) {
  try { $code = Invoke-Action $Action } catch { Write-Host $_.Exception.Message; $code = 1 }
  if ($KeepOpen) { Read-Host '按回车关闭窗口' | Out-Null }
  exit $code
}
while ($true) {
  Clear-Host
  Write-Host '============================================='
  Write-Host '              Codex 网关控制台'
  Write-Host "              $gatewayVersion"
  Write-Host '       在 Codex 中切换官方订阅与 DeepSeek'
  Write-Host '============================================='
  Write-Host '  1. 启动网关'
  Write-Host '  2. 停止网关      （恢复 Codex 原有连接）'
  Write-Host '  3. 查看状态'
  Write-Host '  4. 配置向导      （首次使用；修改前先停止网关）'
  Write-Host '  5. 重启网关      （更换程序或网络设置后使用）'
  Write-Host '  6. 卸载网关      （保留 Codex 对话历史）'
  Write-Host '  0. 退出'
  Write-Host ''
  Write-Host '  首次使用：4 配置 → 1 启动 → 重新打开 Codex'
  Write-Host '  启动成功后可以关闭此窗口。'
  $opt = Read-Host '请输入序号后回车'
  if ($opt -eq '0') { exit 0 }
  $commands = @{ '1'='start'; '2'='stop'; '3'='status'; '4'='setup'; '5'='restart'; '6'='uninstall' }
  if ($commands.ContainsKey($opt)) {
    try {
      $code = Invoke-Action $commands[$opt]
      if ($code -ne 0) { Write-Host "操作失败（错误码 $code），请查看上述信息。" }
    } catch { Write-Host $_.Exception.Message }
  } else { Write-Host '无效输入。' }
  Read-Host '按回车返回菜单' | Out-Null
}
