param(
    [string]$HostExecutable = (Join-Path $PSScriptRoot '../claude-ls-host.exe'),
    [string]$DataRoot = (Join-Path $env:LOCALAPPDATA 'ClaudeCodeBridge/ls-engine'),
    [string]$TokenFile = (Join-Path $env:LOCALAPPDATA 'ClaudeCodeBridge/ls-host.token'),
    [int]$Port = 18791,
    [string]$StartupLog = (Join-Path $env:LOCALAPPDATA 'ClaudeCodeBridge/ls-host-startup.log')
)
$ErrorActionPreference = 'Stop'
if ($PSVersionTable.PSVersion.Major -lt 7) { throw 'PowerShell 7 is required' }
[IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($StartupLog))) | Out-Null
Start-Transcript -Path $StartupLog -Append | Out-Null
if ($Port -lt 1 -or $Port -gt 65535) { throw 'Invalid port' }
$executable = (Resolve-Path -LiteralPath $HostExecutable).Path
$tokenPath = [IO.Path]::GetFullPath($TokenFile)
$engineRoot = [IO.Path]::GetFullPath($DataRoot)
if (-not (Test-Path -LiteralPath $tokenPath)) {
    [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($tokenPath)) | Out-Null
    $token = [Convert]::ToHexString([Security.Cryptography.RandomNumberGenerator]::GetBytes(32))
    $file = [IO.File]::Open($tokenPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try { $bytes = [Text.Encoding]::UTF8.GetBytes($token); $file.Write($bytes) } finally { $file.Dispose() }
}
# The interactive user and SYSTEM (the bridge service) can read the token.
$acl = [Security.AccessControl.FileSecurity]::new()
$acl.SetAccessRuleProtection($true, $false)
$identity = [Security.Principal.WindowsIdentity]::GetCurrent().User
$system = [Security.Principal.SecurityIdentifier]::new('S-1-5-18')
foreach ($principal in @($identity, $system)) {
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($principal, 'FullControl', 'Allow'))
}
# Installed bridge runs under its virtual service account, not SYSTEM.
if (Get-Service -Name 'ClaudeCodeBridge' -ErrorAction SilentlyContinue) {
    $bridgeSid = ([Security.Principal.NTAccount]::new('NT SERVICE\ClaudeCodeBridge')).Translate([Security.Principal.SecurityIdentifier])
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($bridgeSid, 'Read', 'Allow'))
}
# Change only the DACL. Set-Acl on an existing complete descriptor can request
# SeSecurityPrivilege and fail under the task's intentionally limited token.
[IO.FileSystemAclExtensions]::SetAccessControl([IO.FileInfo]::new($tokenPath), $acl)
$saved = @{}
foreach ($key in @('HTTP_PROXY','HTTPS_PROXY','NO_PROXY','ALL_PROXY')) { $saved[$key] = [Environment]::GetEnvironmentVariable($key,'Process') }
try {
    if ([Diagnostics.Process]::GetCurrentProcess().SessionId -eq 0) { throw 'Legacy standalone startup requires an interactive user; use bridge-managed mode for the service' }
    foreach ($key in @('HTTP_PROXY','HTTPS_PROXY','NO_PROXY','ALL_PROXY')) {
        $value = [Environment]::GetEnvironmentVariable($key,'User')
        if ($null -eq $value) { $value = [Environment]::GetEnvironmentVariable($key,'Machine') }
        [Environment]::SetEnvironmentVariable($key,$value,'Process')
    }
    if (-not $env:HTTP_PROXY -and -not $env:HTTPS_PROXY) { throw 'User proxy environment is missing' }
    # No console is allocated, including when launched from the GUI task entry point.
    $info = [Diagnostics.ProcessStartInfo]::new($executable)
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    foreach ($argument in @('--data-root',$engineRoot,'--token-file',$tokenPath,'--port',"$Port")) {
        $info.ArgumentList.Add($argument)
    }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $info
    $stdout = [IO.File]::Create($StartupLog + '.stdout')
    $stderr = [IO.File]::Create($StartupLog + '.stderr')
    try {
        if (-not $process.Start()) { throw 'LS Host did not start' }
        $outCopy = $process.StandardOutput.BaseStream.CopyToAsync($stdout)
        $errCopy = $process.StandardError.BaseStream.CopyToAsync($stderr)
        $process.WaitForExit()
        $outCopy.GetAwaiter().GetResult()
        $errCopy.GetAwaiter().GetResult()
        $hostExit = $process.ExitCode
    } finally {
        $stdout.Dispose()
        $stderr.Dispose()
        $process.Dispose()
    }
    if ($hostExit -ne 0) { throw "LS Host exited with $hostExit" }
} finally {
    foreach ($key in $saved.Keys) { [Environment]::SetEnvironmentVariable($key,$saved[$key],'Process') }
}
