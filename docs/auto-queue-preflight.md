# Auto-Queue Sandbox Preflight

Run the fixture-mode preflight for a selected repo/group/pipeline fixture:

```bash
scripts/e2e/auto-queue-preflight.sh \
  --fixture tests/fixtures/auto-queue-preflight/basic.json \
  --report /tmp/agentdesk-auto-queue-preflight.json
```

Run the advanced #3800 sandbox scenario suite:

```bash
scripts/e2e/auto-queue-preflight.sh \
  --suite advanced \
  --report-dir /tmp/agentdesk-auto-queue-preflight-reports
```

The default fixture uses repo `agentdesk/preflight-fixture`, group
`sandbox-auto-queue`, and pipeline `repo-group-pipeline-fixture-v1`.
The advanced suite runs these independent fixture reports:

- `phase-gates.json`: blocked phase gate, repair, pass, and final gate
  completion.
- `review.json`: controlled review dispatch, direct review idempotency,
  review-decision, terminal skip/rework observations, and duplicate review
  guard.
- `multislot-recovery.json`: same-agent multislot dispatch, reorder,
  cancel/restore recovery, and terminal cleanup fallback.
- `pipeline-compatibility.json`: requested/backlog/ready/done transition
  compatibility and expected preflight failure reporting for unsupported
  transitions.

The harness starts an in-process API router backed by a temporary PostgreSQL
database, then exercises:

- `POST /api/queue/generate`
- `POST /api/queue/dispatch-next`
- `GET /api/queue/status`
- `GET /api/queue/history`
- advanced scenarios also cover `PATCH /api/queue/reorder`,
  `POST /api/queue/cancel`, `POST /api/queue/runs/{id}/restore`, and
  `POST /api/queue/runs/{id}/phase-gates/repair`

Default mode is sandbox-only. It seeds synthetic repo/card/agent rows in the
temporary database, lets `/api/queue/generate` choose the normal queue shape,
then requires `/api/queue/dispatch-next` to create a real `task_dispatches` row
and bind it to the queue entry with a slot index. Fixture kanban-card metadata
marks the run as `sandbox_preflight=true` with
`production_mutation_allowed=false`, so the dispatch creation path keeps the
canonical queue/dispatch state transitions while disabling external side
effects such as Discord channel validation, fresh worktree creation, and
dispatch-channel notification outbox rows. The harness then completes the
created dispatch through `PATCH /api/dispatches/{id}` so the real terminal sync
path advances `auto_queue_entries` and `auto_queue_runs`. It does not contact
GitHub, create PR/branch tracking rows, create worktrees, enqueue production
dispatch-channel notifications, or start live agent sessions.

Fixtures declare an `agent_mode` lane:

- `none`: no provider behavior, used for static/pipeline checks.
- `controlled`: deterministic synthetic provider behavior inside the temporary
  database only.
- `real_live`: reserved for explicit live opt-in. The harness rejects this lane
  unless `AGENTDESK_AUTO_QUEUE_PREFLIGHT_ALLOW_LIVE=1` is set.

The JSON report includes the run id, entry ids, dispatch ids, slot ids,
phase-gate state, repo/group/pipeline identity, scenario kind, `agent_mode`,
`real_provider_contacted`, terminal statuses, endpoint observations,
scenario-specific observations, expected `preflight_failure_reasons`,
production-safety counters, and raw failure reasons. The harness fails on
split-brain completion for entry-bound implementation dispatches
(`task_dispatches.status=completed` while the matching queue entry/run did not
advance), reserved slots, entries stuck in `dispatched`, blocking phase gates
without a visible reason, diagnostics that omit correlation ids, or any default
sandbox mutation of production cards, PR/branch tracking, live sessions,
dispatch delivery, channel messages, or worktree/branch context.

Requirements: the same local PostgreSQL test environment used by the repo's
Postgres-backed tests (`POSTGRES_TEST_DATABASE_URL_BASE` or `PG*` variables).
