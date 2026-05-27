# TUI Relay Stabilization E2E — SUPERSEDED

> **Superseded** by [`multi-provider-e2e.md`](multi-provider-e2e.md) on
> 2026-05-28. This document describes the legacy single-pair smoke that
> targeted the `adk-dashboard-e2e` channel pair. Use the cell-based driver
> (`scripts/e2e/run_tui_relay.py --cell <cell> --channel-id <id>`) for new
> work. The legacy harness is kept executable only until
> `adk-dashboard-e2e` is archived.

Dedicated channels:

- Claude TUI: `1506295332949196840` (`AgentDesk-claude-adk-dash-cc-e2e`)
- Codex TUI: `1506295335096549406` (`AgentDesk-codex-adk-dash-cdx-e2e`)

## Required Scenarios

1. Basic turn relay
   - Send a short marker prompt to each channel.
   - Expect one visible processing placeholder while the turn is active.
   - Expect the placeholder to be edited or removed after terminal response delivery.

2. Multi-turn continuity
   - Send a second prompt asking for the previous marker.
   - Expect the provider to retain the session and answer with the previous marker.

3. Long-running idle visibility
   - Send a prompt that runs a short shell sleep or equivalent provider-visible delay.
   - Expect the processing placeholder to remain visible until the response completes.

4. Rollover / long output
   - Ask for output large enough to exercise Discord message rollover.
   - Expect no duplicate terminal response and no stale processing card.

5. Recap and clear
   - Let a resumable idle session reach recap eligibility.
   - Expect one recap card only.
   - Send a new user message and expect the old recap card to be deleted.
   - Click `[새 세션 시작]` and expect provider session id plus TUI tmux/process state to clear.

6. Noise regression checks
   - No Discord pin/unpin system noise.
   - No `No response requested` relay.
   - No post-terminal processing placeholder.

## Smoke Command

Run after deploying the candidate build to release:

```bash
scripts/e2e-tui-relay-scenarios.sh
```

The script verifies the two dedicated TUI tmux sessions are alive, sends marker
turns to both channels, waits for idle diagnostics, and scans tmux tails for
known relay noise. The recap button scenario still requires a live Discord
interaction because the button click is delivered by Discord Gateway.
