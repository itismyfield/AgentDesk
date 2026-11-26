export const title = "thread under a Herdr parent: where its new session runs (S2)";
export const state = (v) =>
  v.inq === "Refused" ? "Input refused"
  : v.exec === "Unread" ? "Row unreadable"
  : v.exec === "Pending" ? (v.made ? "Herdr pane made, location not recorded" : "Herdr row Pending, no pane yet")
  : v.exec === "Placed" ? (v.ran === false ? "Herdr pane placed, its command may not have run" : "Herdr pane placed, not bound yet")
  : v.boot === "Gate" ? "Boot waits before consumers"
  : v.exec === "Bound" ? (v.inq === "Queued" ? "Herdr pane bound, first input waiting" : v.idle ? "Herdr pane idle" : "Herdr pane running")
  : v.exec === "Tmux" ? (v.idle ? "tmux session idle" : "tmux session running")
  : v.exec === "Retired" ? "Herdr execution retired"
  : v.exec === "Gone" ? "Row deleted by thread_gc"
  : v.exec === "LegacyDead" ? "Legacy row, no tmux session"
  : "No session row";
export const group = (v) =>
  v.inq === "Refused" ? "Refused"
  : ["Pending", "Placed", "Bound", "Retired"].includes(v.exec) ? "Herdr"
  : v.exec === "Tmux" ? "Tmux" : "None";
export const colors = { Herdr: "teal", Tmux: "tan", None: "blue", Refused: "pink" };
const steps = {
  Launch: "a queued input starts the thread's session (turn host judges the host)",
  Create: "Herdr creates the pane (answer may be lost)",
  Resolve: "unplaced row resolved: the pane adopted (not bound), or the row retired",
  Bind: "own SessionStart and source checked: the row binds",
  OrdinaryReady: "an ordinary adoption's writer becomes ready",
  GateRefused: "a gate refuses the unverified thread",
  ReadRecovers: "the row read recovers",
  ReadRefused: "the turn host refuses the unreadable row",
  SubmitTmux: "the input goes to the live tmux session",
  Settle: "the submitted input settles with its receipt",
  Reserve: "the output reservation is made durable before the first key",
  DispatchReset: "the dispatch resets the provider before its turn",
  BootNoGate: "boot starts consumers without a seed gate",
  BootRetireDead: "boot retires the row whose pane is gone, then starts consumers",
  RelayRetire: "the relay's rehydrate pass retires the row whose pane is gone",
  OtherServe: "another channel's queued work runs",
  CmdRestartVacant: "/restart seeds the vacant thread",
  Rejudge: "the replayed input meets the unresolved Pending row",
  SubmitHerdr: "the first input is submitted to the Herdr pane",
  Restart2: "AgentDesk restarts",
  Idle: "the turn ends and the thread stays idle past an hour",
  ReleaseIdle: "idle cleanup ends the session",
  Gc: "thread_gc deletes the row",
  SlotReset: "auto-queue resets the slot thread",
  PlacedResidue: "the placed pane's untouched shell (exact marker, root, pristine tab) is closed and the row retired",
  Successor: "a reused dispatch or a direct message starts a successor in the released thread",
  ArchiveThread: "the slot reset archives the thread on Discord",
};
export const step = (action, from, to) =>
  action === "Launch" && to.exec === "Tmux" ? "a queued input starts the session on tmux"
  : steps[action] ?? action;
