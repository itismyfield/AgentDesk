-- no-transaction
-- Leading prefix of the UNIQUE uq_aq_phase_gates_run_phase_dispatch_key (run_id, phase, ...).
-- CONCURRENTLY must run outside a transaction, so this file holds one executable statement.
DROP INDEX CONCURRENTLY IF EXISTS idx_aq_phase_gates_run_phase;
