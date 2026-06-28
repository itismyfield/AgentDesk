# Dispatch Delivery Events Design

Source issue: #1791

Epic: #1790

Last refreshed: 2026-06-28

Current tracker: #3747

## Background

Dispatch Discord notification idempotency is currently guarded through two
`kv_meta` keys in `src/services/dispatches/discord_delivery.rs`:

- `dispatch_reserving:{dispatch_id}` claims an in-flight notification send.
- `dispatch_notified:{dispatch_id}` marks the semantic notification as already
  delivered.

That guard is effective for preventing duplicate Discord posts, but it is hard
to audit, hard to expose in dashboards, and hard to reconcile against the v3
outbound delivery contract. The v3 outbound path already models a durable
identity with `correlation_id`, `semantic_event_id`, target metadata, operation
metadata, and a structured `DeliveryResult`. `dispatch_delivery_events` should
make that state queryable without changing first-step delivery behavior.

## Goals

- Add a typed Postgres table for dispatch notification delivery state.
- Preserve the current `kv_meta` guard as the source of truth during rollout.
- Record enough target, result, fallback, and error data to debug delivery from
  API and dashboard surfaces.
- Provide a clear cutover path from `kv_meta` to typed idempotency once shadow
  writes prove stable.

## Current rollout state

The legacy `kv_meta` markers remain the authoritative reservation and
finalization guard until a future cutover GO decision lands. The current open
tracker for the remaining typed-authority work is #3747.

The historical tracking issues should not be interpreted as a completed typed
cutover:

- #1952 is closed because it produced the 2026-05-08 NO-GO decision report.
  That issue completed the go/no-go exercise; it did not approve typed
  authority.
- #1864 is closed as historical cleanup tracking, but the cleanup remains
  blocked by the NO-GO decision. Removing or weakening the legacy guard requires
  a new GO decision under #3747 or a successor issue.

`dispatch_delivery_events` is no longer write-only shadow data. The current
guard reads typed rows to:

- detect a prior successful, fallback, skipped, or duplicate delivery before
  claiming a new send;
- return prior delivery metadata during duplicate replay;
- block concurrent sends when a non-expired typed reservation is active; and
- recover expired typed reservations by marking them `failed` before a retry.

That means operators should use the typed table for delivery diagnosis during
rollout while still treating `kv_meta` as the source of truth for whether the
legacy guard has reserved or finalized a notification. Full typed-table
authority remains incomplete until #3747 or a successor tracker records a future
GO decision.

## Runtime and operator source of truth

Until the future GO decision:

| State | Current source of truth | Typed-table role |
| ----- | ----------------------- | ---------------- |
| In-flight reservation | `kv_meta['dispatch_reserving:{dispatch_id}']` | Diagnostic/shadow row; typed active reservations are read to avoid duplicate sends during rollout but do not replace the legacy marker. |
| Final delivery dedupe | `kv_meta['dispatch_notified:{dispatch_id}']` | Diagnostic, typed-read, duplicate metadata, API/dashboard, and reconciliation state. |
| Cutover readiness | #3747 plus the latest cutover report | Evidence source only; not runtime authority. |

Operators should diagnose delivery through `dispatch_delivery_events`, the
delivery-events API, and the dashboard panel, but should treat `kv_meta` as the
authoritative reservation/finalization guard until a future GO changes this
contract.

## Non-Goals

- No behavior change in the design step.
- Do not replace `message_outbox`; this table describes delivery idempotency and
  results, not queue ownership.
- Do not remove the in-memory outbound deduper during the first implementation.
- Do not introduce a SQLite runtime fallback for this feature.

## Row Model

One row represents one semantic dispatch notification delivery attempt. The
initial implementation should write attempt `1` and update that row from
`reserved` to a terminal status. Later retries after a failed send or an expired
reservation may increment `attempt`.

The logical delivery key mirrors v3 outbound idempotency:

- `correlation_id`, currently `dispatch:{dispatch_id}`
- `semantic_event_id`, currently `dispatch:{dispatch_id}:notify`
- target metadata (`target_kind`, `target_channel_id`, `target_thread_id`)
- `operation`, normally `send`

`dispatch_id` remains a first-class column for joins, support queries, and API
routes; callers must not parse it back out of the correlation strings.

## Final Column Set

