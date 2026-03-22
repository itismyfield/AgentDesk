import type {
  KanbanCard,
  KanbanCardMetadata,
  KanbanCardPriority,
  KanbanCardStatus,
  KanbanReviewChecklistItem,
  UiLanguage,
} from "../../types";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

export const COLUMN_DEFS: Array<{
  status: KanbanCardStatus;
  labelKo: string;
  labelEn: string;
  accent: string;
}> = [
  { status: "backlog", labelKo: "백로그", labelEn: "Backlog", accent: "#64748b" },
  { status: "ready", labelKo: "준비됨", labelEn: "Ready", accent: "#0ea5e9" },
  { status: "requested", labelKo: "요청됨", labelEn: "Requested", accent: "#8b5cf6" },
  { status: "in_progress", labelKo: "진행 중", labelEn: "In Progress", accent: "#f59e0b" },
  { status: "review", labelKo: "검토", labelEn: "Review", accent: "#14b8a6" },
  { status: "qa_pending", labelKo: "QA 대기", labelEn: "QA Pending", accent: "#e879f9" },
  { status: "qa_in_progress", labelKo: "QA 진행", labelEn: "QA In Progress", accent: "#c084fc" },
  { status: "qa_failed", labelKo: "QA 실패", labelEn: "QA Failed", accent: "#fb7185" },
  { status: "pending_decision", labelKo: "판단 대기", labelEn: "Pending Decision", accent: "#f472b6" },
  { status: "blocked", labelKo: "막힘", labelEn: "Blocked", accent: "#ef4444" },
  { status: "done", labelKo: "완료", labelEn: "Done", accent: "#22c55e" },
];

export const TERMINAL_STATUSES = new Set<KanbanCardStatus>(["done"]);
export const QA_STATUSES = new Set<KanbanCardStatus>(["qa_pending", "qa_in_progress", "qa_failed"]);
export const PRIORITY_OPTIONS: KanbanCardPriority[] = ["low", "medium", "high", "urgent"];
export const REVIEW_DISPATCH_TYPES = new Set(["review", "review-decision"]);

/** Quick-transition targets per status. Order = button order (primary first). */
export const STATUS_TRANSITIONS: Record<KanbanCardStatus, KanbanCardStatus[]> = {
  backlog: ["ready"],
  ready: ["backlog"],
  requested: ["ready", "in_progress"],
  in_progress: ["review", "blocked"],
  review: ["done", "in_progress"],
  blocked: ["in_progress"],
  done: ["backlog"],
  qa_pending: ["qa_in_progress", "done"],
  qa_in_progress: ["done", "qa_failed"],
  qa_failed: ["ready"],
  pending_decision: ["review", "blocked", "in_progress"],
};

export const TRANSITION_STYLE: Record<string, { bg: string; text: string }> = {
  ready: { bg: "rgba(14,165,233,0.18)", text: "#38bdf8" },
  requested: { bg: "rgba(139,92,246,0.18)", text: "#a78bfa" },
  in_progress: { bg: "rgba(245,158,11,0.18)", text: "#fbbf24" },
  review: { bg: "rgba(20,184,166,0.18)", text: "#2dd4bf" },
  done: { bg: "rgba(34,197,94,0.22)", text: "#4ade80" },
  blocked: { bg: "rgba(239,68,68,0.18)", text: "#f87171" },
  backlog: { bg: "rgba(100,116,139,0.18)", text: "#94a3b8" },
  cancelled: { bg: "rgba(107,114,128,0.18)", text: "#9ca3af" },
  failed: { bg: "rgba(249,115,22,0.18)", text: "#fb923c" },
  qa_pending: { bg: "rgba(232,121,249,0.18)", text: "#e879f9" },
  qa_in_progress: { bg: "rgba(192,132,252,0.18)", text: "#c084fc" },
  qa_failed: { bg: "rgba(251,113,133,0.18)", text: "#fb7185" },
  pending_decision: { bg: "rgba(244,114,182,0.18)", text: "#f472b6" },
};

export const REQUEST_TIMEOUT_MS = 45 * 60 * 1000;
export const IN_PROGRESS_STALE_MS = 60 * 60 * 1000;

