# Shared frame ledger file: V2 owner and out-of-scope writer (#8343 slice 5)

<!-- doc-path-refs: proposed src/services/tui_input/human_frame/history/v2/keyless.rs -->

Production base: `6b91594c5c`, which is slice 4 repair 1. The scope-free proof reads `history::registered`
(`history.rs:335`) and `unique_attempt` (`:894`) through `readable()` → `v2::unscoped_store()` → `owned()`.

Model: `InputLedgerFormat.tla`, view `ledger-format.view.mjs`. Revisions:
- 1 added the ops procedure (`Deploy`).
- 2 added the transcript binding a proof needs and `Rollback`.
- 3 (for slice 5b2) adds the rest of E2 and makes E3 an explicit deploy condition. E2 now also covers
  tui_o retry debt (`retryDebt`) and unsettled keyless attempts (`keylessObl`). A settled historical
  keyless record (`KeylessHist`) is not debt.
- 4 (for slice 5b2) makes the keyless obligation fail-closed. The single Boolean becomes the persisted
  Codex admission/decision (`kst`: Reserved, EffectPossible, Uncertain, Admitted, HandedPending,
  Joined) and whether the native row reached the transcript (`krow`). Only a terminal persisted state
  (`Joined`) passes. A missing transcript row is no proof that nothing was pasted
  (`append.rs:483-500 replay_start`; `codex_tui/busy_inject.rs:396-453` marks EffectPossible before the
  paste and Admitted after Enter).
- 5 adds the slice 5b1 split writer (`Writer = "split"`, 5b1 DESIGN_DELTA, mac-0903). The format is chosen
  under the ledger lock: a V1 file keeps the V1 writer, and a missing or V2 file is written keyless. The
  code is `v2/keyless.rs:11-19` → `history.rs:171-185 update_v1` | `reconcile`, and `v2.rs:279-329 locked`,
  at 5b1 head `4c116ef2a5`.

## What this model checks, and what it does not

It checks only these things:
- **Format change:** which envelope each writer can open.
- **Proof visibility and usability:** the ledger record, and the transcript binding `mend` needs
  (`queued_reconcile.rs:166-183,242-249,382-386`).
- **Stale overwrite:** a read-modify-write outside the shared lock.
- **Ops procedure:**
  - intake close and the stop;
  - the final check on persisted state after the stop (E1 format, E2 obligations, E3 start binary);
  - session replacement;
  - a hold that reopens the old binary on the same sessions;
  - a rollback after the new binary has written.

What it leaves out:
- C4/C5 in the slice 5 design (keyed/keyless reconciliation, native-row uniqueness) are still to be
  proven by implementation tests.
- `retryDebt` (a Boolean) and `kst`/`krow` stand for the persisted classes in design §7 E2. The ledger
  structure behind them is not modeled, and both settle only on the original binding (assumed). Whether
  the one-off reader computes them correctly from real records is a fixture question (design §7 O6 table),
  not a TLC one.
- `RecordedAtMostOnce` holds by construction.
- `OwnerProgress` means every input settles. `ObligationsSettled` means every debt settles.
  `DeployReached` means the 5b binary started.

## Variables and ranges (from the slice 5 entry census)

| Constant | Range | Meaning |
|---|---|---|
| `Writer` | `v1`, `v2`, `split` | how the out-of-scope writer registers: V1 update (before 5b1), V2 transaction (5b2), or split by format under the lock (5b1) |
| `Migrate` | BOOLEAN | `migration::restore` of a V1 file exists |
| `View` | `v1`, `v2` | proof read of the start binary: V1 `read()` (main before slice 4) or the `owned()` view (slice 4 repair 1). For procedure configs this is also E3. |
| `Boot` | `none`, `v1`, `v2` | ledger file on disk when the first binary starts |
| `Locked` | BOOLEAN | the out-of-scope read-modify-write is one step under the shared lock |
| `Restart` | BOOLEAN | a restart between paste and settle leaves the out-of-scope head Unknown |
| `Deploy` | `none`, `precheck`, `guarded`, `replaceonv2`, `skipretry`, `skipkeyless`, `skipe3`, `keylessall`, `nonceabsent`, `anydecision` | `guarded` is revision 4; the rest are revision 0 or mutants, each dropping one condition |
| `Rollback` | `none`, `binary`, `ledger` | after the 5b binary has written |

`guarded`:
1. Close intake.
2. Settle on the original binding.
3. Stop.
4. Replace the sessions and start only if, from persisted state after the stop, all of these hold:
   - the ledger is V2, V3 or missing;
   - no Unknown head remains;
   - there is no retry debt, and every keyless attempt is absent or terminal (`Joined`);
   - the start binary has repair 1.

   Otherwise hold: the old binary comes back on the same sessions with intake open again, and the
   procedure starts over.

