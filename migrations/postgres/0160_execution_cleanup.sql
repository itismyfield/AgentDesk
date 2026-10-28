ALTER TABLE intake_session_owners
    ADD COLUMN routing_cleanup JSONB,
    ADD COLUMN routing_cleanup_state TEXT,
    ADD COLUMN routing_cleanup_deadline TIMESTAMPTZ,
    ADD COLUMN routing_cleanup_seen TIMESTAMPTZ,
    ADD COLUMN routing_cleanup_next TIMESTAMPTZ,
    ADD COLUMN routing_cleanup_attempts INT NOT NULL DEFAULT 0,
    ADD COLUMN routing_cleanup_claim UUID,
    ADD CONSTRAINT execution_cleanup_shape CHECK (
        (routing_cleanup IS NULL AND routing_cleanup_state IS NULL)
        OR (routing_cleanup IS NOT NULL AND routing_cleanup_deadline IS NOT NULL
            AND routing_cleanup_next IS NOT NULL AND routing_cleanup_attempts BETWEEN 0 AND 3
            AND routing_cleanup_state IN ('pending','busy','running','retry','deferred','needs_operator','identity_changed','ended')));
CREATE INDEX execution_cleanup_due ON intake_session_owners(owner_instance_id,routing_cleanup_next)
    WHERE routing_cleanup_state IN ('pending','busy','running','retry','deferred');

CREATE OR REPLACE FUNCTION intake_routing_bundle_pinned(bundle UUID) RETURNS BOOLEAN
LANGUAGE SQL STABLE AS $$
    SELECT EXISTS (
        SELECT 1 FROM intake_outbox delivery
        JOIN intake_outbox receipt ON receipt.id=delivery.routing_receipt_id
        WHERE delivery.routing_state IN ('held','admitted')
          AND (delivery.routing_state='held' OR delivery.status IN ('pending','claimed','accepted','spawned'))
          AND receipt.replay_disposition IN ('registered_not_started','started_unclassified')
          AND delivery.attachment_refs @> jsonb_build_array(jsonb_build_object('bundle_id',bundle::TEXT))
    )
$$;

CREATE FUNCTION intake_routing_bundle_retention() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.replay_disposition='withheld' AND OLD.replay_disposition IS DISTINCT FROM 'withheld' THEN
        PERFORM pg_advisory_xact_lock(hashtext('agentdesk.attachment-bundles.v1')::BIGINT);
        UPDATE intake_attachment_bundles b SET expires_at=GREATEST(b.expires_at,NOW()+INTERVAL '7 days')
        WHERE EXISTS (SELECT 1 FROM intake_outbox d
            WHERE (d.routing_receipt_id=NEW.id OR d.id=NEW.id)
              AND d.attachment_refs @> jsonb_build_array(jsonb_build_object('bundle_id',b.id::TEXT)));
    END IF;
    RETURN NEW;
END
$$;
CREATE TRIGGER intake_routing_bundle_retention AFTER UPDATE OF replay_disposition ON intake_outbox
    FOR EACH ROW EXECUTE FUNCTION intake_routing_bundle_retention();
UPDATE intake_attachment_bundles b SET expires_at=GREATEST(b.expires_at,NOW()+INTERVAL '7 days')
WHERE EXISTS(SELECT 1 FROM intake_outbox d JOIN intake_outbox r ON r.id=d.routing_receipt_id
    WHERE r.replay_disposition='withheld'
      AND d.attachment_refs @> jsonb_build_array(jsonb_build_object('bundle_id',b.id::TEXT)));
