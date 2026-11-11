export const title = "Consent: worker authority, observation, locked CAS";
export const state = (v) => `${v.row.status}; marker=${v.row.marker}; owner=${v.live ? "live" : "retired"}; workers=${JSON.stringify(v.pc)}`;
export const group = (v) => v.row.marker === "released" ? "Released" : v.row.marker === "rearm" ? "Rearmed" : v.row.marker === "held" ? "Held" : "Worker";
export const colors = { Worker: "blue", Held: "pink", Rearmed: "teal", Released: "teal" };
export const step = (action) => action;
