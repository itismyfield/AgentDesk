# Relay Authority Warrant — Acceptance and Rollback Runbook

Source issue: #5464 (#5071 T5). Related: #5874 (cohort-width publication),
#5883 (observation-sink retention), #5902 (entry-gate cutover).

Last refreshed: 2026-09-12

> **No sign-off has been recorded. Every owner slot in [Sign-Off](#sign-off) is
> blank and marked `사용자 비준 필요`; the stage question in
> [Which Stage Governs](#which-stage-governs--undefined) is likewise unresolved.**
> This runbook documents the procedure and the evidence. It is not itself an
> acceptance, and it does not authorise a dial move today. Every coordinate below
> is pinned to `main` @ `26687f6264`.

## Scope

`runtime.relay_authority_mode` + `runtime.relay_authority_cohort_percent`
(`config.rs:1858-1865`) are the two-operand rollout dial for the #5464 AC2-R
relay-authority warrant. This runbook covers **accepting the deployed
`Enforce/100` position and rolling it back**. The promotion *formula* lives in
`scripts/relay_authority_rollout_report.py`; this document is how an operator
reads it, and what reverting actually does.

The sibling execution-identity dial is a different switch with its own runbooks
([promotion criteria](execution-identity-promotion-criteria.md),
[Enforce rollout](execution-identity-enforce-rollout.md)); nothing here applies
to it.

## The Dial

Three stages (`config.rs:1782-1798`), each gated by a mode predicate:

| Stage | `records_authority_observations` (`config.rs:1805`) | `governs_destructive_authority` (`config.rs:1812`) |
|---|---|---|
| `Legacy` | false | false |
| `Observe` | **true** | false |
| `Enforce` | **true** | **true** |

Both operands are vetoes and both shipped defaults are the denying value
(`Legacy`, `0`): `cohort::admits` (`cohort.rs:130`) is a conjunction of
`mode.consults_cohort()` (`config.rs:1822`) and
`cohort_bucket(channel_id) < effective_cohort_percent(percent)`, so either
default alone admits nobody.

**Observed stage history**, read off the archive's own cohort fingerprints
(`cohort::cohort_fingerprint`, `cohort.rs:159`; vectors pinned at
`cohort.rs:490-492`):

| Fingerprint | Position | Window observed |
|---|---|---|
| `5ce20688c105144b` | `Observe/100` | 2026-09-04T22:24 .. 2026-09-06T17:07 (100 turns, 3 days) |
| `d1d48477e7e326bd` | **`Enforce/100` — current** | 2026-09-06T17:45 .. present |
| `18a16fbe4259fa89` | `Legacy/0` — the rollback destination | — |

Live position at the time of writing:
`~/.adk/release/config/agentdesk.yaml:933-934` =
`relay_authority_mode: enforce`, `relay_authority_cohort_percent: 100`.

## Reading the Live Dial

The dial is published **only** on the authenticated detail build
(`health_api.rs:518` / `:639`, `health/snapshot.rs:1108`; the public allowlist
excludes it):

```bash
curl -s http://127.0.0.1:8791/api/health/detail \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["relay_authority_rollout"])'
```

Since #5874 (`4160e3d673`) the block carries five fields: `mode`,
`cohort_percent` (clamped, in force), `cohort_percent_configured` (YAML raw,
`cohort.rs:192`), `cohort_percent_clamped` (`cohort.rs:199`) and
`cohort_fingerprint`. **Confirm the running binary actually has them** — a
deployed binary older than that commit publishes only three, and then
"is the value I configured the value in force?" is not answerable from a poll.
Measured 2026-09-12 on the live release dcserver: three fields only.

`config_live_reload::current()` (`config_live_reload.rs:73`) is read per
decision, so a dial edit applies **without a dcserver restart**, at the next
decision.

## Acceptance

### 1. The evidence archive is read-only

`$AGENTDESK_ROOT_DIR/relay_authority/YYYY-MM-DD.jsonl` (root resolution:
`config.rs:2973`; default `~/.adk/release`). For the release host that is
`~/.adk/release/relay_authority/`.

**Read it, never write it.** Do not rewrite, truncate, line-filter, sort or
"tidy" these files. Two independent reasons: they are the sole acceptance
evidence, and the report's `line_integrity` criterion is computed over the exact
bytes on disk, so any edit silently changes the verdict. The only writer is the
runtime; the only deleter is the retention sweep — whole files, on a 30-day
floor (`authority_retention.rs:53`, swept by `:128`, called from
`authority_observation.rs:656`). That floor is const-asserted to exceed the
13-day worst-case promotion window (`authority_retention.rs:59,64`), so a
window cannot be half-deleted, but an acceptance older than 30 days has no
evidence left to re-read.

Axis-B records share these files and are **not** axis-A losses; the reader
excludes them from every axis-A denominator (`is_complete_axis_b`,
`relay_authority_rollout_report.py:208`). A reader without that exclusion scores
the same archive at 75% unusable and fails `line_integrity` spuriously — so run
the report from a checkout at or after #5883, never an older copy.

### 2. Run the report

```bash
AGENTDESK_ROOT_DIR=~/.adk/release \
  python3 scripts/relay_authority_rollout_report.py --stage 1
```

Read-only; exit status is `0` exactly when `promotion_ready` is true
(`relay_authority_rollout_report.py:778`). Criteria and floors
(`:143-168`): `window_days` ≥ 7, `turn_samples` ≥ 200 (stage 1) or ≥ 500
(stage 2), `new_stricter` == 0, `loop_exit_coverage` and `stream_coverage`
≥ 0.5, `line_integrity` ≤ 0.01. All six are judged on the **newest contiguous
fingerprint segment only**, never on the whole input.

Pass condition: all six `[PASS]` at the fingerprint you intend to accept —
confirm `target fingerprint` equals the dial you are accepting, not a stale
segment. `promotion_ready` is advisory by design (§5.3): the decision is human.

Two fields the criteria cannot see, and which a reader must check by eye: the
`evicted share` (turns that reached the log only because a successor arrived — a
low value is *not* an all-clear, it moves the wrong way under the loss it
describes) and `out_of_scope_unusable_lines` (fail-open, invisible to
`line_integrity`).

Measured 2026-09-12 at `26687f6264`, target `d1d48477e7e326bd`: stage 1 — six of
six PASS, `promotion_ready: True`, rc 0 (328 turns / 7 days, `new_stricter` 0,
coverage 0.81 / 0.83, `line_integrity` 0.0). Stage 2 — `turn_samples` 328 < 500
FAIL, `promotion_ready: False`, rc 1.

### Which Stage Governs — UNDEFINED

`--stage` selects the turn-sample floor and **defaults to 1**
(`relay_authority_rollout_report.py:144,762`). Which stage T5 acceptance is
judged at is **not defined anywhere in the repository**, and the two answers
disagree on the live archive: stage 1 passes 6/6, stage 2 fails on
`turn_samples` alone. Nothing else differs between them.

**`사용자 비준 필요` — the operator picks the stage, and this runbook must not.**
Record the choice and its rationale at sign-off. Choosing stage 2 means the
acceptance waits for ~172 more distinct turns at the current fingerprint; a dial
move before then restarts the segment and the count.

## Rollback

### The operation

Edit the live `agentdesk.yaml` (`$AGENTDESK_ROOT_DIR/config/agentdesk.yaml`)
and set the dial to the target position. Two knobs, two granularities:

- `relay_authority_mode: legacy` — stops enforcement **and** observation.
- `relay_authority_mode: observe` — stops enforcement, keeps observation.
- `relay_authority_cohort_percent: <n>` — keeps the mode, drops every channel
  whose `cohort_bucket` is ≥ `n` out of enforcement (`cohort.rs:130`).

No restart and no deploy. The next decision reads the new value.

**`observe` is not a safe halfway house for this dial.** Enforcement is
`governs_destructive_authority`, which is `matches!(self, Self::Enforce)`
(`config.rs:1812`) — so `observe` withdraws enforcement exactly as completely as
`legacy` does. It differs only in that the evidence keeps accruing. Choose it to
preserve measurement, never to soften the behavioural revert.

Do **not** roll back by deleting the two keys (see below).

### After the rollback: confirm

1. Re-poll `/api/health/detail` and confirm `mode` **and** `cohort_percent`
   equal what you intended, and that `cohort_percent_clamped` is `false`.
2. Confirm the dcserver log carries no
   `relay_authority_cohort_percent out of range; clamped to full cohort`
   (`cohort.rs:106`) for your edit.
3. **Run the §"Known Residual Risk" check below.** This is not optional.

## Known Residual Risk of the Rollback Direction

#5874 states it directly: the remaining risk of this dial is **not** wrongly
enrolling new channels, it is *"롤백하거나 다이얼을 되돌릴 때 무음으로
`Legacy/0` 으로 복귀해 `AuthorityLost` 가 부활하는 쪽"*.

**What returns.** Inside the cohort under `Enforce`, a vanished durable inflight
row is absorbed: `guarded_persist.rs:108` maps `GuardedSaveOutcome::Missing` to
`Suppressed`, leaving the turn alive and `post_loop_finalize` reachable. Outside
it, the same outcome falls through to `AuthorityLost` (`guarded_persist.rs:111`)
→ `stream_tick.rs:360` → `stream_loop.rs:892-894` breaks the loop →
`turn_bridge/mod.rs:702-706` relinquishes bridge authority, defuses the inflight
guard and **returns without running `post_loop_finalize`**, orphaning a finished
answer. The entry gate reverts with it: `bridge_entry_rowless_cohort_admits`
(`bridge_entry_persist.rs:96`) delegates to the same predicate, and outside the
cohort `:116` drops the `ContinueRowless` verdict. In the measured window that
is 34 of 328 turns (`entry old->new: end->continue_rowless: 34`).

**Why it can be silent.** Four production read sites resolve the dial with
`.unwrap_or_default()` — i.e. to `Legacy/0` — with no log line:
`cohort.rs:219` (health), `authority_observation.rs:350` (observation),
`guarded_persist.rs:79` (stream gate), and the entry gate via
`bridge_entry_persist.rs:97`. Both fields are `#[serde(default, …)]`
(`config.rs:1858,1864`), so a **deleted, misspelled or dropped key parses
clean and reads as `Legacy/0`** — no rejection, no warning. Both also carry
`skip_serializing_if`, so a whole-config rewrite by a binary that predates the
keys removes them outright; the inventory books that path at
`t5-t6-removal-inventory.md:374-385` and `:559`.

Neither existing WARN covers this. `cohort.rs:106` fires only on the *widening*
typo band `101..=255`, and `config_live_reload.rs:536-540` fires only on a
config that fails to parse — which keeps the previous dial installed
(fail-stale, `cohort.rs:211-218`). **A silent return to `Legacy/0` produces no
log line at all.** That gap is the risk; this step is the compensating control.

**Mandatory check, immediately after any rollback or dial move:**

```bash
curl -s http://127.0.0.1:8791/api/health/detail | python3 -c '
import sys,json
r = json.load(sys.stdin)["relay_authority_rollout"]
print(r)
assert r["mode"] != "legacy" or "<intended legacy>", "dial is Legacy — intended?"
assert r["cohort_percent"] != 0 or "<intended 0>", "cohort is 0 — intended?"'
grep -n "relay_authority" "$AGENTDESK_ROOT_DIR/config/agentdesk.yaml"
```

Both keys must be **present in the YAML** and the health block must report the
position you intended. `mode: legacy` / `cohort_percent: 0` read back after an
edit that did not ask for them is the failure this check exists to catch: the
keys were lost, not set. Restore them explicitly rather than assuming the file
is authoritative.

## What This Runbook Does Not Cover

- **Enforcement sites outside the two gated cells.** The dial governs the
  stream-loop `Missing` cell (`guarded_persist.rs:108`) and the entry-gate
  rowless continuation (`bridge_entry_persist.rs:116-120`). Every other
  destructive path is unchanged in every mode.
- **Repairing what a rollback already stranded.** A turn that took
  `AuthorityLost` returned before `post_loop_finalize`; re-arming the dial does
  not replay it.
- **Per-channel cohort membership.** `cohort_bucket` is FNV-1a over the channel
  id (`cohort.rs:55`); no tool prints which channels a width admits.
- **Evidence older than 30 days**, deleted by the retention sweep.

## Sign-Off

- Author: #5464 (#5071 T5) R6, 2026-09-12 — procedure and evidence documented;
  no acceptance and no GO recorded.
- Acceptance stage (1 or 2): **blank — `사용자 비준 필요`** (see
  [Which Stage Governs](#which-stage-governs--undefined)).
- Acceptance owner: **blank — `사용자 비준 필요`**.
- Rollback GO reviewer: **blank — `사용자 비준 필요`**.

Format follows the sibling
[`execution-identity-promotion-criteria.md`](execution-identity-promotion-criteria.md),
whose threshold owner is likewise `pending`.
