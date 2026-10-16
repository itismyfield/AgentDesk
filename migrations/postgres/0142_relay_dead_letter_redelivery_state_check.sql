-- #6021: reject unknown redelivery states on every new or updated row.
-- Adding NOT VALID first enforces future writes without scanning under the stronger
-- ADD CONSTRAINT lock. Validation then checks all existing rows and fails the
-- migration if any unknown state exists; do not guess-map one to pending, which
-- could duplicate delivery. Investigate and repair such rows explicitly.
ALTER TABLE relay_dead_letter
    ADD CONSTRAINT relay_dead_letter_redelivery_state_check
    CHECK (redelivery_state IN ('pending', 'claimed', 'delivered', 'declined', 'superseded')) NOT VALID;

ALTER TABLE relay_dead_letter
    VALIDATE CONSTRAINT relay_dead_letter_redelivery_state_check;
