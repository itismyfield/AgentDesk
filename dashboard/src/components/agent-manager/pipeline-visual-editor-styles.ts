import type { PipelineConfigFull } from "../../types";

export function transitionAccent(type: PipelineConfigFull["transitions"][number]["type"]) {
  if (type === "free") {
    return {
      stroke: "var(--info)",
      background: "color-mix(in srgb, var(--info) 14%, var(--th-card-bg))",
      text: "var(--info)",
    };
  }
  if (type === "gated") {
    return {
      stroke: "var(--warn)",
      background: "color-mix(in srgb, var(--warn) 14%, var(--th-card-bg))",
      text: "var(--warn)",
    };
  }
  return {
    stroke: "var(--err)",
    background: "color-mix(in srgb, var(--err) 14%, var(--th-card-bg))",
    text: "var(--err)",
  };
}

export function fsmStateTone(stateId: string, terminal = false) {
  switch (stateId) {
    case "ready":
    case "requested":
      return { stroke: "var(--info)", glow: "color-mix(in srgb, var(--info) 14%, transparent)" };
    case "in_progress":
    case "qa_in_progress":
    case "review":
    case "qa_pending":
      return { stroke: "var(--warn)", glow: "color-mix(in srgb, var(--warn) 14%, transparent)" };
    case "done":
      return { stroke: "var(--ok)", glow: "color-mix(in srgb, var(--ok) 14%, transparent)" };
    case "failed":
    case "blocked":
    case "qa_failed":
      return { stroke: "var(--err)", glow: "color-mix(in srgb, var(--err) 14%, transparent)" };
    default:
      return terminal
        ? { stroke: "var(--ok)", glow: "color-mix(in srgb, var(--ok) 14%, transparent)" }
        : { stroke: "var(--fg-muted)", glow: "color-mix(in srgb, var(--fg-muted) 12%, transparent)" };
  }
}
