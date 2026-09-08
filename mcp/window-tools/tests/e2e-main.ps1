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
Add-Type -Namespace W -Name G -MemberDefinition '[DllImport("user32.dll")] public static extern uint GetGuiResources(System.IntPtr hProcess, uint uiFlags);'
Add-Type -Namespace W -Name U -MemberDefinition '[DllImport("user32.dll")] public static extern bool ShowWindow(System.IntPtr hWnd, int nCmdShow);'
# 服务端日志落盘:WGC 生命周期测试(LRU 淘汰/会话清扫/尺寸重建)以日志为证据
$logPath = Join-Path $env:TEMP 'wt-e2e.log'
Remove-Item $logPath -Force -ErrorAction SilentlyContinue
$psi.EnvironmentVariables['WT_LOG_LEVEL'] = 'info'
$psi.EnvironmentVariables['WT_LOG_FILE'] = $logPath
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
function Png-Dims($content) {
  $img = $content | Where-Object { $_.type -eq 'image' } | Select-Object -First 1
  if ($null -eq $img) { return @(0, 0) }
  $b = [Convert]::FromBase64String($img.data)
  # PNG 签名 8 字节 + IHDR;宽度偏移 16、高度偏移 20,大端 u32。
  # 注意 -shl 左操作数必须 [int]:[byte] 会在 byte 算术中回绕(1 -shl 8 = 0)。
  $w = ([int]$b[16] -shl 24) -bor ([int]$b[17] -shl 16) -bor ([int]$b[18] -shl 8) -bor [int]$b[19]
  $h = ([int]$b[20] -shl 24) -bor ([int]$b[21] -shl 16) -bor ([int]$b[22] -shl 8) -bor [int]$b[23]
  return @([int]$w, [int]$h)
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
  # v0.7:每行 elevated 字段(UIPI 预判,布尔)
  Check 'list_windows elevated flag boolean' ($null -ne $p.windows[0].PSObject.Properties['elevated'] -and $p.windows[0].elevated -is [bool]) ("sample elevated=" + $p.windows[0].elevated)

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
  $waM = Send-Tool 'window_activate' @{ hwnd=$nh }
  $pwam = Tool-Payload $waM
  Check 'window_activate reports activation_method' ((-not $waM.result.isError) -and $pwam.activation_method -in @('direct','attach_input','alt_trick')) ("method=" + $pwam.activation_method)
  Start-Sleep -Milliseconds 400
  $si = Send-Tool 'send_input' @{ hwnd=$nh; activate=$false; actions=@(@{kind='key_tap'; vkey='a'}) }
  Check 'send_input foreground key_tap' (-not $si.result.isError) ($si.result.content[0].text)
  Start-Sleep -Milliseconds 200
  $siN = Send-Tool 'send_input' @{ hwnd='197480'; activate=$false; actions=@(@{kind='key_tap'; vkey='b'}) }
  Check 'send_input refuses non-foreground' ($siN.result.isError) $siN.result.content[0].text
  Start-Sleep -Milliseconds 200
  $siA = Send-Tool 'send_input' @{ hwnd=$nh; activate=$true; actions=@(@{kind='key_tap'; vkey='c'}) }
  Check 'send_input activate:true raises and injects' (-not $siA.result.isError) ($siA.result.content[0].text)
  # v0.7 多模型容错:整数字符串参数 + 激活方式标注
  $siS = Send-Tool 'send_input' @{ hwnd=$nh; activate=$true; delay_ms='30'; actions=@(@{kind='key_tap'; vkey='d'}) }
  Check 'send_input string delay_ms tolerated' (-not $siS.result.isError) ($siS.result.content[0].text)
  $siP = Tool-Payload $siS
  Check 'send_input activation_method reported' ($siP.activation_method -in @('direct','attach_input','alt_trick')) ("method=" + $siP.activation_method)

  $scanN = Send-Tool 'pixel_scan_line' @{ hwnd=$nh; x=250; y_from=0; y_to=300 }
  $psn = Tool-Payload $scanN
  Check 'pixel_scan_line winver in-range' ($psn -and $psn.band_count -gt 0) ("bands=" + $psn.band_count)
  $scanO = Send-Tool 'pixel_scan_line' @{ hwnd=$nh; x=250; y_from=0; y_to=400 }
  Check 'pixel_scan_line out-of-range error' ($scanO.result.isError) $scanO.result.content[0].text
  # v0.7:采样来源与遮挡标注
  $scanS = Send-Tool 'pixel_scan_line' @{ hwnd=$nh; x=250; y_from=0; y_to=100; method='auto' }
  $pss = Tool-Payload $scanS
  Check 'pixel_scan_line reports source+occluded' ($pss -and $pss.source -eq 'screen' -and $null -ne $pss.PSObject.Properties['occluded']) ("source=" + $pss.source + " occluded=" + $pss.occluded)
  $scanW = Send-Tool 'pixel_scan_line' @{ hwnd=$nh; x=250; y_from=0; y_to=100; method='wgc' }
  $psw = Tool-Payload $scanW
  Check 'pixel_scan_line method=wgc' ($psw -and $psw.source -eq 'wgc' -and $psw.band_count -gt 0) ("source=" + $psw.source + " bands=" + $psw.band_count)
  $findW = Send-Tool 'pixel_find_color' @{ hwnd=$nh; x=0; y=0; width=100; height=100; color='#000000'; tolerance=60; method='wgc' }
  $pfw = Tool-Payload $findW
  Check 'pixel_find_color method=wgc' ($pfw -and $pfw.source -eq 'wgc') ("source=" + $pfw.source)
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
  # --- auto 链来源标注契约:无回退,或标注必须是 wgc/screen 形态 ---
  $swText = $sw.result.content[0].text
  Check 'screenshot_window auto source annotation' ($swText -notmatch 'fallback=' -or $swText -match 'fallback=(wgc|screen) \(') $swText

  # --- v0.7:JPEG 编码(4:4:4)与 max_dim 降采样 ---
  $jpg = Send-Tool 'screenshot_window' @{ hwnd=$nh; client_only=$false; format='jpeg'; quality=85 }
  Check 'screenshot_window format=jpeg' ((-not $jpg.result.isError) -and $jpg.result.content[1].mimeType -eq 'image/jpeg') ($jpg.result.content[0].text + " b64len=" + $jpg.result.content[1].data.Length)
  $jbytes = [Convert]::FromBase64String($jpg.result.content[1].data)
  Check 'jpeg magic bytes FFD8' ($jbytes[0] -eq 0xFF -and $jbytes[1] -eq 0xD8) ("head=" + $jbytes[0].ToString('X2') + $jbytes[1].ToString('X2'))
  $dim = Send-Tool 'screenshot_window' @{ hwnd=$nh; client_only=$false; max_dim=100 }
  $dimDims = Png-Dims $dim.result.content
  Check 'screenshot_window max_dim scales output' ((-not $dim.result.isError) -and $dimDims[0] -eq 100 -and $dimDims[1] -eq 80 -and $dim.result.content[0].text -match '输出 100x80') ($dim.result.content[0].text + " png=" + $dimDims[0] + "x" + $dimDims[1])
  $incJ = Send-Tool 'screenshot_window' @{ hwnd=$nh; method='wgc'; incremental=$true; format='jpeg' }
  Check 'incremental rejects jpeg format' ($incJ.result.isError) ($incJ.result.content[0].text)

  # --- WGC 实验路径:GPU 合成帧,独立于 PrintWindow/屏幕采样 ---
  $wg = Send-Tool 'screenshot_window' @{ hwnd=$nh; client_only=$false; method='wgc' }
  Check 'screenshot_window method=wgc winver' ((-not $wg.result.isError) -and $wg.result.content.Count -ge 2 -and $wg.result.content[1].mimeType -eq 'image/png' -and $wg.result.content[1].data.StartsWith('iVBORw0KGgo')) ($wg.result.content[0].text + " b64len=" + $wg.result.content[1].data.Length)
  $wgBad = Send-Tool 'screenshot_window' @{ hwnd=$nh; method='bogus' }
  Check 'screenshot_window unknown method rejected' ($wgBad.result.isError) $wgBad.result.content[0].text

  # --- screenshot_all ---
  $sa = Send-Tool 'screenshot_all' @{}
  $imgCount = 0; $txtLines = @()
  foreach ($c in $sa.result.content) { if ($c.type -eq 'image') { $imgCount++ } else { $txtLines += $c.text } }
  Check 'screenshot_all' ((-not $sa.result.isError) -and $imgCount -ge 1) ("images=" + $imgCount + " monitors-text: " + ($txtLines -join ' | ').Substring(0, [Math]::Min(160, ($txtLines -join ' | ').Length)))
  # --- screenshot_all WGC 显示器路径:每屏一次 CreateForMonitor 会话(含光标) ---
  $saw = Send-Tool 'screenshot_all' @{ method='wgc' }
  $sawImg = 0; $sawTxt = @()
  foreach ($c in $saw.result.content) { if ($c.type -eq 'image') { $sawImg++ } else { $sawTxt += $c.text } }
  $sawText = $sawTxt -join ' | '
  Check 'screenshot_all method=wgc' ((-not $saw.result.isError) -and $sawImg -ge 1 -and $sawText -match 'method=wgc') ("images=" + $sawImg + " " + $sawText.Substring(0, [Math]::Min(160, $sawText.Length)))
  $sawBad = Send-Tool 'screenshot_all' @{ method='bogus' }
  Check 'screenshot_all unknown method rejected' ($sawBad.result.isError) ($sawBad.result.content[0].text)
  # v0.7:screenshot_all 支持 jpeg + max_dim
  $saj = Send-Tool 'screenshot_all' @{ format='jpeg'; max_dim=400 }
  $sajImg = 0
  foreach ($c in $saj.result.content) { if ($c.type -eq 'image' -and $c.mimeType -eq 'image/jpeg') { $sajImg++ } }
  Check 'screenshot_all format=jpeg + max_dim' ((-not $saj.result.isError) -and $sajImg -ge 1) ("jpeg images=" + $sajImg)

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
    public static System.Threading.Timer CoverTimer;
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
            // Second button: gives the incremental-diff test a deterministic
            // focus-ring change (WinForms auto-focuses the first button on
            // Show, so clicking it produces no pixel change).
            var b2 = new Button();
            b2.Text = "UIA_OK_probe2";
            b2.Size = new Size(100, 40);
            b2.Location = new Point(20, 20);
            f.Controls.Add(b2);
            f.Show();
            Console.WriteLine(f.Handle.ToInt64());
            Console.Out.Flush();
            // Cover form for occlusion tests: hidden until the flag file appears.
            // Owned by the main form so it always stays above it in z-order.
            // Keep this Add-Type source free of non-ASCII comments (PS 5.1 encoding).
            var f2 = new Form();
            f2.Text = "UIA_E2E_COVER";
            f2.FormBorderStyle = FormBorderStyle.None;
            f2.BackColor = Color.Magenta;
            f2.ClientSize = new Size(200, 120);
            f2.StartPosition = FormStartPosition.Manual;
            f2.TopMost = true;  // owner f is TopMost; plain windows sit below that band
            f2.Owner = f;
            // Drive the toggle from the main form via BeginInvoke: invoking on a
            // hidden control whose handle is not created throws, so never touch
            // f2 from the timer thread directly.
            UiaProbe.CoverTimer = new System.Threading.Timer(_ => {
                try {
                    bool want = System.IO.File.Exists(System.IO.Path.Combine(System.IO.Path.GetTempPath(), "wt-cover.flag"));
                    f.BeginInvoke((Action)(() => {
                        if (want != f2.Visible) {
                            if (want) { f2.Location = f.Location + new Size(60, 60); f2.Show(); }
                            else { f2.Hide(); }
                        }
                    }));
                } catch { }
            }, null, 0, 500);
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
    # --- WGC 重复采集:每次调用独立建链(D3D11 设备/帧池/会话/帧),句柄须有界 ---
    $hp = Get-Process -Id $proc.Id
    $wg0 = [W.G]::GetGuiResources($hp.Handle, 0)
    $hwg0 = $hp.HandleCount
    for ($i = 0; $i -lt 3; $i++) {
      $wgr = Send-Tool 'screenshot_window' @{ hwnd=$fh; client_only=$false; method='wgc' }
      if ($wgr.result.isError) { throw ("wgc iteration $i failed: " + $wgr.result.content[0].text) }
    }
    $hp3 = Get-Process -Id $proc.Id
    $wg1 = [W.G]::GetGuiResources($hp3.Handle, 0)
    $hwg1 = $hp3.HandleCount
    Check 'wgc 3x capture handle bounded' (($wg1 -le $wg0 + 10) -and ($hwg1 -le $hwg0 + 25)) ("gdi " + $wg0 + "->" + $wg1 + ", handles " + $hwg0 + "->" + $hwg1)
    # --- auto 链在 WinForms 窗口上完整走通,来源标注符合契约 ---
    $fha = Send-Tool 'screenshot_window' @{ hwnd=$fh; client_only=$false; method='auto' }
    $fhaText = if ($fha.result.content) { $fha.result.content[0].text } else { '' }
    Check 'screenshot_window auto probe (chain)' ((-not $fha.result.isError) -and ($fhaText -notmatch 'fallback=' -or $fhaText -match 'fallback=(wgc|screen) \(')) $fhaText

    # --- v0.7 像素采样遮挡感知:auto(屏幕,遮挡即错采并标注) vs wgc(合成帧,遮挡也准确) ---
    # 覆盖窗(同进程、Owner 挂在探针上、品红底)由探针按标记文件显示/隐藏,位置确定。
    $coverFlag = Join-Path $env:TEMP 'wt-cover.flag'
    Set-Content -Path $coverFlag -Value 'on' -Encoding ASCII
    Start-Sleep -Milliseconds 1500
    $coverLw = Send-Tool 'list_windows' @{ title='UIA_E2E_COVER' }
    $pcover = Tool-Payload $coverLw
    Check 'occlusion cover window visible' ($pcover -and $pcover.count -ge 1) ("cover windows=" + $pcover.count)
    $scanCov = Send-Tool 'pixel_scan_line' @{ hwnd=$fh; x=150; y_from=80; y_to=160; method='auto' }
    $pscov = Tool-Payload $scanCov
    Check 'pixel_scan_line auto detects occlusion' ($pscov -and $pscov.source -eq 'screen' -and $pscov.occluded -eq $true) ("occluded=" + $pscov.occluded + " band0=" + ($pscov.bands[0] | ConvertTo-Json -Compress))
    $scanCovW = Send-Tool 'pixel_scan_line' @{ hwnd=$fh; x=150; y_from=80; y_to=160; method='wgc' }
    $pscovw = Tool-Payload $scanCovW
    Check 'pixel_scan_line wgc bypasses occlusion' ($pscovw -and $pscovw.source -eq 'wgc' -and $pscovw.band_count -gt 0) ("source=" + $pscovw.source + " bands=" + $pscovw.band_count)
    # 覆盖下屏幕采样到品红(覆盖窗内容),WGC 帧给出探针真实内容:首带颜色必须不同
    Check 'occlusion: screen vs wgc pixels differ' ($pscov.bands[0].color -ne $pscovw.bands[0].color) ("screen=" + $pscov.bands[0].color + " wgc=" + $pscovw.bands[0].color)
    Remove-Item $coverFlag -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 1200
    $scanClear = Send-Tool 'pixel_scan_line' @{ hwnd=$fh; x=150; y_from=80; y_to=160; method='auto' }
    $psclear = Tool-Payload $scanClear
    Check 'pixel_scan_line auto clears after uncover' ($psclear -and $psclear.occluded -eq $false) ("occluded=" + $psclear.occluded)

    # --- 增量差分(WGC 会话缓存):基线 → 无变化 → 点击触发变化 → 变化 → 再无变化 ---
    # 增量会话禁用光标捕获,差分与光标移动无关,判定确定。
    $incBad = Send-Tool 'screenshot_window' @{ hwnd=$fh; method='auto'; incremental=$true }
    Check 'incremental requires method=wgc' ($incBad.result.isError) ($incBad.result.content[0].text)
    $inc1 = Send-Tool 'screenshot_window' @{ hwnd=$fh; client_only=$true; method='wgc'; incremental=$true }
    $inc1Img = ($inc1.result.content | Where-Object { $_.type -eq 'image' }).Count
    $inc1Text = if ($inc1.result.content) { $inc1.result.content[0].text } else { '' }
    Check 'incremental baseline frame' ((-not $inc1.result.isError) -and $inc1Img -eq 1 -and $inc1Text -match '基线') ($inc1Text)
    $inc2 = Send-Tool 'screenshot_window' @{ hwnd=$fh; client_only=$true; method='wgc'; incremental=$true }
    $inc2Img = ($inc2.result.content | Where-Object { $_.type -eq 'image' }).Count
    Check 'incremental no-change is text-only' ((-not $inc2.result.isError) -and $inc2Img -eq 0 -and $inc2.result.content[0].text -match '无变化') ($inc2.result.content[0].text)
    # 点击探针第二按钮(焦点环从按钮1移到按钮2,必产生客户区像素变化),
    # 等待 DWM 产帧。按钮2 逻辑中心 (70,40),按 DPI 缩放转物理坐标。
    $scale = $prf.client_rect.width / 420
    $b2x = [int](70 * $scale); $b2y = [int](40 * $scale)
    $click = Send-Tool 'send_input' @{ hwnd=$fh; activate=$true; actions=@(@{kind='click'; x=$b2x; y=$b2y}) }
    Check 'incremental setup click' (-not $click.result.isError) ($click.result.content[0].text)
    Start-Sleep -Milliseconds 700
    $inc3 = Send-Tool 'screenshot_window' @{ hwnd=$fh; client_only=$true; method='wgc'; incremental=$true }
    $inc3Img = ($inc3.result.content | Where-Object { $_.type -eq 'image' }).Count
    $inc3Text = if ($inc3.result.content) { $inc3.result.content[0].text } else { '' }
    Check 'incremental reports change after click' ((-not $inc3.result.isError) -and $inc3Img -ge 1 -and $inc3Text -match '变化区域') ($inc3Text)
    Start-Sleep -Milliseconds 300
    $inc4 = Send-Tool 'screenshot_window' @{ hwnd=$fh; client_only=$true; method='wgc'; incremental=$true }
    $inc4Img = ($inc4.result.content | Where-Object { $_.type -eq 'image' }).Count
    Check 'incremental settles back to no-change' ((-not $inc4.result.isError) -and $inc4Img -eq 0) ($inc4.result.content[0].text)
    # v0.7 多模型容错:鼠标坐标整数字符串(再次点击按钮2,无视觉变化,不影响后续)
    $clickStr = Send-Tool 'send_input' @{ hwnd=$fh; activate=$true; actions=@(@{kind='click'; x="$b2x"; y="$b2y"}) }
    Check 'send_input string coords tolerated' (-not $clickStr.result.isError) ($clickStr.result.content[0].text)

    # --- ① 最小化中途:缓存会话后最小化 → 确定性结构化拒绝(不挂 15s),恢复后正常 ---
    # 背景:GetWindowRect 对最小化窗口返回 (-32000,-32000) 位置但尺寸仍为正,
    # 尺寸预检拦不住;缓存会话还会复用最小化前的旧帧。v0.6.1 起 IsIconic 显式拒绝。
    $null = [W.U]::ShowWindow([IntPtr][int64]$fh, 6)  # SW_MINIMIZE
    Start-Sleep -Milliseconds 500
    $swMin = [System.Diagnostics.Stopwatch]::StartNew()
    $min1 = Send-Tool 'screenshot_window' @{ hwnd=$fh; client_only=$false; method='wgc' }
    $swMin.Stop()
    Check 'wgc minimized (cached session) -> structured reject fast' ($min1.result.isError -and $min1.result.error_code -eq 'WINDOW_NOT_VISIBLE' -and $swMin.Elapsed.TotalMilliseconds -lt 3000) ("code=" + $min1.result.error_code + " elapsed=" + [Math]::Round($swMin.Elapsed.TotalMilliseconds) + "ms " + $min1.result.content[0].text)
    $minInc = Send-Tool 'screenshot_window' @{ hwnd=$fh; client_only=$true; method='wgc'; incremental=$true }
    Check 'incremental minimized -> structured reject' ($minInc.result.isError -and $minInc.result.error_code -eq 'WINDOW_NOT_VISIBLE') ("code=" + $minInc.result.error_code + " " + $minInc.result.content[0].text)
    $minAuto = Send-Tool 'screenshot_window' @{ hwnd=$fh; client_only=$false; method='auto' }
    Check 'auto minimized -> structured reject' ($minAuto.result.isError -and $minAuto.result.error_code -eq 'WINDOW_NOT_VISIBLE') ("code=" + $minAuto.result.error_code + " " + $minAuto.result.content[0].text)
    $null = [W.U]::ShowWindow([IntPtr][int64]$fh, 9)  # SW_RESTORE
    Start-Sleep -Milliseconds 600
    $min2 = Send-Tool 'screenshot_window' @{ hwnd=$fh; client_only=$false; method='wgc' }
    Check 'wgc after restore works again' ((-not $min2.result.isError) -and $min2.result.content.Count -ge 2) ($min2.result.content[0].text)

    # --- ② 会话缓存压力:同一窗口连续 50 次,句柄/内存有界 + 复用标记 ---
    $hp = Get-Process -Id $proc.Id
    $w50g0 = [W.G]::GetGuiResources($hp.Handle, 0)
    $w50h0 = $hp.HandleCount
    $w50ws0 = $hp.WorkingSet64
    $sw50 = [System.Diagnostics.Stopwatch]::StartNew()
    $reusedSeen = $false
    for ($i = 0; $i -lt 50; $i++) {
      $r = Send-Tool 'screenshot_window' @{ hwnd=$fh; client_only=$false; method='wgc' }
      if ($r.result.isError) { throw ("wgc 50x iteration $i failed: " + $r.result.content[0].text) }
      if ($r.result.content[0].text -match '复用上次帧') { $reusedSeen = $true }
    }
    $sw50.Stop()
    $hp4 = Get-Process -Id $proc.Id
    $w50g1 = [W.G]::GetGuiResources($hp4.Handle, 0)
    $w50h1 = $hp4.HandleCount
    $w50ws1 = $hp4.WorkingSet64
    Check 'wgc session cache 50x handle bounded' (($w50g1 -le $w50g0 + 1) -and ($w50h1 -le $w50h0 + 3)) ("gdi " + $w50g0 + "->" + $w50g1 + ", handles " + $w50h0 + "->" + $w50h1 + ", " + [Math]::Round($sw50.Elapsed.TotalMilliseconds) + "ms/50")
    Check 'wgc session cache 50x memory bounded' (($w50ws1 - $w50ws0) -lt 64MB) ("ws +" + [Math]::Round(($w50ws1 - $w50ws0) / 1MB, 1) + "MB")
    Check 'wgc session reuse observed in 50x' ($reusedSeen) "复用上次帧 seen"

    # --- ⑤ 尺寸变化:调整窗口后采集必须反映新尺寸(会话重建路径,E2E 首验) ---
    $sz0 = Send-Tool 'screenshot_window' @{ hwnd=$fh; client_only=$false; method='wgc' }
    $oldDims = Png-Dims $sz0.result.content
    Check 'wgc pre-resize frame measured' ((-not $sz0.result.isError) -and $oldDims[0] -gt 100) ("frame=" + $oldDims[0] + "x" + $oldDims[1])
    $rz5 = Send-Tool 'resize_window' @{ hwnd=$fh; width=640; height=480 }
    Check 'probe resize to 640x480' (-not $rz5.result.isError) $rz5.result.content[0].text
    Start-Sleep -Milliseconds 700
    $sz1 = Send-Tool 'screenshot_window' @{ hwnd=$fh; client_only=$false; method='wgc' }
    $newDims = Png-Dims $sz1.result.content
    Check 'wgc after resize reflects new size' ((-not $sz1.result.isError) -and ([Math]::Abs($newDims[0] - 640) -le 20) -and ([Math]::Abs($newDims[1] - 480) -le 20) -and ($newDims[0] -ne $oldDims[0])) ("frame " + $oldDims[0] + "x" + $oldDims[1] + " -> " + $newDims[0] + "x" + $newDims[1] + " " + $sz1.result.content[0].text)

    # --- ③ 多会话并发:5 个窗口会话(缓存上限 4)触发 LRU 淘汰,淘汰不破坏在用会话 ---
    $lruProc = @()
    for ($i = 0; $i -lt 5; $i++) { $lruProc += (Start-Process winver -PassThru) }
    Start-Sleep -Milliseconds 2500
    $lruH = @()
    for ($i = 0; $i -lt 5; $i++) {
      $plw = Send-Tool 'list_windows' @{ pid=$lruProc[$i].Id }
      $plwp = Tool-Payload $plw
      if (-not $plwp -or $plwp.count -lt 1) { throw ("lru winver $i not found") }
      $lruH += [string]$plwp.windows[0].hwnd
    }
    for ($i = 0; $i -lt 5; $i++) {
      $r = Send-Tool 'screenshot_window' @{ hwnd=$lruH[$i]; client_only=$false; method='wgc' }
      if ($r.result.isError) { throw ("lru create $i failed: " + $r.result.content[0].text) }
    }
    # 第 5 个会话新建时 LRU 淘汰了最旧的 win1 → 重捕 win1 = 全新会话(无复用标记)
    $l1 = Send-Tool 'screenshot_window' @{ hwnd=$lruH[0]; client_only=$false; method='wgc' }
    Check 'lru evicted window recaptured fresh' ((-not $l1.result.isError) -and ($l1.result.content[0].text -notmatch '复用上次帧')) ($l1.result.content[0].text)
    # 重捕 win1 淘汰了当时最旧的 win2 → win2 重捕同样无复用标记(淘汰级联正确)
    $l2 = Send-Tool 'screenshot_window' @{ hwnd=$lruH[1]; client_only=$false; method='wgc' }
    Check 'lru eviction cascades to then-oldest' ((-not $l2.result.isError) -and ($l2.result.content[0].text -notmatch '复用上次帧')) ($l2.result.content[0].text)
    # 未被淘汰的 win5 复用缓存会话,不受两次淘汰影响
    $l5 = Send-Tool 'screenshot_window' @{ hwnd=$lruH[4]; client_only=$false; method='wgc' }
    Check 'lru in-use session unharmed' ((-not $l5.result.isError) -and ($l5.result.content[0].text -match '复用上次帧')) ($l5.result.content[0].text)

    # --- ④ 窗口销毁中途 + 无会话最小化:结构化错误、会话清扫、服务存活 ---
    $wp6 = Start-Process winver -PassThru
    Start-Sleep -Milliseconds 1500
    $plw6 = Send-Tool 'list_windows' @{ pid=$wp6.Id }
    $plwp6 = Tool-Payload $plw6
    $h6 = [string]$plwp6.windows[0].hwnd
    $null = [W.U]::ShowWindow([IntPtr][int64]$h6, 6)
    Start-Sleep -Milliseconds 400
    $swMin2 = [System.Diagnostics.Stopwatch]::StartNew()
    $min6 = Send-Tool 'screenshot_window' @{ hwnd=$h6; client_only=$false; method='wgc' }
    $swMin2.Stop()
    Check 'wgc fresh minimized -> structured reject fast' ($min6.result.isError -and $min6.result.error_code -eq 'WINDOW_NOT_VISIBLE' -and $swMin2.Elapsed.TotalMilliseconds -lt 3000) ("code=" + $min6.result.error_code + " elapsed=" + [Math]::Round($swMin2.Elapsed.TotalMilliseconds) + "ms")
    $null = [W.U]::ShowWindow([IntPtr][int64]$h6, 9)
    Start-Sleep -Milliseconds 500
    $cap6 = Send-Tool 'screenshot_window' @{ hwnd=$h6; client_only=$false; method='wgc' }
    Check 'wgc fresh restored captures ok' ((-not $cap6.result.isError) -and $cap6.result.content.Count -ge 2) ($cap6.result.content[0].text)
    $wp6.Kill()
    $null = $wp6.WaitForExit(3000)
    Start-Sleep -Milliseconds 300
    $dead = Send-Tool 'screenshot_window' @{ hwnd=$h6; client_only=$false; method='wgc' }
    Check 'wgc destroyed window -> WINDOW_NOT_FOUND' ($dead.result.isError -and $dead.result.error_code -eq 'WINDOW_NOT_FOUND') ("code=" + $dead.result.error_code + " " + $dead.result.content[0].text)
    $alive = Send-Tool 'screenshot_window' @{ hwnd=$fh; client_only=$false; method='wgc' }
    Check 'capture alive window after destroy' ((-not $alive.result.isError) -and $alive.result.content.Count -ge 2) ($alive.result.content[0].text)
  }
  if (!$probeProc.HasExited) { Stop-Process -Id $probeProc.Id -Force -ErrorAction SilentlyContinue }
  Remove-Item $probeFile, $probeOut -Force -ErrorAction SilentlyContinue

  # --- WGC 生命周期日志证据(WT_LOG_FILE 落盘) ---
  $logText = if (Test-Path $logPath) { Get-Content -Raw -Encoding UTF8 $logPath } else { '' }
  Check 'log: LRU eviction recorded' ($logText -match 'LRU 淘汰') "wgc-cache evict"
  Check 'log: dead session swept' ($logText -match '清理已销毁窗口会话') "wgc-cache sweep"
  Check 'log: session rebuild on resize recorded' ($logText -match '重建会话') "wgc-cache rebuild"
  Remove-Item $logPath -Force -ErrorAction SilentlyContinue

  Write-Output ""
  Write-Output ("RESULT: " + $pass + " passed, " + $fail + " failed")
} finally {
  if (!$proc.HasExited) { $proc.Kill(); $null = $proc.WaitForExit(2000) }
  Get-Process winver -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
  if ($null -ne $probeProc -and !$probeProc.HasExited) { Stop-Process -Id $probeProc.Id -Force -ErrorAction SilentlyContinue }
}
if ($fail -gt 0) { exit 1 } else { exit 0 }
