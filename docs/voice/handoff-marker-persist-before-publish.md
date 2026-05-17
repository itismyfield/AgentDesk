# Voice background handoff: persist-before-publish ordering (#2392)

## Background

`VoiceBargeInRuntime::dispatch_voice_background_handoff` posts an
announce-bot trigger message to a routed background text channel and
records a typed `VoiceBackgroundHandoffMeta` marker keyed by the
returned `message_id`. The turn bridge later consults that marker on
terminal delivery to route the spoken summary back into the originating
voice channel (#2236).

## The race (Codex #2351 v7 follow-up, surfaced as #2392)

Previously the dispatch ordering was:

1. `driver.start()` — publish the announce-bot message to Discord
2. (await `message_id` round-trip)
3. `insert_handoff(message_id, meta)` — stamp the typed marker

Between steps 1 and 3 a fast downstream turn — or, more importantly,
another node receiving the Discord `MESSAGE_CREATE` webhook — could
call `voice_background_completion_target(message_id)` and find the
marker store empty. The Option C eventual-consistency fallback added in
#2351 v6 did not cover this case because both the durable PG row and
the in-memory marker were equally absent during that window. Spoken
summary routing dropped silently.

## Fix: three-phase dispatch keyed by `correlation_id`

The deterministic `voice_announce_delivery_id(...)` correlation_id is
known **before** publish (it is computed from `(guild_id,
voice_channel_id, utterance_id, generation)` — all of which the
dispatch site already holds). We use it as a pre-publish reservation
key, and atomically promote to a `message_id`-keyed marker after
publish:

```
phase 1: reserve_handoff(correlation_id, meta)        // pre-publish, sync
phase 2: driver.start() -> message_id                 // Discord publish
phase 3: bind_handoff_message_id(correlation_id, m_id) // atomic promote
```

`bind_handoff_message_id` removes the pending entry and inserts a
committed entry under the same `handoff_entries` lock that
`get_handoff` / `take_handoff` consult — once `bind` returns, the
lookup-side observes the marker with no intervening window.

### Failure paths

- `driver.start()` returns `Err` → reservation cancelled.
- Publish succeeds but returns no `message_id` → reservation cancelled
  and the legacy fallback warn is emitted (caller falls back to prefix
  detection).
- No `guild_id` available → no reservation can be made (the driver
  cannot stamp a delivery id either); falls back to the legacy direct
  `insert_handoff(message_id, ...)` so the no-guild case is not
  regressed.
- Reservation evaporates before `bind` (e.g. TTL expiry under
  pathological publish latency) → falls back to the legacy direct
  insert and emits a warn.

### Why not block on PG persist?

The marker store is in-memory and lock-free relative to the long PG
durability path documented in #2351. The race in #2392 is specifically
about the **in-memory** typed marker keyed by `message_id`; the
durable PG row continues to follow the Option C eventual-consistency
model from #2351 and is not affected by this change. If/when the
durable persist is folded back in, the same `correlation_id` can be
used as the durable PK so the PG row is also pre-published — but that
broader refactor is scoped under #2274 / #2355, not #2392.

## Tests

- `announce_meta::tests::reserve_then_bind_makes_marker_visible_by_message_id`
  asserts the ordering invariant: pre-bind lookup misses, post-bind
  lookup hits on the very first call.
- `announce_meta::tests::cancel_reservation_drops_pending_entry`
  asserts publish-failure cleanup does not leak the pending map.
- `announce_meta::tests::bind_without_reservation_is_noop` is a
  defensive check against double-bind / late-bind.
- `announce_meta::tests::ordering_invariant_bind_then_lookup_never_misses`
  exercises the lock-acquisition ordering 32 times to guard against
  any non-determinism between the `pending_handoff_entries` and
  `handoff_entries` write locks.

## Refs

- Issue #2392 — this fix
- PR #2351 — Option C durable PG persist (DRAFT, separate scope)
- Issue #2236 — typed marker (replaced spoofable Korean-prefix match)
- Issue #2274 — original durability requirement
- Issue #2355 — Option C design decision
