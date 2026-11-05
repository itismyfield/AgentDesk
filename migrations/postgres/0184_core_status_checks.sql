-- Core status vocabularies: every value a current writer stores, plus the past values readers
-- still branch on. NOT VALID checks new tuples only; existing rows are not scanned here.
--
-- Past values kept so an UPDATE of an old row that still holds one does not fail:
--   auto_queue_entries cancelled  (uq_auto_queue_entry_card predicate, campaign_handoff.rs ON CONFLICT)
--   message_outbox     delivered  (message_outbox_circuit_authority.rs activation reads)
--   dispatch_outbox    claimed    (0049 partial index, node_registry.rs claim count)
--   routine_runs       error      (routines/store.rs run summary)
-- Writers: src/db/auto_queue/entries.rs ENTRY_STATUS_*, src/db/auto_queue/run_status.rs,
-- src/server/outbox_worker + src/services/message_outbox*.rs, src/db/dispatches/outbox +
-- src/services/health_diagnostics.rs (acknowledged), src/services/routines/store.rs.
ALTER TABLE auto_queue_entries
    ADD CONSTRAINT auto_queue_entries_status_check
    CHECK (status IN ('pending', 'dispatched', 'done', 'skipped', 'failed', 'user_cancelled',
                      'cancelled')) NOT VALID;

ALTER TABLE auto_queue_runs
    ADD CONSTRAINT auto_queue_runs_status_check
    CHECK (status IN ('generated', 'pending', 'active', 'paused', 'restoring', 'completed',
                      'cancelled')) NOT VALID;

ALTER TABLE message_outbox
    ADD CONSTRAINT message_outbox_status_check
    CHECK (status IN ('pending', 'held', 'processing', 'sent', 'failed', 'cancelled',
                      'delivered')) NOT VALID;

ALTER TABLE dispatch_outbox
    ADD CONSTRAINT dispatch_outbox_status_check
    CHECK (status IN ('pending', 'processing', 'done', 'failed', 'acknowledged',
                      'claimed')) NOT VALID;

ALTER TABLE routines
    ADD CONSTRAINT routines_status_check
    CHECK (status IN ('enabled', 'paused', 'detached')) NOT VALID;

ALTER TABLE routine_runs
    ADD CONSTRAINT routine_runs_status_check
    CHECK (status IN ('running', 'succeeded', 'failed', 'skipped', 'paused', 'interrupted',
                      'error')) NOT VALID;
