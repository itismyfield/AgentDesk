-- Validate existing redelivery states so the constraint covers every stored row.
ALTER TABLE relay_dead_letter VALIDATE CONSTRAINT relay_dead_letter_redelivery_state_check;
