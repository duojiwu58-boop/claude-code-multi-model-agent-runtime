param(
    [int]$Port = 18788,
    [string]$CodexCommand,
    [string]$CodexHome,
    [string]$LogDirectory,
    [string]$ProxyUrl
)
$ErrorActionPreference = 'Stop'
if ($Port -lt 1 -or $Port -gt 65535) { throw 'Port must be between 1 and 65535' }
if ([string]::IsNullOrWhiteSpace($CodexCommand)) {
    $installedCommand = Get-Command codex.exe -CommandType Application -ErrorAction SilentlyContinue
    if ($null -ne $installedCommand) {
        $CodexCommand = $installedCommand.Source
    }
    else {
        # Logon tasks do not inherit the VS Code terminal's extended PATH.
        # Resolve the installed extension again after extension upgrades.
        $extensionsDirectory = Join-Path $env:USERPROFILE '.vscode\extensions'
        if (Test-Path -LiteralPath $extensionsDirectory -PathType Container) {
            $extensions = Get-ChildItem -LiteralPath $extensionsDirectory -Directory -Filter 'openai.chatgpt-*' |
                Sort-Object LastWriteTimeUtc -Descending
            foreach ($extension in $extensions) {
                $candidate = Join-Path $extension.FullName 'bin\windows-x86_64\codex.exe'
                if (Test-Path -LiteralPath $candidate -PathType Leaf) {
                    $CodexCommand = $candidate
                    break
                }
            }
        }
    }
    if ([string]::IsNullOrWhiteSpace($CodexCommand)) {
        throw 'codex.exe was not found. Specify -CodexCommand with its absolute path.'
    }
}
$executable = (Resolve-Path -LiteralPath $CodexCommand -ErrorAction Stop).Path
if ([IO.Path]::GetExtension($executable) -ne '.exe') { throw 'CodexCommand must point to codex.exe' }
if (-not [string]::IsNullOrWhiteSpace($CodexHome)) {
    $env:CODEX_HOME = (Resolve-Path -LiteralPath $CodexHome -ErrorAction Stop).Path
}
# Run as the current user so Codex owns its normal login and credential store.
# Keep this console running. The bridge service connects over loopback and
# never receives or copies account tokens. Ctrl+C stops this owned host.
Write-Host "Codex host: ws://127.0.0.1:$Port"
Write-Host 'Keep this terminal open while using the Local Codex provider.'
$savedProxyEnvironment = @{}
if (-not [string]::IsNullOrWhiteSpace($ProxyUrl)) {
    $proxyUri = $null
    if (-not [Uri]::TryCreate($ProxyUrl, [UriKind]::Absolute, [ref]$proxyUri) -or
        $proxyUri.Scheme -notin @('http', 'https')) { throw 'ProxyUrl must be an HTTP(S) proxy URL.' }
    foreach ($name in @('HTTP_PROXY', 'HTTPS_PROXY', 'NO_PROXY')) {
        $savedProxyEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
    }
    $env:HTTP_PROXY = $ProxyUrl
    $env:HTTPS_PROXY = $ProxyUrl
    $env:NO_PROXY = (@($savedProxyEnvironment['NO_PROXY'], 'localhost', '127.0.0.1', '::1') |
        Where-Object { -not [string]::IsNullOrWhiteSpace($_) }) -join ','
}
try {
    if ([string]::IsNullOrWhiteSpace($LogDirectory)) {
        & $executable app-server --listen "ws://127.0.0.1:$Port" -c 'features.hooks=false' -c 'features.shell_tool=false' -c 'features.apps=false'
        $codexExit = $LASTEXITCODE
    }
    else {
        $resolvedLogs = [IO.Path]::GetFullPath($LogDirectory)
        [IO.Directory]::CreateDirectory($resolvedLogs) | Out-Null
        $stdoutLog = Join-Path $resolvedLogs 'codex-host.stdout.log'
        $stderrLog = Join-Path $resolvedLogs 'codex-host.stderr.log'
        & $executable app-server --listen "ws://127.0.0.1:$Port" -c 'features.hooks=false' -c 'features.shell_tool=false' -c 'features.apps=false' 1>> $stdoutLog 2>> $stderrLog
        $codexExit = $LASTEXITCODE
    }
}
finally {
    foreach ($name in $savedProxyEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($name, $savedProxyEnvironment[$name], 'Process')
    }
}
if ($codexExit -ne 0) { throw "Codex host exited with code $codexExit" }
