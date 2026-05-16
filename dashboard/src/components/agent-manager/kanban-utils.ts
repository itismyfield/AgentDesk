import { KANBAN_STATUS_TONES } from "../../theme/statusTokens";
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

export type KanbanBoardColumnStatus =
  | "backlog"
  | "requested"
  | "in_progress"
  | "review"
  | "qa_pending"
  | "qa_in_progress"
  | "failed"
  | "done";

interface KanbanColumnDef<TStatus extends string> {
  status: TStatus;
  labelKo: string;
  labelEn: string;
  accent: string;
}

export const COLUMN_DEFS: Array<KanbanColumnDef<KanbanCardStatus | "failed">> = [
  { status: "backlog", labelKo: "백로그", labelEn: "Backlog", accent: KANBAN_STATUS_TONES.backlog.accent },
  { status: "ready", labelKo: "준비됨", labelEn: "Ready", accent: KANBAN_STATUS_TONES.ready.accent },
  { status: "requested", labelKo: "준비됨", labelEn: "Ready", accent: KANBAN_STATUS_TONES.requested.accent },
  { status: "failed", labelKo: "실패", labelEn: "Failed", accent: KANBAN_STATUS_TONES.failed.accent },
  { status: "in_progress", labelKo: "진행 중", labelEn: "In Progress", accent: KANBAN_STATUS_TONES.in_progress.accent },
  { status: "review", labelKo: "검토", labelEn: "Review", accent: KANBAN_STATUS_TONES.review.accent },
  { status: "qa_pending", labelKo: "QA 대기", labelEn: "QA Pending", accent: KANBAN_STATUS_TONES.qa_pending.accent },
  { status: "qa_in_progress", labelKo: "QA 진행", labelEn: "QA In Progress", accent: KANBAN_STATUS_TONES.qa_in_progress.accent },
  { status: "qa_failed", labelKo: "QA 실패", labelEn: "QA Failed", accent: KANBAN_STATUS_TONES.qa_failed.accent },
  { status: "done", labelKo: "완료", labelEn: "Done", accent: KANBAN_STATUS_TONES.done.accent },
];

export const BOARD_COLUMN_DEFS: Array<KanbanColumnDef<KanbanBoardColumnStatus>> = [
  { status: "backlog", labelKo: "백로그", labelEn: "Backlog", accent: KANBAN_STATUS_TONES.backlog.accent },
  { status: "requested", labelKo: "준비됨", labelEn: "Ready", accent: KANBAN_STATUS_TONES.requested.accent },
  { status: "in_progress", labelKo: "진행 중", labelEn: "In Progress", accent: KANBAN_STATUS_TONES.in_progress.accent },
  { status: "review", labelKo: "검토", labelEn: "Review", accent: KANBAN_STATUS_TONES.review.accent },
  { status: "qa_pending", labelKo: "QA 대기", labelEn: "QA Pending", accent: KANBAN_STATUS_TONES.qa_pending.accent },
  { status: "qa_in_progress", labelKo: "QA 진행", labelEn: "QA In Progress", accent: KANBAN_STATUS_TONES.qa_in_progress.accent },
  { status: "failed", labelKo: "실패", labelEn: "Failed", accent: KANBAN_STATUS_TONES.failed.accent },
  { status: "done", labelKo: "완료 일감", labelEn: "Completed Work", accent: KANBAN_STATUS_TONES.done.accent },
];

export const TERMINAL_STATUSES = new Set<KanbanCardStatus>(["done"]);
export const QA_STATUSES = new Set<KanbanCardStatus>(["qa_pending", "qa_in_progress", "qa_failed"]);
export const PRIORITY_OPTIONS: KanbanCardPriority[] = ["low", "medium", "high", "urgent"];
export const REVIEW_DISPATCH_TYPES = new Set(["review", "review-decision"]);
const MANUAL_INTERVENTION_REVIEW_STATUSES = new Set(["dilemma_pending"]);

