-- One row per delegated channel. While the row exists it is the only source of the channel's
-- O home; no row means the channel stays on the gateway rules.
CREATE TABLE IF NOT EXISTS o_channel_homes (
    channel_id TEXT PRIMARY KEY CHECK (BTRIM(channel_id) <> ''),
    provider TEXT NOT NULL CHECK (BTRIM(provider) <> ''),
    state TEXT NOT NULL CHECK (
        state IN ('releasing', 'released', 'worker', 'reclaiming', 'reclaimed', 'orphaned')
    ),
    holder TEXT CHECK (BTRIM(holder) <> ''),
    target TEXT CHECK (BTRIM(target) <> ''),
    epoch BIGINT NOT NULL CHECK (epoch >= 1),
    renewed_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    detail TEXT,
    -- Exactly the holding states name a holder, and a holder always carries its lease time.
    CONSTRAINT o_channel_homes_holder_by_state CHECK (
        (state IN ('releasing', 'worker', 'reclaiming')) = (holder IS NOT NULL)
        AND (holder IS NULL OR renewed_at IS NOT NULL)
    ),
    -- A hand-off names its receiver; a worker-owned row has nobody to hand to.
    CONSTRAINT o_channel_homes_target_by_state CHECK (
        CASE state
            WHEN 'worker' THEN target IS NULL
            WHEN 'orphaned' THEN TRUE
            ELSE target IS NOT NULL
        END
        AND (holder IS NULL OR target IS NULL OR holder <> target)
    )
);
