# Input notice settlement and iMessage echo

Code SHA: `a2fdbaf6c52190a8cb1d6b45989fe9f76eb12433`.
Design: #7811 design 3 plus independent review's implementation additions 1–11.
This is a design model; the production change and Rust regression tests have not been implemented.

## 상태 차트

The model has two registered attempts, two original sources, two registered native rows and
one later direct twin, two logical HTTP requests (one accepted and one rejected), replay and
at most one restart. Truth about which row came from an injection is a separate fixture function;
the computed owner is paired from source, offset, occupied attempts and exact UUID receipts.
An enqueue receipt never counts as a native user receipt. Census completion precedes ordering.

| Entry or branch | Code at the stated SHA | Modeled contract |
|---|---|---|
| Channel start | `src/server/routes/agents.rs:659`, `:768`, `:801`, `:816` | all backend Ok statuses accept; rejected start has no echo |
| DM start | `src/server/routes/agents.rs:694`, `:787` | excluded from channel echo; delegated to HTTP tests |
| Start status domain | `src/services/discord/router/turn_start.rs:47` | accepted, started, consumed are abstracted as Ok |
| Deliver, origin, obligations ON | `src/server/routes/agents_turn_deliver.rs:212`, `:223` | only winner executes and enqueues; replay has no enqueue |
| Deliver, origin, obligations OFF | `src/server/routes/agents_turn_deliver.rs:208`; `agents_turn_deliver/legacy_origin.rs:18`, `:23`, `:26`, `:35` | preflight, existing replay, race replay, winner |
| Deliver, no origin, obligations OFF | `src/server/routes/agents_turn_deliver.rs:227` | accepted request enqueues; per-request identity delegated to tests |
| Deliver, no origin, obligations ON | `src/server/routes/agents_turn_deliver.rs:148` | existing 400 remains a rejection |
| Handoff | `src/server/routes/agents.rs:1013`, `:1044`, `:1049` | same accepted-start abstraction; resolved channel delegated to tests |
| Scheduled message creation | `src/server/routes/scheduled_messages/create.rs:206`; `src/services/scheduled_messages.rs:435`; `scheduled_messages/owner_input.rs:211` | metadata source becomes scheduled_message execution source; outside echo |
| Discord send | `src/services/discord/outbound/send_api.rs:57` | outbound producer source, outside native input |
| Send-to-agent / send-DM | `src/server/routes/domains/ops.rs:43`, `:46` | outbound routes; no native input source contract |
| Agent message | `src/server/routes/agents.rs:74`, `:965` | no source-bearing input DTO |
| Messages | `src/server/routes/messages.rs:36` | message storage DTO has no source |
| Stop/cancel/resume/queue | `src/server/routes/domains/runtime.rs:90`, `:122` | no new source-bearing prompt; no new model action |

The endpoint strings start/deliver are the only HTTP route values in the model; handoff shares
the start acceptance contract. Display parsing, source spelling, DM suppression, all deliver
outcome variants and real durable dedupe identities require route tests, not this abstraction.

| cfg | Expected broken safety | Expected broken liveness |
|---|---|---|
| Before | — | OwnedEventuallyExcluded, AcceptedEventuallyEchoed |
| BeforeKnown | NoFalseExcludeDirect | — |
| After | — | — |
| AcceptanceGap | — | AcceptedEventuallyEchoed |

The final four-config check matched this matrix exactly: 12.5 s / 9.7 s / 5.0 s / 2.1 s
respectively (AcceptanceGap / After / Before / BeforeKnown), with no checker errors.
This uses the repository checker's `check_cfg` function, including the actual fairness precheck,
all safety reruns and singleton temporal-property runs. The full repository suite was started
before the final model correction and then stopped; no full-suite PASS is claimed.

Each PROPERTY config uses a fairness-bearing SPECIFICATION and no SYMMETRY.
After checks NoRepostOwned, DirectAtMostOnce, OwnerSameSource, OwnerInjective,
SettledIsPostedOrExcluded, NoSettleWithoutProof, EchoOncePerAccepted, RestartKeepsDurable,
NoFalseExcludeDirect and NoDoublePublisher, plus all three liveness properties.
NoSettleWithoutProof checks that a settle has its modeled proof path; BeforeKnown deliberately
accepts global known as that path and the separate NoFalseExcludeDirect rule rejects its scope.
Restart leaves notice states, native evidence, attempt occupancy, outbox identities and posted
counts unchanged. The invariant also checks that restart snapshots of durable outbox and POST
counts remain preserved.

Counterexample correspondence with the baseline:

1. Register → native row after enqueue-only receipt → Pending forever: admission's Pending
   branch (`src/services/tui_o/writer/input_notices/admission.rs:81`) waits for a receipt;
   `history.rs:488` does not perform the new bounded source attribution. No user receipt for
   attempt 1 is manufactured by the model. R1/R2/R3/R8a still require actual writer RED tests.
2. Global known after an attempt was consumed → direct twin Excluded: this is a forbidden
   shortcut, **not baseline behavior**. `src/services/tui_input/human_frame/history.rs:354`
   compares class hashes globally; using it as ownership would lose the source/UUID distinction.
   R5b must distinguish the current baseline Pending from the proposed fix Posted.
3. Backend accepted → crash before enqueue, or backend effect with error/409 → no echo:
   `src/services/imsg_reply/admission.rs:196` explicitly permits an error after effects, and
   `src/services/discord/router/message_handler/headless_turn.rs:1197` maps owner Pending
   to conflict. This is the accepted design gap, requiring diagnostic logging in the implementation.

Transport recovery and execution through the accepted-to-enqueue interval are assumptions of
After. AcceptanceGap removes the latter: crash can lose an unqueued accepted echo, and a
rejected response can retain backend effects. The property named AcceptedEventuallyEchoed
quantifies backend effects in this gap config so the latter case remains visible.
Ambiguous same-offset registrations, missing files/bindings, V1/V2 reconstruction, historical
receipt preservation outside a sparse census, raw GC, final-admission races and real PG behavior
are delegated to the required regression tests. None has been run in this lane yet.

Reproduce: `TLA2TOOLS=~/.cache/tla2tools/tla2tools.jar python3 scripts/ci/statecharts_tlc.py`.
TLC 1.7.4 SHA256: `936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`.
Raw per-run logs and the lane's incomplete implementation status are recorded in
`/home/adk/adk/lane-runs/i7811/report-1.md`.
