param([string]$Exe = '', [string]$OutFile = '')
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$exe = if ($Exe) { $Exe } else { 'D:\dev\apps\codex-gemini-bridge\mcp\window-tools\target\x86_64-pc-windows-msvc\release\mcp-window-tools.exe' }

# 本进程(将作为 MCP 服务的父进程,子进程继承完整性级别)是否提权
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class TokQ {
  [DllImport("advapi32.dll", SetLastError=true)]
  public static extern bool OpenProcessToken(IntPtr h, uint acc, out IntPtr t);
  [DllImport("advapi32.dll")]
  public static extern bool GetTokenInformation(IntPtr t, int cls, out uint info, uint len, out uint ret);
  [DllImport("kernel32.dll")]
  public static extern bool CloseHandle(IntPtr h);
  public static bool SelfElevated() {
    IntPtr tok; uint info = 0, ret = 0;
    if (!OpenProcessToken(System.Diagnostics.Process.GetCurrentProcess().Handle, 0x0008, out tok)) return false;
    GetTokenInformation(tok, 20, out info, 4, out ret);
    CloseHandle(tok);
    return info != 0;
  }
}
'@
$selfElevated = [TokQ]::SelfElevated()

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $exe
$psi.UseShellExecute = $false
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
$psi.CreateNoWindow = $true
$proc = [System.Diagnostics.Process]::Start($psi)
$utf8 = New-Object System.Text.UTF8Encoding($false)
$stdin = $proc.StandardInput.BaseStream
$stdout = $proc.StandardOutput
$id = 0
function Send-Req($method, $prms) {
  $script:id++
  $req = @{ jsonrpc='2.0'; id=$script:id; method=$method }
  if ($null -ne $prms) { $req.params = $prms }
  $json = $req | ConvertTo-Json -Compress -Depth 30
  $bytes = $utf8.GetBytes($json + "`n")
  $stdin.Write($bytes, 0, $bytes.Length)
  $stdin.Flush()
  $line = $stdout.ReadLine()
  if ($null -eq $line) { throw "server closed" }
  return ($line | ConvertFrom-Json)
}
function Send-Tool($name, $argz) { return (Send-Req 'tools/call' @{ name=$name; arguments=$argz }) }

try {
  $null = Send-Req 'initialize' @{ protocolVersion='2025-11-25'; capabilities=@{}; clientInfo=@{ name='wt-uipi-tester'; version='1.0' } }
  $lw = Send-Tool 'list_windows' @{ interactive_only=$false }
  $winList = $lw.result.content[0].text | ConvertFrom-Json
  $target = $winList.windows | Where-Object { $_.elevated } | Select-Object -First 1
  if ($null -eq $target) {
    Write-Output 'SKIP: no elevated window on desktop'
  }
  else {
    $si = Send-Tool 'send_input' @{ hwnd=[string]$target.hwnd; actions=@(@{ kind='key_tap'; vkey='A' }) }
    $wa = Send-Tool 'window_activate' @{ hwnd=[string]$target.hwnd }
    $siText = $si.result.content[0].text
    $waText = $wa.result.content[0].text
    $result = [PSCustomObject]@{
      selfElevated = $selfElevated
      targetHwnd   = $target.hwnd
      targetPid    = $target.pid
      elevatedFlag = $target.elevated
      sendInputErrorCode = if ($si.result.isError) { $si.result.error_code } else { $null }
      activateErrorCode  = if ($wa.result.isError) { $wa.result.error_code } else { $null }
      sendInputText = $siText
      activateText  = $waText
    } | ConvertTo-Json -Compress
    if ($OutFile) { $result | Set-Content -Path $OutFile -Encoding UTF8 }
    Write-Output $result
  }
}
finally {
  if (-not $proc.HasExited) { $proc.Kill() }
  $proc.WaitForExit(3000) | Out-Null
}
