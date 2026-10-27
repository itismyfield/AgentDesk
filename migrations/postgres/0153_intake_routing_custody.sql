ALTER TABLE intake_outbox
    ADD COLUMN routing_receipt_id BIGINT REFERENCES intake_outbox(id),
    ADD COLUMN routing_state TEXT,
    ADD COLUMN routing_home_revision BIGINT,
    ADD COLUMN routing_wait_owner_generation BIGINT,
    ADD COLUMN routing_not_before TIMESTAMPTZ,
    ADD CONSTRAINT intake_routing_custody_shape CHECK ((
        (routing_state IS NULL AND routing_receipt_id IS NULL)
        OR (routing_state IN ('held','admitted','superseded')
            AND routing_receipt_id IS NOT NULL AND NOT replay_only
            AND routing_home_revision IS NOT NULL AND routing_home_revision >= 0 AND routing_not_before IS NOT NULL
            AND (routing_state <> 'held' OR
                 (status='unknown' AND claim_owner IS NULL AND claimed_at IS NULL
                  AND accepted_at IS NULL AND spawned_at IS NULL
                  AND owner_instance_id IS NULL AND owner_generation IS NULL)))) IS TRUE);
CREATE UNIQUE INDEX intake_routing_receipt_uq ON intake_outbox(routing_receipt_id)
    WHERE routing_receipt_id IS NOT NULL;
CREATE INDEX intake_routing_held_head ON intake_outbox(provider,channel_id,id)
    WHERE routing_state='held';
ALTER TABLE intake_session_owners ADD COLUMN routing_cleanup_deferred BOOLEAN NOT NULL DEFAULT FALSE;

CREATE FUNCTION intake_routing_bundle_pinned(bundle UUID) RETURNS BOOLEAN
LANGUAGE SQL STABLE AS $$
    SELECT EXISTS (
        SELECT 1 FROM intake_outbox delivery
        JOIN intake_outbox receipt ON receipt.id=delivery.routing_receipt_id
        WHERE delivery.routing_state IS NOT NULL
          AND receipt.replay_disposition <> 'classified_normal'
          AND delivery.attachment_refs @> jsonb_build_array(jsonb_build_object('bundle_id',bundle::TEXT))
    )
$$;
