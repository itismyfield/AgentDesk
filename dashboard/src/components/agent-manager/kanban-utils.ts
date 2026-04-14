import type { GitHubComment } from "../../api";
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

export const BOARD_COLUMN_DEFS: Array<{
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
  { status: "done", labelKo: "완료 일감", labelEn: "Completed Work", accent: "#22c55e" },
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
  in_progress: ["backlog"],
  review: ["backlog"],
  blocked: ["backlog"],
  done: ["backlog"],
  qa_pending: ["backlog"],
  qa_in_progress: ["backlog"],
  qa_failed: ["backlog"],
  pending_decision: ["backlog"],
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

export function isManualInterventionCard(card: KanbanCard): boolean {
  return card.status === "blocked" || card.status === "pending_decision";
}

export function hasManualInterventionReason(card: KanbanCard): boolean {
  return Boolean(card.blocked_reason);
}

export function isReviewCard(card: KanbanCard): boolean {
  return !!(card.latest_dispatch_type && REVIEW_DISPATCH_TYPES.has(card.latest_dispatch_type));
}

export function hasManualInterventionReason(card: KanbanCard | null | undefined): boolean {
  return Boolean(card?.blocked_reason);
}

export function isManualInterventionCard(card: KanbanCard | null | undefined): boolean {
  if (!card) return false;
  const reviewStatus = card.review_status;
  return (
    card.status === "blocked"
    || card.status === "pending_decision"
    || hasManualInterventionReason(card)
    || (card.status === "review"
      && reviewStatus != null
      && MANUAL_INTERVENTION_REVIEW_STATUSES.has(reviewStatus))
  );
}

export function getBoardColumnStatus(status: KanbanCardStatus): KanbanCardStatus {
  if (status === "blocked") return "in_progress";
  if (status === "pending_decision") return "review";
  return status;
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
    case "pending_decision":
      return { warmMs: 45 * 60 * 1000, staleMs: 2 * 60 * 60 * 1000 };
    case "blocked":
      return { warmMs: 30 * 60 * 1000, staleMs: 90 * 60 * 1000 };
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
    case "pending_decision":
      return coerceTimestampMs(card.review_entered_at ?? card.updated_at ?? card.started_at ?? card.created_at);
    case "done":
      return coerceTimestampMs(card.completed_at ?? card.updated_at ?? card.created_at);
    case "blocked":
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

// ---------------------------------------------------------------------------
// GitHub Comment Timeline Parser
// ---------------------------------------------------------------------------

export type GitHubTimelineKind = "review" | "pm" | "work" | "general";
export type GitHubTimelineStatus =
  | "reviewing"
  | "changes_requested"
  | "passed"
  | "decision"
  | "completed"
  | "comment";

export interface ParsedGitHubComment {
  kind: GitHubTimelineKind;
  status: GitHubTimelineStatus;
  title: string;
  summary: string | null;
  details: string[];
  createdAt: string;
  author: string;
  body: string;
}

export interface CoalescedGitHubTimelineItem {
  id: string;
  kind: GitHubTimelineKind;
  status: GitHubTimelineStatus;
  author: string;
  createdAt: string;
  updatedAt: string;
  entries: ParsedGitHubComment[];
  coalesced: boolean;
  highlights: string[];
}

const TIMELINE_COALESCE_WINDOW_MS = 2 * 60_000;

const TIMELINE_COALESCE_INCLUDE_PATTERNS = [
  /상태.*(변경|전환|업데이트)/u,
  /(변경|전환|업데이트).*(상태|메타데이터|우선순위|라벨|체크리스트|태그)/u,
  /메타데이터.*(변경|수정|업데이트)/u,
  /우선순위.*(변경|수정|업데이트)/u,
  /라벨.*(변경|수정|업데이트)/u,
  /체크리스트.*(변경|수정|업데이트)/u,
  /\bstatus\b.*(changed|updated|set)/i,
  /\bmetadata\b.*(changed|updated|set)/i,
  /\bpriority\b.*(changed|updated|set)/i,
  /\blabels?\b.*(changed|updated|set)/i,
  /\bchecklist\b.*(changed|updated|set)/i,
];

const TIMELINE_COALESCE_EXCLUDE_PATTERNS = [
  /에이전트.*할당/u,
  /담당자.*변경/u,
  /담당 에이전트/u,
  /할당 변경/u,
  /assignee/i,
  /assigned agent/i,
  /assignment/i,
  /리뷰 결정/u,
  /review decision/i,
  /pm 결정/u,
  /pm decision/i,
  /verdict/i,
];

function cleanMarkdownLine(line: string): string {
  return line
    .replace(/^#+(?:\s+|$)/, "")
    .replace(/^\d+\.\s+/, "")
    .replace(/^[-*]\s+/, "")
    .replace(/\*\*/g, "")
    .replace(/`/g, "")
    .trim();
}

function firstMeaningfulLine(body: string): string | null {
  for (const raw of body.split("\n")) {
    const line = cleanMarkdownLine(raw);
    if (!line) continue;
    if (line === "---") continue;
    return line;
  }
  return null;
}

function extractListHighlights(body: string, limit = 3): string[] {
  const results: string[] = [];
  for (const raw of body.split("\n")) {
    if (!/^\s*(\d+\.\s+|[-*]\s+)/.test(raw)) continue;
    const line = cleanMarkdownLine(raw);
    if (!line) continue;
    results.push(line);
    if (results.length >= limit) break;
  }
  return results;
}

function extractSectionHeadings(body: string, limit = 3): string[] {
  const results: string[] = [];
  for (const raw of body.split("\n")) {
    const match = raw.match(/^#{2,6}\s+(.+)$/);
    if (!match) continue;
    const line = cleanMarkdownLine(match[1]);
    if (!line || line.includes("완료 보고")) continue;
    results.push(line);
    if (results.length >= limit) break;
  }
  return results;
}

function filterCommentBody(body: string): string {
  const lines: string[] = [];
  let inCodeBlock = false;
  for (const raw of body.split("\n")) {
    const trimmed = raw.trim();
    if (trimmed.startsWith("```")) {
      inCodeBlock = !inCodeBlock;
      continue;
    }
    if (inCodeBlock || trimmed.startsWith(">")) continue;
    lines.push(raw);
  }
  return lines.join("\n");
}

function getHeading(filteredBody: string): string | null {
  for (const raw of filteredBody.split("\n")) {
    const match = raw.match(/^##\s+(.+)$/);
    if (match) return cleanMarkdownLine(match[1]);
  }
  return null;
}

function getMeaningfulLines(body: string): string[] {
  return body
    .split("\n")
    .map((raw) => cleanMarkdownLine(raw))
    .filter((line) => line && line !== "---");
}

const REVIEW_PASS_PATTERNS = [
  "추가 blocking finding은 없습니다",
  "현재 diff 기준으로 머지를 막을 추가 결함은 확인하지 못했습니다",
  "머지를 막을 추가 결함은 확인하지 못했습니다",
  "추가 결함은 확인하지 못했습니다",
];

const REVIEW_BLOCKING_PATTERNS = [
  /blocking finding\s*\d+건/i,
  /blocking \d+건/i,
  /확인된 이슈 \d+건/,
  /결함 \d+건/,
  /문제 \d+건/,
  /남아 있습니다/,
];

const REVIEW_FEEDBACK_PREFIXES = [
  "리뷰했습니다",
  "추가 리뷰했습니다",
  "추가 구현분 리뷰했습니다",
  "재확인했습니다",
  "재검토했습니다",
  "코드 리뷰 결과",
  "코드 검토 결과",
  "리뷰 결과",
  "검토 결과",
];

/** Broader review-signal keywords — used for secondary heuristic detection */
const REVIEW_SIGNAL_KEYWORDS = [
  /리뷰[를을]?\s*(완료|진행|확인)/,
  /검토[를을]?\s*(완료|진행|확인)/,
  /코드\s*리뷰/,
  /code\s*review/i,
  /reviewed?\b/i,
];

const PM_MARKER_PATTERNS = [
  /^PM 결정(?:[:\s]|$)/u,
  /^PM 판단(?:[:\s]|$)/u,
  /^PMD 결정(?:[:\s]|$)/u,
  /^PMD 판단(?:[:\s]|$)/u,
  /^프로듀서 결정(?:[:\s]|$)/u,
  /^프로듀서 판단(?:[:\s]|$)/u,
  /^PM Decision\b/i,
  /^PM Verdict\b/i,
];

const WORK_HEADING_PATTERNS = [
  /완료 보고/u,
  /^#\d+\s+작업 완료(?:[:\s]|$)/u,
  /^작업 완료(?:[:\s]|$)/u,
];

function matchesAny(text: string, patterns: RegExp[]): boolean {
  return patterns.some((pattern) => pattern.test(text));
}

function timelineCoalescingText(entry: ParsedGitHubComment): string {
  return [entry.title, entry.summary ?? "", entry.body].join("\n");
}

function isTimelineCoalesceEligible(entry: ParsedGitHubComment): boolean {
  if (entry.kind !== "general") return false;
  const text = timelineCoalescingText(entry);
  if (matchesAny(text, TIMELINE_COALESCE_EXCLUDE_PATTERNS)) return false;
  return matchesAny(text, TIMELINE_COALESCE_INCLUDE_PATTERNS);
}

function buildCoalescedTimelineItem(
  entries: ParsedGitHubComment[],
  index: number,
): CoalescedGitHubTimelineItem {
  const first = entries[0];
  const last = entries[entries.length - 1];
  const highlights = Array.from(
    new Set(
      entries
        .flatMap((entry) => [entry.title, entry.summary ?? ""])
        .map((value) => cleanMarkdownLine(value))
        .filter(Boolean),
    ),
  ).slice(0, 3);

  return {
    id: `${first.author}-${first.createdAt}-${index}`,
    kind: first.kind,
    status: first.status,
    author: first.author,
    createdAt: first.createdAt,
    updatedAt: last.createdAt,
    entries,
    coalesced: entries.length > 1,
    highlights,
  };
}

export function parseGitHubCommentTimeline(comments: GitHubComment[]): ParsedGitHubComment[] {
  return comments.flatMap<ParsedGitHubComment>((comment) => {
    const body = comment.body.trim();
    if (!body) return [];

    const filteredBody = filterCommentBody(body);
    const classificationBody = filteredBody.trim() || body;
    const meaningfulLines = getMeaningfulLines(classificationBody);
    const firstLine = meaningfulLines[0] ?? firstMeaningfulLine(body);
    const heading = getHeading(classificationBody);
    const leadText = meaningfulLines.slice(0, 3).join(" ");
    const author = comment.author?.login ?? "unknown";

    if (body.startsWith("🔍 칸반 상태:") || firstLine?.startsWith("🔍 칸반 상태:")) {
      return [{
        kind: "review",
        status: "reviewing",
        title: "리뷰 진행",
        summary: cleanMarkdownLine(firstLine ?? body),
        details: [],
        createdAt: comment.createdAt,
        author,
        body,
      }];
    }

    // Pre-compute work heading to guard secondary review heuristic
    const workMarker = heading ?? firstLine ?? "";
    const isWorkHeading = heading ? matchesAny(heading, WORK_HEADING_PATTERNS) : false;
    const isWorkLead =
      (!heading && (workMarker.includes("완료 보고") || /^#\d+\s+작업 완료(?:[:\s]|$)/u.test(workMarker)))
      || workMarker.startsWith("구현 완료")
      || workMarker.startsWith("수정 완료")
      || workMarker.startsWith("배포 완료");
    const isWorkComment = isWorkHeading || isWorkLead;

    const passed = REVIEW_PASS_PATTERNS.some((pattern) => leadText.includes(pattern))
      && !matchesAny(leadText, REVIEW_BLOCKING_PATTERNS);
    const reviewFeedbackExplicit =
      REVIEW_FEEDBACK_PREFIXES.some((prefix) => leadText.startsWith(prefix))
      || leadText.includes("재검토 결과")
      || /blocking finding/i.test(leadText)
      || /blocking \d+건/i.test(leadText)
      || /확인된 이슈 \d+건/.test(leadText)
      || /결함 \d+건/.test(leadText)
      || /문제 \d+건/.test(leadText);
    // Secondary heuristic: review keyword + numbered code-reference list
    // Guard: skip if heading matches work patterns (e.g. "완료 보고" with code refs)
    const hasReviewSignal = !isWorkComment && matchesAny(leadText, REVIEW_SIGNAL_KEYWORDS);
    const hasNumberedFindings = /^\s*\d+\.\s+/m.test(classificationBody)
      && /`[^`]+\.\w+[:`]/.test(classificationBody); // file reference like `foo.rs:123`
    const reviewFeedback = reviewFeedbackExplicit
      || (hasReviewSignal && hasNumberedFindings)
      || (hasReviewSignal && matchesAny(leadText, REVIEW_BLOCKING_PATTERNS));

    if (passed || reviewFeedback) {
      const highlights = extractListHighlights(body, passed ? 1 : 3);
      return [{
        kind: "review",
        status: passed ? "passed" : "changes_requested",
        title: passed ? "리뷰 통과" : "리뷰 피드백",
        summary: highlights[0] ?? cleanMarkdownLine(firstLine ?? "리뷰 결과"),
        details: passed ? [] : highlights.slice(1),
        createdAt: comment.createdAt,
        author,
        body,
      }];
    }

    const pmMarker = heading ?? firstLine ?? "";
    if (matchesAny(pmMarker, PM_MARKER_PATTERNS)) {
      return [{
        kind: "pm",
        status: "decision",
        title: heading ?? "PM 결정",
        summary: cleanMarkdownLine(firstLine ?? "PM 결정"),
        details: extractListHighlights(body, 3),
        createdAt: comment.createdAt,
        author,
        body,
      }];
    }

    if (isWorkComment) {
      return [{
        kind: "work",
        status: "completed",
        title: heading ?? "작업 완료",
        summary: cleanMarkdownLine(firstLine ?? "작업 완료"),
        details: extractSectionHeadings(body, 3),
        createdAt: comment.createdAt,
        author,
        body,
      }];
    }

    // Fallback: unrecognized comments shown as "general" type
    const truncated = body.length > 200 ? body.slice(0, 200) + "…" : body;
    return [{
      kind: "general",
      status: "comment",
      title: heading ?? cleanMarkdownLine(firstLine ?? "코멘트"),
      summary: truncated,
      details: [],
      createdAt: comment.createdAt,
      author,
      body,
    }];
  });
}

export function coalesceGitHubCommentTimeline(
  entries: ParsedGitHubComment[],
): CoalescedGitHubTimelineItem[] {
  const groups: ParsedGitHubComment[][] = [];
  let currentGroup: ParsedGitHubComment[] = [];
  let groupStartMs = Number.NaN;

  const flushGroup = () => {
    if (currentGroup.length === 0) return;
    groups.push(currentGroup);
    currentGroup = [];
    groupStartMs = Number.NaN;
  };

  for (const entry of entries) {
    const entryTs = Date.parse(entry.createdAt);
    const eligible = Number.isFinite(entryTs) && isTimelineCoalesceEligible(entry);

    if (!eligible) {
      flushGroup();
      groups.push([entry]);
      continue;
    }

    if (currentGroup.length === 0) {
      currentGroup = [entry];
      groupStartMs = entryTs;
      continue;
    }

    const sameAuthor = currentGroup[0]?.author === entry.author;
    const withinWindow = entryTs - groupStartMs <= TIMELINE_COALESCE_WINDOW_MS;

    if (sameAuthor && withinWindow) {
      currentGroup.push(entry);
      continue;
    }

    flushGroup();
    currentGroup = [entry];
    groupStartMs = entryTs;
  }

  flushGroup();

  return groups.map((group, index) => buildCoalescedTimelineItem(group, index));
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

export function labelForStatus(status: KanbanCardStatus, tr: (ko: string, en: string) => string): string {
  const col = COLUMN_DEFS.find((column) => column.status === status);
  return col ? tr(col.labelKo, col.labelEn) : status;
}
