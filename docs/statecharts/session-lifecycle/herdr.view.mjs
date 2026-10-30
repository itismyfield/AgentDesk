// Reader's words for HerdrSession.tla: one Discord channel's Claude execution on a Herdr pane.
export const title = "AgentDesk channel session on Herdr, launch to retire, with one server crash";

// The down phase is folded into the crash arrow: a crash resets the in-memory launch, pin and
// terminate, and the boot only reads, so each state below is labelled by what survives.
export const state = (v) => {
  if (v.launch === "creating") return "Launch: create request sent";
  if (v.launch === "placing") return "Launch: pane made, not recorded yet";
  if (v.launch === "evidence") return "Launch: pane recorded, Claude being checked";
  if (v.closing || (v.rec === "bound" && v.pane === "closed")) return "Pane closed, row not retired yet";
  if (v.input === "pinned") return "Prompt checked, about to be sent";
  switch (v.rec) {
    case "pending":
      return v.stray > 0
        ? "Pending row with no pane, an unrecorded pane runs Claude"
        : "Pending row with no pane";
    case "placed":
      return "Pane recorded, Claude not proven";
    case "evidenced":
      return "Launched, not bound yet";
    case "bound":
      if (v.pane === "agent") return "Bound, Claude running";
      return "Bound, Claude exited, shell left in the pane";
  }
  return "No execution running";
};

export const group = (v) => {
  if (v.launch !== "idle") return "Launch";
  if (v.rec === "pending") return "Stuck";
  return "Run";
};

export const colors = { Run: "blue", Launch: "teal", Stuck: "pink" };

const labels = {
  StartLaunch: "a turn starts a launch",
  CreateOk: "Herdr answers with the new pane",
  CreateLost: "the create reply is lost",
  CreateNotSent: "the create request never leaves",
  RecordPane: "the pane is recorded",
  RecordPaneFails: "recording the pane fails",
  RecordEvidence: "Claude is confirmed in the pane",
  EvidenceGap: "Claude is not found in the pane",
  ProbeAgain: "a later turn confirms Claude",
  Bind: "the first prompt binds the execution",
  Pin: "the input gate checks Claude in the pane",
  Send: "the prompt is sent",
  ProviderExits: "Claude exits",
  RowEnd: "the ended execution's row is retired",
  OperatorClose: "an operator terminates the pane",
  OperatorRetire: "the row is retired",
  Crash: "the server crashes and restarts",
  Boot: "the server boots, reconcile only reads",
};

export const step = (action) => labels[action] ?? action;
