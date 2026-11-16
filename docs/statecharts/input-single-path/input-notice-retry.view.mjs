export const title = "Input notice waits for evidence";
export const state = (v) => v.retryWithoutWake ? "Retried without a wake" :
  v.posted === 1 ? "Delivered once" :
  !v.owed ? "Notice removed from owed" :
  v.delayed ? "Withheld for evidence" : "Owed native input notice";
export const step = (action) => ({
  Select: "select changed evidence; Before erases the hold",
  Deliver: "admit, withhold or settle the notice",
  Rotate: "let the later normal output pass",
  ObserveEvidence: "record native evidence and wake",
  ObserveLedger: "record terminal ledger state and wake",
  Restart: "restore durable acknowledgements and observe once",
  SessionGone: "live session ends; historical evidence stays eligible",
})[action] ?? action;
