# Archived source boot verification

Modeling code baseline: `ac9e137b7994cff3005b173434d54c455302f452`.
Implementation code SHA: `f48772993775e746a88afd5183fa21c13f702629` (code commit; the following evidence commit changes documentation only).
Model SHA-256: `0bfb549d2d91c9b9ae3da9608e4f8ccd99983349d7c50f1a21b9e98d9bdfbdb3`.

`WriterArchivedSource.tla` keeps the five existing archive/rebind configurations
and adds twelve `BootSpec` configurations. The boot defect is a liveness gap:
a current archived source with no retained FD cannot reach readiness even when
its complete archive matches the durable cursor. Safety already holds before
the repair. The design permits retirement only after the retained spool has
replayed, capture debt and real unsealed bodies are absent, the decoder reaches
EOF at exactly the cursor length, the entire prefix hash matches, and the
original/archive snapshots still admit the proof at commit.

`detail.phantom` represents an async question UI announcement that cannot be a
Body to seal. It is distinct from `detail.real`, the channel-wide real Body
announcement, including one replayed from a later retained source. Removing
only the phantom registry obligation must not weaken the real-unsealed guard.
The model does not require public classifier facts, identity version 7 or posted
unit identities to change.

## Entry points and state bounds

The following code coordinates refer to the modeling baseline above. They pin
the source of the modeled entry values rather than assuming every writer boots
through one host path. File paths under `writer/` are relative to
`src/services/tui_o/`; the two runtime rows use full repository paths.

| Entry or model value | Baseline code coordinate | Modeled contract |
|---|---|---|
| `legacy_current_present`, `legacy_current_absent` | `writer/rotation.rs:347`, `writer/rotation/readers.rs:108` | A missing current source needs the archive candidate to survive initial reopen before later proof evaluation; producer absence alone is not proof. |
| `configured` | `writer/rotation.rs:372`, `writer/rotation/readers.rs:166` | The configured cleanup path uses the same verified boot proof and keeps store-I/O failure strict. |
| `unknown_current` | `writer/rotation/current.rs:5` | Unknown is not historical; it needs its own valid archive proof. |
| `live`, retained FD | `writer/rotation.rs:80`, `writer/rotation/retained_capture.rs:43` | The existing retained-FD EOF path remains represented. |
| `standby_restart` | `src/services/discord/runtime_bootstrap.rs:388`, `src/services/discord/runtime_bootstrap/gateway_lease_recovery.rs:476` | Promotion supplies a cold actor restart with no retained FD; the model does not assume every standby is promoted. |
| `gate_reacquire` and `detail.owner` | `writer/actor.rs:629`, `writer/deliver.rs:138`, `writer/host.rs:988` | A source proof and gateway ownership are distinct; posting still requires the existing Owned permit. |
| `manual_retired` | `store/spool.rs:29`, `writer/rotation/readers.rs:110` | An already persisted retired cursor can boot without requiring newly invented proof metadata. |
| `pending`, `detail.real`, `detail.phantom` | `writer/rotation.rs:81`, `writer/pieces.rs:201`, `writer/pieces.rs:266` | Capture/drain debt and the channel-wide replayed registry are available before retirement is chosen. |
| `detail.original`, `race`, `changed`, `proof` | `writer/rotation/readers.rs:504`, `shadow/capture.rs:280` | FD, archive and preexisting manual retirement are distinct; original return and archive replacement are checked during verification. |
| `newBound`, `retried`, `parked`, `attached` | `writer/host/missing.rs:81`, `writer/rotation/readers.rs:450` | A supplied valid new binding may be retried once; historical parking is not EOF retirement and cannot hide a reappeared original. |
| `oldPosts`, `newPosts`, `replayed` | `writer/replay.rs:15`, `writer/owed.rs:273` | Previously posted output stays single across spool replay and restart; the new binding's first reply is emitted at most once. |

The normalized cursor is byte 1. Length cases are short (0), exact (1), and
extra or partial tail (2). Hash mismatch and decoder failure are independent
cases. The initial state ranges over archive presence, real/phantom unsealed
state, capture debt, original/archive races and ownership. Restart and new
binding are each bounded to one. `BootOriginalReturns` and
`BootArchiveChanges` can interleave with verification before the commit guard.

## Checked results

These are the existing `sc-out/v4` design runs from 2026-11-11 00:29–00:30 KST.
The implementation support task did not rerun TLC or cargo. Before reusing the
artifacts it compared the current model and all 17 cfg files with the checked
input snapshot: all 18 files were byte-identical. The state counts below come
from the statechart run summaries, not from a partial safety failure's queue.

