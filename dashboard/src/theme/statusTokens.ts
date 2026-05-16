export interface StatusTone {
  accent: string;
  bg: string;
  text: string;
}

export const KANBAN_STATUS_TONES = {
  backlog: {
    accent: "#64748b",
    bg: "rgba(100,116,139,0.18)",
    text: "#94a3b8",
  },
  ready: {
    accent: "#0ea5e9",
    bg: "rgba(14,165,233,0.18)",
    text: "#38bdf8",
  },
  requested: {
    accent: "#10b981",
    bg: "rgba(16,185,129,0.18)",
    text: "#10b981",
  },
  failed: {
    accent: "#f97316",
    bg: "rgba(249,115,22,0.18)",
    text: "#fb923c",
  },
  in_progress: {
    accent: "#f59e0b",
    bg: "rgba(245,158,11,0.18)",
    text: "#fbbf24",
  },
  review: {
    accent: "#14b8a6",
    bg: "rgba(20,184,166,0.18)",
    text: "#2dd4bf",
  },
  done: {
    accent: "#22c55e",
    bg: "rgba(34,197,94,0.22)",
    text: "#4ade80",
  },
  blocked: {
    accent: "#ef4444",
    bg: "rgba(239,68,68,0.18)",
    text: "#f87171",
  },
  cancelled: {
    accent: "#6b7280",
    bg: "rgba(107,114,128,0.18)",
    text: "#9ca3af",
  },
  qa_pending: {
    accent: "#06b6d4",
    bg: "rgba(6,182,212,0.18)",
    text: "#06b6d4",
  },
  qa_in_progress: {
    accent: "#3b82f6",
    bg: "rgba(59,130,246,0.18)",
    text: "#3b82f6",
  },
  qa_failed: {
    accent: "#fb7185",
    bg: "rgba(251,113,133,0.18)",
    text: "#fb7185",
  },
  pending_decision: {
    accent: "#f97316",
    bg: "rgba(249,115,22,0.18)",
    text: "#f97316",
  },
} satisfies Record<string, StatusTone>;

export const TIMELINE_KIND_TONES = {
  review: { bg: "rgba(20,184,166,0.16)", text: KANBAN_STATUS_TONES.review.text },
  pm: { bg: "rgba(249,115,22,0.16)", text: "#fdba74" },
  work: { bg: "rgba(96,165,250,0.16)", text: "#93c5fd" },
  general: { bg: "rgba(148,163,184,0.10)", text: "#94a3b8" },
} satisfies Record<string, Pick<StatusTone, "bg" | "text">>;

export const TIMELINE_STATUS_TONES = {
  reviewing: TIMELINE_KIND_TONES.review,
  changes_requested: { bg: "rgba(251,113,133,0.16)", text: "#fda4af" },
  passed: { bg: "rgba(34,197,94,0.18)", text: "#86efac" },
  decision: TIMELINE_KIND_TONES.pm,
  completed: TIMELINE_KIND_TONES.work,
  comment: { bg: "rgba(148,163,184,0.12)", text: "#94a3b8" },
} satisfies Record<string, Pick<StatusTone, "bg" | "text">>;

export function getKanbanStatusTone(status: string): StatusTone {
  return (KANBAN_STATUS_TONES as Record<string, StatusTone>)[status] ?? KANBAN_STATUS_TONES.backlog;
}
