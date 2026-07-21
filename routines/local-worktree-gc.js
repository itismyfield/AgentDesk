// Local agent worktree inventory routine (#4684)
//
// QuickJS routines intentionally have no filesystem/network bridge, so this
// routine cannot enumerate `.claude/worktrees` itself. Matching the reviewed
// daily-log-digest frame, it dispatches one fresh agent turn per KST day whose
// only job is to run the deterministic, READ-ONLY sibling helper
// (`routines/monitoring/local_worktree_inventory.js`) and return its JSON stdout
// verbatim. The helper performs zero destructive actions by construction; this
// routine likewise issues no cleanup — it only schedules the inventory.

const CHECKPOINT_VERSION = 1;

function dayKey(now) {
  const value = typeof now === "string" ? now : now.toISOString();
  const kst = new Date(new Date(value).getTime() + 9 * 60 * 60 * 1000);
  return kst.toISOString().slice(0, 10);
}

function loadCheckpoint(raw) {
  if (!raw || raw.version !== CHECKPOINT_VERSION) {
    return { version: CHECKPOINT_VERSION, last_dispatched_day: null };
  }
  return { version: CHECKPOINT_VERSION, last_dispatched_day: raw.last_dispatched_day || null };
}

function buildPrompt(day) {
  return [
    "# Local agent worktree inventory",
    "",
    `Inventory day: ${day}`,
    "",
    "Run the repository-bundled deterministic, read-only helper:",
    "```bash",
    'ROOT="${AGENTDESK_ROOT_DIR:-${ADK_REL:-$HOME/.adk/release}}"',
    'REPO="${AGENTDESK_REPO_DIR:-$ROOT/workspaces/agentdesk}"',
    'AGENTDESK_REPO_DIR="$REPO" node "$REPO/routines/monitoring/local_worktree_inventory.js"',
    "```",
    "",
    "Return the helper stdout (a single JSON report) verbatim as your final response, with no preface.",
    "This is a report-only inventory. Do NOT remove, prune, or modify any worktree, ref, branch, or file.",
    "The helper never deletes anything; you must not either. Uncommitted, locked, and unmerged worktrees",
    "are reported with disposition PRESERVE and must be left untouched.",
  ].join("\n");
}

agentdesk.routines.register({
  name: "Local agent worktree inventory",

  tick(ctx) {
    const day = dayKey(ctx.now);
    const checkpoint = loadCheckpoint(ctx.checkpoint);
    if (checkpoint.last_dispatched_day === day) {
      return {
        action: "complete",
        result: {
          status: "already_dispatched",
          summary: `local worktree inventory already dispatched for ${day}`,
        },
        checkpoint,
      };
    }

    checkpoint.last_dispatched_day = day;
    return {
      action: "agent",
      prompt: buildPrompt(day),
      lastResult: `local worktree inventory dispatched for ${day}`,
      checkpoint,
    };
  },
});
