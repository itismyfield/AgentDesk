# Thread child activation model

`WriterThreadActive.tla` models one thread child from its first input to a retired store, written against main `33d1f4b5a9` and revised over main `8896695e65`. The parent is either an O writer channel (possibly dormant) or a Legacy channel, chosen at the start. It replaces no existing rule: the O-a reader projection (`thread-prep.report.md`) stays as it is.

Values come only from the entry table in `docs/session-host-parity.design.md` §1 and the code it names. Every producer (Discord intake, dispatch, routine TUI, terminal input) is one `Arrive` at the input placement boundary; `Admit` is the checked parent read there; `Resolve` is the existing 2 s owner refresh; `Boot` reads recorded children without REST and holds known unrecorded ones; a Legacy body send is a claim (`LegacyClaim`) that stays counted until it ends (`LegacyDone`); `GcDelete` is the `thread_gc` row delete; `DeployCheck`/`DeployStop`/`DeploySwap` are the rollback guard, the service stop and the binary replacement.

## Rules

| Rule | Meaning |
|---|---|
| `NoLegacyBody` | Legacy posts no body for a child of an O parent, except the late reservation's own turn before the handoff |
| `ExactlyOnce` | one input is never posted by both Legacy and O |
| `OwnAuthority` | the child uses its own adoption and its own ready writer, never the parent's |
| `FloorBeforeInit` | a child `init` exists only after the floor |
| `ReadyNeedsInit` | a writer is ready only over a committed child `init` |
| `OldReaderBoots` | a binary without the child capability never boots over a child `init` |
| `TombExact` | a retirement mark names the child's current generation |
| `NoSilentLoss` | a body held for an unresolved child is never dropped |
| `InheritDormant` | a running child writer reads in its parent's dormant or active mode |
| `ChildInputAnswered` (liveness) | a waiting child input is posted by its ready writer or fails visibly |
| `HeldResolves` (liveness) | an unresolved child is resolved while its process runs |

Fairness is weak fairness on `Admit`, `Resolve`, `LegacyClaim`, `LegacyDone`, `StartWriter`, `PublishFloor`, `PublishInit`, `Ready`, `Post`, `HeldVisible`, `Boot`, `Retire` and `DeploySwap`. No SYMMETRY. Progress is claimed only while the checked parent read eventually succeeds; a failed init ends in a visible halt, not in progress.

## Results

Plain TLC 1.7.4 through `scripts/ci/statecharts_tlc.py`, 16 configs, 76.0 s, on 2026-11-14. State counts are the first-violation runs.

| cfg | distinct states | result |
|---|---:|---|
| WriterThreadActiveBefore | 34 | `NoLegacyBody` broken: main leaves the child to Legacy |
| WriterThreadActiveAfter | 4337 | GREEN, both liveness properties included |
| WriterThreadActiveDirect | 78 | `NoLegacyBody`, `ExactlyOnce`: a gate that reads only boot membership |
| WriterThreadActiveAlias | 20 | `OwnAuthority`, `ReadyNeedsInit`: the parent's adoption stands in |
| WriterThreadActivePending | 70 | `NoLegacyBody`: an undecided adoption a body claim releases |
| WriterThreadActiveReverse | 180 | `FloorBeforeInit`, `OldReaderBoots`: `init` before the floor |
| WriterThreadActiveReadyEarly | 78 | `ReadyNeedsInit`, `ExactlyOnce`: ready announced before the `init` |
| WriterThreadActiveParentReady | 34 | `OwnAuthority`, `ExactlyOnce`: the parent's readiness accepts child input |
| WriterThreadActiveNoBootScan | 167 | `NoLegacyBody`: a restart forgets a child with an open turn |
| WriterThreadActiveNoResolve | 3659 | `ChildInputAnswered`, `HeldResolves`: no refresh step resolves a held child |
| WriterThreadActiveNoGuard | 817 | `OldReaderBoots`: no rollback guard |
| WriterThreadActiveNoRecheck | 936 | `OldReaderBoots`: guard judged only before the service stops |
| WriterThreadActiveLateTomb | 443 | `TombExact`, `ChildInputAnswered`: a late retirement after a new input |
| WriterThreadActiveNoWait | 750 | `ExactlyOnce`: init commits while the late turn's Legacy send is running, so O posts it again |
| WriterThreadActiveHoldFails | 31 | `NoSilentLoss`: the gate answers an unresolved child as a failed send and the caller drops it |
| WriterThreadActiveNoInherit | 54 | `InheritDormant`: a dormant parent's child starts an active writer |

## Before and after

| | Before (main) | After |
|---|---|---|
| Child body | Legacy posts it (`NoLegacyBody` broken in 5 states) | the child's own writer posts it once |
| Restart with a recorded child | the record is read but grants no membership | read without REST, hosted again |
| Rollback over a child store | guarded once, before the stop | guarded again once no writer can create the floor |
| Row deleted by `thread_gc` | the child stays an active candidate | retired only for the exact, settled generation |
| Late child with a Legacy send running | not modelled | the handoff (`init`) waits for that send; the next turn is O's, posted once |
| Unrecorded child at boot | Legacy posts at once | held by the gate until the refresh resolves it, then sent once by its owner |
| Dormant parent's child | not modelled | its writer reads dormant, as the parent's |

## Findings carried into the code

- `NoRecheck`: the floor can be created between the rollback guard and the service stop, and the replaced binary then refuses to boot. The guard has to run again after the stop. The current script judges it once, before the stop.
- `Reverse` also breaks `OldReaderBoots`, so the floor order is what keeps an old reader bootable, not only an ordering nicety.
- `LateTomb` breaks liveness as well as `TombExact`: a mark that survives a superseding input hides the new input from every writer start.
- `NoWait`: committing the child's `init` while the late turn's Legacy send is still counted lets the new writer post the same turn. The handoff has to wait for that send and for the late turn to close.
- `HoldFails`: an unresolved child answered as a failed send loses the body at any caller that does not retry. Several callers cannot wait in place either (see the b3 report's site table), so the hold needs an owner other than the waiting caller.

## Scope

The model covers one child and one parent. Sibling isolation is checked in the Rust tests, not here. Rows 1–3 of "Before and after" for a recorded child are implemented by the first O-b review unit and retirement by the second; the first `init`, the producers, the late handoff wait, the gate-held body, the dormant inheritance, the capability flag and the second rollback guard are model predictions until the activation unit lands. Counterexamples for the recorded-child rules are replayed by `thread_children_*` and `thread_child_*` tests.

Logs: `~/.adk/release/lane-runs/hl-20260925/i8118/logs/ob-tlc`, revision `logs/ob3/tlc`.
