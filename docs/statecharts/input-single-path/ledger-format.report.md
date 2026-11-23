# Shared frame ledger file: V2 owner and out-of-scope writer (#8343 slice 5)

Production base: `6b91594c5c` (slice 4 repair 1: the scope-free proof reads `history::registered`
`history.rs:335` / `unique_attempt` `:894` through `readable()` → `v2::unscoped_store()` → `owned()`).
Model: `InputLedgerFormat.tla`, view `ledger-format.view.mjs`. Revision 1 adds the ops procedure (`Deploy`).

## What this model checks, and what it does not

It checks four things:
- **Format change:** which envelope each writer can open.
- **Proof visibility:** whether the recorded attempt stays visible to the scope-free proof.
- **Stale overwrite:** a read-modify-write outside the shared lock.
- **Ops procedure:** where the final format check sits relative to stopping the producers.

It is **not** a proof of C4/C5 in the slice 5 design, the keyed/keyless reconciliation and native-row
uniqueness. Those are proven by concurrent-transaction tests in the implementation (see Not modeled).
`RecordedAtMostOnce` holds because each input registers once by construction, not because a dedupe
was checked.

One file, `runtime/injected_frames.json`, is shared by every channel of the process. Two writers meet in it:

| Writer | Code | Today | After slice 5b |
|---|---|---|---|
| V2 input owner | `first_input.rs:101 prepare_history` → `v2.rs:334 prepare` → `v2.rs:260 transaction` | V2 transaction. A V1 file is restored to passive (`v2.rs:299 migration::restore`); a missing one starts from `migration::MISSING`. | V2 transaction. A missing file starts from an empty V2 snapshot; a V1 file is refused. |
| Out-of-scope writer (path A `inject.rs:491`, path B `queued_owner.rs` → `queued_provider.rs:42`) | `history.rs:235 register` → `:264 update` (`:177`) | V1 update, refused on a V2/V3 file (`compat.rs:98`). The input is NotSent(`load_failed`) and handed back (`inject.rs:708-731`). | V2 transaction writing a registration-less passive entry, the shape `migration::restore` makes today |

Both writers take the same `.lock` (`history.rs:181-188`, `v2.rs:270-277`); `native_control.rs:261` try-locks it.

## Variables and ranges (from the slice 5 entry census)

| Constant | Range | Meaning |
|---|---|---|
| `Writer` | `v1`, `v2` | how the out-of-scope writer registers |
| `Migrate` | BOOLEAN | `migration::restore` of a V1 file exists |
| `View` | `v1`, `v2` | mend proof read: V1 `read()` (before repair 1) or the `owned()` view |
| `Boot` | `none`, `v1`, `v2` | ledger file on disk when the (first) binary starts |
| `Locked` | BOOLEAN | the out-of-scope read-modify-write is one step under the shared lock |
| `Restart` | BOOLEAN | a restart between paste and settle leaves the out-of-scope head Unknown, proven later by `queued_reconcile::mend` (`:81`, `nonce_in :335`) |
| `Deploy` | `none`, `precheck`, `stopcheck`, `bakunchecked` | with anything but `none`, the old binary (V1 writer, restore, `View`) is checked, stopped and replaced by the 5b binary (V2 writer, no restore, V2 view) |

`Deploy` values:
- `precheck` is design revision 0's procedure: the live check plans what to do at the stop.
- `stopcheck` is revision 1's: a final check after the stop. A V1 file with an open proof obligation
  goes back to the old binary; otherwise it is archived (`.bak`).
- `bakunchecked` is a mutant that archives a V1 file without the obligation check.

A 5b binary booted on V1 stops its Claude runtime before intake: `native.rs:199 boot_records` →
`runtime_bootstrap/native_input.rs:15-20` → `runtime_bootstrap.rs:299-301`.

Rules:
- `RecordedExactlyOnce`: a pasted input has exactly one record, in the ledger or its archive.
- `RecordedAtMostOnce`.
- `ProofKept`: an Unknown head keeps its proof in the live ledger.
- `NewBootSupported`: the 5b binary never boots on V1.
- `OwnerProgress`: every input is eventually done.

## Results

`scripts/ci/statecharts_tlc.py --base ab2d0dcb81 --jobs 4`: PASS 12/12 against `expected.tsv`.

