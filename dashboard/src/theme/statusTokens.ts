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

export const QUEUE_ENTRY_STATUS_TONES = {
  pending: {
    bg: KANBAN_STATUS_TONES.backlog.bg,
    text: KANBAN_STATUS_TONES.backlog.text,
    label: "대기",
    labelEn: "Pending",
  },
  dispatched: {
    bg: KANBAN_STATUS_TONES.in_progress.bg,
    text: KANBAN_STATUS_TONES.in_progress.text,
    label: "진행",
    labelEn: "Active",
  },
  done: {
    bg: KANBAN_STATUS_TONES.done.bg,
    text: KANBAN_STATUS_TONES.done.text,
    label: "완료",
    labelEn: "Done",
  },
  review: {
    bg: KANBAN_STATUS_TONES.review.bg,
    text: KANBAN_STATUS_TONES.review.text,
    label: "리뷰",
    labelEn: "Review",
  },
  rework: {
    bg: "rgba(236,72,153,0.22)",
    text: "#f472b6",
    label: "리뷰 반영",
    labelEn: "Rework",
  },
  skipped: {
    bg: KANBAN_STATUS_TONES.cancelled.bg,
    text: KANBAN_STATUS_TONES.cancelled.text,
    label: "건너뜀",
    labelEn: "Skipped",
  },
  failed: {
    bg: KANBAN_STATUS_TONES.blocked.bg,
    text: KANBAN_STATUS_TONES.blocked.text,
    label: "실패",
    labelEn: "Failed",
  },
} satisfies Record<string, Pick<StatusTone, "bg" | "text"> & { label: string; labelEn: string }>;

export const AUTOQUEUE_RUN_STATUS_TONES = {
  generated: {
    bg: KANBAN_STATUS_TONES.qa_in_progress.bg,
    text: "#60a5fa",
    label: "생성됨",
    labelEn: "Generated",
  },
  pending: {
    bg: "rgba(56,189,248,0.2)",
    text: KANBAN_STATUS_TONES.ready.text,
    label: "PMD 대기",
    labelEn: "Awaiting PMD",
  },
  active: {
    bg: "rgba(16,185,129,0.2)",
    text: KANBAN_STATUS_TONES.requested.text,
    label: "실행 중",
    labelEn: "Active",
  },
  paused: {
    bg: "rgba(245,158,11,0.2)",
    text: KANBAN_STATUS_TONES.in_progress.text,
    label: "일시정지",
    labelEn: "Paused",
  },
  completed: {
    bg: "rgba(34,197,94,0.2)",
    text: KANBAN_STATUS_TONES.done.text,
    label: "완료",
    labelEn: "Done",
  },
  cancelled: {
    bg: "rgba(248,113,113,0.18)",
    text: KANBAN_STATUS_TONES.blocked.text,
    label: "취소됨",
    labelEn: "Cancelled",
  },
} satisfies Record<string, Pick<StatusTone, "bg" | "text"> & { label: string; labelEn: string }>;

export const QUEUE_GROUP_COLORS = [
  KANBAN_STATUS_TONES.requested.accent,
  KANBAN_STATUS_TONES.ready.text,
  KANBAN_STATUS_TONES.in_progress.accent,
  KANBAN_STATUS_TONES.in_progress.text,
  KANBAN_STATUS_TONES.done.text,
  KANBAN_STATUS_TONES.failed.text,
  KANBAN_STATUS_TONES.blocked.accent,
  "#22d3ee",
  "#a3e635",
  KANBAN_STATUS_TONES.blocked.text,
] as const;

export const REVIEW_STATUS_TONES = {
  blocked: {
    accent: "#eab308",
    text: "#fde047",
  },
  rework: {
    accent: KANBAN_STATUS_TONES.failed.accent,
    text: "#fdba74",
  },
  review: {
    accent: KANBAN_STATUS_TONES.review.accent,
    text: KANBAN_STATUS_TONES.review.text,
  },
} satisfies Record<string, Pick<StatusTone, "accent" | "text">>;

export const DELIVERY_EVENT_STATUS_TONES = {
  reserved: { bg: "rgba(156,163,175,0.12)", text: KANBAN_STATUS_TONES.cancelled.text },
  sent: { bg: "rgba(34,197,94,0.16)", text: "#86efac" },
  fallback: TIMELINE_KIND_TONES.pm,
  duplicate: KANBAN_STATUS_TONES.qa_in_progress,
  skipped: { bg: "rgba(156,163,175,0.12)", text: KANBAN_STATUS_TONES.cancelled.text },
  failed: { bg: "rgba(248,113,113,0.16)", text: "#fca5a5" },
} satisfies Record<string, Pick<StatusTone, "bg" | "text">>;

export function getKanbanStatusTone(status: string): StatusTone {
  return (KANBAN_STATUS_TONES as Record<string, StatusTone>)[status] ?? KANBAN_STATUS_TONES.backlog;
}

export function getQueueGroupColor(index: number): string {
  return QUEUE_GROUP_COLORS[index % QUEUE_GROUP_COLORS.length];
}

export function getBatchPhaseColor(phase: number): string {
  if (phase <= 0) return KANBAN_STATUS_TONES.backlog.text;
  return getQueueGroupColor(phase - 1);
}
