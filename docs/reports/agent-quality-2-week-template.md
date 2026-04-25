# Agent Quality — 2-Week Observation Report (Template)

Filled at T + 14 days after [#1105](https://github.com/itismyfield-org/agentdesk/issues/1105)
("docs/agent-quality.md + tuning mapping" / 911-5) lands on `main`. Rename
the rendered file to `agent-quality-<yyyy-mm-dd>.md` and place it next to the
other observation reports under `docs/reports/`.

This template mirrors the structure of `cost-efficiency-908.md` and the
storage-retention 2-week report, so operators get a consistent shape across
campaigns.

---

## Header

| Field             | Value |
|-------------------|-------|
| Report period     | TBD (YYYY-MM-DD → YYYY-MM-DD) |
| Author            | TBD |
| Issue             | #1105 (campaign R4 / 911-5) |
| Merge commit      | TBD |
| Total agents observed | TBD |
| Total turns observed  | TBD |
| Total prompt edits    | TBD |

---

## 1. Real-Data Presence

How many agents had a meaningful sample (denominator ≥ 5) for each metric over
the 7d window during the report period?

| Metric                  | Agents with N ≥ 5 (7d) | Agents below threshold | Coverage % |
|-------------------------|------------------------|------------------------|------------|
| Turn success            | TBD                    | TBD                    | TBD        |
| Review pass             | TBD                    | TBD                    | TBD        |
| Dispatch completion     | TBD                    | TBD                    | TBD        |
| Recovery burden         | TBD                    | TBD                    | TBD        |
| Stream stability        | TBD                    | TBD                    | TBD        |

Per-agent breakdown (top 10 by total turns):

| Agent ID | Turns 14d | Turn-success 7d | Review-pass 7d | Recovery burden 7d |
|----------|-----------|-----------------|----------------|--------------------|
| TBD      | TBD       | TBD             | TBD            | TBD                |

---

## 2. False-Positive Rate

Tracks whether the alert thresholds in `docs/agent-quality.md` §4 fire on
real regressions or noise.

| Alert ID                 | Fired | Resolved-by-prompt-fix | Resolved-by-runtime-fix | Self-resolved (no action) | False-positive % |
|--------------------------|-------|------------------------|-------------------------|---------------------------|------------------|
| `quality.turn_drop`      | TBD   | TBD                    | TBD                     | TBD                       | TBD              |
| `quality.turn_critical`  | TBD   | TBD                    | TBD                     | TBD                       | TBD              |
| `quality.review_drop`    | TBD   | TBD                    | TBD                     | TBD                       | TBD              |
| `quality.recovery_high`  | TBD   | TBD                    | TBD                     | TBD                       | TBD              |
| `quality.regression`     | TBD   | TBD                    | TBD                     | TBD                       | TBD              |

Definitions:

- **Self-resolved**: predicate cleared without operator action and we judged
  in retrospect that no real regression existed (sleepy week, vacation
  pattern, sample-size hovering near 5).
- **False-positive %**: `self_resolved / fired`.

Action: if any row is above 50% false-positive, file a follow-up to retune
the predicate or hysteresis.

---

## 3. Self-Feedback Impact

Did self-feedback injection (911-3) actually shift behavior?

| Quantity                                            | Value |
|-----------------------------------------------------|-------|
| Total turns with self-feedback block injected       | TBD   |
| Total turns without self-feedback (fallback path)   | TBD   |
| Avg turn success rate, injected cohort              | TBD   |
| Avg turn success rate, fallback cohort              | TBD   |
| Difference (injected − fallback), pp                | TBD   |
| p-value (or "insufficient sample to test")          | TBD   |

Notes:

- Mean comparison is biased by the fact that "fallback cohort" includes
  measurement-unavailable agents. Treat the headline number as descriptive,
  not causal.
- TBD — narrative on whether agents that received the block changed their
  behavior in observable ways (apology phrases, retries, escalation cadence).

---

## 4. Prompt-Commit Mapping Coverage

Validates that the `agent_prompt_commits` table is being populated and that
the 7d correlation window is producing usable data.

| Quantity                                                     | Value |
|--------------------------------------------------------------|-------|
| Prompt-edit commits in window                                | TBD   |
| Of those, rows recorded in `agent_prompt_commits`            | TBD   |
| Hook coverage % (recorded / total)                           | TBD   |
| Edits whose 7d window contained ≥ 5 turns (measurable)       | TBD   |
| Edits with measurable lift (≥ +5 pp turn success)            | TBD   |
| Edits with measurable regression (≤ −5 pp turn success)      | TBD   |
| Edits with no measurable change                              | TBD   |

Top edits by absolute metric movement:

| Commit SHA | Agent | Δ turn-success 7d | Δ review-pass 7d | Notes |
|------------|-------|-------------------|------------------|-------|
| TBD        | TBD   | TBD               | TBD              | TBD   |

Action items:

- TBD — if hook coverage < 80%, file a follow-up to harden the post-commit
  recorder and backfill missing rows.
- TBD — if no edits produced a measurable lift, revisit whether 7d is the
  right window or whether the prompts being shipped are too small to move
  the needle.

---

## 5. Findings

- TBD — headline finding on data presence (do we have enough data to grade
  the system?).
- TBD — headline finding on alert quality.
- TBD — headline finding on self-feedback impact.
- TBD — headline finding on mapping table value.

---

## 6. Action Items

- [ ] TBD
- [ ] TBD
- [ ] TBD

---

## 7. Appendix: Source Queries

Place the SQL used to compute each section here, so future readers can
reproduce the numbers exactly. The base join from `agent-quality.md` §6.2 is
the starting point; per-section queries should derive from it.

```sql
-- TBD
```
