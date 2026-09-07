param([string]$Exe = '')
# 一次性运行全部 E2E 回归:主套件(协议/工具/副作用/GDI 泄漏)+ 挂起窗口活性套件。
# 用法: pwsh -File tests/run-e2e.ps1 [-Exe <路径>]
$ErrorActionPreference = 'Stop'
$dir = Split-Path -Parent $MyInvocation.MyCommand.Path
$mainArgs = @()
$hungArgs = @()
if ($Exe) { $mainArgs += '-Exe', $Exe; $hungArgs += '-Exe', $Exe }

Write-Output "===== E2E main suite ====="
& (Join-Path $dir 'e2e-main.ps1') @mainArgs
$mainCode = $LASTEXITCODE

Write-Output ""
Write-Output "===== E2E hung-window suite ====="
& (Join-Path $dir 'e2e-hung.ps1') @hungArgs
$hungCode = $LASTEXITCODE

if ($mainCode -ne 0 -or $hungCode -ne 0) {
    Write-Output ("OVERALL: FAILED (main=" + $mainCode + " hung=" + $hungCode + ")")
    exit 1
}
Write-Output "OVERALL: all suites passed"
exit 0
