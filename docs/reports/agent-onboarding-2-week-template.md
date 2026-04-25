# Agent Onboarding — 2-Week Observation Report (Template)

Filled at T + 14 days after [#1111](https://github.com/itismyfield-org/agentdesk/issues/1111)
("docs/agent-onboarding.md + E2E smoke test + 2-week report template" / 912-6)
lands on `main`. Rename the rendered file to
`agent-onboarding-<yyyy-mm-dd>.md` and place it next to the other observation
reports under `docs/reports/`.

This template mirrors the structure of `agent-quality-2-week-template.md` and
`cost-efficiency-908.md` so operators get a consistent shape across campaigns.

Reference: [`docs/agent-onboarding.md`](../agent-onboarding.md).

---

## Header

| Field                   | Value |
|-------------------------|-------|
| Report period           | TBD (YYYY-MM-DD → YYYY-MM-DD) |
| Author                  | TBD |
| Issue                   | #1111 (campaign R4 / 912-6) |
| Merge commit            | TBD |
| Total wizard runs       | TBD |
| Total agents created    | TBD |
| Total duplicates run    | TBD |
| Total archive / unarchive cycles | TBD |

---

## 1. Onboarding Failure Rate

How often did `/api/agents/setup` end with a non-2xx response?

| Quantity                                       | Value |
|------------------------------------------------|-------|
| Total `setup` calls (dry_run=false)            | TBD   |
| 2xx responses                                  | TBD   |
| 5xx responses with non-empty `rolled_back[]`   | TBD   |
| 4xx responses (validation / conflict)          | TBD   |
| Rollback rate (`rolled_back > 0` / total)      | TBD   |
| Wizard-attributed retries (same body, ≤ 5 min apart) | TBD |

Action threshold: if rollback rate > 5%, file a follow-up to harden the
failing step (most likely `db_seed` due to channel-binding races, or
`skill_mapping` due to manifest drift).

---

## 2. Rollback Occurrence By Step

Which step was the rollback initiated from?

| Step initiator     | Count | % of all rollbacks | Most common cause |
|--------------------|-------|--------------------|-------------------|
| `agentdesk_yaml`   | TBD   | TBD                | TBD               |
| `prompt_file`      | TBD   | TBD                | TBD               |
| `workspace_seed`   | TBD   | TBD                | TBD               |
| `db_seed`          | TBD   | TBD                | TBD               |
| `skill_mapping`    | TBD   | TBD                | TBD               |

For each row with count > 0 attach a one-line postmortem (commit, log
excerpt, ticket).

---

## 3. Wizard vs Legacy CLI Usage

`/api/onboarding/*` is one-shot per host, but operators can still bypass the
agent setup wizard by editing `agentdesk.yaml` directly or by using the
deprecated preview-bridge CLI. We track the ratio to see whether the doc /
wizard / API are doing their job.

| Surface                              | Calls (period) | Agents created | Failure rate |
|--------------------------------------|----------------|----------------|--------------|
| `/api/agents/setup` (wizard)         | TBD            | TBD            | TBD          |
| Manual `agentdesk.yaml` edit         | TBD            | TBD            | TBD          |
| Legacy preview-bridge CLI            | TBD            | TBD            | TBD          |

Detection:

- Wizard calls — count `created.audit_log` files under
  `~/.adk/release/config/.audit/agent-setup-*.json`.
- Manual edits — diff `agentdesk.yaml` against the audit log (added entries
  with no matching audit file).
- Legacy CLI — grep `dcserver.stdout.log` for the deprecated subcommand.

Action threshold: if legacy CLI usage > 10% of total agent additions,
investigate **why** the wizard was rejected (UX bug, channel rate-limit,
restricted permissions).

---

## 4. Time-To-First-Message

How long from agent creation until the first relayed Discord message?

| Quantity                                         | Value |
|--------------------------------------------------|-------|
| Median (p50)                                     | TBD   |
| 90th percentile (p90)                            | TBD   |
| Worst observed                                   | TBD   |
| Agents that never sent a message in the period   | TBD   |

Definition: `first_message_at - created_at` where `created_at` comes from
the agent's audit log and `first_message_at` is the earliest
`message_outbox` row whose `target_channel` matches one of the agent's
configured channels.

---

## 5. Archive / Unarchive Cycle Health

| Quantity                                              | Value |
|-------------------------------------------------------|-------|
| Archives initiated                                    | TBD   |
| Archives rejected (`409` due to active turn)          | TBD   |
| Archives that snapshotted `discord_action != none`    | TBD   |
| Unarchives initiated                                  | TBD   |
| Unarchives whose restored config diffs from snapshot  | TBD   |
| Median archive → unarchive interval                   | TBD   |

Action threshold: any non-zero "restored config diffs from snapshot" row
indicates `agent_archive` snapshot loss; file an immediate fix.

---

## 6. Duplicate Sensitive-Field Audit

Verifies the duplicate-time deny-list documented in
`docs/agent-onboarding.md` §6.3.

| Forbidden field in body | Times observed | Times leaked into new row |
|-------------------------|----------------|---------------------------|
| `id` / `agent_id`       | TBD            | TBD                       |
| `discord_channel_id`    | TBD            | TBD                       |
| `token`                 | TBD            | TBD                       |
| `api_key`               | TBD            | TBD                       |
| `system_prompt`         | TBD            | TBD                       |

Any "Times leaked" > 0 is a security regression — file a P0.

---

## 7. Findings

- TBD — headline finding on rollback rate.
- TBD — headline finding on legacy CLI fallback frequency.
- TBD — headline finding on time-to-first-message.
- TBD — headline finding on archive/unarchive health.
- TBD — headline finding on duplicate sensitive-field audit.

---

## 8. Action Items

- [ ] TBD
- [ ] TBD
- [ ] TBD

---

## 9. Appendix: Source Queries

Place the SQL / shell used to compute each section here so future readers
can reproduce the numbers exactly.

```sql
-- Onboarding failure rate (Postgres)
-- TBD
```

```bash
# Wizard audit file count
ls -1 ~/.adk/release/config/.audit/agent-setup-*.json | wc -l
```

```bash
# Time-to-first-message — pseudo
# join agent_archive (or agent created_at) with message_outbox.first_at
# TBD
```
