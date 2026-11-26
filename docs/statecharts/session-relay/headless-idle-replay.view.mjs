export const title = "Headless answer and idle replay"

const includes = (set, turn) => set.includes(turn)

export function state({ s }) {
  const a = s.posts.A
  const b = s.posts.B
  if (a > 1 || b > 1) return "Same answer posted twice"
  if (includes(s.retired, "B") && b === 0) return "Later answer silently consumed"
  if (includes(s.retired, "B")) return "Both answers visible once"
  if (s.state.B === "Sent") return "Later headless answer sent"
  if (includes(s.retired, "A")) return "First replay settled"
  if (!s.grace) return "Idle replay eligible"
  if (s.inflight === "None") return "Inflight gone and idle grace active"
  if (s.state.A === "Sent") return "Outbox confirms the first answer"
  if (s.state.A === "Posted") return "First reply visible in Discord"
  if (s.state.A === "Pending") return "First reply queued"
  return "Wrapper terminal readable"
}

export function step(action) {
  const labels = {
    Capture: "read completed record",
    Stage: "queue answer",
    WorkerPost: "worker posts answer",
    MarkSent: "confirm outbox sent",
    ClearInflight: "remove inflight",
    ExpireGrace: "ten-second grace expires",
    ConsumeProof: "consume proven span",
    IdlePost: "idle relay posts span",
    Append: "later terminal appears",
  }
  return labels[action] ?? action
}