| Config suffix | States | Expected safety violation | Expected liveness violation |
|---|---:|---|---|
| `After` | 79 | `-` | `-` |
| `Before` | 72 | `ArchiveAtVerifiedEofRetires` | `-` |
| `BootAfter` | 23,180 | `-` | `-` |
| `BootBefore` | 19,520 | `-` | `VerifiedArchiveOnlyProgress` |
| `BootNoDebt` | 24,732 | `NoArchiveObligationLost` | `-` |
| `BootNoDecoder` | 23,128 | `NoArchiveObligationLost` | `-` |
| `BootNoHash` | 23,164 | `NoArchiveObligationLost` | `-` |
| `BootNoLength` | 23,132 | `NoArchiveObligationLost` | `-` |
| `BootNoParkGuard` | 23,742 | `NoHistoricalParkOverOriginal` | `-` |
| `BootNoPostedGuard` | 26,840 | `NoDuplicatePostedBodies` | `-` |
| `BootNoQuestionFilter` | 21,368 | `-` | `VerifiedArchiveOnlyProgress` |
| `BootNoRace` | 23,080 | `NoArchiveObligationLost` | `-` |
| `BootNoReopen` | 20,496 | `-` | `VerifiedArchiveOnlyProgress` |
| `BootNoUnsealedGuard` | 26,804 | `NoRealUnsealedRetired` | `-` |
| `NoDrain` | 90 | `NoUnsentTailLost` | `-` |
| `RebindAfter` | 89 | `-` | `-` |
| `RebindBefore` | 79 | `-` | `NewBindingProgress` |

`BootAfter` holds all five safety invariants, deadlock freedom,
`VerifiedArchiveOnlyProgress` and `BootNewBindingProgress`. The three boot
liveness RED configurations preserve safety. Each other boot mutant removes
one named guard and exposes the listed safety violation; these are TLA+
counterfactuals, not completed Rust mutation tests.

The committed `archive-boot.chart.mmd` is copied byte-for-byte from the generated
BootBefore chart. It shows only the checked liveness counterexample:
archive-only boot → halted → restart → halted forever. BootAfter's 23,180-state
space exceeds the chart tool's 500-state drawing budget, so no full GREEN chart
was generated or substituted for the counterexample.

## Progress assumptions and limits

`BootSpec` applies weak fairness to classification, verification, commit,
the selected standby promotion/reacquisition, the evidence-based retry,
attachment and posted replay. Process restart is permitted once but is not
forced. A new external binding is not forced to arrive.
`VerifiedArchiveOnlyProgress` requires a good stable archive, no real unsealed
body, no pending capture debt and Owned authority. `BootNewBindingProgress`
requires an already supplied new binding and excludes the original-return race;
the earlier Missing-only `Spec` and its rebind RED/GREEN pair remain unchanged.

The model covers eligible source scopes with successful metadata/store I/O. It
does not prove decoder implementation behavior, SHA-256, filesystem atomicity,
raw spool coverage, Discord acknowledgement, output splitting or actual GC.
Rust tests and implementation review must establish those contracts. A source
proof does not authorize posting while ownership is absent or unknown.

The retirement safety rules cover the new archive-proof and existing live-FD
archive retirement transitions. Existing quiet-successor, witness and operator
transition retirement are not reimplemented or newly asserted to have every
real-unsealed guard (`writer/rotation/readers.rs:390`, `:535`, `:743` and
`store/spool/transition.rs:24` at the modeling baseline). Future archive changes
after commit and legacy no-FD original regrowth are outside this model. The
baseline already closes the FD after live retirement and skips no-FD legacy
tend readers (`writer/rotation/readers.rs:529`, `:753`); this change does not
claim to extend that boundary.

Raw reports, TLC output and the exact checked input snapshot are retained under
`~/.adk/release/lane-runs/hl-20260925/i8051b/sc-out/v4/`; per-config execution
summaries are in `logs/design/model-summary.json`. The reuse byte comparison is
in `logs/impl/model-doc-input-sha256.json`. The implementation code SHA above pins the Rust change. The final evidence commit preserves those Rust and Cargo blobs; its exact SHA, every final plain-TLC command, fairness check, safety/liveness output and expectation comparison are retained in `sc-out/implementation-final/summary.json` and `configs/` under the same lane-run directory. `report-3.md` records the final result and checked SHA. The table above remains the original design-run evidence, distinct from that final replay.
