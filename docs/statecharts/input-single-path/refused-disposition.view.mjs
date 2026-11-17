// What stays of an input after its readiness wait ended without evidence.
export const title = "An input refused for good: its body, its one notice, a restart and a ticket retry";
export const state = (v) => {
  if (v.returned && v.place === "items") return "RED: the settled input is back in the queue";
  if (v.outbox > 1) return "RED: the notice posted twice";
  const notice = v.told ? "notice posted and marked" : v.outbox ? "notice in the outbox, not marked" : "notice owed";
  return {
    head: "Reserved head, wait ended without evidence",
    retained: `Retained with its body (${notice})`,
    refused: `Refused for good, no body (${notice})`,
    items: "Back in the queue",
    sent: "RED: sent again without a resend",
  }[v.place];
};
export const step = (action) => ({
  Settle: "owner settles the head",
  Post: "drain posts the notice",
  Mark: "owner marks the notice",
  Restart: "daemon restarts: requeue, then drain",
  Retry: "caller retries its ticket",
  Drive: "drive submits the queue front",
  Quiet: "nothing left to do",
}[action] ?? action);
