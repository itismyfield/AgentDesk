# Discord Outbound Remaining Producers

Issue #1175 finished the remaining #1006 producer families after the core
dispatch, review, placeholder, gateway, and CLI text/DM helpers landed in
#1188/#1189. Issue #2535 removed the final legacy outbound bridge by migrating
issue announcements, monitoring status, meeting notifications, and routine
Discord summaries to direct v3 envelopes.

## Migrated in this slice

- `monitoring_status`: status banner send/edit routes through
  `outbound::delivery::deliver_outbound` with `preserve_inline_content`; only
  the delete path remains direct because delete is not a send/edit operation in
  the current shared contract.
- `meeting_orchestrator` and `commands/meeting_cmd`: meeting status, progress,
  cancellation, parse-error, and failure notices use direct v3 outbound
  envelopes. Long meeting transcripts and summaries intentionally keep
  `send_long_message_raw` because the existing contract only models a single
  send/edit, not ordered continuation chunks.
- `issue_announcements`: create/edit announcements use direct v3 envelopes and
  `OutboundOperation::Edit` where applicable.
- `routines::discord_log`: routine summary send/edit uses direct v3 envelopes
  with idempotency disabled for repeated summary writes.
- `message_outbox`: PG drains pass row metadata into `/api/discord/send` as
  a `ManualOutboundDeliveryId`, so the shared send contract sees the source,
  reason/session correlation, and row semantic event instead of anonymous
  manual delivery.
- `auto-queue-monitor`: shell-detected alerts and recoveries persist a stable
  action ID before calling `/api/message-outbox/monitor-alerts`. The protected
  route durably deduplicates that ID in `message_outbox`; a retry after a local
  state-commit crash cannot create a second notification obligation.
- `/api/discord/send-dm`: DM content delivery now uses the same manual outbound
  delivery path as `/api/discord/send` after DM-channel resolution. Callers can pass
  `correlation_id` plus `semantic_event_id`, or `idempotency_key`, to opt into
  retry dedupe. Requests without those fields are delivered without semantic
  dedupe so repeated identical DMs are still sent.

## Explicit exclusions

- Native Discord interaction responses (`ctx.say`, `ctx.send`,
  `ComponentInteraction::create_response`) stay direct. They are ACK/token
  operations, often ephemeral, and the current shared contract does not model
  interaction tokens or ephemeral visibility.
- File/attachment sends stay direct where the file itself is the payload. The
  current contract handles text fallback policy but not arbitrary attachment
  upload metadata.
- Thread creation/archive, message delete, and reaction updates stay direct.
  They are Discord resource-management operations, not outbound text
  send/edit producers.
- Streaming long-message helpers stay direct for ordered split continuation.
  They preserve full terminal/meeting output across multiple messages; moving
  them requires a future contract variant that returns ordered chunk metadata.
