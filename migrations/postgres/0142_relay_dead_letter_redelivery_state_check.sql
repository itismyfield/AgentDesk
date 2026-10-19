-- Enforce new writes; validate legacy rows in a separate migration after checking their state distribution.
-- 0120's "never back" comment is obsolete: deferred claimed attempts can return to pending.
ALTER TABLE relay_dead_letter
    ADD CONSTRAINT relay_dead_letter_redelivery_state_check
    CHECK (redelivery_state IN ('pending', 'claimed', 'delivered', 'declined', 'superseded')) NOT VALID;
