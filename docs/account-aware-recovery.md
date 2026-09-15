# Account-aware live recovery

Live recovery supports a second agent on the same provider when its effective
authentication profile differs from the owner's. The provider must support
isolated extra-account homes: Claude, Codex, Qwen, OpenCode or Grok. AGY/Gemini
OAuth account isolation is not implied by a named environment overlay.

Create and log in both accounts using the existing provider-auth-profile flow,
then reference their registered profile IDs in `org.yaml`:

```yaml
agents:
  coder:
    display_name: Coder
    provider: codex
    auth_profile: codex-work
    recovery:
      enabled: true
      fallback_agent_id: backup
      triggers: [rate_limit, idle_timeout, mailbox_stall, process_death]
  backup:
    display_name: Backup
    provider: codex
    auth_profile: codex-backup
channels:
  by_id:
    "DISCORD_CHANNEL_ID":
      agent: coder
      workspace: /path/to/project
```

Channel profile overrides belong to the channel's owner, not its fallback.
Both profile IDs are frozen into PostgreSQL recovery context. No credentials
are written to recovery state or WAL. Deleted/missing profiles fail closed.
Distinct profile IDs do not prove distinct upstream quotas: connect independent
accounts when using fallback to recover from account exhaustion.

A terminal provider error recognized as HTTP 429, rate/usage limit, exhausted
quota, or stream idle timeout saves the unfinished request and partial output,
commits a takeover intent, then wakes the existing watchdog immediately without
waiting for `stall_secs`. Permission, authentication and context-length failures
retain ordinary error handling. Recovery does not bypass tool permissions.

The executor stops the old runtime, then starts a fresh session with the backup
account, explicit agent identity, inherited workspace and checkpoint prompt.
Pending launches require their exact internal lease. While same-provider recovery
owns a channel, ordinary owner/unscoped chat/API/routine starts are refused; new
chat messages are not automatically reassigned to the fallback role by this change.

Only the original owner can initiate an error takeover. Generation fencing rejects
stale/duplicate errors. A 60-second cooldown bounds account ping-pong if the owner
is still exhausted after restoration; during cooldown normal error reporting
continues. Failure of the fallback does not select a third agent/account.

Checkpoints carry bounded request/output context and workspace continuity, not a
provider's private session state or exactly-once external side effects. Inspect
the workspace before repeating work. Deploy the updated binary, apply recovery
migrations through 0119, and grant the configured runtimes access to the channel.
This change does not provision credentials or deploy the operator's server.
