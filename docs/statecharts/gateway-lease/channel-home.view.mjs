// Reader's words for ChannelHome.tla: one O channel whose execution node is set per channel.
// "Designated node" is the node the setting names; "gateway" is the node that holds the Discord
// gateway lease and runs the channel when the designated node cannot.
export const title = "One O channel with a per-channel execution node, issue 8486 (revision 4)";

const node = (n) => (n === "D" ? "the designated node" : "the gateway");

const home = (v) => {
  if (v.owner === "none") return "No owner row, the gateway serves";
  return `Home is ${node(v.owner)}`;
};

const setting = (v) => {
  const now = { D: "set to the designated node", G: "set to the gateway", none: "no node set" }[v.want];
  const edited = v.edits > 0 ? `, ${v.edits} file edit${v.edits > 1 ? "s" : ""} so far` : "";
  return (v.yaml.G === v.want ? now : `${now}, the hub's file says otherwise`) + edited;
};

const input = (v) => {
  if (v.q === "arrived") return ", an input waits at the gateway";
  if (v.q === "held") {
    const asked = { none: "", pending: ", old home asked to end", busy: ", old home is busy", ended: ", old home ended", expired: ", old home did not answer" }[v.clean];
    return `, an input is kept for ${node(v.tgt)}${asked}`;
  }
  if (v.q === "admitted") return `, an input is handed to ${node(v.tgt)}`;
  return "";
};

const turns = (v) => {
  const running = ["D", "G"].filter((n) => v.sess[n] === "busy");
  if (running.length === 0) return "";
  const stale = (n) => (v.owner !== "none" && (v.owner !== n || v.stamp[n] !== "cur") ? " (old session)" : "");
  return `, turn runs on ${running.map((n) => node(n) + stale(n)).join(" and ")}`;
};

const idle = (v) => {
  const kept = ["D", "G"].filter((n) => v.sess[n] === "idle");
  return kept.length ? `, idle session on ${kept.map(node).join(" and ")}` : "";
};

const lifecycle = (v) => {
  const dead = ["D", "G"].filter((n) => v.bind[n] === "dead");
  const parts = [];
  if (dead.length) parts.push(`, ${dead.map(node).join(" and ")} cleared its session`);
  if (v.child) parts.push(", thread recorded on the designated node");
  const residue = ["D", "G"].filter((n) => v.res[n] === "old");
  if (residue.length) parts.push(`, ${residue.map(node).join(" and ")} keeps an older tenure's binding log`);
  if (v.qclr) parts.push(", the waiting input was cleared");
  const pending = Object.entries(v.note).filter(([, reserved]) => reserved).length;
  if (pending) parts.push(`, ${pending} notice${pending > 1 ? "s" : ""} waiting in the outbox`);
  return parts.join("");
};

const harm = (v) => {
  const marks = [];
  if (v.refusals > 0) marks.push("refusal shown");
  const fates = Object.values(v.fate);
  const held = (t) => v.qtok === Number(t) || ["D", "G"].some((n) => v.run[n] === Number(t));
  const lost = Object.entries(v.fate).filter(([t, fate]) => fate === "open" && !held(t)).length;
  if (lost > 0) marks.push(`${lost} input${lost > 1 ? "s" : ""} left unanswered`);
  const unsent = Object.entries(v.fate).filter(([t, fate]) => fate === "withheld" && !v.note[t]).length;
  if (unsent > 0) marks.push(`${unsent} withheld input${unsent > 1 ? "s" : ""} with no notice`);
  if (v.misplaced) marks.push("an input ran on the wrong node");
  if (v.selfCut) marks.push("cleanup ended the session the plan keeps");
  if (["D", "G"].some((n) => v.sess[n] === "busy" && v.store[n] === "old" && v.owner === n))
    marks.push("writes from an old store");
  if (Object.values(v.ans).some((count) => count > 1)) marks.push("answered twice");
  if (v.cutBusy) marks.push("busy turn cut");
  if (Object.keys(v.ans).some((t) => v.ans[t] + v.ndel[t] > 1)) marks.push("two effects for one input");
  if (Object.entries(v.fate).some(([t, fate]) => fate === "noticed" && v.ndel[t] === 0)) marks.push("noticed with no notice sent");
  if (v.ranCleared) marks.push("a cleared input ran");
  return marks.length ? ` [${marks.join(", ")}]` : "";
};

export const state = (v) =>
  `${home(v)} (${setting(v)}${v.up ? "" : ", designated node down"})${input(v)}${turns(v)}${idle(v)}${lifecycle(v)}${harm(v)}`;

export const group = (v) => {
  if (v.owner === "none") return "Unmanaged";
  if (v.q === "held" && v.clean !== "none") return "Moving";
  return v.owner === "D" ? "Designated" : "Gateway";
};

export const colors = { Unmanaged: "tan", Designated: "teal", Gateway: "blue", Moving: "pink" };

const labels = {
  EditYaml: "an operator edits one node's config file",
  Sync: "the hub copies its setting to the database",
  DDown: "the designated node stops",
  DUp: "the designated node boots and ends its session of an older owner",
  SeenOffline: "the designated node's heartbeat expires",
  SeenOnline: "the designated node's heartbeat is fresh again",
  Arrive: "you send a message",
  RouteLocal: "the gateway starts the turn itself",
  BeforeSticky: "the gateway keeps the input on its live session",
  BeforeRefuse: "the gateway refuses to place the channel on another node",
  HoldFirst: "the gateway's writer check refuses before routing",
  Stage: "the gateway puts the input into shared custody",
  Replan: "the gateway's tick picks the target again",
  PromoteSame: "the input is handed to the node that owns the channel",
  RequestCleanup: "the old home is asked to end its session",
  Respond: "the old home answers the request",
  Expire: "the request's deadline passes without an answer",
  Transfer: "the owner row moves to the new home",
  Start: "the home starts the turn",
  Post: "the answer reaches Discord",
  Withheld: "the turn ends and its answer is withheld",
  Refresh: "a node reads the owner row again",
  StaleEnd: "the old home ends its idle session",
  Clear: "you run /clear at the gateway",
  Renew: "the home opens a new generation for its next session",
  RecordChild: "the designated node records the thread",
  DropAdmitted: "the input leaves custody with no turn",
  OrphanMove: "the gateway moves the channel off the offline home and reserves a notice",
  IndexRefresh: "the designation index catches up",
  SendNotice: "the notice reaches Discord",
};

export const step = (action) => (action in labels ? labels[action] : action);
