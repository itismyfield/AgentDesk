export const title = "Herdr root replacement: produce policy evidence before capturing the target";
const states = {
  running: "Old execution and exact binding",
  down: "dcserver evidence lost",
  "new-server": "New Herdr witness, pane inventory pending",
  "herdr-ready": "Herdr ready, old binding retained",
};
export const state = (v) => {
  if (states[v.phase]) return states[v.phase];
  if (v.q.effects === 1) return v.q.execution === 1 ? "Input sent once to the same execution" : "Input sent once to the new execution";
  if (v.row.state === "pending") return "New execution Pending before create";
  if (v.row.state === "placed") return "New pane recorded";
  if (v.row.state === "fresh-bound") return "New execution Bound";
  if (v.row.state === "retired") return "Old row Retired, hold cleanup";
  const execution = {
    idle: "Read the exact old binding",
    prepare: "RootReplaced candidate, policy preparation",
    capture: v.startupWitness === v.witness ? "Current startup evidence available" : "Current startup evidence absent",
    prove: "New target copies current evidence",
    retire: "Exact policy and end proof pass",
    settled: "Same root and provider remain alive",
    held: v.recovery.reason === "PolicyUnverified" ? "Held: current server policy unverified" : "Held: execution or quiet unverified",
  }[v.recovery.stage];
  const input = { absent: "no input", queued: "input queued", inflight: "old binding claimed", held: "input held, inflight retained", done: "input sent" }[v.q.state];
  return `${execution}; ${input}`;
};
export const group = (v) => v.q.effects === 1 ? "Complete" : v.recovery.stage === "held" ? "Held" : "Recovery";
export const colors = { Recovery: "blue", Held: "pink", Complete: "teal" };
const labels = {
  DcRestart: "dcserver restarts with empty process cache",
  HerdrReplaces: "Herdr starts with a different server witness",
  PaneRootReplaces: "the pane inventory has a different root",
  HerdrSurvives: "Herdr and the exact pane survive",
  Reconcile: "restart reconciliation completes",
  ObserveExecution: "reader compares the exact pane root",
  UseExactGrant: "reuse the exact current-witness grant",
  PreparePolicy: "verify peer, config and server startup provenance",
  LegacyPreparePolicy: "legacy server-replaced recovery observes policy",
  SkipPreparation: "installed owner has no policy producer",
  CaptureTarget: "capture a new target after policy preparation",
  StrictEndProof: "check policy, lone shell, originals and server witness",
  RetireReplacedBound: "recheck current row, quiet and final proof, then CAS",
  QuietHeld: "current-generation inflight prevents retirement",
  ReleaseOldHold: "finish the old hold cleanup",
  RetryReader: "next reader pass retries the retained candidate",
  AcceptInput: "accept the original input",
  ClaimOldBinding: "claim a turn against the old binding",
  RejectOldBinding: "old pane check refuses input and retains custody",
  SettleRestartedInflight: "settle eligible inflight from an older generation",
  SendSameRoot: "send once to the exact live execution",
  StartLaunch: "reserve Pending on the retired row",
  CreateOk: "create one physical pane with a current create permit",
  RecordEvidenceAndBind: "record evidence and bind the fresh execution",
  SendFresh: "send the original input once",
  Done: "complete",
};
export const step = (action) => labels[action] ?? action;
