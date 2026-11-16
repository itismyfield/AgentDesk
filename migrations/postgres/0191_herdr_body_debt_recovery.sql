-- Recovery authority is separate from ordinary intake/native output admission.
CREATE TABLE o_herdr_debt_runs (
    provider TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    binding JSONB NOT NULL CHECK (jsonb_typeof(binding) = 'object'),
    active BOOLEAN NOT NULL,
    PRIMARY KEY(provider, channel_id)
);
CREATE TABLE o_herdr_debt_effects (
    id UUID PRIMARY KEY,
    provider TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    episode TEXT NOT NULL,
    input_key TEXT NOT NULL,
    coverage TEXT NOT NULL,
    plan_sha TEXT NOT NULL,
    snapshots JSONB NOT NULL CHECK (jsonb_typeof(snapshots) = 'array'),
    provenance JSONB NOT NULL,
    payload TEXT NOT NULL,
    payload_sha TEXT NOT NULL,
    unit_key JSONB NOT NULL CHECK ((unit_key->>'kind') IS NOT DISTINCT FROM 'body'),
    piece_index INTEGER NOT NULL CHECK (piece_index >= 0),
    bot_id BIGINT NOT NULL CHECK (bot_id > 0),
    state TEXT NOT NULL CHECK (state IN ('ready','reserved','confirmed')),
    attempt_id UUID,
    receipt JSONB,
    CHECK (state <> 'confirmed' OR receipt IS NOT NULL),
    UNIQUE(provider,channel_id,episode,input_key,coverage)
);
CREATE TABLE o_herdr_debt_grants (
    id UUID PRIMARY KEY,
    effect_id UUID NOT NULL REFERENCES o_herdr_debt_effects(id),
    episode TEXT NOT NULL,
    claim_core JSONB NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('issued','reserved','consumed','revoked'))
);
CREATE TABLE o_herdr_debt_attempts (
    id UUID PRIMARY KEY,
    effect_id UUID NOT NULL REFERENCES o_herdr_debt_effects(id),
    grant_id UUID NOT NULL REFERENCES o_herdr_debt_grants(id),
    payload_sha TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('reserved','confirmed','no_effect')),
    no_effect_witness JSONB,
    CHECK ((phase = 'no_effect') = (no_effect_witness IS NOT NULL)),
    CHECK (no_effect_witness IS NULL OR
       ((no_effect_witness->>'version') IS NOT DISTINCT FROM '1' AND
        ((no_effect_witness->>'kind') IN ('owning_not_polled','transport_no_effect')) IS TRUE AND
        (no_effect_witness->>'attempt') IS NOT DISTINCT FROM id::TEXT AND
        (no_effect_witness->>'effect') IS NOT DISTINCT FROM effect_id::TEXT AND
        (no_effect_witness->>'payload_sha') IS NOT DISTINCT FROM payload_sha))
);
ALTER TABLE o_herdr_debt_effects ADD CONSTRAINT o_herdr_debt_attempt_fk
    FOREIGN KEY(attempt_id) REFERENCES o_herdr_debt_attempts(id);
