-- Discord-only mention metadata for scheduled push delivery.
--
-- The canonical content remains provider-neutral. Discord handoff renders
-- this ordered list as <@user_id> mentions while Kakao keeps content unchanged.
LOCK TABLE worker_nodes, scheduled_messages IN SHARE ROW EXCLUSIVE MODE;

CREATE OR REPLACE FUNCTION agentdesk_assert_scheduled_discord_mention_consumer_v1_ready()
RETURNS VOID AS $$
DECLARE
    legacy_nodes TEXT;
BEGIN
    SELECT string_agg(instance_id, ', ' ORDER BY instance_id)
      INTO legacy_nodes
      FROM worker_nodes
     WHERE status = 'online'
       AND COALESCE(
           capabilities #>> '{scheduled_messages,discord_mention_consumer_v1}',
           'false'
       ) <> 'true';

    IF legacy_nodes IS NOT NULL THEN
        RAISE EXCEPTION
            'scheduled Discord mention consumer requires a stopped and upgraded fleet'
            USING ERRCODE = '55000',
                  DETAIL = format(
                      'online nodes missing scheduled_messages.discord_mention_consumer_v1: %s',
                      legacy_nodes
                  ),
                  HINT = 'Stop and drain the fleet, mark verified stopped worker_nodes offline, then apply migration 0112 before restarting only mention-capable binaries.';
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT agentdesk_assert_scheduled_discord_mention_consumer_v1_ready();

ALTER TABLE scheduled_messages
    ADD COLUMN discord_mention_user_ids TEXT[] NOT NULL DEFAULT '{}';

ALTER TABLE scheduled_messages
    ADD CONSTRAINT chk_smsg_discord_mention_user_ids_shape CHECK (
        cardinality(discord_mention_user_ids) <= 20
        AND (
            cardinality(discord_mention_user_ids) = 0
            OR array_to_string(discord_mention_user_ids, ',', '')
                ~ '^([1-9][0-9]*)(,[1-9][0-9]*)*$'
        )
    );

COMMENT ON COLUMN scheduled_messages.discord_mention_user_ids IS
    'Ordered Discord-only user snowflakes rendered at push handoff; external provider content is unchanged.';

CREATE OR REPLACE FUNCTION agentdesk_require_scheduled_discord_mention_consumer_v1()
RETURNS TRIGGER AS $$
BEGIN
    IF NEW.status = 'firing'
       AND OLD.status IS DISTINCT FROM 'firing'
       AND cardinality(NEW.discord_mention_user_ids) > 0
       AND COALESCE(
           current_setting('agentdesk.scheduled_discord_mention_consumer_v1', true),
           ''
       ) <> 'enabled'
    THEN
        RAISE EXCEPTION
            'scheduled Discord mention claim requires consumer capability v1'
            USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_scheduled_discord_mention_consumer_v1
    BEFORE UPDATE ON scheduled_messages
    FOR EACH ROW
    EXECUTE FUNCTION agentdesk_require_scheduled_discord_mention_consumer_v1();
