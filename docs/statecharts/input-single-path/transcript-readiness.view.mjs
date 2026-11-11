// Production SHA 574af580b0c5d6e6ce280994f4ed243463de5d2d; hand-authored projection.
export const title = "Claude Idle readiness, unchanged busy queue, continuation fence";
export const state = (v) => {
  const r = v.inputReadiness ?? v.r ?? {};
  if (r.badIdleRunning) return "RED: Idle-ready submission while Running";
  if (r.badContinuation) return "RED: Idle submit/compact before continuation ended";
  if (r.badDraft) return "RED: person's draft executed";
  if (r.badCompact) return "RED: compact before approved terminal Idle";
  if (Object.values(r.q ?? {}).includes("native-queued")) return "Busy paste+Enter accepted in native queue";
  if (r.continuing) return r.softStage < 4 ? "Suppressed Stop: summary/timer cannot authorize" : "Continuation observed: await later final";
  if (r.phase === "Booting") return "Wait for this launch's SessionStart";
  if (r.head) return "Idle prepared: revalidate before first key";
  if (r.draft === "Unread") return "Held: draft read unavailable; automatic retry";
  if (r.draft === "Person") return "Held: existing person draft policy";
  return r.phase === "Running" ? "Running: existing Busy entry remains available" : "Observe current source, approved terminal and latch";
};
export const step = (action) => action;
