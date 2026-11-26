export const title = "Parent endpoint consent and exact input custody";
export const state = ({ s }) => {
  if (s.discarded.includes("newer")) return "Another generation discarded";
  if (s.phase === "retry" && s.routeFault === "retry") return "Retry lost its parent route";
  if (s.phase === "accepted" && s.routeFault === "live-owner") return "Owner validation lost its parent route";
  if (s.phase !== "applied") return s.phase;
  if (s.effects > s.effectsAtApply) return "Applied again after replay";
  if (Object.values(s.releases).some((count) => count > 1)) return "Original released twice";
  if (!s.ready) return s.routeFault === "recovery"
    ? "Receipt recovery lost its parent route" : "Applied receipt after restart";
  const released = Object.values(s.releases).reduce((sum, count) => sum + count, 0);
  if (s.discarded.length) {
    return released ? "Remaining original released after clear" : "Applied, one original cleared";
  }
  return ["Applied, no originals released", "Applied, one original released",
    "Both originals released"][released];
};
export const step = (action) => ({
  IssueCard: "post card on child",
  CardSendFailed: "card needs retry",
  Retry: "owner retries from an original's channel",
  Decide: "owner approves the exact card",
  ApplyConsent: "apply once on parent endpoint",
  RestartApplied: "restart with the durable receipt",
  ReplayApplied: "replay the receipt",
  ReleaseOriginal: "release one stored original",
  ClearOriginal: "discard the cleared original",
})[action] ?? action;
