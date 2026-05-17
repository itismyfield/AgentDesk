# Voice Background Handoff Persist-Before-Publish

Issue #2392 closed the race where the announce bot could publish a
background handoff message before AgentDesk had recorded the typed handoff
marker used by terminal delivery.

## Chosen Approach

AgentDesk now uses a pre-publish correlation reservation:

1. Generate a `voice-bg:<uuid>` correlation id before publishing.
2. Reserve the handoff metadata in memory under that id.
3. Persist a durable Postgres reservation keyed as `pending:<correlation id>`.
4. Publish the announce-bot prompt with a spoiler-wrapped
   `ADK_VOICE_BACKGROUND_HANDOFF` marker containing the correlation id.
5. After Discord returns the real `message_id`, bind the local and durable
   reservations to that message id.

If a very fast background turn completes before step 5, terminal delivery
parses the correlation marker from the original prompt and atomically claims
the pending reservation instead. The later bind sees that the reservation is
already consumed and does not create a new row.

## Invariants

- Publish does not happen until the local reservation exists.
- With Postgres available, publish does not happen until the durable pending
  row exists.
- A late bind or retry must never clear `consumed_at` on an already-consumed
  handoff row.
- The correlation fallback is one-shot and still validates the recorded
  background channel before routing a spoken summary.
