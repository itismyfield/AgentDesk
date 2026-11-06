export const title = "Input notice ownership and accepted iMessage echo";
export const state = (v) => v.notice?.[3] === "Excluded" ? "Direct twin falsely excluded" :
  v.echoed?.length ? "Durable echo delivered" :
  v.outbox?.length ? "Durable echo pending" : "Native notice and HTTP admission";
export const step = (action) => ({ Register: "register original attempt",
  Capture: "materialize native rows", Census: "complete original source census",
  ObserveReceipt: "observe exact user UUID", Attribute: "pair unoccupied attempt",
  Settle: "post direct input or exclude owned input", Http: "start or deliver result",
  Enqueue: "enqueue accepted input once", Send: "deliver durable echo",
  Replay: "return replay without enqueue", Restart: "preserve durable records" })[action] ?? action;
