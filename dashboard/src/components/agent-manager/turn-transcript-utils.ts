import type { SessionTranscript } from "../../api";

const UUID_LIKE_RE =
  /\b[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\b/i;
const BRACKETED_PREFIX_RE = /^(\[[^\]]+\])\s+(.+)$/;

export function parseTranscriptDate(value: string | null | undefined): Date | null {
  if (!value) return null;
  const normalized = value.includes("T") ? value : value.replace(" ", "T");
  const parsed = new Date(normalized);
  return Number.isNaN(parsed.getTime()) ? null : parsed;
}

export function formatTranscriptTimestamp(value: string, isKo: boolean): string {
  const parsed = parseTranscriptDate(value);
  if (!parsed) return value;
  return parsed.toLocaleString(isKo ? "ko-KR" : "en-US", {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function buildTranscriptCardLabel(
  transcript: Pick<SessionTranscript, "card_title" | "github_issue_number">,
): string | null {
  const title = transcript.card_title?.trim() || null;
  const issueNumber = transcript.github_issue_number;
  if (issueNumber != null && title) return `#${issueNumber} ${title}`;
  if (issueNumber != null) return `#${issueNumber}`;
  return title;
}

function isRawCardIdentifier(
  value: string,
  transcript: Pick<SessionTranscript, "kanban_card_id">,
): boolean {
  const trimmed = value.trim();
  if (!trimmed) return false;
  if (transcript.kanban_card_id && trimmed === transcript.kanban_card_id) return true;
  return UUID_LIKE_RE.test(trimmed);
}

export function transcriptSelectionLabel(
  transcript: Pick<
    SessionTranscript,
    | "dispatch_title"
    | "card_title"
    | "github_issue_number"
    | "kanban_card_id"
    | "created_at"
  >,
  isKo: boolean,
): string {
  const dispatchTitle = transcript.dispatch_title?.trim() || "";
  const cardLabel = buildTranscriptCardLabel(transcript);

  if (dispatchTitle) {
    const bracketed = dispatchTitle.match(BRACKETED_PREFIX_RE);
    if (bracketed) {
      const [, prefix, detail] = bracketed;
      if (cardLabel && isRawCardIdentifier(detail, transcript)) {
        return `${prefix} ${cardLabel}`;
      }
      if (!cardLabel && isRawCardIdentifier(detail, transcript)) {
        return prefix;
      }
    }

    if (cardLabel && isRawCardIdentifier(dispatchTitle, transcript)) {
      return cardLabel;
    }
    return dispatchTitle;
  }

  return cardLabel ?? formatTranscriptTimestamp(transcript.created_at, isKo);
}

export function normalizeActiveEventIndex(
  activeIndex: number | null,
  eventCount: number,
): number | null {
  if (eventCount <= 0) return null;
  if (activeIndex == null || activeIndex < 0) return 0;
  return Math.min(activeIndex, eventCount - 1);
}
