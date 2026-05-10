# Intake-Message Routing to Worker Nodes — Design

> Status: **Draft v5** — codex-reviewed (rounds 1 + 2 + 3 + 4),
> addressing same-target no-op message drop, family-attempt cap
> enforcement, post-accept retry transition, and stale post-accept
> SLA prose.
> Awaiting human review (요부장) and codex high-effort review round 5.
> Owner: project-agentdesk (adk-cc).
> Related issues: #1984 retro (cluster IDENTIFY constraint), #1946 retro
> (publication routing). Codex review summaries in
> [Appendix A (round 1)](#appendix-a-codex-v1-review),
> [Appendix B (round 2)](#appendix-b-codex-v2-review),
> [Appendix C (round 3)](#appendix-c-codex-v3-review), and
> [Appendix D (round 4)](#appendix-d-codex-v4-review).

## Background

The AgentDesk Discord control plane has a hard asymmetry between
*dispatches* and *intake messages*:

- **Dispatches** (agent → agent task hand-off) flow through
  `dispatch_outbox` and are claimed by any worker matching the
  `cluster.dispatch_routing` policy. Mac-book worker handles many of
  these today.
- **Intake messages** (humans typing in a Discord channel) are
  received by whichever node holds the bot's `IDENTIFY` lease.
  Discord rate-limits IDENTIFY *per token*, so only the leader (mac-mini
  in our cluster) ever IDENTIFYs (validated by retro #1984).
  Intake therefore always spawns its tmux turn on the leader.

Requirement: **for selected agents/channels, route the intake turn
to a preferred worker node** so heavy work (Unreal builds, GAS state
queries, MCP runs) does not pin the leader's CPU.

This document is the up-front design before any code changes; we
explicitly avoid PoC iteration in favor of a single coherent rollout.
Draft v2 incorporates codex round-1 review findings (Appendix A).

## Goals

1. Per-agent (or per-channel) `preferred_intake_node_labels` config
   that mirrors the existing `cluster.dispatch_routing` shape.
2. When the leader receives an intake message and a healthy worker
   matches the agent's preferred labels, the leader forwards the work
   item via PG and the worker spawns the tmux turn locally. The
   leader's `handle_text_message` returns early.
3. When no matching worker is online, intake stays on the leader (no
   pending wait queue — the human is waiting for a reply).
4. Worker-side turn output reaches Discord through the worker's own
   `serenity::http::Http` REST client (the bot token already lives on
   every node; only IDENTIFY is leader-only).
5. Resumption of an existing session must respect node affinity: if a
   `sessions` row for this channel already has `instance_id = worker`,
   subsequent intake messages forward to that worker as long as that
   worker is online and fresh.

## Non-goals

- Auto-provisioning agent worktrees / repo clones across nodes.
  Operator is responsible for keeping `~/.adk/release/workspaces/<agent-id>/`
  and the user's repo paths (e.g. `~/CookingHeart`) consistent on
  every node that might be a routing target. Worker-side spawn will
  validate `cwd` existence and fail-soft (mark outbox row `failed` →
  leader fallback).
- Cross-node tmux migration. Once a turn has spawned on node X, it
  finishes on node X.
- Leader-less operation. We always assume one IDENTIFY-holding leader.
- Reworking the dispatch path. This design is additive to
  `dispatch_outbox`; it does not change dispatch claiming, scoring,
  or fallback behaviour.
- **Multi-primary PostgreSQL.** Design assumes a single linearizable
  PG primary that both leader and workers read/write. Sharded or
  geo-replicated PG is out of scope; we will reject the configuration
  at startup if cluster is configured against multiple primaries.

## Current state (read-only audit)

### Intake call chain
- `serenity::FullEvent` → `intake_gate::handle_event()`
  (`src/services/discord/router/intake_gate.rs:781`)
- → `handle_text_message()`
  (`src/services/discord/router/message_handler.rs:1795`)
- → `start_reserved_headless_turn()` (`router/turn_start.rs`)
- → `spawn_turn_bridge()` (`turn_bridge/mod.rs:1142`)
- → provider tmux create — `platform::tmux::create_session()`
  (`platform/tmux.rs:67`)

The intake path is **completely cluster-unaware today**: there is no
inspection of `cluster.instance_id`, `worker_nodes`, or session-owner
affinity inside `discord/router/`.

### Worker-side gateway state (codex blocker 1)

Non-leader nodes skip Discord gateway startup when the gateway lease
is held elsewhere
(`src/services/discord/runtime_bootstrap.rs:810`).
`cached_serenity_ctx` is only populated after a real gateway client
starts (`runtime_bootstrap.rs:1137`). But `handle_text_message`
expects a live `serenity::Context` and later constructs a
`DiscordGateway` around it (`message_handler.rs:1795,4123`).

**A "skip routing" flag is therefore not enough** to run the existing
function on a worker. We need a refactor that separates
"REST-only turn core" from "gateway-context-dependent intake work."

### Cluster primitives we will reuse

| Primitive | Source | Reused for |
|---|---|---|
| `worker_nodes` table (labels JSONB, last_heartbeat_at, status) | `migrations/postgres/0029_worker_nodes.sql` | Health + label match |
| `lease_ttl_secs` staleness (default 180s) | `src/server/cluster.rs:165` | Stale worker exclusion |
| `cluster.dispatch_routing` policy shape | `src/config.rs:731-788` | Mirror for `intake_routing` |
| `RoutingEngine::route()` capability matching | `src/services/dispatches/outbox_claiming.rs:75-99` | Same label scoring fn |
| `FOR UPDATE SKIP LOCKED` claim pattern | `src/db/dispatches/outbox/claim.rs:38,108` | Same SQL idiom |
| Adaptive 500ms→5s polling backoff | `src/services/dispatches/outbox_queue.rs:397` | Same loop tempo |
| `sessions.instance_id` column (already exists) | `migrations/postgres/0040_sessions_instance_id.sql` | Affinity routing |
| `session_owner_routing_status()` foreign-detection | `src/server/cluster_session_routing.rs:51-115` | Reuse verbatim |
| `serenity::http::Http` REST client | `src/services/discord/gateway.rs:95` | Worker-side response posting |
| `credential::read_bot_token()` | `src/credential.rs:1-25` | Worker-side token (no IDENTIFY needed for REST) |

### Confirmed feasibility constraints

1. Worker can call Discord REST without IDENTIFY — token loads from
   the credential store on every node. (Validated by reading the
   existing `HttpOutboundClient` in `services/discord/outbound/legacy.rs:533+`.)
2. `tmux_runtime::create_session()` fails fast if `cwd` is missing —
   worker can detect this and bail to leader without losing the
   message.
3. `sessions.instance_id` already pins each session to the spawn
   node; cross-node identity is a routing concern, not a schema gap.

## Proposed design

### Data model

#### A. `agents.preferred_intake_node_labels` (new column)
```sql
ALTER TABLE agents
    ADD COLUMN IF NOT EXISTS preferred_intake_node_labels JSONB
        NOT NULL DEFAULT '[]'::JSONB;
```
- Empty `[]` = no preference (current behaviour).
- Operators opt agents in one at a time.

#### B. `intake_outbox` (new table — modeled on `dispatch_outbox` but
distinct because the payload, ownership semantics, and completion
events differ)

```sql
CREATE TABLE IF NOT EXISTS intake_outbox (
    id                BIGSERIAL PRIMARY KEY,

    -- routing identity (codex blocker 2: instance-locked claim)
    target_instance_id      TEXT NOT NULL,
    forwarded_by_instance_id TEXT NOT NULL,
    required_labels         JSONB NOT NULL DEFAULT '[]'::JSONB,
        -- audit/diagnostics only; claim is by instance_id

    -- intake payload (parameters of handle_text_message)
    channel_id        TEXT NOT NULL,
    user_msg_id       TEXT NOT NULL,
    request_owner_id  TEXT NOT NULL,
    request_owner_name TEXT,
    user_text         TEXT NOT NULL,
    reply_context     TEXT,
    has_reply_boundary BOOLEAN NOT NULL DEFAULT FALSE,
    dm_hint           BOOLEAN,
    turn_kind         TEXT NOT NULL,
    merge_consecutive BOOLEAN NOT NULL DEFAULT FALSE,
    reply_to_user_message BOOLEAN NOT NULL DEFAULT FALSE,
    defer_watcher_resume BOOLEAN NOT NULL DEFAULT FALSE,
    wait_for_completion BOOLEAN NOT NULL DEFAULT FALSE,
    agent_id          TEXT NOT NULL,

    -- state machine (codex blocker 3 + round-2 P0 #2)
    status            TEXT NOT NULL DEFAULT 'pending',
    claim_owner       TEXT,           -- echoes target_instance_id once claimed
    claimed_at        TIMESTAMPTZ,    -- claim transition
    accepted_at       TIMESTAMPTZ,    -- worker accepted (after ALL retryable validation)
    spawned_at        TIMESTAMPTZ,    -- tmux session created
    completed_at      TIMESTAMPTZ,
    last_error        TEXT,
    retry_count       INTEGER NOT NULL DEFAULT 0,

    -- Round-3 P0 #1: per-message attempt history. Each `retry-local`
    -- / `retry-as-new` / sweep auto-retry creates a fresh row with
    -- `attempt_no = MAX(prior) + 1` and `parent_outbox_id` pointing
    -- at the row whose terminal failure motivated the retry. The
    -- original `(channel_id, user_msg_id)` uniqueness is replaced by
    -- a 3-tuple including attempt_no so all attempts persist for
    -- audit while open-route exclusivity stays per-channel.
    attempt_no        INTEGER NOT NULL DEFAULT 1,
    parent_outbox_id  BIGINT REFERENCES intake_outbox(id) ON DELETE SET NULL,

    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- Round-5 P1 #1: name the 3-tuple constraint so the Rust
    -- handler can distinguish a "duplicate attempt_no within a
    -- (channel,user_msg_id) family" violation (this constraint)
    -- from a "second OPEN row per channel" violation (the partial
    -- unique index `intake_outbox_one_open_route_per_channel`).
    -- PostgreSQL surfaces both as SQLSTATE 23505; the
    -- constraint/index name is the discriminator.
    CONSTRAINT intake_outbox_unique_message_attempt
        UNIQUE (channel_id, user_msg_id, attempt_no),
    CONSTRAINT intake_outbox_attempt_no_positive CHECK (attempt_no >= 1),

    -- DB-level state guard. A round-2 P1 finding: prose is not enough.
    CONSTRAINT intake_outbox_status_check CHECK (status IN (
        'pending',
        'claimed',
        'accepted',
        'spawned',
        'done',
        'failed_pre_accept',   -- codex round-2 P0 #2: retryable
        'failed_post_accept'   -- codex round-2 P0 #2: terminal/manual
    ))
);

-- Round-2 P0 #1: durable per-channel open-route invariant. PostgreSQL
-- partial unique index lets at most ONE row per channel exist in any
-- "open" status. Concurrent inserts that would create a second open
-- row fail with unique-violation; the loser re-evaluates against the
-- existing row's target. Covers the gap where two messages serialize
-- through the advisory lock but each sees the other still in `pending`.
CREATE UNIQUE INDEX IF NOT EXISTS intake_outbox_one_open_route_per_channel
    ON intake_outbox (channel_id)
    WHERE status IN ('pending', 'claimed', 'accepted', 'spawned');

-- Worker poll: only own target.
CREATE INDEX IF NOT EXISTS idx_intake_outbox_worker_pending
    ON intake_outbox (target_instance_id, status, created_at)
    WHERE status = 'pending';

-- Leader sweep: stale claims that never reached `accepted`.
CREATE INDEX IF NOT EXISTS idx_intake_outbox_pre_accept_sweep
    ON intake_outbox (status, claimed_at)
    WHERE status = 'claimed';

-- Round-3 P0 #2: leader sweep for `failed_pre_accept` rows that
-- still have retry budget remaining. These get re-issued as a fresh
-- attempt via SQL transition 10 below.
CREATE INDEX IF NOT EXISTS idx_intake_outbox_failed_pre_accept_sweep
    ON intake_outbox (status, retry_count, updated_at)
    WHERE status = 'failed_pre_accept';

-- Round-3 P1 #3: fast SLA detector for `accepted` rows that have
-- not reached `spawned` within the per-channel SLA window.
CREATE INDEX IF NOT EXISTS idx_intake_outbox_accepted_unspawned_sla
    ON intake_outbox (status, accepted_at)
    WHERE status = 'accepted';

-- Audit chain lookup: walk parent_outbox_id back to attempt_no = 1.
CREATE INDEX IF NOT EXISTS idx_intake_outbox_parent
    ON intake_outbox (parent_outbox_id)
    WHERE parent_outbox_id IS NOT NULL;

-- Trigger to keep `updated_at` fresh on every transition.
CREATE OR REPLACE FUNCTION intake_outbox_touch_updated_at()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_intake_outbox_touch_updated_at
    BEFORE UPDATE ON intake_outbox
    FOR EACH ROW EXECUTE FUNCTION intake_outbox_touch_updated_at();
```

**Column note: no Discord token in the outbox.** The worker loads
its own bot token via `credential::read_bot_token()` at startup;
nothing token-shaped is ever serialized into PG.

**Column note: no attachment payload.** If the original message has
file uploads, the leader handles it locally (see "Pending uploads"
below); we never copy uploaded bytes into the outbox.

#### B-bis. Canonical state-transition SQL (codex round-2 P1 #3)

Every transition is a **compare-and-set** UPDATE. The DB-level CHECK
constraint above blocks illegal terminal status values; the WHERE
clauses below enforce legal *transitions*. Sweeps use
`FOR UPDATE SKIP LOCKED` so concurrent leader/worker sweeps never
contend.

```sql
-- 1. Leader: insert a forwarded intake. The partial unique index
--    blocks a second open row per channel.
INSERT INTO intake_outbox (
    target_instance_id, forwarded_by_instance_id, required_labels,
    channel_id, user_msg_id, request_owner_id, request_owner_name,
    user_text, reply_context, has_reply_boundary, dm_hint, turn_kind,
    merge_consecutive, reply_to_user_message, defer_watcher_resume,
    wait_for_completion, agent_id, status
) VALUES (..., 'pending')
-- ON CONFLICT (channel_id) WHERE status IN ('pending','claimed',
--   'accepted','spawned') is not directly expressible; instead we
-- catch the unique-violation in the leader path and re-evaluate.
;

-- 2. Worker: claim a pending row addressed to me. Single row, locked.
SELECT id, channel_id, user_msg_id, ... -- full payload
  FROM intake_outbox
 WHERE target_instance_id = $local
   AND status = 'pending'
 ORDER BY created_at
 FOR UPDATE SKIP LOCKED
 LIMIT 1;

UPDATE intake_outbox
   SET status = 'claimed',
       claim_owner = $local,
       claimed_at = NOW()
 WHERE id = $1
   AND status = 'pending'         -- compare-and-set guard
   AND target_instance_id = $local
RETURNING id;
-- If RETURNING is empty, somebody else won. Re-poll.

-- 3. Worker: accept (only after every retryable validation
--    succeeds: cwd present, agent workspace ready, etc.).
--    Accepting AFTER any user-visible side effect (placeholder
--    POST, mailbox enqueue, reaction add) is forbidden — see
--    §"Worker-side state machine details" for ordering.
UPDATE intake_outbox
   SET status = 'accepted',
       accepted_at = NOW()
 WHERE id = $1
   AND status = 'claimed'
   AND claim_owner = $local
RETURNING id;

-- 4. Worker: spawned (tmux session created, turn_bridge attached).
UPDATE intake_outbox
   SET status = 'spawned',
       spawned_at = NOW()
 WHERE id = $1
   AND status = 'accepted'
   AND claim_owner = $local
RETURNING id;

-- 5. Worker: terminal success.
UPDATE intake_outbox
   SET status = 'done',
       completed_at = NOW()
 WHERE id = $1
   AND status IN ('spawned', 'accepted')
   AND claim_owner = $local
RETURNING id;

-- 6a. Worker: pre-accept retryable failure (cwd missing, workspace
--     unprovisioned, transient DB / token issue before any
--     user-visible side effect). Leader sweep can spawn a fresh
--     attempt via transition 10 (capped by max_attempts_per_message).
UPDATE intake_outbox
   SET status = 'failed_pre_accept',
       last_error = $2,
       retry_count = retry_count + 1
 WHERE id = $1
   AND status IN ('pending', 'claimed')
   AND claim_owner = $local
RETURNING id, retry_count;

-- 6b. Worker: post-accept terminal failure (turn execution failure
--     after we already posted a placeholder / changed user-visible
--     state). Leader sweep does NOT auto-retry. Manual via CLI.
UPDATE intake_outbox
   SET status = 'failed_post_accept',
       last_error = $2,
       completed_at = NOW()
 WHERE id = $1
   AND status IN ('accepted', 'spawned')
   AND claim_owner = $local
RETURNING id;

-- 7. Leader: pre-claim takeover. Re-target to leader's own
--    instance_id; reset claim metadata. Bound by retry_count.
UPDATE intake_outbox
   SET target_instance_id = $local_leader,
       claim_owner = NULL,
       claimed_at = NULL,
       retry_count = retry_count + 1,
       last_error = COALESCE(last_error, '') ||
                    '; pre_claim_timeout from prior target'
 WHERE id = $1
   AND status = 'pending'
   AND retry_count < $max_retries
   AND created_at < NOW() - ($pre_claim_timeout * INTERVAL '1 second')
RETURNING id;

-- 8. Leader: stale-claim recovery (worker died after claim, before
--    accept). The claim was BEFORE any user-visible side effect, so
--    re-pinging is safe.
UPDATE intake_outbox
   SET status = 'pending',
       claim_owner = NULL,
       claimed_at = NULL,
       retry_count = retry_count + 1,
       last_error = COALESCE(last_error, '') ||
                    '; stale_claim_recovery'
 WHERE id = $1
   AND status = 'claimed'
   AND claimed_at < NOW() - ($stale_claim_threshold * INTERVAL '1 second')
   AND retry_count < $max_retries
RETURNING id;

-- 9. Leader: retry-budget exhausted while still in pre-accept
--    statuses. Mark as terminal failure (still pre-accept means no
--    user-visible state change occurred; CLI can request a fresh
--    attempt via transition 10).
UPDATE intake_outbox
   SET status = 'failed_pre_accept',
       last_error = COALESCE(last_error, '') ||
                    '; retry_count exhausted',
       completed_at = NOW()
 WHERE id = $1
   AND retry_count >= $max_retries
   AND status IN ('pending', 'claimed')
RETURNING id;

-- 10. Leader sweep / CLI: spawn a fresh attempt from a terminal
--     failed_pre_accept row. Round-3 P0 #2 fix: prose said
--     failed_pre_accept was retryable but no SQL transitioned it.
--
--     This INSERT generates a new row with attempt_no = family_max
--     + 1 in the same (channel_id, user_msg_id) family,
--     parent_outbox_id pointing at the source terminal row,
--     status='pending', target_instance_id = leader.
--
--     Round-4 P0 #2 fix: cap is on the COMPUTED next attempt
--     number against the family max, not on the parent row's
--     attempt_no. Plus we require the parent to BE the family max
--     so retries from old (non-latest) attempts are rejected —
--     keeps the audit chain linear and prevents back-dated
--     retries from another fork bypassing the cap.
WITH family AS (
    SELECT
        parent.id           AS parent_id,
        parent.channel_id,
        parent.user_msg_id,
        parent.attempt_no   AS parent_attempt_no,
        parent.required_labels,
        parent.request_owner_id,
        parent.request_owner_name,
        parent.user_text,
        parent.reply_context,
        parent.has_reply_boundary,
        parent.dm_hint,
        parent.turn_kind,
        parent.merge_consecutive,
        parent.reply_to_user_message,
        parent.defer_watcher_resume,
        parent.wait_for_completion,
        parent.agent_id,
        (SELECT COALESCE(MAX(attempt_no), 0)
           FROM intake_outbox sub
          WHERE sub.channel_id = parent.channel_id
            AND sub.user_msg_id = parent.user_msg_id) AS family_max
      FROM intake_outbox parent
     WHERE parent.id = $1
       AND parent.status = 'failed_pre_accept'
)
INSERT INTO intake_outbox (
    target_instance_id, forwarded_by_instance_id, required_labels,
    channel_id, user_msg_id, request_owner_id, request_owner_name,
    user_text, reply_context, has_reply_boundary, dm_hint, turn_kind,
    merge_consecutive, reply_to_user_message, defer_watcher_resume,
    wait_for_completion, agent_id, status,
    attempt_no, parent_outbox_id, retry_count
)
SELECT
    $local_leader, $local_leader, required_labels,
    channel_id, user_msg_id, request_owner_id, request_owner_name,
    user_text, reply_context, has_reply_boundary, dm_hint, turn_kind,
    merge_consecutive, reply_to_user_message, defer_watcher_resume,
    wait_for_completion, agent_id, 'pending',
    family_max + 1,
    parent_id,
    0
  FROM family
 WHERE family_max + 1 <= $max_attempts_per_message
   AND parent_attempt_no = family_max  -- only retry from latest
RETURNING id, attempt_no;

-- 10b (transition 12). Operator-confirmed retry from a post-accept
--     row (`accepted`, `spawned`, or `failed_post_accept`).
--     Round-4 P1 #3 fix: transition 10 only handled
--     `failed_pre_accept`; the CLI's `retry-as-new` path needs a
--     distinct transition that (a) marks the source terminal first
--     in the same transaction so the partial unique index lets the
--     new row in, and (b) requires explicit operator confirmation
--     because user-visible state has already happened.
--
--     Two-step in one transaction:
BEGIN;

UPDATE intake_outbox
   SET status = 'failed_post_accept',
       last_error = COALESCE(last_error, '') ||
                    '; force_failed_for_retry_as_new by operator',
       completed_at = COALESCE(completed_at, NOW())
 WHERE id = $1
   AND status IN ('accepted', 'spawned')   -- already-failed_post_accept rows skip this UPDATE
RETURNING id;

WITH family AS (
    SELECT parent.id, parent.channel_id, parent.user_msg_id,
           parent.attempt_no AS parent_attempt_no,
           parent.required_labels, parent.request_owner_id,
           parent.request_owner_name, parent.user_text,
           parent.reply_context, parent.has_reply_boundary,
           parent.dm_hint, parent.turn_kind, parent.merge_consecutive,
           parent.reply_to_user_message, parent.defer_watcher_resume,
           parent.wait_for_completion, parent.agent_id,
           (SELECT COALESCE(MAX(attempt_no), 0) FROM intake_outbox sub
             WHERE sub.channel_id = parent.channel_id
               AND sub.user_msg_id = parent.user_msg_id) AS family_max
      FROM intake_outbox parent
     WHERE parent.id = $1
       AND parent.status IN ('failed_post_accept')
)
INSERT INTO intake_outbox (
    target_instance_id, forwarded_by_instance_id, required_labels,
    channel_id, user_msg_id, request_owner_id, request_owner_name,
    user_text, reply_context, has_reply_boundary, dm_hint, turn_kind,
    merge_consecutive, reply_to_user_message, defer_watcher_resume,
    wait_for_completion, agent_id, status,
    attempt_no, parent_outbox_id, retry_count
)
SELECT $local_leader, $local_leader, required_labels,
       channel_id, user_msg_id, request_owner_id, request_owner_name,
       user_text, reply_context, has_reply_boundary, dm_hint, turn_kind,
       merge_consecutive, reply_to_user_message, defer_watcher_resume,
       wait_for_completion, agent_id, 'pending',
       family_max + 1, id, 0
  FROM family
 WHERE family_max + 1 <= $max_attempts_per_message
   AND parent_attempt_no = family_max
RETURNING id, attempt_no;

COMMIT;

-- 11. Leader sweep: SLA on `accepted` not reaching `spawned`.
--     Round-3 P1 #3: `accepted` should normally last seconds. If
--     >`accepted_unspawned_sla_secs` (default 120s) without a
--     `spawned_at`, surface as a fast operator alert; the row stays
--     in `accepted` (auto-retry is forbidden post-accept). Operator
--     decides force-fail or wait.
SELECT id, channel_id, user_msg_id, claim_owner, accepted_at
  FROM intake_outbox
 WHERE status = 'accepted'
   AND accepted_at < NOW() - ($accepted_unspawned_sla * INTERVAL '1 second');
```

**Why no SQL function wrappers**: keep call sites discoverable; Rust
helpers in `src/db/intake_outbox/` host the parameter binding and
typed return values. The SQL itself stays in plain query strings so
operators can run it manually for incident response.

**Why a fresh row instead of in-place reset for retry**: keeps every
attempt's `last_error`, timing, and `claim_owner` in PG for audit. A
row whose state is `failed_pre_accept` is the historical record of
that attempt; the next attempt is a new row with `parent_outbox_id`
linking it to the previous one. Operators reading the audit chain
(via the `idx_intake_outbox_parent` index) see the full history of
"this user message was forwarded to mac-book, failed cwd validation,
retried as attempt 2 on leader, succeeded."

#### C. Config: `cluster.intake_routing`

```yaml
cluster:
  intake_routing:
    # Hard kill switch — flip false during incident response to
    # disable all intake forwarding cluster-wide. Hot-reload supported.
    enabled: true
    # observe: emit decision events but do NOT INSERT outbox rows.
    # enforce: actually forward.
    mode: "observe"
    # Pre-claim takeover threshold. After this many seconds without
    # reaching `claimed` (= worker SELECT FOR UPDATE), leader steals
    # and runs locally. Spawned/accepted rows are NEVER stolen.
    forward_pre_claim_timeout_secs: 12
    # Stale claim recovery. After this many seconds in 'claimed'
    # without reaching 'accepted', re-mark pending so another node
    # can pick up. Tuned > worker startup grace.
    stale_claim_recovery_secs: 60
    # Round-3 P1 #3: fast SLA on `accepted` not reaching `spawned`.
    # Auto-retry is forbidden post-accept, so this surfaces as an
    # operator alert only. Default 2 minutes; tune up if Unreal cold
    # provider startup needs longer.
    accepted_unspawned_sla_secs: 120
    # Max retries before status='failed_pre_accept'. Prevents
    # infinite re-pick within a single attempt.
    max_retries: 3
    # Round-3 P0 #1: hard cap on attempts per (channel_id,
    # user_msg_id) family across all `failed_pre_accept`-driven
    # transition-10 retries. Prevents an unrelenting cwd-missing
    # condition from filling the audit chain.
    max_attempts_per_message: 5
```

We deliberately **drop `default_preferred_labels`** (codex round-1
recommendation): cluster-wide preference would route ALL agents'
intake to a worker, which is too coarse for the rollout. Per-agent
opt-in only.

### Decision flow

```
                        ┌──────────────────────────────────┐
       Discord intake → │ leader: handle_text_message()    │
                        └─────────────┬────────────────────┘
                                      ▼
                  ┌───────────────────────────────────────┐
                  │ load session for (provider, channel) │
                  └───────────────┬───────────────────────┘
                                  ▼
        ┌─────────────────────────────────────────────────────┐
        │ resolve_intake_target(agent, session, cluster_cfg, │
        │                       local_instance, workers)     │
        └────────────────────┬────────────────────────────────┘
                             ▼
            ┌───────────────────────────────────┐
            │ target == Local? → continue       │
            │ existing handle_text_message body │
            └───────────────────────────────────┘
                             OR
            ┌───────────────────────────────────────────────┐
            │ target == ForwardToWorker { instance_id, ... }:
            │   if cluster.mode == observe:                 │
            │     emit `intake_routing_decision` event only │
            │     and continue local handling               │
            │   if cluster.mode == enforce:                 │
            │     hold pg_advisory_xact_lock(channel_id)     │
            │     check no OPEN row exists for the channel — │
            │       OPEN = status IN ('pending','claimed',   │
            │       'accepted','spawned'). Round-3 P1 #4 fix.│
            │     INSERT INTO intake_outbox (target_instance │
            │       _id = worker, status='pending',          │
            │       attempt_no = 1, parent_outbox_id = NULL).│
            │       Partial unique index is the durable      │
            │       guard; advisory lock is liveness only.   │
            │     emit `intake_routing_decision` event       │
            │     leave 📩 reaction on user message          │
            │     return Ok(()) early — no mailbox enqueue,  │
            │     no turn spawn locally                      │
            └───────────────────────────────────────────────┘

                  Worker (mac-book) polling loop:
                    SELECT id FROM intake_outbox
                     WHERE target_instance_id = $local AND status='pending'
                     ORDER BY created_at FOR UPDATE SKIP LOCKED LIMIT 1
                    → UPDATE status='claimed', claim_owner=$local,
                       claimed_at=NOW() (transition 2)
                    → validate cwd / agent workspace exists
                       on failure: status='failed_pre_accept' (transition 6a);
                                   leader sweep transition 10 spawns
                                   a fresh attempt on leader
                    → UPDATE status='accepted', accepted_at=NOW()
                       (transition 3 — only after ALL retryable
                        validation done, before any user-visible
                        side effect)
                    → execute_intake_turn_core(payload)
                       (REST-safe wrapper, see next section)
                    → UPDATE status='spawned', spawned_at=NOW()
                       (transition 4 — turn_bridge spawned tmux)
                    → on turn_bridge completion / failure:
                       UPDATE status='done' (transition 5)
                       or  status='failed_post_accept' (transition 6b)
```

### `execute_intake_turn_core` — REST-safe extraction (codex blocker 1)

The current `handle_text_message` mixes:

a) Routing decision (cluster-aware) ← we add this
b) Gateway-context lookups (e.g. `cached_serenity_ctx`)
c) Live serenity event handle wiring
d) REST-safe operations (post placeholder, edit reaction, mailbox
   enqueue, turn spawn, dispatched session lifecycle)

