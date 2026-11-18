export const title = "Campaign profile snapshot keeps its execution pin";
export const state = v => {
  if (v.execution !== "unavailable") return "Snapshot opened execution";
  if (JSON.stringify(v.pin) !== JSON.stringify(v.firstPin)) return "Stored pin silently replaced";
  if (v.result === "matching") return JSON.stringify(v.pinInput) === JSON.stringify(v.input) ? "Matching pin accepted; execution unavailable" : "Changed input falsely accepted";
  if (v.result === "drift") return "Old or changed pin reports drift";
  if (Number(v.snapshotInput[1]) === 3) return "No current snapshot";
  if (JSON.stringify(v.snapshotInput) !== JSON.stringify(v.input)) return "Input changed; fresh snapshot needed";
  return Number(v.pin[1]) === 3 ? "Snapshot created; no execution pin" : "Current snapshot awaits pin revalidation";
};
export const step = action => ({
  CreateSnapshot: "construct a snapshot from current source, scope and roles",
  PinSnapshot: "explicitly create a previously absent pin",
  ChangeInput: "source, approved scope or supported roles change",
  RevalidatePin: "revalidate the stored pin against the new profile",
})[action] ?? null;
export const group = v => v.result === "drift" ? "drift" : v.result === "matching" ? "matching" : "pending";
export const colors = {drift:"orange", matching:"teal", pending:"blue"};
