param(
    [ValidateSet('Install','Start','Stop','Uninstall')][string]$Action = 'Install',
    [string]$InstallDir = (Join-Path $env:ProgramFiles 'ClaudeCodeBridge'),
    [string]$DataRoot = (Join-Path $env:LOCALAPPDATA 'ClaudeCodeBridge/ls-engine'),
    [string]$TokenFile = (Join-Path $env:LOCALAPPDATA 'ClaudeCodeBridge/ls-host.token'),
    [int]$Port = 18791
)
$ErrorActionPreference = 'Stop'
if ($PSVersionTable.PSVersion.Major -lt 7) { throw 'PowerShell 7 is required' }
$exe = [IO.Path]::GetFullPath((Join-Path $InstallDir 'claude-ls-host.exe'))
$shortcutPath = Join-Path ([Environment]::GetFolderPath('Startup')) 'ClaudeCodeBridge-LS.lnk'
$owned = @(Get-CimInstance Win32_Process -Filter "Name='claude-ls-host.exe'" | Where-Object { $_.ExecutablePath -eq $exe -and $_.CommandLine.Contains($DataRoot) })
if ($Action -in @('Stop','Uninstall')) {
    if ($owned.Count -gt 0) {
        $headers = @{Authorization = 'Bearer ' + [IO.File]::ReadAllText($TokenFile).Trim()}
        $null = Invoke-RestMethod -Uri "http://127.0.0.1:$Port/shutdown" -Method Post -Headers $headers -ContentType 'application/json' -Body '{}' -NoProxy -TimeoutSec 15
        foreach ($process in $owned) {
            Wait-Process -Id $process.ProcessId -Timeout 20 -ErrorAction SilentlyContinue
            if (Get-Process -Id $process.ProcessId -ErrorAction SilentlyContinue) { throw 'Owned Host did not stop' }
        }
    }
    if ($Action -eq 'Uninstall' -and (Test-Path -LiteralPath $shortcutPath)) { Remove-Item -LiteralPath $shortcutPath }
    return
}
throw 'LS startup is now bridge-managed. Install install-ls-session-task.ps1 once, then select the LS provider in Model Center. No login shortcut or proxy override is used.'
