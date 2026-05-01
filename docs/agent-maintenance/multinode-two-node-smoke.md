# Multinode Two-Node Smoke

Last refreshed: 2026-05-01.

This smoke separates CI-reproducible multinode invariants from the physical
MacBook + Mac mini runtime check.

## CI Lane

Nightly workflow: `.github/workflows/ci-nightly.yml`

Job: `multinode_regression`

Coverage:

- `single leader`: two PG pools contend for
  `CLUSTER_LEADER_ADVISORY_LOCK_ID`; only one holder is accepted, then standby
  acquires after release.
- `exactly-once claim`: two workers concurrently claim the same
  `task_dispatches` row; only one claim succeeds.
- `lease reclaim`: an expired dispatch claim is reclaimed by a second worker.
- `resource lock exclusive`: two workers contend for the same
  `unreal:project:<repo>` lock; only one holder is accepted.
- `tested head merge gate`: merge automation blocks when a required phase is
  missing for the current PR head SHA.

Local command:

```bash
cargo test --bin agentdesk multinode_regression:: -- --nocapture --test-threads=1
node --test policies/__tests__/merge-automation.test.js
```

## Physical Smoke

Use this only against a dev runtime and shared dev PostgreSQL. Do not stop
`AgentDesk-*` tmux work sessions unless the operator explicitly asks.

1. Start Mac mini as leader-capable:

```bash
AGENTDESK_CLUSTER_ENABLED=true \
AGENTDESK_CLUSTER_INSTANCE_ID=mac-mini-release \
AGENTDESK_CLUSTER_ROLE=auto \
scripts/deploy-dev.sh
```

2. Start MacBook as worker:

```bash
AGENTDESK_CLUSTER_ENABLED=true \
AGENTDESK_CLUSTER_INSTANCE_ID=mac-book-release \
AGENTDESK_CLUSTER_ROLE=worker \
scripts/deploy-dev.sh
```

3. Verify node registry and role split:

```bash
curl http://localhost:8787/api/cluster/nodes
```

Expected:

- two distinct `worker_nodes` rows
- exactly one `effective_role=leader`
- MacBook/Mac mini capability surfaces differ where expected

4. Verify claim and routing diagnostics:

```bash
curl -X POST http://localhost:8787/api/cluster/task-dispatches/claim \
  -H 'content-type: application/json' \
  -d '{"claim_owner":"mac-book-release","limit":10}'
```

Expected:

- eligible rows are claimed once
- ineligible rows remain unclaimed and contain `routing_diagnostics`

5. Verify Unreal exclusivity:

```bash
curl -X POST http://localhost:8787/api/cluster/resource-locks/acquire \
  -H 'content-type: application/json' \
  -d '{"lock_key":"unreal:project:CookingHeart","holder_instance_id":"mac-mini-release","holder_job_id":"smoke-unreal"}'
```

Repeat from the second node with a different `holder_instance_id`. Expected:
second acquire returns `409` with the current holder.

6. Verify phase evidence merge gate:

```bash
curl -X POST http://localhost:8787/api/cluster/issue-specs/upsert \
  -H 'content-type: application/json' \
  -d '{"issue_id":"smoke","card_id":"smoke-card","body":"## Acceptance Criteria\n- Smoke\n\n## Test Plan\n- Smoke\n\n## Definition of Done\n- Smoke\n\n## Required Phases\n- Unreal Smoke"}'

curl -X POST http://localhost:8787/api/cluster/test-phase-runs/upsert \
  -H 'content-type: application/json' \
  -d '{"phase_key":"unreal-smoke","head_sha":"smoke-head","status":"passed","card_id":"smoke-card"}'
```

Expected:

- `/api/cluster/issue-specs?card_id=smoke-card` shows `unreal-smoke`
- `/api/cluster/test-phase-runs/evidence?phase_key=unreal-smoke&head_sha=smoke-head`
  returns the passing evidence
- merge automation blocks a different PR head SHA until fresh evidence exists

## Evidence To Record

When the physical smoke runs, record in the tracking issue or PR:

- timestamp and hosts
- `/api/cluster/nodes` summary
- exactly-once claim result
- resource lock conflict result
- phase evidence head SHA
- any skipped steps and why
