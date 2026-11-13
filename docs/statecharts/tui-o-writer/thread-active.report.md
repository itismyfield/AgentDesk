# Thread child activation model

`WriterThreadActive.tla` models one thread child of an O-selected parent from its first input to a retired store, written against main `33d1f4b5a9`. It replaces no existing rule: the O-a reader projection (`thread-prep.report.md`) stays as it is.

Values come only from the entry table in `docs/session-host-parity.design.md` §1 and the code it names. Every producer (Discord intake, dispatch, routine TUI, terminal input) is one `Arrive` at the input placement boundary; `Admit` is the checked parent read there; `Resolve` is the existing 2 s owner refresh; `Boot` reads recorded children without REST and holds known unrecorded ones; `GcDelete` is the `thread_gc` row delete; `DeployCheck`/`DeployStop`/`DeploySwap` are the rollback guard, the service stop and the binary replacement.

## Rules

| Rule | Meaning |
|---|---|
| `NoLegacyBody` | Legacy posts no body for a child of an O parent |
| `ExactlyOnce` | one input is never posted by both Legacy and O |
| `OwnAuthority` | the child uses its own adoption and its own ready writer, never the parent's |
| `FloorBeforeInit` | a child `init` exists only after the floor |
| `ReadyNeedsInit` | a writer is ready only over a committed child `init` |
| `OldReaderBoots` | a binary without the child capability never boots over a child `init` |
| `TombExact` | a retirement mark names the child's current generation |
| `ChildInputAnswered` (liveness) | a waiting child input is posted by its ready writer or fails visibly |

Fairness is weak fairness on `Admit`, `Resolve`, `LegacyBody`, `StartWriter`, `PublishFloor`, `PublishInit`, `Ready`, `Post`, `HeldVisible`, `Boot`, `Retire` and `DeploySwap`. No SYMMETRY. Progress is claimed only while the checked parent read eventually succeeds; a failed init ends in a visible halt, not in progress.

## Results

Plain TLC 1.7.4 through `scripts/ci/statecharts_tlc.py`, 13 configs, 21.2 s, on 2026-11-13.

| cfg | distinct states | result |
|---|---:|---|
| WriterThreadActiveBefore | 5 | `NoLegacyBody` broken: main leaves the child to Legacy |
| WriterThreadActiveAfter | 818 | GREEN, liveness included |
| WriterThreadActiveDirect | 11 | `NoLegacyBody`, `ExactlyOnce`: a gate that reads only boot membership |
| WriterThreadActiveAlias | 6 | `OwnAuthority`, `ReadyNeedsInit`: the parent's adoption stands in |
| WriterThreadActivePending | 13 | `NoLegacyBody`: an undecided adoption a body claim releases |
| WriterThreadActiveReverse | 23 | `FloorBeforeInit`, `OldReaderBoots`: `init` before the floor |
| WriterThreadActiveReadyEarly | 23 | `ReadyNeedsInit`: ready announced before the `init` |
| WriterThreadActiveParentReady | 14 | `OwnAuthority`: the parent's readiness accepts child input |
| WriterThreadActiveNoBootScan | 28 | `NoLegacyBody`: a restart forgets a child with an open turn |
| WriterThreadActiveNoResolve | 729 | `ChildInputAnswered`: no refresh step resolves a held child |
| WriterThreadActiveNoGuard | 85 | `OldReaderBoots`: no rollback guard |
| WriterThreadActiveNoRecheck | 112 | `OldReaderBoots`: guard judged only before the service stops |
| WriterThreadActiveLateTomb | 159 | `TombExact`, `ChildInputAnswered`: a late retirement after a new input |

## Before and after

| | Before (main) | After |
|---|---|---|
| Child body | Legacy posts it (`NoLegacyBody` broken in 5 states) | the child's own writer posts it once |
| Restart with a recorded child | the record is read but grants no membership | read without REST, hosted again |
| Rollback over a child store | guarded once, before the stop | guarded again once no writer can create the floor |
| Row deleted by `thread_gc` | the child stays an active candidate | retired only for the exact, settled generation |

## Findings carried into the code

- `NoRecheck`: the floor can be created between the rollback guard and the service stop, and the replaced binary then refuses to boot. The guard has to run again after the stop. The current script judges it once, before the stop.
- `Reverse` also breaks `OldReaderBoots`, so the floor order is what keeps an old reader bootable, not only an ordering nicety.
- `LateTomb` breaks liveness as well as `TombExact`: a mark that survives a superseding input hides the new input from every writer start.

## Scope

The model covers one child and one parent. Sibling isolation is checked in the Rust tests, not here. Rows 1–3 of "Before and after" for a recorded child are implemented by the first O-b review unit; the first `init`, the producers, the capability flag, the second rollback guard and retirement belong to the later units and are model predictions until those land. Counterexamples for the recorded-child rules are replayed by `thread_children_*` and `thread_child_*` tests.

Logs: `~/.adk/release/lane-runs/hl-20260925/i8118/logs/ob-tlc`.
