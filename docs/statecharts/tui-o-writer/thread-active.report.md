# Thread child activation model

`WriterThreadActive.tla` models one thread child from its first input to a retired store, written against main `33d1f4b5a9` and revised over main `8896695e65`. The parent is either an O writer channel (possibly dormant) or a Legacy channel, chosen at the start. It replaces no existing rule: the O-a reader projection (`thread-prep.report.md`) stays as it is.

Values come only from the entry table in `docs/session-host-parity.design.md` §1 and the code it names. Every producer (Discord intake, dispatch, routine TUI, terminal input) is one `Arrive` at the input placement boundary; `Admit` is the checked parent read there, which on failure holds the input at placement before it reaches the session; `Resolve` is that held placement tried again; `EarlyPlace` is the next input's placement recording a boot-reserved child while Legacy still sends its open turn; `Boot` reads recorded children without REST and reserves known unrecorded ones late, leaving their open turn to Legacy; a Legacy body send is a claim (`LegacyClaim`) that stays counted until it ends (`LegacyDone`); `GcDelete` is the `thread_gc` row delete; `InstallExec` is the owner refresh installing a Bound execution view over a recorded child's own adoption; `LegacyCut` is Legacy's terminal settling, with no body, a turn the gate calls O's; `DeployCheck`/`DeployStop`/`DeploySwap` are the rollback guard, the service stop and the binary replacement.

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
| `NoSilentLoss` | an input held at placement for an unresolved child is never dropped |
| `LateTurnLegacyOnce` | the late reservation's own turn ends sent by Legacy exactly once and never by O |
| `InheritDormant` | a running child writer reads in its parent's dormant or active mode |
| `ChildInputAnswered` (liveness) | a waiting child input is posted by its ready writer or fails visibly |
| `HeldResolves` (liveness) | an input held at placement is resolved while its process runs |

Fairness is weak fairness on `Admit`, `Resolve`, `LegacyClaim`, `LegacyDone`, `StartWriter`, `PublishFloor`, `PublishInit`, `Ready`, `Post`, `HeldVisible`, `Boot`, `Retire` and `DeploySwap`. No SYMMETRY. Progress is claimed only while the checked parent read eventually succeeds; a failed init ends in a visible halt, not in progress.

## Results

Plain TLC 1.7.4 through `scripts/ci/statecharts_tlc.py`, 17 configs, 24.9 s (rerun with `LateTurnLegacyOnce`; `NoWait`, `ReadyEarly` and `ParentReady` also break it, After stays GREEN at 2957 states), on 2026-11-14, after the boot hold was replaced by the late reservation (decision E). State counts are the first-violation runs.

| cfg | distinct states | result |
|---|---:|---|
| WriterThreadActiveBefore | 34 | `NoLegacyBody` broken: main leaves the child to Legacy |
| WriterThreadActiveAfter | 3131 | GREEN, both liveness properties included |
| WriterThreadActiveDirect | 71 | `NoLegacyBody`, `ExactlyOnce`: a gate that reads only boot membership |
| WriterThreadActiveAlias | 20 | `OwnAuthority`, `ReadyNeedsInit`, `ChildInputAnswered`: the parent's adoption stands in |
| WriterThreadActivePending | 64 | `NoLegacyBody`, `ChildInputAnswered`: an undecided adoption a body claim releases |
| WriterThreadActiveReverse | 65 | `FloorBeforeInit`, `OldReaderBoots`: `init` before the floor |
| WriterThreadActiveReadyEarly | 65 | `ReadyNeedsInit`, `ExactlyOnce`: ready announced before the `init` |
| WriterThreadActiveParentReady | 33 | `OwnAuthority`, `ExactlyOnce`: the parent's readiness accepts child input |
| WriterThreadActiveNoBootScan | 145 | `NoLegacyBody`: a restart forgets the child, so Legacy's send of its open turn is reserved by nobody |
| WriterThreadActiveNoResolve | 3131 | `ChildInputAnswered`, `HeldResolves`: an input held at placement is never tried again |
| WriterThreadActiveNoGuard | 280 | `OldReaderBoots`: no rollback guard |
| WriterThreadActiveNoRecheck | 318 | `OldReaderBoots`: guard judged only before the service stops |
| WriterThreadActiveLateTomb | 445 | `TombExact`, `ChildInputAnswered`: a late retirement after a new input |
| WriterThreadActiveNoWait | 1278 | `ExactlyOnce`: the placement records the child during Legacy's open turn and `init` commits before that send ends, so O posts it again |
| WriterThreadActiveInitNoWait | 477 | `LateTurnLegacyOnce`: a failed first `init` during Legacy's open late turn marks that turn failed with no send at all |
| WriterThreadActiveDropHeld | 13 | `NoSilentLoss`: a failed placement read drops the input instead of holding it |
| WriterThreadActiveNoInherit | 48 | `InheritDormant`: a dormant parent's child starts an active writer |
| WriterThreadActiveExecFirst | 280 | `LateTurnLegacyOnce`, `ChildInputAnswered`: the gate lets a Bound execution view own a late child before its `init`, so Legacy's terminal settles the open turn with no body and the `init` waits for a send that never comes |
| WriterThreadActiveReregister | 95 | `NoLegacyBody`, `ExactlyOnce`: a thread that is itself a configured binding is recorded again as a child, and the gate reads that late adoption while the member writer keeps posting |

