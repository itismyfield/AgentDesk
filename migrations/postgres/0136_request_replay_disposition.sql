-- Durable request authority fences replay after a provider start across restore, claim and retries.
-- This migration adds schema and fences; no production writer sets a disposition yet.
ALTER TABLE intake_outbox
    ADD COLUMN IF NOT EXISTS replay_only BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN IF NOT EXISTS replay_disposition TEXT,
    ADD COLUMN IF NOT EXISTS replay_source_message_ids TEXT[],
    ADD COLUMN IF NOT EXISTS replay_episode_nonce TEXT,
    ADD COLUMN IF NOT EXISTS replay_owner_incarnation TEXT,
    ADD COLUMN IF NOT EXISTS replay_request_hash TEXT,
    ADD COLUMN IF NOT EXISTS replay_request_key TEXT,
    ADD COLUMN IF NOT EXISTS replay_hold_reason TEXT,
    ADD COLUMN IF NOT EXISTS replay_preserved JSONB;

-- A disposition names every absorbed source; a replay-only receipt never enters delivery or claim.
ALTER TABLE intake_outbox
    ADD CONSTRAINT intake_outbox_replay_disposition_check CHECK (
        CASE WHEN replay_disposition IS NULL
             THEN replay_source_message_ids IS NULL AND NOT replay_only
             ELSE replay_disposition IN ('registered_not_started', 'started_unclassified',
                      'startup_failed_no_effect', 'classified_normal', 'withheld')
                  AND COALESCE(user_msg_id = ANY(replay_source_message_ids), FALSE)
                  AND cardinality(replay_source_message_ids) > 0
                  AND array_position(replay_source_message_ids, NULL) IS NULL
                  AND array_position(replay_source_message_ids, '') IS NULL
                  AND (NOT replay_only OR (status = 'unknown' AND attempt_no = 1))
        END
    ) NOT VALID;

-- Same name as the replaced tuple constraint so the Rust conflict classifier keeps matching it;
-- replay-only receipts sit outside the delivery attempt sequence.
ALTER TABLE intake_outbox DROP CONSTRAINT intake_outbox_unique_message_attempt;
CREATE UNIQUE INDEX intake_outbox_unique_message_attempt
    ON intake_outbox (channel_id, user_msg_id, attempt_no)
    WHERE NOT replay_only;

CREATE UNIQUE INDEX idx_intake_outbox_replay_request_key
    ON intake_outbox (replay_request_key) WHERE replay_request_key IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_intake_outbox_replay_blocked_sources
    ON intake_outbox USING GIN (replay_source_message_ids)
    WHERE replay_disposition IN ('started_unclassified', 'startup_failed_no_effect', 'withheld');

-- Dispatch projection of the receipt disposition, kept equal to the receipt by the fences below.
ALTER TABLE task_dispatches
    ADD COLUMN IF NOT EXISTS replay_receipt_id BIGINT,
    ADD COLUMN IF NOT EXISTS replay_disposition TEXT;
ALTER TABLE task_dispatches
    ADD CONSTRAINT task_dispatches_replay_projection_check
    CHECK (replay_receipt_id IS NOT NULL OR replay_disposition IS NULL) NOT VALID;
CREATE INDEX IF NOT EXISTS idx_task_dispatches_replay_receipt
    ON task_dispatches (replay_receipt_id)
    WHERE replay_receipt_id IS NOT NULL;

ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS current_replay_receipt_id BIGINT,
    ADD COLUMN IF NOT EXISTS replay_episode_nonce TEXT;

-- Every state but registered and classified forbids an ordinary automatic rerun.
CREATE OR REPLACE FUNCTION replay_disposition_blocks_rerun(disposition TEXT) RETURNS BOOLEAN
LANGUAGE sql IMMUTABLE AS $$
    SELECT COALESCE(disposition IN ('started_unclassified', 'startup_failed_no_effect', 'withheld'),
                    FALSE)
$$;

