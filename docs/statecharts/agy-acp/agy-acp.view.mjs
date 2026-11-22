export const title = "agy acp channel session: one terminal post per person input";
const ids = (v) => Object.keys(v.st);
const stranded = (v) => ids(v).some((i) => v.st[i] === "sent" && !v.done[i] && !v.open.includes(Number(i)));
const procLabel = (v) => !v.running ? "AgentDesk restarting"
  : stranded(v) ? "Input sent, nothing will answer it"
  : v.proc !== "up" ? "No agy child"
  : v.loading ? "Session loading (replay)"
  : v.attached === 0 ? "Child up, no session"
  : v.open.length > 1 ? "Two prompts open (steer)"
  : v.open.length === 1 ? (v.cancelSent ? "Prompt open, cancel sent" : v.cancelReq ? "Prompt open, stop asked" : "Prompt open")
  : "Session idle";
export const state = (v) => procLabel(v);
export const group = (v) => !v.running ? "Restart" : v.proc !== "up" ? "Process" : v.open.length > 0 ? "Turn" : "Session";
export const colors = { Restart: "pink", Process: "tan", Turn: "blue", Session: "teal" };
const steps = {
  Arrive: "a person sends a message",
  Spawn: "AgentDesk starts agy acp",
  NewSession: "session/new",
  Reattach: "reattach the stored id (resume, or load)",
  ReattachRefused: "agy refuses the stored id; session/new",
  ReplayOld: "load replays an old answer",
  LoadAnswered: "session/load answers",
  SendPrompt: "session/prompt for the next input",
  AgyResponds: "agy answers the prompt (stopReason)",
  AgyDies: "agy exits; stdout EOF fails open prompts",
  WatchdogKill: "deadline passes; process group killed",
  ZombieResponse: "late response from the killed child",
  UserStop: "person sends !stop",
  SendCancel: "session/cancel",
  Clear: "person runs /clear",
  CloseAfterTurn: "turn done; child closed",
  Settle: "writer posts the one terminal message",
  Crash: "AgentDesk restarts",
  Boot: "boot settles orphaned prompts",
  Rest: "all inputs settled",
};
export const step = (action) => steps[action] ?? action;
