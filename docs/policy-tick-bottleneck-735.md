# Policy Tick Bottleneck Profiling for #735

## Summary

- `policy_tick_loop` is already isolated from the main server runtime on this branch. `src/server/worker_registry.rs:365-377` starts a dedicated OS thread and builds a `current_thread` Tokio runtime only for policy ticks.
- The `#735` code change still matters because `src/server/mod.rs:480-547` now runs each tick hook through `spawn_blocking`, adds a 5s timeout, and skips overlap while a timed-out hook is still finishing.
- Local baseline profiling on `2026-04-17` with the real repository policy files and an empty test DB measured:
  - `OnTick1min`: `5ms`
  - `OnTick5min`: `7ms`
- That baseline is too small to support "plain QuickJS hook dispatch is inherently 8-10s". The remaining credible bottlenecks are data-dependent policy work: broad SQLite sweeps, write contention, or synchronous HTTP fan-out inside the policy body.

## Measured Baseline

Command:

```bash
cargo test profile_real_policy_tick_hooks_empty_db_baseline -- --ignored --nocapture
```

Observed output:

```text
profile_real_policy_tick_hooks_empty_db_baseline onTick1min outcome=Ok elapsed_ms=5
profile_real_policy_tick_hooks_empty_db_baseline onTick5min outcome=Ok elapsed_ms=7
```

What this proves:

- The actual `policies/auto-queue.js` and `policies/timeouts.js` files load and execute quickly when the database is small and there is no live operational backlog.
- Baseline QuickJS invocation overhead, hook lookup, and light GC pressure are in the single-digit millisecond range in this environment.
- An 8-10s production spike therefore needs a data-dependent explanation. The engine wrapper alone is not enough.

## Code-Level Evidence

### 1. Runtime starvation diagnosis was stale

- `src/server/worker_registry.rs:365-377` already isolates the tick loop on its own OS thread.
- `src/engine/mod.rs:82-180` already isolates `PolicyEngineActor` on its own thread.

This means the original "shared Tokio executor blocked for 8-10s" diagnosis does not hold on the current branch.

### 2. `auto-queue.js` has heavy 1-minute DB recovery paths

`policies/auto-queue.js:463-545` performs multiple sweeps in one `onTick1min` pass:

- scans terminal pending entries across `auto_queue_entries`, `auto_queue_runs`, and `kanban_cards`
- scans active or paused runs that can be finalized
- scans active runs with pending entries
- scans stuck dispatched entries and probes `task_dispatches` existence/status before resetting them

These are exactly the kind of queries that stay fast on an empty DB and degrade with real queue volume, missing indexes, or SQLite busy time.

### 3. `timeouts.js` 5-minute path includes synchronous HTTP fan-out

- `policies/timeouts.js:1474-1498` runs multiple sections in `onTick5min`
- `policies/timeouts.js:1357-1429` (`_section_O`) queries idle sessions and then issues one `agentdesk.http.post(.../force-kill)` per candidate session

This is the strongest explanation for the observed `onTick5min` outliers. A run that finds many idle sessions can spend most of its wall clock time in repeated local HTTP requests even if raw DB time stays moderate.

### 4. New instrumentation now distinguishes DB-heavy vs non-DB-heavy hooks

- `src/engine/mod.rs:564-580` and `src/engine/mod.rs:658-674` log `policy hook slow`
- `src/engine/ops/db_ops.rs:165-173` logs `policy db query slow`
- `src/engine/ops/db_ops.rs:225-232` logs `policy db execute slow`

These logs let production data separate:

- slow hook + slow DB log: SQLite scan/lock/contention dominated
- slow hook + many idle-kill HTTP logs: policy HTTP fan-out dominated
- slow hook without DB or HTTP evidence: JS object churn or QuickJS GC remains possible, but secondary

## Conclusion

Current evidence points to data-dependent policy logic, not baseline QuickJS overhead:

- Most likely 1-minute culprit: `policies/auto-queue.js:463-545`
- Most likely 5-minute culprit: `policies/timeouts.js:1357-1429` inside `timeouts.onTick5min`
- Least supported hypothesis after local profiling: "QuickJS dispatch itself normally takes 8-10s"

So the short-term `#735` fix is correct as damage control:

- move tick execution off the async runtime path with `spawn_blocking`
- bound each tick with a timeout
- avoid overlap with `skipped_inflight`

But the real long-tail bottleneck still needs policy-level or engine-architecture follow-up.

## Follow-Up

- Follow-up issue created: `#747` `fix(policy): timed-out tick hooks still occupy PolicyEngineActor after timeout`
- After deploy, inspect:

```bash
grep "policy-tick.*took\\|policy hook slow\\|policy db .* slow\\|idle-kill" ~/.adk/release/logs/dcserver.stdout.log
```

- If `policy hook slow` correlates with `policy db query slow` or `policy db execute slow`, optimize the specific SQL path or indexing first.
- If `policy hook slow` correlates with repeated `[idle-kill]` lines, reduce or batch the `_section_O` HTTP fan-out path first.
