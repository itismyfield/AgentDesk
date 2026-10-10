-- Exact attempt transitions for replay receipts; 0136 fences stay as they are and only gain refusals.
-- A retried attempt records the nonce it replaced so the session can follow it exactly once.
ALTER TABLE intake_outbox ADD COLUMN IF NOT EXISTS replay_retry_of_nonce TEXT;

-- A start keeps its registered nonce, a retry names the nonce it replaces and its saved projection,
-- a classification keeps its nonce and a hold keeps a preserved result.
CREATE OR REPLACE FUNCTION intake_outbox_replay_exact_fence() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.replay_disposition = 'started_unclassified'
        AND OLD.replay_disposition IS DISTINCT FROM 'started_unclassified'
        AND ((OLD.replay_disposition = 'registered_not_started'
              AND NEW.replay_episode_nonce IS DISTINCT FROM OLD.replay_episode_nonce)
             OR (OLD.replay_disposition = 'startup_failed_no_effect'
                 AND (NEW.replay_retry_of_nonce IS DISTINCT FROM OLD.replay_episode_nonce
                      OR NEW.replay_preserved #>> '{projection,nonce}'
                         IS DISTINCT FROM NEW.replay_episode_nonce))))
       OR (OLD.replay_disposition = 'started_unclassified'
           AND NEW.replay_disposition IN ('startup_failed_no_effect', 'classified_normal', 'withheld')
           AND NEW.replay_episode_nonce IS DISTINCT FROM OLD.replay_episode_nonce)
       OR (NEW.replay_disposition = 'withheld' AND OLD.replay_disposition IS DISTINCT FROM 'withheld'
           AND NEW.replay_preserved IS NULL)
       OR (NEW.replay_retry_of_nonce IS DISTINCT FROM OLD.replay_retry_of_nonce
           AND NOT (OLD.replay_disposition = 'startup_failed_no_effect'
                    AND NEW.replay_disposition = 'started_unclassified'))
       OR (OLD.replay_disposition NOT IN ('registered_not_started', 'startup_failed_no_effect')
           AND NEW.replay_preserved #> '{projection}' IS DISTINCT FROM OLD.replay_preserved #> '{projection}')
    THEN
        RAISE EXCEPTION 'replay_disposition_fence: row % breaks its exact % attempt',
            OLD.id, OLD.replay_disposition
            USING CONSTRAINT = 'replay_disposition_fence';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS trg_intake_outbox_replay_exact_fence ON intake_outbox;
CREATE TRIGGER trg_intake_outbox_replay_exact_fence
    BEFORE UPDATE ON intake_outbox
    FOR EACH ROW WHEN (OLD.replay_disposition IS NOT NULL)
    EXECUTE FUNCTION intake_outbox_replay_exact_fence();

-- 0136 session fence plus one exception: an exact live retry moves the session to its receipt's
-- next nonce and drops only the stale resume selector.
CREATE OR REPLACE FUNCTION sessions_replay_fence() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE fresh_handoff BOOLEAN := FALSE; exact_retry BOOLEAN := FALSE;
BEGIN
    IF NOT replay_session_held(OLD.current_replay_receipt_id, OLD.active_dispatch_id) THEN
        IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
        RETURN NEW;
    END IF;
    IF TG_OP = 'UPDATE' THEN
        SELECT EXISTS (SELECT 1 FROM intake_outbox r
                        WHERE r.id = NEW.current_replay_receipt_id
                          AND r.replay_disposition = 'registered_not_started'
                          AND NEW.active_dispatch_id IS NULL
                          AND r.id IS DISTINCT FROM OLD.current_replay_receipt_id
                          AND r.replay_episode_nonce = NEW.replay_episode_nonce
                          AND r.channel_id = OLD.channel_id
                          AND lower(btrim(r.provider)) = lower(btrim(OLD.provider))
                          AND NOT replay_sources_blocked(r.provider, r.channel_id,
                                                        r.replay_source_message_ids))
          INTO fresh_handoff;
        SELECT EXISTS (SELECT 1 FROM intake_outbox r
                        WHERE r.id = OLD.current_replay_receipt_id
                          AND r.id = NEW.current_replay_receipt_id
                          AND r.replay_disposition = 'started_unclassified'
                          AND r.replay_retry_of_nonce = OLD.replay_episode_nonce
                          AND r.replay_episode_nonce = NEW.replay_episode_nonce)
          INTO exact_retry;
        IF (NEW.claude_session_id IS NOT DISTINCT FROM OLD.claude_session_id
            OR (exact_retry AND NEW.claude_session_id IS NULL))
           AND (NEW.raw_provider_session_id IS NOT DISTINCT FROM OLD.raw_provider_session_id
                OR (exact_retry AND NEW.raw_provider_session_id IS NULL))
           AND NEW.session_key IS NOT DISTINCT FROM OLD.session_key
           AND NEW.cwd IS NOT DISTINCT FROM OLD.cwd
           AND NEW.instance_id IS NOT DISTINCT FROM OLD.instance_id
           AND NEW.provider IS NOT DISTINCT FROM OLD.provider AND NEW.channel_id IS NOT DISTINCT FROM OLD.channel_id
           AND NEW.identity_kind IS NOT DISTINCT FROM OLD.identity_kind AND NEW.discord_token_hash IS NOT DISTINCT FROM OLD.discord_token_hash
           AND NEW.status NOT IN ('idle', 'disconnected', 'aborted')
           AND (fresh_handoff OR (
               NEW.active_dispatch_id IS NOT DISTINCT FROM OLD.active_dispatch_id
               AND NEW.current_replay_receipt_id IS NOT DISTINCT FROM OLD.current_replay_receipt_id
               AND (exact_retry OR NEW.replay_episode_nonce IS NOT DISTINCT FROM OLD.replay_episode_nonce)
               AND NEW.active_turn_nonce IS NOT DISTINCT FROM OLD.active_turn_nonce
               AND NEW.dispatched_origin_turn_nonce IS NOT DISTINCT FROM OLD.dispatched_origin_turn_nonce)) THEN
            RETURN NEW;
        END IF;
    END IF;
    RAISE EXCEPTION 'replay_disposition_fence: session % keeps its held request', OLD.id
        USING CONSTRAINT = 'replay_disposition_fence';
END
$$;
