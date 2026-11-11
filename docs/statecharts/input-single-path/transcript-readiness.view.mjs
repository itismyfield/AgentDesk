// Code SHA 574af580b0c5d6e6ce280994f4ed243463de5d2d; hand-authored projection.
export const title = "Claude input: scoped evidence, existing draft veto, one original";
export const state = (v) => {
  const r = v.inputReadiness ?? v.r ?? {};
  if (r.badRunning) return "RED: payload while Running";
  if (r.badDraft) return "RED: person's draft executed";
  if (r.badCompact) return "RED: compact before terminal Idle";
  if (r.phase === "Booting") return "Wait for this launch's SessionStart";
  if (r.head) return "Prepared: revalidate before first key";
  if (r.draft === "Unread") return "Held: draft read unavailable; automatic retry";
  if (r.draft === "Person") return "Held: existing person draft policy";
  return r.phase === "Running" ? "Running: no new follow-up keys" : "Observe current source and turn";
};
export const step = (action) => action;
