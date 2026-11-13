// Production SHA 082c77aa4e06b81c76b614f55b8fed73ce01923f; hand-authored projection.
export const title = "Codex ready, busy, end and /stop judged from the rollout, the Stop hook and the process";
export const state = (v) => {
  const r = v.r ?? {};
  if (r.badEarly) return "RED: Idle-route input reached a running turn";
  if (r.badShell) return "RED: input reached a pane with no live Codex";
  if (r.badKill) return "RED: relaunch killed a running turn";
  if (r.badStopIdle) return "RED: /stop answered idle while its turn runs";
  if (r.badStopKey) return "RED: stop key written with no open turn";
  // The launch and turn counts tell apart states that one-off events can no longer leave.
  const at = `launch ${r.inc}, ${r.turn} turn(s) started`;
  const label = (text) => `${text} (${at})`;
  if (r.stop === "asked") return label("/stop asked: judge the own turn");
  if (r.head) return label("Ready proven: read again before the first key");
  if (r.phase === "Booting") return label("Booting: wait for this launch's Codex process");
  if (r.phase === "Exited") return label("Process gone: new incarnation");
  if (r.phase === "Running") return label("Turn running: the busy entry stays available");
  if (r.mode === "before") {
    return label(r.screen === "ok" ? "Idle: the pane shows an empty composer" : "Idle, pane unread: no ready verdict");
  }
  if (r.file === "zst") return label("Idle, rollout archived: no signal can prove the end");
  if (r.late) return label("Idle, end record not written yet");
  if (r.rec === "unknown") return label(r.hook ? "Idle, end record unreadable: Stop hook proves the end" : "Idle, end record unreadable, no hook");
  if (r.rec === "started") return label(r.hook ? "Idle, end record lost: Stop hook proves the end" : "Idle, every end signal lost");
  return label("Idle: the rollout proves the end");
};
const steps = {
  Boot: "Codex becomes the pane's running process",
  PersonStart: "a person starts a turn in the TUI",
  Finish: "the turn ends in the TUI",
  WriteLate: "the end record reaches the rollout",
  CompressRollout: "Codex archives the idle rollout to .zst",
  Exit: "the Codex process exits",
  Restart: "dcserver restarts",
  Prepare: "the next input proves ready",
  Submit: "the input is written, or its proof no longer holds",
  BusyDeliver: "the busy entry delivers the input",
  Relaunch: "a new incarnation is launched",
  StopAsk: "/stop is asked",
  StopJudge: "/stop is judged",
};
export const step = (action) => steps[action] ?? action;
