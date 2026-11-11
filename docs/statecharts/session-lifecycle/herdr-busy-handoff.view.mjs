// Hand-authored design projection; the WSL run used the repository TLC checker.
export const title = "Claude forward handoff: pinned pane, native first input, journal recovery";
export const state = (v) => {
  if (v.server === "down") return `Restart during ${v.phase}`;
  if (!v.actor && v.journal) return "Journal exists; boot census must select actor";
  if (!v.awake && v.actor) return "Actor selected; BootWake still required";
  const inputs = Object.values(v.q);
  if (inputs.includes("blocked")) return "Active first input missed Begin (RED)";
  if (inputs.includes("retained")) return "Pre-key refusal retained original (RED)";
  if (inputs.includes("held")) return "Typed first-input hold; native/host level wake";
  if (inputs.includes("recover")) return "Recover exact receipt or positive no-key proof";
  if (inputs.includes("sent")) return "Input effect happened; owner awaits receipt";
  if (inputs.includes("armed")) return "One destination armed; first-key fence";
  return {
    legacy: "Probe and commit active/idle intake",
    stopping: "Close exact old pane on the pinned server connection",
    checking: "Close dispatched; provider end still unproven",
    fallback: "Provisional old destination, releasable only before any key",
    revalidating: "Positive no-key release; revalidate old identity",
    ended: "Old provider absent; final current native ID captured",
    pending: "Durable Pending launch",
    created: "Await own resume nonce/source record",
    bound: "Native first-input epoch ready",
    complete: "Originals settled and unfinished journal cleared",
  }[v.phase] ?? v.phase;
};
export const group = (v) => v.server === "down" || !v.awake ? "Restart"
  : v.phase === "fallback" || v.phase === "revalidating" ? "Fallback" : "Forward";
export const colors = { Restart: "pink", Fallback: "blue", Forward: "teal" };
export const step = (action) => action;
