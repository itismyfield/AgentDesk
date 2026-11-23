# Shared frame ledger file: V2 owner and out-of-scope writer (#8343 slice 5)

Production base: `ab2d0dcb81` (slice 4), assuming slice 4 repair 1: the scope-free proof reads
`history::registered` / `unique_attempt` go through `readable()` → `v2::unscoped_store()` → `owned()`.
Model: `InputLedgerFormat.tla`, view `ledger-format.view.mjs`.

One file, `runtime/injected_frames.json`, is shared by every channel of the process. Two writers meet in it:

| Writer | Code | Today | After slice 5b |
|---|---|---|---|
| V2 input owner | `first_input.rs:101 prepare_history` → `v2.rs:334 prepare` → `v2.rs:260 transaction` | V2 transaction; a V1 file is restored to passive (`v2.rs:299 migration::restore`), a missing one starts from `migration::MISSING` | V2 transaction; a missing file starts from an empty V2 snapshot; a V1 file is refused |
| Out-of-scope writer (path A `inject.rs:491`, path B `queued_owner.rs` → `queued_provider.rs:42`) | `history.rs:235 register` → `:264 update` (`:177`) | V1 update; refused on a V2/V3 file (`compat.rs:98`), so the input is NotSent(`load_failed`) and handed back (`inject.rs:708-731`) | V2 transaction writing a registration-less passive entry, the shape `migration::restore` makes today |

Both writers take the same `.lock` (`history.rs:181-188`, `v2.rs:270-277`; `native_control.rs:261` try-locks it).

## Variables and ranges (from the slice 5 entry census)

| Constant | Range | Meaning |
|---|---|---|
| `Writer` | `v1`, `v2` | how the out-of-scope writer registers |
| `Migrate` | BOOLEAN | `migration::restore` of a V1 file exists |
| `View` | `v1`, `v2` | mend proof read: V1 `read()` (before repair 1) or the `owned()` view |
| `Boot` | `none`, `v1`, `v2` | ledger file on disk when the binary starts |
| `Locked` | BOOLEAN | the out-of-scope read-modify-write is one step under the shared lock |
| `Restart` | BOOLEAN | a restart between paste and settle leaves the out-of-scope head Unknown, proven later by `queued_reconcile::mend` (`:81`, `nonce_in :335`) |

Rules: `RecordedExactlyOnce` (a pasted input has exactly one record), `RecordedAtMostOnce`, `OwnerProgress`
(every input is eventually done).

## Results

`scripts/ci/statecharts_tlc.py --base ab2d0dcb81 --jobs 4`: PASS 8/8 against `expected.tsv`.

| Config | Writer / Migrate / View / Boot / Locked | Expected | Result |
|---|---|---|---|
| `InputLedgerFormatNow.cfg` | v1 / yes / v2 / none / yes | GREEN | GREEN, 14 states |
| `InputLedgerFormatNowBeforeRepair.cfg` | v1 / yes / **v1** / none / yes | RED before repair 1 | OwnerProgress RED: [chart](ledger-format-before-repair.chart.mmd) |
| `InputLedgerFormatNowOpsV2.cfg` | v1 / yes / v2 / **v2** / yes | GREEN (the observed ops file) | GREEN, 15 states: the out-of-scope input is refused and delivered by the owner |
| `InputLedgerFormatDropMigrationOnly.cfg` | v1 / **no** / v2 / none / yes | RED (slice 4 DESIGN_DELTA) | OwnerProgress RED: [chart](ledger-format-drop-migration.chart.mmd) |
| `InputLedgerFormatAfter.cfg` | v2 / no / v2 / none / yes | GREEN | GREEN, 12 states |
| `InputLedgerFormatAfterBootV2.cfg` | v2 / no / v2 / v2 / yes | GREEN | GREEN, 12 states |
| `InputLedgerFormatAfterBootV1.cfg` | v2 / no / v2 / **v1** / yes | RED: the ops procedure must leave no V1 file | OwnerProgress RED: [chart](ledger-format-boot-v1.chart.mmd) |
| `InputLedgerFormatAfterUnlocked.cfg` | v2 / no / v2 / none / **no** | RED mutant | RecordedExactlyOnce RED: [chart](ledger-format-unlocked.chart.mmd) |

## Counterexamples against the code

- **Before repair 1** (`XRegister` → `Crash` → `OwnerPrepare` → `Settle`, then stuck):
  1. `XRegister`: path B head H registers through `history.rs:264` → `update` with the file missing: V1 `{"entries":[H]}`.
  2. `Crash`: the restart lands between paste and settle; H stays Unknown in the queue.
  3. `OwnerPrepare`: any V2 owner input on any channel opens the same file; `v2.rs:299` restores H as passive and publishes V2.
  4. `mend` → `claude_proof` → `nonce_in` (`queued_reconcile.rs:335`) → `history::registered` (`history.rs:342`) reads V1 `read()` = `compat::read_legacy` → Err on V2 → false. No proof, H Unknown forever (`queued_owner.rs:25` → BlockedUnknown).
  Repair 1 (`readable()` → `owned()`, which keeps restored passive entries) makes `NowBeforeRepair` the GREEN `Now`.
- **Drop migration only** (`XRegister` → `Settle`, then stuck): the out-of-scope writer creates V1 on a missing file; without `migration::restore` `v2.rs:299` has no V1 branch, every owner transaction errs, and `first_input.rs:101` keeps refusing the owner's input.
- **After, V1 file at boot**: the out-of-scope transaction refuses V1 and hands the input back; the owner's transaction refuses it too. No product fallback is added; the ops procedure guarantees no V1 file at boot (design §7).
- **Unlocked mutant** (`XRead` → `OwnerPrepare` → `XWrite`): a writer that reads the file, lets the owner publish, and writes back its old copy erases the owner's registration. The after design keeps every out-of-scope mutation inside one `Registry::transaction` (in-process `cached` mutex, then the `.lock` flock, then read, apply, publish).

## Not modeled

- The owner's own restart recovery (`recovery.rs:34 sync_history` → `project`) and native V3 settlement: they touch
  registered entries only; the registration-less entries the out-of-scope writer owns are never projected.
- Capacity (`CAPACITY` 4096) and an unconfirmed publication (`v2.rs:326-330`): described in the design, not in
  the state space.
