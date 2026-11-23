# Shared frame ledger file: V2 owner and out-of-scope writer (#8343 slice 5)

Production base: `6b91594c5c`, which is slice 4 repair 1. The scope-free proof reads `history::registered`
(`history.rs:335`) and `unique_attempt` (`:894`) through `readable()` → `v2::unscoped_store()` → `owned()`.

Model: `InputLedgerFormat.tla`, view `ledger-format.view.mjs`.
- Revision 1 added the ops procedure (`Deploy`).
- Revision 2 added the transcript binding a proof needs (`binding`/`xbind`) and rollback (`Rollback`). It
  also replaced the V1 archive branches with "hold the deploy".

## What this model checks, and what it does not

It checks only these things:
- **Format change:** which envelope each writer can open.
- **Proof visibility and usability:** the ledger record, and the transcript binding `mend` needs
  (`queued_reconcile.rs:166-183,242-249,382-386`).
- **Stale overwrite:** a read-modify-write outside the shared lock.
- **Ops procedure:** intake close, the stop, the final check, session replacement, and a rollback after
  the new binary has written.

What it leaves out:
- C4/C5 in the slice 5 design (keyed/keyless reconciliation, native-row uniqueness) are still to be
  proven by the implementation's concurrent-transaction tests.
- `RecordedAtMostOnce` holds because each input registers once by construction.
- `OwnerProgress` means every input settles. It does not mean the deploy finished; `DeployReached`
  says that.

One file, `runtime/injected_frames.json`, is shared by every channel of the process. Two writers meet in it:

| Writer | Code | Today | After slice 5b |
|---|---|---|---|
| V2 input owner | `first_input.rs:101 prepare_history` → `v2.rs:334 prepare` → `v2.rs:260 transaction` | V2 transaction. A V1 file is restored to passive (`v2.rs:299 migration::restore`); a missing one starts from `migration::MISSING`. | V2 transaction. A missing file starts from an empty V2 snapshot; a V1 file is refused. |
| Out-of-scope writer (path A `inject.rs:491`, path B `queued_owner.rs` → `queued_provider.rs:42`) | `history.rs:235 register` → `:264 update` (`:177`) | V1 update, refused on a V2/V3 file (`compat.rs:98`). The input is NotSent(`load_failed`) and handed back (`inject.rs:708-731`). | V2 transaction writing a registration-less passive entry, the shape `migration::restore` makes today |

## Variables and ranges (from the slice 5 entry census)

| Constant | Range | Meaning |
|---|---|---|
| `Writer` | `v1`, `v2` | how the out-of-scope writer registers |
| `Migrate` | BOOLEAN | `migration::restore` of a V1 file exists |
| `View` | `v1`, `v2` | proof read of the binary that is not 5b: V1 `read()` (main before slice 4) or the `owned()` view (slice 4 repair 1) |
| `Boot` | `none`, `v1`, `v2` | ledger file on disk when the first binary starts |
| `Locked` | BOOLEAN | the out-of-scope read-modify-write is one step under the shared lock |
| `Restart` | BOOLEAN | a restart between paste and settle leaves the out-of-scope head Unknown |
| `Deploy` | `none`, `precheck`, `guarded`, `replaceonv2` | see below |
| `Rollback` | `none`, `binary`, `ledger` | after the 5b binary has written, back to the old binary with the current ledger or with the pre-deploy copy |

`Deploy` values:
- `precheck` is revision 0: a live format check, then stop, replace the sessions and start.
- `guarded` is revision 2:
  1. Close intake.
  2. Settle on the original binding.
  3. Stop.
  4. Replace the sessions and start only if the stopped ledger is V2/V3/missing and no Unknown head
     remains. Otherwise hold: the old binary comes back up on the same sessions.
- `replaceonv2` is a mutant that replaces the sessions whenever the format is V2.

A 5b binary booted on V1 stops its Claude runtime before intake: `native.rs:199` →
`runtime_bootstrap/native_input.rs:15-20` → `runtime_bootstrap.rs:299-301`. An operator's manual settlement
during a hold is not modeled.

Rules:
- `RecordedExactlyOnce` (live ledger)
- `RecordedAtMostOnce`
- `ProofKept`
- `BindingKept`: an Unknown head keeps the transcript binding its proof needs.
- `NewBootSupported`
- `OwnerProgress`
- `DeployReached`: procedure configs only.

## Results

`scripts/ci/statecharts_tlc.py --base 6b91594c5c --jobs 4`: see the slice 5 leg report for the run
(15/15 against `expected.tsv`).

