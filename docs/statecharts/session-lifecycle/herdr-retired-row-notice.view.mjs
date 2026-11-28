export const title = "Notice for a retired Herdr execution";
export const state = v => `${v.phase} · episode ${JSON.stringify(v.key)} · ${v.restarted ? "restarted" : "same process"}`;
export const step = a => ({
  Retire: "Confirm execution end",
  LiveTick: "Start live row end pass",
  Publish: "Discord acknowledges notice",
  DeliveryFailed: "Keep the row after delivery failure",
  UseReceipt: "Use recorded delivery",
  SettleFailed: "Keep the row after settlement failure",
  SettleSucceeded: "Release the turn",
  AdvanceSave: "Advance the row's save generation",
  Restart: "Lose the live actor and its receipts",
  NewActor: "Start a new turn after restart",
})[a] ?? a;
