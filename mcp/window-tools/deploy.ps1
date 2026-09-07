param(
    [string]$Deployed = 'C:\Program Files\ClaudeCodeBridge\claude-mcp-window-tools.exe',
    [switch]$SkipBuild
)
# 部署流程:构建 release → 冒烟验证 → 杀运行中的 MCP 进程 → 备份 → 覆盖 → 校验。
# Claude Code 会在下一次工具调用时自动重连 MCP,新二进制即刻生效(无需重启会话)。
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$crate = Join-Path $repoRoot 'mcp\window-tools'
$built = Join-Path $crate 'target\x86_64-pc-windows-msvc\release\mcp-window-tools.exe'

if (-not $SkipBuild) {
    Write-Output "[1/4] cargo build --release"
    & cargo build --release --manifest-path (Join-Path $crate 'Cargo.toml')
    if ($LASTEXITCODE -ne 0) { throw "构建失败" }
}

Write-Output "[2/4] 冒烟验证(initialize + list_windows)"
$smoke = @'
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"deploy","version":"1"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_windows","arguments":{"limit":1}}}
'@
$out = $smoke | & $built
if ($LASTEXITCODE -ne 0) { throw "冒烟验证失败" }
$init = $out | Select-Object -First 1 | ConvertFrom-Json
$version = $init.result.serverInfo.version
Write-Output ("    server: " + $init.result.serverInfo.name + " v" + $version)

Write-Output "[3/4] 停止运行中的 MCP 进程"
$procs = Get-Process claude-mcp-window-tools -ErrorAction SilentlyContinue
foreach ($p in $procs) {
    Stop-Process -Id $p.Id -Force
    Write-Output ("    killed PID " + $p.Id)
}
Start-Sleep -Milliseconds 500

Write-Output "[4/4] 备份并覆盖部署"
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$backup = $Deployed + ".backup-" + $stamp
if (Test-Path $Deployed) { Copy-Item $Deployed $backup -Force }
Copy-Item $built $Deployed -Force
$d = Get-Item $Deployed; $b = Get-Item $built
Write-Output ("    backup: " + (Split-Path $backup -Leaf))
Write-Output ("    deployed: " + $d.Length + " bytes, " + $d.LastWriteTime + ", size-match=" + ($d.Length -eq $b.Length))
Write-Output ("完成。Claude Code 下次工具调用会自动重连到新版本 v" + $version + "。")
