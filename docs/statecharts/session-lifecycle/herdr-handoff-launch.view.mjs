// Companion projection of HerdrHandoffLaunch; the chart is hand-authored.
export const title = "Herdr restart: bounded launch failure and visible input custody";
export const state = (v) => {
  if (v.early) return "Early retry (RED)";
  if (v.wait > 0) return `Persisted backoff; failures=${v.failures}`;
  if (Object.values(v.retained ?? {}).some((n) => n > 0)) return "Original retained; notice owed once";
  return { root: "Bound on old server", input: "RootReplaced: Retired", prepare: "First input / restore journal", launch: v.journal ? "Ended journal: launch" : "Retired: ordinary launch" }[v.phase] ?? v.phase;
};
export const group = (v) => v.early ? "Violation" : v.wait > 0 ? "Backoff" : "Progress";
export const colors = { Violation: "pink", Backoff: "blue", Progress: "teal" };
export const step = (action) => action;
