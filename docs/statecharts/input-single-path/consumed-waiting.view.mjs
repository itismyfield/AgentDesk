export const title = "A consumed input survives a restart while the next input waits";
const ownedState = (v) => {
  const c = v.cw;
  if (c.phase === "initial") return "A waits beside a busy Claude turn";
  if (c.phase === "pasted") {
    return "A pasted once; the real busy effect has not returned";
  }
  if (c.phase === "returned") return c.writeSettled ? "The actual busy write succeeded; Submitted settlement is next" : "The busy write is uncertain; its answer observer still runs";
  if (c.phase === "consumed") return c.consumed ? "Submitted is settled; the answer observer awaits the actual user row" : "WriteUncertain is settled; the answer observer still awaits the user row";
  if (c.phase === "observed") return "The existing observer found an actual native user row";
  if (c.phase === "settled") return c.pinVerified ? "The observer persisted A's verified pin" : c.pinSupplier ? "Unrelated or unsettled native evidence gives A no pin" : "The old observer supplies no pin for A";
  if (c.phase === "down") return c.generation === 0 ? "Shutdown marks A for the next process" : "Another shutdown refreshes A's marker";
  if (c.phase === "restoring") return "New runtime restores the live pane";
  if (c.phase === "done") return "B started once; a late A result leaves B intact";
  if (c.readopt === "ok" && v.row === "carried") return "A re-adopted; its live owner must finish it";
  if (!c.terminal) return c.marker ? "A re-adoption refused; old marker remains" : "A re-adoption refused; marker consumed";
  if (!c.nStarted) return "A ended; a later native turn may start";
  if (v.row === "none") return "A row closed; the existing kickoff wakes B";
  if (c.close === "locked") return "Idle health pass holds A's transition lock";
  if (c.close === "committed") return `A's ${c.receiptOutcome} receipt committed; archive checks its captured identity`;
  if (c.token === "BClaim") return "B claims the mailbox; A's closer preserves that claim";
  if (c.pendingB) {
    const retry = (v.wake && c.attemptsB < 2 ? "owner wake available" : "no owner wake remains")
      + (c.generation < 2 ? "; one restart remains" : "; restart bound reached");
    return v.nat === "busy" ? `B waits behind A's row while Claude is busy; ${retry}` : `B waits behind A's row even while Claude is idle; ${retry}`;
  }
  return "A ended; unrelated input-notice debt stays at twenty";
};
export const state = (v) => v.cw.outputOwner === "owned" ? ownedState(v)
  : `${ownedState(v)} [${v.cw.outputOwner} output ownership; O cannot close]`;
const labels = {
  OwnerPastesA: "the owner pastes A once",
  BusyEffectReturns: "the actual busy effect returns and arms only a successful attempt",
  SettleBusySubmission: "the attempt's Submitted or WriteUncertain terminal settles",
  ObserveBusyReceipt: "the unchanged answer observer confirms an actual native user row",
  SupplyBusyPin: "only an armed matching native user is saved through A's birth episode",
  ShutdownRestamps: "planned shutdown refreshes the restart marker",
  SettleBoot: "a new runtime settles boot", RestoreAlive: "the live pane is restored and adoption is decided",
  EndA: "A's canonical terminator is written", NativeStartsN: "a later native turn starts",
  CaptureAndDeliverA: "O captures and delivers A's output; input-notice debt remains",
  FinishReadoptedA: "A's adopted owner finishes its own episode",
  CloseATick: "the idle health pass locks A using its own evidence",
  CommitReceiptA: "A's receipt commits before archive", ArchiveA: "A's identity CAS archives only A",
  KickoffB: "the existing kickoff wakes the waiting owner",
  AdmitB: "turn/start admits B to the waiting queue",
  ClaimB: "the waiting input claims its own mailbox token before create",
  ForeignAdmission: "B's create collides with A; B releases its claim and keeps its front",
  NativeIdle: "the later native turn ends; its result belongs to N",
  StartB: "the owner starts B on an eligible idle edge", LateCloseA: "a late A result fails B's identity guard",
  HealthKeepsA: "the existing health tick keeps A because its output gate still refuses",
  CWDone: null,
};
export const step = (action) => labels[action] ?? null;
export const group = (v) => v.cw.phase === "done" ? "Done" : v.cw.pendingB ? "Waiting" : "Progress";
export const colors = { Done: "teal", Waiting: "pink", Progress: "blue" };
