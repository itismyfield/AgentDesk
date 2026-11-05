export const title = "Same input waits for Herdr root recovery before claim";
const phases = {
  running: "Old execution and exact binding",
  down: "dcserver process evidence lost",
  "new-server": "New server witness, old row retained",
  "herdr-ready": "Herdr ready before reconciliation",
};
export const state = (v) => {
  if (phases[v.phase]) return phases[v.phase];
  if (v.q.effects === 1) return v.q.execution === 1 ? "Original sent once to the same pane" : "Original sent once to the new pane";
  if (v.q.state === "claimed" && v.row.state === "bound") return "Front claim granted, dispatch not started";
  if (v.q.inflight) return v.q.state === "inflight" ? "Old-binding dispatch persisted current inflight" : "Held: current-generation inflight blocks quiet";
  if (v.row.state === "pending") return "New execution Pending before create";
  if (v.row.state === "placed") return "One new physical pane recorded";
  if (v.row.state === "fresh-bound") return "New execution Bound";
  if (v.row.state === "retired") {
    if (v.recovery.stage === "cleanup") return "Old row Retired, cleanup pending";
    if (v.q.state === "held") return "Recovery complete, original still Held";
    return "Recovery complete, original ready";
  }
  if (v.q.state === "recovering") return "Original awaits preclaim recovery without inflight";
  if (v.q.state === "held") return "Original Held before claim, custody retained";
  if (v.q.state === "queued") return "Original queued before claim";
  return v.recovery.candidate ? "Reader retains exact RootReplaced candidate" : "Reconciled, exact row retained";
};
export const group = (v) => v.q.effects === 1 ? "Complete" : v.q.state === "held" ? "Held" : "Recovering";
export const colors = { Recovering: "blue", Held: "pink", Complete: "teal" };
const labels = {
  DcRestart: "dcserver restarts",
  HerdrReplaces: "Herdr restarts with a new witness",
  PaneRootReplaces: "the old root is replaced by a lone shell",
  HerdrSurvives: "the exact live pane survives",
  Reconcile: "restart reconciliation completes",
  UseExactGrant: "reuse the current witness grant",
  PreparePolicy: "produce current startup evidence",
  LegacyPreparePolicy: "legacy recovery observes startup policy",
  SkipPreparation: "policy preparation is unavailable",
  CaptureTarget: "capture a new target after observation",
  StrictEndProof: "verify policy, original processes and lone shell",
  QuietHeld: "current inflight prevents retirement",
  RetryReader: "next reader pass retries the retained candidate",
  SettleRestartedInflight: "eligible older-generation inflight settles",
  AwaitWake: "owner awaits the completion signal",
  OtherOwner: "the existing owner retains control",
  Done: "complete",
  ObserveExecution: "reader retains exact replacement evidence",
  AcceptInput: "accept the original input",
  ClaimOldBinding: "spend the front grant without inflight",
  BeginOldBindingStart: "start the old binding and persist inflight",
  RejectOldBinding: "pane gate refuses and retains inflight",
  PreclaimRecover: "existing evidence triggers fresh preclaim recovery",
  HoldBeforeClaim: "temporary refusal keeps the original unclaimed",
  RetireReplacedBound: "strict proof and quiet permit exact retirement",
  ReleaseOldHold: "finish old hold cleanup",
  WakeOriginalInput: "published completion wakes the owner once",
  PrepareFresh: "prepare a fresh create permit",
  StartLaunch: "reserve Pending for the same input",
  CreateOk: "create one physical pane",
  RecordEvidenceAndBind: "record evidence and bind",
  SendFresh: "send the original once",
  SendSameRoot: "reuse the same live pane once",
};
export const step = (action) => labels[action] ?? action;
