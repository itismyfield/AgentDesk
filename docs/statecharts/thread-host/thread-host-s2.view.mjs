export const title = "thread under a Herdr parent: where its new session runs (S2)";
export const state = (v) =>
  v.inq === "Refused" ? "Input refused"
  : v.exec === "Pending" ? (v.made ? "Herdr pane made, location not recorded" : "Herdr row Pending, no pane yet")
  : v.exec === "Bound" ? (v.inq === "Queued" ? "Herdr pane bound, first input waiting" : v.idle ? "Herdr pane idle" : "Herdr pane running")
  : v.exec === "Tmux" ? (v.idle ? "tmux session idle" : "tmux session running")
  : v.exec === "Retired" ? "Herdr execution retired"
  : v.exec === "Gone" ? "Row deleted by thread_gc"
  : v.exec === "LegacyDead" ? "Legacy row, no tmux session"
  : "No session row";
export const group = (v) =>
  v.inq === "Refused" ? "Refused"
  : ["Pending", "Bound", "Retired"].includes(v.exec) ? "Herdr"
  : v.exec === "Tmux" ? "Tmux" : "None";
export const colors = { Herdr: "teal", Tmux: "tan", None: "blue", Refused: "pink" };
const steps = {
  Launch: "a queued input starts the thread's session (turn host judges the host)",
  Create: "Herdr creates the pane (answer may be lost)",
  Resolve: "unplaced row resolved: the pane adopted, or the row retired",
  Rejudge: "the replayed input meets the unresolved Pending row",
  SubmitHerdr: "the first input is submitted to the Herdr pane",
  Restart2: "AgentDesk restarts",
  Idle: "the turn ends and the thread stays idle past an hour",
  ReleaseIdle: "idle cleanup ends the session",
  Gc: "thread_gc deletes the row",
  SlotReset: "auto-queue resets the slot thread",
};
export const step = (action, from, to) =>
  action === "Launch" && to.exec === "Tmux" ? "a queued input starts the session on tmux"
  : steps[action] ?? action;
