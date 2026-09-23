#Requires -Version 5.1
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [ValidatePattern('^[A-Za-z0-9_.-]+$')]
    [string]$TaskName = 'AgentDeskWorker',
    [ValidatePattern('^[A-Za-z0-9][A-Za-z0-9_.-]*$')]
    [string]$DatabaseSshAlias,
    [ValidateRange(1024, 65535)]
    [int]$DatabaseLocalPort = 15433,
    [ValidateRange(1, 65535)]
    [int]$DatabaseRemotePort = 5432,
    [switch]$Start
)

$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') { throw 'This installer requires Windows Task Scheduler.' }
# Direct execution avoids a launcher process and preserves Task Scheduler's
# process lifetime/exit status. The runtime uses its existing per-user layout.
$runtimeRoot = Join-Path $env:USERPROFILE '.adk\release'
$binary = Join-Path $runtimeRoot 'bin\agentdesk.exe'
$configuration = Join-Path $runtimeRoot 'config\agentdesk.yaml'
foreach ($file in @($binary, $configuration)) {
    if (-not (Test-Path -LiteralPath $file -PathType Leaf)) { throw "Required runtime file is missing: $file" }
}
foreach ($scope in @('User', 'Machine')) {
    $override = [Environment]::GetEnvironmentVariable('AGENTDESK_ROOT_DIR', $scope)
    if ($override -and [IO.Path]::GetFullPath($override) -ne [IO.Path]::GetFullPath($runtimeRoot)) {
        throw 'AGENTDESK_ROOT_DIR overrides the standard per-user runtime. Resolve that conflict before installing this task.'
    }
    if ([Environment]::GetEnvironmentVariable('AGENTDESK_CONFIG', $scope)) {
        throw 'AGENTDESK_CONFIG overrides config discovery. Resolve that conflict before installing this task.'
    }
}
$identity = [Security.Principal.WindowsIdentity]::GetCurrent().Name
$principal = New-ScheduledTaskPrincipal -UserId $identity -LogonType Interactive -RunLevel Limited
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $identity
# Cap consecutive failures at roughly sixteen hours of one-minute retries.
$restartAttemptLimit = 999
$restartInterval = New-TimeSpan -Minutes 1
$settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -RestartCount $restartAttemptLimit `
    -RestartInterval $restartInterval -ExecutionTimeLimit ([TimeSpan]::Zero) `
    -MultipleInstances IgnoreNew -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries

function Install-OwnedTask {
    param([string]$Name, $Action, [string]$Description)
    $existing = Get-ScheduledTask -TaskName $Name -ErrorAction SilentlyContinue
    if ($existing) {
        $actualOwner = [Security.Principal.NTAccount]::new($identity).Translate([Security.Principal.SecurityIdentifier]).Value
        $storedOwner = $existing.Principal.UserId
        if ($storedOwner -ne $identity -and $storedOwner -ne $actualOwner) {
            throw "Task $Name belongs to another principal."
        }
        if ($existing.Actions.Count -ne 1 -or $existing.Actions[0].Execute -ne $Action.Execute) {
            throw "Task $Name already runs a different executable."
        }
    }
    if ($PSCmdlet.ShouldProcess($Name, 'Register current-user runtime task')) {
        Register-ScheduledTask -TaskName $Name -Action $Action -Principal $principal `
            -Trigger $trigger -Settings $settings -Description $Description -Force | Out-Null
    }
}

if ($DatabaseSshAlias) {
    $ssh = (Get-Command ssh.exe -ErrorAction Stop).Source
    $tunnelName = "$TaskName-DatabaseTunnel"
    # Three unanswered probes detect a dead tunnel within about 45 seconds.
    $keepAliveIntervalSeconds = 15
    $keepAliveMissLimit = 3
    $arguments = "-N -T -o BatchMode=yes -o StrictHostKeyChecking=yes -o ExitOnForwardFailure=yes -o ServerAliveInterval=$keepAliveIntervalSeconds -o ServerAliveCountMax=$keepAliveMissLimit -L 127.0.0.1:${DatabaseLocalPort}:127.0.0.1:${DatabaseRemotePort} $DatabaseSshAlias"
    $action = New-ScheduledTaskAction -Execute $ssh -Argument $arguments -WorkingDirectory $env:USERPROFILE
    Install-OwnedTask $tunnelName $action 'AgentDesk PostgreSQL tunnel; uses the current user SSH config and pinned known host.'
    if ($Start -and $PSCmdlet.ShouldProcess($tunnelName, 'Start database tunnel')) { Start-ScheduledTask -TaskName $tunnelName }
}
$action = New-ScheduledTaskAction -Execute $binary -Argument 'dcserver' -WorkingDirectory $runtimeRoot
Install-OwnedTask $TaskName $action 'AgentDesk runtime under the signed-in user. Requires an interactive logon; credentials remain in the user profile.'
if ($Start -and $PSCmdlet.ShouldProcess($TaskName, 'Start AgentDesk runtime')) { Start-ScheduledTask -TaskName $TaskName }
Write-Output "Runtime task: $TaskName; user: $identity; root: $runtimeRoot"
