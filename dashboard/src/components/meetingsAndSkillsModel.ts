import { useEffect, useState } from "react";
import type { I18nContextValue } from "../i18n";
import type { MeetingIssueState } from "../lib/meetingHelpers";
import type { RoundTableMeeting } from "../types";

export type MeetingNotificationType =
  | "info"
  | "success"
  | "warning"
  | "error";
export type MeetingNotifier = (
  message: string,
  type?: MeetingNotificationType,
) => string | void;
export type MeetingNotificationUpdater = (
  id: string,
  message: string,
  type?: MeetingNotificationType,
) => void;
export type MobilePane = "meetings" | "skills";
export type MeetingStatusFilter =
  | "all"
  | RoundTableMeeting["status"]
  | "open_issues";

const DESKTOP_SPLIT_QUERY = "(min-width: 1024px)";

export const MEETING_TOGGLE_PATTERNS = [
  /new meeting/i,
  /새 회의/u,
  /close form/i,
  /입력 닫기/u,
];

function normalizeNodeLabel(node: Element): string {
  const text = node.textContent ?? "";
  const title = node.getAttribute("title") ?? "";
  const ariaLabel = node.getAttribute("aria-label") ?? "";
  return `${text} ${title} ${ariaLabel}`.replace(/\s+/g, " ").trim();
}

export function clickMatchingButton(
  root: HTMLElement | null,
  patterns: readonly RegExp[],
): boolean {
  if (!root) return false;

  const buttons = Array.from(root.querySelectorAll("button"));
  const target = buttons.find((button) =>
    patterns.some((pattern) => pattern.test(normalizeNodeLabel(button))),
  );

  if (!target) return false;
  target.click();
  return true;
}

export function useDesktopSplitLayout(): boolean {
  const [isDesktopSplit, setIsDesktopSplit] = useState(() => {
    if (typeof window === "undefined") return false;
    return window.matchMedia(DESKTOP_SPLIT_QUERY).matches;
  });

  useEffect(() => {
    if (typeof window === "undefined") return;
    const mediaQuery = window.matchMedia(DESKTOP_SPLIT_QUERY);
    const handleChange = (event: MediaQueryListEvent) => {
      setIsDesktopSplit(event.matches);
    };

    setIsDesktopSplit(mediaQuery.matches);
    mediaQuery.addEventListener("change", handleChange);
    return () => mediaQuery.removeEventListener("change", handleChange);
  }, []);

  return isDesktopSplit;
}

export function formatMeetingDate(timestamp: number, locale: string): string {
  return new Intl.DateTimeFormat(locale, {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  }).format(new Date(timestamp));
}

export function formatRelativeTime(
  timestamp: number,
  language: string,
  locale: string,
): string {
  const isKo = language === "ko";
  const diffMs = Date.now() - timestamp;
  const hours = Math.max(0, Math.floor(diffMs / (1000 * 60 * 60)));
  if (hours < 1) return isKo ? "방금" : "Just now";
  if (hours < 24) return isKo ? `${hours}시간 전` : `${hours}h ago`;

  const days = Math.floor(hours / 24);
  if (days === 1) return isKo ? "어제" : "Yesterday";
  if (days < 7) return isKo ? `${days}일 전` : `${days}d ago`;
  return formatMeetingDate(timestamp, locale);
}

export function formatProvider(provider: string | null | undefined): string {
  if (!provider) return "N/A";
  return provider
    .split(/[_\s-]+/)
    .filter(Boolean)
    .map((token) => token.charAt(0).toUpperCase() + token.slice(1))
    .join(" ");
}

function formatDurationCompact(durationMs: number, language: string): string {
  const isKo = language === "ko";
  const safeMs = Math.max(durationMs, 0);
  const totalMinutes = Math.max(1, Math.round(safeMs / 60_000));
  if (totalMinutes < 60) {
    return isKo ? `${totalMinutes}분` : `${totalMinutes}m`;
  }

  const hours = Math.floor(totalMinutes / 60);
  const minutes = totalMinutes % 60;
  if (minutes === 0) return isKo ? `${hours}시간` : `${hours}h`;
  return isKo ? `${hours}시간 ${minutes}분` : `${hours}h ${minutes}m`;
}

export function formatMeetingDuration(
  meeting: RoundTableMeeting,
  language: string,
  t: I18nContextValue["t"],
): string {
  if (meeting.completed_at && meeting.completed_at > meeting.started_at) {
    return formatDurationCompact(
      meeting.completed_at - meeting.started_at,
      language,
    );
  }
  if (meeting.status === "in_progress") {
    return t({ ko: "진행 중", en: "In progress" });
  }
  return "—";
}

export function getParticipantInitials(name: string): string {
  const parts = name
    .trim()
    .split(/\s+/)
    .filter(Boolean);
  if (parts.length === 0) return "?";
  if (parts.length === 1) return parts[0].slice(0, 2).toUpperCase();
  return `${parts[0][0] ?? ""}${parts[1][0] ?? ""}`.toUpperCase();
}

export function getMeetingStatusLabel(
  meeting: RoundTableMeeting | null,
  t: I18nContextValue["t"],
): string {
  if (!meeting) return "—";
  if (meeting.status === "in_progress") {
    return t({ ko: "진행 중", en: "In Progress" });
  }
  if (meeting.status === "completed") {
    return t({ ko: "완료", en: "Completed" });
  }
  return t({ ko: "취소됨", en: "Cancelled" });
}

export function getMeetingStatusTone(
  meeting: RoundTableMeeting | null,
): "ok" | "warn" | "err" {
  if (!meeting) return "warn";
  if (meeting.status === "completed") return "ok";
  if (meeting.status === "in_progress") return "warn";
  return "err";
}

export function getMeetingIssueStateLabel(
  state: MeetingIssueState,
  t: I18nContextValue["t"],
): string {
  if (state === "created") return t({ ko: "생성됨", en: "Created" });
  if (state === "failed") return t({ ko: "실패", en: "Failed" });
  if (state === "discarded") return t({ ko: "폐기", en: "Discarded" });
  return t({ ko: "대기", en: "Pending" });
}

export function meetingSearchText(meeting: RoundTableMeeting): string {
  return [
    meeting.agenda,
    meeting.summary,
    meeting.status,
    meeting.primary_provider,
    meeting.reviewer_provider,
    meeting.participant_names.join(" "),
    meeting.proposed_issues
      ?.map((issue) => `${issue.title} ${issue.assignee ?? ""}`)
      .join(" "),
  ]
    .filter(Boolean)
    .join(" ")
    .toLocaleLowerCase();
}
