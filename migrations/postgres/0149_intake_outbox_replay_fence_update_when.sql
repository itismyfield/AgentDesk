-- The update fence took one advisory lock per source on every updated row, so a bulk
-- UPDATE of plain rows ran the shared lock table out. It now fires only on updates
-- that one of its branches can refuse.
DROP TRIGGER IF EXISTS trg_intake_outbox_replay_fence_update ON intake_outbox;
CREATE TRIGGER trg_intake_outbox_replay_fence_update
    BEFORE UPDATE ON intake_outbox
    FOR EACH ROW
    WHEN (OLD.replay_disposition IS NOT NULL
          OR NEW.replay_disposition IS NOT NULL
          OR OLD.replay_source_message_ids IS NOT NULL
          OR (NEW.status IN ('pending', 'claimed', 'accepted', 'spawned')
              AND (NEW.status IS DISTINCT FROM OLD.status
                   OR NEW.claim_owner IS DISTINCT FROM OLD.claim_owner
                   OR NEW.target_instance_id IS DISTINCT FROM OLD.target_instance_id)))
    EXECUTE FUNCTION intake_outbox_replay_fence();
