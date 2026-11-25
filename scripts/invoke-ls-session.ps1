param([Parameter(Mandatory)][string]$RequestFile)
$ErrorActionPreference = 'Stop'
if ($PSVersionTable.PSVersion.Major -lt 7) { throw 'PowerShell 7 required' }
Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class LsSessions {
    public static System.Threading.Tasks.Task<int> WaitForEof() { return System.Threading.Tasks.Task.Run(() => Console.In.Read()); }
    [StructLayout(LayoutKind.Sequential)] struct Session { public int Id; public IntPtr Name; public int State; }
    [DllImport("wtsapi32.dll", SetLastError=true)] static extern bool WTSEnumerateSessionsW(IntPtr server, int reserved, int version, out IntPtr data, out int count);
    [DllImport("wtsapi32.dll")] static extern void WTSFreeMemory(IntPtr data);
    [DllImport("kernel32.dll")] static extern uint WTSGetActiveConsoleSessionId();
    public static int[] Active() {
        IntPtr data; int count;
        if (!WTSEnumerateSessionsW(IntPtr.Zero, 0, 1, out data, out count)) {
            // A restricted service can lack WTS enumeration rights. The OS's
            // active-console ID is still authoritative; reject ambiguity with
            // other desktop sessions rather than guessing a username/token.
            var console = WTSGetActiveConsoleSessionId();
            var desktops = new HashSet<int>();
            foreach (var p in System.Diagnostics.Process.GetProcessesByName("explorer")) {
                using (p) { desktops.Add(p.SessionId); }
            }
            if (console != 0xffffffff && console > 0 && desktops.Count == 1 && desktops.Contains((int)console)) return new [] {(int)console};
            throw new Exception("Cannot identify one active interactive session (WTS unavailable; no unambiguous active console)");
        }
        try {
            var result = new List<int>(); int size = Marshal.SizeOf<Session>();
            for (int i=0; i<count; i++) { var s=Marshal.PtrToStructure<Session>(IntPtr.Add(data,i*size)); if (s.State==0 && s.Id>0) result.Add(s.Id); }
            return result.ToArray();
        } finally { WTSFreeMemory(data); }
    }
}
'@
$sessions = @([LsSessions]::Active())
if ($sessions.Count -ne 1) { throw "LS requires exactly one active interactive user session; found $($sessions.Count)" }
$sessionId = $sessions[0]
$requestPath = (Resolve-Path -LiteralPath $RequestFile).Path
if ($requestPath.Contains('"')) { throw 'Invalid request path' }
$leasePath = $requestPath + '.lease'
$scheduler = New-Object -ComObject Schedule.Service
$scheduler.Connect()
$task = $scheduler.GetFolder('\').GetTask('ClaudeCodeBridge-LS-Interactive')
if ($task.State -eq 4) { throw 'Interactive LS task is already running; refusing to adopt it' }
$running = $null
try {
    [IO.File]::WriteAllText($leasePath,'lease')
    $running = $task.RunEx($requestPath,4,$sessionId,$null)
    [Console]::Error.WriteLine("LS task started in session {0}; instance {1}",$sessionId,$running.InstanceGuid)
    # This pipe belongs to the bridge. EOF or loss of the selected session stops
    # this exact task instance; no process-name termination is used.
    $eof = [LsSessions]::WaitForEof()
    while (-not $eof.IsCompleted) {
        $running.Refresh()
        if ($running.State -notin @(2,4)) { break }
        if ($sessionId -notin @([LsSessions]::Active())) { break }
        [IO.File]::SetLastWriteTimeUtc($leasePath,[DateTime]::UtcNow)
        Start-Sleep -Milliseconds 500
    }
    [Console]::Error.WriteLine("LS controller ending: stdin_completed={0}, task_state={1}",$eof.IsCompleted,$running.State)
} finally {
    if ($null -ne $running) {
        try { $running.Refresh(); if ($running.State -in @(2,4)) { $running.Stop() } } catch { Write-Warning $_.Exception.Message }
    }
}