```sql
CREATE TABLE IF NOT EXISTS dispatch_delivery_events (
    id                  BIGSERIAL PRIMARY KEY,
    dispatch_id         TEXT NOT NULL REFERENCES task_dispatches(id) ON DELETE CASCADE,
    correlation_id      TEXT NOT NULL,
    semantic_event_id   TEXT NOT NULL,
    operation           TEXT NOT NULL DEFAULT 'send',
    target_kind         TEXT NOT NULL DEFAULT 'channel',
    target_channel_id   TEXT,
    target_thread_id    TEXT,
    status              TEXT NOT NULL CHECK (
        status IN ('reserved', 'sent', 'fallback', 'duplicate', 'skipped', 'failed')
    ),
    attempt             INTEGER NOT NULL DEFAULT 1 CHECK (attempt > 0),
    message_id          TEXT,
    messages_json       JSONB NOT NULL DEFAULT '[]'::jsonb,
    fallback_kind       TEXT,
    error               TEXT,
    result_json         JSONB NOT NULL DEFAULT '{}'::jsonb,
    reserved_until      TIMESTAMPTZ,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

Column notes:

- `status='reserved'` is the typed equivalent of `dispatch_reserving:{id}`.
- `status IN ('sent', 'fallback', 'duplicate', 'skipped')` is the typed
  equivalent of `dispatch_notified:{id}`.
- `status='failed'` records the failed attempt after the reservation is released.
- `message_id` stores the primary or first delivered Discord message id for
  common filtering; `messages_json` stores the ordered v3 `DeliveredMessage`
  list for split deliveries and duplicate replay.
- `fallback_kind`, `error`, and `result_json` preserve the structured
  `DeliveryResult` context without forcing every result variant into columns.
- `reserved_until` is nullable during shadow-write rollout, then becomes the
  recovery boundary for stale reservations when the typed path becomes
  authoritative.

## Indexes

```sql
CREATE UNIQUE INDEX IF NOT EXISTS uq_dispatch_delivery_events_attempt
    ON dispatch_delivery_events (
        correlation_id,
        semantic_event_id,
        operation,
        target_kind,
        COALESCE(target_channel_id, ''),
        COALESCE(target_thread_id, ''),
        attempt
    );

CREATE UNIQUE INDEX IF NOT EXISTS uq_dispatch_delivery_events_active
    ON dispatch_delivery_events (
        correlation_id,
        semantic_event_id,
        operation,
        target_kind,
        COALESCE(target_channel_id, ''),
        COALESCE(target_thread_id, '')
    )
    WHERE status IN ('reserved', 'sent', 'fallback', 'duplicate', 'skipped');

