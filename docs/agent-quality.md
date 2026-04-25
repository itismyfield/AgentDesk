# Agent Quality Measurement & Tuning Loop

Operator + agent-author guide for the AgentDesk *agent quality* feedback loop:
how the five canonical metrics are defined and computed, when the sample is too
small to act on, what alerts fire, how the runtime threads self-feedback back to
the agent that produced the work, and how prompt edits are correlated with
downstream metric movement.

Tracking issue: [#1105](https://github.com/itismyfield-org/agentdesk/issues/1105)
("docs/agent-quality.md + 튜닝 mapping 구조 준비" — campaign R4 / 911-5).

Prerequisite context:

- [#930  / 911-1] `agent_quality_event` schema + 5+ emit points + `/api/quality/events`.
- [#1101 / 911-2] `agent_quality_daily` rollup + hourly maintenance job + 7d/30d windows.
- [#1102 / 911-3] Public quality API: `/api/agents/{id}/quality`, `/ranking`, self-feedback hooks.
- [#1103 / 911-4] Alert rules wired to quality thresholds (referenced from §4 below).

The contract: **every agent prompt change is a hypothesis. This doc tells you
how the runtime tests the hypothesis automatically over the next 7 days.**

---

## 1. Goal

Close the loop between *prompt edit* and *measured downstream behavior* without
relying on operator memory. Specifically:

1. Make every quality metric deterministic, computable from `agent_quality_event`
   alone, and reproducible from the daily rollup table.
2. Refuse to grade an agent on data that isn't statistically meaningful — a low
   sample size is a *measurement gap*, not a "100% success rate".
3. When a prompt edit is committed, automatically attach the next 7 days of
   metric movement to that commit so the agent author can see whether their
   change helped, hurt, or did nothing.
4. Feed the agent its own scorecard in-band (self-feedback) so behavior tuning
   does not require human intervention on every iteration.

Out of scope for 911-5: human review queues, A/B prompt routing, model-level
attribution. Those land in later campaigns and reuse the mapping table this
doc sets up.

---

## 2. The Five Metrics

Each metric is a ratio of counted events. Source counts come from
`agent_quality_event` via `agent_quality_daily` (rolled up hourly). Windows are
1d (today), 7d (campaign-default), and 30d (long-horizon stability check).

| # | Metric                  | Numerator                          | Denominator                          | Semantics                                                                  |
|---|-------------------------|------------------------------------|--------------------------------------|----------------------------------------------------------------------------|
| 1 | **Turn success rate**   | `turn_complete` events             | `turn_complete + turn_error` events  | "When the agent took a turn, did it finish without throwing?"              |
| 2 | **Review pass rate**    | `review_pass` events               | `review_pass + review_fail` events   | "When TD/PD reviewed work, did it pass on first submission?"               |
| 3 | **Dispatch completion** | `dispatch_completed` events        | `dispatch_dispatched` events         | "When the agent was dispatched a card, did it actually complete it?"       |
| 4 | **Recovery burden**     | `recovery_fired + escalation`      | total turns (`turn_start`)           | "How often does this agent need an outside hand to keep going?" (lower is better) |
| 5 | **Stream stability**    | `stream_reattached + watcher_lost` | total turns (`turn_start`)           | "How often does the runtime have to re-glue the agent's session?" (lower is better) |

### 2.1 Calculation Formulas

For all metrics, `N` is the chosen window (1, 7, or 30 days):

```text
turn_success_rate_Nd     = turn_success_count_Nd / (turn_success_count_Nd + turn_error_count_Nd)
review_pass_rate_Nd      = review_pass_count_Nd  / (review_pass_count_Nd  + review_fail_count_Nd)
dispatch_completion_Nd   = dispatch_completed_Nd / dispatch_dispatched_Nd
recovery_burden_Nd       = (recovery_fired_Nd + escalation_Nd) / turn_start_Nd
stream_stability_Nd      = (stream_reattached_Nd + watcher_lost_Nd) / turn_start_Nd
```

`agent_quality_daily` materializes columns 1 and 2 directly (`turn_success_rate_7d`,
`review_pass_rate_7d`, plus 30d siblings). Metrics 3-5 are derived on-the-fly
in `/api/agents/{id}/quality` from the same daily roll-up — they do not need a
schema change because the underlying counts are already partitioned by event
type.

### 2.2 Display Convention

Higher-is-better metrics (1, 2, 3) are rendered as percentages with one decimal
(`94.3%`). Lower-is-better metrics (4, 5) are rendered as a "per-100-turns"
rate (`recovery_burden_7d = 3.1` means 3.1 recoveries per 100 turns). Both
signs are normalized at the API boundary; stored values stay raw ratios.

---

## 3. Sample Size Guard

### 3.1 The rule

A metric is **measurement-unavailable** when the relevant denominator over
window N is `< 5`. The daily rollup writes `measurement_unavailable_7d = TRUE`
and `measurement_unavailable_30d = TRUE` exactly for this case (see
`migrations/postgres/0013_agent_quality_daily.up.sql`, columns 24 and 34).

API responses surface this as `"measurement_unavailable": true` and the rate
fields are `null`. Dashboards render this as a **dash (—) with a "low sample"
tooltip**, never as `0%` or `100%`. Alerts (§4) suppress firing on
measurement-unavailable rows.

### 3.2 Why 5

A denominator of 5 lets a single failure show as 80% — coarse, but not
catastrophically misleading. Below 5, a single event flips the rate by ≥ 20
percentage points, which produces high-amplitude noise that destroys both
trend lines and alert hysteresis. We chose the threshold by replaying the
month preceding 911-1 against several thresholds (3, 5, 8, 10) and picking the
smallest value where the false-alert rate dropped under 1 / agent / week.

### 3.3 What the agent sees

When self-feedback (§5) runs and the metric is unavailable, the system message
to the agent says **"insufficient sample"** rather than printing `null`. The
agent prompt should not interpret a missing rate as a failure.

---

## 4. Alert Rules

Alerts fire from `agent_quality_daily` after the hourly rollup. Each rule is a
predicate over the 7d or 30d window plus the sample-size guard.

| ID                      | Window | Predicate                                                     | Severity | Suppress When                         |
|-------------------------|--------|---------------------------------------------------------------|----------|---------------------------------------|
| `quality.turn_drop`     | 7d     | `turn_success_rate_7d < 0.85`                                 | warn     | `measurement_unavailable_7d = TRUE`   |
| `quality.turn_critical` | 7d     | `turn_success_rate_7d < 0.70`                                 | error    | `measurement_unavailable_7d = TRUE`   |
| `quality.review_drop`   | 7d     | `review_pass_rate_7d < 0.60`                                  | warn     | `measurement_unavailable_7d = TRUE`   |
| `quality.recovery_high` | 7d     | `recovery_burden_7d > 0.05`                                   | warn     | `turn_start_7d < 5`                   |
| `quality.regression`    | 7d→30d | `turn_success_rate_7d < turn_success_rate_30d − 0.10`         | warn     | either window unavailable             |

Wiring into the alert dispatch path is owned by **#1103 (911-4)**. This doc is
the source of truth for *thresholds and predicates* — alert delivery, dedup,
and quiet hours are described in `docs/alerts/agent-quality-alerts.md` (created
under 911-4). When tuning a threshold here, edit the row above first, then
file a follow-up to bump the predicate in the alert config.

### 4.1 Hysteresis

Each alert clears only after **2 consecutive hourly rollups** show the
predicate false. This prevents flap on agents that hover at the threshold —
particularly relevant for `quality.regression`, where a single good turn can
move 7d above 30d momentarily.

---

## 5. Self-Feedback Intent

### 5.1 What it is

Before an agent's turn starts, the runtime injects a *self-feedback block* into
the system prompt. The block is computed from `/api/agents/{id}/quality` for
the requesting agent and contains the five metrics with 7d windows, plus a
short narrative ("turn success dropped 8 pp vs last week — consider reviewing
recent error patterns").

This is **not punishment**. It is the same data a human author would consult
before editing the prompt, surfaced in-band so the agent can self-correct
across turns without an explicit human intervention loop.

The injection point and exact format are owned by **#1102 (911-3)**. The
contract from this doc's side: the metrics in §2 are the *only* metrics
allowed in the self-feedback block. Adding new ones requires extending the
mapping table in §6 first.

### 5.2 Why it can fail safely

If the quality rollup is stale (last `computed_at > 90 minutes`), the runtime
omits the self-feedback block entirely rather than injecting outdated numbers.
If `measurement_unavailable_7d` is true, the block prints "insufficient sample
— focus on baseline behavior" instead of metrics. Both fallbacks are silent
from the agent's perspective; they show up in operator logs only.

### 5.3 Observation requirement

We do not yet know whether self-feedback nudges agents toward better outcomes
or merely toward *gaming the metrics*. The 2-week observation report (§7)
explicitly tracks `self_feedback_injections / day` and asks whether the
agents whose prompts were edited in the same window improved more than the
control set. Treat all v1 self-feedback claims as "preliminary" until that
report lands.

---

## 6. Prompt Commit → Metric Mapping

### 6.1 The structure

When an agent's prompt file changes, we want to know "did the next 7 days of
metrics move in the right direction?" That requires a stable join key
between *the commit that changed the prompt* and *the daily rollup rows that
follow*.

The join key is `(agent_id, commit_sha)` recorded in `agent_prompt_commits`
(see `migrations/postgres/0021_agent_prompt_commits.sql`). One row per
prompt-edit commit per agent. The `committed_at` timestamp anchors the window:

```text
window_start = agent_prompt_commits.committed_at
window_end   = window_start + INTERVAL '7 days'
```

The correlation row is then any `agent_quality_daily.day` such that
`day BETWEEN window_start::date AND window_end::date` for the same `agent_id`.

### 6.2 Mapping table semantics

```text
┌──────────────────────────────┐         ┌─────────────────────────────┐
│ agent_prompt_commits         │ 1 ─── n │ agent_quality_daily         │
│  (agent_id, commit_sha)      │         │  (agent_id, day)            │
│  committed_at TIMESTAMPTZ    │         │  turn_success_rate_7d, …    │
└──────────────────────────────┘         └─────────────────────────────┘
                join: agent_id
                + day ∈ [committed_at, committed_at + 7d]
```

A SQL sketch of the correlation query:

```sql
SELECT
    c.commit_sha,
    c.committed_at,
    d.day,
    d.turn_success_rate_7d,
    d.review_pass_rate_7d,
    d.measurement_unavailable_7d
FROM agent_prompt_commits c
JOIN agent_quality_daily d
  ON d.agent_id = c.agent_id
 AND d.day BETWEEN c.committed_at::date
                AND (c.committed_at + INTERVAL '7 days')::date
WHERE c.agent_id = $1
ORDER BY c.committed_at DESC, d.day ASC;
```

This is intentionally a **read-side join**, not a materialized correlation
table. We don't yet have enough commits to know which correlation summaries
matter (mean delta, peak delta, time-to-peak, etc.) — the 2-week report (§7)
is what tells us. Once we know the right summary, a follow-up issue can
materialize it.

### 6.3 Recording commits

The `agent_prompt_commits` row is written by the post-commit hook documented
in `docs/source-of-truth.md` §prompts whenever a file under
`adk-config/shared/agents/<agent_id>/system-prompt*.md` changes. The hook
takes `(agent_id, commit_sha)` from the commit metadata; `committed_at` is
the commit timestamp (via `NOW()` default if the hook omits it).

Ad-hoc backfill is supported — `INSERT ... ON CONFLICT DO NOTHING` is
idempotent on the composite primary key.

### 6.4 What this enables

1. The agent quality dashboard can show "since last prompt edit, turn success
   moved from 88% → 92% over 7 days" inline with the prompt diff.
2. The 2-week observation report can answer "of N prompt edits this campaign,
   how many produced a measurable lift vs how many were no-ops or
   regressions?"
3. Future A/B routing (out of scope here) can use the same table to evaluate
   competing prompt variants.

---

## 7. 2-Week Observation Report

After this issue ships, the operator runs the same 2-week observation cadence
used for storage retention (#1094) and cost efficiency (#908). The template
lives at
[`docs/reports/agent-quality-2-week-template.md`](reports/agent-quality-2-week-template.md)
and answers four questions:

1. **Real data presence.** How many agents accumulated ≥ 5 turns / 7 days?
   Below this we have no data, and that is itself the headline.
2. **False-positive rate.** Of all alerts fired, how many were resolved by
   "this agent is just sleepy this week" vs an actual prompt or runtime fix?
3. **Self-feedback impact.** Did agents that received self-feedback blocks
   move differently than the control set?
4. **Mapping table coverage.** Of all prompt edits in the window, how many
   landed an `agent_prompt_commits` row, and how many of those correlate with
   a measurable metric movement?

Fill in the template at T+14 days post-merge and put the rendered report at
`docs/reports/agent-quality-<yyyy-mm-dd>.md`.

---

## 8. Related Documents

- [`docs/source-of-truth.md`](source-of-truth.md) — canonical edit paths for
  agent prompts and the post-commit hook that writes `agent_prompt_commits`.
- [`docs/storage-retention.md`](storage-retention.md) — sets the 90-day
  retention window for `agent_quality_event` (raw events) which underpins the
  daily rollup that this doc grades.
- [`migrations/postgres/0012_agent_quality_event.sql`](../migrations/postgres/0012_agent_quality_event.sql) — raw event schema.
- [`migrations/postgres/0013_agent_quality_daily.up.sql`](../migrations/postgres/0013_agent_quality_daily.up.sql) — daily rollup with `measurement_unavailable_7d`/`30d` flags.
- [`migrations/postgres/0021_agent_prompt_commits.sql`](../migrations/postgres/0021_agent_prompt_commits.sql) — prompt-commit mapping table introduced by this issue.
- [`docs/reports/agent-quality-2-week-template.md`](reports/agent-quality-2-week-template.md) — observation report template.