CREATE OR REPLACE FUNCTION replay_sources_blocked(p_provider TEXT, p_channel TEXT, p_sources TEXT[],
                                                  p_except BIGINT DEFAULT NULL)
RETURNS BOOLEAN LANGUAGE sql STABLE AS $$
    SELECT EXISTS (
        SELECT 1 FROM intake_outbox r
         WHERE r.replay_disposition IN ('started_unclassified', 'startup_failed_no_effect',
                                        'withheld')
           AND r.channel_id = p_channel
           AND r.replay_source_message_ids && p_sources
           AND lower(btrim(r.provider)) = lower(btrim(p_provider))
           AND r.id IS DISTINCT FROM p_except)
$$;

CREATE OR REPLACE FUNCTION replay_dispatch_blocked(p_dispatch_id TEXT) RETURNS BOOLEAN
LANGUAGE sql STABLE AS $$
    SELECT EXISTS (SELECT 1 FROM task_dispatches d
                    WHERE d.id = p_dispatch_id
                      AND replay_disposition_blocks_rerun(d.replay_disposition))
$$;

CREATE OR REPLACE FUNCTION replay_session_held(p_receipt_id BIGINT, p_dispatch_id TEXT)
RETURNS BOOLEAN LANGUAGE sql STABLE AS $$
    SELECT replay_dispatch_blocked(p_dispatch_id)
        OR EXISTS (SELECT 1 FROM intake_outbox r
                    WHERE r.id = p_receipt_id
                      AND replay_disposition_blocks_rerun(r.replay_disposition))
$$;

-- Sorted source locks order concurrent authority writes and old-writer admissions alike.
CREATE OR REPLACE FUNCTION replay_lock_sources(p_provider TEXT, p_channel TEXT, p_sources TEXT[])
RETURNS VOID LANGUAGE plpgsql AS $$
DECLARE source TEXT;
BEGIN
    FOR source IN SELECT DISTINCT unnest(p_sources) ORDER BY 1 LOOP
        PERFORM pg_advisory_xact_lock(hashtextextended(
            jsonb_build_array(lower(btrim(p_provider)), p_channel, source)::TEXT, 6008));
    END LOOP;
END
$$;

CREATE OR REPLACE FUNCTION replay_entry_blocked(p_entry TEXT, p_dispatch TEXT)
RETURNS BOOLEAN LANGUAGE sql STABLE AS $$
    SELECT replay_dispatch_blocked(p_dispatch) OR EXISTS (
        SELECT 1 FROM auto_queue_entry_dispatch_history h
         WHERE h.entry_id = p_entry AND replay_dispatch_blocked(h.dispatch_id))
$$;

-- Forward-only lifecycle; a no-effect start re-enters started only under a new episode nonce.
CREATE OR REPLACE FUNCTION replay_disposition_transition_allowed(old_d TEXT, new_d TEXT,
                                                                 nonce_changed BOOLEAN)
RETURNS BOOLEAN LANGUAGE sql IMMUTABLE AS $$
    SELECT COALESCE(old_d IS NULL
        OR (old_d = new_d AND (NOT nonce_changed OR old_d = 'registered_not_started'))
        OR (old_d = 'registered_not_started' AND new_d IN ('started_unclassified', 'withheld'))
        OR (old_d = 'started_unclassified'
            AND new_d IN ('withheld', 'startup_failed_no_effect', 'classified_normal'))
        OR (old_d = 'startup_failed_no_effect'
            AND (new_d = 'withheld' OR (new_d = 'started_unclassified' AND nonce_changed))),
        FALSE)
$$;