| Config | Writer / Migrate / View / Boot / Locked / Deploy | Expected | Result |
|---|---|---|---|
| `InputLedgerFormatNow.cfg` | v1 / yes / v2 / none / yes / none | GREEN | GREEN, 14 states |
| `InputLedgerFormatNowBeforeRepair.cfg` | v1 / yes / **v1** / none / yes / none | RED before repair 1 | OwnerProgress RED: [chart](ledger-format-before-repair.chart.mmd) |
| `InputLedgerFormatNowOpsV2.cfg` | v1 / yes / v2 / **v2** / yes / none | GREEN (the observed ops file) | GREEN, 15 states: the out-of-scope input is refused and delivered by the owner |
| `InputLedgerFormatDropMigrationOnly.cfg` | v1 / **no** / v2 / none / yes / none | RED (slice 4 DESIGN_DELTA) | OwnerProgress RED: [chart](ledger-format-drop-migration.chart.mmd) |
| `InputLedgerFormatAfter.cfg` | v2 / no / v2 / none / yes / none | GREEN | GREEN, 12 states |
| `InputLedgerFormatAfterBootV2.cfg` | v2 / no / v2 / v2 / yes / none | GREEN | GREEN, 12 states |
| `InputLedgerFormatAfterBootV1.cfg` | v2 / no / v2 / **v1** / yes / none | RED: the ops procedure must leave no V1 file | OwnerProgress RED: [chart](ledger-format-boot-v1.chart.mmd) |
| `InputLedgerFormatAfterUnlocked.cfg` | v2 / no / v2 / none / **no** / none | RED mutant | RecordedExactlyOnce RED: [chart](ledger-format-unlocked.chart.mmd) |
| `InputLedgerFormatDeployPrecheck.cfg` | old→5b / v1 / none / yes / **precheck** | RED (rev 0 procedure) | NewBootSupported, ProofKept, OwnerProgress RED: [chart](ledger-format-deploy-precheck.chart.mmd) |
| `InputLedgerFormatDeployStopcheck.cfg` | old→5b / v1 / none / yes / stopcheck | GREEN | GREEN, 107 states |
| `InputLedgerFormatDeployStopcheckBootV1.cfg` | old→5b / v1 / **v1** / yes / stopcheck | GREEN | GREEN, 116 states |
| `InputLedgerFormatDeployBakUnchecked.cfg` | old→5b / v1 / none / yes / **bakunchecked** | RED mutant | ProofKept, OwnerProgress RED: [chart](ledger-format-deploy-bak-unchecked.chart.mmd) |

Revision 1 added `ProofKept` and `NewBootSupported` to every config. The first eight configs keep
`Deploy = "none"`, where both rules hold by construction; their results are unchanged.

## Counterexamples against the code

### Before repair 1
`XRegister` → `OwnerPrepare` → `Settle` → `Crash`, then stuck:
1. `XRegister`: path B head H registers through `history.rs:264` → `update` with the file missing: V1 `{"entries":[H]}`.
2. `OwnerPrepare`: any V2 owner input on any channel opens the same file; `v2.rs:299` restores H as passive and publishes V2.
3. `Crash`: the restart lands between H's paste and its settle, so H stays Unknown.
4. `mend` → `claude_proof` → `nonce_in` (`queued_reconcile.rs:335`) → `history::registered` read V1 `read()` = `compat::read_legacy` → Err on V2 → false.
   No proof, so H stays Unknown forever (`queued_owner.rs:25`, then `queued_head.rs:630` BlockedUnknown).

Repair 1 (`history.rs:343` and `:895` now read `readable()`) makes this the GREEN `Now`.

### Drop migration only
`XRegister` → `Settle`, then stuck. The out-of-scope writer creates V1 on a missing file. Without
`migration::restore`, `v2.rs:299` has no V1 branch, every owner transaction errs, and
`first_input.rs:101` keeps refusing the owner's input.

### After, V1 file at boot
The out-of-scope transaction refuses V1 and hands the input back; the owner's transaction refuses it
too. No product fallback is added; the ops procedure guarantees no V1 file at boot.

### Unlocked mutant
`XRead` → `OwnerPrepare` → `XWrite`: a writer that reads the file, lets the owner publish, and writes back
its old copy erases the owner's registration. The after design keeps every out-of-scope mutation inside
one `Registry::transaction`: in-process `cached` mutex, then the `.lock` flock, then read, apply, publish.

### Rev 0 procedure (`DeployPrecheck`)
`PreCheck` finds no file → `XRegister` → `Stop` → `StartAsPlanned` boots the 5b binary on V1:
1. The node's ledger is missing while the old binary still runs.
2. A channel's owner adoption is held by a legacy backlog: `v2_ensure` returns `Ok(())` without an owner
   (`queued_head/v2/actor.rs:28-43`, `v1_holds` at `queued_head.rs:574-576`). Through
   `ingress.rs:381-394`, the external input then takes path A.
3. The old `history::update` creates V1 (`history.rs:177-195`).
4. The new binary's `boot_records` refuses V1, `prepare_native_boot` returns false, and the Claude runtime
   returns before worker and intake registration.

The same procedure also archives a V1 file at the stop while H is Unknown, which loses its proof
(`ProofKept`).

### Archive without the obligation check (`DeployBakUnchecked`)
The final check moves a V1 file whose Unknown head H still needs `nonce_in`'s ledger proof
(`queued_reconcile.rs:335-342`). H can never be proven (`ProofKept`, `OwnerProgress`).

## Not modeled

- **Adoption hold and legacy drain.** A refused out-of-scope input goes straight to the owner here. In
  the code, `v1_holds` (`queued_head.rs:574-576`) leaves the channel without an owner and the input
  returns to the legacy mailbox until the backlog drains.
- **Register and paste as separate steps.** A crash between them, an unconfirmed publication
  (`v2.rs:325-330`), and a later failure of `mark` or `receipt` are outside the state space.
- **Keyed/keyless reconciliation (C1) and native-row uniqueness (C5).** The model has no `(id, path)`,
  provenance, or row/event keys. Implementation tests prove them; candidate names are in design rev 1
  §8 (`keyless_reconcile_*`, `shared_row_*`).
- **The owner's own restart recovery and native V3 settlement.** `recovery.rs:34 sync_history` →
  `project` and native V3 settlement touch registered entries only.
- **Capacity.** `CAPACITY` is 4096.
- **Which provider a V1 boot stops.** Only the Claude native boot stop is represented (`NewBootSupported`).
  Codex shows V1 as owner projection and first-record errors (design rev 1 §5).
