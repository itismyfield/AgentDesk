# PostgreSQL CI Service Image

GitHub Actions Postgres lanes start their database through
`scripts/ci/postgres-service.sh` instead of workflow-level `services:`.
Workflow services pull the container image before any repository step can run,
so registry slowness or rate limits fail the job without retry context.

The script owns the shared policy:

- `POSTGRES_SERVICE_IMAGE` selects the image. Workflows map this from the
  optional repository/org variable `AGENTDESK_POSTGRES_SERVICE_IMAGE`.
- If the variable is unset, the script defaults to `postgres:17`.
- `POSTGRES_SERVICE_PULL_ATTEMPTS` controls pull retry count and defaults to 3.
- Startup waits for `pg_isready` and prints container logs before failing.

Use `AGENTDESK_POSTGRES_SERVICE_IMAGE` when the project needs to pin a mirror or
registry cache without editing every workflow. Image pull failures are reported
as CI service infrastructure errors; test failures after the database is ready
remain ordinary AgentDesk test failures.

## PostgreSQL 15 floor proof

PostgreSQL 15 is the supported floor. The canonical lanes run the default image
above, so the PR workflow adds one exact proof lane on the floor version:

- Job `pg15_owner_proof` (`PostgreSQL 15 owner proof`) overrides
  `POSTGRES_SERVICE_IMAGE` to `postgres:15` at job level and fails unless
  `SHOW server_version_num` reports a `15xxxx` server.
- It runs only the ids in `scripts/ci/pg15_required_ids.txt` with `--exact`
  through `scripts/run_test_lane.py --expected-ids`, so a missing, extra,
  ignored or unexecuted id, or an empty list, is red. The first id applies every
  migration on PostgreSQL 15.
- Its `needs` and `if` match `test_fast` (`pg_db` or the full-run kill switch),
  and the required `Fast targeted tests (ubuntu-latest)` context mirrors its
  result with the same `pg_db` filter. Branch protection is unchanged.
- The listed ids also stay in the canonical PostgreSQL shards and never appear
  in `scripts/known_test_failures.txt`.
