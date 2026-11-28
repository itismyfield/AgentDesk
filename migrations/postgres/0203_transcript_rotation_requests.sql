CREATE TABLE transcript_rotation_requests (
    provider TEXT NOT NULL,
    native_session_id UUID NOT NULL,
    op_id UUID NOT NULL,
    channel_id TEXT NOT NULL,
    requested_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (provider, native_session_id)
);
