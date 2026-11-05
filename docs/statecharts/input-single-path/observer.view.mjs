export const title = "An idle input waits for its observer, issue 7676 F6";
export const state = ({ observer: o }) => {
  if (!o.initialPending) return "No queued input";
  if (!o.pending) return "Input delivered once";
  if (o.running) return o.observed ? "Owner handoff has its observer" : o.attached ? "Handoff captured no observer, source now attached" : "Owner handoff has no observer";
  if (o.turnHeld) return o.wake ? "Refusal settled, attachment kickoff waits for turn release" : "Refusal settled, bridge still holds the turn token";
  if (o.attached) return o.wake ? "Observer attached, owner wake pending" : "Observer attached, input stranded";
  if (o.rejected) return o.wake ? "Refused before keys, owner wake pending" : "Refused before keys, claim still outstanding";
  return "Owner wake before observer attaches";
};
export const group = ({ observer: o }) => !o.pending ? "Done" : o.attached && !o.wake && !o.running && !o.turnHeld ? "Stuck" : "Waiting";
export const colors = { Done: "teal", Stuck: "pink", Waiting: "blue" };
const labels = {
  ObserverDriveGrants: "the owner drive grants the waiting front",
  ObserverIdleUnobserved: "idle submit refuses without an observer, before any key",
  ObserverTurnReleases: "the bridge releases its turn token and the existing latch completes",
  ObserverSubmits: "the observed owner handoff delivers the input once",

  ObserverDone: null,
};
export const step = (action, from, to) => {
  if (action === "ObserverAttaches") {
    if (from.observer.attached) return "the same observer source registers again [no added wake]";
    const added = to.observer.attachWakes - from.observer.attachWakes;
    return added === 0 ? "a readable observer source attaches without waking the owner"
      : added === 1 ? "a readable observer source attaches and wakes the waiting owner once"
      : "a readable observer source attaches and adds two owner wake requests";
  }
  return action in labels ? labels[action] : action;
};
