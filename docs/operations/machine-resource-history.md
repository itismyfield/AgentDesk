# Machine resource history

Each node records one resource sample per distinct successful heartbeat in
PostgreSQL `machine_resource_samples`. Each row is keyed by the stable
`instance_id` and the machine's `observed_at_ms`. The `resources` JSONB value
contains CPU, memory, disk, and GPU readings together with hardware
labels and the sample expiry time. The heartbeat and execution admission paths
do not wait for this optional history write: a storage failure leaves node
liveness intact and is logged for diagnosis.

Authorized clients can read the most recent 15 minutes through the
authenticated `GET /api/cluster/machine-resources/history?instance_id=<id>`
endpoint. Optional `from_ms`, `to_ms`, and `limit` parameters accept epoch
milliseconds and at most 240 samples in a window no longer than 90 days.
Results are oldest first. Missing readings remain `null`; they must not be
interpreted as zero utilization. Consumers can compare `observed_at_ms` with
`expires_at_ms` to distinguish current telemetry from historical evidence.

Agents with authorized database access can query longer periods by machine and
time, for example:

```sql
SELECT observed_at_ms, resources
FROM machine_resource_samples
WHERE instance_id = $1 AND observed_at_ms BETWEEN $2 AND $3
ORDER BY observed_at_ms;
```

GPU utilization and memory use are nullable when the platform cannot measure
them. Values are observations, not capacity or scheduling guarantees.

The Hub deletes samples older than 90 days in batches of 1,000 during its
hourly background maintenance, continuing for at most 15 seconds per sweep.
Cleanup also runs at startup and yields under database pool pressure. This retention is independent of the node
registry: historical rows remain available if a node later goes offline or
its registry row is removed. Back up the PostgreSQL database before applying
new migrations and verify `_sqlx_migrations` before activating a new binary.
