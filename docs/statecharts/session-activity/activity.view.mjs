export const title = "Claude activity: source evidence, recovery and stop boundary";
export const state = ({r}) => {
  if (r.badPhysical) return "Physical completion raced Escape";
  if (r.badEvidence) return "Escape without activity evidence";
  if (r.badKill) return "Cleanup killed active work";
  if (r.esc) return "Escape delivered from activity evidence";
  if (r.closed && r.open) return "Closed tool reopened by late start";
  return `${r.actual === "idle" ? "Provider idle" : "Provider busy"}: ${r.observed}${r.observed === "unknown" ? (r.expired ? " after lease expiry" : r.didExpire ? " after one recovery cycle" : "") : ""}, ${r.watcher ? "watcher attached" : "watcher absent"}`;
};
export const step = (action, from) => ({
  ObserveActivity: from.r.mode === "current" ? "pane busy signal unavailable" : "read current source activity", ProgressArrival: "new source progress renews lease", Timeout: "expire hints and recheck",
  WatcherReacquire: "recover watcher", Restart: "restart observer",
  StopRead: "check stop authority", StopSend: "send stop if authorized",
  ProviderFinishesBetweenReadAndSend: "provider finishes after check",
  ToolFinishes: "foreground tool finishes", BackgroundFinishes: "subagent finishes",
  PreToolUse: "tool start hook arrives", PostToolUse: "tool completion hook arrives",
  ProbeChildPresence: "observe a live pane child", ObserveExactChildTerminal: "observe exact child terminal",
  GenerationFinishes: "generation ends before terminal arrives", TerminalArrives: "root terminal becomes observable", SubagentStop: "subagent stop arrives",
  IdleCleanup: "attempt idle cleanup", OldIncarnationHook: "ignore old incarnation hook",
  UserPromptSubmit: "submission hook arrives", LoseSubmit: "submission hook is lost",
  LosePreToolUse: "tool start hook is lost", LosePostToolUse: "tool completion hook is lost",
  StopHook: "turn stop hook arrives"
}[action] || action);
