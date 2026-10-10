-- Re-post budget of one O piece, shared by every node. A row exists only once the piece's first
-- post went uncertain or an operator adopted it; no row means the piece was never admitted.
CREATE TABLE IF NOT EXISTS o_piece_delivery (
    channel_id TEXT NOT NULL CHECK (channel_id ~ '^[1-9][0-9]*$'),
    provider TEXT NOT NULL CHECK (provider IN ('claude', 'codex')),
    native_key TEXT NOT NULL CHECK (BTRIM(native_key) <> ''),
    kind TEXT NOT NULL CHECK (kind IN ('body', 'tool_result')),
    piece_index BIGINT NOT NULL CHECK (piece_index >= 0),
    payload TEXT NOT NULL CHECK (payload <> ''),
    payload_sha256 TEXT NOT NULL CHECK (payload_sha256 ~ '^[0-9a-f]{64}$'),
    identity_version INTEGER NOT NULL CHECK (identity_version >= 1),
    split_version INTEGER NOT NULL CHECK (split_version >= 1),
    original_anchor BIGINT NOT NULL CHECK (original_anchor >= 0),
    sender_id BIGINT NOT NULL CHECK (sender_id > 0),
    admitted_by TEXT NOT NULL CHECK (admitted_by IN ('uncertain', 'operator')),
    origin_serial BIGINT NOT NULL CHECK (origin_serial >= 0),
    origin_node TEXT NOT NULL CHECK (BTRIM(origin_node) <> ''),
    failure TEXT CHECK (
        failure IN ('not_found', 'unknown_at_cap', 'rejected', 'cap', 'cap_unknown')
    ),
    -- The last differing payload offered under this key; it never replaces the stored one.
    conflict_sha256 TEXT CHECK (conflict_sha256 ~ '^[0-9a-f]{64}$'),
    conflict_at TIMESTAMPTZ,
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision >= 1),
    admitted_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (channel_id, provider, native_key, kind, piece_index),
    -- Codex tool results are never posted, so they are never re-posted either.
    CONSTRAINT o_piece_delivery_postable_kind CHECK (NOT (provider = 'codex' AND kind = 'tool_result')),
    CONSTRAINT o_piece_delivery_conflict_pair CHECK ((conflict_sha256 IS NULL) = (conflict_at IS NULL))
);

-- Consumed POST slots: slot 0 is the original, 1 and 2 the only further sends. A slot row is a
-- consumed fact and is never deleted or reused, whatever its result.
CREATE TABLE IF NOT EXISTS o_piece_attempts (
    channel_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    native_key TEXT NOT NULL,
    kind TEXT NOT NULL,
    piece_index BIGINT NOT NULL,
    slot SMALLINT NOT NULL CHECK (slot IN (0, 1, 2)),
    grant_id UUID NOT NULL UNIQUE,
    intent TEXT NOT NULL CHECK (
        intent IN ('original', 'prior_retry', 'auto_reconfirm', 'operator_resume')
    ),
    approval_id TEXT CHECK (BTRIM(approval_id) <> ''),
    owner TEXT NOT NULL CHECK (BTRIM(owner) <> ''),
    run_id TEXT NOT NULL CHECK (BTRIM(run_id) <> ''),
    consumed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deadline TIMESTAMPTZ,
    result TEXT CHECK (result IN ('created', 'rejected', 'uncertain', 'not_sent', 'abandoned')),
    settled_at TIMESTAMPTZ,
    PRIMARY KEY (channel_id, provider, native_key, kind, piece_index, slot),
    FOREIGN KEY (channel_id, provider, native_key, kind, piece_index)
        REFERENCES o_piece_delivery (channel_id, provider, native_key, kind, piece_index)
        ON DELETE RESTRICT,
    CONSTRAINT o_piece_attempts_original_slot CHECK ((slot = 0) = (intent = 'original')),
    CONSTRAINT o_piece_attempts_approval CHECK (
        (intent = 'operator_resume') = (approval_id IS NOT NULL)
    ),
    -- An open attempt carries the deadline after which another holder may settle it.
    CONSTRAINT o_piece_attempts_settled CHECK (
        (result IS NULL) = (settled_at IS NULL) AND (result IS NOT NULL OR deadline IS NOT NULL)
    )
);

-- A Discord message counts for at most one piece; a piece may collect several (duplicates).
CREATE TABLE IF NOT EXISTS o_piece_receipts (
    channel_id TEXT NOT NULL,
    message_id BIGINT NOT NULL CHECK (message_id > 0),
    provider TEXT NOT NULL,
    native_key TEXT NOT NULL,
    kind TEXT NOT NULL,
    piece_index BIGINT NOT NULL,
    slot SMALLINT CHECK (slot IN (0, 1, 2)),
    author_id BIGINT NOT NULL CHECK (author_id > 0),
    method TEXT NOT NULL CHECK (
        method IN ('post_response', 'local_posted', 'marker', 'nonce', 'exact_match')
    ),
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (channel_id, message_id),
    FOREIGN KEY (channel_id, provider, native_key, kind, piece_index)
        REFERENCES o_piece_delivery (channel_id, provider, native_key, kind, piece_index)
        ON DELETE RESTRICT
);

CREATE INDEX IF NOT EXISTS o_piece_receipts_piece
    ON o_piece_receipts (channel_id, provider, native_key, kind, piece_index);