`ExecFirst` and `Reregister` were added with the `exec`, `bound` and `stray` variables after the table above was measured; the rows above it are the earlier counts. With them, After is GREEN at 5011 distinct states.

`HoldFails` (the gate answering a held body as a failed send) was removed with the boot hold: under E no body exists for an unresolved child, as the hold is at placement before the input reaches the session. `DropHeld` is the same loss at the new hold point.

## Before and after

| | Before (main) | After |
|---|---|---|
| Child body | Legacy posts it (`NoLegacyBody` broken in 5 states) | the child's own writer posts it once |
| Restart with a recorded child | the record is read but grants no membership | read without REST, hosted again |
| Rollback over a child store | guarded once, before the stop | guarded again once no writer can create the floor |
| Row deleted by `thread_gc` | the child stays an active candidate | retired only for the exact, settled generation |
| Late child with a Legacy send running | not modelled | the handoff (`init`) waits for that send; the next turn is O's, posted once |
| Unrecorded child at boot | Legacy posts at once | reserved late: Legacy keeps posting its open turn once, the next placement records the child, O takes the turn after |
| Dormant parent's child | not modelled | its writer reads dormant, as the parent's |

## Findings carried into the code

- `NoRecheck`: the floor can be created between the rollback guard and the service stop, and the replaced binary then refuses to boot. The guard has to run again after the stop. The current script judges it once, before the stop.
- `Reverse` also breaks `OldReaderBoots`, so the floor order is what keeps an old reader bootable, not only an ordering nicety.
- `LateTomb` breaks liveness as well as `TombExact`: a mark that survives a superseding input hides the new input from every writer start.
- `NoWait`: committing the child's `init` while the late turn's Legacy send is still counted lets the new writer post the same turn. The handoff has to wait for that send and for the late turn to close.
- `ExecFirst`: an execution view is not ownership on its own. A late child stays Legacy's, counted on its own adoption, until that adoption commits; a held permit still holds.
- `Reregister`: a configured binding is never recorded as a child, so one channel has one adoption.
- The gate cannot own a boot hold: several body sites cannot wait in place and most read a sync peek first (b3 report's site table). Decision E moves the hold to the placement, where no body exists yet; `DropHeld` checks that hold keeps the input.

## Scope

The model covers one child and one parent. Sibling isolation is checked in the Rust tests, not here. Rows 1–3 of "Before and after" for a recorded child are implemented by the first O-b review unit and retirement by the second; the dormant inheritance and the second rollback guard are implemented by b3a; the first `init`, the late handoff wait and the census site × child rows by b3b-1; the producers, the boot reservation, the placement read and the capability flag by b3b-2.

The model treats "the late turn has closed" as a fact apart from the Legacy send counter. So a resolution that makes a new adoption (and loses the counted send) is invisible here: `lateTurn` still blocks the `init`. In the code, the late takeover reads that same counter, so the Rust test `thread_child_boot_reservation_counts_legacy_and_keeps_that_send_through_resolution` covers the regenerated-adoption mutant instead. Counterexamples for the recorded-child rules are replayed by `thread_children_*` and `thread_child_*` tests.

Logs: `~/.adk/release/lane-runs/hl-20260925/i8118/logs/ob-tlc`, revision `logs/ob3/tlc`.