The forwardable subset is (a)+(d). Worker has no (b)/(c).

**Refactor strategy** (Phase 2-pre, see implementation phases):

1. Audit `handle_text_message` for every direct use of
   `serenity::Context`. Each call site classifies as either
   "REST equivalent exists" (replaceable with `serenity::http::Http`)
   or "true gateway dependency" (must stay leader-side).
2. Extract `execute_intake_turn_core(deps: IntakeDeps, payload: IntakePayload) -> Result<()>`,
   where `IntakeDeps` carries only what the REST-safe path needs:
   `Arc<serenity::http::Http>`, `Arc<SharedData>`, provider, token,
   PG pool, instance_id.
3. Leader's existing `handle_text_message` becomes a thin wrapper:
   builds `IntakeDeps` from `serenity::Context`, calls
   `execute_intake_turn_core`. **No behaviour change for unforwarded
   messages.**
4. Worker's `handle_forwarded_intake` builds `IntakeDeps` from its
   own gateway-less HTTP client + bot token, calls the same
   `execute_intake_turn_core`.

This refactor PR ships independently and is verified by all
existing intake tests passing on the leader path.

If the audit finds a true gateway dependency in the intake path that
cannot be REST-replaced (e.g. live presence subscription), we
either:
- a) keep that logic leader-side and have the worker emit a PG row
  asking leader to perform that side effect, or
