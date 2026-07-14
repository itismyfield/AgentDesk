# Daily dcserver log digest routine

Issue #4263 adds `monitoring/daily-log-digest.js` to the existing PostgreSQL-backed routine
worker. It is an agent-backed monitoring routine because the QuickJS routine sandbox intentionally
has no filesystem or network bridge. The routine dispatches one fresh agent turn per day; that turn
runs the deterministic sibling helper, and the existing routine Discord logger posts the final
summary to the configured routine channel/thread.

## Attach once

Routines use `routines.default_timezone` (Asia/Seoul by default). Attach one row on the cluster
leader and target the operations channel with `discord_thread_id`:

```bash
REL_PORT="${AGENTDESK_REL_PORT:-8791}"
API="http://127.0.0.1:${REL_PORT}"

curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "monitoring/daily-log-digest.js",
  "name": "daily-dcserver-log-digest",
  "agent_id": "project-agentdesk",
  "execution_strategy": "fresh",
  "schedule": "10 9 * * *",
  "discord_thread_id": "YOUR_OPS_CHANNEL_OR_THREAD_ID",
  "timeout_secs": 900
}'
```

The cron schedule is persisted in the normal routines table and claimed through the existing
routine lease. A checkpoint day key is a second guard against a manual or duplicate same-day run,
so the routine dispatches at most one digest agent turn per KST day.

## Inputs and configuration

`routines/monitoring/daily_log_digest.py` resolves the runtime root in the same order used by
release tooling: `AGENTDESK_ROOT_DIR`, then `ADK_REL`, then `$HOME/.adk/release`. It reads:

- `logs/dcserver.stdout.log` and its numbered rotations (the internal tracing writer);
- `logs/dcserver.launchd.stderr.log` (the path emitted by AgentDesk's launchd/systemd setup).

Timestamped lines are limited to the preceding 24 hours. Undated launchd stderr uses a persistent
device/inode/byte-offset checkpoint under `runtime/daily-log-digest/`, so a range is counted at
most once; rotation or truncation starts a new range. The first observation establishes the
watermark, and a stale file outside the window is baselined without counting its old contents.

Optional environment settings, normally placed in the deployment's preserved
`config/launchd.env`, are:

- `AGENTDESK_LOG_DIGEST_THRESHOLD`: positive daily count threshold, default `50`; a pattern must
  be strictly greater than the threshold. Invalid values warn and fall back to `50`.
- `AGENTDESK_LOG_DIGEST_REPO`: GitHub repository for open-issue dedup, default
  `itismyfield/AgentDesk`.
- `AGENTDESK_LOG_DIGEST_CREATE_ISSUE`: default `off`. Only the literal `confirmed`, set by a human
  after reviewing pending drafts, allows the approval path to inspect per-draft markers.

## Normalization, dedup, and drafts

`log_digest_issue_drafts.py` is the reusable API for this routine and #4265. Its public pipeline is:

```python
patterns = aggregate_normalized_signatures(lines)
decisions = decide_issue_drafts(patterns, open_issues, threshold=50)
drafts = write_pending_drafts(
    [decision.draft for decision in decisions if decision.draft],
    pending_dir,
)
post = maybe_post_approved_drafts(drafts, approval_mode, create_issue)
```

Normalization removes ANSI decoration and timestamps, canonicalizes ERROR/WARN, and replaces UUIDs,
hashes, known embedded/dynamic IDs, request tokens, and most bare numbers with placeholders. HTTP
status codes and explicitly labelled ports remain distinct; unlabeled semantic numbers may still
collapse. Counts are grouped by severity plus normalized signature. Threshold crossings are
compared against the bounded issue title and first non-empty body line using at least three shared
tokens and symmetric Jaccard similarity; direct containment is accepted only for signature-like
candidates. This can miss a duplicate described only deep in a long body, but avoids suppressing a
new short signature merely mentioned in an unrelated epic. If the open-issue query is unavailable,
invalid, or reaches the 1,000-result cap, draft generation fails closed to avoid duplicate pending
work from an incomplete dedup set.

Pending Markdown files use a stable signature hash and live at:

```text
${AGENTDESK_ROOT_DIR:-$HOME/.adk/release}/runtime/pending-issue-drafts/daily-log-digest/
```

The normal/default path never creates an issue. Approval is deliberately two-step: after reviewing
one pending file, create its adjacent marker (for example
`error-0123456789abcdef.md.approved`) and set
`AGENTDESK_LOG_DIGEST_CREATE_ISSUE=confirmed`. Both the literal environment gate and that specific
draft's `.approved` marker must exist before the injected issue-creation callback can run. A future
caller using the shared helper inherits both checks.
