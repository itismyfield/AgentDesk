// Base fdd44b59c7: a readiness wait whose transcript moves once and then stops.
export const title = "Readiness wait after a daemon restart: the transcript moves once, then stops";
export const state = (v) => {
  const bad = v.bad ?? [];
  if (bad.includes("key")) return "RED: keys sent without evidence";
  if (bad.includes("open")) return "RED: retained over an open turn or fence";
  if (bad.includes("busy")) return "RED: retained over a busy screen";
  const turn = { idle: "pane idle", unrecorded: "person's turn on screen only", running: "turn running", fenced: "fence open" }[v.turn];
  const moved = v.moved ? ", transcript moved" : "";
  return {
    restarted: `Daemon restarted, evidence lost (${turn})`,
    queued: `Owner input at the front (${turn})`,
    waiting: `Readiness wait (${turn}${moved})`,
    requiet: `Bounded re-evaluation, window ${v.round + 1} (${turn}${moved})`,
    held: v.proof ? "Held, proof arrived" : `Held with no wake (${turn}${v.marker ? ", session marker" : ""})`,
    retained: "Retained unsent, no key",
    explained: "Resend notice posted, front released",
    done: "Delivered once",
  }[v.phase];
};
export const step = (action) => ({
  OwnerInput: "owner takes the input",
  StartWait: "wait starts, Stamp kept",
  Deliver: "current evidence: keys sent",
  Timeout: "budget ends",
  WindowEnds: "window ends",
  Wake: "held watch reads a proof",
  Notice: "resend notice",
  MetaMoves: "pane appends metadata",
  PersonSubmits: "person presses Enter",
  LateRecord: "turn record lands",
  TurnEnds: "turn or fence ends",
  Quiet: "nothing more happens",
}[action] ?? action);
