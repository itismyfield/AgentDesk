# Transcript rotation request and handoff

The design-3 model comes from `3bf90f277f572cb62aa65c7fc80044cde4bcae84`.
This slice carries its `FixedCore` and `LatchKeyFixed` configurations.
The model's transitions after `Barrier` still describe the design.

| Model action | Code |
| --- | --- |
| `Reserve` | `src/services/transcript_rotation/eligibility.rs:26` and `src/services/transcript_rotation/schedules.rs:7` evaluate the supplied evidence before the request touches PG. |
| `AskDispatch` | `src/db/transcript_rotation_requests.rs:13` spends the provider and native UUID key. `src/services/turn_orchestrator/queued_head/v2/rotation.rs:177` commits the intent after the insert. |
| `Stage` | `src/server/routes/transcript_rotation.rs:21` resolves the durable session row. `src/services/turn_orchestrator/queued_head/v2/rotation.rs:142` checks the ticket and commits the body and receipt together. |

`LatchKeyFixed` checks `NoRepeatedRequest` across A → B → A.
`FixedCore` checks 16 invariants and three liveness properties. Its
`AskDispatch` abstracts PG spend, Owner commit and physical request send as
one action. A PG spend followed by an Owner failure is exercised by the
Rust PG test. Receipt replay, expiry and conflicting submissions are also
covered by Rust tests.
