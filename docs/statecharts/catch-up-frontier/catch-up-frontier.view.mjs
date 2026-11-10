export const title = "Catch-up cursor over a V2 queue channel";
const words = {
  queued: "waiting",
  attempt: "being sent",
  retained: "owed after an unclear send",
  settled: "delivered",
  lost: "dropped, not sent",
};
const next = (v) => {
  const input = v.st[v.cursor + 1];
  return input === undefined ? "every input behind it" : `next input ${words[input]}`;
};
export const state = (v) => `cursor ${v.cursor}: ${next(v)}`;
export const group = (v) => (v.cursor === 0 ? "Stuck" : "Moved");
export const colors = { Stuck: "pink", Moved: "teal" };
const steps = {
  Dispatch: "an input is sent",
  Deliver: "the send is confirmed",
  Unconfirm: "the send is unclear",
  Requeue: "an owed input is queued again",
  Drop: "a waiting input is dropped",
  Scan: "the periodic scan runs",
  Done: "nothing left to do",
};
export const step = (action) => steps[action] ?? action;
