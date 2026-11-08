# Missing-source host retry

Code baseline: `b6d8a728d88f2218fc0815a6f8159d12d704dd05`; fixed behavior is the accompanying F1 implementation. F1 Rust/PG evidence is in the lane's `report-2.md`; F2's final SHA, real Herdr/PG evidence and mutations are in `report-3.md`. The orphan extension also models F2 retirement and its conditional recovery through a subsequent normal input.

The baseline counterexample is Booting → Halted → Ended (claim returned) → Booting with no new evidence. `writer/host.rs` returns from `Hosted::serve` after a permanent halt; `start_managed` releases the settled claim; `discord/runtime_bootstrap/owner_cleanup.rs::refresh_writers` calls the host again. In the fixed path the actor and any POST have already joined, readiness and the halt alarm remain closed, and the existing host waits with the same claim. The owner refresh skips that claim.

`otherPaneLive` models a provider that is alive in another pane while the persisted row still names an orphan pane. Neither its presence nor repeated observations alter this source's evidence. Both Unknown and Present initial classifications are explored. A refused reset, publication revision, repeated same identity, and passage of time are stuttering steps, not wake authority.

| Configuration | Expected violation |
|---|---|
| Before / NoHold | `NoUnchangedReboot` |
| Fixed | none; unchanged Missing stays held |
| Source / Presence / Nonce / Binding | none; one new stable identity is consumed by one restart |
| Early | none; source recovers before the halt waiter starts |
| Metadata | none; an I/O failure after fresh evidence delays the actor until a current observation is readable |
| NoWake | `EvidenceResumes` |

The checker matches the full violation set for all ten F1 configurations. `NoUnchangedReboot` bounds starts by evidence count; `HeldOwnsClaim` and `HaltVisible` keep admission closed; `RetiredCannotBoot` preserves execution retirement; `EvidenceResumes` requires eventual consumption of new evidence unless stopped. Source validity and provider publication are external inputs. Empty binding reads and repeated publications stutter without a new durable identity. Weak fairness applies to the actor/host actions, including wake. The Metadata case assumes its read outage eventually recovers; no evidence arrival or provider exit is forced. Rust can remember the wake identity while opening the store; the model counts its consumption at the next actor start.

`Observation::starting` snapshots the producer and durable binding before each actor start. `Observation::resume` compares that start's evidence after join, including an immediate exact-path check. Presence compares only Absent/Present/Unknown; only a new nonempty nonce changes incarnation. Binding compares source identity plus execution nonce, ignoring seq and timestamps after validating ordered channel/provider rows. The claim retains consumed identities across starts and preserves them through failed or empty reads. A Missing retry waits for readable current metadata before spawning; only that fresh sample supplies absence. Read errors do not count as a changed publication. The existing actor still validates reopened identity and prefix bytes.

The [before chart](missing-halt-before.chart.mmd) and [fixed chart](missing-halt.chart.mmd) are concise hand-authored projections of the checked transitions, with labels supplied by `missing-halt.view.mjs`; they are not renderer-generated reachability dumps. The optional statechart renderer is unavailable on this lane. The repository's documented plain TLC checker is used:

```sh
TLA2TOOLS=/home/adk/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/usr/bin/java \
  python3 scripts/ci/statecharts_tlc.py --root <scoped-model-root>
```

The final scoped root contains this model, its twelve configs (ten F1 and two orphan configs), and their exact `expected.tsv` rows. A final scoped run checks the committed copies. This does not claim the unrelated repository-wide models ran. Raw F1 output is retained under the lane's `logs/tag2/`; the final twelve-config check and direct orphan traces are under `logs/tag3/`.

The F1 submodel abstracts one external evidence change, one process and one claim. It does not prove disk prefix verification, OS liveness, PG publication/CAS races, gate acquisition, source selection across panes, or delivery. Actual PG tests exercise production host and owner-refresh entry points; compiled Rust mutations remove suppression and remove wake observation. Store I/O and non-Missing anomalies retain their existing behavior. These writer actions introduce no row retirement, deletion, kill, lease, body owner, or delivery path. The separate orphan retirement extension is described below.


The orphan extension adds two configurations. `OrphanBefore` holds the old Bound row and Unknown presence forever, violating `OrphanRecovers`; `OrphanFixed` retires the old row, publishes absence only after the existing HerdrRetired conditions pass, accepts one subsequent normal input as a new managed execution, and attaches its own source. The writer consumes presence/nonce/source evidence through F1. No action adopts the unrelated live pane or its transcript. Row retirement is separate from the writer execution retirement represented by `retired`.

`RetireOrphan` abstracts the completed six-stage proof, common lease/fence, final current/quiet checks and exact CAS; it does not independently prove OS or PG atomicity. `PresenceRecovery` is conditional on the existing absence checks and a current publication; fairness in this positive fixture assumes those checks become eligible. `NormalInput` represents an explicit subsequent input, not an autonomous launch by the recovery pass. Actual PG/Herdr tests and the two mutations exercise these boundaries outside the model. Retired alone does not imply absence, writer wake, or Discord delivery.
