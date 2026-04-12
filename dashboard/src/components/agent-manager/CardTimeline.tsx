import { useMemo, useState, type Dispatch, type SetStateAction } from "react";

import MarkdownContent from "../common/MarkdownContent";
import type { GitHubComment } from "../../api";
import type { UiLanguage } from "../../types";
import {
  coalesceGitHubCommentTimeline,
  formatIso,
  parseGitHubCommentTimeline,
  type CoalescedGitHubTimelineItem,
  type GitHubTimelineKind,
  type GitHubTimelineStatus,
  type ParsedGitHubComment,
} from "./kanban-utils";

export const TIMELINE_KIND_STYLE: Record<string, { bg: string; text: string }> = {
  review: { bg: "rgba(20,184,166,0.16)", text: "#5eead4" },
  pm: { bg: "rgba(244,114,182,0.16)", text: "#f9a8d4" },
  work: { bg: "rgba(96,165,250,0.16)", text: "#93c5fd" },
  general: { bg: "rgba(148,163,184,0.10)", text: "#94a3b8" },
};

function getTimelineKindLabel(
  kind: GitHubTimelineKind,
  tr: (ko: string, en: string) => string,
) {
  switch (kind) {
    case "review":
      return tr("리뷰", "Review");
    case "pm":
      return tr("PM 결정", "PM Decision");
    case "work":
      return tr("작업 이력", "Work Log");
    case "general":
      return tr("코멘트", "Comment");
  }
}

function getTimelineStatusLabel(
  status: GitHubTimelineStatus,
  tr: (ko: string, en: string) => string,
) {
  switch (status) {
    case "reviewing":
      return tr("진행 중", "In Progress");
    case "changes_requested":
      return tr("수정 필요", "Changes Requested");
    case "passed":
      return tr("통과", "Passed");
    case "decision":
      return tr("결정", "Decision");
    case "completed":
      return tr("완료", "Completed");
    case "comment":
      return tr("일반", "General");
  }
}

function getTimelineStatusStyle(status: GitHubTimelineStatus) {
  switch (status) {
    case "reviewing":
      return { bg: "rgba(20,184,166,0.16)", text: "#5eead4" };
    case "changes_requested":
      return { bg: "rgba(251,113,133,0.16)", text: "#fda4af" };
    case "passed":
      return { bg: "rgba(34,197,94,0.18)", text: "#86efac" };
    case "decision":
      return { bg: "rgba(244,114,182,0.16)", text: "#f9a8d4" };
    case "completed":
      return { bg: "rgba(96,165,250,0.16)", text: "#93c5fd" };
    case "comment":
      return { bg: "rgba(148,163,184,0.12)", text: "#94a3b8" };
  }
}

