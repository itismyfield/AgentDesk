# Runbook — reclaiming orphaned test databases from a shared PostgreSQL server

**Status: procedure only. Nothing here has been executed. Do not run any step
without a named human owner following it start to finish.**

## Why this exists

Test fixtures create a database per test and drop it at the end. When a test
panics, is cancelled, or is killed mid-run, the drop never happens and the
database stays. Read-only census of the operational server on 2026-08-07:

| measurement | value |
| --- | --- |
| non-template databases | 5035 |
| `agentdesk_*` databases with a hex suffix | 4299 |
| databases whose name is exactly 63 bytes | 4710 |
| databases with no `agentdesk_` prefix at all | 51 |
| databases from the four fixtures #5218 fixes | 2 |

Two facts in that table decide the whole design of this procedure.

**PostgreSQL truncates `datname` at 63 bytes.** 4710 of the names are exactly
63 bytes long, which means the UUID a fixture appended has been cut off
mid-string. A cleanup filter that expects a full 32-character hex suffix matches
almost none of the real orphans, and a filter loosened enough to match them is
loose enough to match something else.

**Orphans do not share a prefix.** 51 databases carry no `agentdesk_` prefix:
`adk_cleanup_substrate_*`, `circuit_producer_default_off_*`,
`circuit_resume_statuses_*`, `kanban_gate_*`, and `postgres` itself. Nothing in
those names distinguishes a leftover fixture from an application database that
someone created deliberately. Name shape alone is not evidence.

The population is also not static. Only 2 of the 5035 came from the fixtures
#5218 repairs; the rest come from fixtures that still build their own base URL
and still fall back to a loopback address (see the follow-up in that issue).
**Sweeping before those fallbacks are removed reclaims space that will refill.**

## Safeguards — all four are mandatory

### (a) Dry-run is the default and the only default

The procedure produces a candidate list and stops. There is no flag ordering
that goes straight to a drop. A drop run takes the *file* a dry run wrote as its
input, not a pattern, so the set that gets dropped is exactly the set a human
read. If the file is older than the confirmation step, regenerate it — a fixture
running in between would otherwise put a live database into the list.

### (b) Protected names are excluded by an explicit allowlist, never by pattern

Keep the list of databases that must never be dropped as literal names, and fail
closed: if a candidate is not provably an orphan, it stays. At minimum this
covers `postgres`, `template0`, `template1`, the database named in
`~/.adk/release/config/agentdesk.yaml` under `database:` (the operational
control-plane database, per `docs/source-of-truth.md`), and any database that a
human has not seen listed. Resolve the operational name from the config file at
run time; do not hardcode it here, because this document being stale is exactly
how the wrong database gets dropped.

Verify the exclusion actually bites before trusting it: the intersection of the
candidate list and the protected list must be empty, and that check must run
against the generated list, not against the pattern that produced it.

### (c) Age and ownership must both be checked, per database

Name shape is not evidence; provenance is. For each candidate require all of:

- **Owner** matches the account the test fixtures run as
  (`pg_get_userbyid(datdba)`). A database owned by anyone else is out.
- **Age** — the database is older than a threshold well beyond the longest test
  run (24 hours is a defensible floor). PostgreSQL does not record a creation
  time for a database, so read it from the filesystem: the mtime of the
  directory under `PGDATA/base/<oid>`, resolved with `pg_relation_filepath`-style
  lookup or `SELECT oid FROM pg_database`. If the age cannot be established for
  a candidate, the candidate is dropped from the list, not from the server.
- **No live connections** (`pg_stat_activity` shows no backend for it). A
  database with an active backend is a running test, not an orphan.

Record all three values in the dry-run output next to each name. A reviewer who
cannot see why a database is a candidate cannot approve it.

### (d) A human confirms the list, and the drop is reversible in the ways it can be

Print the count and the full list, require the reviewer to type the count back,
and dump globals plus the protected databases (`pg_dumpall --globals-only`, plus
a real dump of anything on the protected list) before the first drop. Drop one
database at a time with `DROP DATABASE` — never `WITH (FORCE)`, which would
terminate backends and turn a mis-scoped list into an outage. Log every name
dropped, with a timestamp, to a file outside the server. Stop the entire run on
the first unexpected error rather than continuing down the list.

## Ordering

1. Land the fallback removal everywhere, not just the four modules in #5218.
   While a fixture can still reach a server the lane did not configure, the
   population regrows and a cleanup is a treadmill.
2. Take the census read-only and keep it. It is the before-picture.
3. Dry run. Review. Confirm. Drop, oldest first, in small batches.
4. Re-census. The delta must equal the number of names in the log, exactly. Any
   discrepancy means something else was writing to the server during the run,
   and the remaining batches do not proceed.

## What this runbook does not authorise

- Dropping anything on a server that hosts operational data, while that server
  is serving traffic.
- `DROP DATABASE ... WITH (FORCE)`, `pg_terminate_backend`, or any statement that
  disconnects a session.
- Any sweep driven by a `LIKE` pattern without the per-database owner, age, and
  connection checks in (c).
- Running the sweep as a scheduled job. Every run is human-initiated and
  human-confirmed until the population stops regrowing.
