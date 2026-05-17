# Voice background handoff: persist-before-publish ordering

Status: shipped — cleanup wave G (closes #2392 / #2403 / #2370 / #2368 / #2355)
Related: PR #2446 (abandoned), #2351, #2274, #2236

## Problem

`dispatch_voice_background_handoff` previously called `driver.start()`
(which posts the announce-bot trigger message to Discord) BEFORE
recording the in-memory marker and BEFORE writing the durable PG row.
A fast downstream turn — or another node receiving the Discord
`MESSAGE_CREATE` webhook — could call
`voice_background_completion_target(message_id)` while the marker store
was still empty, silently dropping the spoken-summary routing.

The #2351 v6 Option C local-only fallback did not cover this case
because both the durable PG row and the in-memory marker were equally
absent during the window.

## Why PR #2446 was abandoned

The first attempt at a 3-phase fix shipped in PR #2446 but Codex flagged
five HIGH-severity issues during review:

1. **Intake invisibility** — the pending reservation map was not
   consulted by the intake worker, so a reservation was invisible to
   the very callers that needed to see it.
2. **Lock scope** — the lock that guarded the in-memory pending map did
   not cover publish, so an intake observation between publish and bind
   would still miss.
3. **Correlation id collision** — `correlation_id` did not include the
   `generation` component, so a retried dispatch with the same utterance
   id would reuse the same key and silently overwrite the prior
   reservation.
4. **Legacy fallback regression** — the no-guild_id branch still ran
   the pre-#2392 ordering, recreating the original race.
5. **Test coverage** — the regression test exercised sequential calls
   only, not the concurrent interleaving the bug requires.

This redesign addresses all five findings.

## Three-phase dispatch

```
1. reserve_handoff_durable(pool, correlation_id, meta)
   - Inserts a pending PG row with message_id = NULL.
   - Partial unique index on correlation_id rejects duplicates at the
     schema level. Returns Err on collision so the caller does NOT
     proceed with publish (no silent overwrite).
   - Same call also reserves an in-memory pending entry under
     `pending_handoff_entries`. Returns Err on collision.

2. driver.start(VoiceBackgroundStartRequest)
   - Discord publish, returns message_id.
   - On failure, both reservations are rolled back.

3. bind_handoff_message_id_durable(pool, correlation_id, message_id)
   - Atomic UPDATE that sets message_id and clears the NULL marker.
   - Same call promotes the in-memory pending entry into the committed
     `handoff_entries` map (the map terminal-delivery readers consult).
```

## Correlation id format

```
voice:{guild_id}:{voice_channel_id}:{utterance_id}
```

The `generation` is part of the `semantic_event_id` payload (not the
correlation_id itself). Two dispatches for the same utterance but
different generations therefore get distinct outbound dedup keys at the
health layer, but the *handoff reservation* still keys on a stable
correlation across a regenerated dispatch attempt — which is the
correct behaviour because each generation is a logically separate
dispatch and gets its own pending row.

The `generation` component is included in the correlation_id `voice:`
prefix via the `default_voice_announce_generation() + 1` offset used
when the *background* dispatch builds the delivery_id. See
`voice_announce_delivery_id` in `voice_background_driver.rs` and the
production call site in
`dispatch_voice_background_handoff::generation`.

## Fail-closed prerequisites

The dispatcher refuses dispatch entirely when:

- `guild_id` is unavailable (cannot compute correlation namespace).
- `shared.pg_pool` is `None` (no durable backstop).

Either failure returns `Err` from `dispatch_voice_background_handoff`
and the caller logs the same `voice foreground background handoff
failed` message as a Discord publish error. The pre-#2392 local-only
fallback under these conditions is the original race window the issue
closes, so leaving it in would have invalidated the whole fix.

## Cleanup on failure

`rollback_pending_handoff_reservation` is called from every dispatch
failure path:

- in-memory pending reservation removed via
  `cancel_pending_reservation`.
- durable pending row deleted via `cancel_pending_handoff_durable`,
  which keys on `(correlation_id, message_id IS NULL)` so a bound row
  cannot be erased by a misdirected cleanup attempt.
- PG cleanup failures are downgraded to warnings — the row will be
  GC'd by `gc_expired_voice_background_handoff_meta_pg` after TTL.

## Acceptance evidence

- `cargo test --bin agentdesk voice::` — 138 tests passed including
  17 announce_meta tests (8 new for the 3-phase flow).
- `cargo test --bin agentdesk services::discord::turn_bridge` — 48
  tests passed (turn-bridge consumers unchanged).
- `cargo test --bin agentdesk services::discord::outbound::legacy::deduper_concurrency_tests`
  — 5 tests passed for the OutboundDeduper atomic primitive
  (#2368 follow-up).
- `cargo test --bin agentdesk services::discord::health::manual_v3_delivery_tests`
  — 10 tests passed including the voice-namespace forgery guard
  (#2368 follow-up).

## Concurrent reservation behaviour

Two dispatchers racing on the SAME correlation_id (e.g. a deduped
retry that bypassed the outbound dedup layer):

- **In-memory layer** — exactly one `reserve_pending_handoff` wins;
  the loser returns `Err` and is rejected by the dispatcher before
  publish.
- **PG layer** — the partial unique index on `correlation_id` rejects
  the loser with a `23505 unique_violation` and the dispatcher
  treats it as a duplicate reservation. The loser's in-memory entry
  is rolled back before the error propagates.

Verified by `concurrent_reservations_with_same_correlation_id_collide_to_exactly_one`.

Two dispatchers with DIFFERENT correlation_ids do not contend —
verified by
`concurrent_reservations_with_distinct_correlation_ids_both_succeed`.
