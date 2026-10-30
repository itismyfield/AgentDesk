// Reader's words for TmuxSession.tla: one Discord channel's Claude tmux session.
export const title = "AgentDesk channel session on tmux, across one server restart";

export const state = (v) => {
  if (v.srv === "down") return v.row === "held" ? "Server down, turn row on disk" : "Server down";
  if (v.srv === "boot" && v.turn === "idle")
    return v.row === "held" ? "Restarting, turn row not picked up" : "Restarting, no turn row";
  switch (v.turn) {
    case "starting":
      return "Turn starting";
    case "sending":
      return "Writing the prompt into the live session";
    case "recreating":
      return "Starting a fresh session for the turn";
    case "stopping":
      return "Stop: session killed, row still on disk";
    case "running":
      return v.sess === "live" ? "Turn running" : "Turn running, session died";
  }
  if (v.row === "held")
    return v.sess === "none"
      ? "Open, a turn row names a missing session"
      : "Open, a turn row with no running turn";
  return "Idle";
};

export const group = (v) => {
  if (v.srv !== "up") return "Restart";
  if (v.turn === "idle" && v.row === "held") return "Stuck";
  return "Normal";
};

export const colors = { Normal: "blue", Restart: "yellow", Stuck: "pink" };

const labels = {
  UserSends: "a message arrives",
  ClaimTurn: "a message starts a turn",
  PlanWarm: "the session is live, so the turn reuses it",
  PlanCold: "a new session starts with the prompt [no live session]",
  FollowupDelivered: "the prompt reaches the session",
  FollowupFails: "the prompt write fails, so the turn recreates",
  Recreate: "a new session starts with the prompt",
  ProviderExits: "Claude exits in the session",
  TurnEnds: "the answer is posted",
  TurnDiesMidway: "the watcher posts a died notice",
  StopKill: "you stop the turn",
  StopClear: "the turn row is cleared",
  Crash: "the server crashes",
  Boot: "the server boots",
  RestoreReattach: "recovery reattaches [session live]",
  RestoreRetry: "recovery retries the turn [pane dead]",
  RestoreMissing: "recovery posts a notice and clears the row [session gone]",
  RestoreDefer: "the recovery probe fails, so the row is left",
  RestoreNoRow: "recovery finds no row",
  WatchersAdopt: "watcher restore adopts the live session",
  WatchersKillDead: "watcher restore kills the dead session",
  WatchersNothing: "watcher restore has nothing to do",
  WatchersProbeFails: "the watcher restore probe fails",
  OpenIntake: "intake opens",
};

// Hidden arrows (said in the hand-off): a crash out of the short in-between states, a stop after
// Claude already exited, and a reattached turn that ends before intake opens. Each repeats what
// the crash, stop and end arrows from "Turn running" show.
export const step = (action, from) => {
  if (action === "Crash" && (["sending", "recreating", "stopping"].includes(from.turn)
      || (from.turn === "running" && from.sess !== "live"))) return null;
  if (action === "StopKill" && from.sess !== "live") return null;
  if ((action === "TurnEnds" || action === "TurnDiesMidway") && from.srv === "boot") return null;
  return labels[action] ?? action;
};
