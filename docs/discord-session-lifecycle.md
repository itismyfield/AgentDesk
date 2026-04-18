# Discord Session Lifecycle

AgentDesk separates Discord session lifecycle actions into four categories:

- `soft clear`: clear visible conversation state and provider session reuse while preserving tmux. User action such as `/clear`.
- `hard reset`: reset provider/model state and optionally recreate tmux. The runtime expresses this with `reset_provider_state` and `recreate_tmux`.
- `force-kill`: immediately terminate unhealthy runtime state. Use this for deadlock, auth failure, prompt-too-long, and comparable hard-stop reasons.
- `auto cleanup`: system-driven cleanup for stale or expired sessions. This is distinct from `force-kill` even if it shares the same low-level stop path.

Current contract:

- `force_new_session` remains as a compatibility alias for `reset_provider_state`.
- Fresh dispatch defaults for `implementation`, `review`, and `rework` mean `reset_provider_state=true` and `recreate_tmux=false`.
- `/stop`, `!stop`, `!cc stop`, `/cc stop`, and turn-cancel APIs cancel the active turn while preserving tmux.
- Idle cleanup and other system cleanup notifications use `lifecycle.auto_cleanup`, not `lifecycle.force_kill`.
- Orphan recovery requires the same orphan marker on two consecutive supervisor ticks before rollback/recovery.
- Repeated lifecycle alerts are deduped in `message_outbox` by `(target, reason_code, session_key)` within a short TTL.
