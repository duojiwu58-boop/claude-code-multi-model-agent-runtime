param([string]$Exe = '')
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$exe = if ($Exe) { $Exe } else { 'D:\dev\apps\codex-gemini-bridge\mcp\window-tools\target\x86_64-pc-windows-msvc\release\mcp-window-tools.exe' }

# 1) 启动一个真·挂起窗口进程:独立 STA 线程创建窗口后 busy-sleep,从不取消息
$hangScript = @'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -TypeDefinition @"
using System;
using System.Windows.Forms;
public static class HungWindow {
    public static void Start() {
        var t = new System.Threading.Thread(() => {
            var f = new Form();
            f.Text = "HUNG_WINDOW_PROBE";
            f.Width = 400; f.Height = 300;
            f.Show();
            Console.WriteLine(f.Handle.ToInt64());
            Console.Out.Flush();
            while (true) { System.Threading.Thread.Sleep(60000); }
        });
        t.IsBackground = true;
        t.SetApartmentState(System.Threading.ApartmentState.STA);
        t.Start();
    }
}
"@ -ReferencedAssemblies System.Windows.Forms
[HungWindow]::Start()
while ($true) { Start-Sleep -Seconds 60 }
'@
$hangFile = Join-Path $env:TEMP 'wt-hang-window.ps1'
Set-Content -Path $hangFile -Value $hangScript -Encoding UTF8
$hangProc = Start-Process powershell -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File',$hangFile -PassThru -RedirectStandardOutput (Join-Path $env:TEMP 'wt-hang-out.txt')
Start-Sleep -Milliseconds 2000
$hwnd = (Get-Content (Join-Path $env:TEMP 'wt-hang-out.txt') | Select-Object -First 1).Trim()
Write-Output ("hung window pid=" + $hangProc.Id + " hwnd=" + $hwnd)
# hung 判定需要窗口至少 ~5 秒无响应
Start-Sleep -Milliseconds 5000

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
$pass = 0; $fail = 0
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
function Check($name, $cond, $detail) {
  if ($cond) { $script:pass++; Write-Output ("PASS  " + $name + "  " + $detail) }
  else { $script:fail++; Write-Output ("FAIL  " + $name + "  " + $detail) }
}
try {
  $null = Send-Req 'initialize' @{ protocolVersion='2025-11-25'; capabilities=@{}; clientInfo=@{ name='hung-tester'; version='1.0' } }

  # 诊断:IsHungAppWindow 对制造器窗口的判定(必须先为 True,后续断言才有意义)
  Add-Type -Namespace W -Name H -MemberDefinition '[DllImport("user32.dll")] public static extern bool IsHungAppWindow(System.IntPtr hwnd);'
  $hungFlag = [W.H]::IsHungAppWindow([IntPtr]([int64]$hwnd))
  Write-Output ("IsHungAppWindow(hwnd) = " + $hungFlag)
  Check 'hang-window maker really hangs' $hungFlag "IsHungAppWindow=true"

  # 2) 枚举不卡:挂起窗口在桌面时 list_windows 必须限时完成
  # (挂起窗口标题不可读,会被默认 interactive_only 启发式过滤;此处显式关掉以断言枚举本身)
  $sw = [System.Diagnostics.Stopwatch]::StartNew()
  $lw = Send-Tool 'list_windows' @{ pid=$hangProc.Id; interactive_only=$false }
  $sw.Stop()
  $lp = $lw.result.content[0].text | ConvertFrom-Json
  $found = $lp.count -ge 1
  Check 'list_windows with hung window stays fast' ($found -and $sw.ElapsedMilliseconds -lt 2000) ("count=" + $lp.count + " in " + $sw.ElapsedMilliseconds + "ms")

  # 3) 挂起窗口截图 → 结构化 WINDOW_HUNG,快速返回
  $sw2 = [System.Diagnostics.Stopwatch]::StartNew()
  $sw_resp = Send-Tool 'screenshot_window' @{ hwnd=$hwnd; client_only=$false }
  $sw2.Stop()
  $r = $sw_resp.result
  Check 'screenshot of hung window -> WINDOW_HUNG' (($r.isError -eq $true) -and ($r.error_code -eq 'WINDOW_HUNG') -and ($r.retryable -eq $true)) ("code=" + $r.error_code + " retryable=" + $r.retryable + " in " + $sw2.ElapsedMilliseconds + "ms: " + $r.content[0].text)
  Check 'hung screenshot returns fast' ($sw2.ElapsedMilliseconds -lt 2000) ($sw2.ElapsedMilliseconds.ToString() + "ms")

  # 4) 内核查询类工具对挂起窗口仍正常工作
  $gr = Send-Tool 'get_window_rect' @{ hwnd=$hwnd }
  $pg = $gr.result.content[0].text | ConvertFrom-Json
  Check 'get_window_rect on hung window works' (-not $gr.result.isError) ("outer=" + $pg.window_rect.width + "x" + $pg.window_rect.height)

  # 5) client_only 截图走 WM_PRINT(进入目标线程),挂起窗口必须被超时兜底
  $sw3 = [System.Diagnostics.Stopwatch]::StartNew()
  $sc = Send-Tool 'screenshot_window' @{ hwnd=$hwnd; client_only=$true }
  $sw3.Stop()
  $rc = $sc.result
  if ($rc.isError) {
    Check 'client_only capture of hung window bounded by timeout' (($rc.error_code -eq 'WINDOW_HUNG') -and ($sw3.ElapsedMilliseconds -le 20000)) ("code=" + $rc.error_code + " in " + $sw3.ElapsedMilliseconds + "ms")
  } else {
    # 目标线程恰好响应了 WM_PRINT(如 IsHungAppWindow=false 时),也接受:服务未被卡死
    Check 'client_only capture of hung window bounded by timeout' ($sw3.ElapsedMilliseconds -le 20000) ("completed in " + $sw3.ElapsedMilliseconds + "ms (window responded)")
  }

  # 6) 服务在挂起窗口请求后仍活着
  $ping = Send-Req 'ping' @{}
  Check 'server still alive after hung-window request' ($ping.result -ne $null) "ping ok"

  Write-Output ""
  Write-Output ("RESULT: " + $pass + " passed, " + $fail + " failed")
} finally {
  if (!$proc.HasExited) { $proc.Kill(); $null = $proc.WaitForExit(2000) }
  Stop-Process -Id $hangProc.Id -Force -ErrorAction SilentlyContinue
  Get-Process powershell -ErrorAction SilentlyContinue | Where-Object { $_.Id -eq $hangProc.Id } | Stop-Process -Force -ErrorAction SilentlyContinue
}
if ($fail -gt 0) { exit 1 } else { exit 0 }
