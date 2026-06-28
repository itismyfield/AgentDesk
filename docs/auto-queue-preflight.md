# Auto-Queue Sandbox Preflight

Run the MVP fixture-mode preflight for a selected repo/group/pipeline fixture:

```bash
scripts/e2e/auto-queue-preflight.sh \
  --fixture tests/fixtures/auto-queue-preflight/basic.json \
  --report /tmp/agentdesk-auto-queue-preflight.json
```

The default fixture uses repo `agentdesk/preflight-fixture`, group
`sandbox-auto-queue`, and pipeline `repo-group-pipeline-fixture-v1`.

The harness starts an in-process API router backed by a temporary PostgreSQL
database, then exercises:

- `POST /api/queue/generate`
- `POST /api/queue/dispatch-next`
- `GET /api/queue/status`
- `GET /api/queue/history`

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

The JSON report includes the run id, entry ids, dispatch ids, slot ids,
phase-gate state, repo/group/pipeline identity, terminal statuses, endpoint
observations, production-safety counters, and raw failure reasons. The harness
fails on split-brain completion (`task_dispatches.status=completed` while the
matching queue entry/run did not advance), reserved slots, entries stuck in
`dispatched`, blocking phase gates without a visible reason, or diagnostics
that omit correlation ids.

Requirements: the same local PostgreSQL test environment used by the repo's
Postgres-backed tests (`POSTGRES_TEST_DATABASE_URL_BASE` or `PG*` variables).
