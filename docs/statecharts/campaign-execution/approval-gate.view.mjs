export const title = "Campaign approval stays bound to its current gate";
export const state = v => {
  if (v.wrongBinding) return "Approval with a wrong binding accepted";
  if (v.omittedCleared) return "Old form silently cleared required approvals";
  if (v.accepts > 1) return "The same gate accepted twice";
  if (!v.configured) return "All six approvals default OFF";
  if (v.required.length === 0) return "Explicit edit switched approvals OFF";
  if (v.gateState === "absent") return "Approvals configured; no gate open";
  if (JSON.stringify(v.current) !== JSON.stringify(v.bound)) return "Current binding changed; old gate cannot approve";
  return v.gateState === "approved" ? "Gate approved once; duplicates are no-ops" : "Gate open for an explicit approval";
};
export const step = action => ({
  ConfigureGates: "explicitly configure the approval slots",
  OpenGate: "the server opens the exact current gate",
  ChangeCurrentBinding: "run, node, gate, revision, target or policy changes",
  RestoreTargetBinding: "reopen the original target and keep its approval record",
  ApproveGate: "record the coordinator's explicit approval",
  EditWithOmittedOptions: "an old form omits the configured approval options",
  EditWithExplicitOptions: "edit the definition with explicit approval options",
})[action] ?? null;
export const group = v => v.gateState === "approved" ? "approved" : v.configured ? "waiting" : "default";
export const colors = {approved:"teal", waiting:"orange", default:"blue"};
