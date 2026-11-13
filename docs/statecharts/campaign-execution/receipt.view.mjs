export const title = "Campaign command: locked decision and atomic receipt";
const phases = {begin:"ready",waiting:"waiting for the campaign lock",lookup:"reading the saved receipt",facts:"reading locked facts",decide:"deciding from locked facts",persist:"writing state and evidence in the transaction",render:"rendering the saved response",receipt:"writing the receipt in the transaction",commit:"ready to commit",replay:"replaying the saved response",done:"completed",rolled_back:"rolled back"};
export const state = (v) => {
  const r = v.receiptTx;
  if (!r.server) return "Server stopped, committed rows kept";
  const owner = Number(r.owner);
  const phase = owner ? phases[r.pc[owner]] : Object.values(r.pc).every(p => p === "done" || p === "rolled_back") ? "both calls finished" : "no caller holds the lock";
  const shownPhase = r.earlyFacts && r.pc[owner] === "decide" ? "deciding from a snapshot read before the lock" : phase;
  const bad = r.badFact ? "; decision used facts from outside the lock" : r.badRollback ? "; rollback left a durable effect" : "";
  return `${shownPhase}; durable effect ${r.effects}, receipt ${r.saved === "none" ? "absent" : "saved"}${bad}`;
};
export const group = v => !v.receiptTx.server ? "down" : v.receiptTx.saved === "none" ? "pending" : "committed";
const labels = {RBegin:"a request begins a transaction",RAcquire:"one caller acquires the campaign and run locks",RLookup:"the caller checks the receipt before deciding",RReadFacts:"the caller reads state and stored seal inside the lock",RDecide:"the service decides without releasing the lock",RPersist:"the database stages state and evidence",RRender:"the response is rendered inside the transaction",RWriteReceipt:"the database stages the receipt",RCommit:"the transaction commits and releases the locks",RRollback:"a failure rolls back and releases the locks",RCrash:"the server stops and uncommitted work rolls back",RRestart:"the server reconnects and requests may retry"};
export const step = (action, from, to) => {
  if (action === "RBegin" && to.receiptTx.earlyFacts) return "a request reads facts before acquiring the lock";
  if (action === "RReadFacts" && to.receiptTx.earlyFacts) return "the service keeps the snapshot read before the lock";
  if (action === "RPersist" && to.receiptTx.effects > from.receiptTx.effects) return "state and evidence escape before receipt commit";
  return labels[action] ?? null;
};
export const colors = {pending:"orange",committed:"teal",down:"pink"};
