# Dispatch Delivery Cutover Rollback Runbook

Source issue: #1952

Last refreshed: 2026-05-08

## Scope

Use this runbook only after a future release switches dispatch notification
dedupe authority from the legacy `kv_meta` reservation/finalization path to a
fully typed `dispatch_delivery_events` claim path.

The current 2026-05-08 decision is NO-GO, so this runbook is documentation for a
future cutover, not an instruction to change production today.

## Rollback Goal

Restore runtime dispatch notification authority to the legacy `kv_meta` keys
without deleting or rewriting typed delivery rows:

- `dispatch_reserving:{dispatch_id}` remains the in-flight send claim.
- `dispatch_notified:{dispatch_id}` remains the delivered semantic notification
  marker.
- `dispatch_delivery_events` stays queryable for audit, dashboard, and
  reconciliation history.

## Preconditions

- A cutover release has already made typed claim order authoritative.
- Operators have observed duplicate sends, stuck reservations, or unexplained
  reconciliation drift after that cutover.
- The release process can deploy a rollback commit through
  `scripts/deploy-release.sh`.
- No follow-up cleanup that deletes legacy guard support, especially #1864, has
  been released.

## Immediate Containment

1. Stop the roll-forward sequence. Do not start #1864.
2. Capture the current reconciliation snapshot:

   ```bash
   agentdesk api GET /api/dispatches/delivery-events/reconcile-stats
   ```

3. Capture one affected dispatch event history, if an incident dispatch is
   known:

   ```bash
   agentdesk api GET /api/dispatches/<dispatch_id>/events
   ```

4. If a Discord duplicate was observed, record the affected `dispatch_id`,
   channel id, message id, and KST observation time in the incident note.

## Code Rollback

Rollback should be a normal commit, not a database deletion.

1. In `src/services/dispatches/discord_delivery/guard.rs`, restore the
   `claim_dispatch_delivery_guard` claim order to the legacy authority path:
   - check `dispatch_notified:{dispatch_id}` before sending;
   - delete expired `dispatch_reserving:{dispatch_id}`;
   - insert `dispatch_reserving:{dispatch_id}` with a short TTL;
   - send only when the legacy reservation insert succeeds.

2. Keep typed writes in place for audit:
   - reserve a `dispatch_delivery_events` row after the legacy reservation is
     claimed;
   - finalize the typed row after send success or failure;
   - log typed write failures without changing the delivery result.

3. Keep duplicate results compatible with typed metadata when it is present,
   but do not allow missing typed rows to force a resend if
   `dispatch_notified:{dispatch_id}` exists.

4. Do not delete or truncate `dispatch_delivery_events`.

## Data Handling

Typed rows are preserved. They are the audit trail for the failed cutover.

Permitted cleanup:

```sql
DELETE FROM kv_meta
 WHERE key LIKE 'dispatch_reserving:%'
   AND expires_at IS NOT NULL
   AND expires_at <= NOW();
```

Do not delete `dispatch_notified:%` markers during rollback. They are the
legacy "already delivered" guard and prevent duplicate Discord posts.

Do not run any cleanup against `dispatch_delivery_events` unless a separate
incident review identifies corrupt rows and provides exact dispatch ids.

## Verification

Run local verification before deploy:

```bash
cargo test -p agentdesk --features legacy-sqlite-tests \
  services::dispatches::discord_delivery::guard::tests::duplicate_delivery_replay_returns_prior_message_metadata_without_resend \
  services::dispatches::discord_delivery::guard::tests::expired_reserved_delivery_recovers_with_new_attempt_and_single_transport_send

cargo build
```

After deploy, verify the release runtime:

```bash
agentdesk api GET /api/health
agentdesk api GET /api/dispatches/delivery-events/reconcile-stats
```

Then send or observe one routine dispatch and confirm:

- the Discord notification is posted at most once;
- `/api/dispatches/<dispatch_id>/events` records the typed row;
- the Kanban card detail Delivery Events panel shows the row after the panel is
  scrolled into view;
- reconciliation does not introduce new unclassified mismatches.

## Roll Forward Again

Only retry typed authority after all of these are true:

- release dual-write has at least seven full KST days of observation;
- at least 500 typed dispatch delivery events have been recorded;
- every mismatch in the cutover window is zero or explicitly justified;
- this runbook has one reviewer sign-off;
- #1864 remains blocked until one post-cutover release passes.

## Sign-Off

- Author: Codex dispatch #1952, 2026-05-08
- Reviewer: pending for the future GO cutover
