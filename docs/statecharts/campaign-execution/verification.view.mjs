export const title = "Server campaign: one verification run under stop, interruption and restart";

const phases = (v) => Object.values(v.act);
const check = (v) => {
  if (v.vstate === "collected_pass") return "check passed";
  if (v.vstate === "collected_fail") return "check failed";
  if (v.vstate === "collected_int") return "check interrupted";
  if (v.vstate === "cancelled") return "check cancelled";
  const p = phases(v);
  if (p.includes("unknown")) return "start unresolved";
  if (p.includes("started")) return "check process running";
  if (p.includes("pending")) return "start requested";
  if (p.every((a) => a === "reconciled")) return "result recorded";
  if (p.includes("reconciled")) return "check partly done";
  return "check not started";
};
const run = (v) =>
  v.control === "stopped" ? "Stopped"
    : v.control === "running" ? (v.interrupted ? "Running, task interrupted" : "Running")
    : v.stop === "finish" ? "Stopping after this stage" : "Stopping now";

export const state = (v) => `${run(v)}: ${check(v)}`;
export const group = (v) => (v.control === "running" ? "Running" : v.control === "stopped" ? "Stopped" : "Stopping");

const steps = {
  Pend: "the server commits to starting a check phase",
  GateInitiate: "the gate starts the check process",
  GateRefuse: "the gate refuses an outdated start",
  ClassifyUnsent: "the gate confirms the phase never started",
  RecordStart: "the server records the start receipt",
  RecordResult: "the server records the phase result",
  RecordIndet: "the server records a start it cannot confirm",
  RecordIndetSettled: "the unconfirmed start ends as an interruption",
  Settle: "the server collects or closes the check",
  BeginReview: "the work result is accepted and review begins",
  AcceptWork: "the work stage result is decided",
  AcceptReview: "the review stage result is decided",
  CloseDeadFinish: "the finishing attempt is closed as interrupted",
  ConfirmInterrupted: "the interrupted task is confirmed quiet",
  Interrupt: "you stop this one task",
  RequestStop: "you stop the campaign",
  Escalate: "you change the stop to stop now",
  ConfirmStopped: "everything is quiet, the campaign is stopped",
};
// Hidden: bindings, claims, the launch hand-off to the gate, fence bookkeeping, process exit, delivery, crash and restart, idle.
export const step = (action) => steps[action] ?? null;

export const colors = { Running: "blue", Stopping: "orange", Stopped: "teal" };