// ---------------------------------------------------------------------------
// Pure functions
// ---------------------------------------------------------------------------

export function isReviewCard(card: KanbanCard): boolean {
  return !!(card.latest_dispatch_type && REVIEW_DISPATCH_TYPES.has(card.latest_dispatch_type));
}

export function priorityLabel(priority: KanbanCardPriority, tr: (ko: string, en: string) => string): string {
  switch (priority) {
    case "low":
      return tr("낮음", "Low");
    case "medium":
      return tr("보통", "Medium");
    case "high":
      return tr("높음", "High");
    case "urgent":
      return tr("긴급", "Urgent");
  }
}

export function formatTs(value: number | null | undefined, locale: UiLanguage): string {
  if (!value) return "-";
  return new Intl.DateTimeFormat(locale, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  }).format(value);
}

export function formatIso(value: string | number | null | undefined, locale: UiLanguage): string {
  if (value == null) return "-";
  if (typeof value === "number") return value ? formatTs(value, locale) : "-";
  if (!value) return "-";
  const parsed = new Date(value).getTime();
  if (Number.isNaN(parsed)) return value;
  return formatTs(parsed, locale);
}

export function createChecklistItem(label: string, index = 0): KanbanReviewChecklistItem {
  return {
    id: `check-${Date.now()}-${index}`,
    label: label.trim(),
    done: false,
  };
}

export function parseCardMetadata(value: string | null | undefined): KanbanCardMetadata {
  if (!value) return {};
  try {
    const parsed = JSON.parse(value) as KanbanCardMetadata;
    return {
      ...parsed,
      review_checklist: Array.isArray(parsed.review_checklist)
        ? parsed.review_checklist.filter((item): item is KanbanReviewChecklistItem => Boolean(item?.label))
        : [],
    };
  } catch {
    return {};
  }
}

export function stringifyCardMetadata(metadata: KanbanCardMetadata): string | null {
  const payload: KanbanCardMetadata = {};
  if (metadata.retry_count) payload.retry_count = metadata.retry_count;
  if (metadata.failover_count) payload.failover_count = metadata.failover_count;
  if (metadata.timed_out_stage) payload.timed_out_stage = metadata.timed_out_stage;
  if (metadata.timed_out_at) payload.timed_out_at = metadata.timed_out_at;
  if (metadata.timed_out_reason) payload.timed_out_reason = metadata.timed_out_reason;
  if (metadata.review_checklist && metadata.review_checklist.length > 0) {
    payload.review_checklist = metadata.review_checklist
      .map((item, index) => ({
        id: item.id || `check-${index}`,
        label: item.label.trim(),
        done: item.done === true,
      }))
      .filter((item) => item.label);
  }
  if (metadata.redispatch_count) payload.redispatch_count = metadata.redispatch_count;
  if (metadata.redispatch_reason) payload.redispatch_reason = metadata.redispatch_reason;
  if (metadata.reward) payload.reward = metadata.reward;
  return Object.keys(payload).length > 0 ? JSON.stringify(payload) : null;
}

export function formatAgeLabel(ms: number, tr: (ko: string, en: string) => string): string {
  if (ms < 60 * 1000) {
    return tr("방금", "just now");
  }
  const minutes = Math.round(ms / 60_000);
  if (minutes < 60) {
    return tr(`${minutes}분`, `${minutes}m`);
  }
  const hours = Math.round(minutes / 60);
  if (hours < 24) {
    return tr(`${hours}시간`, `${hours}h`);
  }
  const days = Math.round(hours / 24);
  return tr(`${days}일`, `${days}d`);
}

// ---------------------------------------------------------------------------
// PMD Issue Format Parser
// ---------------------------------------------------------------------------

export interface ParsedIssueSections {
  background: string | null;
  content: string | null;
  dodItems: string[];
  dependencies: string | null;
  risks: string | null;
}