- b) declare the affected feature unsupported on forwarded intakes
  and prove no current agent uses it.

Whichever applies will be documented as part of the Phase 2-pre PR.

### Resolution rules (`resolve_intake_target`)

```rust
enum IntakeTarget {
    Local,
    ForwardToWorker {
        instance_id: String,    // codex blocker 2: instance-locked
        labels_at_decision: Vec<String>,
            // diagnostics only; claim does not match on labels
    },
}
```

Decision tree (evaluation order is important):

1. **Kill switch**: `cluster.intake_routing.enabled = false` → `Local`.
2. **No cluster context**: leader running standalone (no PG, single
   node) → `Local`.
3. **Pending uploads on the message**: forwarded uploads not
   supported in v1 → `Local`. (Operator can opt in once we ship
   upload portability.)
4. **Active session affinity**: existing `sessions` row for
   `(provider, channel_id, status IN active-set)` with
   `instance_id != local && worker is online && fresh heartbeat` →
   forward to that worker.
5. **Stale-affinity guard** (codex round-1 recommendation): if step
   4 finds a session pinned to an offline / stale foreign worker,
   **do not auto-re-pick**; instead surface as a degraded state via
   observability and fall through to `Local`. Re-pinning to a new
   worker requires explicit operator action (CLI command in Phase 5)
   because we do not know whether the foreign worker's tmux is truly
   dead. Routing pre-emptively could spawn a duplicate turn.
