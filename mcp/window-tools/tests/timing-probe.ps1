param([string]$Exe = '')
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$exe = if ($Exe) { $Exe } else { 'D:\dev\apps\codex-gemini-bridge\mcp\window-tools\target\x86_64-pc-windows-msvc\release\mcp-window-tools.exe' }
$logFile = Join-Path $env:TEMP 'wt-timing.log'
if (Test-Path $logFile) { Remove-Item $logFile -Force }
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $exe
$psi.UseShellExecute = $false
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
$psi.CreateNoWindow = $true
$psi.EnvironmentVariables['WT_LOG_LEVEL'] = 'info'
$psi.EnvironmentVariables['WT_LOG_FILE'] = $logFile
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
function Send-Tool($name, $argz) { Send-Req 'tools/call' @{ name=$name; arguments=$argz } }

# 确定性探针窗口(与 e2e-main 同源):独立进程 + STA + Application.Run
$probeScript = @'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -TypeDefinition @"
using System;
using System.Windows.Forms;
using System.Drawing;
public static class TimingProbe {
    public static void Start() {
        var t = new System.Threading.Thread(() => {
            var f = new Form();
            f.Text = "TIMING_PROBE";
            f.TopMost = true;
            f.ClientSize = new Size(900, 600);
            var b = new Button();
            b.Text = "TIMING_BTN";
            b.Size = new Size(200, 60);
            b.Location = new Point(350, 270);
            f.Controls.Add(b);
            f.Show();
            Console.WriteLine(f.Handle.ToInt64());
            Console.Out.Flush();
            Application.Run(f);
        });
        t.IsBackground = true;
        t.SetApartmentState(System.Threading.ApartmentState.STA);
        t.Start();
    }
}
"@ -ReferencedAssemblies 'System.Windows.Forms','System.Drawing'
[TimingProbe]::Start()
while ($true) { Start-Sleep -Seconds 60 }
'@
$probeFile = Join-Path $env:TEMP 'wt-timing-probe.ps1'
Set-Content -Path $probeFile -Value $probeScript -Encoding UTF8
$probeOut = Join-Path $env:TEMP 'wt-timing-probe-out.txt'
$probeProc = Start-Process powershell -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File',$probeFile -PassThru -RedirectStandardOutput $probeOut
Start-Sleep -Milliseconds 2500

try {
  $null = Send-Req 'initialize' @{ protocolVersion='2025-11-25'; capabilities=@{}; clientInfo=@{ name='wt-timing'; version='1.0' } }
  $lw = Send-Tool 'list_windows' @{ pid=$probeProc.Id; title='TIMING_PROBE' }
  $fh = [string]$lw.result.content[0].text | Out-Null
  # list_windows 返回 content[0].text 是 JSON 文本,直接取 windows[0].hwnd
  $payload = $lw.result.content[0].text | ConvertFrom-Json
  $fh = [string]$payload.windows[0].hwnd
  Write-Output ("probe hwnd=" + $fh)
  for ($i = 1; $i -le 3; $i++) {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $r = Send-Tool 'screenshot_window' @{ hwnd=$fh; client_only=$false; method='wgc' }
    $sw.Stop()
    if ($r.result.isError) { Write-Output ("wgc call $i ERROR: " + $r.result.content[0].text) }
    else { Write-Output ("wgc call $i wall=" + $sw.Elapsed.TotalMilliseconds + "ms b64len=" + $r.result.content[1].data.Length) }
  }
  for ($i = 1; $i -le 3; $i++) {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $r = Send-Tool 'screenshot_window' @{ hwnd=$fh; client_only=$false; method='wgc'; incremental=$true }
    $sw.Stop()
    if ($r.result.isError) { Write-Output ("inc call $i ERROR: " + $r.result.content[0].text) }
    else {
      $imgs = ($r.result.content | Where-Object { $_.type -eq 'image' }).Count
      Write-Output ("inc call $i wall=" + $sw.Elapsed.TotalMilliseconds + "ms images=" + $imgs + " text=" + $r.result.content[0].text)
    }
  }
} finally {
  if (!$proc.HasExited) { $proc.Kill(); $null = $proc.WaitForExit(2000) }
  if ($null -ne $probeProc -and !$probeProc.HasExited) { Stop-Process -Id $probeProc.Id -Force -ErrorAction SilentlyContinue }
  Remove-Item $probeFile, $probeOut -Force -ErrorAction SilentlyContinue
}
Write-Output "--- stage log ---"
Get-Content $logFile | Where-Object { $_ -match 'wgc-stage|首帧到达|D3D11 设备' }
