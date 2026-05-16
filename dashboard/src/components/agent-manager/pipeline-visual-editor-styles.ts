import type { PipelineConfigFull } from "../../types";

export function transitionAccent(type: PipelineConfigFull["transitions"][number]["type"]) {
  if (type === "free") {
    return {
      stroke: "var(--th-accent-info)",
      background: "color-mix(in srgb, var(--th-badge-sky-bg) 82%, var(--th-card-bg) 18%)",
      text: "var(--th-accent-info)",
    };
  }
  if (type === "gated") {
    return {
      stroke: "var(--th-accent-warn)",
      background: "color-mix(in srgb, var(--th-badge-amber-bg) 84%, var(--th-card-bg) 16%)",
      text: "var(--th-accent-warn)",
    };
  }
  return {
    stroke: "var(--th-accent-danger)",
    background: "color-mix(in srgb, rgba(255, 107, 107, 0.18) 84%, var(--th-card-bg) 16%)",
    text: "var(--th-accent-danger)",
  };
}

export function fsmStateTone(stateId: string) {
  switch (stateId) {
    case "ready":
      return { stroke: "oklch(0.72 0.14 220)", glow: "rgba(56, 189, 248, 0.16)" };
    case "in_progress":
      return { stroke: "#fb923c", glow: "rgba(251, 146, 60, 0.16)" };
    case "review":
      return { stroke: "#facc15", glow: "rgba(250, 204, 21, 0.14)" };
    case "done":
      return { stroke: "#86efac", glow: "rgba(134, 239, 172, 0.15)" };
    case "failed":
      return { stroke: "#f87171", glow: "rgba(248, 113, 113, 0.14)" };
    default:
      return { stroke: "rgba(148, 163, 184, 0.72)", glow: "rgba(148, 163, 184, 0.08)" };
  }
}
