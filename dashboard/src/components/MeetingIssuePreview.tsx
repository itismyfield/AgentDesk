import {
  Ban,
  CheckCircle2,
  ChevronDown,
  ChevronUp,
  CircleDashed,
  Trash2,
  TriangleAlert,
} from "lucide-react";
import type { I18nContextValue } from "../i18n";
import {
  getMeetingIssueResult,
  getMeetingIssueState,
  getProposedIssueKey,
} from "../lib/meetingHelpers";
import type { ProposedIssue, RoundTableMeeting } from "../types";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceNotice,
} from "./common/SurfacePrimitives";

interface MeetingIssuePreviewProps {
  meeting: RoundTableMeeting;
  expanded: boolean;
  t: I18nContextValue["t"];
  discardingIssueIds: Record<string, boolean>;
  onToggle: (meetingId: string) => void;
  onDiscardIssue: (meetingId: string, issue: ProposedIssue) => void;
}

export default function MeetingIssuePreview({
  meeting,
  expanded,
  t,
  discardingIssueIds,
  onToggle,
  onDiscardIssue,
}: MeetingIssuePreviewProps) {
  if (!meeting.proposed_issues || meeting.proposed_issues.length === 0) {
    return null;
  }

  return (
    <SurfaceCard
      className="space-y-2 rounded-2xl p-3"
      style={{
        background:
          "color-mix(in srgb, var(--th-bg-surface) 88%, transparent)",
        borderColor:
          "color-mix(in srgb, var(--th-border) 68%, transparent)",
      }}
    >
      <button
        onClick={() => onToggle(meeting.id)}
        className="flex min-w-0 items-center gap-1.5 break-words text-left text-xs font-medium transition-colors hover:opacity-80 [overflow-wrap:anywhere]"
        style={{ color: "#34d399" }}
        aria-expanded={expanded}
        aria-label={t({
          ko: `생성될 일감 미리보기 ${expanded ? "닫기" : "열기"}`,
          en: `${expanded ? "Collapse" : "Expand"} issues to create preview`,
        })}
      >
        {expanded ? <ChevronUp size={14} /> : <ChevronDown size={14} />}
        {t({
          ko: `생성될 일감 미리보기 (${meeting.proposed_issues.length}건)`,
          en: `Preview issues to create (${meeting.proposed_issues.length})`,
        })}
      </button>
      {expanded && (
        <div className="space-y-2">
          {meeting.proposed_issues.map((issue, index) => {
            const issueResult = getMeetingIssueResult(meeting, issue);
            const issueState = getMeetingIssueState(issueResult);
            const issueKey = getProposedIssueKey(issue);
            const actionKey = `${meeting.id}:${issueKey}`;
            const isDiscardingIssue = !!discardingIssueIds[actionKey];
            const statusMeta =
              issueState === "created"
                ? {
                    label: t({ ko: "생성됨", en: "Created" }),
                    icon: CheckCircle2,
                    color: "#34d399",
                    bg: "rgba(16,185,129,0.12)",
                    border: "rgba(16,185,129,0.18)",
                  }
                : issueState === "discarded"
                  ? {
                      label: t({ ko: "폐기됨", en: "Discarded" }),
                      icon: Ban,
                      color: "#94a3b8",
                      bg: "rgba(148,163,184,0.12)",
                      border: "rgba(148,163,184,0.18)",
                    }
                  : issueState === "failed"
                    ? {
                        label: t({ ko: "실패", en: "Failed" }),
                        icon: TriangleAlert,
                        color: "#fbbf24",
                        bg: "rgba(245,158,11,0.12)",
                        border: "rgba(245,158,11,0.18)",
                      }
                    : {
                        label: t({ ko: "대기", en: "Pending" }),
                        icon: CircleDashed,
                        color: "#60a5fa",
                        bg: "rgba(96,165,250,0.12)",
                        border: "rgba(96,165,250,0.18)",
                      };
            const StatusIcon = statusMeta.icon;
            const issueTone =
              issueState === "created"
                ? "success"
                : issueState === "discarded"
                  ? "neutral"
                  : issueState === "failed"
                    ? "warn"
                    : "info";

            return (
              <SurfaceNotice
                key={index}
                tone={issueTone}
                compact
                className="items-start"
                action={
                  (issueState === "pending" || issueState === "failed") && (
                    <SurfaceActionButton
                      tone="neutral"
                      compact
                      onClick={() => onDiscardIssue(meeting.id, issue)}
                      disabled={isDiscardingIssue}
                    >
                      <span className="inline-flex items-center gap-1">
                        <Trash2 size={11} />
                        {isDiscardingIssue
                          ? t({ ko: "폐기 중...", en: "Discarding..." })
                          : t({ ko: "폐기", en: "Discard" })}
                      </span>
                    </SurfaceActionButton>
                  )
                }
              >
                <div className="flex min-w-0 flex-col gap-2 sm:flex-row sm:items-start sm:justify-between">
                  <div className="min-w-0 flex-1">
                    <div
                      className="break-words font-medium [overflow-wrap:anywhere]"
                      style={{ color: "var(--th-text)" }}
                    >
                      [RT] {issue.title}
                    </div>
                    <div
                      className="mt-0.5 break-words [overflow-wrap:anywhere]"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      {t({
                        ko: `담당: ${issue.assignee}`,
                        en: `Assignee: ${issue.assignee}`,
                      })}
                    </div>
                    {issueResult?.error && issueState === "failed" && (
                      <div
                        className="mt-1 break-words [overflow-wrap:anywhere]"
                        style={{ color: "#fbbf24" }}
                      >
                        {t({
                          ko: `실패: ${issueResult.error}`,
                          en: `Failed: ${issueResult.error}`,
                        })}
                      </div>
                    )}
                    {issueResult?.issue_url && issueState === "created" && (
                      <a
                        href={issueResult.issue_url}
                        target="_blank"
                        rel="noreferrer"
                        className="mt-1 inline-flex max-w-full break-all hover:underline"
                        style={{ color: "#34d399" }}
                      >
                        {t({
                          ko: "생성된 이슈 열기",
                          en: "Open created issue",
                        })}
                      </a>
                    )}
                  </div>
                </div>
                <div className="mt-2">
                  <span
                    className="inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-xs font-semibold"
                    style={{
                      background: statusMeta.bg,
                      borderColor: statusMeta.border,
                      color: statusMeta.color,
                    }}
                    aria-label={t({
                      ko: `후속 일감 상태: ${statusMeta.label}`,
                      en: `Follow-up issue status: ${statusMeta.label}`,
                    })}
                  >
                    <StatusIcon size={12} aria-hidden="true" />
                    {statusMeta.label}
                  </span>
                </div>
              </SurfaceNotice>
            );
          })}
        </div>
      )}
    </SurfaceCard>
  );
}