CREATE OR REPLACE FUNCTION intake_outbox_replay_fence() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    refusal TEXT;
BEGIN
    IF TG_OP <> 'DELETE' THEN
        PERFORM replay_lock_sources(NEW.provider, NEW.channel_id,
            COALESCE(NEW.replay_source_message_ids, ARRAY[NEW.user_msg_id]));
    END IF;
    IF TG_OP = 'DELETE' THEN
        IF replay_disposition_blocks_rerun(OLD.replay_disposition) THEN
            refusal := format('held request row %s cannot be deleted', OLD.id);
        END IF;
    ELSIF TG_OP = 'INSERT' THEN
        IF NEW.parent_outbox_id IS NOT NULL AND EXISTS (
            SELECT 1 FROM intake_outbox p WHERE p.id = NEW.parent_outbox_id AND p.replay_only) THEN
            refusal := format('replay-only receipt %s cannot parent a retry', NEW.parent_outbox_id);
        ELSIF replay_sources_blocked(NEW.provider, NEW.channel_id,
                                     COALESCE(NEW.replay_source_message_ids,
                                              ARRAY[NEW.user_msg_id])) THEN
            refusal := format('a source of message %s already started', NEW.user_msg_id);
        END IF;
    ELSIF OLD.replay_disposition IS NULL AND NEW.replay_disposition IS NOT NULL
          AND replay_sources_blocked(NEW.provider, NEW.channel_id,
                                     NEW.replay_source_message_ids, NEW.id) THEN
        refusal := format('row %s cannot register a source that already started', NEW.id);
    ELSIF OLD.replay_disposition IS NOT NULL AND (
            NEW.replay_source_message_ids IS DISTINCT FROM OLD.replay_source_message_ids
            OR NEW.provider IS DISTINCT FROM OLD.provider
            OR NEW.channel_id IS DISTINCT FROM OLD.channel_id
            OR NEW.user_msg_id IS DISTINCT FROM OLD.user_msg_id
            OR NEW.replay_request_key IS DISTINCT FROM OLD.replay_request_key
            OR NEW.user_text IS DISTINCT FROM OLD.user_text
            OR NEW.replay_only IS DISTINCT FROM OLD.replay_only
            OR NEW.replay_request_hash IS DISTINCT FROM OLD.replay_request_hash
            OR NOT replay_disposition_transition_allowed(
                   OLD.replay_disposition, NEW.replay_disposition,
                   NEW.replay_episode_nonce IS DISTINCT FROM OLD.replay_episode_nonce)) THEN
        refusal := format('row %s cannot weaken its %s request', OLD.id, OLD.replay_disposition);
    ELSIF OLD.replay_disposition = 'withheld' AND (
            (OLD.replay_preserved IS NOT NULL
                 AND NOT COALESCE(NEW.replay_preserved @> OLD.replay_preserved, FALSE))
            OR (OLD.replay_hold_reason IS NOT NULL
                 AND NEW.replay_hold_reason IS DISTINCT FROM OLD.replay_hold_reason)) THEN
        refusal := format('held row %s keeps its preserved result', OLD.id);
    ELSIF replay_disposition_blocks_rerun(OLD.replay_disposition)
          AND ((NEW.status IN ('pending', 'claimed', 'failed_pre_accept', 'failed_post_accept')
                AND NEW.status IS DISTINCT FROM OLD.status)
               OR NEW.claim_owner IS DISTINCT FROM OLD.claim_owner
               OR NEW.claimed_at IS DISTINCT FROM OLD.claimed_at
               OR NEW.target_instance_id IS DISTINCT FROM OLD.target_instance_id) THEN
        refusal := format('started row %s cannot return to %s', OLD.id, NEW.status);
    ELSIF replay_disposition_blocks_rerun(NEW.replay_disposition)
          AND NOT replay_disposition_blocks_rerun(OLD.replay_disposition)
          AND replay_sources_blocked(NEW.provider, NEW.channel_id, NEW.replay_source_message_ids, NEW.id) THEN
        refusal := format('a source of row %s already started elsewhere', NEW.id);
    ELSIF NEW.status IN ('pending', 'claimed', 'accepted', 'spawned')
          AND (NEW.status IS DISTINCT FROM OLD.status
               OR NEW.claim_owner IS DISTINCT FROM OLD.claim_owner
               OR NEW.target_instance_id IS DISTINCT FROM OLD.target_instance_id)
          AND replay_sources_blocked(NEW.provider, NEW.channel_id,
              COALESCE(NEW.replay_source_message_ids, ARRAY[NEW.user_msg_id]), NEW.id) THEN
        refusal := format('row %s shares a started source', NEW.id);
    END IF;
    IF refusal IS NOT NULL THEN
        RAISE EXCEPTION 'replay_disposition_fence: %', refusal
            USING CONSTRAINT = 'replay_disposition_fence';
    END IF;
    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER trg_intake_outbox_replay_fence_insert
    BEFORE INSERT ON intake_outbox
    FOR EACH ROW EXECUTE FUNCTION intake_outbox_replay_fence();
