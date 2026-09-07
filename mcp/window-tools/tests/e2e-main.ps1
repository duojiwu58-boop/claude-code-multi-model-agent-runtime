param([string]$Exe = '')
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$exe = if ($Exe) { $Exe } else { 'D:\dev\apps\codex-gemini-bridge\mcp\window-tools\target\x86_64-pc-windows-msvc\release\mcp-window-tools.exe' }
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $exe
$psi.UseShellExecute = $false
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
$psi.CreateNoWindow = $true
$proc = [System.Diagnostics.Process]::Start($psi)
Add-Type -Namespace W -Name G -MemberDefinition '[DllImport("user32.dll")] public static extern uint GetGuiResources(System.IntPtr hProcess, uint uiFlags);'
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
function Send-Tool($name, $argz) {
  $r = Send-Req 'tools/call' @{ name=$name; arguments=$argz }
  return $r
}
function Tool-Payload($r) {
  # isError 时返回文本错误消息；否则解析 content[0].text 内层 JSON
  if ($r.result.isError) { return $null }
  if ($r.result.content.Count -eq 0) { return @{} }
  $t = $r.result.content[0].text
  if ($t -is [string] -and $t.StartsWith('{')) { return ($t | ConvertFrom-Json) }
  return @{ text = $t }
}
function Check($name, $cond, $detail) {
  if ($cond) { $script:pass++; Write-Output ("PASS  " + $name + "  " + $detail) }
  else { $script:fail++; Write-Output ("FAIL  " + $name + "  " + $detail) }
}
try {
  $init = Send-Req 'initialize' @{ protocolVersion='2025-11-25'; capabilities=@{}; clientInfo=@{ name='wt-tester'; version='1.0' } }
  Check 'initialize' ($init.result.protocolVersion -eq '2025-11-25') ($init.result.serverInfo.name + ' v' + $init.result.serverInfo.version)

  # --- list_windows ---
  $lw = Send-Tool 'list_windows' @{ limit=50 }
  $p = Tool-Payload $lw
  Check 'list_windows' ($p -and $p.count -gt 0) ("windows=" + $p.count)
  $lwAll = Send-Tool 'list_windows' @{ limit=50; interactive_only=$false }
  $pAll = Tool-Payload $lwAll
  Check 'list_windows interactive_only narrows default' ($pAll.count -ge $p.count) ("interactive=" + $p.count + " all-visible=" + $pAll.count)
  Write-Output ("      sample: class=" + $p.windows[0].class + " hwnd=" + $p.windows[0].hwnd + " visible=" + $p.windows[0].visible)
  $lwP = Send-Tool 'list_windows' @{ pid=[int]$p.windows[0].pid; visible_only=$true }
  Check 'list_windows pid-filter' ($null -ne (Tool-Payload $lwP)) ("matches=" + (Tool-Payload $lwP).count)
  $lwT = Send-Tool 'list_windows' @{ title='__no_such_window__zz' }
  Check 'list_windows title-filter empty' ((Tool-Payload $lwT).count -eq 0) "empty as expected"
  $lwL = Send-Tool 'list_windows' @{ limit=501 }
  $pl = Tool-Payload $lwL
  Check 'list_windows limit clamp 501->500' ((-not $lwL.result.isError) -and $pl.count -le 500) ("count=" + $pl.count)

  # --- get_window_rect ---
  $gr = Send-Tool 'get_window_rect' @{ hwnd='@active' }
  $pg = Tool-Payload $gr
  Check 'get_window_rect @active' ($pg -and $pg.window_rect -and $pg.client_rect) ("outer=" + $pg.window_rect.width + "x" + $pg.window_rect.height + " client=" + $pg.client_rect.width + "x" + $pg.client_rect.height + " title=" + $pg.title.Substring(0,[Math]::Min(20,$pg.title.Length)))
  $g1 = Send-Tool 'get_window_rect' @{}
  Check 'get_window_rect missing selector' ($g1.result.isError) $g1.result.content[0].text
  $g2 = Send-Tool 'get_window_rect' @{ hwnd='abc' }
  Check 'get_window_rect invalid hwnd' ($g2.result.isError) $g2.result.content[0].text
  $g3 = Send-Tool 'get_window_rect' @{ hwnd='1' }
  Check 'get_window_rect nonexistent hwnd' ($g3.result.isError) $g3.result.content[0].text

  # --- pixel tools on @active ---
  $scan = Send-Tool 'pixel_scan_line' @{ hwnd='@active'; x=100; y_from=0; y_to=200 }
  $ps = Tool-Payload $scan
  Check 'pixel_scan_line @active' ($ps -and $ps.band_count -gt 0 -and $ps.bands.Count -gt 0) ("bands=" + $ps.band_count + " band0=" + ($ps.bands[0] | ConvertTo-Json -Compress))
  $find = Send-Tool 'pixel_find_color' @{ hwnd='@active'; x=0; y=0; width=100; height=100; color='#000000'; tolerance=60 }
  $pf = Tool-Payload $find
  Check 'pixel_find_color @active' ($pf -and $pf.match_count -gt 0) ("matches=" + $pf.match_count + " bbox=" + ($pf.bbox | ConvertTo-Json -Compress))
  $findBad = Send-Tool 'pixel_find_color' @{ hwnd='@active'; x=0; y=0; width=10; height=10; color='#ZZZZZZ' }
  Check 'pixel_find_color bad color' ($findBad.result.isError) $findBad.result.content[0].text
  $scanBad = Send-Tool 'pixel_scan_line' @{ hwnd='@active'; x=100; y_from=500; y_to=100 }
  Check 'pixel_scan_line inverted range' ($scanBad.result.isError) $scanBad.result.content[0].text

  # --- winver scratch window ---
  $np = Start-Process winver -PassThru
  Start-Sleep -Milliseconds 1500
  $lwn = Send-Tool 'list_windows' @{ pid=$np.Id }
  $pn = Tool-Payload $lwn
  Check 'find winver window' ($pn -and $pn.count -gt 0) ("pid=" + $np.Id + " hwnd=" + $pn.windows[0].hwnd)
  $null = $np.Refresh()
  $titleFull = $np.MainWindowTitle
  Check 'list_windows full title' ($pn.windows[0].title -eq $titleFull -and $titleFull.Length -ge 2) ("title=" + $pn.windows[0].title + " expected=" + $titleFull)
  $nh = [string]$pn.windows[0].hwnd

  $rz = Send-Tool 'resize_window' @{ hwnd=$nh; width=500; height=400 }
  Check 'resize_window 500x400' (-not $rz.result.isError) "ok"
  Start-Sleep -Milliseconds 300
  $r1 = Send-Tool 'get_window_rect' @{ hwnd=$nh }
  $pr1 = Tool-Payload $r1
  Check 'rect after resize' ($pr1 -and $pr1.window_rect.width -eq 500 -and $pr1.window_rect.height -eq 400) ("outer=" + $pr1.window_rect.width + "x" + $pr1.window_rect.height + " client=" + $pr1.client_rect.width + "x" + $pr1.client_rect.height)
  $rzBad = Send-Tool 'resize_window' @{ hwnd='@active'; width=500; height=400 }
  $prz = Tool-Payload $rzBad
  Check 'resize_window accepts @active (foreground)' ((-not $rzBad.result.isError) -and $prz.hwnd -eq $nh) ("applied to " + $prz.hwnd)

  $wa = Send-Tool 'window_activate' @{ hwnd=$nh }
  Check 'window_activate winver' (-not $wa.result.isError) "ok"
  Start-Sleep -Milliseconds 400
  $si = Send-Tool 'send_input' @{ hwnd=$nh; activate=$false; actions=@(@{kind='key_tap'; vkey='a'}) }
  Check 'send_input foreground key_tap' (-not $si.result.isError) ($si.result.content[0].text)
  Start-Sleep -Milliseconds 200
  $siN = Send-Tool 'send_input' @{ hwnd='197480'; activate=$false; actions=@(@{kind='key_tap'; vkey='b'}) }
  Check 'send_input refuses non-foreground' ($siN.result.isError) $siN.result.content[0].text
  Start-Sleep -Milliseconds 200
  $siA = Send-Tool 'send_input' @{ hwnd=$nh; activate=$true; actions=@(@{kind='key_tap'; vkey='c'}) }
  Check 'send_input activate:true raises and injects' (-not $siA.result.isError) ($siA.result.content[0].text)

  $scanN = Send-Tool 'pixel_scan_line' @{ hwnd=$nh; x=250; y_from=0; y_to=300 }
  $psn = Tool-Payload $scanN
  Check 'pixel_scan_line winver in-range' ($psn -and $psn.band_count -gt 0) ("bands=" + $psn.band_count)
  $scanO = Send-Tool 'pixel_scan_line' @{ hwnd=$nh; x=250; y_from=0; y_to=400 }
  Check 'pixel_scan_line out-of-range error' ($scanO.result.isError) $scanO.result.content[0].text
  $hex = '0x' + [Convert]::ToString([int64]$nh, 16)
  $gh = Send-Tool 'get_window_rect' @{ hwnd=$hex }
  $pgh = Tool-Payload $gh
  Check 'get_window_rect 0x-hex hwnd' ($pgh -and $pgh.hwnd -eq $nh) ("hex=" + $hex + " -> " + $pgh.hwnd)
  $siU = Send-Tool 'send_input' @{ hwnd=$nh; activate=$false; actions=@(@{kind='key_down'; vkey='a'}) }
  Check 'send_input unbalanced batch rejected' ($siU.result.isError) $siU.result.content[0].text

  $sw = Send-Tool 'screenshot_window' @{ hwnd=$nh; client_only=$false }
  Check 'screenshot_window winver' ((-not $sw.result.isError) -and $sw.result.content.Count -ge 2 -and $sw.result.content[1].mimeType -eq 'image/png' -and $sw.result.content[1].data.StartsWith('iVBORw0KGgo')) ($sw.result.content[0].text + " b64len=" + $sw.result.content[1].data.Length)
  $swC = Send-Tool 'screenshot_window' @{ hwnd=$nh; client_only=$true }
  Check 'screenshot_window client_only' ((-not $swC.result.isError) -and $swC.result.content.Count -ge 2) ($swC.result.content[0].text + " b64len=" + $swC.result.content[1].data.Length)

  # --- screenshot_all ---
  $sa = Send-Tool 'screenshot_all' @{}
  $imgCount = 0; $txtLines = @()
  foreach ($c in $sa.result.content) { if ($c.type -eq 'image') { $imgCount++ } else { $txtLines += $c.text } }
  Check 'screenshot_all' ((-not $sa.result.isError) -and $imgCount -ge 1) ("images=" + $imgCount + " monitors-text: " + ($txtLines -join ' | ').Substring(0, [Math]::Min(160, ($txtLines -join ' | ').Length)))

  # --- GDI leak loop: 50x screenshot_window on winver ---
  $hp = Get-Process -Id $proc.Id
  $gdi0 = [W.G]::GetGuiResources($hp.Handle, 0)
  $h0 = $hp.HandleCount
  $swp = [System.Diagnostics.Stopwatch]::StartNew()
  for ($i = 0; $i -lt 50; $i++) {
    $r = Send-Tool 'screenshot_window' @{ hwnd=$nh; client_only=$true }
    if ($r.result.isError) { throw ("iteration $i failed: " + $r.result.content[0].text) }
  }
  $swp.Stop()
  $hp2 = Get-Process -Id $proc.Id
  $gdi1 = [W.G]::GetGuiResources($hp2.Handle, 0)
  $h1 = $hp2.HandleCount
  Check 'gdi leak 50x capture' (($gdi1 -le $gdi0 + 1) -and ($h1 -le $h0 + 3)) ("gdi " + $gdi0 + "->" + $gdi1 + ", handles " + $h0 + "->" + $h1 + ", " + [Math]::Round($swp.Elapsed.TotalMilliseconds) + "ms/50")

  # restore notepad size then close
  $rz2 = Send-Tool 'resize_window' @{ hwnd=$nh; width=640; height=480 }
  $r2 = Send-Tool 'get_window_rect' @{ hwnd=$nh }
  $pr2 = Tool-Payload $r2
  Check 'resize 640x480' ((-not $rz2.result.isError) -and $pr2.window_rect.width -eq 640 -and $pr2.window_rect.height -eq 480) ("outer=" + $pr2.window_rect.width + "x" + $pr2.window_rect.height)
  $np.Kill()

  # --- UIA inspect on a deterministic WinForms probe ---
  # 独立进程 + C# STA 线程 + Application.Run 消息泵:UIA 提供程序是跨进程 COM,
  # 目标必须泵消息才能响应(不同于 e2e-hung 的挂起窗口)。按钮置于客户区几何中心,
  # 命中点取服务器返回的物理客户区中心,与 DPI 缩放/虚拟化无关。
  $probeScript = @'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -TypeDefinition @"
using System;
using System.Windows.Forms;
using System.Drawing;
public static class UiaProbe {
    public static void Start() {
        var t = new System.Threading.Thread(() => {
            var f = new Form();
            f.Text = "UIA_E2E_PROBE";
            f.TopMost = true;
            f.ClientSize = new Size(420, 260);
            var b = new Button();
            b.Text = "UIA_OK_probe";
            b.Size = new Size(160, 48);
            b.Location = new Point(130, 106);
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
[UiaProbe]::Start()
while ($true) { Start-Sleep -Seconds 60 }
'@
  $probeFile = Join-Path $env:TEMP 'wt-uia-probe.ps1'
  Set-Content -Path $probeFile -Value $probeScript -Encoding UTF8
  $probeOut = Join-Path $env:TEMP 'wt-uia-out.txt'
  $probeProc = Start-Process powershell -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File',$probeFile -PassThru -RedirectStandardOutput $probeOut
  Start-Sleep -Milliseconds 2500
  $lwf = Send-Tool 'list_windows' @{ pid=$probeProc.Id; title='UIA_E2E_PROBE' }
  $pf = Tool-Payload $lwf
  Check 'find uia probe window' ($pf -and $pf.count -ge 1) ("hwnd=" + $pf.windows[0].hwnd)
  if ($pf -and $pf.count -ge 1) {
    $fh = [string]$pf.windows[0].hwnd
    $rf = Send-Tool 'get_window_rect' @{ hwnd=$fh }
    $prf = Tool-Payload $rf
    # 工具坐标为客户区局部物理像素:中心点 = 客户区宽高各半
    $cx = [int]($prf.client_rect.width / 2)
    $cy = [int]($prf.client_rect.height / 2)
    $insp = Send-Tool 'inspect_element_at' @{ hwnd=$fh; x=$cx; y=$cy }
    $pi = Tool-Payload $insp
    Check 'inspect_element_at center hits button' ($null -ne $pi -and $pi.element.control_type_id -eq 50000 -and $pi.element.name -eq 'UIA_OK_probe') ("type=" + $pi.element.control_type + " name=" + $pi.element.name + " focusable=" + $pi.element.keyboard_focusable)
    $inspO = Send-Tool 'inspect_element_at' @{ hwnd=$fh; x=($prf.client_rect.width + 50); y=10 }
    Check 'inspect_element_at out-of-range rejected' ($inspO.result.isError) $inspO.result.content[0].text
    $foc = Send-Tool 'inspect_focused_element' @{}
    $pfo = Tool-Payload $foc
    Check 'inspect_focused_element returns an element' ($null -ne $pfo -and $pfo.element.control_type -ne 'Unknown') ("type=" + $pfo.element.control_type + " name=" + $pfo.element.name)
  }
  if (!$probeProc.HasExited) { Stop-Process -Id $probeProc.Id -Force -ErrorAction SilentlyContinue }
  Remove-Item $probeFile, $probeOut -Force -ErrorAction SilentlyContinue

  Write-Output ""
  Write-Output ("RESULT: " + $pass + " passed, " + $fail + " failed")
} finally {
  if (!$proc.HasExited) { $proc.Kill(); $null = $proc.WaitForExit(2000) }
  Get-Process winver -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
  if ($null -ne $probeProc -and !$probeProc.HasExited) { Stop-Process -Id $probeProc.Id -Force -ErrorAction SilentlyContinue }
}
if ($fail -gt 0) { exit 1 } else { exit 0 }