| Config | View / Boot / Deploy / Rollback | Expected | Broken rules |
|---|---|---|---|
| `Now` | v2 / none / none / none (Writer v1, Migrate) | GREEN | none |
| `NowBeforeRepair` | **v1** / none / none / none | RED before repair 1 | OwnerProgress: [chart](ledger-format-before-repair.chart.mmd) |
| `NowOpsV2` | v2 / **v2** / none / none | GREEN (the observed ops files) | none |
| `DropMigrationOnly` | v2 / none / none / none (Migrate **off**) | RED (slice 4 DESIGN_DELTA) | OwnerProgress: [chart](ledger-format-drop-migration.chart.mmd) |
| `After` | v2 / none / none / none (Writer v2, no restore) | GREEN | none |
| `AfterBootV2` | v2 / v2 / none / none (after) | GREEN | none |
| `AfterBootV1` | v2 / **v1** / none / none (after) | RED: no V1 file may be left at boot | OwnerProgress: [chart](ledger-format-boot-v1.chart.mmd) |
| `AfterUnlocked` | v2 / none / none / none (after, **unlocked**) | RED mutant | RecordedExactlyOnce: [chart](ledger-format-unlocked.chart.mmd) |
| `DeployPrecheck` | v2 / none / **precheck** / none | RED (rev 0 procedure) | BindingKept, NewBootSupported, OwnerProgress: [chart](ledger-format-deploy-precheck.chart.mmd) |
| `DeployGuarded` | v2 / none / guarded / none | GREEN (rev 2 procedure) | none |
| `DeployGuardedBootV1` | v2 / **v1** / guarded / none | GREEN | none (the old binary's owner restores V1 before the final check) |
| `DeployGuardedPreRepair` | **v1** / none / guarded / none | RED = the deploy holds | OwnerProgress, DeployReached: [chart](ledger-format-deploy-pre-repair-hold.chart.mmd) |
| `DeployReplaceOnV2` | v2 / none / **replaceonv2** / none | RED mutant (review P1-1) | BindingKept, OwnerProgress: [chart](ledger-format-deploy-replace-on-v2.chart.mmd) |
| `DeployRollbackBinary` | v2 / none / guarded / **binary** | GREEN | none |
| `DeployRollbackLedger` | v2 / none / guarded / **ledger** | RED mutant (review P1-2) | ProofKept, RecordedExactlyOnce, OwnerProgress: [chart](ledger-format-deploy-rollback-ledger.chart.mmd) |

- `DeployGuardedPreRepair` is the compatibility scenario for a start binary without repair 1 (main
  before slice 4). That binary cannot prove a V2-recorded Unknown head, so the guarded procedure holds:
  it loops on the old binary without replacing the sessions. This is the documented hold, not a key or
  a deletion. The design therefore deploys slice 4 before slice 5b.
- `BindingKept` was added to the first eight configs; their results are unchanged.

## Counterexamples against the code

### Before repair 1
`XRegister` → `OwnerPrepare` → `Settle` → `Crash`, then stuck.
1. Path B head H registers V1 on a missing file (`history.rs:264`).
2. A V2 owner input restores it to passive (`v2.rs:299`).
3. The restart leaves H Unknown.
4. `nonce_in`'s `history::registered` read V1 only and fails on V2.

Repair 1 (`history.rs:343,895`) makes this the GREEN `Now`.

### Drop migration only
The out-of-scope writer creates V1. Without `migration::restore`, every owner transaction errs.

### After, V1 file at boot
Both writers refuse V1. The procedure never starts 5b on V1.

### Unlocked mutant
`XRead` → `OwnerPrepare` → `XWrite`: the stale copy erases the owner's registration.

### Rev 0 procedure (`DeployPrecheck`)
A live check finds no file. Then a channel whose owner adoption is held (`queued_head/v2/actor.rs:37`)
takes path A, and the old `history::update` creates V1 (`history.rs:177-195`). The 5b binary boots on it.

### Replace on V2 (`DeployReplaceOnV2`, review P1-1)
`XRegister` → `Close` → `OwnerPrepare` (restores V1 to V2) → `Stop` (H's paste is not settled, so H is
Unknown) → `FinalCheck` replaces the sessions because the file is V2. H's record is in the ledger, but
`Recorded::matches` needs H's transcript (`queued_reconcile.rs:382-386`), so `mend` can never succeed
(`queued_owner.rs:25-27`, then `queued_head.rs:623-630` BlockedUnknown).

### Ledger rollback (`DeployRollbackLedger`, review P1-2)
`Close` → `Stop` → `FinalCheck` (start 5b) → `XRegister` under 5b → `RollbackStep` restores the pre-deploy
copy. The 5b binary's committed registration disappears (`history.rs:335-351`, `:894-905`).

Binary-only rollback (`DeployRollbackBinary`) keeps the current ledger and is GREEN. The old binary reads
V2/V3: `compat.rs:43-55`, `v2.rs:299`/`migration.rs:30`, and `v2.rs:870-880` for registration-less
passive entries. All of these are identical on origin/main and the base.

## Not modeled

- **Adoption hold and legacy drain.** A refused out-of-scope input goes straight to the owner here. In
  the code, `v1_holds` (`queued_head.rs:574-576`) leaves the channel without an owner and the input
  returns to the legacy mailbox until the backlog drains.
- **Register and paste as separate steps.** A crash between them, an unconfirmed publication
  (`v2.rs:325-330`), and a later failure of `mark` or `receipt` are outside the state space.
- **Keyed/keyless reconciliation (C1) and native-row uniqueness (C5).** The model has no `(id, path)`,
  provenance, or row/event keys. These are still to be proven by implementation tests; candidate names
  are in slice 5 design §8.
- **The owner's own restart recovery and native V3 settlement.** A handed-back input the owner takes is
  bound to the sessions current at that moment.
- **The operator's manual settlement during a hold.**
- **Capacity.**
- **Which provider a V1 boot stops.** Only the Claude native boot stop is represented. Codex shows V1 as
  owner projection and first-record errors.