CREATE TRIGGER trg_intake_outbox_replay_fence_update
    BEFORE UPDATE ON intake_outbox
    FOR EACH ROW
    EXECUTE FUNCTION intake_outbox_replay_fence();
CREATE TRIGGER trg_intake_outbox_replay_fence_delete
    BEFORE DELETE ON intake_outbox
    FOR EACH ROW WHEN (OLD.replay_disposition IS NOT NULL)
    EXECUTE FUNCTION intake_outbox_replay_fence();

-- The dispatch projection follows its receipt in the same transaction.
CREATE OR REPLACE FUNCTION intake_outbox_replay_project() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE task_dispatches
       SET replay_disposition = NEW.replay_disposition
     WHERE replay_receipt_id = NEW.id
       AND replay_disposition IS DISTINCT FROM NEW.replay_disposition;
    RETURN NULL;
END
$$;

CREATE TRIGGER trg_intake_outbox_replay_project
    AFTER UPDATE OF replay_disposition ON intake_outbox
    FOR EACH ROW WHEN (OLD.replay_disposition IS DISTINCT FROM NEW.replay_disposition)
    EXECUTE FUNCTION intake_outbox_replay_project();

-- A held dispatch keeps its status, claim and receipt; its projection is always the receipt's.
CREATE OR REPLACE FUNCTION task_dispatches_replay_fence() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    refusal TEXT;
BEGIN
    IF TG_OP = 'DELETE' THEN
        IF replay_disposition_blocks_rerun(OLD.replay_disposition) THEN
            RAISE EXCEPTION 'replay_disposition_fence: held dispatch % cannot be deleted', OLD.id
                USING CONSTRAINT = 'replay_disposition_fence';
        END IF;
        RETURN OLD;
    END IF;
    PERFORM 1 FROM task_dispatches WHERE id = NEW.parent_dispatch_id FOR UPDATE;
    IF TG_OP = 'INSERT' AND (
        replay_dispatch_blocked(NEW.parent_dispatch_id) OR EXISTS (
            SELECT 1 FROM auto_queue_entries e WHERE e.kanban_card_id = NEW.kanban_card_id
             AND replay_entry_blocked(e.id, e.dispatch_id))) THEN
        refusal := format('dispatch %s inherits held request lineage', NEW.id);
    END IF;
    IF NEW.replay_receipt_id IS NOT NULL THEN
        PERFORM 1 FROM intake_outbox r WHERE r.id = NEW.replay_receipt_id FOR SHARE;
        IF NOT FOUND THEN
            refusal := format('dispatch %s names missing receipt %s', NEW.id, NEW.replay_receipt_id);
        END IF;
        NEW.replay_disposition := (SELECT r.replay_disposition FROM intake_outbox r
                                    WHERE r.id = NEW.replay_receipt_id);
    END IF;
    IF TG_OP = 'UPDATE' AND OLD.replay_receipt_id IS NOT NULL
       AND NEW.replay_receipt_id IS DISTINCT FROM OLD.replay_receipt_id THEN
        refusal := format('dispatch %s cannot change its receipt', OLD.id);
    ELSIF TG_OP = 'UPDATE' AND replay_disposition_blocks_rerun(OLD.replay_disposition)
          AND replay_disposition_blocks_rerun(NEW.replay_disposition)
          AND (NEW.status IS DISTINCT FROM OLD.status
               OR NEW.claim_owner IS DISTINCT FROM OLD.claim_owner
               OR NEW.claimed_at IS DISTINCT FROM OLD.claimed_at
               OR NEW.parent_dispatch_id IS DISTINCT FROM OLD.parent_dispatch_id
               OR (OLD.result IS NOT NULL AND NEW.result IS NULL)
               OR NEW.claim_expires_at IS DISTINCT FROM OLD.claim_expires_at) THEN
        refusal := format('held dispatch %s keeps its status and claim', OLD.id);
    END IF;
    IF refusal IS NOT NULL THEN
        RAISE EXCEPTION 'replay_disposition_fence: %', refusal
            USING CONSTRAINT = 'replay_disposition_fence';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER trg_task_dispatches_replay_fence_insert
    BEFORE INSERT ON task_dispatches
    FOR EACH ROW EXECUTE FUNCTION task_dispatches_replay_fence();