CREATE INDEX IF NOT EXISTS idx_dispatch_delivery_events_dispatch_created
    ON dispatch_delivery_events (dispatch_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_dispatch_delivery_events_status_created
    ON dispatch_delivery_events (status, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_dispatch_delivery_events_reserved_until
    ON dispatch_delivery_events (reserved_until)
    WHERE status = 'reserved' AND reserved_until IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_dispatch_delivery_events_message_id
    ON dispatch_delivery_events (message_id)
    WHERE message_id IS NOT NULL;
```

The active unique index is the future durable dedupe guard. It blocks another
send for the same semantic operation while a reservation or successful terminal
record exists, but permits a new attempt after `failed` rows.

## Dual-Write Strategy

1. Create the table and indexes with no runtime reads.
2. Keep `kv_meta` as the authority. The existing claim path still checks
   `dispatch_notified:{id}` and inserts `dispatch_reserving:{id}` first.
3. After the `kv_meta` reservation succeeds, shadow-insert a
   `dispatch_delivery_events` row with `status='reserved'`, attempt `1`,
   target metadata, and `reserved_until`.
4. After delivery finishes, keep the existing guard finalization behavior:
   delete `dispatch_reserving:{id}` and write `dispatch_notified:{id}` only on
   success-like outcomes. Then shadow-update the typed row to `sent`,
   `fallback`, `duplicate`, `skipped`, or `failed`.
5. During shadow mode, typed write failures must log a warning and increment a
   metric, but they must not flip the current delivery result.
6. Add a reconciliation job that compares `dispatch_reserving:*` and
   `dispatch_notified:*` keys against typed rows. Mismatches should be visible in
   logs and the dashboard before the typed table becomes authoritative.

After shadow parity is proven, invert the claim order inside one Postgres
transaction: claim the active unique typed row first, then keep writing the
legacy `kv_meta` markers for one release as a rollback path.

## Read API and Dashboard Plan

Add a small read service over the typed table before exposing routes:

- `GET /api/dispatches/{dispatch_id}/delivery-events` returns the ordered rows
  for one dispatch.
- `GET /api/dispatch-delivery-events?status=&since=&limit=` supports operations
  views for stuck reservations and repeated failures.
- The dispatch detail payload may include `latest_delivery_event` once the route
  is stable, but the standalone endpoint remains the debugging source of truth.

Dashboard surfaces:

- Dispatch detail: compact delivery status, message id link, fallback tag, and
  the latest error.
- Operations panel: filters for `reserved` rows past `reserved_until`, recent
  `failed` rows, and reconciliation mismatches.
- No dashboard code should read Postgres or SQLite directly; use the API route.

## Cutover Criteria

The next GO checklist for #3747 or a successor tracker must pass all of these
items before the typed table can become authoritative:

- Release soak duration: dual-write/shadow mode has run in release for at least
  seven full KST days after the relevant reconciliation, API/dashboard, rollback,
  recovery, and replay surfaces are available in that release.
- Dispatch volume: the same observation window includes at least 500 new
  dispatch notification attempts. If seven days elapse before 500 attempts, keep
  soaking until both thresholds pass.
- Mismatch window: reconciliation reports zero `kv_meta` versus typed-table
  mismatches for `dispatch_reserving:*` and `dispatch_notified:*` inside the
  cutover observation window, or every in-window mismatch is tied to a concrete
  dispatch id and explicitly justified. Legacy pre-shadow history may be
  excluded only when the event or key can be shown to predate the shadow-write
  window; cumulative `missing_typed` counts are not sufficient by themselves.
- Recovery tests prove that expired reservations can be retried without
  duplicate Discord posts.
- Duplicate replay tests prove that the typed active unique key returns the
  prior delivery metadata instead of sending again.
- API and dashboard views can diagnose a stuck, failed, fallback, duplicate, and
  successful delivery without direct DB inspection.
- `GET /api/dispatches/{id}/events` and the Kanban detail delivery-events panel
  have been deployed in release long enough for operators to use them during at
  least one real dispatch incident or routine verification pass.
- Rollback sign-off: the rollback runbook has explicit reviewer sign-off for the
  GO attempt and says how to re-enable `kv_meta` as the authority and ignore
  typed read decisions without deleting typed rows.
- Post-cutover shadow window: after a GO flips typed authority, keep legacy
  `kv_meta` shadow writes for one full release before starting any cleanup that
  removes the legacy guard.
- Tests pin the selected authority mode. Before GO, guard tests must assert that
  bare `kv_meta` reservation/finalization markers still block sends even when
  typed shadow rows are missing. A typed-authority cutover PR must update those
  tests in the same change that flips runtime authority.

Once those criteria pass, switch runtime reads and dedupe claims to
`dispatch_delivery_events`, keep shadow `kv_meta` writes for one release, then
track legacy guard removal in the active cutover tracker or a successor cleanup
issue. The closed #1864 issue is a historical reference, not current approval to
remove the guard.

## Cutover Decision Log

| Date       | Decision | Report                                                                                                      | Notes                                                                                                                                                                                         |
| ---------- | -------- | ----------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 2026-05-08 | NO-GO    | [dispatch-delivery-events-cutover-2026-05-08.md](../reports/dispatch-delivery-events-cutover-2026-05-08.md) | Release snapshot had 188 typed events, 1099 cumulative reconciliation mismatches, and no seven-day post-prerequisite soak. Keep the legacy reservation/finalization path; do not start #1864. |

As of 2026-06-28, #3747 is the single current open tracker for the remaining
typed-authority cutover work. #1952 and #1864 are closed historical issues and
must not be used to infer that typed authority completed.

Rollback procedure for a future typed-authority cutover:
[dispatch-delivery-cutover-rollback.md](../runbooks/dispatch-delivery-cutover-rollback.md).

## Implementation Checklist

- Add the Postgres migration and migration test.
- Add a small repository/service wrapper for reserve, finalize, and query.
- Wire shadow writes beside `claim_dispatch_delivery_guard` and
  `finalize_dispatch_delivery_guard`.
- Add reconciliation logging and dashboard/API read routes.
- Add focused tests for reservation, success, fallback, duplicate, failure,
  reconciliation mismatch, and expired-reservation retry.
