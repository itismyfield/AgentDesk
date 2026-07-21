// Local worktree inventory (#4684)
//
// The routine is intentionally report-only. Age, lock state, and git registration
// are observations, not positive proof that AgentDesk owns a worktree lifecycle.

const REPO = "${AGENTDESK_REPO_DIR:-$HOME/.adk/release/workspaces/agentdesk}";

function buildPrompt() {
  return [
    "# Local agent worktree inventory",
    "",
    `Canonical repository: ${REPO}`,
    "",
    "Inventory only `.claude/worktrees/agent-*` entries associated with the canonical repository.",
    "This run is report-only: perform zero destructive actions and do not modify worktrees, refs, files, or routine state outside the returned checkpoint.",
    "Age and an unlocked state are never lifecycle ownership proof. A clean tree, merged commit, stale mtime, missing directory, or missing git registration is also not ownership proof.",
    "Unknown, active, locked, dirty, unmerged, and registered worktrees must all be preserved.",
    "",
    "## Read-only collection",
    "1. Read `git worktree list --porcelain` from the canonical repository and retain path, HEAD/ref, locked state, and registration state.",
    "2. Enumerate matching directories without following symlinks. For each candidate, read directory mtime and calculate age_seconds relative to the current run time.",
    "3. Include registered matching paths even when their directory is missing, and matching directories even when they are not registered.",
    "4. Use read-only git inspection to classify ref/head presence and worktree state. Inspection failure produces `unknown`; it never authorizes cleanup.",
    "",
    "## Required structured report",
    "Return one JSON object and no prose. Use this shape:",
    "```json",
    '{"mode":"report_only","destructive_actions":0,"candidate_count":0,"candidates":[{"path":"/absolute/path","age_seconds":null,"locked":"unknown","registered_worktree":"unknown","head":null,"ref":null,"worktree_state":"unknown","positive_ownership_proof":false,"ownership_proof_absence_reason":"No lifecycle owner record or owner-issued cleanup authorization is available."}],"inspection_errors":[]}',
    "```",
    "`locked` and `registered_worktree` are true, false, or `unknown`. `worktree_state` is `clean`, `dirty`, `missing`, or `unknown`.",
    "Every candidate must set `positive_ownership_proof` to false and give a concrete absence reason. If no candidates exist, return an empty candidates array.",
    "Never infer ownership from path naming, age, lock state, registration, ref, HEAD ancestry, cleanliness, or process visibility.",
  ].join("\n");
}

agentdesk.routines.register({
  name: "local-worktree-gc",
  metadata: {
    owner: "project-agentdesk",
    description: "Report-only inventory of local Claude agent worktrees; no cleanup actions",
    schedule_intent: "0 9 * * * Asia/Seoul",
    safety_mode: "report_only",
  },
  tick(ctx) {
    return {
      action: "agent",
      prompt: buildPrompt(),
      checkpoint: {
        version: 2,
        last_tick_at: ctx.now,
        runs: (ctx.checkpoint?.runs || 0) + 1,
      },
      lastResult: "local worktree report-only inventory dispatched",
    };
  },
});
