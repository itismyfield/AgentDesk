export const title = "Herdr restart B ports: owned idle recovery, existing operation ticket, effect permission";
const qs = (s) => Object.values(s.q ?? {});
const settled = (s) => qs(s).every(q => !["queued", "inflight"].includes(q));
const tail = (s) => qs(s).includes("queued") ? "; an input waits" : qs(s).includes("incoming") ? "; an input may still arrive" : "";
const baseState = (v) => {
  const s = v.s;
  if (s.oldSeed) return "Greeting reached the execution the restart should end";
  if (s.oldInputs) return "Old pane received an input while the restart was open";
  if (s.seedEffects > 1) return "Restart seed executed twice";
  if (s.creates > 1) return "Unknown create acknowledgement was replayed";
  if (qs(s).includes("lost")) return "Accepted input lost its owner";
  if (Object.values(s.effects ?? {}).some(n => n > 1)) return "Original input executed twice";
  if ((s.oldLive ? 1 : 0) + s.panes > 1) return "Two live sessions";
  if (s.seedAfterClear) return "The seed ran after a clear began";
  if (s.staleCreate) return "A launch older than the clear created a session";
  if (s.resumedPreClear) return "A launch resumed the conversation a clear ended";
  if (s.seedRevived) return "A cleared seed was queued again";
  if (s.falseSuperseded) return "A delivered seed was reported as cleared";
  if (s.falseReady) return "Ready was said on admission";
  if (s.inherit) return "Successor inherited the old dispatch";
  if (s.retries) return "The restart created a retry dispatch";
  if (s.ready && s.inflight) return "Ready while the old turn is unsettled";
  if (s.seed === "stripped") return "Restart record vanished without a result";
  if (s.clear === "held") return "Clear reported a hold and left the launch running";
  if (s.clearRequested && s.clear === "none") return s.permission ? "Clear requested; wait for the accepted physical effect" : "Clear requested; epoch serializer pending";
  if (s.crashed && ["Bound", "PendingPlaced"].includes(s.row) && !s.oldLive && s.seed === "queued") return "Idle ended execution; the seed owner must repair the row";
  if (s.lphase === "pending" && s.effectBegan) return s.permission ? "BeginEffect holds create authority" : "Create authority was observed but not retained";
  if (s.lphase === "pending" && s.checked) return "CheckEpoch observed; live BeginEffect still required";
  if (s.lphase === "created" && s.effectBegan) return s.permission ? "BeginEffect holds input authority" : "Input authority was released before the effect";
  if (s.lphase === "created" && s.checked) return "CheckEpoch observed; live input authority still required";
  if (s.clear === "begun" || s.clear === "discarded") {
    const what = s.clear === "begun" ? "Clear began: epoch bumped" : "Clear discarded the queue";
    const where = s.excl ? "; restart holds the transition" : (s.launcher && s.lphase === "none") ? "; a finished launch never released its registration" : s.launcher ? "; a turn is launching" : s.panes ? "; a new session lives" : s.oldLive ? "; the old session lives" : "; no session";
    return what + where;
  }
  if (s.launcher && s.lphase === "none") return "A finished launch never released its registration";
  if (s.launcher) return ({ begun: "Owner turn began a launch", pending: "Owner turn installed Pending", created: "Create answered; first input not submitted", sent: "First input submitted; receipt not committed", settled: "Receipt committed; source not bound" })[s.lphase] + (s.epoch ? " (after a clear)" : "");
  if (s.ready) return settled(s) ? "One new Ready session; inputs settled once" + tail(s) : "One new Ready session; inputs await the owner";
  if (s.cmd === "idle") return qs(s).includes("inflight") ? "Old session; an input is in flight to it" : "Old session and a queued input";
  if (s.cmd === "seeded") return s.seed === "queued" ? "Seed committed; gate holds it" : "No seed admitted; restart goes on";
  if (s.cmd === "closing") return "Restart holds the transition and fence";
  if (s.cmd === "retiring") return "Exact close confirmed; retire and settle";
  if (s.cmd === "postlude") return s.epoch ? "Old execution retired; a clear overtook the restart" : "Old execution retired; exclusion still held";
  if (s.cmd === "failing") return "Close refused, unconfirmed or row moved on";
  if (s.cmd === "withdrawing") return "Failure answered; seed still to withdraw";
  if (s.cmd === "notice") return "A result awaits its visible answer";
  if (s.cmd === "answering") return s.epoch ? "Exclusion released; superseded reply pending" : "Exclusion released; reply pending" + (s.awake ? "" : "; owner not woken");
  const done = s.clear === "unconfirmed" ? "Clear ended unconfirmed" : s.clear === "gaveup" ? "Clear gave up on the transition" : s.clear === "done" ? "Clear ended" : "";
  if (done) return done + (s.selector === "forgotten" ? "; selector forgotten" : "; selector still kept") + (s.panes ? "; a new session lives" : s.oldLive ? "; the old session lives" : "") + tail(s);
  if (s.seed === "queued") return (s.crashed ? "After boot: seed waits" : "Seed waits for the owner") + (s.awake ? "" : "; owner not woken") + (s.oldLive ? "; old session lives" : "");
  if (s.seed === "uncertain") return "Seed submit unproven: kept, never re-sent" + tail(s);
  if (s.row === "NewPending") return "Pending without a launching turn" + tail(s);
  if (s.row === "NewBound") return (settled(s) ? "New session without a greeting; inputs settled" : "New session without a greeting") ;
  if (s.visible) return (settled(s) ? "Visible failure; inputs settled" : "Visible failure; inputs still owned") + (s.oldLive ? "; old session lives" : "");
  return (settled(s) ? "Restart answered; inputs settled" : "Restart answered; inputs wait") + (s.awake ? "" : "; owner not woken") + (s.oldLive ? "; old session lives" : "");
};
export const state = (v) => {
  const s = v.s;
  const failures = [s.opSeedless && "known ticket entered the seedless path", s.opDuplicate && "known operation inserted another seed", s.unsafeEffect && "physical effect used no live permission"].filter(Boolean);
  if (!failures.length) return baseState(v);
  const names = { incoming: "not yet arrived", queued: "waiting", inflight: "receipt pending", delivered: "delivered", retained: "retained", cancelled: "explicitly dropped", lost: "lost" };
  const originals = qs(s).map((q, i) => `original ${i + 1} ${names[q] ?? q}`).join(", ");
  return `${baseState(v)}; ${failures.join("; ")}; ${originals}`;
};
export const step = (action, from, to) => ({
  Request: to.s.cmd === "notice" ? "Refuse before any effect" : to.s.seed === "queued" ? "Commit the seed original (acceptance)" : "Seed not admitted: restart anyway",
  Admit: "Accept an input into the owner queue",
  OwnerParks: "Owner drive parks until a wake",
  InflightSettle: "The in-flight unit's own receipt settles it",
  TakeExclusion: "Terminate takes transition and fence",
  CloseExact: "Close only the captured pane",
  CloseFails: "Close refused, unconfirmed or row moved on",
  Retire: "Retire CAS and settle the old turn",
  Postlude: to.s.row === "NewBound" ? "Launch the successor inside the exclusion"
    : to.s.seedRevived ? "Queue the seed again after the clear"
    : from.s.epoch ? (to.s.selector === "forgotten" ? "Clear epoch moved: forget selector, drop seed" : "Clear epoch ignored: selector kept")
    : to.s.retries ? "Disconnect and create a retry dispatch"
    : to.s.awake ? "Disconnect keeping selector, release, wake" : "Disconnect keeping selector, release without a wake",
  Answer: to.s.falseSuperseded ? "Reply reads the seed's absence as a clear" : "Reply to the command",
  Fail: "Answer the failure, release",
  Withdraw: "Withdraw the seed",
  NotifyFailure: "Show the result",
  SeedRefused: "Permit refuses the seed with a notice",
  SeedToOld: "Seed typed into the old execution",
  SeedJoin: "Claim the seed on its recovered exact successor",
  LaunchBegin: "Owner starts the front: register the launch",
  InstallPending: "Install Pending over the vacant row",
  AbortUnsent: "Clear epoch moved: retire the unsent Pending",
  CheckCreateEpoch: "Observe the create epoch",
  BeginCreateEffect: "Compare live epoch and retain create permission",
  Create: "Create once under live effect permission",
  LaunchNotSent: "Create not sent: fail the input visibly",
  LaunchUncertain: "Create unknown: keep Pending, fail visibly",
  ReplayCreate: "Create again after an unknown reply",
  AbortSubmit: "Clear epoch moved: do not submit",
  CheckSubmitEpoch: "Observe the input epoch",
  BeginSubmitEffect: "Compare live epoch and retain input permission",
  SubmitEffect: "Submit the first input under live permission",
  SettleReceipt: "Commit the input's receipt",
  BindSource: "Bind the source",
  Adopt: "Probe adopts the Pending pane",
  Deliver: "Submit an input to the successor once",
  DeliverOld: "Submit an input to the still-current session",
  RefuseUnknown: "Unreadable row refuses the input visibly",
  StaleOldDelivery: "Stale permission types into the old pane",
  ReplayInput: "Forget an input receipt",
  RowEndRecovery: "The seed owner or inflight recovery repairs the exact ended row",
  ReplayRequest: to.s.opSeedless ? "Treat known operation as seedless capacity failure" : to.s.opDuplicate ? "Insert a duplicate operation seed" : "Return or join the existing durable ticket",
  ResolveUnplaced: "Resolver settles the unplaced Pending",
  Crash: "Daemon dies and boots",
  ClearRequest: "User requests clear",
  ClearBegin: "Acquire the serializer and bump the clear epoch",
  ClearDiscard: "Clear discards the queue and the launching input",
  ClearEnd: "Clear's teardown acts on the row",
  Wait: "Wait",
}[action] ?? action);
export const group = (v) => (v.s.ready || v.s.visible || ["done", "gaveup", "unconfirmed", "held"].includes(v.s.clear)) ? "Result" : (v.s.cmd === "dead" || v.s.cmd === "done") ? "Owner" : "Restart";
export const colors = { Result: "teal", Restart: "blue", Owner: "purple" };
