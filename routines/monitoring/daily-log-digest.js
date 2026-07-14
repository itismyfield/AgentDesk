// Daily dcserver Log Digest (#4263)
//
// QuickJS routines intentionally have no filesystem/network bridge. Match the
// existing monitoring frame by dispatching one fresh agent turn; the agent runs
// the deterministic sibling helper and its final response is posted through the
// routine Discord logger.

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
  return {
    version: CHECKPOINT_VERSION,
    last_dispatched_day: raw.last_dispatched_day || null,
  };
}

function buildPrompt(day) {
  return [
    "# Daily dcserver log digest",
    "",
    `Digest day: ${day}`,
    "",
    "Run the repository-bundled deterministic helper:",
    "```bash",
    'ROOT="${AGENTDESK_ROOT_DIR:-${ADK_REL:-$HOME/.adk/release}}"',
    'python3 "$ROOT/routines/monitoring/daily_log_digest.py"',
    "```",
    "",
    "Return the helper stdout verbatim as your final response, with no preface or follow-up.",
    "Do not call `gh issue create` directly. The helper writes pending drafts and its shared",
    "gate permits posting only when a human has explicitly set",
    "`AGENTDESK_LOG_DIGEST_CREATE_ISSUE=confirmed` and marked that specific draft",
    "with an adjacent `.approved` file; the default is `off`.",
  ].join("\n");
}

agentdesk.routines.register({
  name: "Daily dcserver Log Digest",

  tick(ctx) {
    const day = dayKey(ctx.now);
    const checkpoint = loadCheckpoint(ctx.checkpoint);
    if (checkpoint.last_dispatched_day === day) {
      return {
        action: "complete",
        result: {
          status: "already_dispatched",
          summary: `daily log digest already dispatched for ${day}`,
        },
        checkpoint,
      };
    }

    checkpoint.last_dispatched_day = day;
    return {
      action: "agent",
      prompt: buildPrompt(day),
      lastResult: `daily log digest dispatched for ${day}`,
      checkpoint,
    };
  },
});
