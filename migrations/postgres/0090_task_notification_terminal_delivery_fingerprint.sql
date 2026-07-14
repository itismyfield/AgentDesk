-- #4295: exact terminal task-notification replay guard.
--
-- Transcript compaction can move a previously delivered user entry to a new
-- byte offset. The provider entry id survives that rewrite, so retain its
-- fingerprint beside the delivered Discord message id in the existing durable
-- card authority. The global (channel, provider, fingerprint) uniqueness is
-- intentionally independent of tmux/session and synthetic anchor identities:
-- those can be recreated after restart, while the provider event cannot.
ALTER TABLE task_notification_card_state
    ADD COLUMN terminal_delivery_fingerprint VARCHAR(64)
        CHECK (
            terminal_delivery_fingerprint IS NULL
            OR char_length(terminal_delivery_fingerprint) = 64
        );

CREATE UNIQUE INDEX idx_task_notification_terminal_delivery_fingerprint
    ON task_notification_card_state
        (channel_id, provider, terminal_delivery_fingerprint)
    WHERE terminal_delivery_fingerprint IS NOT NULL;
