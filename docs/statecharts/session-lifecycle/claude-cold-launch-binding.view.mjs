export const title = "Herdr Claude cold launch and first queue attempt";

export const state = v => {
  const result = {
    eligible: "Queue attempt passed the source gate",
    noPin: "Queue attempt held for missing source pin",
    unbound: "Queue attempt held while the row is Pending",
    waiting: "No queue attempt",
  }[v.queue];
  switch (v.read) {
    case "idle":
      return v.hooks === "none" ? "Execution launched" : "Launch SessionStart observed";
    case "submitted":
      if (v.file) return "First transcript record written";
      if (v.hooks === "prompt") return "Both launch hooks observed before transcript";
      return "Pending after SessionStart";
    case "reading":
      if (v.queue !== "waiting") return result;
      if (v.pin) return v.row === "bound" ? "First turn Bound with verified pin" : "First turn Pending with verified pin";
      return v.row === "bound" ? "First turn Bound without verified pin" : "First turn Pending without verified pin";
    case "endedNoFile":
      return "Turn ended before transcript creation";
    case "endedEarly":
      return v.row === "bound" ? "Turn ended early, row Bound" : "Turn ended early, row Pending";
    case "ended":
      return "Turn ended, row Bound";
  }
};

export const step = action => ({
  SessionStart: "SessionStart arrives",
  Attach: "attach to the launched pane",
  UserPromptSubmit: "UserPromptSubmit arrives",
  WriteTranscript: "Claude writes the first record",
  EndBeforeFile: "turn ends before the transcript exists",
  StartRead: "first progress becomes observable",
  TryQueueInjection: "queue attempts busy injection",
  EndRead: "turn finishes and hands off",
  EndEarly: "read ends before handoff",
})[action];
