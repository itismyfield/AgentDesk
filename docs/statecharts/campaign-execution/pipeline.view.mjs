export const title = "One job's free-edit pipeline instance";
const one = f => (f && typeof f === "object" && "j1" in f) ? f.j1 : f;
export const state = v => {
  const g = v.ghost ?? {};
  if (g.dup) return "One visit committed two exits";
  if (g.prio) return "A later transition overtook an Unknown one";
  if (g.appr) return "An exit used another visit's approval";
  if (g.intent || g.launch) return "A new effect began after the stop";
  if (g.reentry) return "Adoption alone re-entered the state";
  const s = one(v.state), e = one(v.eff)?.st, f = one(v.facts) ?? {};
  if (s === "none") return v.control === "prepared" ? "Prepared; state_id is null" : "Run started; job not yet assigned";
  if (s === "done") return "Reached the success end";
  if (s === "failed") return "Reached the failure end";
  if (v.control === "stopped") return `Stopped at ${s}`;
  if (v.control === "stopping") return `Stopping (${v.mode}) at ${s}`;
  if (e === "intent" || e === "launching" || e === "unknown") return `${s}: entry effect committed, not yet started`;
  if (e === "started") return `${s}: effect running`;
  if (e === "done") return f.a === "U" || (f.a === "F" && f.b === "U") ? `${s}: waiting for facts` : `${s}: an exit is decidable`;
  return `${s}: no effect`;
};
export const step = action => ({
  BeginRun: "Start pins the saved graph",
  StartJob: "the real Start assigns the start state",
  Observe: "a fact of this visit arrives",
  TimerFire: "the timeout exit becomes true",
  Compute: "a worker evaluates the saved state",
  Commit: "one exit of this visit is committed with the next intent",
  Drop: "a stale computation is dropped",
  HostLaunch: "the host gate initiates the effect",
  Receipt: "the started receipt arrives",
  Finish: "the effect finishes",
  Approve: "the gate of this visit is approved",
  RequestStop: "a stop is requested",
  Settle: "the stop settles",
  Resume: "the run resumes",
  SaveDelete: "an edit deletes an unoccupied state",
  Validate: "the edit is validated",
})[action] ?? null;
export const group = v => {
  const s = one(v.state);
  return s === "done" ? "done" : s === "failed" ? "failed" : v.control === "running" ? "running" : "idle";
};
export const colors = {failed: "orange", done: "teal", running: "blue", idle: "tan"};