CREATE TRIGGER trg_task_dispatches_replay_fence_update
    BEFORE UPDATE ON task_dispatches
    FOR EACH ROW WHEN (OLD.replay_receipt_id IS NOT NULL OR NEW.replay_receipt_id IS NOT NULL)
    EXECUTE FUNCTION task_dispatches_replay_fence();
CREATE TRIGGER trg_task_dispatches_replay_fence_delete
    BEFORE DELETE ON task_dispatches
    FOR EACH ROW WHEN (OLD.replay_receipt_id IS NOT NULL)
    EXECUTE FUNCTION task_dispatches_replay_fence();

-- An entry on a held dispatch keeps its status, link and slot, so it is never requeued.
CREATE OR REPLACE FUNCTION auto_queue_entries_replay_fence() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM 1 FROM task_dispatches d WHERE d.id = OLD.dispatch_id OR EXISTS (
        SELECT 1 FROM auto_queue_entry_dispatch_history h
         WHERE h.entry_id = OLD.id AND h.dispatch_id = d.id) ORDER BY d.id FOR UPDATE;
    IF replay_entry_blocked(OLD.id, OLD.dispatch_id) THEN
        RAISE EXCEPTION 'replay_disposition_fence: entry % keeps held dispatch %',
            OLD.id, OLD.dispatch_id
            USING CONSTRAINT = 'replay_disposition_fence';
    END IF;
    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER trg_auto_queue_entries_replay_fence_update
    BEFORE UPDATE ON auto_queue_entries
    FOR EACH ROW
    WHEN (NEW.status IS DISTINCT FROM OLD.status
          OR NEW.dispatch_id IS DISTINCT FROM OLD.dispatch_id
          OR NEW.slot_index IS DISTINCT FROM OLD.slot_index
          OR NEW.retry_count IS DISTINCT FROM OLD.retry_count
          OR NEW.run_id IS DISTINCT FROM OLD.run_id)
    EXECUTE FUNCTION auto_queue_entries_replay_fence();
CREATE TRIGGER trg_auto_queue_entries_replay_fence_delete
    BEFORE DELETE ON auto_queue_entries
    FOR EACH ROW EXECUTE FUNCTION auto_queue_entries_replay_fence();

-- No new or re-armed provider-launching outbox action for a held dispatch.
CREATE OR REPLACE FUNCTION dispatch_outbox_replay_fence() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM 1 FROM task_dispatches WHERE id = NEW.dispatch_id FOR UPDATE;
    IF replay_dispatch_blocked(NEW.dispatch_id) THEN
        RAISE EXCEPTION 'replay_disposition_fence: % for held dispatch %',
            NEW.action, NEW.dispatch_id
            USING CONSTRAINT = 'replay_disposition_fence';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER trg_dispatch_outbox_replay_fence_insert
    BEFORE INSERT ON dispatch_outbox
    FOR EACH ROW WHEN (NEW.action IN ('notify', 'followup'))
    EXECUTE FUNCTION dispatch_outbox_replay_fence();
