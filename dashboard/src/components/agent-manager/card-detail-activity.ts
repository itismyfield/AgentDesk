import type { CardAuditLogEntry, GitHubComment } from "../../api";
import type { KanbanCard, KanbanCardStatus, TaskDispatch, UiLanguage } from "../../types";
import { coerceTimestampMs, labelForStatus } from "./kanban-utils";
import {
  coalesceGitHubCommentTimeline,
  parseGitHubCommentTimeline,
  type CoalescedGitHubTimelineItem,
  type GitHubTimelineKind,
  type GitHubTimelineStatus,
} from "./kanban-timeline-utils";

type Translator = (ko: string, en: string) => string;

export interface ActivityTextPresentation {
  text: string;
  tone: "default" | "warn" | "danger";
}

type ActivityTone = ActivityTextPresentation["tone"];

const KNOWN_KANBAN_STATUSES = new Set<KanbanCardStatus>([
  "backlog",
  "ready",
  "requested",
  "blocked",
  "in_progress",
  "review",
  "done",
  "qa_pending",
  "qa_in_progress",
  "qa_failed",
]);

function cleanText(value: string | null | undefined): string | null {
  if (!value) return null;
  const lines = value
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean);
  return lines.length > 0 ? lines.join("\n") : null;
}

function parseJsonSafely(value: string): unknown | null {
  try {
    return JSON.parse(value) as unknown;
  } catch {
    return null;
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function firstStringField(record: Record<string, unknown>, keys: string[]): string | null {
  for (const key of keys) {
    const value = record[key];
    if (typeof value === "string") {
      const text = cleanText(value);
      if (text) return text;
    }
  }
  return null;
}

function renderStatusLabel(status: string, tr: Translator): string {
  if (KNOWN_KANBAN_STATUSES.has(status as KanbanCardStatus)) {
    return labelForStatus(status as KanbanCardStatus, tr);
  }
  return status.replace(/_/g, " ");
}

function humanizeReasonText(reason: string, tr: Translator): string {
  switch (reason) {
    case "auto_cancelled_on_terminal_card":
      return tr("터미널 카드 정리로 자동 취소", "Auto-cancelled during terminal cleanup");
    case "manual reopen":
      return tr("수동 재오픈", "Manual reopen");
    default:
      break;
  }

  const forceTransition = reason.match(/^force-transition to ([a-z_]+)$/);
  if (forceTransition) {
    const target = renderStatusLabel(forceTransition[1], tr);
    return tr(`강제 전환 정리: ${target}`, `Forced transition cleanup: ${target}`);
  }

  if (reason.includes("_") && !reason.includes(" ") && !reason.includes(":")) {
    return reason.replace(/_/g, " ");
  }

  return reason;
}

function humanizeDecisionLabel(decision: string, tr: Translator): string | null {
  switch (decision) {
    case "accept":
      return tr("리뷰 피드백 수용", "Accepted review feedback");
    case "reject":
      return tr("리뷰 피드백 거부", "Rejected review feedback");
    case "dispute":
      return tr("리뷰 피드백 이견 제기", "Disputed review feedback");
    case "dismiss":
      return tr("리뷰 피드백 기각", "Dismissed review feedback");
    case "rework":
      return tr("재작업 요청", "Requested rework");
    case "cancel":
      return tr("취소", "Cancelled");
    default:
      return null;
  }
}

function humanizePmDecisionLabel(decision: string, tr: Translator): string | null {
  switch (decision) {
    case "rework":
      return tr("PM 재작업 요청", "PM requested rework");
    case "accept":
      return tr("PM 승인", "PM accepted");
    case "cancel":
      return tr("PM 취소", "PM cancelled");
    default:
      return null;
  }
}

function humanizeVerdictLabel(verdict: string, tr: Translator): string | null {
  switch (verdict) {
    case "pass":
      return tr("리뷰 통과", "Review passed");
    case "improve":
      return tr("리뷰 개선 요청", "Review requested improvement");
    case "rework":
      return tr("리뷰 재작업 요청", "Review requested rework");
    case "reject":
      return tr("리뷰 거절", "Review rejected");
    default:
      return null;
  }
}

function mergeToneHints(...tones: Array<ActivityTone | null | undefined>): ActivityTone | null {
  let resolved: ActivityTone | null = null;
  for (const tone of tones) {
    if (tone === "danger") return "danger";
    if (tone === "warn") resolved = "warn";
    else if (tone === "default" && !resolved) resolved = "default";
  }
  return resolved;
}

function normalizedCode(value: string | null | undefined): string | null {
  const text = cleanText(value);
  return text ? text.toLowerCase() : null;
}

function toneForDecision(decision: string | null | undefined): ActivityTone | null {
  switch (normalizedCode(decision)) {
    case "accept":
      return "default";
    case "reject":
    case "dispute":
    case "dismiss":
    case "rework":
    case "cancel":
      return "warn";
    default:
      return null;
  }
}

function toneForPmDecision(decision: string | null | undefined): ActivityTone | null {
  switch (normalizedCode(decision)) {
    case "accept":
      return "default";
    case "rework":
    case "cancel":
      return "warn";
    default:
      return null;
  }
}

function toneForStatusCode(status: string | null | undefined): ActivityTone | null {
  const normalized = normalizedCode(status);
  if (!normalized) return null;

  switch (normalized) {
    case "ok":
    case "pass":
    case "passed":
    case "approved":
    case "success":
    case "succeeded":
    case "completed":
    case "phase_gate_passed":
      return "default";
    case "cancel":
    case "cancelled":
    case "canceled":
      return "warn";
    case "blocked":
    case "fail":
    case "failed":
    case "error":
      return "danger";
    default:
      break;
  }

  if (normalized.startsWith("blocked:") || normalized.startsWith("blocked.")) {
    return "danger";
  }
  if (/(^|.*_)(pass|passed|approved|success|succeeded)$/.test(normalized)) {
    return "default";
  }
  if (/(^|.*_)(cancel|cancelled|canceled)$/.test(normalized)) {
    return "warn";
  }
  if (/(^|.*_)(fail|failed|error)$/.test(normalized)) {
    return "danger";
  }
  return null;
}

function toneForVerdict(verdict: string | null | undefined): ActivityTone | null {
  switch (normalizedCode(verdict)) {
    case "pass":
    case "approved":
      return "default";
    case "improve":
    case "rework":
    case "reject":
      return "warn";
    default:
      return toneForStatusCode(verdict);
  }
}

function toneForReasonCode(reason: string | null | undefined): ActivityTone | null {
  const normalized = normalizedCode(reason);
  if (!normalized) return null;

  switch (normalized) {
    case "auto_cancelled_on_terminal_card":
      return "warn";
    case "manual reopen":
      return "default";
    default:
      break;
  }

  if (/^force-transition to [a-z_]+$/.test(normalized)) {
    return "warn";
  }
  if (normalized.startsWith("blocked:") || normalized.startsWith("blocked.")) {
    return "danger";
  }
  return null;
}

function toneForPlainResult(result: string): ActivityTone | null {
  const normalized = normalizedCode(result);
  if (!normalized) return null;

  switch (normalized) {
    case "ok (force)":
      return "warn";
    case "review passed":
      return "default";
    default:
      return mergeToneHints(
        toneForReasonCode(normalized),
        toneForVerdict(normalized),
        toneForStatusCode(normalized),
      );
  }
}

function buildPresentation(
  headline: string | null,
  detail: string | null,
  toneHint: ActivityTone | null = null,
): ActivityTextPresentation | null {
  if (headline && detail && detail !== headline) {
    return {
      text: `${headline}: ${detail}`,
      tone: toneHint ?? "default",
    };
  }
  if (headline) {
    return { text: headline, tone: toneHint ?? "default" };
  }
  if (detail) {
    return { text: detail, tone: toneHint ?? "default" };
  }
  return null;
}

function presentationFromRecord(record: Record<string, unknown>, tr: Translator): ActivityTextPresentation | null {
  const decision = firstStringField(record, ["decision"]);
  const pmDecision = firstStringField(record, ["pm_decision"]);
  const verdict = firstStringField(record, ["verdict"]);
  const status = firstStringField(record, ["status"]);
  const reasonCode = firstStringField(record, ["reason", "noop_reason"]);
  const detail = firstStringField(record, [
    "comment",
    "reason",
    "result_summary",
    "summary",
    "message",
    "final_message",
    "noop_reason",
    "notes",
    "content",
  ]);

  const headline =
    (pmDecision && humanizePmDecisionLabel(pmDecision, tr))
    || (decision && humanizeDecisionLabel(decision, tr))
    || (verdict && humanizeVerdictLabel(verdict, tr))
    || null;

  const normalizedDetail = detail ? humanizeReasonText(detail, tr) : null;
  const toneHint = mergeToneHints(
    toneForPmDecision(pmDecision),
    toneForDecision(decision),
    toneForVerdict(verdict),
    toneForStatusCode(status),
    toneForReasonCode(reasonCode),
  );
  const presentation = buildPresentation(headline, normalizedDetail, toneHint);
  if (presentation) return presentation;

  for (const key of ["result", "details", "payload"]) {
    const nested = record[key];
    if (typeof nested === "string") {
      const nestedText = cleanText(nested);
      if (nestedText) {
        return {
          text: humanizeReasonText(nestedText, tr),
          tone: toneForPlainResult(nestedText) ?? "default",
        };
      }
    }
    if (isRecord(nested)) {
      const nestedPresentation = presentationFromRecord(nested, tr);
      if (nestedPresentation) return nestedPresentation;
    }
  }

  return null;
}

function humanizePlainResult(result: string, tr: Translator): string {
  switch (result) {
    case "OK (force)":
      return tr("강제 전환", "Forced transition");
    case "review passed":
      return tr("리뷰 통과", "Review passed");
    default:
      break;
  }

  if (result.startsWith("BLOCKED:")) {
    const detail = result.slice("BLOCKED:".length).trim();
    return tr(`차단됨: ${detail}`, `Blocked: ${detail}`);
  }

  return humanizeReasonText(result, tr);
}

export function formatDispatchSummary(summary: string | null | undefined): string | null {
  return cleanText(summary);
}

export function formatAuditResult(
  result: string | null | undefined,
  tr: Translator,
): ActivityTextPresentation | null {
  const normalized = cleanText(result);
  if (!normalized || normalized === "OK") {
    return null;
  }

  const parsed = parseJsonSafely(normalized);
  if (typeof parsed === "string") {
    return formatAuditResult(parsed, tr);
  }
  if (Array.isArray(parsed)) {
    for (const item of parsed) {
      if (typeof item === "string") {
        const presentation = formatAuditResult(item, tr);
        if (presentation) return presentation;
      } else if (isRecord(item)) {
        const presentation = presentationFromRecord(item, tr);
        if (presentation) return presentation;
      }
    }
  }
  if (isRecord(parsed)) {
    const presentation = presentationFromRecord(parsed, tr);
    if (presentation) return presentation;
  }

  const text = humanizePlainResult(normalized, tr);
  return { text, tone: toneForPlainResult(normalized) ?? "default" };
}

// ---------------------------------------------------------------------------
// Unified card activity: dispatches, state transitions and GitHub comments in one dated list.
// ---------------------------------------------------------------------------

export type CardActivitySource = "status" | "dispatch" | GitHubTimelineKind;
export type CardActivityFilter = "all" | CardActivitySource;
export type CardActivityTone = "neutral" | "info" | "success" | "warning" | "danger" | "purple";

export const CARD_ACTIVITY_SOURCES: CardActivitySource[] = ["status", "dispatch", "review", "pm", "work", "general"];

export type CardActivityDetail =
  | { kind: "status"; from: string | null; to: string | null; origin: string | null; result: ActivityTextPresentation | null }
  | { kind: "dispatch"; dispatch: TaskDispatch; summary: string | null; latest: boolean }
  | { kind: "comment"; item: CoalescedGitHubTimelineItem };

export interface CardActivityRecord {
  id: string;
  source: CardActivitySource;
  /** Unix ms; null when the server sent no usable time. */
  at: number | null;
  /** The one-line sentence of the row; the detail shows it in full. */
  text: string;
  /** Who or what acted: a comment author, an audit source or the receiving agent. */
  actor: string | null;
  /** A short second fact after the actor, e.g. a dispatch type. */
  sub: string | null;
  badge: { label: string; tone: CardActivityTone; live?: boolean } | null;
  detail: CardActivityDetail;
}

export function cardActivitySourceLabel(source: CardActivitySource, tr: Translator): string {
  switch (source) {
    case "status": return tr("상태 전환", "Status");
    case "dispatch": return tr("디스패치", "Dispatch");
    case "review": return tr("리뷰", "Review");
    case "pm": return tr("PM 결정", "PM Decision");
    case "work": return tr("작업 이력", "Work Log");
    case "general": return tr("코멘트", "Comment");
  }
}

export function dispatchStatusPresentation(status: string, tr: Translator): { label: string; tone: CardActivityTone; live?: boolean } {
  switch (status) {
    case "pending": return { label: tr("대기", "Pending"), tone: "warning" };
    case "dispatched": return { label: tr("전송됨", "Dispatched"), tone: "info" };
    case "in_progress": return { label: tr("진행 중", "In progress"), tone: "info", live: true };
    case "completed": return { label: tr("완료", "Completed"), tone: "success" };
    case "failed": return { label: tr("실패", "Failed"), tone: "danger" };
    case "cancelled": return { label: tr("취소됨", "Cancelled"), tone: "neutral" };
    default: return { label: status.replace(/_/g, " "), tone: "neutral" };
  }
}

export function timelineStatusPresentation(status: GitHubTimelineStatus, tr: Translator): { label: string; tone: CardActivityTone } | null {
  switch (status) {
    case "reviewing": return { label: tr("진행 중", "In progress"), tone: "info" };
    case "changes_requested": return { label: tr("수정 필요", "Changes requested"), tone: "warning" };
    case "passed": return { label: tr("통과", "Passed"), tone: "success" };
    case "decision": return { label: tr("결정", "Decision"), tone: "purple" };
    case "completed": return { label: tr("완료", "Completed"), tone: "success" };
    case "comment": return null;
  }
}

function auditBadge(result: ActivityTextPresentation | null, tr: Translator): CardActivityRecord["badge"] {
  if (result?.tone === "danger") return { label: tr("문제", "Problem"), tone: "danger" };
  if (result?.tone === "warn") return { label: tr("주의", "Notice"), tone: "warning" };
  return null;
}

function statusText(status: string | null, tr: Translator): string {
  return status ? renderStatusLabel(status, tr) : "—";
}

function commentRecord(item: CoalescedGitHubTimelineItem, tr: Translator): CardActivityRecord {
  const first = item.entries[0];
  const at = Date.parse(item.createdAt);
  if (item.coalesced) {
    const count = tr(`${item.entries.length}개 변경`, `${item.entries.length} changes`);
    return {
      id: `comment:${item.id}`, source: item.kind, at: Number.isFinite(at) ? at : null,
      text: item.highlights.length > 0 ? `${count} · ${item.highlights.join(" · ")}` : count,
      actor: item.author, sub: null, badge: { label: tr("합산", "Coalesced"), tone: "neutral" },
      detail: { kind: "comment", item },
    };
  }
  const summary = first.summary && first.summary !== first.title && first.kind !== "general" ? first.summary : null;
  return {
    id: `comment:${item.id}`, source: item.kind, at: Number.isFinite(at) ? at : null,
    text: summary ? `${first.title}: ${summary}` : first.title,
    actor: item.author, sub: null, badge: timelineStatusPresentation(first.status, tr),
    detail: { kind: "comment", item },
  };
}

/** Builds the card's activity newest first; records without a time sink to the end in arrival order. */
export function buildCardActivity({
  card,
  dispatches,
  auditLog,
  ghComments,
  tr,
  getAgentLabel,
}: {
  card: Pick<KanbanCard, "id" | "latest_dispatch_id">;
  dispatches: TaskDispatch[];
  auditLog: CardAuditLogEntry[];
  ghComments: GitHubComment[];
  tr: Translator;
  getAgentLabel: (agentId: string | null | undefined) => string;
}): CardActivityRecord[] {
  const records: CardActivityRecord[] = [];
  for (const dispatch of dispatches) {
    if (dispatch.kanban_card_id !== card.id) continue;
    const target = dispatch.to_agent_id ? getAgentLabel(dispatch.to_agent_id) : null;
    const title = cleanText(dispatch.title);
    records.push({
      id: `dispatch:${dispatch.id}`, source: "dispatch", at: coerceTimestampMs(dispatch.created_at),
      text: title ?? (target ? `${dispatch.dispatch_type} → ${target}` : dispatch.dispatch_type),
      actor: target, sub: [`#${dispatch.id.slice(0, 8)}`, dispatch.dispatch_type].filter(Boolean).join(" · "),
      badge: dispatchStatusPresentation(dispatch.status, tr),
      detail: { kind: "dispatch", dispatch, summary: formatDispatchSummary(dispatch.result_summary), latest: dispatch.id === card.latest_dispatch_id },
    });
  }
  for (const log of auditLog) {
    const result = formatAuditResult(log.result, tr);
    records.push({
      id: `status:${log.id}`, source: "status", at: coerceTimestampMs(log.created_at),
      text: `${statusText(log.from_status, tr)} → ${statusText(log.to_status, tr)}`,
      actor: cleanText(log.source), sub: result?.text.split("\n")[0] ?? null, badge: auditBadge(result, tr),
      detail: { kind: "status", from: log.from_status, to: log.to_status, origin: cleanText(log.source), result },
    });
  }
  for (const item of coalesceGitHubCommentTimeline(parseGitHubCommentTimeline(ghComments))) records.push(commentRecord(item, tr));
  return records
    .map((record, index) => ({ record, index }))
    .sort((a, b) => {
      if (a.record.at == null || b.record.at == null) return a.record.at == null ? (b.record.at == null ? a.index - b.index : 1) : -1;
      return b.record.at - a.record.at || a.index - b.index;
    })
    .map(({ record }) => record);
}

export function countCardActivity(records: CardActivityRecord[]): Record<CardActivityFilter, number> {
  const counts = { all: records.length, status: 0, dispatch: 0, review: 0, pm: 0, work: 0, general: 0 } satisfies Record<CardActivityFilter, number>;
  for (const record of records) counts[record.source] += 1;
  return counts;
}

/** Filter by source, then match the query against the sentence, actor, second fact and source label. */
export function filterCardActivity(records: CardActivityRecord[], filter: CardActivityFilter, query: string, tr: Translator): CardActivityRecord[] {
  const needle = query.trim().toLowerCase();
  return records.filter((record) => {
    if (filter !== "all" && record.source !== filter) return false;
    if (!needle) return true;
    return [record.text, record.actor ?? "", record.sub ?? "", cardActivitySourceLabel(record.source, tr)].join(" ").toLowerCase().includes(needle);
  });
}

const DAY_MS = 86_400_000;
const localMidnight = (time: number) => { const date = new Date(time); date.setHours(0, 0, 0, 0); return date.getTime(); };

/** Groups records that share a local day, keeping their order; timeless records form the last group. */
export function groupCardActivityByDay(records: CardActivityRecord[]): { key: number | null; items: CardActivityRecord[] }[] {
  const groups: { key: number | null; items: CardActivityRecord[] }[] = [];
  for (const record of records) {
    const key = record.at == null ? null : localMidnight(record.at);
    const last = groups.at(-1);
    if (last && last.key === key) last.items.push(record);
    else groups.push({ key, items: [record] });
  }
  return groups;
}

/** Today and yesterday by name, other days as a short date with the weekday. */
export function cardActivityDayLabel(key: number | null, now: number, locale: UiLanguage, tr: Translator): string {
  if (key == null) return tr("시각 없음", "No time");
  const diff = Math.round((localMidnight(now) - key) / DAY_MS);
  if (diff <= 0) return tr("오늘", "Today");
  if (diff === 1) return tr("어제", "Yesterday");
  return new Intl.DateTimeFormat(locale, { month: "short", day: "numeric", weekday: "long" }).format(key);
}

export function cardActivityClock(at: number | null, locale: UiLanguage): string {
  if (at == null) return "—";
  return new Intl.DateTimeFormat(locale, { hour: "2-digit", minute: "2-digit", hour12: false }).format(at);
}
