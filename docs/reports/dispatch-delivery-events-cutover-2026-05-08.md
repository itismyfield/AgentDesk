# Dispatch Delivery Events Cutover Decision, 2026-05-08

Source issue: #1952

Decision: NO-GO

All observation times are KST.

## Evidence Sources

- API documentation checked before runtime API use:
  - `GET /api/docs`
  - `GET /api/docs/runtime/dispatches`
- Reconciliation endpoint:
  - `GET /api/dispatches/delivery-events/reconcile-stats`
- Per-dispatch event endpoint:
  - `GET /api/dispatches/0c5b4607-1a7c-49fb-b2b2-2342245b0d4d/events`
- Dashboard routine verification:
  - `http://127.0.0.1:8791/kanban`
  - card `#1952`
  - latest dispatch `0c5b4607-1a7c-49fb-b2b2-2342245b0d4d`

## Reconciliation Snapshot

Snapshot taken on 2026-05-08 17:41 KST.

| Metric                     | Value |
| -------------------------- | ----: |
| `typed_events_checked`     |   188 |
| `kv_notified_checked`      |  1256 |
| `kv_reserving_checked`     |     0 |
| `mismatch_count`           |  1099 |
| `missing_typed`            |  1098 |
| `missing_kv_meta`          |     1 |
| `notified_status_mismatch` |     0 |

The metric rows reported:

| Metric name                                        | Kind              | Value |
| -------------------------------------------------- | ----------------- | ----: |
| `agentdesk_dispatch_delivery_event_mismatch_total` | `missing_typed`   |  1098 |
| `agentdesk_dispatch_delivery_event_mismatch_total` | `missing_kv_meta` |     1 |

## Daily Mismatch Log

| KST date   | Observation                           | Mismatch count | Notes                                                                   |
| ---------- | ------------------------------------- | -------------: | ----------------------------------------------------------------------- |
| 2026-05-08 | First #1952 cutover decision snapshot |           1099 | `missing_typed=1098`, `missing_kv_meta=1`, `notified_status_mismatch=0` |

This is not a seven-day soak series. The current API exposes cumulative
reconciliation state, not a persisted per-day cutover-window ledger. The
`missing_typed` majority is interpreted as legacy `kv_meta` history predating
typed shadow writes, but that interpretation is not enough for GO because the
cutover window still cannot be isolated through the runtime API.

## Dispatch Volume Criterion

The typed table has 188 checked events in the release snapshot. The cutover
criterion requires at least 500 dispatch delivery events in addition to the full
seven-day release soak.

Result: FAIL.

## Seven-Day Soak Criterion

The dependent work for reconciliation logging, dashboard panel exposure, and
recovery/replay tests all completed on 2026-05-08. This report was produced on
the same date.

Result: FAIL. The release has not accumulated seven full KST days of verified
dual-write observation after all prerequisite surfaces were available.

## Mismatch Criterion

The endpoint reports 1099 cumulative mismatches.

Justified or partially justified:

- `missing_typed=1098` is consistent with legacy `dispatch_notified:*` markers
  created before typed delivery rows existed.

Not yet justified:

- `missing_kv_meta=1` needs a concrete dispatch-level explanation before GO.

Result: FAIL.

## Dashboard Routine Verification

Routine verification used the #1952 card detail drawer in the release dashboard.

The Delivery Events panel initially showed its loading state until the section
was scrolled into view. After the panel was visible, it loaded from
`GET /api/dispatches/0c5b4607-1a7c-49fb-b2b2-2342245b0d4d/events` and rendered
one `sent` row:

| Field          | Value                                  |
| -------------- | -------------------------------------- |
| dispatch id    | `0c5b4607-1a7c-49fb-b2b2-2342245b0d4d` |
| created        | 2026-05-08 17:37:49 KST                |
| status         | `sent`                                 |
| attempt        | 1                                      |
| target channel | `1501841431302770725`                  |
| message id     | `1502228029995483147`                  |

Operator note: the panel uses visibility-gated polling. When validating a card,
scroll the Delivery Events section into view before concluding it is stuck.

Result: PASS for one routine dashboard usage review.

## Rollback Runbook

Rollback procedure:

- `docs/runbooks/dispatch-delivery-cutover-rollback.md`

Reviewer sign-off is pending because the current decision is NO-GO and no typed
authority rollback is being approved today.

## Decision

NO-GO.

Do not change `src/services/dispatches/discord_delivery/guard.rs` to fully typed
claim authority in this release. Keep the existing legacy reservation and
finalization path in place, with typed delivery rows retained for audit,
reconciliation, dashboard display, and duplicate metadata where already
supported.

#1864 remains blocked. It should not start until a future GO report confirms:

- seven full KST days of release dual-write observation;
- at least 500 typed delivery events;
- zero cutover-window mismatches, or every mismatch fully justified;
- rollback runbook reviewer sign-off;
- one release has passed after the typed-authority cutover.
