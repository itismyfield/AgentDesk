# Restart handoff custody

Source baseline: `2ab9d69829fe9f83a757c5f2f065585dbec5a43c`.
Before and After preserve the same transitions. Actual TLC ran with
`scripts/ci/statecharts_tlc.py`, Java OpenJDK and tla2tools 1.7.4.

| cfg | expected | observed |
|---|---|---|
| Before, After | GREEN | GREEN including WritersFinish |
| NoLock, EarlySample | MetadataInLock, NoLostUpdate | RED |
| DiskRead | BootMemo | RED |
| NoCas | SuccessorPreserved | RED |
| NoFinal | PersistedWins | RED |

Both writer processes share one canonical path. Allocation binds generation 1;
an overlapping process can advance only the disk counter. Sampling captures
both metadata and prior fields within the record lock. CAS protects a later
attempt from both stale settlement and stale consumption. The final persisted
read resolves publication during cancellation observation.

HTTP acceptance, routing decisions, filesystem durability errors and v1 wire
migration remain real-entry test obligations. The residual unlocked interval
before HTTP is preserved and does not establish a send-versus-replacement fence.
Proof body/filename nonce validation is covered by real filesystem tests; the
model assumes that proven artifacts passed that boundary. No deletion, retry or
kill authority is added. The chart omits the independent disk advance and proof
publication arrows; these actions remain in the checked model.
