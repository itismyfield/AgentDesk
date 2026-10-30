// Reader's words for HostMigration.tla: one Claude channel moving tmux -> Herdr and back, keeping
// its session. "Old tmux Claude" is the tmux Claude the channel ran before the move.
export const title = "Claude channel moving between tmux and Herdr with the same session, issue 7503";

const where = (v) => (v.mapping === "herdr" ? "set to Herdr" : "set to tmux");
const tmux = (v) => (v.tmuxP === "run" ? ", old tmux Claude still runs" : "");

export const state = (v) => {
  const blocked = v.pending > 0 && !v.named && v.mapping === "tmux" ? ", input waits, channel has no name" : "";
  switch (v.row) {
    case "Legacy":
      return v.tmuxP === "run"
        ? `On tmux, Claude running (${where(v)})`
        : `On tmux, no Claude running (${where(v)})`;
    case "Pending":
      return `Herdr launch sent, not bound yet${tmux(v)}`;
    case "Bound":
      if (v.herdrP === "run") return `On Herdr, Claude running (${where(v)})${tmux(v)}${blocked}`;
      return `Herdr Claude ended, row not retired (${where(v)})${blocked}`;
    case "Retired":
      if (v.marker === "herdr" || v.listed)
        return `Row retired, move back unfinished (${where(v)})${tmux(v)}${blocked}`;
      return v.tmuxP === "run"
        ? `Back on tmux, Claude running (${where(v)})`
        : `Back on tmux, no Claude running (${where(v)})`;
  }
  return "?";
};

export const group = (v) => {
  if (v.row === "Legacy" || (v.row === "Retired" && v.marker === "tmux" && !v.listed)) return "Tmux";
  if (v.row === "Pending" || v.row === "Bound") return v.herdrP === "run" ? "Herdr" : "Moving";
  return "Moving";
};

export const colors = { Tmux: "blue", Herdr: "teal", Moving: "pink" };

const labels = {
  UserSends: "you send a message",
  Restart: "the server restarts, maybe with the other host set",
  ForwardEndsTmux: "the old tmux Claude gets /exit and ends",
  ForwardLaunchResume: "Herdr launches with --resume of the same session",
  ForwardLaunchFresh: "Herdr launches a fresh session [no session to resume]",
  Attach: "the launch reports its start and binds the Herdr row",
  HerdrServes: "Herdr answers the message",
  ReturnEndsHerdr: "the Herdr Claude gets /exit and ends",
  ReturnRetires: "the Herdr row is retired",
  ReturnReleases: "the Herdr pane leaves the listing",
  ReturnMarksTmux: "the host marker is set to tmux",
  TmuxLaunch: "tmux launches with --resume of the same session",
  TmuxServes: "tmux answers the message",
  Done: null,
};

export const step = (action) => (action in labels ? labels[action] : action);
