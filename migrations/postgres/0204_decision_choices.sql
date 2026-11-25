-- The one choice of a request: the first valid selection committed under the request's row lock,
-- with the input event it created.
CREATE TABLE IF NOT EXISTS decision_choices (
    request_id UUID PRIMARY KEY REFERENCES decision_requests (request_id),
    ordinal SMALLINT NOT NULL,
    actor_user_id TEXT NOT NULL,
    source TEXT NOT NULL CHECK (source IN ('button')),
    selection_event_id UUID NOT NULL UNIQUE REFERENCES decision_input_events (event_id),
    chosen_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- open: the buttons take a selection; selecting: the choice is stored and being delivered;
-- closed: its delivery ended, and the thread is archived and locked once the result is shown.
ALTER TABLE decision_requests
    DROP CONSTRAINT IF EXISTS decision_requests_lifecycle_check,
    ADD CONSTRAINT decision_requests_lifecycle_check CHECK (lifecycle IN
        ('stored', 'posting', 'disabled', 'post_failed', 'open', 'selecting', 'closed')),
    ADD COLUMN IF NOT EXISTS inactive_reason TEXT,
    ADD COLUMN IF NOT EXISTS sealed_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS moderation_error TEXT;
