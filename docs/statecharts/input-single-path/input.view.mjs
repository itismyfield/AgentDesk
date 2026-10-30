// Reader's words for InputPath.tla: one Claude TUI channel where every Discord input goes
// through the owner line and one drive task (read the composer, C-s, paste, Enter, settle).
export const title = "One input path for a Claude TUI channel, issue 7272 (landed code + 4f design)";

const busy = (v) => (v.prov === "busy" ? "Claude busy" : "Claude idle");
const drive = (v) => Object.values(v.drv).find((pc) => pc !== "idle");

export const state = (v) => {
  const marks = [];
  if (v.harm === "mixed") marks.push("draft sent with an input");
  if (v.harm === "lost") marks.push("draft overwritten");
  const tail = marks.length ? `, ${marks.join(", ")}` : "";
  if (v.booting) return `Server restarting, line kept on disk${tail}`;
  switch (drive(v)) {
    case "reserved": return `Input taken, composer being read (${busy(v)})${tail}`;
    case "direct": return `Ready to paste without C-s${tail}`;
    case "stash": return `Ready to park the draft with C-s${tail}`;
    case "keyed": return `Keys started, pasting${tail}`;
    case "pasted": return `Pasted, Enter next${tail}`;
  }
  if (v.head === "MayHaveSent") return `Maybe sent, nobody watching it${tail}`;
  if (v.head === "Unknown") return `Write uncertain, waits for the deadline${tail}`;
  if (v.queue.length > 0 || Object.keys(v.queue).length > 0)
    return v.heldWait ? `Held, waits for the hold to clear${tail}` : `Inputs wait, no drive running (${busy(v)})${tail}`;
  return `No input waiting (${busy(v)})${tail}`;
};

export const group = (v) => {
  if (v.booting) return "Restart";
  if (drive(v)) return "Driving";
  if (v.head !== "none") return "Unsure";
  return "Waiting";
};

export const colors = { Driving: "teal", Unsure: "pink", Waiting: "blue", Restart: "orange" };

const labels = {
  Arrive: "a Discord input is admitted",
  DriveFindsNothing: "the drive wakes and finds nothing it may take",
  DriveTakesFront: "the drive takes the first input",
  Prepare: "the drive reads the composer",
  ArmAndKey: "the attempt is armed (C-s if parking)",
  Paste: "the input is pasted",
  WriteUncertain: "the paste or Enter is uncertain",
  Enter: "Enter is applied, settled as sent",
  DeadlinePasses: "30 s pass",
  Backstop: "the backstop retains it with a notice",
  Restart: "the server restarts",
  Boot: "boot recovery runs",
  Absorb: "Claude takes the queued prompt into this turn",
  TurnEnds: "the turn ends",
  Stop: "you /stop the turn",
  HumanTypes: "you type a draft",
  ModalOpens: "a modal opens",
  ModalCloses: "the modal closes",
  HoldRetries: "the hold clears",
  LegacyStarts: "a non-owner start runs the same input",
  Done: null,
};

const nonEmpty = (set) => (Array.isArray(set) ? set.length : Object.keys(set || {}).length) > 0;

export const step = (action, from, to) => {
  if (action === "Prepare") {
    const pc = Object.values(to.drv).find((p) => p !== "idle");
    if (pc === "stash") return "the drive sees a draft";
    if (pc === "direct")
      return nonEmpty(from.comp)
        ? "the drive pastes behind text in the composer [unread, or no C-s allowed]"
        : "the composer reads empty or cannot be read";
    return "the drive is held [modal, or unread when that holds]";
  }
  return action in labels ? labels[action] : action;
};
