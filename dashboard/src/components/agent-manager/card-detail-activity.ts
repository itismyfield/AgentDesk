import type { KanbanCardStatus } from "../../types";
import { labelForStatus } from "./kanban-utils";

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
