# Durable campaign ledger

Campaigns use the existing server and its configured canonical PostgreSQL pool.
All nodes and dependency edges are one atomic revision. There is no local JSON
copy, session-bound owner, or restart replay, and nothing runs unless a campaign
opts into the auto-queue handoff below. After a
clear, compaction, quota stop, provider switch or server restart, load the same
campaign ID before doing more work. A `running` node is a saved checkpoint, not
proof that its old process remains alive: inspect its session and evidence before
resuming it. Session IDs are historical references and are not foreign keys to
ephemeral runtime sessions.

The `0121_campaigns.sql` migration creates the ledger and its revision history
through the normal server migration workflow. History is not append-only: each
write keeps only the newest 10 revisions of a campaign and destroys the rest, so
anything that must survive belongs in the current document or an external
artifact, never in an older snapshot. Every member of a cluster
must use its shared canonical PostgreSQL configuration. Database backups remain
the durability boundary; no dashboard cache is authoritative.

## API

All routes are under `/api` and use the same protected admin middleware as
`/departments` and `/settings`, including the configured server Bearer token.

| Method | Route | Result |
| --- | --- | --- |
| GET | `/campaigns?limit=100&offset=0` | `{campaigns: Campaign[], live, issue_drift, limit, offset}`; latest updated first, limit 1–500 |
| POST | `/campaigns` | HTTP 201 `{campaign}`; optional client ID, otherwise UUID; existing ID returns 409 |
| GET | `/campaigns/{id}` | `{campaign, live, issue_drift}`; missing ID returns 404 |
| PUT | `/campaigns/{id}` | `{campaign}`; requires `expected_revision`, replaces complete aggregate |
| GET | `/campaigns/{id}/history` | `{revisions: Campaign[]}`; the retained newest 10 revisions, descending; older ones are deleted, not archived |

`live` is the execution projection the ledger itself does not hold. For each node
whose `issue_url` is a GitHub issue that has a kanban card, it reports `card_id`,
`card_status`, the card's newest `dispatch_id`/`dispatch_type`/`dispatch_status`, the
`session_status` and `session_seen_at` heartbeat of the session holding that
dispatch, and the newest auto-queue `queue_status` (list: campaign id → node id →
status; single read: node id → status). A dispatch row can stay `dispatched` after
its session is gone, so only `running` claims work is happening now: any dispatch
on the card is `dispatched` and has a `turn_active` or `awaiting_bg` session with a
heartbeat inside the stale-turn grace window. The nullable `working_dispatch_id`,
`working_dispatch_type`, `working_session_id` (database ID as text),
`working_session_status`, and `working_session_seen_at` identify a matching pair,
preferring the freshest heartbeat. Newer pending or completed sidecars do not
hide older running work. The dashboard uses this pair for running labels and
session details; the existing dispatch/session fields retain their latest-record
meaning. `live` is computed on every read and never written back, so it can
disagree with a node's saved `status`.

`issue_drift` lists completed nodes whose linked issue github-sync last saw open
(list: campaign id → entries; single read: entries), each with `node_id`,
`issue_url`, `node_status`, `reason` and `observed_at`. `open_issue` means the node
was completed while the issue stayed open (for example a bundle PR that only
mentions `(#N)`); `changed_after_auto_close` means the issue reopened after an
automatic close but a person had changed the node, so it was left alone. The
server never closes the issue; that is a decision for a person.

Campaign fields: `id`, `title`, `description`, `status`, `round`, `revision`,
`auto_queue`, `nodes`, `created_at`, `updated_at`. `auto_queue` defaults to false;
a POST or PUT that omits it keeps the stored value, so older writers cannot turn
it off by accident. Status is `planned`, `active`, `paused`,
`completed`, or `cancelled`. Round is a positive integer; revision starts at 1.

Node fields: `id`, `title`, `status`, `stage`, `group`, `round`, `assignee`, `session_id`,
`provider`, `dependencies`, `issue_url`, `pr_url`, `head_sha`, `evidence`,
`next_action`, `blocker`, `summary`, `benefit`, `post_merge_hold`, `details`, `acceptance`, `findings`,
`evidence_records`, `updated_at`, and the server-owned `auto_closed_from` and `issue_drift`. Status is `pending`, `running`, `blocked`, `completed`, `failed`, or
`skipped`. Stage is a separate free-text workflow label. Optional scalar fields
may be null; arrays default to empty. `details` defaults to an empty string.
`group` is an optional, caller-supplied organizational label independent of
stage and status. Surrounding whitespace is trimmed; blank, null or omitted
values become null (unclassified). Existing documents without this field stay
unclassified; no group is inferred from titles, stages, or statuses. Group
changes use the same revision CAS and durable history as other node changes.
`summary` (one-line plain-language gist) and `benefit` (expected effect once done)
are optional text for the dashboard's first screen; documents written before they
existed read them as null, and writers that omit them are unaffected.
`post_merge_hold` names work left after the merge (deploy, observation); while it
is set, a closed issue does not complete the node, and the dashboard shows the
node as on hold. Clear it when that work is done.
Evidence records contain a required `summary` and optional `command`, `result`,
`head_sha`, `recorded_at` (RFC3339), and `references` (string array).

