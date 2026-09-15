-- Key rate_limit_cache by (provider, profile_id) so extra CLI auth
-- profiles do not overwrite the implicit default/global row.
ALTER TABLE rate_limit_cache
    ADD COLUMN IF NOT EXISTS profile_id TEXT NOT NULL DEFAULT 'default';

UPDATE rate_limit_cache
   SET profile_id = 'default'
 WHERE profile_id IS NULL
    OR btrim(profile_id) = '';

ALTER TABLE rate_limit_cache
    DROP CONSTRAINT IF EXISTS rate_limit_cache_pkey;

ALTER TABLE rate_limit_cache
    ADD CONSTRAINT rate_limit_cache_pkey PRIMARY KEY (provider, profile_id);

COMMENT ON COLUMN rate_limit_cache.profile_id IS
    'Auth profile id; implicit global home is default.';
