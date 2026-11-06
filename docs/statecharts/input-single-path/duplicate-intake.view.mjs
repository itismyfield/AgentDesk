export const title = "Live and catch-up intake of the same Discord message";
export const state = (v) => v.warnings > 0 ? "False queue warning" :
  v.kept.length > 0 ? "Original waits in committed custody" :
  v.receipts.length > 0 ? "Delivered; committed receipt remains" : "Awaiting intake";
export const step = (action) => ({ Receive: "live or catch-up receives message",
  Deliver: "owner delivers original once", Restart: "restart preserves custody" })[action] ?? action;
