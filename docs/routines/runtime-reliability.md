# Routine runtime reliability: boot grace, start deferral, failure alerts

A dcserver restart registers the provider runtimes (Claude, Codex, ...) a little after the
routine runtime starts ticking. Without the guards below, routines due right after a restart
failed with `provider runtime not registered` or `agent mailbox is busy`, and with the
operator defaults (`failure_pause_auto_resume_secs: 0`, `stale_paused_alert_secs: 0`) nobody
was told.

## Keys (`routines:` in `agentdesk.yaml`)

| Key | Default | Meaning |
|---|---|---|
| `startup_grace_secs` | `120` | For this many seconds after the routine runtime boots, due routines are not claimed. Agent-turn polling and run recovery keep going. A slot missed during the grace stays due (`next_due_at` in the past) and is claimed **once** when the grace ends; missed slots never queue up because a routine holds at most one in-flight run. `0` disables the grace. |
| `max_consecutive_failures` | `3` | When a routine's consecutive `failed` runs reach exactly this count, one actionable alert (`routine-runtime` / `routine_consecutive_failures`) goes to the routine's log thread, or the agent's primary channel when it has no thread. The alert carries the routine id, the last error and the next slot. Further failures in the same streak stay quiet; any non-failed terminal run (`succeeded`, `skipped`, `paused`) resets the streak. `interrupted` runs (restart recovery) neither extend nor reset it. `0` disables the alert. |

Both keys are hot-reloadable. The alert is independent of the pause knobs:
`failure_pause_auto_resume_secs: 0` only means "do not auto-pause on terminal failure", and
`stale_paused_alert_secs: 0` only disables the long-paused reminder. Neither silences the
consecutive-failure alert.

## Agent start deferral

Before an agent-backed routine run touches its thread, reservation or run row, the executor
checks that the agent's provider runtime is registered. Two start failures are treated as
transient and do not count as a failure or spend a `max_retries` attempt:

- `provider_not_ready`: the provider runtime is not registered or not ready yet.
- `mailbox_busy`: the headless turn start returned a conflict (HTTP 409 `agent mailbox is busy`,
  or a session transition that stayed busy).

The run stays `running` with `result_json.status = "deferred"`, `deferred_reason` set to one of
the codes above, and `next_retry_at` 60 seconds out; the attempt log records a `deferred`
event. Re-attempts reuse the original attempt kind and DM target. Once 10 minutes have passed
since the first deferral, the next transient failure goes through the regular policy
(retry, fallback agent, or `failed`).
