# codex-gateway 控制台（PowerShell 版，由同目录 bat 启动）
$ErrorActionPreference = 'Continue'

# 统一控制台为 UTF-8：exe 输出的是 UTF-8 字节，必须 65001 才不乱码
cmd /c "chcp 65001 >nul" | Out-Null
try { [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 } catch {}
try { [Console]::InputEncoding = [System.Text.Encoding]::UTF8 } catch {}
try { $Host.UI.RawUI.WindowTitle = 'Codex 网关控制台' } catch {}

$root  = Split-Path -Parent $MyInvocation.MyCommand.Path
$exe   = Join-Path $root 'target\release\codex-gateway.exe'
$gwDir = Join-Path $env:USERPROFILE '.codex-gateway'
$log   = Join-Path $gwDir 'gateway.log'

function Invoke-Gateway([string]$arg) {
  if (-not (Test-Path -LiteralPath $exe)) {
    Write-Host ''
    Write-Host "未找到网关程序: $exe"
    Write-Host '请先在项目目录编译: cargo build --release'
    return
  }
  & $exe $arg
}

function Confirm-Uninstall {
  Write-Host '============================================='
  Write-Host '  警告：将彻底卸载 codex-gateway'
  Write-Host '   - 停止代理并还原 Codex 原生配置'
  Write-Host "   - 删除网关数据目录: $gwDir"
  Write-Host '   - 不会删除 ~/.codex 下的会话历史'
  Write-Host '============================================='
  $conf = Read-Host '确认卸载？输入 yes 后回车'
  if ($conf -eq 'yes') { Invoke-Gateway 'uninstall' } else { Write-Host '已取消。' }
}

# ---------- 参数模式（供开始菜单快捷方式调用） ----------
$arg = $args[0]
if ($arg) {
  switch ($arg.ToLower()) {
    'start'     { Invoke-Gateway 'start';     exit }
    'stop'      { Invoke-Gateway 'stop';      exit }
    'status'    { Invoke-Gateway 'status';    exit }
    'setup'     { Invoke-Gateway 'setup';     exit }
    'uninstall' { Confirm-Uninstall;          exit }
    'log' {
      if (Test-Path -LiteralPath $log) { notepad $log } else { Write-Host "日志文件不存在: $log" }
      exit
    }
  }
}

# ---------- 菜单模式 ----------
while ($true) {
  try { Clear-Host } catch {}
  Write-Host '============================================='
  Write-Host '            Codex 网关控制台'
  Write-Host '============================================='
  if (Test-Path -LiteralPath $exe) {
    Write-Host "  [程序] 已找到: $exe"
  } else {
    Write-Host '  [程序] 未找到，请先在项目目录编译:'
    Write-Host '         cargo build --release'
  }
  Write-Host '---------------------------------------------'
  Write-Host '  1. 启动网关        （后台运行，Codex 走 DeepSeek）'
  Write-Host '  2. 停止网关        （切回原生 GPT，会话不丢）'
  Write-Host '  3. 查看状态        （进程 / 配置 / 上游连通性）'
  Write-Host '  4. 配置向导        （setup：API Key、模型、端口）'
  Write-Host '  5. 查看日志        （用记事本打开 gateway.log）'
  Write-Host '  6. 彻底卸载        （还原原生配置并删除网关数据）'
  Write-Host '  0. 退出'
  Write-Host '---------------------------------------------'
  $opt = Read-Host '请输入序号后回车'
  switch ($opt) {
    '1' { Invoke-Gateway 'start' }
    '2' { Invoke-Gateway 'stop' }
    '3' { Invoke-Gateway 'status' }
    '4' { Invoke-Gateway 'setup' }
    '5' {
      if (Test-Path -LiteralPath $log) { notepad $log }
      else { Write-Host "日志文件不存在: $log（网关可能尚未启动过）" }
    }
    '6' { Confirm-Uninstall }
    '0' { exit }
    default { Write-Host '无效输入，请重新选择。' }
  }
  Write-Host ''
  Read-Host '按回车返回菜单'
}
