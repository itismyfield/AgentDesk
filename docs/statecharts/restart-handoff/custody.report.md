# Restart handoff custody

Source baseline: `2ab9d69829fe9f83a757c5f2f065585dbec5a43c`.
Source implementation: `3266672203e0f74841136481ab903bf26ed1c68e`.
Before and After preserve the same transitions. Actual TLC checked seven cfgs
with `scripts/ci/statecharts_tlc.py`, OpenJDK and tla2tools 1.7.4: 28 runs,
all expected safety and liveness sets matched.

| cfg | expected | observed |
|---|---|---|
| Before, After | GREEN | GREEN, WritersFinish and OperationFinishes |
| NoLock, EarlySample | MetadataInLock, NoLostUpdate | RED |
| DiskRead | BootMemo | RED |
| NoCas | SuccessorPreserved | RED |
| NoFinal | PersistedWins | RED |

| Actual entry | Model action / obligation |
|---|---|
| store::announce_restart_in_root | Acquire, Sample, Publish on the canonical record |
| store::prepare_restart_handoff_in_root | Same canonical lock while sampling metadata and publishing the fresh attempt |
| process_generation::allocate_process_generation_binding / process_generation_binding | Boot allocation memo; AdvanceDisk cannot replace the bound generation |
| store::restart_claim_for_context_in_root | Same attempt, Awaiting state, generation memo and absent terminal proof |
| proof::terminal_proof_with_readers | FirstRead, CancelRead, FinalRead; persisted publication can interleave |
| store::settle_restart_path / settle_awaiting_for_flush | Settle; a stale attempt preserves its successor |
| Discord send_report / current_report_delivery_nonce | Accept after current attempt/state validation; lock released before HTTP |
| consume_after_send_acceptance / store::compare_delete | StaleConsume requires acceptance and exact-attempt CAS |
| record_path_lock::lock_record_path / Drop | One sidecar flock retained across read/sample/write, released with its file |

Two writers share one canonical path. Allocation binds generation 1, while an
overlapping process can advance only the disk counter. Sampling captures both
metadata and prior fields within the record lock. Settlement and accepted
consumption keep later attempts intact. An accepted send can race a replacement
in the existing unlocked HTTP interval; exact-attempt consumption remains fenced.

HTTP acceptance and a proven cancellation are explicit port assumptions.
No-proof Awaiting expiry, v1 migration, filename/body nonce validation and
filesystem durability failures are real-entry test obligations. The model does
not promise HTTP completion or delivery-versus-replacement atomicity. No new
deletion, retry or kill authority is added. The chart omits independent disk
advance and proof-publication arrows; both remain in the checked model.

Reproduce with the repository TLC checker selecting the restart-handoff cfgs
from `docs/statecharts/expected.tsv`. Their PROPERTY clauses check both writer
and attempt-operation completion under the stated weak fairness assumptions.
