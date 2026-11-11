export const title = "Before-effects authority: Current / public receipt d1 / keyed scope r2";
export const state = ({s}) => `${s.row.status}; claim=${s.row.claim}; epoch=${s.epoch}; effects=${s.effects}; ${JSON.stringify(s.a)}`;
export const group = ({s}) => s.badAfterEffects ? "Unsafe" : s.row.marker === "rearm" ? "Rearmed" : "Authority";
export const colors = { Authority: "blue", Rearmed: "teal", Unsafe: "pink" };
export const step = (action) => action;
