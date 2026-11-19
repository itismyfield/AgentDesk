export const title = "Channel selector after an in-pane /clear";
const names = { old: "old session", new: "new session", c: "third session", none: "empty", x: "newer selector", null: "empty", "-": "not started" };
export const state = (v) =>
  `${v.owner === "herdr" ? "Herdr, " : ""}${v.hopped ? "hopped twice, " : ""}${v.front !== "none" ? "queue front held, " : ""}${v.hook === "queued" ? "next hook queued, " : ""}cache ${names[v.cache]}${v.cleared ? " (cleared)" : ""}, stored ${names[v.db]}, rotation ${v.rotation}${v.followed ? " (selector followed)" : ""}, next turn ${names[v.started]}`;
export const group = (v) => (v.started === "old" ? "ResumedOld" : v.rotation === "pending" ? "Pending" : "Settled");
export const colors = { ResumedOld: "pink", Pending: "yellow", Settled: "teal" };
const steps = {
  Adopt: "the hook moves the pane to the new session",
  FrozenFinish: "the frozen turn finishes and releases the queue",
  WatcherSettle: "the settle pass moves the selector, then releases",
  HerdrFollow: "the settle pass moves a watcherless Herdr pane's selector",
  BeforeRelease: "the settle pass releases the queue",
  BeforeRetire: "the settle pass moves the selector and retires",
  TurnRead: "the queued input's turn start reads the selector",
  TurnFinish: "that turn start writes the cache and claims",
  Hook: "the next hook arrives",
  Retry: "the deferred retry pass runs",
  Clear: "the user runs !clear",
  DbNewer: "a newer selector is stored",
  StoreDown: "the selector store fails",
  StoreUp: "the selector store recovers",
  Restart: "dcserver restarts",
};
export const step = (action) => steps[action] ?? action;