function renderTimelineEntry(
  entry: ParsedGitHubComment,
  tr: (ko: string, en: string) => string,
  locale: UiLanguage,
  opts: { compact?: boolean; hideAuthor?: boolean } = {},
) {
  const statusStyle = getTimelineStatusStyle(entry.status);
  const kindStyle = TIMELINE_KIND_STYLE[entry.kind];
  const isGeneral = entry.kind === "general";
  const compact = opts.compact ?? false;

  return (
    <div
      className={`rounded-xl border space-y-2 ${compact ? "p-2.5" : "p-3"}`}
      style={{
        borderColor: isGeneral ? "rgba(148,163,184,0.08)" : `${kindStyle.text}22`,
        backgroundColor: isGeneral ? "rgba(255,255,255,0.02)" : `${kindStyle.text}06`,
      }}
    >
      <div className="flex flex-wrap items-center gap-2 text-xs">
        <span
          className="px-2 py-0.5 rounded-full font-medium"
          style={{ backgroundColor: kindStyle.bg, color: kindStyle.text }}
        >
          {getTimelineKindLabel(entry.kind, tr)}
        </span>
        {!isGeneral && (
          <span
            className="px-2 py-0.5 rounded-full font-medium"
            style={{ backgroundColor: statusStyle.bg, color: statusStyle.text }}
          >
            {getTimelineStatusLabel(entry.status, tr)}
          </span>
        )}
        {!opts.hideAuthor && (
          <span className="font-medium" style={{ color: "#93c5fd" }}>
            {entry.author}
          </span>
        )}
        <span style={{ color: "var(--th-text-muted)" }}>
          {formatIso(entry.createdAt, locale)}
        </span>
      </div>
      <div className="space-y-1">
        {!isGeneral && (
          <div className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
            {entry.title}
          </div>
        )}
        {!isGeneral && entry.summary && entry.summary !== entry.title && (
          <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
            {entry.summary}
          </div>
        )}
        {entry.details.length > 0 && (
          <ul className="space-y-1 pl-4 text-xs list-disc" style={{ color: "var(--th-text-secondary)" }}>
            {entry.details.map((detail, detailIdx) => (
              <li key={detailIdx}>{detail}</li>
            ))}
          </ul>
        )}
        <div
          className="rounded-lg border px-3 py-2 text-sm"
          style={{
            borderColor: "rgba(148,163,184,0.16)",
            backgroundColor: "var(--th-overlay-subtle)",
            color: "var(--th-text-primary)",
          }}
        >
          <MarkdownContent content={entry.body} />
        </div>
      </div>
    </div>
  );
}