`retryDebt` and a keyless attempt (any non-terminal `kst`, with or without its row) can appear while the
old producer still runs, including after the live
check (`Appear`). The model's hold does not include the operator's manual settlement.

Rules:
- Invariants: `RecordedExactlyOnce`, `RecordedAtMostOnce`, `ProofKept`, `BindingKept`, `NewBootSupported`,
  `NoDebtAcrossReplace` (no session replacement while a retry debt or a non-terminal keyless attempt remains), `E3Enforced` (5b never starts
  without a repair 1 start binary, in every run).
- Properties: `OwnerProgress`, `ObligationsSettled` (`<>[]` no debt), `DeployReached` (procedure configs).

## Results

`scripts/ci/statecharts_tlc.py --base 6b91594c5c --jobs 4`: see the slice 5 leg report (21/21 against
`expected.tsv`; 25/25 with the split configs).

| Config | View / Boot / Deploy / Rollback | Expected | Broken rules |
|---|---|---|---|
| `Now` | v2 / none / none / none (Writer v1, Migrate) | GREEN | none |
| `NowBeforeRepair` | **v1** / none / none / none | RED before repair 1 | OwnerProgress: [chart](ledger-format-before-repair.chart.mmd) |
| `NowOpsV2` | v2 / **v2** / none / none | GREEN (the observed ops files) | none |
| `DropMigrationOnly` | v2 / none / none / none (Migrate **off**) | RED | OwnerProgress: [chart](ledger-format-drop-migration.chart.mmd) |
| `After`, `AfterBootV2` | v2 / none or v2 / none / none (after) | GREEN | none |
| `AfterBootV1` | v2 / **v1** / none / none (after) | RED: E1 | OwnerProgress: [chart](ledger-format-boot-v1.chart.mmd) |
| `AfterUnlocked` | after, **unlocked** | RED mutant | RecordedExactlyOnce: [chart](ledger-format-unlocked.chart.mmd) |
| `Split`, `SplitBootV1`, `SplitBootV2` | v2 / none, v1, v2 / none / none (Writer **split**, Migrate) | GREEN (5b1) | none |
| `SplitUnlocked` | v2 / none / none / none (split, **unlocked**) | RED mutant (5b1 R1) | RecordedExactlyOnce: [chart](ledger-format-split-unlocked.chart.mmd) |
| `DeployPrecheck` | v2 / none / **precheck** / none | RED (rev 0) | BindingKept, NewBootSupported, NoDebtAcrossReplace, OwnerProgress, ObligationsSettled: [chart](ledger-format-deploy-precheck.chart.mmd) |
| `DeployGuarded` | v2 / none / guarded / none | GREEN (rev 4) | none (1,500 states) |
| `DeployGuardedBootV1` | v2 / **v1** / guarded / none | GREEN | none |
| `DeployGuardedPreRepair` | **v1** / none / guarded / none | `E3Enforced` holds in every run; the deploy never starts | OwnerProgress, DeployReached: [chart](ledger-format-deploy-pre-repair-hold.chart.mmd) |
| `DeployReplaceOnV2` | v2 / none / **replaceonv2** / none | RED mutant (rev 1 review P1-1) | BindingKept, OwnerProgress: [chart](ledger-format-deploy-replace-on-v2.chart.mmd) |
| `DeploySkipRetry` | v2 / none / **skipretry** / none | RED mutant (rev 2 review P1-1) | NoDebtAcrossReplace, ObligationsSettled: [chart](ledger-format-deploy-skip-retry.chart.mmd) |
| `DeploySkipKeyless` | v2 / none / **skipkeyless** / none | RED mutant (rev 2 review P1-1) | NoDebtAcrossReplace, ObligationsSettled: [chart](ledger-format-deploy-skip-keyless.chart.mmd) |
| `DeployKeylessAll` | v2 / none / **keylessall** / none | RED mutant: a settled history record must not hold the deploy | DeployReached: [chart](ledger-format-deploy-keyless-all.chart.mmd) |
| `DeployNonceAbsent` | v2 / none / **nonceabsent** / none | RED mutant (rev 3 review P1-1, case A) | NoDebtAcrossReplace, ObligationsSettled: [chart](ledger-format-deploy-nonce-absent.chart.mmd) |
| `DeployAnyDecision` | v2 / none / **anydecision** / none | RED mutant (rev 3 review P1-1, case B: HandedPending) | NoDebtAcrossReplace, ObligationsSettled: [chart](ledger-format-deploy-any-decision.chart.mmd) |
| `DeploySkipE3` | **v1** / none / **skipe3** / none | RED mutant (rev 2 review P2-2) | E3Enforced, OwnerProgress, DeployReached: [chart](ledger-format-deploy-skip-e3.chart.mmd) |
| `DeployRollbackBinary` | v2 / none / guarded / **binary** | GREEN | none (1,576 states) |
| `DeployRollbackLedger` | v2 / none / guarded / **ledger** | RED mutant (rev 1 review P1-2) | ProofKept, RecordedExactlyOnce, OwnerProgress: [chart](ledger-format-deploy-rollback-ledger.chart.mmd) |

