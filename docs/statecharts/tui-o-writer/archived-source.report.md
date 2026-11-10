# Archived source classification and rebind recovery

Code baseline: `73615f1feeaadbf693c30197c80b05feef7c329b`; the rebind repair
starts at `a84d9cdb130dc39408c66c52c80f8e04c2bb924d`. This extends the
missing-source boundary in `WriterWake` and `WriterMissingHalt`.
`input-single-path` models an input owner's not-yet-filled durable path instead.

The durable cursor and a prior EOF witness cover byte 1. An optional append
before archival extends the actual end to byte 2 without moving that cursor.
Only a retained FD can distinguish these histories once the path is gone.
Archive contents are never observed. `NoUnsentTailLost` remains an INVARIANT:
retirement requires the actual end to equal the durable cursor, without pending
capture debt. A saved witness alone cannot authorize first-boot retirement.

`BindNew` adds a valid, readable B after archival. After A halts, `ResumeBound`
consumes this evidence once and resumes without A's FD, as the joined host does.
The repaired classification parks historical A without marking it retired and
then permits `AttachNew` for B. Current A without that binding remains halted.
The original retired-A attach case also remains reachable: B may bind while A
is already ready. `Restart` resets the memory park and readiness while retaining
retirement, the binding and the completed attach. `attachPermit` records the
readiness at attachment rather than assuming readiness persists over restart.

| Config | Expected violation | Checker result |
|---|---|---|
| Before | `ArchiveAtVerifiedEofRetires` | expected safety RED |
| After | none | safety GREEN |
| NoDrain | `NoUnsentTailLost` | expected safety RED |
| RebindBefore | `NewBindingProgress` | expected liveness RED |
| RebindAfter | none | safety and liveness GREEN |

The first three configurations retain the original retirement policy comparison.
The rebind pair keeps EOF checks enabled and changes only `AllowHistoricalPark`.
`NewBindingProgress` requires a supplied new binding to reach attachment.
Its Spec applies weak fairness to classification, the host's one evidence-based
resume and attachment. The external bind is not forced to arrive. B is assumed
readable and metadata/store operations available; HTTP delivery and PG/OS
atomicity are outside this bounded model. Rust tests verify the real host,
retained spool/cursor and exactly one first reply. `HistoricalParkIsNotRetirement`
prevents a historical park from claiming that A reached EOF. Unknown later bytes
of an absent A are not declared captured by this invariant.

The rebind-before counterexample is Archive -> missing classification/halt ->
BindNew -> ResumeBound -> missing classification/halt -> stutter. The host has
consumed B's evidence but cannot reach the actor's post-resume follow(B).
Rebind-after parks A at that second classification and fairness reaches B.
The NoDrain counterexample still retires unread bytes or pending capture debt.
The separate archive -> bind -> follow -> capture drain-boundary interleaving
is checked by the Rust test with both reader configurations and an unread tail.

The scoped repository TLC checker ran all five configs in 8.7 seconds with
TLC 1.7.4 and Java 21. Every expected violation set matched. Raw output is in
the lane's `logs/leg2/statecharts-final.log`; inputs are in its `statecharts/`
subdirectory. Use `scripts/ci/statecharts_tlc.py` with
`TLA2TOOLS=/home/adk/.local/share/tla2tools/tla2tools-1.7.4.jar`.
The Mermaid chart is a hand-authored projection of checked transitions.

The state space bounds process restart and new binding to one each. File
identity, prefix bytes, partial lines, store I/O failures, health and provider
selection are Rust obligations. The external archive abstracts a completed
archive, without proving the archiver or future appends after the EOF snapshot.
Current/Unknown archive still needs EOF proof; producer absence cannot replace
it. Non-archive park, cleanup heuristics and source regrowth handling remain.
