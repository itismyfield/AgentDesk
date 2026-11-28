export const title = "Catch-up cursor over a V2 queue channel";
const words = {
  queued: "waiting",
  attempt: "being sent",
  unsent: "retained unsent, notice owed",
  unconfirmed: "retained unconfirmed, notice owed",
  toldUnsent: "retained unsent, resend notice enqueued",
  toldUnconfirmed: "retained unconfirmed, notice enqueued",
  clearedUnsent: "cleared unsent, still reads as owed",
  clearedUnconfirmed: "cleared unconfirmed",
  settled: "settled",
  lost: "dropped and recoverable",
};
export const inputLabel = (input) => words[input];
export const state = (v) => {
  const input = v.st[v.cursor + 1];
  return input === undefined ? `cursor ${v.cursor}: every input behind it`
    : `cursor ${v.cursor}: ${inputLabel(input)}`;
};
export const group = (v) => (v.cursor === 0 ? "Before" : "Advanced");
export const colors = { Before: "pink", Advanced: "teal" };
const steps = {
  Dispatch: "the owner attempts the input",
  Deliver: "the owner confirms delivery",
  RetainUnsent: "the owner retains the unsent input",
  Unconfirm: "the confirmation window ends",
  Tell: "the owner records the enqueued notice",
  Requeue: "the owner requeues an eligible unsent input",
  Drop: "a waiting input is dropped",
  Clear: "the owner clears waiting inputs",
  Scan: "catch-up scans",
  Done: "nothing left to do",
};
export const step = (action) => steps[action] ?? action;