6. **Per-agent preference**: `agents.preferred_intake_node_labels`
   non-empty → match against online fresh workers. If local matches
   → `Local`. If a foreign worker matches → forward.
7. **No match** → `Local` (leader fallback; user never waits).

### Per-channel ordering lock (codex round-2 P1 #5)

The DB-level partial unique index from §B is the **durable**
guarantee — only one open route per channel exists at any time. The
advisory lock is a **liveness aid** that lets concurrent leader
inserts serialize cleanly instead of all but one failing with
unique-violation.

**Critical-section discipline**: the advisory lock window must be
small. All slow lookups (worker_nodes label match, session
inspection, decision evaluation) happen *outside* the transaction.
Inside the locked transaction the leader only:

1. Re-reads the current open route for the channel (single indexed
   SELECT against the partial unique index).
2. Re-validates the chosen worker is still online + fresh.
3. INSERTs the new row, or no-ops if an open route already points
   at the same target.

```rust
// Pseudocode for the leader insert path.
// Pre-computed outside the txn (no lock).
let candidate_target = resolve_intake_target(...).await?;
let worker_snapshot   = load_eligible_workers().await?;

// Short critical section.
let mut tx = pool.begin().await?;
sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
    .bind(&channel_id).execute(&mut tx).await?;

let existing_open = lookup_open_route(&mut tx, &channel_id).await?;
match (existing_open, candidate_target) {
    // Round-4 P0 #1 fix: only treat as a true no-op when this is
    // the exact same (channel_id, user_msg_id) — i.e. a duplicate
    // delivery of the same Discord event we already routed.
    // Different user_msg_id on the same channel must NOT be silently
    // dropped here.
    (Some(open), ForwardToWorker { instance_id, .. })
        if open.target_instance_id == instance_id
            && open.user_msg_id == new_msg.user_msg_id => {
        /* idempotent retransmission: do nothing */
    }
    // Distinct user_msg_id on a channel that already has an open
    // forwarded route → fall back to local handling. The existing
    // forwarded turn will eventually finish and the user's next
    // message proceeds normally. Queueing a second forwarded turn
    // for the same channel would race the worker's mailbox/turn
    // bridge in undefined ways; we keep ordering simple.
    (Some(_), ForwardToWorker { .. }) => return Ok(IntakeOutcome::Local),
    // Channel already has an open forwarded route, but we resolved
    // to Local for this message (e.g. preference flipped while a
    // previous forward is still in flight). Honor the existing
    // forward to preserve session affinity.
    (Some(open), Local) => return Ok(IntakeOutcome::Forwarded(open.target_instance_id)),
    // Fresh channel: insert.
    (None, ForwardToWorker { instance_id, .. })
        if still_eligible(&worker_snapshot, &instance_id) => {
        insert_outbox_row(&mut tx, ...).await?;
    }
    _ => return Ok(IntakeOutcome::Local),
}
tx.commit().await?;
```

The advisory lock is `pg_advisory_xact_lock`, scoped to the
transaction; it auto-releases at commit/rollback. We use
`hashtextextended(channel_id, 0)` to derive the int8 lock key from
the channel id string.

**Why both the lock AND the partial unique index**: the unique
index is correctness; the advisory lock is to avoid log noise from
unique-violation rollback under burst load. Either alone is
sufficient for correctness.

### Worker-side state machine details (codex blocker 3 + round-2 P0 #2)

```
pending ──worker SELECT FOR UPDATE──▶ claimed ──validation OK──▶ accepted ──tmux──▶ spawned ──turn end──▶ done
   │                                     │                          │                  │
   │                                     │ validation FAIL          │ run FAIL         │ run FAIL
   │                                     ▼                          ▼                  ▼
   │   ┌── leader pre-claim takeover  failed_pre_accept     failed_post_accept   failed_post_accept
   │   │   (re-target to leader,           (retryable)          (terminal/manual)
   │   │   bump retry, stay pending)
   │
   ▼
pending (retried)
```