/** Quick-transition targets per status. Order = button order (primary first). */
export function isManualStatusTransitionAllowed(
  from: KanbanCardStatus,
  to: KanbanCardStatus,
): boolean {
  return (from === "backlog" && to === "ready") || (from !== to && to === "backlog");
}

export const STATUS_TRANSITIONS: Record<KanbanCardStatus, KanbanCardStatus[]> = {
  backlog: ["ready"],
  ready: ["backlog"],
  requested: ["backlog"],
  blocked: ["backlog"],
  in_progress: ["backlog"],
  review: ["backlog"],
  done: ["backlog"],
  qa_pending: ["backlog"],
  qa_in_progress: ["backlog"],
  qa_failed: ["backlog"],
};

export const TRANSITION_STYLE: Record<string, { bg: string; text: string }> = {
  ready: KANBAN_STATUS_TONES.ready,
  requested: KANBAN_STATUS_TONES.requested,
  in_progress: KANBAN_STATUS_TONES.in_progress,
  review: KANBAN_STATUS_TONES.review,
  done: KANBAN_STATUS_TONES.done,
  blocked: KANBAN_STATUS_TONES.blocked,
  backlog: KANBAN_STATUS_TONES.backlog,
  cancelled: KANBAN_STATUS_TONES.cancelled,
  failed: KANBAN_STATUS_TONES.failed,
  qa_pending: KANBAN_STATUS_TONES.qa_pending,
  qa_in_progress: KANBAN_STATUS_TONES.qa_in_progress,
  qa_failed: KANBAN_STATUS_TONES.qa_failed,
  pending_decision: KANBAN_STATUS_TONES.pending_decision,
};

export const REQUEST_TIMEOUT_MS = 45 * 60 * 1000;
export const IN_PROGRESS_STALE_MS = 60 * 60 * 1000;
const BENIGN_BLOCKED_REASON_PREFIXES = [
  "ci:waiting",
  "ci:running",
  "ci:rerunning",
  "ci:rework",
  "deploy:waiting",
  "deploy:deploying:",
] as const;
const LEGACY_STATUS_LABELS: Record<string, { labelKo: string; labelEn: string }> = {
  blocked: { labelKo: "막힘", labelEn: "Blocked" },
  pending_decision: { labelKo: "판단 대기", labelEn: "Pending Decision" },
};

export interface CardDwellBadge {
  label: string;
  detail: string;
  tone: "fresh" | "warm" | "stale";
  textColor: string;
  backgroundColor: string;
  borderColor: string;
}

// ---------------------------------------------------------------------------
// Pure functions
// ---------------------------------------------------------------------------

export function isReviewCard(card: KanbanCard): boolean {
  return !!(card.latest_dispatch_type && REVIEW_DISPATCH_TYPES.has(card.latest_dispatch_type));
}

export function getBoardColumnStatus(status: KanbanCardStatus): KanbanBoardColumnStatus {
  if (status === "ready" || status === "requested") return "requested";
  if (status === "blocked" || status === "qa_failed") return "failed";
  return status;
}

function githubPathSegments(value: string | null | undefined): string[] | null {
  const trimmed = (value ?? "").trim().replace(/\.git$/, "").replace(/^\/+|\/+$/g, "");
  if (!trimmed) return null;

  let path = trimmed;
  if (trimmed.startsWith("git@github.com:")) {
    path = trimmed.slice("git@github.com:".length);
  } else if (trimmed.startsWith("ssh://git@github.com/")) {
    path = trimmed.slice("ssh://git@github.com/".length);
  } else if (trimmed.startsWith("https://github.com/") || trimmed.startsWith("http://github.com/")) {
    const index = trimmed.lastIndexOf("github.com/");
    path = trimmed.slice(index + "github.com/".length);
  }

  const segments = path
    .replace(/^\/+|\/+$/g, "")
    .split("/")
    .map((segment) => segment.trim())
    .filter(Boolean);
  return segments.length > 0 ? segments : null;
}

