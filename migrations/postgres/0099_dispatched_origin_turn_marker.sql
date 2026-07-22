-- #4642: distinguish dispatched-origin turns from ordinary interactive turns during restore.
ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS active_turn_nonce TEXT,
    ADD COLUMN IF NOT EXISTS dispatched_origin_turn_nonce TEXT;
