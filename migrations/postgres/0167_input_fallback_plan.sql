ALTER TABLE intake_outbox ADD COLUMN routing_plan JSONB;
ALTER TABLE intake_outbox ADD CONSTRAINT input_fallback_plan_object
    CHECK (routing_plan IS NULL OR COALESCE(jsonb_typeof(routing_plan)='object' AND routing_plan->>'version'='1',FALSE));

ALTER TABLE intake_session_owners
    ADD COLUMN routing_adoption JSONB,
    ADD COLUMN routing_notice JSONB,
    ADD COLUMN routing_notice_outbox_id BIGINT REFERENCES message_outbox(id),
    ADD COLUMN routing_notice_delivery_state TEXT,
    ADD COLUMN routing_notice_started_at TIMESTAMPTZ,
    ADD COLUMN routing_notice_message_id TEXT,
    ADD CONSTRAINT input_fallback_adoption_object CHECK (
        routing_adoption IS NULL OR (adopted_from_session AND generation=0 AND jsonb_typeof(routing_adoption)='object')),
    ADD CONSTRAINT input_fallback_notice_pointer CHECK (
        (routing_notice IS NULL AND routing_notice_outbox_id IS NULL)
        OR (routing_notice IS NOT NULL AND jsonb_typeof(routing_notice)='object' AND routing_notice_outbox_id IS NOT NULL));

ALTER TABLE intake_session_owners ADD CONSTRAINT input_fallback_notice_delivery CHECK (
    (routing_notice IS NULL AND routing_notice_delivery_state IS NULL
        AND routing_notice_started_at IS NULL AND routing_notice_message_id IS NULL)
    OR (routing_notice IS NOT NULL AND routing_notice_delivery_state IS NOT NULL AND (
        (routing_notice_delivery_state='pending' AND routing_notice_started_at IS NULL AND routing_notice_message_id IS NULL)
        OR (routing_notice_delivery_state IN ('posting','uncertain') AND routing_notice_started_at IS NOT NULL AND routing_notice_message_id IS NULL)
        OR (routing_notice_delivery_state='sent' AND routing_notice_started_at IS NOT NULL
            AND routing_notice_message_id IS NOT NULL AND btrim(routing_notice_message_id)<>''))));
