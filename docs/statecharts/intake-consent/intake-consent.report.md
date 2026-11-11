# Intake consent authority — S4-07 PLAN

Code SHA: `08e2336e47986d70d0cbe95e56215858377aed5c`.
Compared main: `1fe3a57d266e1e7fe384796d22a925076f1d99fc` (fetch 2026-11-11).
Model investigation/draft/refinement: 2026-11-11 04:29–04:53 UTC, within 30 minutes.

`SC_RUN=mac-needed`. The specified WSL runner is absent. No SANY/TLC, state counts,
temporal results, generated graph, or passing model verdict is claimed. The chart is
hand-authored from the design. expected.tsv rows are predictions for the Mac run,
not measurements. Repository config/manifest parsing is separate evidence only.

## Existing models and scope

Read `execution-authority/ExecutionAuthority.tla` (permit/lease refresh, owner change
and custody contention), `input-single-path/InputPath.tla` (queue, first-key stop,
restart/boot), and `InputNativeStop.tla` (native command/absorption authority). A search
for `consent|before.?effects|before.?record` in these two areas' TLA/Markdown found no
consent handoff or before-record action. Their existing permit/native-stop/queue
transitions are not replaced. The root index and expected manifest register this new
area. [Design, source classification and exhaustive entry-point tables](design.md)
fix the actual boundary and every adjacent SQL/control/load path.

Current and Proposed preserve successful spawned CAS → Issue at the existing scope
→ destructive Take at executor entry → record attempt → by-value handoff and FS
observation → owner lock → row lock → exact CAS → commit. Proposed represents the
current successful-CAS control-flow fact by an opaque, linear receipt. Both configs
retain production owner transfer's coupled retirement of the old outbox row. No new
active-owner hold query, cleanup authority or shifted issuance time is proposed.

## Rules and predicted outcomes

| Rule | Current prediction | Proposed prediction | Code obligation / limit |
|---|---|---|---|
| INVARIANT `NoUnauthorizedIssue` | holds, unexecuted | holds, unexecuted | Every issued proof has successful spawned CAS history; Rust privacy/no constructor/no Clone/Deserialize require same-crate compile probes, not TLC. |
| INVARIANT `ProofConsumedOnce` | holds, unexecuted | holds, unexecuted | Destructive slot take, including mismatch; by-value handoff/rollback, normal drop and process loss dispose of one proof. A stale/dropped proof need not commit a DB change. |
| INVARIANT `NoStaleProofWrite` | holds, unexecuted | holds, unexecuted | Exact payload/source/claim/generation/status plus existing owner-transfer/row-retirement coupling; see code replay below. |
| INVARIANT `RollbackExactAndBookkeeping` | holds, unexecuted | holds, unexecuted | No record attempt before rollback; empty marker, exact live row/stamp; retry/completed/created preserved; at most one durable rearm across reclaim. |
| INVARIANT `ReleaseOnce` | holds, unexecuted | holds, unexecuted | Released marker remains; a later nonce is a receipt, never new authority. |
| PROPERTY `ProofEventuallySettles` | expected to hold | expected to hold | Weak fairness of each worker's enabled internal steps; rejection/drop/restart counts as disposal. |
| PROPERTY `TransactionEventuallyEnds` | expected to hold | expected to hold | Fair lock holder eventually commits/aborts; unbounded infrastructure outage excluded. |

`CHECK_DEADLOCK FALSE` allows terminal preservation with no next action. A stutter or
this setting is not a progress proof. Only the explicit PROPERTY formulas under Spec
make progress claims. They do not promise every held input executes or authorize retry
of a crash-stranded Spawned row.

## Candidate replay against production callers

C-owner candidate: FS observation matches, then owner alone is superseded while the
old Spawned outbox row is unchanged, then hold writes its marker. The lower-level
`owner_record.rs:363–448` transfer helper permits this if called alone, and hold has
no direct active-owner query (`consent_handoff.rs:255–310`).

**Production replay blocks that candidate.** Its production caller is
`owner_record/custody.rs:133–210`: on the same channel lock/transaction it first runs
`retire_open_on_tx` (:190–192), then transfers owner (:201–203). Retirement locks all
NOT replay_only pending/claimed/accepted/spawned rows (:323–324) and changes started
rows to unknown (:383–392; no-receipt branch calls
`db/replay_disposition/write.rs:217–228`). A replay-only receipt cannot be the spawned
worker row: `0136_request_replay_disposition.sql:15–25` constrains it to unknown, and
`write/start.rs:151–158` inserts unknown. Thus hold's `status='spawned'` CAS refuses the
old proof after a production owner switch commits. Main at the SHA above has the same
coupling. This already-satisfied safety boundary is preserved, not reimplemented.

