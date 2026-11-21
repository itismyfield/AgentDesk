export const title = "Antigravity child shadow ownership";
export const state = ({ agy }) => {
  if (!agy.selected) return "Outside writer selection";
  if (agy.late) return "Late body posted";
  if (agy.retired) return "Retired";
  if (agy.owners.length > 1) return "Both senders own the body";
  if (!agy.verified) return "Parent not yet verified";
  if (agy.boot === "ready") return "Ready";
  if (agy.prior) return "Waiting for the prior Legacy receipt";
  return agy.generation === 1 ? "Checking custody" : "Checking restored custody";
};
export const step = (action) => ({
  OwnerVerify: "parent relationship verified",
  OwnerLegacyReceipt: "prior Legacy turn confirmed",
  OwnerBoot: "spool scan completed",
  OwnerAdopt: "one body owner resolved",
  OwnerRestart: "host restarted",
  OwnerRetire: "child retired",
  OwnerLateBody: "late body passed the fence",
})[action] ?? action;
