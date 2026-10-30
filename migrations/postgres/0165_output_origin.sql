ALTER TABLE message_outbox ADD COLUMN output_origin JSONB;
ALTER TABLE message_outbox ADD CONSTRAINT message_outbox_output_origin_object
CHECK (output_origin IS NULL OR jsonb_typeof(output_origin)='object');
