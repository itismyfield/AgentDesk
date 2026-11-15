export const title = "Session host: one channel across dcserver restarts, host loss and terminate";
const has = (set, x) => Array.isArray(set) ? set.includes(x) : Object.values(set || {}).includes(x);
const count = set => Array.isArray(set) ? set.length : Object.keys(set || {}).length;
const base = v => {
  if (v.killed) return "Provider ended with dcserver";
  if (v.srv === "down") return count(v.live) ? "dcserver down, provider still running" : "dcserver down, nothing running";
  if (v.srv === "boot") return "Boot reconcile";
  if (v.flight === "closing") return has(v.live, v.record) || !count(v.live) ? "Closing the session" : "Closing, but the running provider is not the recorded one";
  const unreachable = v.host === "process" && !v.registry;
  if (v.stop) return !count(v.live) ? "End requested, provider gone" : unreachable ? "End requested, provider running but unreachable" : "End requested, provider running";
  if (v.recoverOwed) return `Waiting to re-adopt the ${v.row === "Pending" ? "created" : "bound"} execution`
    + (has(v.live, v.record) ? "" : " (provider gone)");
  if (v.row === "Pending") {
    if (v.flight === "created") return "Created, location not yet recorded";
    if (v.flight === "creating") return "Creating";
    if (!v.locator) return "Pending, no pane recorded";
    if (!has(v.live, v.record)) return "Pane recorded, pane gone";
    if (!v.prov) return "Pane recorded, only a shell";
    if (v.firstIn === "sent") return v.src ? "First prompt taken, its source logged, not bound" : "First prompt sent, waiting for its hook";
    return v.src ? "Pane recorded, provider up with its source, not bound" : "Pane recorded, provider up, no prompt yet";
  }
  if (v.row === "Bound") return !has(v.live, v.record) ? "Row bound, provider gone" : v.src ? "Session up" : "Bound without its source";
  if (count(v.live)) return "Row closed, provider still running";
  if (v.kept) return "No session, input kept for a host";
  if (v.lost) return "No session, input lost";
  return v.answer === "refused" ? "Input refused, no session"
    : v.answer === "unavailable" ? "No host, input answered" : "No session";
};
export const state = v => {
  let label = base(v);
  if (count(v.live) > 1) label += ", two providers running";
  else if (count(v.live) && !has(v.live, v.record) && v.srv !== "down" && !v.killed) label += ", an unrecorded provider running";
  if (v.hostSrv === "down") label += ", host server down";
  else if (v.losses === 1) label += ", host server replaced";
  if (v.want && !label.includes("waiting")) label += ", a message waiting";
  if (v.srv !== "down" && v.crashes === 1 && !v.killed) label += " (after the restart)";
  if (v.cancel === "requested") label += ", cancel pending";
  if (v.cancel === "dropped") label += ", cancel dropped";
  if (v.cancel === "delivered") label += ", cancel delivered";
  if (v.kept && !v.tmuxUp) label += ", no tmux yet";
  if (v.inputs >= 2 && !v.want) label += " (no more messages)";
  return label;
};
export const group = v => v.recoverOwed || v.srv !== "up" ? "Recovery" : v.stop || v.flight === "closing" ? "Termination" : "Session";
export const colors = { Recovery: "blue", Termination: "pink", Session: "teal" };
const words = {
  Arrive: "a message arrives", Reserve: "a message reserves a new execution", CreateEffect: "the host creates it",
  CreateAborts: "the create reply is lost", RecordLocation: "its pane is recorded",
  ProviderStarts: "the provider starts in the pane", Bind: "the turn binds the execution",
  TurnFails: "the turn is refused: no provider, or its first prompt already taken", PolicyRefuse: "the message is refused by policy",
  Unavailable: "no host: one notice, the message is kept", Requeue: "a host is back: the kept message runs",
  FirstPrompt: "the turn sends Codex its first prompt", HookArrives: "Codex's first hook arrives",
  TurnAttach: "the turn binds on that hook", FollowupNoSource: "the next prompt is refused: no source",
  TmuxReturns: "tmux becomes usable", ConfirmReady: "the waiting provider is confirmed, still Pending",
  RequestCancel: "a cancel is requested", DeliverCancel: "the cancel is delivered or dropped",
  AdoptUnplaced: "the census finds its pane", AdoptPlaced: "the recorded pane's provider is adopted",
  ResolvePlacedGone: "the recorded pane is gone: row closed", ResidueClose: "the lone shell is closed",
  StartTurn: "the next turn starts on the same session", Crash: "dcserver stops",
  Boot: "dcserver starts", BootDone: "intake opens", ProbeRecovers: "a host probe stops failing",
  Adopt: "the same execution is adopted again", HostLoss: "the host server ends",
  HostStart: "the host server starts again", ResolveEmpty: "the census finds nothing and closes the row",
  DetectLoss: "the gone provider's row is closed", ReplayPending: "a second create from the same Pending",
  RequestStop: "an end is requested", BeginClose: "the close is sent", CloseEffect: "the owned provider is confirmed gone",
  FinishTerminate: "the row is retired",
};
export const step = (action) => (action in words ? words[action] : action);
