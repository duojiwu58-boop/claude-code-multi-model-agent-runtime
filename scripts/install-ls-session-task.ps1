param([string]$InstallDir = (Join-Path $env:ProgramFiles 'ClaudeCodeBridge'))
$ErrorActionPreference = 'Stop'
$exe = (Resolve-Path -LiteralPath (Join-Path $InstallDir 'claude-ls-launcher.exe')).Path
$serviceSid = ([Security.Principal.NTAccount]::new('NT SERVICE\ClaudeCodeBridge')).Translate([Security.Principal.SecurityIdentifier]).Value
$control = Join-Path $env:ProgramData 'ClaudeCodeBridge\ls-control'
$null = [IO.Directory]::CreateDirectory($control)
# Requests/leases contain paths and a nonce, never credentials. Only the service
# and administrators can mutate them; interactive users can read the request.
$acl = [Security.AccessControl.DirectorySecurity]::new()
$acl.SetSecurityDescriptorSddlForm("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;$serviceSid)(A;OICI;GRGX;;;IU)")
[IO.FileSystemAclExtensions]::SetAccessControl([IO.DirectoryInfo]::new($control),$acl)
$scheduler = New-Object -ComObject Schedule.Service
$scheduler.Connect()
$definition = $scheduler.NewTask(0)
$definition.RegistrationInfo.Description = 'Bridge-demanded LS in the selected interactive user session; no logon/boot trigger'
$definition.Principal.GroupId = 'S-1-5-4'
$definition.Principal.RunLevel = 0
$definition.Settings.Enabled = $true
$definition.Settings.AllowDemandStart = $true
$definition.Settings.DisallowStartIfOnBatteries = $false
$definition.Settings.StopIfGoingOnBatteries = $false
$definition.Settings.ExecutionTimeLimit = 'PT0S'
$definition.Settings.MultipleInstances = 2
$action = $definition.Actions.Create(0)
$action.Path = $exe
$action.Arguments = '--managed-request "$(Arg0)"'
$action.WorkingDirectory = $InstallDir
$sddl = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGX;;;$serviceSid)"
$null = $scheduler.GetFolder('\').RegisterTaskDefinition('ClaudeCodeBridge-LS-Interactive',$definition,6,'S-1-5-4',$null,4,$sddl)
Write-Output 'Installed demand-only interactive LS task (dynamic session, limited user token)'
