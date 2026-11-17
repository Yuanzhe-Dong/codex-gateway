# 为当前用户安装或移除快捷方式，无需管理员权限。
param([switch]$Remove)
$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
$console = Join-Path $root 'codex-gateway 控制台.ps1'
$ico = Join-Path $root 'assets\gateway.ico'
$menu = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Codex 网关'
$links = @(
  @('Codex 网关控制台', ''),
  @('启动网关', 'start'),
  @('停止网关', 'stop'),
  @('重启网关', 'restart')
)
# 旧版本创建过的入口只做迁移清理，不再展示给新用户。
$legacyLinkNames = @('更新并重启网关', '查看状态', '配置向导', '查看日志', '检查旧会话', '刷新模型目录', '测试 DeepSeek Flash')
$knownLinkNames = @($links | ForEach-Object { $_[0] }) + $legacyLinkNames
if ($Remove) {
  foreach ($name in $knownLinkNames) {
    $file = Join-Path $menu ($name + '.lnk')
    if (Test-Path -LiteralPath $file -PathType Leaf) { Remove-Item -LiteralPath $file -Force }
  }
  # 只删除本工具已知的快捷方式，不递归删除用户在目录中添加的文件。
  if ((Test-Path -LiteralPath $menu -PathType Container) -and
      -not (Get-ChildItem -LiteralPath $menu -Force | Select-Object -First 1)) {
    Remove-Item -LiteralPath $menu
  }
  Write-Host '网关快捷方式已移除。'
  exit 0
}
if (-not (Test-Path -LiteralPath $console -PathType Leaf)) { throw "未找到控制台：$console" }
if (-not (Test-Path -LiteralPath $ico -PathType Leaf)) { throw "未找到图标：$ico" }
foreach ($name in $legacyLinkNames) {
  $file = Join-Path $menu ($name + '.lnk')
  if (Test-Path -LiteralPath $file -PathType Leaf) { Remove-Item -LiteralPath $file -Force }
}
New-Item -ItemType Directory -Force -Path $menu | Out-Null
$ws = New-Object -ComObject WScript.Shell
foreach ($link in $links) {
  $shortcut = $ws.CreateShortcut((Join-Path $menu ($link[0] + '.lnk')))
  $shortcut.TargetPath = "$env:WINDIR\System32\WindowsPowerShell\v1.0\powershell.exe"
  $shortcut.Arguments = '-NoProfile -ExecutionPolicy Bypass -File "' + $console + '"'
  if ($link[1]) { $shortcut.Arguments += ' -Action ' + $link[1] + ' -KeepOpen' }
  $shortcut.WorkingDirectory = $root
  $shortcut.WindowStyle = 1
  $shortcut.IconLocation = "$ico,0"
  $shortcut.Description = 'codex-gateway：官方订阅与 DeepSeek 本地网关'
  $shortcut.Save()
  Write-Host ("已创建：" + $link[0])
}
[Runtime.InteropServices.Marshal]::FinalReleaseComObject($ws) | Out-Null
Write-Host '完成！开始菜单 → Codex 网关。'
