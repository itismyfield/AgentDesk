#Requires -Version 5.1
#Requires -RunAsAdministrator
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    # Explicit because the administrator may be a different Windows account.
    [Parameter(Mandatory = $true)]
    [string]$RuntimeRoot,
    [Parameter(Mandatory = $true)]
    [System.Net.IPAddress[]]$LeaderAddress,
    [ValidateRange(1, 65535)]
    [int]$Port = 8791
)

$ErrorActionPreference = 'Stop'
$binary = [IO.Path]::GetFullPath((Join-Path $RuntimeRoot 'bin\agentdesk.exe'))
if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) { throw "Worker executable is missing: $binary" }
if (-not $LeaderAddress.Count) { throw 'At least one leader IP address is required.' }
$addresses = @($LeaderAddress | ForEach-Object { $_.ToString() } | Sort-Object -Unique)
$name = "AgentDeskWorker-TCP-$Port"
$existing = Get-NetFirewallRule -Name $name -ErrorAction SilentlyContinue
if ($existing) {
    $application = $existing | Get-NetFirewallApplicationFilter
    if ($existing.Group -ne 'AgentDesk' -or $application.Program -ne $binary) {
        throw "Rule $name belongs to a different installation."
    }
}
$rule = @{
    Name = $name
    DisplayName = "AgentDesk worker API ($Port)"
    Group = 'AgentDesk'
    Description = 'Leader-to-worker API access scoped to the configured leader IP addresses.'
    Direction = 'Inbound'
    Action = 'Allow'
    Enabled = 'True'
    Protocol = 'TCP'
    LocalPort = $Port
    Program = $binary
    RemoteAddress = $addresses
    Profile = 'Any'
    EdgeTraversalPolicy = 'Block'
}
if ($PSCmdlet.ShouldProcess("$binary TCP/$Port from $($addresses -join ', ')", 'Allow leader access to worker API')) {
    if ($existing) {
        Set-NetFirewallRule @rule | Out-Null
    } else {
        New-NetFirewallRule @rule | Out-Null
    }
}
