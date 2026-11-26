export const title = "Parent endpoint consent and exact input custody";
export const state = ({ s }) => {
  if (s.phase !== "applied") return s.phase;
  if (s.effects > s.effectsAtApply) return "Applied again after replay";
  if (!s.ready) return "Applied receipt after restart";
  const released = Object.values(s.releases).reduce((sum, count) => sum + count, 0);
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
})[action] ?? action;
