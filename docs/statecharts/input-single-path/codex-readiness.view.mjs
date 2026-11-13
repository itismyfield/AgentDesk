// Production SHA 082c77aa4e06b81c76b614f55b8fed73ce01923f; hand-authored projection.
export const title = "Codex ready, busy, end and /stop judged from the rollout, the hooks and the process";
export const state = (v) => {
  const r = v.r ?? {};
  if (r.badEarly) return "RED: Idle-route input reached a running turn";
  if (r.badShell) return "RED: input reached a pane with no live Codex";
  if (r.badKill) return "RED: a replacement killed a running turn";
  if (r.badStopIdle) return "RED: /stop answered idle while its turn runs";
  if (r.badStopKey) return "RED: stop key written with no open turn";
  // The launch and turn counts tell apart states that one-off events can no longer leave.
  const at = `${r.host} launch ${r.inc}, ${r.turn} turn(s) started${r.mismatch ? ", launch options changed" : ""}`;
  const label = (text) => `${text} (${at})`;
  if (r.stop === "asked") return label("/stop asked: judge the own turn");
  if (r.head) return label("Ready proven: read again before the first key");
  if (r.wait) {
    return label(r.phase === "Running"
      ? "45 s pane wait while a turn runs: the timeout will replace the pane"
      : "45 s pane wait: the pane is read for an empty composer");
  }
  if (r.phase === "Booting") return label("Booting: wait for this launch's Codex process");
  if (r.phase === "Exited") return label("Process gone: new incarnation");
  if (r.phase === "Running") {
    if (r.pending) {
      return label(r.ups
        ? "Enter pressed, task_started not written: the UserPromptSubmit hook says busy"
        : "Enter pressed, task_started not written: no signal says busy");
    }
    if (r.file === "zst") return label(r.ups ? "Turn running on an archived rollout: the UserPromptSubmit hook says busy" : "Turn running on an archived rollout: no signal says busy");
    return label("Turn running: the rollout reads open");
  }
  if (r.mode === "before") {
    if (r.rec === "started" || (r.host === "tmux" && r.rec === "unknown")) {
      return label(r.host === "tmux"
        ? "Idle, but the rollout still reads busy: follow-ups stay queued"
        : "Idle, pane unread, the rollout still reads an open turn");
    }
    return label(r.screen === "ok" ? "Idle: the pane shows an empty composer" : "Idle, pane unread: no ready verdict");
  }
  if (r.ups) return label("Idle, a UserPromptSubmit hook with no readable turn start: Busy until it expires");
  if (r.file === "zst") return label("Idle, rollout archived: no signal can prove the end");
  if (r.late) return label("Idle, end record not written yet");
  if (r.rec === "unknown") return label(r.hook ? "Idle, end record unreadable: Stop hook proves the end" : "Idle, end record unreadable, no hook");
  if (r.rec === "started") return label(r.hook ? "Idle, end record lost: Stop hook proves the end" : "Idle, every end signal lost: held with a reason");
  return label("Idle: the rollout proves the end");
};
const steps = {
  Boot: "Codex becomes the pane's running process",
  PersonEnter: "a person presses Enter in the TUI",
  RecordStart: "task_started reaches the rollout",
  PersonBlocked: "a UserPromptSubmit hook arrives and no turn follows",
  ConfigChange: "the launch options or the binding change",
  ConfigReplace: "warm reuse is refused: the pane is replaced and the prompt goes in the argv",
  UpsExpire: "the UserPromptSubmit evidence expires: quiet rollout, no child",
  Finish: "the turn ends in the TUI",
  WriteLate: "the end record reaches the rollout",
  CompressRollout: "Codex archives the idle rollout to .zst",
  Exit: "the Codex process exits",
  Restart: "dcserver restarts",
  Admit: "the follow-up enters the 45 s pane wait",
  TimeoutReplace: "the wait times out: the pane is replaced and the prompt goes in the argv",
  Prepare: "the next input proves ready",
  Submit: "the input is written, or its proof no longer holds",
  BusyDeliver: "the busy entry delivers the input",
  Relaunch: "a new incarnation is launched",
  StopAsk: "/stop is asked",
  StopJudge: "/stop is judged",
};
export const step = (action) => steps[action] ?? action;
