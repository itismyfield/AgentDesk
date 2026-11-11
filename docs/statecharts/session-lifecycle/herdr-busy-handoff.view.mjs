// Design projection for HerdrBusyHandoff; the frontend is absent on this WSL lane.
export const title = "Claude busy tmux to Herdr handoff and the original input";
export const state = (v) => {
  if (v.server === "down") return `Restart during ${v.phase}`;
  if (Object.values(v.q).some((q) => q === "retained")) return "Retained original";
  if (Object.values(v.q).some((q) => q === "recover")) return "Recover exact receipt or NoKeyYet";
  if (Object.values(v.q).some((q) => q === "await-receipt")) return "Effect happened; owner awaits receipt";
  if (Object.values(v.q).some((q) => q === "armed")) return "One destination armed";
  return {
    legacy: "Original queued, old Claude running",
    stopping: "End old execution, independent of composer readiness",
    checking: "Stop dispatched; old absence not yet verified",
    fallback: "Same original on verified old execution",
    ended: "Old provider confirmed absent",
    pending: "Durable Pending launch",
    created: "New provider, same native session",
    ready: "Bound Herdr execution; originals settle once",
  }[v.phase] ?? v.phase;
};
export const group = (v) => (v.server === "down" ? "Restart" : v.phase === "fallback" ? "Fallback" : "Forward");
export const colors = { Restart: "pink", Fallback: "blue", Forward: "teal" };
export const step = (action) => action;
