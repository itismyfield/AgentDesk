-- Durable image attachments for scheduled push messages.
--
-- The definition and its outbox handoff both retain the bytes. A filesystem
-- path is intentionally not used: retries may happen after restart or on a
-- different node, where the original path is not guaranteed to exist.

ALTER TABLE scheduled_messages
    ADD COLUMN IF NOT EXISTS image_filename TEXT,
    ADD COLUMN IF NOT EXISTS image_content_type TEXT,
    ADD COLUMN IF NOT EXISTS image_data BYTEA;

ALTER TABLE message_outbox
    ADD COLUMN IF NOT EXISTS attachment_filename TEXT,
    ADD COLUMN IF NOT EXISTS attachment_content_type TEXT,
    ADD COLUMN IF NOT EXISTS attachment_data BYTEA;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'chk_smsg_image_attachment_complete'
    ) THEN
        ALTER TABLE scheduled_messages
            ADD CONSTRAINT chk_smsg_image_attachment_complete
            CHECK (
                (image_filename IS NULL AND image_content_type IS NULL AND image_data IS NULL)
                OR
                (image_filename IS NOT NULL AND image_content_type IS NOT NULL AND image_data IS NOT NULL)
            ) NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'chk_smsg_image_attachment_push_only'
    ) THEN
        ALTER TABLE scheduled_messages
            ADD CONSTRAINT chk_smsg_image_attachment_push_only
            CHECK (image_data IS NULL OR delivery_kind = 'push') NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'chk_smsg_image_attachment_size'
    ) THEN
        ALTER TABLE scheduled_messages
            ADD CONSTRAINT chk_smsg_image_attachment_size
            CHECK (image_data IS NULL OR octet_length(image_data) <= 8388608) NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'chk_message_outbox_attachment_complete'
    ) THEN
        ALTER TABLE message_outbox
            ADD CONSTRAINT chk_message_outbox_attachment_complete
            CHECK (
                (attachment_filename IS NULL
                    AND attachment_content_type IS NULL
                    AND attachment_data IS NULL)
                OR
                (attachment_filename IS NOT NULL
                    AND attachment_content_type IS NOT NULL
                    AND attachment_data IS NOT NULL)
            ) NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'chk_message_outbox_attachment_size'
    ) THEN
        ALTER TABLE message_outbox
            ADD CONSTRAINT chk_message_outbox_attachment_size
            CHECK (attachment_data IS NULL OR octet_length(attachment_data) <= 8388608) NOT VALID;
    END IF;
END $$;
