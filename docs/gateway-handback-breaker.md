# Gateway handback suppression

`cluster.gateway_handback_breaker` defaults to `enabled: true`, `window_secs: 600`,
`max_empty: 2`, and `suppress_secs: 1800`. Each decision reloads configuration.
Two empty handbacks within the window suppress voluntary handback and initial
preference grace for 30 minutes. Four empty handbacks within 24 hours, or a second
window activation within 24 hours, suppress them until manual reset. Lease
keepalive, reacquisition, backend service, and fencing on another holder continue.

State belongs to a token and survives process restarts at
`<runtime_root>/gateway_handback_breaker/<provider>-<token_hash16>.json`.
Delete that one file to reset; the next keepalive decision observes its absence
(the normal keepalive interval is 15 seconds). Corrupt or unreadable state also
suppresses handback; it is never silently replaced. A failed pending write prevents
unlock. Failed settlement is retried without counting the handback twice.

Set `enabled: false` for immediate legacy behavior at the next decision, including
when state is unreadable or manually held. Disabled decisions do not read or write
state. Turning it back on restores the saved history and suppression. An unsettled
pending handback is preserved until an acquisition observation or manual reset.
Logs emit `gateway_handback_suppressed` with provider/owner on activation and
`gateway_handback_breaker_state_error` with owner on state failure.

The first activation follows two outages with the dense rule or up to four with
the 24-hour cumulative rule. Daily outage cost is the sum over the actual N empty
handbacks (N <= 4 while enabled and state is preserved) of
`T_restart + G_eff + T_ready`. Six minutes is only the grace contribution when all
four events exhaust the default 90-second grace. The 90-second setting and the
5-second recheck interval are not completion-time guarantees; SQL and scheduling
add delay. Restart and READY durations require measurement in the target runtime.

Waiter removal is synchronous at backend exit. The next heartbeat advertises it;
the default 10-second publication interval does not bound the commit delay.
Provider-level advertisements can still cause empty handbacks across different
tokens. Suppression limits those events per token, and may delay a healthy home's
return for 30 minutes or until manual reset while the current holder keeps serving.