export function parseIssueSections(desc: string | null | undefined): ParsedIssueSections | null {
  if (!desc || !desc.includes("## DoD")) return null;

  const sections: Record<string, string> = {};
  let currentKey: string | null = null;
  let currentLines: string[] = [];

  for (const line of desc.split("\n")) {
    const heading = line.match(/^##\s+(.+)$/);
    if (heading) {
      if (currentKey) sections[currentKey] = currentLines.join("\n").trim();
      currentKey = heading[1].trim();
      currentLines = [];
    } else {
      currentLines.push(line);
    }
  }
  if (currentKey) sections[currentKey] = currentLines.join("\n").trim();

  const dodText = sections["DoD"] ?? "";
  const dodItems = dodText
    .split("\n")
    .map((line) => line.replace(/^-\s*\[[ x]\]\s*/, "").trim())
    .filter(Boolean);

  return {
    background: sections["배경"] || null,
    content: sections["내용"] || null,
    dodItems,
    dependencies: sections["의존성"] || null,
    risks: sections["리스크"] || null,
  };
}

/** Sync DoD items from parsed issue body into review_checklist, preserving existing done states. */
export function syncDodToChecklist(
  dodItems: string[],
  existingChecklist: KanbanReviewChecklistItem[],
): KanbanReviewChecklistItem[] {
  const existing = new Map(existingChecklist.map((item) => [item.label, item]));
  return dodItems.map((label, i) => {
    const match = existing.get(label);
    return match ?? createChecklistItem(label, i);
  });
}

// ---------------------------------------------------------------------------
// Editor state
// ---------------------------------------------------------------------------

export interface EditorState {
  title: string;
  description: string;
  assignee_agent_id: string;
  priority: KanbanCardPriority;
  status: KanbanCardStatus;
  blocked_reason: string;
  review_notes: string;
  review_checklist: KanbanReviewChecklistItem[];
}

export const EMPTY_EDITOR: EditorState = {
  title: "",
  description: "",
  assignee_agent_id: "",
  priority: "medium",
  status: "ready",
  blocked_reason: "",
  review_notes: "",
  review_checklist: [],
};

export function coerceEditor(card: KanbanCard | null): EditorState {
  if (!card) return EMPTY_EDITOR;
  const metadata = parseCardMetadata(card.metadata_json);
  const parsed = parseIssueSections(card.description);
  const checklist = parsed
    ? syncDodToChecklist(parsed.dodItems, metadata.review_checklist ?? [])
    : metadata.review_checklist ?? [];
  return {
    title: card.title,
    description: card.description ?? "",
    assignee_agent_id: card.assignee_agent_id ?? "",
    priority: card.priority,
    status: card.status,
    blocked_reason: card.blocked_reason ?? "",
    review_notes: card.review_notes ?? "",
    review_checklist: checklist,
  };
}

export function getCardMetadata(card: KanbanCard): KanbanCardMetadata {
  return parseCardMetadata(card.metadata_json);
}

export function getChecklistSummary(card: KanbanCard): string | null {
  const checklist = getCardMetadata(card).review_checklist ?? [];
  if (checklist.length === 0) return null;
  const done = checklist.filter((item) => item.done).length;
  return `${done}/${checklist.length}`;
}

export function getCardDelayBadge(
  card: KanbanCard,
  tr: (ko: string, en: string) => string,
): { label: string; tone: string; detail: string } | null {
  const now = Date.now();
  if (card.status === "requested" && card.requested_at) {
    const age = now - card.requested_at;
    if (age >= REQUEST_TIMEOUT_MS) {
      return { label: tr("수락 지연", "Ack delay"), tone: "#f97316", detail: formatAgeLabel(age, tr) };
    }
  }
  if (card.status === "in_progress" && card.started_at) {
    const age = now - card.started_at;
    if (age >= IN_PROGRESS_STALE_MS) {
      return { label: tr("정체", "Stalled"), tone: "#f59e0b", detail: formatAgeLabel(age, tr) };
    }
  }
  return null;
}

export function labelForStatus(status: KanbanCardStatus, tr: (ko: string, en: string) => string): string {
  const col = COLUMN_DEFS.find((column) => column.status === status);
  return col ? tr(col.labelKo, col.labelEn) : status;
}
