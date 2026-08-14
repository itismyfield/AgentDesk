# Runbook — auditing dispatched intake-outbox rows

Use this procedure to inventory `intake_outbox` rows whose status is
`dispatched`.

## Run the audit

This command requires migration `0107_intake_outbox_dispatched_clock.sql` to
be applied to the target database. Confirm it with:

```sql
SELECT version, success FROM public._sqlx_migrations WHERE version = 107;
```

On a normally SQLx-managed database, the prerequisite is satisfied only when
this returns exactly one row with `success = true`. If migrations 0052–0106
were normally applied, migration 0107 was not applied, and `config::load`,
`db::postgres::connect`, and its `SELECT 1` health check all succeed, the CLI
exits with this complete stderr:

```text
Error: list dispatched intake_outbox rows: error returned from database: column "dispatched_at" does not exist
```

Manual DDL or migration-ledger drift is outside that inference; verify both
the ledger and actual schema before acting.

On a host configured for the target PostgreSQL database, run:

```sh
agentdesk intake-outbox dispatched-audit
```

An empty population prints `(no dispatched intake_outbox rows)` and exits
successfully. Otherwise the command prints one TAB/LF/CR-safe physical TSV
record per database row, ordered by `dispatched_at` with NULL clocks first and
then by `id`.

Record the complete output in the incident. The identity fields are `id`,
`channel_id`, `user_msg_id`, `attempt_no`, and `parent_outbox_id`; the remaining
facts are `dispatched_at`, `claim_owner`, `provider`, and `provider_nonempty`.
In text fields, `\`, tab, newline, and carriage return are rendered as `\\`,
`\t`, `\n`, and `\r`. The unescaped marker `\N` represents a database NULL;
a literal `-` remains `-`, and an empty non-NULL `provider` remains an empty
field with `provider_nonempty` set to `false`.

That physical-record guarantee covers TAB/LF/CR delimiters only. ESC,
backspace, U+2028, U+2029, and other non-delimiter control or rendering
characters pass through unchanged, so terminal-rendering and ANSI-injection
risk remains.

`provider_nonempty` means exactly that `provider.trim()` is non-empty, matching
the provider guard used by the operator force-fail implementation. It does not
establish worker availability, capability, feature support, labels, placement
ownership, generation, or freedom from route and attempt conflicts. A
dispatched row is still refused by that force-fail command.

The partial UNIQUE index `intake_outbox_one_open_route_per_channel` makes
`channel_id` unique across dispatched rows. Seeing the same `channel_id` twice
means the open-route fence is not enforcing its schema contract, including a
missing or INVALID index, and requires immediate migration-integrity response.
The audit selects only dispatched rows, so the absence of duplicates in its
output does not prove fence health: a dispatched and pending row for the same
channel would violate the same index while only the dispatched row appears here.

## Scope and limits

The command uses no explicit multi-statement transaction, row lock, or advisory
lock. The `list_dispatched_audit` query's `SELECT ... FROM public.intake_outbox`
takes an ordinary ACCESS SHARE relation lock in its implicit transaction. That
lock is compatible with DML but can delay ACCESS EXCLUSIVE DDL for the duration
of the query. Do not run the audit during a migration or deployment: it can
delay DDL, and queued ACCESS EXCLUSIVE DDL can in turn make later readers wait.

The single statement reads a consistent MVCC snapshot, so it cannot see half
of an in-progress transaction. Rows committed after statement start are absent,
and rows in the result can change before an operator acts; the output is
point-in-time evidence that can become stale immediately. The query has no
LIMIT or pagination and fetches the full dispatched population into memory.
Track the underlying unbounded-growth risk in [#5320](https://github.com/itismyfield/AgentDesk/issues/5320),
which covers production retention, cleanup ownership, and archival policy for
`intake_outbox`.

The command does not determine whether Discord delivery occurred, repair a
NULL clock, resolve an open route, or authorize a retry. Correlate the printed
row identity with the incident's independently collected delivery and receipt
evidence before choosing any later response. Run this audit on demand; this
procedure does not define a schedule or batch worker.
