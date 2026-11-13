export const title = "Herdr /restart r2: seed at the queue head, exact close, the owner launches";
const settled = (s) => Object.values(s.q ?? {}).every(q => q !== "queued");
export const state = (v) => {
  const s = v.s;
  if (s.oldSeed) return "Greeting reached the execution the restart should end";
  if (s.oldInputs) return "Old pane received an input during the restart";
  if (s.seedEffects > 1) return "Restart seed executed twice";
  if (s.creates > 1) return "Unknown create acknowledgement was replayed";
  if (Object.values(s.q ?? {}).includes("lost")) return "Accepted input lost its owner";
  if (Object.values(s.effects ?? {}).some(n => n > 1)) return "Original input executed twice";
  if ((s.oldLive ? 1 : 0) + s.panes > 1) return "Two live sessions";
  if (s.resumedPreClear) return "A launch resumed the conversation a clear ended";
  if (s.seedRevived) return "A cleared seed was queued again";
  if (s.inherit) return "Successor inherited the old dispatch";
  if (s.ready && s.inflight) return "Ready while the old turn is unsettled";
  if (s.seed === "stripped") return "Restart record vanished without a result";
  if (s.clearHeld) return "Clear reported a hold: a turn was launching";
  if (s.ready) return settled(s) ? "One new Ready session; inputs settled once" : "One new Ready session; inputs await the owner";
  if (s.cmd === "idle") return "Old session and a queued input";
  if (s.cmd === "seeded") return "Seed committed at the queue head; gate holds it";
  if (s.cmd === "closing") return "Restart holds the transition and fence";
  if (s.cmd === "retiring") return "Exact close confirmed; retire and settle";
  if (s.cmd === "postlude") return "Old execution retired; exclusion still held";
  if (s.cmd === "failing") return "Close refused or unconfirmed";
  if (s.cmd === "withdrawing") return "Failure answered; seed still to withdraw";
  if (s.cmd === "notice") return "A result awaits its visible answer";
  if (s.row === "NewPending") return "Owner turn installed Pending and creates";
  if (s.seed === "queued" && !s.awake) return "Seed waits; the owner drive was never woken";
  if (s.seed === "queued") return s.crashed ? "After boot: seed waits at the queue head" : "Exclusion released; seed waits for the owner";
  if (s.seed === "cancelled") {
    if (s.selector === "forgotten") return "Clear won: seed dropped, selector forgotten";
    const q = Object.values(s.q ?? {});
    const waits = q.includes("queued") ? "; an input waits" : q.includes("incoming") ? "; an input may still arrive" : "";
    if (s.clear === "gaveup") return "Clear gave up on the transition; selector still kept" + waits;
    if (s.clear === "done") return "Clear finished; selector still kept" + waits;
    return "Clear dropped the seed; its teardown still to run" + waits;
  }
  if (s.visible) return settled(s) ? "Visible failure; inputs settled" : "Visible failure; inputs still owned";
  return settled(s) ? "No restart result; inputs settled" : "No restart result yet";
};
export const step = (action, from, to) => ({
  Request: to.s.seed === "queued" ? "Commit the seed original (acceptance)" : "Refuse before any effect",
  Admit: "Accept an input into the owner queue",
  OwnerParks: "Owner drive parks until a wake",
  TakeExclusion: "Terminate takes transition and fence",
  CloseExact: "Close only the captured pane",
  CloseFails: "Close refused, unconfirmed or row moved on",
  Retire: "Retire CAS and settle the old turn",
  Postlude: to.s.row === "NewBound" ? "Launch the successor inside the exclusion"
    : to.s.seedRevived && !from.s.seedRevived ? "Queue the seed again after the clear"
    : to.s.cmd === "notice" ? (to.s.selector === "forgotten" ? "Seed gone: forget selector as the clear would" : "Seed gone: selector left as it was")
    : to.s.awake ? "Disconnect keeping selector, release, wake" : "Disconnect keeping selector, release without a wake",
  Fail: "Answer the failure, release",
  Withdraw: "Withdraw the seed",
  NotifyFailure: "Show the result",
  SeedRefused: "Permit refuses the seed with a notice",
  SeedToOld: "Seed typed into the old execution",
  SeedLaunch: "Owner starts the seed: install Pending",
  SeedJoin: "Seed joins the successor",
  InputLaunch: "Owner starts an input: install Pending",
  Create: "Create the pane once",
  LaunchNotSent: "Create not sent: fail the input visibly",
  LaunchUncertain: "Create unknown: keep Pending, fail visibly",
  Adopt: "Probe adopts the Pending pane",
  ReplayCreate: "Create again after an unknown reply",
  Submit: "Submit the first input and bind the source",
  Deliver: "Submit an input to the successor once",
  DeliverOld: "Submit an input to the still-current session",
  RefuseUnknown: "Unreadable row refuses the input visibly",
  StaleOldDelivery: "Stale permission types into the old pane",
  ReplayInput: "Forget an input receipt",
  ReplaySeed: "Submit the seed again after boot",
  RowEndRecovery: "Recovery retires the row of a gone pane",
  ResolveUnplaced: "Resolver settles the unplaced Pending",
  Crash: "Daemon dies and boots",
  ClearRead: "User clear reads the row",
  ClearDiscard: "Clear discards the queue with receipts",
  ClearEnd: "Clear ends the execution and forgets the selector",
  ClearGivesUp: "Clear's terminate times out on the transition",
  Wait: "Wait",
}[action] ?? action);
export const group = (v) => (v.s.ready || v.s.visible || v.s.seed === "cancelled") ? "Result" : (v.s.cmd === "dead" || v.s.cmd === "done") ? "Owner" : "Restart";
export const colors = { Result: "teal", Restart: "blue", Owner: "purple" };
