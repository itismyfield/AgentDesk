-- #2257 concern 5 — Stripe-style idempotency keys for write APIs
--
-- Stores caller-supplied `Idempotency-Key` headers alongside a fingerprint
-- of the request and (once the handler finishes) the response payload, so
-- duplicate calls return the same response without re-running the
-- mutation. Multi-node safe via the PG primary key.
--
-- Lifecycle:
--   1. Handler sees an Idempotency-Key header.
--   2. INSERT ... ON CONFLICT (key) DO NOTHING RETURNING ...
--   3. INSERT succeeded → run business logic → UPDATE row with response.
--   4. INSERT conflicted → load row.
--        - response_status IS NULL → another caller is mid-flight; reject 409.
--        - response_status IS NOT NULL AND request_fingerprint matches →
--          replay cached response.
--        - request_fingerprint differs → reject 422 (key reuse for a
--          different request body).
--
-- The `scope` column lets the same key string be reused across unrelated
-- endpoints (e.g. "phase-gate-repair" vs "rereview") without collision.
-- Background GC sweeps expired rows; rows are intended to live ~24h.

CREATE TABLE IF NOT EXISTS idempotency_keys (
  -- Compound primary key (scope, key) so an operator's UUID can be reused
  -- across endpoints without aliasing.
  scope               TEXT        NOT NULL,
  key                 TEXT        NOT NULL,
  -- Fingerprint of the full request (method + path + canonical body).
  -- Lets the handler reject reuse with a different payload.
  request_fingerprint TEXT        NOT NULL,
  -- HTTP status of the cached response. NULL while the original request
  -- is still in-flight so concurrent retries can detect "someone else is
  -- running this right now" and bail out with 409.
  response_status     SMALLINT,
  -- Cached response body verbatim. Bounded indirectly by the response
  -- size of the underlying endpoints; the GC sweep keeps the table small.
  response_body       JSONB,
  -- Caller identity at the time of insert. Helps the audit story when
  -- the same key is reused and we have to choose whether to allow replay.
  -- Stored as TEXT so it can hold "agent:foo", "channel:1234", "api"
  -- without modeling each shape.
  caller              TEXT,
  created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  completed_at        TIMESTAMPTZ,
  -- Hard expiry — rows older than this are deletable by the GC sweep.
  expires_at          TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (scope, key)
);

-- Sweep index — GC reads "rows whose expires_at < now()". Without this
-- index the cleanup pass would scan the full table at every tick.
CREATE INDEX IF NOT EXISTS idempotency_keys_expires_at_idx
  ON idempotency_keys (expires_at);
