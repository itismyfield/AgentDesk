// Reader's words for SessionHost.tla: one Discord channel's managed session on its host.
export const title = "AgentDesk channel session on its host (tmux, Herdr, process) across restarts";

const hostName = { tmux: "tmux", herdr: "Herdr", process: "process" };

export const state = (v) => {
  const where = hostName[v.kind] ?? "no host";
  const down = v.hostSrv === "down" && v.kind !== "process" && v.kind !== "none" ? ", host server down" : "";
  // The model bounds what the environment does; a spent budget is why a step no longer leaves.
  const budgets = [
    v.inputs >= 3 && !v.want ? "inputs" : null,
    v.crashes >= 2 ? "restarts" : null,
    v.hostLosses >= 1 ? "host losses" : null,
    v.terms >= 1 ? "terminates" : null,
  ].filter(Boolean);
  const spent = budgets.length > 0 ? ` [run has no more ${budgets.join(", ")}]` : "";
  const orphan = (v.orphans > 0 ? ", an unreachable provider still runs" : "") + down + spent;
  if (v.srv === "down") {
    return v.live.length > 0
      ? `dcserver down, session alive on ${where}${orphan}`
      : `dcserver down, no session${orphan}`;
  }
  if (v.phase === "recon") return `Booting, session not picked up yet (${where})${orphan}`;
  if (v.closing) return "Row retired, close still owed to the host";
  if (v.rec === "pending" || v.rec === "bound") {
    if (v.live.length === 0) return `Row names a session that is gone (${where})${orphan}`;
    if (!v.attached) return `Session alive on ${where}, not held by this dcserver${orphan}`;
    const what = v.rec === "pending" ? "launched, not bound" : v.turn === "running" ? "turn running" : "idle";
    return `On ${where}, ${what}${orphan}`;
  }
  if (v.live.length > 0) return `Provider runs on ${where} with no row${orphan}`;
  if (v.answer === "refused") return `No session, last input refused${orphan}`;
  return `No session${orphan}`;
};

export const group = (v) => {
  if (v.orphans > 0 || v.killed) return "Lost";
  if (v.srv === "down" || v.phase === "recon") return "Restart";
  return "Run";
};

export const colors = { Run: "blue", Restart: "teal", Lost: "pink" };

const labels = {
  Arrive: "a message arrives",
  Launch: "a turn launches a new session",
  Bind: "the first input binds the Herdr execution",
  StartTurn: "a follow-up turn starts",
  Refuse: "the input is refused with an error",
  TurnEnds: "the turn ends",
  DcCrash: "dcserver stops",
  Boot: "dcserver starts",
  Reconcile: "boot reconcile, then intake opens",
  HostLoss: "the tmux or Herdr server goes away",
  HostStart: "the host server is started again",
  DetectLoss: "the gone session's row is retired",
  TermClose: "a terminate closes the session on its host",
  TermRetireFirst: "a terminate retires the row first",
  TermCloseLate: "the owed close reaches the host",
};

export const step = (action) => labels[action] ?? action;
