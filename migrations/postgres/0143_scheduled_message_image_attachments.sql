-- Definition images remain dormant until transport and capability gates exist.
-- Nullable columns preserve old writers and existing text-only definitions.
ALTER TABLE scheduled_messages
    ADD COLUMN image_filename TEXT,
    ADD COLUMN image_content_type TEXT,
    ADD COLUMN image_data BYTEA,
    ADD CONSTRAINT chk_smsg_image_attachment_complete CHECK (
        (image_filename IS NULL AND image_content_type IS NULL AND image_data IS NULL)
        OR
        (image_filename IS NOT NULL AND image_content_type IS NOT NULL AND image_data IS NOT NULL)
    ),
    ADD CONSTRAINT chk_smsg_image_attachment_push_only CHECK (
        image_data IS NULL OR delivery_kind = 'push'
    ),
    ADD CONSTRAINT chk_smsg_image_attachment_size CHECK (
        image_data IS NULL OR octet_length(image_data) BETWEEN 1 AND 8388608
    );
