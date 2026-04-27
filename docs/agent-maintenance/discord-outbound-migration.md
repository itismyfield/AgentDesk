# Discord Outbound Migration (#1006)

> Coverage map for the five Discord outbound producer families. Status as of
> 2026-04-27 grep against `main`. The detailed companion live in
> [`docs/discord-outbound-remaining-producers.md`](../discord-outbound-remaining-producers.md);
> this page is the agent-facing decision table.

- feature: `discord_outbound`
- canonical_modules: `src/services/discord/outbound/{mod,legacy,message,policy,result,decision}.rs`
- legacy_modules (still in use): `src/services/discord/outbound/legacy.rs`
  (`deliver_outbound`, `OutboundDeduper`, `Discord*` types)
- do_not_edit_without_migration_plan: `src/services/discord/formatting.rs::send_long_message_raw`
  (line 1971) — ordered-chunk continuation contract not yet modelled in v3.

## Production Callsite Coverage

| # | Family | Files / Entry points | Status | Evidence |
| - | --- | --- | --- | --- |
| 1 | `dispatch_outbox` | `src/server/routes/dispatches/discord_delivery.rs` (lines 769, 785, 3871, 3900); `src/server/background.rs::message_outbox_loop` (lines 422, 438) | migrated | `discord_delivery.rs` calls `deliver_outbound(&outbound_client, &dedup, outbound_msg, policy)` directly. `message_outbox_loop` posts to local `/api/send`, which is the canonical alias of `/api/discord/send` (`src/server/routes/domains/access.rs:42`) and itself uses `deliver_outbound` via the `ManualOutboundDeliveryId` path (`src/server/mod.rs:3241`). |
| 2 | `review_notifications` | `src/server/routes/review_verdict/{decision_route.rs, verdict_route.rs}`; `src/engine/ops/review_automation_ops.rs` | unknown | No direct `deliver_outbound` reference in `review_verdict/*`; review verdicts publish into outbox/dispatch flows whose own status is `migrated`, but the standalone notify path needs an explicit audit before claiming `migrated`. Track under #1280. |
| 3 | `dm_reply` | `src/server/routes/dm_reply.rs`; `src/engine/ops/dm_reply_ops.rs`; consumed by the `/api/senddm` (alias for `/api/discord/send-dm`) handler in `src/server/routes/health_api.rs:757` | migrated | `/api/senddm` was switched to the `ManualOutboundDeliveryId` path in #1188/#1189 (see `docs/discord-outbound-remaining-producers.md` "Migrated in this slice"). `dm_reply.rs` itself only registers pending replies (`register_pending_dm_reply_db`) and does not send. |
| 4 | `placeholder_sends` | `src/services/discord/placeholder_sweeper.rs`; `src/services/discord/placeholder_cleanup.rs` (edits via `gateway::edit_outbound_message`) | migrated | Both go through `gateway::edit_outbound_message`, which uses `deliver_outbound` (`src/services/discord/gateway.rs:293, 309, 337, 357`). |
| 5 | `dashboard_discord_proxy` | Dashboard-triggered sends originating from `src/server/routes/{kanban.rs, analytics.rs, receipt.rs, auto_queue.rs}` and the `/api/send`, `/api/senddm`, `/api/send_to_agent` handlers in `src/server/routes/health_api.rs` | migrated | These all route through the manual-outbound delivery path described in #1175 (`docs/discord-outbound-remaining-producers.md`). Dashboard handlers do not call `serenity::ChannelId::send_message` directly; they POST to the local API. |

Status legend:

- `migrated` — every send/edit on this path uses `outbound::deliver_outbound`
  (or its v3 successor) on the latest `main`.
- `legacy` — at least one send/edit on this path still uses a direct
  `serenity` call or `formatting::send_long_message_raw` outside the contract.
  Bugfix on the legacy code is permitted while the row stays in this state.
- `unknown` — needs explicit audit before next migration step. Treat as
  `legacy` for the purpose of new-send rules below.

## Invariants

- `new_send_must_use_v3`: any new outbound `send` or `edit` call from a
  production module MUST go through `outbound::deliver_outbound` (or the v3
  successor once 1.1 lands). Direct `channel_id.send_message`,
  `channel_id.say`, or raw `http.send_message` is a review block.
- `legacy_bugfix_only_when_table_legacy`: a PR may modify
  `outbound/legacy.rs`, `formatting::send_long_message_raw`, or any
  pre-migration legacy callsite ONLY if the corresponding row in the table
  above is `legacy` or `unknown`. Once a row flips to `migrated`, edits to
  its legacy entry points are review-block.
- `interaction_token_exception`: `ctx.say`, `ctx.send`, and
  `ComponentInteraction::create_response` stay direct (per
  `docs/discord-outbound-remaining-producers.md` "Explicit exclusions").
  These are ACK/token operations and are not part of this migration.
- `attachment_and_thread_ops_excluded`: file/attachment sends, thread
  create/archive, message delete, and reaction updates are out of scope and
  stay direct.

## Allowed Changes

- `bugfix`: permitted on `legacy` and `unknown` rows; review-block on
  `migrated` rows.
- `new_feature`: must land on `outbound::*` v3 surfaces; review-block if it
  lands on `legacy.rs` or any direct `serenity` send.
- `extraction`: contract changes in `outbound/{message,policy,result}.rs`
  must come with parallel updates to the unit tests in those files plus the
  integration tests below.

## Tests

- `src/integration_tests/discord_flow/scenarios.rs` (canonical callsite test
  for `deliver_outbound`).
- `src/integration_tests/agents_setup_e2e.rs` (end-to-end send via
  `deliver_outbound` against the mock Discord client).
- Per-module unit tests in
  `src/services/discord/outbound/{message,policy,decision,result}.rs`.

## Update Cadence

Every PR that flips a row's status MUST update this table in the same PR.
The next quarterly audit re-runs grep:

```
rg -n 'deliver_outbound' src --type rust
rg -n 'channel_id\.(send_message|say)' src --type rust | rg -v 'tests?\\.rs|integration_tests'
rg -n 'send_long_message_raw' src --type rust | rg -v 'tests?\\.rs'
```

If any new direct send appears outside the test tree, file a follow-up under
#1280 and flip the affected row to `legacy` until it is fixed.

## Related Issues

- #1006 — refactor: Discord outbound delivery → length-safe idempotent API.
- #1175 — finishes the remaining #1006 producer families.
- #1280 — outbound v3 migration coverage map and remaining callsite
  classification (this page is its agent-facing form).
