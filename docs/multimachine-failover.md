# Multimachine Failover (Mac Mini + Mac Book)

Operator guide for running AgentDesk across two physical hosts (e.g. the Mac
Mini as the active machine and the Mac Book as the standby) backed by a single
shared PostgreSQL `agentdesk` database. Singleton coordination relies entirely
on PostgreSQL session-scoped advisory locks; there is no separate consensus
layer to manage.

> Status: code-side guarantees and unit/integration tests landed under #835.
> Real two-machine smoke-test verification is still pending; see
> [Known limitations](#known-limitations) before treating this as a hot
> standby.

## Prerequisites

1. **Single shared `agentdesk` Postgres database**, reachable from both hosts.
   Both `dcserver` instances must point at the same `host:port/dbname` in
   `~/.adk/release/config/agentdesk.yaml` under the `database:` block (see the
   "Operational database routing" row of [`docs/source-of-truth.md`](source-of-truth.md)).
2. **`agentdesk.yaml` parity** between the two hosts. The discord bindings,
   agent roster, and MCP declarations must match — bot tokens are hashed into
   the singleton lease key, so any drift will cause both hosts to think they are
   running different bots and both will run gateways for whichever side is
   misconfigured.
3. **`dcserver` installed on each machine** through the canonical deploy flow
   (`scripts/deploy-release.sh`). Each host runs its own LaunchAgent; the
   release plist is generated with `agentdesk emit-launchd-plist`.
4. **System clocks loosely in sync** (NTP). Advisory locks are immune to skew
   (see [Clock-skew note](#clock-skew-note)), but operators reading log
   timestamps across machines need them to align within a few seconds.

## Bring-up sequence

1. Promote the same release binary to both hosts (`scripts/deploy-release.sh`).
2. Start the active host first:

   ```bash
   launchctl bootout gui/$(id -u)/com.agentdesk.release 2>/dev/null
   launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.agentdesk.release.plist
   ```

3. Confirm the active host has acquired the gateway lease for every configured
   bot token (see [Health checks](#health-checks)).
4. Start the standby host the same way. Its `dcserver` will start, attempt to
   acquire each lease, observe `pg_try_advisory_lock` returning `false`, log
   the standby state, and continue running idle (it does not crash, it simply
   does not run a gateway or singleton job).

## Singleton leases

| Lock                      | Lock id                                | Holder behavior                                                         |
| ------------------------- | -------------------------------------- | ----------------------------------------------------------------------- |
| `GATEWAY-LEASE` per bot   | `discord_gateway_lock_id(token_hash)`  | Holder runs the Discord gateway for that bot; standby fences off.       |
| Policy-tick singleton     | `7_801_001`                            | Holder runs the 30s/1min/5min tick loops; standby skips with a debug log. |
| GitHub-sync singleton     | `7_801_002`                            | Holder runs `github-sync`; standby skips with a debug log.               |
| Outbox drain (SKIP LOCKED) | None (per-row `FOR UPDATE SKIP LOCKED`) | Both hosts drain in parallel; row-level locking guarantees no double send. |

### `GATEWAY-LEASE` distribution

`src/services/discord/runtime_bootstrap.rs::discord_gateway_lock_id()` builds
one lock id per bot token by stripping the literal `discord_` prefix from the
SHA-256 token hash and folding the remaining 16 hex characters into the lower
48 bits of a fixed prefix. This means:

- **Each bot gets its own advisory lock**, so the active host can run the
  Claude bot while the standby runs the Codex bot if you want to split load.
  The default deployment puts all bots on the active host, which is fine; the
  point is that there is no global `dcserver` lock — the lease is per token.
- **`#87f80a60` prefix-strip fix** is required for this to work. Without it
  every bot collapses onto the same fallback lock id and only the first
  `dcserver` to start can acquire any lease. The fix is covered by unit tests
  `discord_gateway_lock_id_strips_discord_prefix_so_each_bot_gets_unique_id`
  and `discord_gateway_lock_id_changes_for_different_token_hashes` in the same
  file.

The lease is acquired through `crate::db::postgres::AdvisoryLockLease`, which
opens a dedicated `PgConnection` outside the shared pool and runs
`pg_try_advisory_lock`. The lock is released automatically when the
connection is dropped (process exit, kill, or network drop), which is the
mechanism that makes failover automatic.

### Policy-tick / GitHub-sync verification

Both singletons are acquired through `try_acquire_pg_singleton_lock` in
`src/server/mod.rs`. Look for these markers in the standby's log:

- `[policy-tick] skipped: advisory lock held elsewhere` — emitted at
  `tracing::debug` level every 30 seconds while the active host owns
  `7_801_001`.
- `[github-sync] skipped: advisory lock held elsewhere` — emitted whenever
  github-sync would have run (default cadence) while the active host owns
  `7_801_002`.

On the active host, look for the periodic job execution markers (e.g.
`record_periodic_job_execution_pg(... "github_sync" ...)`) and the policy-tick
hook timing logs.

### SKIP LOCKED outbox drain — dual-drain is safe

The `message_outbox` claim query in
`claim_pending_message_outbox_batch_pg` (`src/server/mod.rs`) uses

```sql
SELECT id ... FROM message_outbox
 WHERE status = 'pending' OR (status = 'processing' AND claimed_at <= now() - $1)
 ORDER BY id ASC
 FOR UPDATE SKIP LOCKED
 LIMIT 10
```

inside a `WITH claimed AS (...) UPDATE ... FROM claimed` CTE. Both machines
drain concurrently; PostgreSQL guarantees that any row currently being claimed
by one host's `FOR UPDATE` is invisible to the other host's `SKIP LOCKED`
scan. The `claim_owner` column records which host took the row, which makes
audit and stale-claim recovery (`MESSAGE_OUTBOX_CLAIM_STALE_SECS`) possible
without pinning drain to one host. **No advisory lock is needed for outbox
drain** — running it on both hosts increases throughput and provides a hot
spare without coordinating with the singleton lease.

## Failover playbook

### Active dcserver killed (graceful or kill -9)

1. Active host's `dcserver` exits (`launchctl bootout`, `kill -9`, or crash).
2. PostgreSQL closes its session, which releases every advisory lock the
   active host was holding (gateway leases plus singletons).
3. Standby's next probe acquires the lock. The probe cadence is:
   - `policy-tick` retries every 30 s, so the singleton handover is bounded
     by **≤ 30 s**.
   - `github-sync` retries on its scheduled cadence (typically tens of
     seconds), so handover is bounded by that cadence.
   - `GATEWAY-LEASE` retries via the runtime keepalive loop
     (`DISCORD_GATEWAY_LEASE_KEEPALIVE_INTERVAL = 15 s`), so a Discord
     gateway is back online within **≤ 60 s** end-to-end (worst case includes
     the original session's TCP keepalive timeout to PostgreSQL).
4. Verify with the [Health checks](#health-checks) below.

### Network drop on the active host

1. Active host loses its PostgreSQL connection. PostgreSQL detects the dead
   session via TCP keepalive (`tcp_keepalives_idle`,
   `tcp_keepalives_interval`, `tcp_keepalives_count`). On a default Linux
   setup this is several minutes; on managed Postgres providers it is
   typically faster. Until detection, the standby will continue to be fenced
   off — this is not a split-brain bug, it is the expected behavior of
   session-scoped advisory locks.
2. When PostgreSQL reaps the dead session, the locks release and the standby
   acquires within one probe cycle.
3. When the active host reconnects, it discovers the leases are now held by
   the standby and continues running in standby mode (its own probes return
   `false`). It will reclaim the leases the next time the standby releases
   them — i.e. on the standby's next graceful shutdown or its own network
   drop.
4. **Recovery action**: tighten PG keepalive settings if the default detection
   window is too long. There is no client-side fence to enable; advisory lock
   semantics already cover this.

## Standby self-fence

Even though the standby is mostly idle, it must not begin processing turns it
already started if the lease is yanked away. The self-fence rule is:

- **Lease lost mid-turn** → drain inflight Discord turns to completion (don't
  abort mid-write), but stop pulling new work. The next iteration of the
  gateway loop's keepalive will observe the lease loss and the runtime will
  shut down its provider tasks. The runtime should not auto-rejoin the
  gateway until it can reacquire the lease.
- **Lease never held** → no work to drain; the runtime simply continues
  retrying the lease and emitting standby logs.

This behavior is implemented at the `AdvisoryLockLease::keepalive` site in
`src/db/postgres.rs` plus the runtime bootstrap shutdown path. See
`postgres_discord_gateway_lease_fails_over_across_separate_runtime_pools` for
the unit-level coverage.

## Rollback to single-machine

1. Stop the standby host first:

   ```bash
   launchctl bootout gui/$(id -u)/com.agentdesk.release
   ```

2. The standby releases all leases on disconnect; the active host continues
   to hold any leases it already had. No additional cleanup is required.
3. Optionally disable the standby's LaunchAgent so it does not auto-restart:
   `launchctl disable gui/$(id -u)/com.agentdesk.release`.

If the active host died first and the standby took over, treat the standby as
the new active and stop the original active before bringing it back up — the
restart will rejoin as a standby because the lease is now elsewhere.

## Clock-skew note

Advisory locks themselves are **immune to clock skew**: PostgreSQL evaluates
`pg_try_advisory_lock` purely on session liveness, not on wall-clock
timestamps. Failover correctness does not depend on the two hosts agreeing on
the time.

However, several log/audit fields *do* compare timestamps:

- `claimed_at <= NOW() - INTERVAL` in the outbox stale-claim recovery query
  (uses Postgres `NOW()` only — single source of truth, safe).
- `chrono::Local::now()` in tracing markers — these are **per-host wall
  clocks**. If the two hosts disagree by more than a few seconds, log lines
  will appear out of order when grepping across both hosts' logs. Inspect
  with `chrony` / `timedatectl status` if log ordering looks wrong.
- LaunchAgent `mtime`-based startup checks (`recover_orphan_pending_dispatches`
  in `runtime_bootstrap.rs`) use the local PID file's mtime, which is
  per-host and intentionally local.

In short: the lease subsystem is skew-immune. The diagnostic surface is not.
Keep NTP enabled on both hosts.

## Health checks

Run these from any host with `psql` access to the shared `agentdesk` DB.

### Who currently holds the singleton leases?

```sql
SELECT classid, objid, granted, pid, application_name, client_addr
  FROM pg_locks
  JOIN pg_stat_activity USING (pid)
 WHERE locktype = 'advisory'
 ORDER BY objid;
```

Look for rows with `objid` ∈ {`7801001` (policy-tick), `7801002`
(github-sync)} and the per-bot gateway ids (the upper 16 bits of those are
`0x0443`, i.e. lock ids ≥ `0x0443_0000_0000_0000`). The `client_addr` column
identifies the current holder host.

Caveat: PostgreSQL splits 64-bit advisory keys into `(classid, objid)` (two
32-bit halves) when reported via `pg_locks`. For lock ids that fit in 32 bits
(both singleton ids do), the value appears in `objid` with `classid = 0`.

### Are both hosts connected at all?

```sql
SELECT application_name, client_addr, state, backend_start, COUNT(*)
  FROM pg_stat_activity
 WHERE datname = 'agentdesk'
 GROUP BY 1, 2, 3, 4
 ORDER BY backend_start DESC;
```

You should see both `client_addr` values; the active host will have at least
one connection per held advisory lock plus the shared pool.

### Recent periodic job runs

```sql
SELECT job_name, status, started_at, duration_ms
  FROM periodic_job_executions
 ORDER BY started_at DESC
 LIMIT 20;
```

If `policy_tick` and `github_sync` rows appear regularly, the singletons are
running on *some* host; cross-reference `client_addr` to find which.

## Known limitations

- **Real two-machine verification is still pending.** The code-level
  guarantees are covered by unit tests (`discord_gateway_lock_id_*` family)
  and the integration tests
  `postgres_discord_gateway_lease_fails_over_across_separate_runtime_pools`
  (per-bot lease) and
  `policy_tick_advisory_lock_blocks_split_brain_across_pools` /
  `github_sync_advisory_lock_blocks_split_brain_across_pools` (singleton
  jobs) in `src/server/mod.rs`. Things still to validate by hand:
  - Mac Mini ↔ Mac Book gateway handover end-to-end timing.
  - Network-drop recovery time under realistic Postgres TCP keepalive
    settings on the production DB.
  - Outbox dual-drain throughput and audit (no double-send under load).
- **Network-drop recovery is bounded by PostgreSQL TCP keepalive**, not by
  AgentDesk. Tune PG TCP keepalives if the default detection window is
  unacceptable for your operational SLO.
- **No automatic lease preference.** Whichever host probes first after a
  release wins. There is no "prefer Mac Mini" knob today; if you want one,
  delay the standby's startup with `WatchPaths`/`StartCalendarInterval` so
  the active host always starts first.
- **Single shared Postgres is a SPOF.** This guide only covers `dcserver`
  redundancy; database HA is out of scope.
- **Standby idle resource use is non-zero.** Even when fenced off, the
  standby maintains a Postgres pool and runs probe loops. Monitor connection
  count if your Postgres has tight `max_connections`.
