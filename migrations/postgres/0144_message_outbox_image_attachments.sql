-- Payload and delivery identity live in the same durable outbox row.
ALTER TABLE message_outbox
    ADD COLUMN attachment_filename TEXT,
    ADD COLUMN attachment_content_type TEXT,
    ADD COLUMN attachment_data BYTEA,
    ADD CONSTRAINT message_outbox_attachment_complete CHECK (
        (attachment_filename IS NULL AND attachment_content_type IS NULL AND attachment_data IS NULL)
        OR (attachment_filename IS NOT NULL AND attachment_content_type IS NOT NULL AND attachment_data IS NOT NULL)
    ),
    ADD CONSTRAINT message_outbox_attachment_size CHECK (
        attachment_data IS NULL OR octet_length(attachment_data) BETWEEN 1 AND 8388608
    );