function renderCoalescedGroup(
  group: CoalescedGitHubTimelineItem,
  isExpanded: boolean,
  onToggle: () => void,
  tr: (ko: string, en: string) => string,
  locale: UiLanguage,
) {
  const kindStyle = TIMELINE_KIND_STYLE[group.kind];
  const timeRange =
    group.updatedAt !== group.createdAt
      ? `${formatIso(group.createdAt, locale)} - ${formatIso(group.updatedAt, locale)}`
      : formatIso(group.createdAt, locale);

  return (
    <div
      className="rounded-xl border p-3 space-y-3"
      style={{
        borderColor: `${kindStyle.text}22`,
        backgroundColor: `${kindStyle.text}08`,
      }}
    >
      <button
        type="button"
        onClick={onToggle}
        className="w-full text-left space-y-2"
      >
        <div className="flex flex-wrap items-center gap-2 text-xs">
          <span
            className="px-2 py-0.5 rounded-full font-medium"
            style={{ backgroundColor: kindStyle.bg, color: kindStyle.text }}
          >
            {getTimelineKindLabel(group.kind, tr)}
          </span>
          <span
            className="px-2 py-0.5 rounded-full font-medium"
            style={{
              backgroundColor: "rgba(96,165,250,0.14)",
              color: "#93c5fd",
            }}
          >
            {tr("합산", "Coalesced")}
          </span>
          <span className="font-medium" style={{ color: "#93c5fd" }}>
            {group.author}
          </span>
          <span style={{ color: "var(--th-text-muted)" }}>{timeRange}</span>
          <span
            className="ml-auto px-2 py-0.5 rounded-full font-medium"
            style={{
              backgroundColor: "rgba(148,163,184,0.12)",
              color: "var(--th-text-muted)",
            }}
          >
            {isExpanded ? tr("접기", "Collapse") : tr("펼치기", "Expand")}
          </span>
        </div>
        <div className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
          {tr(`${group.entries.length}개 변경`, `${group.entries.length} changes`)}
        </div>
        {group.highlights.length > 0 && (
          <div className="text-sm" style={{ color: "var(--th-text-secondary)" }}>
            {group.highlights.join(" · ")}
          </div>
        )}
      </button>

      {isExpanded && (
        <div
          className="space-y-2 border-t pt-3"
          style={{ borderColor: "rgba(148,163,184,0.12)" }}
        >
          {group.entries.map((entry, index) => (
            <div key={`${group.id}-${index}`}>
              {renderTimelineEntry(entry, tr, locale, {
                compact: true,
                hideAuthor: true,
              })}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

export interface CardTimelineProps {
  ghComments: GitHubComment[];
  timelineFilter: GitHubTimelineKind | null;
  setTimelineFilter: Dispatch<SetStateAction<GitHubTimelineKind | null>>;
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  onRefresh: () => void;
}

export default function CardTimeline({
  ghComments,
  timelineFilter,
  setTimelineFilter,
  tr,
  locale,
  onRefresh,
}: CardTimelineProps) {
  const [expandedGroups, setExpandedGroups] = useState<Record<string, boolean>>({});
  const timelineItems = useMemo(
    () => coalesceGitHubCommentTimeline(parseGitHubCommentTimeline(ghComments)),
    [ghComments],
  );

  if (timelineItems.length === 0) return null;

  const filteredItems = timelineItems.filter(
    (entry) => !timelineFilter || entry.kind === timelineFilter,
  );
  const kindCounts = timelineItems.reduce<Record<string, number>>((acc, entry) => {
    acc[entry.kind] = (acc[entry.kind] ?? 0) + 1;
    return acc;
  }, {});
  const hasMultipleKinds = Object.keys(kindCounts).length > 1;

  return (
    <div className="rounded-2xl border p-4 bg-surface-subtle space-y-3" style={{ borderColor: "var(--th-border-subtle)" }}>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h4 className="font-medium" style={{ color: "var(--th-text-heading)" }}>
          {tr("GitHub 코멘트 타임라인", "GitHub Comment Timeline")}
          <span className="ml-2 text-xs font-normal" style={{ color: "var(--th-text-muted)" }}>
            ({timelineItems.length})
          </span>
        </h4>
        <button
          type="button"
          onClick={onRefresh}
          className="rounded-full px-2.5 py-1 text-xs font-medium border transition-opacity hover:opacity-80"
          style={{
            borderColor: "rgba(147,197,253,0.28)",
            backgroundColor: "rgba(96,165,250,0.12)",
            color: "#93c5fd",
          }}
        >
          {tr("새로고침", "Refresh")}
        </button>
      </div>

      {hasMultipleKinds && (
        <div className="flex flex-wrap gap-1.5">
          <button
            className="px-2 py-0.5 rounded-full text-xs font-medium transition-colors"
            style={{
              backgroundColor: !timelineFilter ? "rgba(96,165,250,0.18)" : "rgba(148,163,184,0.08)",
              color: !timelineFilter ? "#93c5fd" : "var(--th-text-muted)",
            }}
            onClick={() => setTimelineFilter(null)}
          >
            {tr("전체", "All")} ({timelineItems.length})
          </button>
          {(["review", "pm", "work", "general"] as const)
            .filter((kind) => kindCounts[kind])
            .map((kind) => (
              <button
                key={kind}
                className="px-2 py-0.5 rounded-full text-xs font-medium transition-colors"
                style={{
                  backgroundColor: timelineFilter === kind ? TIMELINE_KIND_STYLE[kind].bg : "rgba(148,163,184,0.08)",
                  color: timelineFilter === kind ? TIMELINE_KIND_STYLE[kind].text : "var(--th-text-muted)",
                }}
                onClick={() => setTimelineFilter(timelineFilter === kind ? null : kind)}
              >
                {getTimelineKindLabel(kind, tr)} ({kindCounts[kind]})
              </button>
            ))}
        </div>
      )}

      <div className="space-y-3 max-h-96 overflow-y-auto">
        {filteredItems.map((item) =>
          item.coalesced
            ? (
                <div key={item.id}>
                  {renderCoalescedGroup(
                    item,
                    !!expandedGroups[item.id],
                    () =>
                      setExpandedGroups((prev) => ({
                        ...prev,
                        [item.id]: !prev[item.id],
                      })),
                    tr,
                    locale,
                  )}
                </div>
              )
            : (
                <div key={item.id}>
                  {renderTimelineEntry(item.entries[0], tr, locale)}
                </div>
              ),
        )}
      </div>
    </div>
  );
}
