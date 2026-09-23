-- Intake reservations and actual provider turns share one node budget.
CREATE TABLE node_execution_leases (
    instance_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    nonce UUID NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (instance_id, provider, channel_id)
);
CREATE INDEX node_execution_leases_expiry ON node_execution_leases(expires_at);
CREATE TABLE node_execution_assignments (instance_id TEXT PRIMARY KEY, last_execution_assignment_at TIMESTAMPTZ NOT NULL);

CREATE FUNCTION node_execution_occupancy(node_id TEXT)
RETURNS TABLE(provider TEXT, channel_id TEXT) LANGUAGE sql VOLATILE AS $$
    SELECT provider, channel_id FROM intake_outbox
     WHERE target_instance_id=node_id
       AND status IN ('pending','claimed','accepted','spawned','dispatched')
    UNION
    SELECT provider, channel_id FROM node_execution_leases
     WHERE instance_id=node_id AND expires_at>clock_timestamp()
$$;

-- Every writer (live ingress, owner recording, automatic and operator retry)
-- passes this guard. An advisory lock avoids a FOR SHARE/UPDATE node-row cycle.
CREATE FUNCTION reserve_node_execution(node_id TEXT, bot TEXT, channel TEXT)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE slots INTEGER; occupied BIGINT;
BEGIN
    PERFORM pg_advisory_xact_lock(hashtext('agentdesk.execution_capacity.v1'),hashtext(node_id));
    SELECT CASE WHEN capabilities #>> '{execution_capacity,version}'='1'
        THEN (capabilities #>> '{execution_capacity,slots}')::INTEGER END
      INTO slots FROM worker_nodes WHERE instance_id=node_id;
    IF slots IS NULL THEN RETURN; END IF;
    SELECT count(*) INTO occupied FROM node_execution_occupancy(node_id) o
     WHERE (o.provider,o.channel_id)<>(bot,channel);
    IF slots<1 OR occupied>=slots THEN
        RAISE EXCEPTION 'node execution capacity exhausted: %', node_id
          USING ERRCODE='23514', CONSTRAINT='node_execution_capacity_available';
    END IF;
END
$$;

CREATE FUNCTION guard_intake_execution_capacity() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.status IN ('pending','claimed','accepted','spawned','dispatched') THEN
        PERFORM reserve_node_execution(NEW.target_instance_id,NEW.provider,NEW.channel_id);
        INSERT INTO node_execution_assignments VALUES(NEW.target_instance_id,clock_timestamp())
          ON CONFLICT(instance_id) DO UPDATE SET last_execution_assignment_at=EXCLUDED.last_execution_assignment_at;
    END IF;
    RETURN NEW;
END
$$;
CREATE TRIGGER intake_execution_capacity BEFORE INSERT OR UPDATE OF target_instance_id
    ON intake_outbox FOR EACH ROW EXECUTE FUNCTION guard_intake_execution_capacity();

CREATE FUNCTION guard_provider_execution_capacity() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM reserve_node_execution(NEW.instance_id,NEW.provider,NEW.channel_id);
    RETURN NEW;
END
$$;
CREATE TRIGGER provider_execution_capacity BEFORE INSERT ON node_execution_leases
    FOR EACH ROW EXECUTE FUNCTION guard_provider_execution_capacity();
