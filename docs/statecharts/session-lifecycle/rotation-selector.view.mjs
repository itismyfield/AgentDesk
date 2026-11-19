export const title = "Channel selector after an in-pane /clear";
const names = { old: "old session", new: "new session", none: "empty", x: "newer selector", null: "empty", "-": "not started" };
export const state = (v) =>
  `cache ${names[v.cache]}${v.cleared ? " (cleared)" : ""}, stored ${names[v.db]}, rotation ${v.rotation}, next turn ${names[v.started]}`;
export const group = (v) => (v.started === "old" ? "ResumedOld" : v.rotation === "pending" ? "Pending" : "Settled");
export const colors = { ResumedOld: "pink", Pending: "yellow", Settled: "teal" };
const steps = {
  Adopt: "the hook moves the pane to the new session",
  FrozenFinish: "the frozen turn finishes and releases the queue",
  FixedSettle: "the settle pass moves the selector, then releases",
  BeforeRelease: "the settle pass releases the queue",
  BeforeRetire: "the settle pass moves the selector and retires",
  TurnStart: "the queued input's turn starts",
  Clear: "the user runs !clear",
  DbNewer: "a newer selector is stored",
  StoreDown: "the selector store fails",
  StoreUp: "the selector store recovers",
  Restart: "dcserver restarts",
};
export const step = (action) => steps[action] ?? action;