function isValidGitHubSegment(segment: string): boolean {
  return Boolean(segment) && !segment.includes(":") && !/\s/.test(segment) && segment !== "." && segment !== "..";
}

export function normalizeGitHubRepo(value: string | null | undefined): string | null {
  const segments = githubPathSegments(value);
  const owner = segments?.[0];
  const repo = segments?.[1]?.replace(/\.git$/, "");
  if (!owner || !repo || !isValidGitHubSegment(owner) || !isValidGitHubSegment(repo)) {
    return null;
  }
  return `${owner}/${repo}`;
}

export function normalizeGitHubIssueUrl(value: string | null | undefined): string | null {
  const repo = normalizeGitHubRepo(value);
  const segments = githubPathSegments(value);
  if (!repo || !segments) return null;
  const issueIndex = segments.findIndex((segment) => segment === "issues");
  const issueNumber = issueIndex >= 0 ? Number(segments[issueIndex + 1]) : NaN;
  if (!Number.isInteger(issueNumber) || issueNumber <= 0) return null;
  return `https://github.com/${repo}/issues/${issueNumber}`;
}

export function buildGitHubIssueUrl(
  repo: string | null | undefined,
  issueNumber: number | null | undefined,
  issueUrl?: string | null,
): string | null {
  const normalizedIssueUrl = normalizeGitHubIssueUrl(issueUrl);
  if (normalizedIssueUrl) return normalizedIssueUrl;
  if (!Number.isInteger(issueNumber) || !issueNumber || issueNumber <= 0) return null;
  const normalizedRepo = normalizeGitHubRepo(repo);
  if (!normalizedRepo) return null;
  return `https://github.com/${normalizedRepo}/issues/${issueNumber}`;
}

export function isBenignBlockedReason(reason: string | null | undefined): boolean {
  if (!reason) return false;
  return BENIGN_BLOCKED_REASON_PREFIXES.some((prefix) => reason.startsWith(prefix));
}

export function hasManualInterventionReason(card: Pick<KanbanCard, "blocked_reason">): boolean {
  const reason = card.blocked_reason?.trim();
  return Boolean(reason) && !isBenignBlockedReason(reason);
}

export function isManualInterventionCard(
  card: Pick<KanbanCard, "review_status" | "blocked_reason">,
): boolean {
  return MANUAL_INTERVENTION_REVIEW_STATUSES.has(card.review_status ?? "") || hasManualInterventionReason(card);
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

export function coerceTimestampMs(value: string | number | null | undefined): number | null {
  if (value == null || value === "") return null;
  if (typeof value === "number") {
    return value < 1e12 ? value * 1000 : value;
  }
  const numeric = Number(value);
  if (Number.isFinite(numeric)) {
    return numeric < 1e12 ? numeric * 1000 : numeric;
  }
  const parsed = new Date(value).getTime();
  return Number.isNaN(parsed) ? null : parsed;
}

export function formatTs(value: string | number | null | undefined, locale: UiLanguage): string {
  const ts = coerceTimestampMs(value);
  if (!ts) return "-";
  return new Intl.DateTimeFormat(locale, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  }).format(ts);
}

