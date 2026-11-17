# Regression checks for the console. UAC is mocked; no real gateway is controlled.
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$console = Get-ChildItem -LiteralPath $root -Filter 'codex-gateway *.ps1' | Select-Object -First 1
$tokens = $null
$parseErrors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($console.FullName, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw 'Console parse failed' }
foreach ($name in @('ConvertTo-PowerShellLiteral', 'Invoke-Gateway', 'Resolve-GatewayExecutable')) {
  $functionAst = $ast.Find({ param($node)
    $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name
  }, $true)
  . ([scriptblock]::Create($functionAst.Extent.Text))
}

function Assert-Console($condition, [string]$message) {
  if (-not $condition) { throw $message }
}
function Start-Process {
  param($FilePath, [string[]]$ArgumentList, $Verb, $WindowStyle, [switch]$Wait, [switch]$PassThru)
  $script:elevationCalls++
  Assert-Console ($Verb -eq 'RunAs' -and $Wait -and $PassThru) 'Invalid elevation invocation'
  Assert-Console ($ArgumentList[-2] -eq '-EncodedCommand') 'Missing encoded command'
  $script:decoded = [Text.Encoding]::Unicode.GetString([Convert]::FromBase64String($ArgumentList[-1]))
  if ($script:cancelElevation) { throw 'UAC canceled' }
  return [pscustomobject]@{ ExitCode = $script:elevatedExit }
}

$testDir = Join-Path $root ('.codex/console-tests/' + [guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($testDir) | Out-Null
$exe = Join-Path $testDir 'mock-gateway.cmd'
[IO.File]::WriteAllText($exe, "@echo off`r`nexit /b %GATEWAY_CONSOLE_TEST_EXIT%`r`n", [Text.Encoding]::ASCII)
$gwDir = "C:\gateway with spaces\O'Brien"
$codexDir = "C:\codex with spaces\O'Brien"
$previousTestExit = $env:GATEWAY_CONSOLE_TEST_EXIT
$elevationCalls = 0
$cancelElevation = $false
$elevatedExit = 0
$ElevationAttempt = $false
try {
  $oldExe = Join-Path $testDir 'old.cmd'
  $newExe = Join-Path $testDir 'new.cmd'
  [IO.File]::WriteAllText($oldExe, "@echo codex-gateway 0.3.4`r`n", [Text.Encoding]::ASCII)
  [IO.File]::WriteAllText($newExe, "@echo codex-gateway 0.3.5`r`n", [Text.Encoding]::ASCII)
  (Get-Item -LiteralPath $oldExe).LastWriteTimeUtc = [DateTime]::UtcNow.AddDays(2)
  Assert-Console ((Resolve-GatewayExecutable @($oldExe, $newExe)) -eq $newExe) 'Timestamp overrode executable version'
  [IO.File]::Delete($oldExe)
  [IO.File]::Delete($newExe)
  $env:GATEWAY_CONSOLE_TEST_EXIT = '0'
  Assert-Console ((Invoke-Gateway 'restart') -eq 0) 'Successful restart failed'
  Assert-Console ($elevationCalls -eq 0) 'Success requested elevation'
  $env:GATEWAY_CONSOLE_TEST_EXIT = '1'
  Assert-Console ((Invoke-Gateway 'restart') -eq 1) 'Generic error was swallowed'
  Assert-Console ($elevationCalls -eq 0) 'Generic error requested elevation'
  $env:GATEWAY_CONSOLE_TEST_EXIT = '5'
  Assert-Console ((Invoke-Gateway 'restart') -eq 0) 'Permission retry failed'
  Assert-Console ($elevationCalls -eq 1) 'Permission error did not retry once'
  $encodedTokens = $null
  $encodedErrors = $null
  $encodedAst = [Management.Automation.Language.Parser]::ParseInput($decoded, [ref]$encodedTokens, [ref]$encodedErrors)
  Assert-Console ($encodedErrors.Count -eq 0) 'Quoted paths broke the elevated command'
  $literals = @($encodedAst.FindAll({ param($node)
    $node -is [Management.Automation.Language.StringConstantExpressionAst]
  }, $true) | ForEach-Object { $_.Value })
  Assert-Console ($literals -contains $gwDir -and $literals -contains $codexDir) 'Data paths changed'
  Assert-Console ($decoded.Contains('-ElevationAttempt')) 'Missing retry-loop guard'
  $ElevationAttempt = $true
  Assert-Console ((Invoke-Gateway 'restart') -eq 5) 'Elevated failure was swallowed'
  Assert-Console ($elevationCalls -eq 1) 'Elevation loop detected'
  $ElevationAttempt = $false
  Assert-Console ((Invoke-Gateway 'status') -eq 5) 'Status error was swallowed'
  Assert-Console ($elevationCalls -eq 1) 'Read-only status requested elevation'
  $elevatedExit = 1
  Assert-Console ((Invoke-Gateway 'restart') -eq 1) 'Elevated error was swallowed'
  $cancelElevation = $true
  Assert-Console ((Invoke-Gateway 'restart') -eq 5) 'Canceled UAC reported success'
  Write-Host 'Console regression checks passed (no actual elevation or gateway operations).'
} finally {
  $env:GATEWAY_CONSOLE_TEST_EXIT = $previousTestExit
  [IO.File]::Delete($exe)
  [IO.Directory]::Delete($testDir)
}
