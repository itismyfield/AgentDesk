export const title = "Separate contracts: restart lifecycle B and Codex resume helper A";
const restartState = (v) => {
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
export const step = (raw, from, to) => {
  const action = raw.startsWith("R") ? raw.slice(1) : raw.startsWith("P") ? "A" + raw.slice(1) : raw;
  return action === "Request" && to.s.phase === "notice"
  ? "User /restart is refused before cancellation"
  : action === "CloseExact" && !from.s.oldLive ? "No old pane requires closure"
  : action === "AConfirmEnd" && !to.a.resumeIntent ? "Ordinary launch keeps fresh mode"
  : action === "ACapture" && !to.a.resumeIntent ? "Ordinary fresh launch keeps reader offset zero"
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
  AConfirmEnd: "Confirm old provider End before EOF capture",
  ABeforeRefusal: "Current fresh-only entry refuses explicit resume",
  ACapture: "Compare post-End EOF with O cursor",
  APersist: "Persist reader offset and mode in the incarnation",
  ALaunch: "Launch once and observe own Startup or Resume",
  ACrashBeforeSeed: "Crash after startup metadata and before seed",
  ABoot: "Recover the recorded incarnation and reader offset",
  ANoCrash: "Continue without a process crash",
  AObserve: "Bind the scoped cause and tail from the recorded offset",
  AWait: "Wait",
  Wait: "Wait",
}[action] ?? action);
};
const resumeState = (v) => {
  const a = v.a;
  if (a.tailStarted && a.readOffset < a.capturedOffset) return "Resumed reader replays old history";
  if (a.tailStarted && a.readOffset > a.capturedOffset) return "Recovered reader skips new prefix";
  if (a.recovered && a.persisted && a.loadedOffset !== a.durableOffset) return "Boot replaced the recorded offset";
  if (a.captured && a.resumeIntent && !a.endConfirmed) return "EOF was captured before confirmed old End";
  if (a.prepared && a.resumeIntent && !a.cursorMatches) return "Resume accepted a mismatched O cursor";
  if (!a.resumeIntent && a.mode === "resume") return "Ordinary launch acquired resume mode";
  if (a.launched && !a.persisted) return "Missing durable offset at " + a.phase;
  return {
    end: "Verified resume intent or ordinary fresh launch",
    capture: a.resumeIntent ? "Old End confirmed before EOF capture" : "Ordinary launch has no old reader boundary",
    record: a.resumeIntent ? "EOF and O cursor agree; prepare immutable incarnation" : "Prepare an ordinary fresh incarnation with offset zero",
    launch: "Offset durably recorded before provider launch",
    before_seed: "Own startup metadata appended; seed not submitted",
    boot: "Process crashed; volatile reader state lost",
    observe: "Boot restored the original reader offset",
    done: a.mode === "resume" ? "Resume cause accepted; old history excluded" : "Ordinary fresh launch uses Startup and offset zero",
    failed: "Visible preparation or binding failure; no tail",
  }[a.phase] ?? a.phase;
};
export const state = (v) => ({ "Restart lifecycle B": restartState(v), "Codex resume helper A": resumeState(v) });
export const group = () => "Contract";
export const colors = { Contract: "blue" };