- `DeployGuardedPreRepair` and `DeploySkipE3` differ only in whether the final check requires E3. With the
  check, `E3Enforced` holds in **every** run: no run replaces the sessions on a start binary without repair 1,
  even with an empty ledger and no debt. Without the check, a run starts 5b (`E3Enforced` RED).
- `OwnerProgress` breaking in the PreRepair configs is the start binary's own limit: it cannot prove a
  V2-recorded Unknown head. It is not a product of the procedure.

## Counterexamples against the code

### Split writer (5b1)
`Split*` are GREEN from a missing, V1 or V2 file. A V1 file stays V1 until the owner's restore, and the
out-of-scope register is never refused, so path A/B no longer hands back with `load_failed` on V2.
`SplitUnlocked` (`XRead` → `OwnerPrepare` → `XWrite`) shows why the format check and the write must stay
in one locked step, as 5b1's `locked(true, …)` does (R1).

### Before repair 1
1. Path B head H registers V1 on a missing file (`history.rs:264`).
2. A V2 owner input restores it to passive (`v2.rs:299`).
3. A restart leaves H Unknown.
4. `nonce_in`'s V1-only `registered` fails on V2.

Repair 1 (`history.rs:343,895`) fixes it.

### Drop migration only
The out-of-scope writer creates V1, and every owner transaction errs without `migration::restore`.

### After, V1 at boot
Both writers refuse V1.

### Unlocked
`XRead` → `OwnerPrepare` → `XWrite`: a stale copy erases the owner's registration.

### Rev 0 (`DeployPrecheck`)
After the live check, a held-adoption channel (`queued_head/v2/actor.rs:37`) creates V1 through
`history.rs:177-195`, and 5b boots on it.

### Replace on V2
`XRegister` → `Close` → `OwnerPrepare` (V1 → V2) → `Stop` (H Unknown) → `FinalCheck` replaces the sessions.
`Recorded::matches` (`queued_reconcile.rs:382-386`) then never holds for H.

### Skip retry / skip keyless
`Appear` (a retry debt or keyless obligation) → `Close` → `Stop` → `FinalCheck` replaces the sessions with
no Unknown head left. This is the rev 2 review's chain: tui_o records a `NotFound` with retry budget between
the live check and the stop. The code points are:
- `tui_o/writer/local_retry.rs:22-46` for Body;
- `local_retry/notice_selection.rs:12-46` for InputNotice;
- `actor/transition_snapshot.rs:275-289` for obligations;
- `history.rs:505-546` for keyless Pending.

### Nonce absent / any decision
`Appear` (a keyless attempt that is non-terminal, with no native row yet, or HandedPending) → `Close` →
`Stop` → `FinalCheck` replaces the sessions:
- `nonceabsent` treats "no row in the transcript" as "never pasted". Codex marks EffectPossible before the
  paste and Admitted after Enter, and the native row is written later (`codex_tui/busy_inject.rs:396-453`,
  `rollout.rs:578-620`).
- `anydecision` treats HandedPending as settled. `injected_steer.rs:143-156` defers it, and
  `append::replay_start` (`append.rs:494`) keeps it a recovery target.

### Keyless all
Counting the settled history record as debt holds every deploy (`DeployReached`). Rev 3 §7 E2 classifies
keyless records and does not hold on history.

### Skip E3
`Close` → `Stop` → `FinalCheck` → 5b starts from a start binary whose proof read is V1 only (main
`history.rs:342,894`).

### Ledger rollback
`Close` → `Stop` → `FinalCheck` → `XRegister` under 5b → `RollbackStep` restores the old copy, which erases
5b's committed registration.

`DeployRollbackBinary` keeps the current ledger and is GREEN. The old binary reads V2/V3:
- `compat.rs:43-55`;
- `v2.rs:299` → `migration.rs:30`;
- `v2.rs:870-880` for registration-less passive entries.

All of these are identical on origin/main and the base.

## Not modeled

- **Adoption hold and legacy drain.** A refused out-of-scope input goes straight to the owner here, but
  `v1_holds` (`queued_head.rs:574-576`) leaves the channel ownerless until the legacy backlog drains.
- **Register and paste as separate steps.** A crash between them, an unconfirmed publication
  (`v2.rs:325-330`), and a later failure of `mark` or `receipt` are outside the state space.
- **Keyed/keyless reconciliation (C1) and native-row uniqueness (C5).** These are still to be proven by
  implementation tests.
- **The structure behind `retryDebt` and `keylessObl`.** This means tui_o ledger replay and keyless row
  scans. The model also has no operator manual settlement during a hold.
- **The owner's own restart recovery and native V3 settlement.**
- **Capacity.**
- **Which provider a V1 boot stops.**