export function formatIso(value: string | number | null | undefined, locale: UiLanguage): string {
  if (value == null || value === "") return "-";
  const ts = coerceTimestampMs(value);
  if (ts != null) return formatTs(ts, locale);
  return typeof value === "string" ? value : "-";
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

function dwellThresholdsForStatus(status: KanbanCardStatus): { warmMs: number; staleMs: number } {
  switch (status) {
    case "requested":
      return { warmMs: 15 * 60 * 1000, staleMs: REQUEST_TIMEOUT_MS };
    case "in_progress":
      return { warmMs: 90 * 60 * 1000, staleMs: 4 * 60 * 60 * 1000 };
    case "review":
      return { warmMs: 45 * 60 * 1000, staleMs: 2 * 60 * 60 * 1000 };
    case "qa_pending":
    case "qa_in_progress":
    case "qa_failed":
      return { warmMs: 60 * 60 * 1000, staleMs: 3 * 60 * 60 * 1000 };
    case "backlog":
    case "ready":
    case "done":
      return { warmMs: 12 * 60 * 60 * 1000, staleMs: 36 * 60 * 60 * 1000 };
    default:
      return { warmMs: 60 * 60 * 1000, staleMs: 3 * 60 * 60 * 1000 };
  }
}

export function getCardStateEnteredAt(card: KanbanCard): number | null {
  switch (card.status) {
    case "requested":
      return coerceTimestampMs(card.requested_at ?? card.updated_at ?? card.created_at);
    case "in_progress":
      return coerceTimestampMs(card.started_at ?? card.updated_at ?? card.created_at);
    case "review":
      return coerceTimestampMs(card.review_entered_at ?? card.updated_at ?? card.started_at ?? card.created_at);
    case "done":
      return coerceTimestampMs(card.completed_at ?? card.updated_at ?? card.created_at);
    case "qa_pending":
    case "qa_in_progress":
    case "qa_failed":
    case "ready":
    case "backlog":
    default:
      return coerceTimestampMs(card.updated_at ?? card.created_at);
  }
}

export function getCardDwellBadge(
  card: KanbanCard,
  now: number,
  tr: (ko: string, en: string) => string,
): CardDwellBadge | null {
  const enteredAt = getCardStateEnteredAt(card);
  if (enteredAt == null) return null;
  const elapsed = Math.max(0, now - enteredAt);
  const { warmMs, staleMs } = dwellThresholdsForStatus(card.status);
  if (elapsed >= staleMs) {
    return {
      label: tr("체류", "Dwell"),
      detail: formatAgeLabel(elapsed, tr),
      tone: "stale",
      textColor: "#fca5a5",
      backgroundColor: "rgba(239,68,68,0.18)",
      borderColor: "rgba(239,68,68,0.38)",
    };
  }
  if (elapsed >= warmMs) {
    return {
      label: tr("체류", "Dwell"),
      detail: formatAgeLabel(elapsed, tr),
      tone: "warm",
      textColor: "#fde68a",
      backgroundColor: "rgba(234,179,8,0.18)",
      borderColor: "rgba(234,179,8,0.34)",
    };
  }
  return {
    label: tr("체류", "Dwell"),
    detail: formatAgeLabel(elapsed, tr),
    tone: "fresh",
    textColor: "#86efac",
    backgroundColor: "rgba(34,197,94,0.18)",
    borderColor: "rgba(34,197,94,0.32)",
  };
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

export {
  coalesceGitHubCommentTimeline,
  parseGitHubCommentTimeline,
  type CoalescedGitHubTimelineItem,
  type GitHubTimelineKind,
  type GitHubTimelineStatus,
  type ParsedGitHubComment,
} from "./kanban-timeline-utils";

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
  if (card.status === "requested") {
    const requestedAt = coerceTimestampMs(card.requested_at);
    const age = requestedAt == null ? null : now - requestedAt;
    if (age != null && age >= REQUEST_TIMEOUT_MS) {
      return { label: tr("수락 지연", "Ack delay"), tone: "#f97316", detail: formatAgeLabel(age, tr) };
    }
  }
  if (card.status === "in_progress") {
    const startedAt = coerceTimestampMs(card.started_at);
    const age = startedAt == null ? null : now - startedAt;
    if (age != null && age >= IN_PROGRESS_STALE_MS) {
      return { label: tr("정체", "Stalled"), tone: "#f59e0b", detail: formatAgeLabel(age, tr) };
    }
  }
  return null;
}

export function labelForStatus(
  status: KanbanCardStatus | "failed",
  tr: (ko: string, en: string) => string,
): string {
  const col = COLUMN_DEFS.find((column) => column.status === status);
  if (col) return tr(col.labelKo, col.labelEn);
  const legacy = LEGACY_STATUS_LABELS[String(status)];
  return legacy ? tr(legacy.labelKo, legacy.labelEn) : status;
}
