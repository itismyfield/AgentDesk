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