CREATE TRIGGER trg_dispatch_outbox_replay_fence_rearm
    BEFORE UPDATE ON dispatch_outbox
    FOR EACH ROW
    WHEN (NEW.action IN ('notify', 'followup') AND NEW.status = 'pending'
          AND (OLD.status IS DISTINCT FROM 'pending' OR NEW.action IS DISTINCT FROM OLD.action
               OR NEW.dispatch_id IS DISTINCT FROM OLD.dispatch_id
               OR NEW.retry_count < OLD.retry_count
               OR NEW.claim_owner IS DISTINCT FROM OLD.claim_owner))
    EXECUTE FUNCTION dispatch_outbox_replay_fence();

-- A missing current link still retains its durable entry-to-dispatch history.
CREATE OR REPLACE FUNCTION auto_queue_history_replay_fence() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF replay_entry_blocked(NEW.entry_id, NULL)
           AND NOT EXISTS (SELECT 1 FROM auto_queue_entry_dispatch_history h
                            WHERE h.entry_id = NEW.entry_id AND h.dispatch_id = NEW.dispatch_id) THEN
            RAISE EXCEPTION 'replay_disposition_fence: entry % keeps its held lineage', NEW.entry_id
                USING CONSTRAINT = 'replay_disposition_fence';
        END IF;
        RETURN NEW;
    END IF;
    IF replay_dispatch_blocked(OLD.dispatch_id) THEN
        RAISE EXCEPTION 'replay_disposition_fence: history % keeps held dispatch %',
            OLD.id, OLD.dispatch_id USING CONSTRAINT = 'replay_disposition_fence';
    END IF;
    IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
    RETURN NEW;
END
$$;
CREATE TRIGGER trg_auto_queue_history_replay_fence
    BEFORE INSERT OR UPDATE OR DELETE ON auto_queue_entry_dispatch_history
    FOR EACH ROW EXECUTE FUNCTION auto_queue_history_replay_fence();

-- Destructive session writers must fail in their transaction before a held request is detached.
CREATE OR REPLACE FUNCTION sessions_replay_fence() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE fresh_handoff BOOLEAN := FALSE;
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
        IF NEW.claude_session_id IS NOT DISTINCT FROM OLD.claude_session_id
           AND NEW.raw_provider_session_id IS NOT DISTINCT FROM OLD.raw_provider_session_id
           AND NEW.session_key IS NOT DISTINCT FROM OLD.session_key
           AND NEW.cwd IS NOT DISTINCT FROM OLD.cwd
           AND NEW.instance_id IS NOT DISTINCT FROM OLD.instance_id
           AND NEW.status NOT IN ('idle', 'disconnected', 'aborted')
           AND (fresh_handoff OR (
               NEW.active_dispatch_id IS NOT DISTINCT FROM OLD.active_dispatch_id
               AND NEW.current_replay_receipt_id IS NOT DISTINCT FROM OLD.current_replay_receipt_id
               AND NEW.replay_episode_nonce IS NOT DISTINCT FROM OLD.replay_episode_nonce
               AND NEW.active_turn_nonce IS NOT DISTINCT FROM OLD.active_turn_nonce
               AND NEW.dispatched_origin_turn_nonce IS NOT DISTINCT FROM OLD.dispatched_origin_turn_nonce)) THEN
            RETURN NEW;
        END IF;
    END IF;
    RAISE EXCEPTION 'replay_disposition_fence: session % keeps its held request', OLD.id
        USING CONSTRAINT = 'replay_disposition_fence';
END
$$;
CREATE TRIGGER trg_sessions_replay_fence
    BEFORE UPDATE OR DELETE ON sessions
    FOR EACH ROW EXECUTE FUNCTION sessions_replay_fence();
