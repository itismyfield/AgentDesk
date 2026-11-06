export const title = "Watcherless presence: progress is not execution identity";
const phases = {
  begin: "Start a fresh observation",
  "collect-before": "Collect admission inputs",
  "collect-after": "Collect admission inputs again",
  admit: "Compare identity and owner evidence",
  "unbound-before": "Collect ordinary Unbound inputs",
  "unbound-after": "Recheck ordinary Unbound inputs",
  unbound: "Present inflight keeps the channel held",
  "read-open": "Read the native open turn",
  "publication-before": "Collect fresh publication inputs",
  "publication-after": "Collect publication inputs again",
  "publication-pair": "Check fresh and pinned identity",
  "publication-final": "Re-read files and publish if current",
  down: "Process stopped; memory proof discarded",
};
export const state = (v) => `${phases[v.phase] ?? v.phase}: ${v.busy ? "Busy" : "Unknown"}`;
export const group = (v) => v.phase === "down" ? "Restart" : v.phase.startsWith("publication") ? "Publication" : "Admission";
const steps = {
  Progress: "save progress or append the same source",
  ChangeIdentity: "change one execution identity axis",
  UnrelatedEpochBump: "another channel changes the global owner epoch",
  ReadFail: "a required input becomes absent or unreadable",
  ReadRecover: "the required input recovers",
  Retry: "recollect changed admission inputs once",
  Admit: "admit this observation's session",
  RejectAdmission: "fall back to a fresh Unbound observation",
  ObserveUnknown: "hold and advance the continuous Unknown grace",
  ReadOpen: "native reader reports an open turn",
  FinalCheckAndPublish: "compare the final files and publish this Reading",
  Crash: "discard process-local proof and reader",
  Boot: "collect the surviving row under a new process identity",
};
export const step = (action) => steps[action] ?? action;
