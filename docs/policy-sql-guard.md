# Policy SQL Guard And Capability Manifest

This document is the source of truth for the policy SQL guard boundary and the
MVP shape of future policy capability manifests.

## Trust Boundary

Policy files under `policies/` are trusted automation, not untrusted plugins.
They are repo-reviewed JavaScript hooks that can call AgentDesk bridge APIs to
coordinate cards, dispatches, queue entries, GitHub automation, tmux sessions,
loopback HTTP, and database state.

The raw DB bridge remains available for transitional and operator-owned policy
code. The SQL guard is therefore not a full SQL sandbox. It blocks specific
state-transition writes that already have typed bridge APIs, while other raw
reads and writes remain allowed until a policy slice is migrated behind a typed
facade or a per-policy capability manifest.

## Current Runtime Guard

`src/engine/sql_guard.rs` blocks the following direct raw SQL writes through
`agentdesk.db.execute` and `ExecuteSQL` intents:

| Target | Blocked write | Required facade |
| --- | --- | --- |
| `kanban_cards.status` | `UPDATE` assigning `status` | `agentdesk.kanban.setStatus(cardId, newStatus)` |
| `kanban_cards.review_status` | `UPDATE` assigning `review_status` | `agentdesk.kanban.setReviewStatus(cardId, status, opts)` |
| `kanban_cards.latest_dispatch_id` | `UPDATE` assigning `latest_dispatch_id` | `agentdesk.dispatch.create()` or related dispatch helpers |
| `task_dispatches` | `INSERT`, `REPLACE`, `UPDATE`, `DELETE` | `agentdesk.dispatch.create()/markFailed()/markCompleted()/setRetryCount()` |
| `card_review_state` | `INSERT`, `REPLACE`, `UPDATE`, `DELETE` | `agentdesk.reviewState.sync(cardId, state, opts)` |
| `auto_queue_entries` | `INSERT`, `REPLACE`, `UPDATE`, `DELETE` | `agentdesk.autoQueue.updateEntryStatus(entryId, status, source, opts)` |

Raw DB usage is also budgeted by
`src/engine/ops/tests.rs::policies_raw_db_count_stays_within_budget`. New raw
DB callsites must migrate to a typed facade or carry a
`legacy-raw-db: policy=<name> capability=<intent> source_event=<hook>` marker
so audit logs can attribute the capability.

The current audited baseline is 195 unmarked raw DB callsites plus 3 annotated
escape-hatch callsites. This baseline records the existing trusted-automation
surface; it is not permission for silent growth.

## Additional Protected Table Candidates

These tables should be reviewed before they are added to runtime blocking. They
are not blocked today because existing policies still use raw SQL for some
transitional flows.

| Candidate | Why it is critical | Short-term direction |
| --- | --- | --- |
| `agents` | Controls provider identity, Discord channel routing, prompt paths, review routing, and assignment state. Bad writes can route work or messages to the wrong actor. | Prefer `agentdesk.agents.*` typed reads; add typed mutation APIs before blocking writes. |
| `pipeline_config` | Defines pipeline stages, transitions, and automation flow. Bad writes can bypass review or strand cards. | Treat writes as configuration changes owned by pipeline APIs or migrations. |
| `kv_meta` | Stores live runtime overrides, cooldowns, migration markers, and policy bookkeeping. Bad writes can silently change behavior across restarts. | Prefer `agentdesk.kv.*`; split high-value keys into named typed facades before table-level blocking. |
| `dispatch_outbox` and delivery tables | Own outbound Discord/API side effects and retry state. Bad writes can duplicate, suppress, or misroute messages. | Keep writes behind outbox service APIs; only allow diagnostic reads in policies. |
| `sessions` and session activity tables | Own tmux lifecycle, provider resume state, and idle cleanup decisions. Bad writes can kill or orphan active work. | Route lifecycle mutations through session/recovery services before blocking raw writes. |
| `github_*` sync/cache tables | Feed merge automation and issue/PR state decisions. Bad writes can cause stale or incorrect merge behavior. | Keep policy access read-heavy; writes should come from the GitHub sync service. |

## Capability Manifest MVP

The first manifest should be declarative and audit-oriented before it becomes a
runtime denylist. Store it next to the policy as `policies/<name>.cap.yaml`.
Example:

```yaml
version: 1
policy: merge-automation
trust: trusted-automation
source_events:
  - onCardCompleted
  - onTick
db:
  read:
    tables:
      - kanban_cards
      - task_dispatches
      - kv_meta
  write:
    tables:
      - kanban_cards.metadata
      - kv_meta
    guarded_targets:
      - task_dispatches
  raw_sql:
    mode: audited
    markers_required: true
exec:
  allow:
    - gh
    - git
http:
  loopback_only: true
session:
  kill: false
```

### Required Fields

| Field | Meaning |
| --- | --- |
| `version` | Manifest schema version. Start at `1`. |
| `policy` | Policy basename without `.js`; must match the file it describes. |
| `trust` | Must be `trusted-automation` for repo policies. Other trust levels are out of scope until untrusted policy loading exists. |
| `source_events` | Hook names or scheduler events that exercise the listed capabilities. |
| `db.read.tables` | Tables the policy can read through raw SQL or typed facades. |
| `db.write.tables` | Tables or table columns the policy can write directly or through typed facades. |
| `db.write.guarded_targets` | Guarded targets the policy must not mutate through raw SQL. |
| `db.raw_sql.mode` | `forbidden`, `audited`, or `transitional`. MVP defaults to `audited` for legacy policies. |
| `db.raw_sql.markers_required` | Whether raw SQL callsites need the `legacy-raw-db` marker. |
| `exec.allow` | Executable names allowed through the policy exec bridge. |
| `http.loopback_only` | Must be `true` for policy HTTP calls. |
| `session.kill` | Whether policy code may request session termination. |

### Enforcement Plan

1. Add manifests for the highest-risk policies with `db.raw_sql.mode:
   audited`.
2. Add a static CI check that each `agentdesk.db.*` callsite maps to a
   manifest capability and marker.
3. Migrate hot paths to typed facades listed in `docs/policy-typed-facade.md`.
4. Flip individual policies or slices from `audited` to `forbidden` once typed
   facades cover their required behavior.
5. Add runtime manifest loading only after CI has made drift visible; runtime
   denial should follow the manifest instead of growing ad hoc regexes.
