# Archived source classification

Code baseline: `73615f1feeaadbf693c30197c80b05feef7c329b`. This extends the
missing-source boundary represented by `WriterWake` and `WriterMissingHalt`;
the input-owner missing durable path in `input-single-path` is a different path.

`WriterArchivedSource` distinguishes a retained descriptor from an already
missing source at boot. The durable cursor and a prior EOF witness both cover
byte 1. An optional append before archival extends the actual end to byte 2
without moving the durable cursor. A saved witness cannot distinguish these
histories after the descriptor is gone. No archive contents are observed.

The new transition retires a Codex archive only with a retained FD, no pending
capture debt and a verified actual end equal to the durable cursor. A restart
retains that retirement. A new source can attach after readiness. A first boot
without the FD or an earlier retirement keeps the existing missing-source
classification: an old EOF witness alone is insufficient.

| Config | Expected broken invariant | Checker result |
|---|---|---|
| Before | `ArchiveAtVerifiedEofRetires` | expected RED |
| After | none | GREEN |
| NoDrain | `NoUnsentTailLost` | expected RED |

`NoUnsentTailLost` is the zero untransmitted tail loss INVARIANT. The NoDrain
counterexample archives an unread tail or a pending capture and then retires
it. The Rust negative test also appends after resume, so the cursor still
matches the earlier EOF while the retained FD proves a longer end.

The scoped repository TLC checker ran all three configurations in 3.6 seconds
with tla2tools 1.7.4 and Java 21. Raw output is in the lane's
`logs/statecharts-final.log`. Run with `scripts/ci/statecharts_tlc.py` and
`TLA2TOOLS=/home/adk/.local/share/tla2tools/tla2tools-1.7.4.jar`.

The chart is a hand-authored projection of the checked transitions. The model
assumes the existing park path is inapplicable (current source, presence Unknown).
This is a safety model, with no liveness or fairness claim. File identity,
prefix verification, partial lines, store errors, health routing and provider
selection are Rust test obligations. The archive action abstracts a completed
external archive; it does not prove the external archiver's behavior. Existing
non-archive park suppression, cleanup heuristics and source regrowth handling
are unchanged. For an archive, producer absence cannot replace final EOF proof;
the Rust boot regression covers both Unknown and Absent observations.
