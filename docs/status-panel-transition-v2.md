# Status-panel transition v2 substrate

AgentDesk #4891 Slice 1 adds a dormant, non-authoritative substrate for a future status-panel transition protocol. The module is not wired into Discord delivery, watcher recovery, the legacy inflight store, singleton/orphan stores, or any production caller.

## Contract

- `Prepared` records cannot bind.
- Discord acknowledgement is represented as `AckUnverified`; there is deliberately no `CandidateAcknowledged` state and no Discord nonce claim beyond the explicit unverified/quarantine model.
- The pure reducer commits acknowledgement and protection together.
- Binding requires both `BindAuthorized` and `JournalOwned`.
- Deletion requires a prior `RetireBeforeDelete` transition.
- Per-channel persistence uses a stable channel lock and revision CAS. Writes use a same-directory temporary file, file sync, rename, and parent-directory fsync; errors fail closed.
- Malformed, unknown-state, and malformed candidate-only payloads are classified separately. Malformed is never treated as missing.
- Recovery returns dry-run decisions only and performs no network action under the lock.

## Activation blockers

This substrate must not be activated until a separately reviewed slice defines authoritative ownership, legacy-store isolation, production caller admission, Discord delivery semantics, restart recovery policy, and cross-process mutation proofs. Activation also requires explicit evidence that production paths do not create, bind, delete, or mutate legacy inflight/singleton/orphan state through this module.
