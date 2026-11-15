// Base 9b42e53297; projection of #8161: the daemon restarts over a live idle Claude pane.
export const title = "Daemon restart over an idle herdr Claude pane: first input";
export const state = (v) => {
  const r = v.inputReadiness ?? v.r ?? {};
  if (r.badIdleRunning) return "RED: input over a running turn";
  if (r.badEarly) return "RED: input before the new incarnation started";
  const q = Object.values(r.q ?? {});
  const sent = Object.values(r.effects ?? {}).some((n) => n > 0);
  if (sent) return "Delivered";
  if (r.restarts > 0) return r.phase === "Running" ? "Before restart: turn running" : "Before restart: idle";
  if (r.lag === "both") return "Restarted: earlier send unrecorded, durable in-flight";
  if (r.lag === "transcript") return "Restarted: person's Enter unrecorded, screen running";
  if (r.phase === "Booting") return "Restarted: new incarnation booting";
  if (q.every((s) => s === "retained")) return "Every input retained (resend asked)";
  if (q.includes("retained")) return "One input retained, next waits";
  if (r.restored === "floor") return r.draft === "Person" ? "Idle behind the restart floor, person draft" : "Idle behind the restart floor";
  return r.phase === "Running" ? "Running after the floor passed" : "Idle, floor passed";
};
export const step = (action) => action;
