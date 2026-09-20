-- The configured cluster PostgreSQL is the sole campaign authority. A campaign
-- and its DAG are one revisioned aggregate, so dependencies and node changes
-- cannot become visible separately. Every accepted revision is retained.
CREATE TABLE campaigns (
    id TEXT PRIMARY KEY,
    revision BIGINT NOT NULL CHECK (revision > 0),
    document JSONB NOT NULL CHECK (jsonb_typeof(document) = 'object'),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (document->>'id' = id),
    CHECK ((document->>'revision')::BIGINT = revision)
);
CREATE INDEX campaigns_updated_at_idx ON campaigns (updated_at DESC, id);

CREATE TABLE campaign_revisions (
    campaign_id TEXT NOT NULL REFERENCES campaigns(id),
    revision BIGINT NOT NULL CHECK (revision > 0),
    document JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (campaign_id, revision)
);
