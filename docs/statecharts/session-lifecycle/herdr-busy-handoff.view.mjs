// Implementation code SHA bc5cd87fa24778737d23d98b4f959c19e1d42025; documentation-only follow-up.
// Hand-authored projection of HerdrBusyHandoff slice A; see the TLC report.
export const title = "Claude forward handoff: exact pane close, composer hold, boot recovery";
export const state = (v) => {
  if (!v.up) return `Restart during ${v.phase}`;
  if (!v.actor && v.journal) return "Journal-only actor needs boot census";
  if (!v.awake) return "Actor needs boot wake";
  const inputs = Object.values(v.q ?? {});
  if (inputs.includes("blocked")) return "Active intake missed Begin (RED)";
  if (inputs.includes("retained")) return v.crashes > 0 ? "Main crash contract: retained Unconfirmed" : "Crash-free retained leak (RED)";
  if (inputs.includes("uncertain")) return "Restored Unknown: exact receipt or main retention";
  if (inputs.includes("held")) return "Typed no-key hold; existing composer wake";
  if (inputs.includes("may")) return "MayHaveSent committed before backend effect";
  if (inputs.includes("sent")) return "Effect happened; receipt settlement pending";
  return {
    legacy: "Probe active or idle intake",
    revalidate: "Revalidate owner grant and old identity",
    closing: "Close exact pane on one live control connection",
    "held-close": "Backoff retry; original Held and old keys zero",
    checking: "Close acknowledged; verify provider absence",
    ended: "Read final durable native ID and resume guards",
    bound: "Existing composer and Person draft guard",
    complete: "Originals settled or retained; journal complete",
    stuck: "Main unread / retained failure (RED)",
  }[v.phase] ?? v.phase;
};
export const group = (v) => !v.up || !v.awake ? "Restart" : v.phase === "held-close" ? "Held" : "Forward";
export const colors = { Restart: "pink", Held: "blue", Forward: "teal" };
export const step = (action) => action;
