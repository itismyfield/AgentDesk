# PostgreSQL Migration Checksum Repair

Numbered files under `migrations/postgres/` are immutable after merge. `sqlx`
stores the applied checksum in `_sqlx_migrations`; editing a previously applied
file changes the resolved checksum embedded in a newly built binary and can make
release deploy fail before later migrations run.

## Guard

Run the checksum guard before release work:

```bash
python3 scripts/check_postgres_migration_checksums.py
```

New migration files must be appended to
`migrations/postgres/immutable-checksums.json`.

For an intentional edit to an existing numbered migration, do not update that
existing entry in the immutable manifest. Add a matching entry to
`migrations/postgres/checksum-repair-allowlist.json` with:

- `path`
- `old_sha256`
- `new_sha256`
- `issue`
- `reason`
- `covered_by_migration`
- `repair_doc`

That allowlist entry is the review surface for checksum drift.

## Approved Repair Procedure

Use this only when all of these are true:

- A fresh-install schema migration was accidentally changed after live DBs had
  already applied that migration.
- The live DB schema difference is already covered by a later migration that was
  pending or already applied.
- `agentdesk doctor --json` reports only checksum drift for the affected
  version; it does not report missing or unsuccessful migrations that need a
  separate fix.

Never repair a checksum to paper over a schema difference that is not covered by
a later migration. Add a new migration instead.

1. Capture the doctor evidence:

```bash
agentdesk doctor --json | jq '.checks[] | select(.id == "postgres_connection")'
```

The checksum mismatch evidence includes:

- `version`
- `applied_checksum`
- `resolved_checksum`

2. Confirm the later migration covers the live DB. For the #2919 incident,
   `0001_initial_schema.sql` gained `message_outbox.dedupe_key` and
   `message_outbox.dedupe_expires_at`, and live DBs were covered by
   `0066_message_outbox_dedupe_key.sql`.

3. Back up the database or take an operator-approved snapshot.

4. Repair only the checksum row, guarded by the old applied checksum:

```sql
BEGIN;

SELECT
  version,
  description,
  encode(checksum, 'hex') AS applied_checksum
FROM _sqlx_migrations
WHERE version = 1;

UPDATE _sqlx_migrations
SET checksum = decode('<resolved_checksum_from_doctor>', 'hex')
WHERE version = 1
  AND checksum = decode('<applied_checksum_from_doctor>', 'hex');

COMMIT;
```

5. Re-run doctor and then deploy:

```bash
agentdesk doctor --json | jq '.checks[] | select(.id == "postgres_connection")'
./scripts/deploy-release.sh
```

If the guarded update affects zero rows, stop and re-read the doctor evidence;
the live DB changed between diagnosis and repair.
