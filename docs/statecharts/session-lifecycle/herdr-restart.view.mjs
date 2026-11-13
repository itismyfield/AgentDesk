export const title = "Herdr /restart: exact close, new incarnation, original input custody";
export const state = (v) => {
  const s = v.s;
  if (s.clearSeen && s.phase === "ready") return "Superseded restart published Ready";
  if (s.oldInputs) return "Old pane received a restart input";
  if (s.foreignClosed) return "Reused numeric target was closed";
  if (s.seedEffects > 1) return "Restart seed executed twice";
  if (s.creates > 1) return "Unknown create acknowledgement was replayed";
  if (Object.values(s.q ?? {}).includes("lost")) return "Accepted input lost its owner";
  if (Object.values(s.effects ?? {}).some(n => n > 1)) return "Original input executed twice";
  if (s.phase === "ready" && s.oldLive) return "Two live sessions after restart";
  if (s.creates > 0 && s.nonce === 0) return "New pane reused the old incarnation";
  return {
    idle: "Old session and original queued input",
    closing: "Restart excludes old input and captures exact pane",
    retiring: "Exact close confirmed; settle old inflight",
    preparing: "Old incarnation retired; originals still held",
    creating: "Fresh incarnation Pending before create",
    binding: "New pane created; wait for evidence and binding",
    ready: s.awake ? (Object.values(s.q ?? {}).every(q => ["delivered", "cancelled"].includes(q)) ? "One new Ready session; originals settled once" : "One new Ready session; originals await the owner") : "New Ready session has no input wake",
    notice: "Failure awaiting a visible result",
    failed: "Visible failure; originals remain owned",
  }[s.phase] ?? s.phase;
};
export const step = (action, from, to) => action === "Request" && to.s.phase === "notice"
  ? "User /restart is refused before cancellation"
  : action === "CloseExact" && !from.s.oldLive ? "No old pane requires closure"
  : ({
  Request: "User /restart is accepted under transition and fence",
  RefuseIdentity: "Unknown or changed identity refuses close",
  CloseFails: "Exact close is unconfirmed",
  CloseExact: "Close only the captured pane",
  RetireAndSettle: "Retire and settle only the old token",
  InstallPending: "Commit new incarnation before create",
  Create: "Create reply is observed once",
  BindReady: "Evidence and binding admit the successor",
  NotifyFailure: "Show the failure and retain input custody",
  ClearSupersedes: "User clear discards originals with terminal receipts",
  AbortSuperseded: "Changed queue generation cancels the old restart",
  Admit: "Accept an original into the existing owner",
  Deliver: "Submit an original to the successor once",
  StaleOldDelivery: "Stale input permission types into old pane",
  ReplayInput: "Forget an original receipt",
  UnsafeForeignClose: "Reuse a numeric target after replacement",
  Wait: "Wait",
}[action] ?? action);
export const group = (v) => ["ready", "failed"].includes(v.s.phase) ? "Result" : "Restart";
export const colors = { Result: "teal", Restart: "blue" };
