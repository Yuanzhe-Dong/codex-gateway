# 安装 / 卸载 codex-gateway 的开始菜单快捷方式
# 用法:
#   powershell -ExecutionPolicy Bypass -File "安装到开始菜单.ps1"          # 安装
#   powershell -ExecutionPolicy Bypass -File "安装到开始菜单.ps1" -Remove  # 卸载快捷方式
param([switch]$Remove)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$bat  = Join-Path $root 'codex-gateway 控制台.bat'
$ico  = Join-Path $root 'assets\gateway.ico'
$menu = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Codex 网关'

$ws = New-Object -ComObject WScript.Shell

function New-GatewayLink {
  param(
    [string]$Name,
    [string]$Arg,
    [int]$Style = 1
  )
  $lnk = $ws.CreateShortcut((Join-Path $menu "$Name.lnk"))
  $lnk.TargetPath = $bat
  if ($Arg) { $lnk.Arguments = $Arg }
  $lnk.WorkingDirectory = $root
  $lnk.WindowStyle = $Style   # 1=普通 7=最小化
  $lnk.IconLocation = "$ico,0"
  $lnk.Description = "codex-gateway：Codex 无缝模型切换网关"
  $lnk.Save()
  Write-Host "  ✔ $Name.lnk"
}

if ($Remove) {
  if (Test-Path $menu) {
    Remove-Item $menu -Recurse -Force
    Write-Host "已删除快捷方式文件夹: $menu"
  } else {
    Write-Host "快捷方式文件夹不存在（无需卸载）: $menu"
  }
  exit 0
}

if (-not (Test-Path $bat))   { throw "未找到控制台脚本: $bat" }
if (-not (Test-Path $ico))   { throw "未找到图标: $ico" }

New-Item -ItemType Directory -Force -Path $menu | Out-Null
Write-Host "正在创建开始菜单快捷方式 → $menu"

New-GatewayLink -Name 'Codex 网关控制台' -Style 1
New-GatewayLink -Name '启动网关'        -Arg 'start'  -Style 7
New-GatewayLink -Name '停止网关'        -Arg 'stop'   -Style 7
New-GatewayLink -Name '查看状态'        -Arg 'status' -Style 7
New-GatewayLink -Name '配置向导'        -Arg 'setup'  -Style 1
New-GatewayLink -Name '查看日志'        -Arg 'log'    -Style 7

Write-Host ""
Write-Host "完成！开始菜单 → 所有应用 → Codex 网关 可找到全部快捷方式。"
Write-Host "（若未立即出现，重启资源管理器或稍等几秒）"