**Critical invariant** (round-2 P0 #2 fix): `accepted` is reached
**only after all retryable validations succeed AND before any
user-visible side effect**. Concretely, the worker performs in
order:

1. Validate cwd / agent workspace exists.
2. Validate any agent-specific preconditions (provider runtime
   reachable, CLI binary present, tmux available).
3. Atomically transition `claimed → accepted` via the SQL in §B-bis.
4. **Only after `accepted`** does the worker call
   `execute_intake_turn_core`, which posts placeholders, edits
   reactions, enqueues mailbox, etc.

This ordering means `failed_pre_accept` rows never have associated
user-visible state, so leader sweep can safely re-target them
without duplicating anything.

Conversely, if any failure happens *after* the accepted transition,
it's `failed_post_accept` and is terminal until an operator
explicitly intervenes via CLI.

**Pre-claim phase** (`pending`):
- Pre-claim takeover by leader IS allowed after
  `forward_pre_claim_timeout_secs` (default 12s). Leader rewrites
  `target_instance_id = local_leader_id`, leaves `status='pending'`,
  bumps `retry_count`. Leader's own poll then claims it.
- Bounded by `max_retries` (default 3); on exhaustion, transition
  9 in §B-bis promotes to `failed_pre_accept` for operator action.

**Claimed phase** (`claimed`):
- Worker has SELECTed FOR UPDATE and UPDATEd to claimed.
- Worker now runs the validation steps above. **No user-visible
  side effects yet.**
- If validation fails → transition 6a → `failed_pre_accept`.
- If worker dies before reaching `accepted` (process kill, panic
  in validation), `claimed_at` becomes stale → leader sweep
  (`stale_claim_recovery_secs`, default 60s) → transition 8 resets
  to `pending`.

**Accepted phase** (`accepted`):
- Worker has validated everything that can be validated cheaply and
  is about to begin user-visible work. **No other node may UPDATE
  this row's status until the worker writes `spawned`,
  `done`, or `failed_post_accept`** — auto-retry forbidden.
- Round-3 P1 #3 fast SLA: a row stuck in `accepted` past
  `accepted_unspawned_sla_secs` (default 120s) without
  `spawned_at` triggers an operator alert via transition 11. The
  alert is the recovery signal; CLI `force-fail <row>` writes
  `failed_post_accept` + audit. No auto recovery.

**Spawned phase** (`spawned`):
- Tmux session created, turn_bridge running.
- Completion callback writes `done`; panic / process death leaves
  `spawned` indefinitely. A row stuck in `spawned` past 24h with
  no completion triggers a slow operator alert (worker-side panic
  suspected). Same CLI tooling as `accepted` stuck.

**Failure terminals**:
- `failed_pre_accept`: retry-eligible. **Leader sweep applies
  transition 10 to INSERT a fresh attempt** with `attempt_no =
  family_max + 1`, `parent_outbox_id` linked, `target_instance_id =
  local_leader`. Capped by `max_attempts_per_message`. Transitions
  7 and 8 only operate on still-OPEN rows (`pending`/`claimed`)
  and are unrelated to `failed_pre_accept` recovery — round-3 P0
  fix added transition 10 specifically for that lane. CLI
  `retry-local <row>` is the operator-driven equivalent.
- `failed_post_accept`: terminal. Operator decides. CLI provides
  `force-fail <row>` (audit only) and `retry-as-new <row>` which
  runs **transition 12** in a single transaction: force-fail the
  source + INSERT a fresh attempt linked via `parent_outbox_id`.
  Same `family_max + 1 <= max_attempts_per_message` cap.

**Why this split matters**: under v2 a single `failed` status
created the contradiction codex flagged — the doc allowed retry
after worker death (in flight cwd validation) but forbade it after
spawn. v3 makes the boundary explicit at the schema level.

### Pending uploads (codex round-1 recommendation)

If the intake message has Discord attachments, leader downloads them
during `handle_text_message` (today) and stores temp paths in
`shared.pending_uploads`. **These local paths are not portable to
worker.**

v1 policy: rule 3 of the decision tree returns `Local` whenever
`message.attachments.is_empty() == false`. Leader handles uploads.

v2 (out of scope for this PR): forward upload bytes through
`message_outbox`-style payload table, or have worker re-download
from Discord CDN (requires re-resolving attachment URLs from the
message; possibly cleaner).

### Worker → Discord response

No new component; the worker uses its own `serenity::http::Http`
client built from the locally-loaded bot token (path identical to
leader). Existing turn-bridge output streaming, reaction toggling,
and queued-card edits all use the worker's local Http client — no
PG hop needed for the response.

### Hook semantics on the worker (codex round-1 recommendation)

For features that are leader-only by current design (e.g. some
gateway-context-dependent flow we discover during the Phase 2-pre
audit), the worker's intake handler must NOT emit them — but only
those.

**Crucially**, hooks like `OnDispatchCompleted` (the JS policy hook
made authoritative for phase-gate state in #1980) MUST fire normally
on the worker if the forwarded turn happens to complete a dispatch.
The hook is gated on the dispatch terminal write, not on whether the
turn was forwarded. The Phase 2-pre refactor preserves this: the
hook fires from `dispatch_status::set_dispatch_status_on_pg_with_sync`
which is REST-safe and identical on both sides.

## Failure modes & fallback

| Mode | Detection | Action |
|---|---|---|
| Worker offline (stale heartbeat) | leader-side `worker_nodes.last_heartbeat_at` check at `resolve_intake_target` | route falls through to `Local` |
| Worker dies before claim | row stays `pending` past `forward_pre_claim_timeout_secs` (12s) | transition 7: leader re-targets to local, bumps retry; on retry exhaustion → `failed_pre_accept` + alert |
| Worker dies in `claimed` (cwd validation in flight) | row stuck `claimed` past `stale_claim_recovery_secs` (60s) | transition 8: leader resets to `pending`, increments retry; on exhaustion → `failed_pre_accept` + alert |
| Worker validation fails (cwd missing, workspace unprovisioned) | worker writes `failed_pre_accept` via transition 6a | leader sweep applies transition 10 to INSERT a fresh attempt with `target_instance_id = local_leader` (linked via `parent_outbox_id`). If `attempt_no >= max_attempts_per_message`, transition 10 refuses → operator alert; further retries require `retry-as-new` via transition 12 |
| Worker dies in `accepted` | accepted_at > `accepted_unspawned_sla_secs` (default 120s) without `spawned_at` (transition 11) | fast operator alert; **no auto recovery** (round-2 P0 #2). Operator runs `force-fail <row>` (terminal failure) or `retry-as-new <row>` (transition 12, fresh row leader-targeted) |
| Worker dies in `spawned` | row stuck without `done` > 24h | operator alert; **no auto recovery** (round-2 P0 #2). Operator runs `force-fail <row>` (terminal failure) or `retry-as-new <row>` (fresh row, leader-targeted) |
| Worker post-accept turn failure (provider error, panic in tmux) | worker writes `failed_post_accept` via transition 6b | terminal; operator decides via CLI |
| Discord token revoked on worker (REST 401) | worker writes `failed_post_accept` (we already POSTed placeholder under that token) | terminal; operator rotates token + `retry-as-new` |
| Pending uploads on message | rule 3 of decision tree | always `Local`; leader handles |
| Active session pinned to stale foreign worker | rule 5 of decision tree | `Local`, observability alert; operator decides via CLI whether to clear session pin |
| Single-PG-primary assumption violated | startup self-check at boot | refuse to start with intake_routing.enabled=true; log error |
| Two same-channel forward attempts (round-2 P0 #1) | partial unique index `intake_outbox_one_open_route_per_channel` | second INSERT fails unique-violation; leader catches, re-evaluates against existing row. **Round-4 P0 #1 fix**: only the same `(channel_id, user_msg_id)` is treated as idempotent no-op; a different `user_msg_id` returns `Local` (the existing forwarded turn proceeds, the new message is handled locally — never silently dropped) |
| Two leader instances both decide to forward (e.g. failover transition) | partial unique index | same as above; only one wins |
| `observe → enforce` flip mid-flight | leader snapshots config inside the short insert transaction | inserts that began under `observe` complete locally; first insert under `enforce` writes a row |
| `enforce → observe` rollback (incident) | leader snapshots config; new intakes go `Local` | already-inserted rows in `pending`/`claimed` continue per the state machine — they are not cancelled by config flip. Operator may run `force-fail` to drain. `accepted`/`spawned` rows always finish on the worker. |

The general principle: **never let a forwarded intake make the user
wait longer than they would have without forwarding, and never run
the same user message twice.**

## Observability

Reuse `emit_event` (introduced in #1995 for placeholder POST
failures). New event types:

- `intake_routing_decision`
  - phase: `local` | `forward_observe` | `forward_enforce` | `fallback`
  - agent_id, channel_id, target_instance_id (forward only)
  - reason: `kill_switch` | `single_node` | `pending_uploads` | `affinity_match` | `stale_affinity_fallback` | `agent_preference_match` | `no_label_match`

- `intake_forward_claimed` — worker → leader visibility
- `intake_forward_accepted` — worker has validated and started
- `intake_forward_spawned` — turn_bridge attached
- `intake_forward_completed` — terminal status (done | failed)
- `intake_forward_recovery` — leader sweep took over a stale row
  (with reason: `pre_claim_timeout` | `stale_claim` | `cwd_missing` |
  `worker_failed`)

Daily aggregation gives operators a per-agent latency / fallback
rate that drives Phase 7 promotion decisions.

Discord operator alerts (24h dedupe, mirror of #1994's
`enqueue_outbox_pg_with_ttl`) fire on:

- A row reaches `failed_post_accept` (terminal; user-visible state
  changed; manual operator decision needed).
- A row reaches `failed_pre_accept` after exhausting
  `max_attempts_per_message` via transition 10 (the audit chain has
  no more retry budget).
- A row stays in `accepted` past `accepted_unspawned_sla_secs`
  (default 120s) without `spawned_at` — round-3 P1 #3 fast-SLA
  detector. Auto-retry forbidden, so the alert IS the recovery
  signal.
- A row stays in `spawned` past 24h without `done` (worker-side
  panic suspected).

## Implementation phases

Each phase ships as its own PR and merges independently. The leader
hook is **gated behind `cluster.intake_routing.mode`** (default
`observe`) so Phase 4 emits decisions but does not actually forward
until Phase 5 flips to `enforce`.

### Phase 1 — Schema migration (lowest risk, no behaviour change)
- New migration `0070_intake_node_routing.sql`:
  - `ALTER TABLE agents ADD COLUMN preferred_intake_node_labels JSONB DEFAULT '[]'`
  - `CREATE TABLE intake_outbox (...)` with all columns from §B
    (target_instance_id, attempt_no/parent_outbox_id, state machine
    columns, CHECK constraints, four indices).
  - `CREATE OR REPLACE FUNCTION intake_outbox_touch_updated_at` and
    its `BEFORE UPDATE` trigger.
- Migration is idempotent (`IF NOT EXISTS` everywhere).
- Tests: migration apply + rollback on a fresh PG; baseline test
  that empty column doesn't disturb existing agent reads;
  CHECK-constraint rejection of unknown status values.

#### Migration safety preflight (round-4 prompt #6)
Phase 1 ships against a fresh table — no rows exist, so the partial
unique index, attempt_no defaults, etc. cannot conflict. The
migration is therefore safe to apply unconditionally.

For any *future* change that adds new constraints to a populated
`intake_outbox`, the migration must include a preflight that:
1. Confirms no channel has more than one row in the OPEN set
   (would violate the partial unique index).
2. Confirms no `(channel_id, user_msg_id)` family has duplicate
   `attempt_no` values (would violate the 3-tuple uniqueness).
3. Confirms no rows have `attempt_no IS NULL` or
   `attempt_no < 1` (would violate the CHECK constraint).

If any preflight fails, the migration aborts and an operator
inspects via the future `outbox-status` CLI (Phase 5) to clean up.

#### Retention policy (deferred; documented for future)
This design deliberately does NOT delete `done`/`failed_*` rows.
Audit chain via `parent_outbox_id` references prior attempts; row
deletion fragments those chains.

When a retention job is added (separate PR, post-pilot):
- Honor the audit chain: deleting a parent must either delete the
  whole family or use a tombstone row keyed by
  `(channel_id, user_msg_id)` to preserve the attempt counter.
- `parent_outbox_id ... ON DELETE SET NULL` is intentional so a
  partial deletion does not break referential integrity, but it
  means `max_attempts_per_message` becomes unenforceable on
  families where some ancestors were retained out.
- Recommended: retain rows for 30 days OR until 5 attempts beyond
  the latest `done`, whichever is longer. Tune after pilot
  observability data.

### Phase 2-pre — Extract REST-safe intake core (codex blocker 1, scoped per round-2 P1 #4)

`handle_text_message` is **2,379 lines** (`message_handler.rs:1795-4173`)
and ownership-overloaded: auto-start, dispatch thread reuse, session
cwd updates, queue handoff, placeholder ownership, watchdogs,
inflight persistence, provider spawn, bridge wiring. It also passes
`LiveDiscordTurnContext { ctx, token, request_owner }` into
`DiscordGateway` (`message_handler.rs:4123`), and
`gateway.rs:495` recursively calls back into `handle_text_message`
for queued-turn dispatch with that same live context.

**This is not a single-PR refactor.** Budget as 3 sequential PRs:

#### Phase 2-pre.1 — Dependency extraction
- Introduce `IntakeDeps { http: Arc<Http>, shared: Arc<SharedData>, provider, token, pg_pool, instance_id }`.
- Convert `handle_text_message` parameters that currently come from
  `serenity::Context` into explicit `IntakeDeps` fields, taken
  initially from `LiveDiscordTurnContext`.
- No behaviour change. Pass-through refactor.

#### Phase 2-pre.2 — REST-only gateway / queue semantics
- Replace every `ctx: &serenity::Context` use inside the body with
  `IntakeDeps`-derived equivalents. Each replacement classified as:
  - **REST-replaceable** (e.g. `ctx.http`-driven attachment download,
    user lookup): swap to `deps.http`.
  - **Gateway-only** (e.g. live presence): list with file:line and
    decide per call site whether to (a) split into a leader-side
    companion call invoked over PG, or (b) declare the dependent
    feature unsupported on forwarded intakes (with proof no current
    agent uses it).
- Update the recursive callback at `gateway.rs:495` to take
  `IntakeDeps` instead of `LiveDiscordTurnContext`.
- Still no behaviour change for the leader; tests must pass.

#### Phase 2-pre.3 — Worker-callable surface
- Extract `execute_intake_turn_core(deps: IntakeDeps, payload: IntakePayload) -> Result<()>`.
- Leader's `handle_text_message` becomes a wrapper that builds
  `IntakeDeps` from `serenity::Context` and `IntakePayload` from
  the message + reaction state, then calls the core.
- Worker's later `handle_forwarded_intake` (Phase 3) builds
  `IntakeDeps` from its local HTTP client and `IntakePayload` from
  the deserialized outbox row.
- **Still no Phase 4 routing hook**; Phase 2-pre.3 only provides
  the callable surface.

Each sub-phase merges independently; tests for the leader intake
path must pass after each. Phase 2 (routing primitives) does not
depend on 2-pre.3 specifically — it can ship in parallel with
2-pre.1/2 since it touches different files.

### Phase 2 — Routing primitives (no behaviour change)
- `src/services/cluster/intake_routing.rs` (new) — pure functions:
  `resolve_intake_target(...) -> IntakeTarget`.
- `src/db/intake_outbox/` (new) — INSERT (with pg_advisory_xact_lock),
  claim (FOR UPDATE SKIP LOCKED, instance-locked), state transitions
  (`pending → claimed → accepted → spawned → terminal`), sweep
  (pre-claim takeover, stale-claim recovery, stale-spawned alert).
- Config: `ClusterIntakeRoutingConfig` in `src/config.rs`.
  - `enabled: true`
  - `mode: observe`
  - `forward_pre_claim_timeout_secs: 12`
  - `stale_claim_recovery_secs: 60`
  - `accepted_unspawned_sla_secs: 120`
  - `max_retries: 3`
  - `max_attempts_per_message: 5`
- Tests: unit on `resolve_intake_target` covering all 7 rules + 6
  branches of the state machine; PG tests on outbox claim,
  state transitions, advisory lock contention.

### Phase 3 — Worker polling loop
- `src/services/intake_outbox/worker_loop.rs` — poll-claim-handle
  pattern matching `dispatch_outbox_loop`. Adaptive 500ms→5s backoff.
- Worker invokes `handle_forwarded_intake()` which builds
  `IntakeDeps` from its local HTTP client and calls
  `execute_intake_turn_core` from Phase 2-pre.
- Leader also runs the loop (for pre-claim takeover and stale
  recovery sweep).
- Tests: PG-backed test stages a row, runs one poll iteration,
  verifies state transitions and that a mock REST handler runs.

### Phase 4 — Leader hook in `observe` mode
- Insert `resolve_intake_target` call into the leader-side
  `handle_text_message` after session-load, before mailbox enqueue.
- In `observe` mode, emit `intake_routing_decision` event and continue
  local handling. **No outbox row is INSERTed; no behaviour change
  for the user.**
- Tests: integration test using existing intake test harness verifies
  the decision events are emitted without altering the local turn
  spawn behaviour.
- Operators run for 1 week minimum to confirm decision distribution
  matches expectations before any flip.

### Phase 5 — Leader hook in `enforce` mode + ops surface
- Flip `cluster.intake_routing.mode` to `enforce` after Phase 4
  observation. (Config-only change; no code change.)
- Add operator CLI (`agentdesk cluster intake_routing <subcmd>`):
  - `status` — current mode, per-agent labels, eligible workers,
    recent decision distribution, in-flight outbox row counts per
    state.
  - `outbox-status [--channel <id>] [--state <s>]` — list outbox
    rows; pretty-print state machine, retry counts, age in each
    state. Round-2 P1 #6.
  - `clear-session-pin <channel_id>` — manual recovery for stale
    affinity (rule 5).
  - `force-fail <row_id>` — write `failed_post_accept` with audit
    note. Round-2 P1 #6. Allowed in any non-terminal state; refuses
    if row is already `done`.
  - `retry-local <row_id>` — operates on `failed_pre_accept` rows
    only (no user-visible state change occurred). Runs SQL
    transition 10 in §B-bis: INSERTs a fresh row with
    `attempt_no = MAX + 1`, `parent_outbox_id = $row_id`,
    `target_instance_id = local_leader`, `status = 'pending'`.
    Refuses if the source row is `pending`/`claimed` (use
    `force-fail` first), `accepted`/`spawned` (use `retry-as-new`),
    or already `done`. Refuses if `attempt_no >=
    max_attempts_per_message`.
  - `retry-as-new <row_id>` — operator-confirmed: works on
    `accepted/spawned/failed_post_accept` rows. Runs SQL
    transition 12 in §B-bis as a single transaction:
    1. Force-fails the source row to `failed_post_accept` (no-op
       if already terminal).
    2. INSERTs a fresh row with `attempt_no = family_max + 1`
       linked via `parent_outbox_id`, `target_instance_id =
       local_leader`, `status='pending'`.
    Within the transaction's unique-index check, the INSERT sees
    the source row's prior UPDATE (out of the OPEN partial index
    predicate) before evaluating the new row, so the constraint
    never observes both rows OPEN at once. Concurrent readers
    outside the transaction see the source as OPEN until commit
    but cannot see the new pending row until commit either, so
    they too never observe both simultaneously.
    Confirm prompt + audit log entry. Refuses if
    `family_max + 1 > max_attempts_per_message`.
- `migrate-session-to <node>` is **NOT** in v1 (would need careful
  tmux handoff). Documented as future work; for now operators must
  terminate the foreign session via existing tools and let the next
  intake re-route.
- Add 📩 reaction on user message when forward executes.
- Document the kill switch, CLI, and rollback procedure in
  `docs/runtime/cluster-routing.md`.

#### Rollback procedure (round-2 P1 #7)
For incident response when `enforce` mode causes problems:

1. Operator flips `cluster.intake_routing.mode = observe` in
   `agentdesk.yaml` and reloads config.
2. New intakes go `Local` immediately (leader snapshots config
   inside the short insert transaction).
3. **In-flight rows are NOT auto-cancelled.** State machine
   continues:
   - `pending`/`claimed` rows continue per the normal sweep flow.
     If a worker happens to pick up a `pending` row after the
     rollback, that's fine — it was already a forward decision
     made under the old config and the worker is honoring it.
   - `accepted`/`spawned` rows always finish on the worker.
4. If the operator wants to drain the in-flight queue immediately,
   the procedure is **two-step per row** (round-5 P2 #3 fix —
   `retry-local` refuses `pending`/`claimed` rows by spec):
   a. `agentdesk cluster intake_routing outbox-status --state pending`
      lists pending rows.
   b. For each row to drain: `force-fail <row_id>` (writes
      `failed_post_accept` + audit) followed by `retry-as-new <row_id>`
      (transition 12 — fresh attempt on leader). Skip rows that
      have already started executing on a worker (`accepted` /
      `spawned`); they are left to finish.
5. The flip is auditable via `intake_routing_decision` events
   showing `mode: observe` from the flip moment.

### Phase 6 — Pilot rollout
- Enable for ch-td only:
  `UPDATE agents SET preferred_intake_node_labels = '["unreal","heavy-cli"]' WHERE id = 'ch-td';`
- Monitor for 1 week:
  - Outbox failure rate (alert)
  - Pre-claim takeover rate
  - Accepted-stuck rate
  - End-to-end latency comparison vs leader-only baseline
- Roll out to other heavy agents (TAD, AD) only after ch-td baseline
  is stable.

## Test strategy summary

| Layer | Examples |
|---|---|
| Unit (Rust) | `resolve_intake_target` decision tree (7 rules); `execute_intake_turn_core` REST-safe path under faked deps |
| PG (Rust) | outbox INSERT with advisory lock contention; instance-locked claim; pre-claim sweep; stale-claim recovery; stale-spawned alert; state machine forward / illegal transitions |
| Integration (Rust) | leader observe→enforce flip; observe emits decisions but no rows; enforce inserts row, worker poll picks up, mock Discord HTTP, status reaches `done`; pre-claim timeout takeover; cwd missing on worker → leader fallback |
| Operational | `/cluster intake_routing status` matches PG truth |
| Regression | All existing intake tests pass after Phase 2-pre refactor (leader behaviour unchanged) |

Codex high-effort review at every phase boundary.

## Rollout / kill switch

- `cluster.intake_routing.enabled = true` ships as default; `mode =
  observe` ships as default. Operators flip `mode` to `enforce` only
  after Phase 4 observation looks healthy.
- Per-agent opt-in: `agents.preferred_intake_node_labels = '[]'` (the
  default) means current behaviour. We never change behaviour for an
  agent without an explicit operator UPDATE.
- Single-PG-primary self-check at startup: if cluster config lists
  multiple PG primaries, log error and refuse to start with
  intake_routing enabled.

## Open questions for round-5 review (요부장 + codex)

1. **Phase 2-pre.2 audit outcome**: if the `serenity::Context` audit
   finds gateway-only call sites we cannot REST-replace, the
   leader-side companion call route is fragile (worker → PG → leader
   side-effect → response → worker). Alternative: declare those
   features unsupported on forwarded intakes and require operators
   to opt-out the agent from forwarding when they need that feature.
   Decision needed before Phase 2-pre.2 lands.

2. **`retry-as-new` idempotency assertion**: this CLI command lets
   operators re-run a turn that may have already produced
   user-visible state. We rely on the operator to assert
   idempotency. Should we instead refuse the command and require
   manual SQL? Tradeoff: easier rollback vs operator footgun.

3. **`stale_claim_recovery_secs = 60s`** vs `forward_pre_claim_timeout_secs = 12s`:
   the gap exists because cwd validation can take a few seconds on
   a cold worker. Is 60s right, or should it be tighter (30s) given
   our workers are usually warm?

4. **Stale-spawned 24h threshold**: arbitrary. Should it be tied to
   the existing `dispatch_outbox` stale threshold (300s for stolen
   claim) or to the longest-running existing turn we have data for?
   (24h seems generous; some Unreal builds run >1h.)

5. **Phase 6 pilot**: monitoring period 1 week. Operators may want
   shorter (3 days) or longer (2 weeks). Set expectation now.

6. **`retry-local` after a failed accept**: round-2 P1 #6 says this
   command should be guarded to `pre_accept` rows only. v3 implements
   that, but operator may want to relax for `failed_post_accept`
   (uses `retry-as-new` instead with confirmation). Acceptable?

7. **`max_attempts_per_message = 5`**: arbitrary cap. Should it be
   tunable per-agent (some agents may always need extra retries
   because of transient network issues to a remote cluster), or is
   a single cluster-wide cap fine?

8. **`accepted_unspawned_sla_secs = 120s`**: based on guess for
   "Unreal cold provider startup." Is this realistic, or should we
   tune up after a few weeks of `observe` mode data?

9. **Same-channel different-user_msg_id during forwarded turn in
   flight**: round-4 fix returns `Local` for the late-arriving
   message (no silent drop). But this means the user's second
   message goes to the leader while the first is still running on
   the worker — they see two responses arriving from different
   nodes. Is that operationally fine, or do we need a queued-
   successor model that holds the second message until the first
   completes? Leaning toward "fine for v1; pilot data will tell."

These nine plus any new codex round-5 findings get answered before
Phase 1 lands.

---

## Appendix A: codex v1 review summary

Codex identified **3 architectural blockers** in v1:

1. **`handle_text_message` not worker-safe.** Worker has no
   `cached_serenity_ctx` because IDENTIFY is leader-only. v2
   addresses with explicit Phase 2-pre refactor extracting
   `execute_intake_turn_core(IntakeDeps, IntakePayload)`.

2. **Outbox claim by labels alone is wrong.** Could let any
   same-labeled worker claim. v2 introduces `target_instance_id` as
   the ownership key; `required_labels` becomes diagnostics.

3. **Timeout stealing can duplicate live turns.** v1 had a single
   timeout. v2 introduces a 5-state machine
   (pending → claimed → accepted → spawned → done|failed) and
   forbids steals after `accepted`. Operator alert + manual
   intervention only.

Plus several non-blocking but adopted recommendations:

- Single PG primary assumption made explicit (non-goal + startup
  self-check).
- Stale foreign-worker affinity → fall through to `Local` rather
  than auto re-pick (codex's "step 4 too aggressive" point).
- Drop `default_preferred_labels`. Per-agent opt-in only.
- `mode: observe | enforce` for dark-launch.
- Pre-claim timeout 5s → 12s.
- Pending uploads stay leader-routed in v1.
- Per-channel `pg_advisory_xact_lock` at INSERT.
- `OnDispatchCompleted` and similar dispatch-completion hooks fire
  normally on worker; only true gateway-context features get
  worker-side suppression after the Phase 2-pre audit.

---

## Appendix B: codex v2 review summary

Codex round-2 review identified **2 P0** + **5 P1** issues that v3
addresses:

### P0 issues

1. **Open-route invariant gap**: advisory lock alone does not prevent
   two same-channel messages from each seeing the other still in
   `pending` and routing to different targets. v3 adds DB-level
   partial UNIQUE INDEX `intake_outbox_one_open_route_per_channel`
   on `channel_id WHERE status IN ('pending','claimed','accepted','spawned')`.
   Concurrent inserts get unique-violation; loser re-evaluates.

2. **`failed` retry contradiction**: v2 said `accepted` was an
   absolute no-steal boundary, but the failure table allowed retry
   for cwd/token failures. v3 splits the terminal into:
   - `failed_pre_accept` — retryable (no user-visible side effects
     have happened yet).
   - `failed_post_accept` — terminal; operator-only via CLI.
   Plus `accepted` now strictly means "all retryable validation
   passed AND no user-visible side effect has fired yet."

### P1 issues

3. **No concrete transition SQL**: v3 adds a "Canonical
   state-transition SQL" section with 9 explicit
   compare-and-set queries, plus a CHECK constraint on `status`.

4. **Phase 2-pre under-scoped**: `handle_text_message` is 2,379
   lines and has a recursive callback from `gateway.rs:495` that
   hard-requires `LiveDiscordTurnContext`. v3 splits Phase 2-pre
   into 3 sub-phases (dependency extraction → REST-only refactor →
   worker-callable surface).

5. **Advisory lock scope**: v3 specifies pre-computation outside the
   transaction; lock window contains only the open-route re-read +
   eligibility revalidation + INSERT.

6. **CLI scope**: v3 adds `outbox-status`, `force-fail`,
   `retry-local` (pre-accept guarded), and `retry-as-new` (operator
   confirms idempotency).

7. **`observe → enforce` flip semantics**: v3 specifies leader
   snapshots config inside the short insert transaction; rollback
   procedure documented in Phase 5.

### Non-blocking findings adopted

- DB-level `updated_at` trigger so transitions can be observed by
  third-party tools.
- Schema preserves operator-runnable plain SQL (no SQL functions).
- Round-2 P2 (no distributed barrier needed for `observe→enforce`
  if workers don't interpret mode) accepted as-is.

---

## Appendix C: codex v3 review summary

Codex round-3 review identified **1 P0** + **3 P1** issues that v4
addresses:

### P0 issues

1. **Retry CLI commands cannot INSERT under v3 schema**: v3 had
   `UNIQUE (channel_id, user_msg_id)` but `retry-local`/`retry-as-new`
   said they INSERT a fresh row for the same user message. Mutually
   exclusive. v4 introduces `attempt_no INTEGER NOT NULL DEFAULT 1`
   and `parent_outbox_id BIGINT REFERENCES intake_outbox(id)`,
   replaces the uniqueness with `(channel_id, user_msg_id,
   attempt_no)`, and adds SQL transition 10 in §B-bis as the
   canonical "spawn fresh attempt from terminal failed_pre_accept."

### P1 issues

2. **`failed_pre_accept` retry SQL gap**: prose said leader sweep
   could re-pick `failed_pre_accept`, but transitions 7/8 only
   operated on `pending`/`claimed`. v4 adds transition 10 as the
   missing CAS link, gated by `max_attempts_per_message`.

3. **`accepted` SLA**: 24h alert was operationally too slow. v4
   adds `accepted_unspawned_sla_secs` (default 120s) + transition
   11 as a fast detector + alert. Auto-retry remains forbidden;
   the alert IS the recovery signal.

4. **Stale prose alignment**: the decision-flow diagram and the
   worker polling pseudocode still referenced `status='failed'`
   and the legacy state set without `pending`. v4 updates both to
   reference the canonical names + transition numbers and reads
   the OPEN set as `('pending','claimed','accepted','spawned')`
   consistently.

### Confirmed (no change needed)

- `done` rows do NOT block fresh same-channel forwards (partial
  unique index excludes terminal statuses). Correct semantics.
- Transitions 7/8 reset to `pending` correctly block fresh INSERT
  via the partial unique index. Correct semantics.
- `updated_at` BEFORE-UPDATE trigger is safe under MVCC + the
  CHECK constraint.
- The `dispatch_queued_turn` recursive callback at
  `gateway.rs:495` is the only major hard gateway dependency
  found by the audit; Phase 2-pre.2 must replace it. No
  typing/presence-style design breaker exists.

---

## Appendix D: codex v4 review summary

Codex round-4 review identified **2 P0** + **2 P1** issues that v5
addresses:

### P0 issues

1. **"Same target no-op" path drops distinct messages**: v4 had
   `if open.target_instance_id == instance_id => /* idempotent */`
   without checking `user_msg_id`. A different message arriving on
   the same channel while a forwarded turn was open would match
   this branch and be silently dropped. v5 narrows the no-op
   condition to the same `(channel_id, user_msg_id)` and returns
   `Local` for distinct user_msg_id (so the late message proceeds
   on the leader rather than disappearing).

2. **`max_attempts_per_message` cap unenforced**: v4 gated transition
   10 on `parent.attempt_no < cap`, but the inserted row used
   `family_max + 1`. With attempts 1..5 already in PG and parent =
   attempt 1, the gate passed (1 < 5) but the insert produced
   attempt_no = 6, exceeding the cap. v5 rewrites transition 10 to
   use a CTE that exposes both `parent_attempt_no` and `family_max`
   and gates on `family_max + 1 <= cap` PLUS
   `parent_attempt_no = family_max` (so retries from old/forked
   parents are rejected, keeping the audit chain linear).

### P1 issues

3. **`retry-as-new` lacked an SQL transition**: v4 said the CLI
   force-fails post-accept rows then runs transition 10, but
   transition 10 only accepts `failed_pre_accept`. v5 adds
   transition 12 — a single-transaction force-fail + insert that
   matches the same `family_max + 1` cap rule and atomically
   removes the source from the OPEN set before the new row
   appears.

4. **Stale post-accept SLA prose**: v4 still had `accepted/spawned
   > 24h` in the worker-side state machine and the failure-mode
   table, contradicting the new `accepted_unspawned_sla_secs`
   (120s) for `accepted` and the existing 24h for `spawned`. v5
   splits these explicitly: `accepted` uses the fast SLA via
   transition 11; `spawned` keeps 24h.

### Confirmed (no change needed)

- Concurrent `family_max + 1` race is fine; the loser catches the
  3-tuple unique-violation and re-evaluates.
- Different-user_msg_id rows for the same channel can have
  attempt_no = 1 simultaneously; the partial unique index excludes
  the terminal (failed_pre_accept) row, so they're independent
  families.
- Transition 11 is SELECT-only; alert dedupe metadata is stored
  outside `intake_outbox` (mirrors #1994's pattern).

### Deferred (documented; out of Phase 1 scope)

- Retention policy for old rows. Documented in Phase 1 §"Retention
  policy" — the audit chain via `parent_outbox_id` constrains
  cleanup design.
- Populated-table migration preflight. Documented in Phase 1
  §"Migration safety preflight" — Phase 1 itself is empty-table so
  no preflight is needed yet, but future schema changes must run
  the three checks listed there.

---

## Appendix E: codex v5 review summary (round 5 — small patches)

Codex round-5 review found **no P0 issues**. Two P1 + one P2 prose
and naming patches landed in this same v5 file (no v6 spin-up):

### P1 issues (now patched in v5)

1. **3-tuple unique constraint unnamed** — Rust handler must
   distinguish a 3-tuple violation from the partial unique index
   violation; both are SQLSTATE 23505. v5 names the constraint
   `intake_outbox_unique_message_attempt`. Rust matches by
   constraint name vs `intake_outbox_one_open_route_per_channel`
   index name.

2. **Stale prose claiming `failed_pre_accept` recovery via
   transitions 7/8** — actually transition 10. Patched at the
   failure-modes table, the "Failure terminals" subsection, and
   the transition 6a comment.

### P2 issue (now patched in v5)

3. **Rollback runbook used `retry-local` on `pending` rows** but
   CLI spec refuses that. Patched to two-step
   `force-fail` → `retry-as-new` (transition 12) per row.

### Confirmed (no further v5 action)

- Transition 12 atomicity is correct against the partial unique
  index — INSERT sees its own prior UPDATE within the transaction.
  Prose tightened to "within the transaction's unique-index check"
  per round-5 prompt #1.
- `parent_attempt_no = family_max` gate is the right v1 default;
  `--from-fork` would be a deliberate later schema change.
- PostgreSQL surfaces both unique violations as SQLSTATE 23505;
  the constraint/index name discriminates. The v5 explicit naming
  closes the gap.
- Same-channel different-`user_msg_id` returning `Local` is an
  acceptable v1 caveat (cross-node interleaving). Recorded as open
  question 9; pilot data will inform whether a queued-successor
  model is worth shipping post-v1.

---
*End of draft v5 (post-round-5 patches). Phase 1 PR can now ship
once the user (요부장) signs off on the 9 open questions.*