Store enough acceptance criteria, findings, evidence summaries and next actions
in the ledger for another session to resume without reading a temporary file.
URLs and paths may supplement this information. They must not be its only copy.
Keep long raw logs in artifacts and preserve their conclusion and revision here.

PUT uses campaign-wide optimistic concurrency. Read, modify the needed fields,
then send the full document with its observed revision in `expected_revision`.
On 409, reload and reconcile; do not blindly resend a stale document with a new
revision. Nodes omitted from PUT are removed from the current DAG, but remain in
revision history. Both the current aggregate and history commit in one database
transaction. Node timestamps are generated by the server and preserved when the
node content has not changed. Caller-supplied read-only timestamps are ignored.
No API deletes campaign history.

Duplicate node IDs, duplicate/missing dependencies, self edges and cycles return
400 before writes. A completed campaign must contain nodes, all completed or
skipped. The API validates structure, not the truth of a claimed test result;
callers must verify their evidence before marking work complete.

## Auto-queue handoff

The campaign decides which nodes may start; auto-queue only runs them. A node is
ready when it is saved as `pending` and every dependency is saved as `completed`
or `skipped`, or is saved as `pending` or `running` while its issue card has
reached a terminal pipeline state (a dependency saved as `blocked` or `failed`
holds its dependents even when its card finished). A ready
node whose issue card is not finished and has no live auto-queue entry or
dispatch joins the auto-queue run of the card's assigned agent: the newest
active run for that repo and agent, in a new lane of its current phase, or a new
run labelled `campaign` with phase gates off (up to four lanes at once). Backlog
cards are moved to ready first, as `/api/queue/generate` does. Auto-queue's
minute tick dispatches the new entries. The handoff never writes the ledger: the
node's card and `live` show its progress.

## Issue sync

Each github-sync cycle (`github.sync_interval_minutes`) makes the nodes of active
campaigns follow the GitHub issue in their `issue_url`, whether or not the issue
has a card. Issues outside the synced issue list, and every issue whose node may
change, are read by number together with the closing event.

- Closed as completed → `completed`; closed as not planned or duplicate →
  `skipped`, with stage `완료: 이슈 닫힘 자동 반영` / `건너뜀: 이슈 닫힘 자동 반영`.
  An evidence record names what actually closed the issue (the closing pull
  request or commit, or the person who closed it by hand) and when. Nodes with
  `post_merge_hold`, and nodes already completed or skipped, are left alone.
- The previous status, stage and time go to `auto_closed_from`. Reopening the
  issue puts them back on the next sync, unless a person changed the node's status
  or stage after the automatic close (even back to the same values); then the node
  is kept and reported in `issue_drift` as `changed_after_auto_close`. A node a
  person reopened is not closed again for the same closing.
- `auto_closed_from` and `issue_drift` are owned by the server: PUT and POST
  ignore them and keep the stored values, and both are dropped when the node's
  `issue_url` changes.
- The server writes under the same row lock and revision fence as PUT, so a
  writer that read an older revision gets 409 and must reload. A node written
  after the issue was read waits for the next cycle. Paused, completed and other
  inactive campaigns are not touched, including reopen recovery.
- Only a read by number made after an `issue_drift` report clears it once the
  issue is closed; the synced issue list never does. When GitHub answers without
  one of the issues, its listed state can still raise `issue_drift`, but the
  node's status does not change.
- A failed GitHub read leaves those nodes unchanged for that cycle. The sync
  response carries `campaign_nodes` (`changed_nodes`, `deferred_nodes`, `errors`,
  `fetch_errors`) and `campaign_nodes_failed`; the periodic sync logs the failure
  and counts the cycle as failed. `deferred_nodes` alone is not a failure.

Ready nodes that cannot be queued are listed in `waiting` with a reason:
`no_issue_card`, `no_assigned_agent`, `previous_attempt_stopped` (its last queue
entry failed or was skipped or cancelled; reset the card or skip the node),
`card_not_ready` (the card is in another workflow step), `not_enqueueable`,
`run_paused` (that agent's queue is paused; the handoff never starts a second
run beside it), `queue_not_started` (a generated or pending queue for that agent
waits to be started; the node joins it once it runs), `campaign_changed` (the
campaign was saved again meanwhile), or `already_in_run`.

