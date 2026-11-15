export const title = "Session host: same execution after dcserver restart";
export const state = v => {
  if (v.terminated) return "Terminated";
  if (v.srv === "down") return "Daemon down; host owns execution";
  if (v.srv === "boot") return "Boot reconciliation";
  if (v.flight === "closing") return "Close requested; row retained";
  if (v.stop) return "Stop awaiting confirmed absence";
  if (v.recoverOwed) return "Waiting for exact execution adoption";
  if (v.row === "Legacy") return "No hosted execution";
  if (v.row === "Pending") return v.flight === "created"
    ? "Created; durable location not written" : "Pending; reconcile before another create";
  if (v.row === "Retired") return "Retired after absence proof";
  return "Bound; same execution adopted";
};
export const group = v => v.recoverOwed ? "Recovery" : v.stop ? "Termination" : "Session";
export const colors = { Recovery: "blue", Termination: "pink", Session: "teal" };
