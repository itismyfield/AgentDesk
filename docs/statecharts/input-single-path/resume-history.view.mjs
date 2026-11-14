// Production base 1a1b0b1823; projection of a resumed execution's first input.
export const title = "Resumed Claude execution: first input over old history";
export const state = (v) => {
  const r = v.inputReadiness ?? v.r ?? {};
  if (r.badIdleRunning) return "RED: Idle-ready submission while Running";
  const sent = Object.values(r.effects ?? {}).some((n) => n > 0);
  if (r.phase === "Booting") return "Resuming: wait for this launch's SessionStart";
  if (r.turn === 0) {
    if (r.head) return "Start admits: revalidate before first key";
    const held = Object.values(r.q ?? {}).includes("held");
    return held
      ? `Idle over ${r.history} history: queued input held`
      : `Idle over ${r.history} history: queued input waits`;
  }
  if (r.phase === "Running") return sent ? "Running our input" : "Running a prompt typed in the pane";
  return sent ? "Idle after our input" : "Idle after the person's turn: approved terminal admits";
};
export const step = (action) => action;
