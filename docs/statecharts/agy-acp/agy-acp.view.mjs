export const title = "agy acp channel session: one terminal post per person input";
const ids = (v) => Object.keys(v.st);
const turnHasOpen = (v, t) => v.open.some((j) => v.turnOf[j] === t);
const stranded = (v) => ids(v).some((i) => v.st[i] === "sent" && !v.open.includes(Number(i))
  && (v.turnOf[i] === 0 || (!v.done[v.turnOf[i]] && !turnHasOpen(v, v.turnOf[i]))));
const closing = (v) => ids(v).some((i) => v.turnOf[i] === Number(i) && v.done[i] && v.posts[i] === 0);
const anyIn = (v, f) => ids(v).some((i) => f(i));
const procLabel = (v) => !v.running ? "AgentDesk restarting"
  : anyIn(v, (i) => v.returning[i]) ? "Rejected steer held by Returning record"
  : anyIn(v, (i) => v.retMem[i]) ? "Rejected steer held in memory only"
  : stranded(v) ? "Input sent, nothing will answer it"
  : v.orphan ? "Old child orphaned, sweep pending"
  : v.proc !== "up" ? "No agy child"
  : v.loading ? "Session loading (replay)"
  : v.attached === 0 ? "Child up, no session"
  : v.open.length > 1 ? "Turn steered: two prompts open"
  : v.open.length === 1 ? (v.cancelSent ? "Prompt open, cancel sent" : v.cancelReq ? "Prompt open, stop asked" : "Prompt open")
  : closing(v) ? "Turn answered, post pending"
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
  SendPrompt: "session/prompt for the next input (new turn, or steer while a prompt is open)",
  SteerRejected: "steer prompt rejected before execution",
  SteerErrored: "steer prompt fails with unknown outcome",
  ReturnToQueue: "rejected steer put back at the queue front",
  SweepOrphan: "boot kills the old child's process group",
  AgyResponds: "agy answers the prompt (stopReason)",
  AgyDies: "agy exits; stdout EOF fails open prompts",
  WatchdogKill: "deadline passes; the child's own process group killed",
  ZombieResponse: "late response from the killed child",
  UserStop: "person sends !stop",
  SendCancel: "session/cancel",
  Clear: "person runs /clear",
  CloseAfterTurn: "turn done; child closed",
  Settle: "writer posts the turn's one terminal message (covers steered inputs)",
  Crash: "AgentDesk restarts",
  Boot: "boot settles orphaned prompts",
  Rest: "all inputs settled",
};
export const step = (action) => steps[action] ?? action;
