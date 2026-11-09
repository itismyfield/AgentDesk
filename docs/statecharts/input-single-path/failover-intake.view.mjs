export const title = "A Discord message kept across a gateway handback";
export const state = (v) => v.runs.standby + v.runs.preferred > 1 ? "Answered twice" :
  v.queue.standby || v.queue.preferred ? "Input waits in a node's queue file" :
  v.receipt ? "Answered; completed receipt shared" : "Awaiting the message";
export const step = (action) => ({ Arrive: "message posts while the preferred node is down",
  CatchUp: "lease holder's catch-up queues it", Handback: "lease moves; queue file stays",
  Deliver: "node runs the front; its turn records the receipt",
  RetireSettledElsewhere: "completed receipt retires the front without a turn" })[action] ?? action;
