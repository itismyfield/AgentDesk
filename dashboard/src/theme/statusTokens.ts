export interface StatusTone {
  accent: string;
  bg: string;
  text: string;
}

// Labels follow the active theme; fixed accents remain usable for alpha borders and dots.
export const SYSTEM_HEALTH_TONES = {
  healthy: {
    accent: "#22c55e",
    bg: "var(--th-badge-emerald-bg)",
    text: "var(--th-badge-emerald-text)",
  },
  warning: {
    accent: "#f59e0b",
    bg: "var(--th-badge-amber-bg)",
    text: "var(--th-badge-amber-text)",
  },
  critical: {
    accent: "#ef4444",
    bg: "color-mix(in oklch, var(--err) 14%, var(--th-bg-surface) 86%)",
    text: "color-mix(in oklch, var(--err) 70%, var(--th-text-primary) 30%)",
  },
  idle: {
    accent: "#64748b",
    bg: "var(--th-overlay-medium)",
    text: "var(--th-text-secondary)",
  },
  info: {
    accent: "#0ea5e9",
    bg: "var(--th-badge-sky-bg)",
    text: "var(--th-badge-sky-text)",
  },
  unknown: {
    accent: "#6b7280",
    bg: "var(--th-overlay-medium)",
    text: "var(--th-text-secondary)",
  },
} satisfies Record<string, StatusTone>;

export type SystemHealthTone = keyof typeof SYSTEM_HEALTH_TONES;

export function getSystemHealthTone(tone: SystemHealthTone | string): StatusTone {
  return (
    (SYSTEM_HEALTH_TONES as Record<string, StatusTone>)[tone] ??
    SYSTEM_HEALTH_TONES.unknown
  );
}

export const KANBAN_STATUS_TONES = {
  backlog: {
    accent: "#64748b",
    bg: "var(--th-overlay-medium)",
    text: "var(--th-text-secondary)",
  },
  ready: {
    accent: "#0ea5e9",
    bg: "var(--th-badge-sky-bg)",
    text: "var(--th-badge-sky-text)",
  },
  requested: {
    accent: "#10b981",
    bg: "var(--th-badge-emerald-bg)",
    text: "var(--th-badge-emerald-text)",
  },
  failed: {
    accent: "#f97316",
    bg: "var(--th-badge-amber-bg)",
    text: "var(--th-badge-amber-text)",
  },
  in_progress: {
    accent: "#f59e0b",
    bg: "var(--th-badge-amber-bg)",
    text: "var(--th-badge-amber-text)",
  },
  review: {
    accent: "#14b8a6",
    bg: "var(--th-badge-blue-bg)",
    text: "var(--th-badge-blue-text)",
  },
  done: {
    accent: "#22c55e",
    bg: "var(--th-badge-emerald-bg)",
    text: "var(--th-badge-emerald-text)",
  },
  blocked: {
    accent: "#ef4444",
    bg: "color-mix(in oklch, var(--err) 14%, var(--th-bg-surface) 86%)",
    text: "color-mix(in oklch, var(--err) 70%, var(--th-text-primary) 30%)",
  },
  cancelled: {
    accent: "#6b7280",
    bg: "var(--th-overlay-medium)",
    text: "var(--th-text-secondary)",
  },
  qa_pending: {
    accent: "#06b6d4",
    bg: "var(--th-badge-sky-bg)",
    text: "var(--th-badge-sky-text)",
  },
  qa_in_progress: {
    accent: "#3b82f6",
    bg: "var(--th-badge-blue-bg)",
    text: "var(--th-badge-blue-text)",
  },
  qa_failed: {
    accent: "#fb7185",
    bg: "color-mix(in oklch, var(--err) 14%, var(--th-bg-surface) 86%)",
    text: "color-mix(in oklch, var(--err) 70%, var(--th-text-primary) 30%)",
  },
  pending_decision: {
    accent: "#f97316",
    bg: "var(--th-badge-amber-bg)",
    text: "var(--th-badge-amber-text)",
  },
} satisfies Record<string, StatusTone>;

export const TIMELINE_KIND_TONES = {
  review: { bg: KANBAN_STATUS_TONES.review.bg, text: KANBAN_STATUS_TONES.review.text },
  pm: { bg: KANBAN_STATUS_TONES.pending_decision.bg, text: KANBAN_STATUS_TONES.pending_decision.text },
  work: { bg: KANBAN_STATUS_TONES.ready.bg, text: KANBAN_STATUS_TONES.ready.text },
  general: { bg: SYSTEM_HEALTH_TONES.idle.bg, text: SYSTEM_HEALTH_TONES.idle.text },
} satisfies Record<string, Pick<StatusTone, "bg" | "text">>;

export const TIMELINE_STATUS_TONES = {
  reviewing: TIMELINE_KIND_TONES.review,
  changes_requested: { bg: KANBAN_STATUS_TONES.qa_failed.bg, text: KANBAN_STATUS_TONES.qa_failed.text },
  passed: { bg: SYSTEM_HEALTH_TONES.healthy.bg, text: SYSTEM_HEALTH_TONES.healthy.text },
  decision: TIMELINE_KIND_TONES.pm,
  completed: TIMELINE_KIND_TONES.work,
  comment: TIMELINE_KIND_TONES.general,
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
