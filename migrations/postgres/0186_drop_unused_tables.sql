-- Drop five tables that no code writes or reads (owner decision on #7801, 2026-11-05).
-- No CASCADE: a foreign key, view, trigger or function still depending on one must fail this file.
--   db_table_metadata         no reference left; the module it described is gone.
--   dispatch_queue            no reference; its NO ACTION key to kanban_cards only blocked card deletes.
--   test_results              no writer or reader; only its own key to test_phase_runs depends on it.
--   deferred_hooks            nothing inserts rows; the startup drain that emptied it is removed.
--   channel_recovery_custody  dormant ledger from 0140; never written or read.
DROP TABLE IF EXISTS db_table_metadata;
DROP TABLE IF EXISTS dispatch_queue;
DROP TABLE IF EXISTS test_results;
DROP TABLE IF EXISTS deferred_hooks;
DROP TABLE IF EXISTS channel_recovery_custody;
