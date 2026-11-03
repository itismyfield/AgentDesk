// Reader's words for InputOwnership.tla: one Discord or turn/start input on a Claude tmux channel,
// pasted once, and who answers it chosen from the transcript row it produced.
export const title = "Who answers an input pasted into a Claude pane, issue 7676 A2+D";

const pane = (v) => (v.nat === "busy" ? "Claude busy" : "Claude idle");

export const state = (v) => {
  switch (v.inp) {
    case "queued": return `Input waits for the owner (${pane(v)})`;
    case "held": return `Input held for a later wake (${pane(v)})`;
    case "retained":
      return v.old === "untold" ? "Input an older binary kept, no ask sent yet" : "Input kept, person asked to send again";
    case "failed": return "Turn ended with a failure, input never pasted";
    case "sent": return "Pasted, no row yet";
    case "enq": return "Claude queued it behind the running turn";
    case "absorbed":
      return v.claim === "none" ? "Taken into the running turn" : "Taken into the running turn, answerer known";
    case "newturn":
      if (v.claim === "none") return "Opened its own turn";
      return v.natOwner === "none" || v.natOwner === "adNoObs"
        ? "Opened its own turn, nobody answers it"
        : "Opened its own turn, answerer known";
    case "answered": return "Answer posted";
  }
  return v.inp;
};

export const group = (v) => {
  if (v.inp === "queued" || v.inp === "held") return "Waiting";
  if (v.inp === "retained" || v.inp === "failed") return "Lost";
  if (v.inp === "answered") return "Done";
  return "Delivered";
};

export const colors = { Waiting: "blue", Lost: "pink", Done: "teal", Delivered: "orange" };

const labels = {
  DriveBusy: "the owner pastes into the AgentDesk turn [refused when the transcript is idle, main]",
  DriveIdleBusyPane: "the owner meets a busy pane with no AgentDesk turn",
  DriveIdle: "the owner submits to an idle pane [another account's pane takes the launch route in c]",
  DriveStartBusy: "a turn/start's turn meets a busy pane [read as idle in main and a2d1]",
  ScreenClears: "the modal, copy mode, lock, buffer load or transcript clears, with no event",
  TimerFires: "the drive's fixed recheck fires [main, a2d1]",
  WatchSees: "the held front's watch reads its refusal cleared [a2d3, causes in c]",
  WatchEnds: null,
  WatchMisreads: "one read of the held pane or the queue fails [ends the watch in a2d3]",
  StaleWatchEnds: "an earlier front's watch ends [drops this hold's watch in a2d3]",
  DeclarationAges: "more than 10 s pass before the relay reads the row",
  RowOpensTurn: "a user row opens a new turn",
  RowEnqueued: "Claude writes an enqueue row",
  Absorb: "Claude takes it into the running turn",
  WindowPasses: "the 3 s receipt window passes",
  DriveFindsNothing: "the wake finds nothing the owner may take",
  BootRequeues: "a restart requeues an input kept before its ask went out [slice C]",
  Observe: "the relay reads the input's row",
  TimeHands: "the 3 s window passes with only an enqueue",
  TurnEnds: "the turn ends",
  HumanStartsTurn: "a person starts a turn at the terminal",
  MailboxReleases: "the AgentDesk turn is released",
  CauseClearsBeforeRead: "the cause clears before its watch's first read [slice C]",
  PaneGoes: "the pane goes away while its cause stands [slice C]",
  WatchLosesPane: "the cause's watch reads its pane gone and ends [slice C]",
  Done: null,
};

export const step = (action, from, to) => (action in labels ? labels[action] : action);
