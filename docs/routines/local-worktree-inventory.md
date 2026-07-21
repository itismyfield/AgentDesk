# Local agent worktree inventory (#4684)

Isolation `worktree` agent spawns leave `.claude/worktrees/agent-*` git worktrees
behind whenever the sub-agent commits (auto-clean only removes *unchanged*
worktrees). With no reclamation these accumulate — the issue observed 70GB / 134
worktrees, 52 orphaned. This routine provides the missing **visibility**: a
scheduled, report-only inventory that surfaces sizes, ages, and orphan
classifications so the leak is measurable and a later prune step has trustworthy
input.

## Two pieces

- `routines/monitoring/local_worktree_inventory.js` — the deterministic,
  **read-only** helper that does the real work: enumerates `agent-*` worktree
  directories (no symlink follow), cross-references `git worktree list
  --porcelain`, and per candidate reads mtime/age, apparent disk size (`du
  -sk`), dirty state (`git status --porcelain`), lock state, git registration,
  and merge state (`git merge-base --is-ancestor <HEAD> origin/main`). It emits
  one schema-validated JSON report.
- `routines/local-worktree-gc.js` — the QuickJS routine. QuickJS routines have
  no filesystem bridge, so (matching `daily-log-digest`) it only dispatches one
  fresh agent turn per KST day that runs the helper and returns its JSON stdout
  verbatim. A checkpoint day-key prevents duplicate same-day dispatch.

## Safety: report-only, safe by construction

The helper performs **zero destructive operations** — its entire source contains
no worktree remove/prune, branch delete, ref delete, `rm -rf`, `find -delete`, or
`fs` unlink/rm call (asserted by test). Every child process is a read-only git or
`du` subcommand. It never deletes anything and never claims deletion authority:
every reported entry sets `positive_ownership_proof: false` and the report sets
`destructive_actions: 0`.

Dispositions are advisory labels for a human or a future prune step, never an
instruction the helper acts on:

- **PRESERVE** — dirty (uncommitted work), locked, unknown/uninspectable,
  registered-but-missing directory, or clean-but-unmerged-and-recent. This is the
  #4595 lesson: a naive GC could have destroyed the exact uncommitted work this
  work was recovered from, so anything dirty or locked is preserved
  unconditionally (schema validation rejects any dirty/locked entry not marked
  PRESERVE).
- **AGED_ORPHAN_REVIEW** — clean but unmerged and older than the age threshold
  (default 7 days). Flagged for human review; any future removal must first back
  up the branch tip to `refs/archive/worktree-gc/<id>` (issue proposal #2). The
  helper does not remove it.
- **SAFE_MERGED_CANDIDATE** — clean AND merged into `origin/main` AND registered
  (issue proposal #1, the session-verified safe-reclaim condition). Surfaced as a
  candidate only; still report-only.

## Scheduling (attach once)

Like `daily-log-digest`, scheduling is a persisted routine row, not JS metadata.
Attach one row on the cluster leader targeting the operations channel:

```bash
REL_PORT="${AGENTDESK_REL_PORT:-8791}"
API="http://127.0.0.1:${REL_PORT}"

curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "local-worktree-gc.js",
  "name": "local-agent-worktree-inventory",
  "agent_id": "project-agentdesk",
  "execution_strategy": "fresh",
  "schedule": "0 9 * * *",
  "discord_thread_id": "YOUR_OPS_CHANNEL_OR_THREAD_ID",
  "timeout_secs": 900
}'
```

The cron schedule (09:00 `routines.default_timezone`, Asia/Seoul by default) is
persisted in the routines table and claimed through the existing routine lease.

## Configuration

The helper resolves the repository from `AGENTDESK_REPO_DIR` (the routine prompt
sets it to `$ROOT/workspaces/agentdesk`). The age threshold defaults to 7 days
and the integration ref to `origin/main`; both are parameters of `runInventory`.
Running `node routines/monitoring/local_worktree_inventory.js` directly prints the
current inventory report for ad-hoc inspection.
