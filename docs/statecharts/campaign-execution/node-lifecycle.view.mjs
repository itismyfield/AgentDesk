export const title = "One campaign node from start to confirmed completion";
export const state = v => {
  if (v.badEntry) return "A stage was entered without its current approval";
  if (v.dup) return "The same next action took effect twice";
  if (v.retried) return "A node that spent its budget got another Work";
  if (v.control === "prepared") return "Prepared; public start stays unavailable";
  if (v.control === "stopped") return "Stopped; the open attempt is interrupted";
  if (v.control === "succeeded") return "Complete on confirmed facts";
  if (v.failed) return "Failed: the repair budget is spent";
  if (v.stage === "requirements") return "Waiting to enter the first Work";
  if (v.stage === "work") return v.open ? `Work running (repair cycle ${v.cycle})` : `Work to assign (repair cycle ${v.cycle})`;
  if (v.stage === "review") return "Review running";
  if (v.stage === "landing") return "Reviewed; waiting at the Landing door";
  return v.facts ? "Landing admitted; facts confirmed" : "Landing admitted; waiting for main and acceptance facts";
};
export const step = action => ({
  BeginRun: "the server starts the run",
  Approve: "the coordinator records an approval of the current candidate",
  Compute: "the driver computes the next action from the saved state",
  Apply: "the server applies the action at its position",
  Replay: "a retransmitted action is applied again",
  Crash: "the driver restarts and loses its in-flight action",
  Outcome: "the attempt reports pass or quality failure",
  Confirmed: "main integration and acceptance are recorded",
  Stop: "the run is stopped",
  Edit: "the definition is edited while stopped",
  Resume: "the same run is resumed",
})[action] ?? null;
export const group = v => v.failed ? "failed" : v.control === "succeeded" ? "done" : v.control === "running" ? "running" : "idle";
export const colors = {failed:"red", done:"teal", running:"blue", idle:"gray"};