No production bug or actual TLC counterexample is claimed. OwnerOnly removes the
coupling and is a diagnostic mutant. A future production owner-only caller would
invalidate this model assumption and require separate design. Existing raw-SQL/test
owner changes do not establish that production reachability. The existing
`requires_exact_payload_source_and_live_owner_pg` test changes owner *after hold*
and checks release (`consent_handoff_tests.rs:185–255`); it does not by itself prove
the upstream retirement/hold boundary.

Required preservation test: pause after FS observation, run the actual production
promote transaction (retirement plus transfer), snapshot the retired row, resume stale
hold and assert no further row change. Main is expected GREEN here. Removing only
`retire_open_on_tx` from promote should make the same boundary test RED. This tests
the causal guard rather than adding a redundant live-owner hold query.

## Mutant configs (all unexecuted)

| Config | Expected failed invariant | Meaning |
|---|---|---|
| `IntakeConsentCurrent.cfg` | none | Current boundary with production retirement coupling. |
| `IntakeConsentProposed.cfg` | none | Same abstract issuance/consumption/observation/CAS ordering. |
| `IntakeConsentForge.cfg` | `NoUnauthorizedIssue` | Forge proof without spawned CAS receipt. |
| `IntakeConsentReuse.cfg` | `ProofConsumedOnce` | Second consumption after move/drop. |
| `IntakeConsentStale.cfg` | `NoStaleProofWrite` | Remove exact current-row comparison. |
| `IntakeConsentBookkeeping.cfg` | `RollbackExactAndBookkeeping` | Rollback increments retry bookkeeping. |
| `IntakeConsentOwnerOnly.cfg` | `NoStaleProofWrite` | Transfer owner without retiring its old open row. |

These are mutant hypotheses, not production exploits. Replay every real Mac trace
against the actual entry points before promoting it to a regression test. Syntax
error, timeout, missing rule or zero explored states is not expected RED.

## Abstractions and exclusions

- Two workers, one stamped row, one payload/source/claim/generation/status/marker or
  owner drift, one process loss. Normal owner drift includes row retirement atomically;
  status `failed` abstracts both terminal failure and retained unknown. Actual tuple
  values, legacy null/half stamps, digest bytes, schemas and scalar bounds need Rust/PG
  tests. Payload projection must retain 26 row fields plus one final Null (27 slots).
- `ticket` abstracts successful CAS control flow/current and a nonclone receipt/proposed;
  `cas` and `attempted` are ghost history. No production ledger/schema is added.
- Full routing/replay/operator child transactions are not composed. Other row mutators
  use bounded Drift; SQL row lock/status/claim/marker checks remain the real final fence.
  Do not assume every adjacent writer takes the owner advisory lock.
- FS load precedes begin/owner/row lock. Later FS change leaves the already-observed
  snapshot valid for this call in both configs; no cross-FS/PG transaction is claimed.
  RecordError may have published, disposes proof and never rolls back the row.
- Clean restart drains accepted work (`intake_worker.rs:366–381,550–566`). Restart
  models process loss: discard in-memory proof/receipt and uncommitted tx, preserve
  durable marker. Boot recovery replays references and never reconstructs BeforeEffects.
- Cancel/Terminate, clear, stop and native writer authority remain separate. Their
  source/owner classification and exclusions are in design.md. Retention counts outbox
  candidates, never deletes them. This change adds no delete/kill/retry authority.
- Clock rollback, digest collisions, publication internals and permanent outages are
  outside the model. Lower-level owner-only SQL/helper calls are a separate contract
  boundary; the refactor neither adds callers nor claims a universal guard against them.

## Mac handoff

Run every cfg with the existing statechart script and `intake-consent.view.mjs`, then
run `scripts/ci/statecharts_tlc.py` for this area. Save raw output, actual invariant and
PROPERTY sets, state counts and per-config/total duration. Respect the root README's
300-second/config and 15-minute/job budgets; bounds/timing are unmeasured on WSL.
Do not label Current or Proposed GREEN before actual execution. Correct predictions
from measured results; do not raise baselines to conceal a counterexample.
