# Policy Tick Bottleneck Profiling for #735

## What Changed

- `src/server/mod.rs`
  - tick hook execution now leaves the async runtime via `tokio::task::spawn_blocking`
  - each hook run is bounded by a 5 second timeout
  - overlapping tick hooks are skipped while a timed-out background hook is still finishing, so repeated timeouts do not pile up on Tokio's blocking pool
- `src/engine/mod.rs`
  - each policy hook invocation now logs per-policy elapsed time
- `src/engine/ops/db_ops.rs`
  - slow policy SQL query/execute calls now log elapsed time and compact SQL

## Current Architecture Finding

The current branch already runs `policy_tick_loop` on a dedicated OS thread:

- `src/server/worker_registry.rs:376` starts a dedicated thread for `policy_tick_loop`
- that thread builds its own current-thread Tokio runtime and calls `rt.block_on(super::policy_tick_loop(...))`

That means the original incident description about the shared server Tokio executor being blocked is stale for this branch. The immediate risk that still remained was different:

- a slow tick hook could still pin the tick runtime thread
- there was no hook timeout
- repeated timeouts could enqueue more blocking work than necessary

The short-term fix in `src/server/mod.rs` addresses those remaining failure modes.

## Bottleneck Hypothesis

Local tests did not reproduce an 8 to 10 second hook, so the profiling result here is based on static inspection plus the new runtime logs.

Most likely hotspots:

- `policies/auto-queue.js`
  - `onTick1min` performs broad recovery and queue scans; this is the strongest 1 minute candidate
- `policies/timeouts.js`
  - `onTick5min` runs multiple sweeps and reconciliation passes; this is the strongest 5 minute candidate
- `src/engine/ops/db_ops.rs`
  - bridge calls already use separate SQLite connections, so the more plausible DB issue is long SQLite work or busy waiting, not the Rust `Db` mutex itself
- QuickJS / JS heap pressure
  - still possible, but only if slow hook logs appear without matching slow DB logs

## How To Read The New Logs

1. Look for slow per-policy hook logs from `src/engine/mod.rs`.
2. Correlate them with slow DB logs from `src/engine/ops/db_ops.rs`.

Interpretation:

- slow hook log + slow DB log
  - bottleneck is policy logic that is DB-heavy, or SQLite contention/scan cost
- slow hook log without slow DB log
  - bottleneck is more likely JS logic, repeated object churn, or QuickJS GC
- repeated `timeout` or `skipped_inflight` tick status in `kv_meta`
  - hook runtime still exceeds the enforced bound and needs policy-level optimization

## Follow-Up

- After deploy, inspect `grep "policy-tick.*took\\|policy hook slow\\|policy db .* slow" ~/.adk/release/logs/dcserver.stdout.log`
- If `auto-queue.js` or `timeouts.js` dominates the slow-hook logs, split a follow-up optimization ticket against that specific policy rather than the engine wrapper