With `auto_queue: true` this happens after every save of an active campaign
(the response carries `handoff`, or `handoff_error` when the save succeeded but
the handoff failed) and whenever any card reaches a terminal state. Saving the
campaign again retries waiting nodes. Pausing the campaign stops further
handoffs; entries already queued keep running in auto-queue.

## Dashboard navigation

The first screen leads with running, then blocked, tasks as cards (running follows
the "Running now" rule below): gist (`summary`,
else the title), a seven-step bar (investigate → design → implement → review → fix →
merge → deploy check) inferred from the free-text `stage` by the stage keyword
written first (unmatched stages show their short text), `benefit`, and `blocker`
when present. Other statuses appear as counts that expand into a list. Commits,
evidence and findings stay in the task details.

Below it, the campaign view is a compact, collapsible list organized by the stored
`group`, intended for campaigns with hundreds of issues. Group labels describe
work areas; they are independent of workflow stage and status. Missing labels
remain ungrouped rather than being inferred from titles. Search and status/group
filters narrow the list without changing the canonical DAG or completion counts.

Select a task to inspect its session, review round, evidence and next action.
Connections first summarizes relationships between groups, then provides a focused
task dependency view rather than a miniature rendering of the entire campaign.
Aggregated group relationships can be cyclic even when the task DAG is acyclic.
The task view shows direct predecessors and successors across groups and filters,
with explicit omitted counts and a complete connection list for high fan-in/out.
The saved `running` status remains a checkpoint, not a live process-health signal;
the list row and task details show the `live` card, dispatch, session and queue state
beside it. "Running now" counts nodes saved as `running`, plus nodes not saved as
completed or skipped whose `live.running` is true.

## CLI usage

Use the existing `curl` and `jq` tools. Set `ADK_URL` to the canonical server
base URL and `ADK_AUTH_TOKEN` from the configured admin token without printing it.
The following request creates a stable campaign ID:

```sh
curl --fail-with-body --silent --show-error \
  -H "Authorization: Bearer $ADK_AUTH_TOKEN" \
  -H 'Content-Type: application/json' \
  "$ADK_URL/api/campaigns" --data-binary @- <<'JSON'
{
  "id": "session-continuity", "title": "Durable session continuity",
  "description": "Preserve reviewed progress across sessions and quota interruptions.",
  "status": "active", "round": 1,
  "nodes": [
    {
      "id": "implement", "title": "Persist canonical checkpoints",
      "status": "running", "stage": "implementation", "round": 1,
      "group": "Backend",
      "assignee": "backend", "session_id": "current-session", "provider": "codex",
      "dependencies": [], "issue_url": null, "pr_url": null,
      "details": "Write the DAG and revision history in one PostgreSQL transaction.",
      "acceptance": ["A stale concurrent writer receives HTTP 409"],
      "findings": [], "evidence": [], "evidence_records": [],
      "next_action": "Run the concurrent-write integration test and record its command, result and HEAD."
    },
    {
      "id": "review", "title": "Review durability evidence",
      "status": "pending", "stage": "review", "round": 1,
      "group": null,
      "dependencies": ["implement"], "next_action": "Review the implementation evidence after it is completed."
    }
  ]
}
JSON
```

Read the authoritative checkpoint at the start of every replacement session:

```sh
curl --fail-with-body --silent --show-error \
  -H "Authorization: Bearer $ADK_AUTH_TOKEN" \
  "$ADK_URL/api/campaigns/session-continuity" | jq '.campaign'
```

A read-modify-write operation that saves a quota pause with a fenced revision:

```sh
curl --fail-with-body --silent --show-error \
  -H "Authorization: Bearer $ADK_AUTH_TOKEN" \
  "$ADK_URL/api/campaigns/session-continuity" |
  jq '.campaign | .expected_revision = .revision | .status = "paused" |
      (.nodes[] | select(.id == "implement")) |=
      (.status = "blocked" | .blocker = "Provider quota exhausted" |
       .next_action = "Resume the recorded validation command with a new session; inspect evidence before rerunning side effects.")' |
  curl --fail-with-body --silent --show-error -X PUT \
    -H "Authorization: Bearer $ADK_AUTH_TOKEN" \
    -H 'Content-Type: application/json' \
    "$ADK_URL/api/campaigns/session-continuity" --data-binary @-
```

Completion, stage changes, review rounds and reassignment follow the same PUT
contract. The ledger does not automatically infer completion from transcript
length, session exit, a stopped process, or provider quota events.
