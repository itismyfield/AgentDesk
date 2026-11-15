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
  if (v.row === "Pending") return v.flight === "created" ? "Created, location not yet recorded"
    : v.flight === "creating" ? "Creating" : "Pending, awaiting the host census";
  if (v.row === "Bound") return has(v.live, v.record) ? "Session up" : "Row bound, provider gone";
  if (count(v.live)) return "Row closed, provider still running";
  return v.answer === "refused" ? "Input refused, no session" : "No session";
};
export const state = v => {
  let label = base(v);
  if (count(v.live) > 1) label += ", two providers running";
  else if (count(v.live) && !has(v.live, v.record) && v.srv !== "down" && !v.killed) label += ", an unrecorded provider running";
  if (v.hostSrv === "down") label += ", host server down";
  else if (v.losses === 1) label += ", host server replaced";
  if (v.want && !label.includes("waiting")) label += ", a message waiting";
  if (v.srv !== "down" && v.crashes === 1 && !v.killed) label += " (after the restart)";
  if (v.inputs >= 2 && !v.want) label += " (no more messages)";
  return label;
};
export const group = v => v.recoverOwed || v.srv !== "up" ? "Recovery" : v.stop || v.flight === "closing" ? "Termination" : "Session";
export const colors = { Recovery: "blue", Termination: "pink", Session: "teal" };
const words = {
  Arrive: "a message arrives", Reserve: "a message reserves a new execution", CreateEffect: "the host creates it",
  CreateAborts: "the create reply is lost", RecordLocation: "its location is recorded and the turn starts",
  StartTurn: "the next turn starts on the same session", Refuse: "the message is refused before any spawn", Crash: "dcserver stops",
  Boot: "dcserver starts", BootDone: "intake opens", ProbeRecovers: "a host probe stops failing",
  Adopt: "the same execution is adopted again", HostLoss: "the host server ends",
  HostStart: "the host server starts again", ResolveEmpty: "the census finds nothing and closes the row",
  DetectLoss: "the gone provider's row is closed", ReplayPending: "a second create from the same Pending",
  RequestStop: "an end is requested", BeginClose: "the close is sent", CloseEffect: "the owned provider is confirmed gone",
  FinishTerminate: "the row is retired",
};
export const step = (action) => (action in words ? words[action] : action);
