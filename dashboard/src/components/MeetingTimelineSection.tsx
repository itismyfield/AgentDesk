import type { CSSProperties } from "react";
import { FileText, Trash2 } from "lucide-react";
import type { I18nContextValue } from "../i18n";
import type { GitHubRepoOption } from "../api/client";
import type { ProposedIssue, RoundTableMeeting } from "../types";
import MeetingProviderFlow from "./MeetingProviderFlow";
import { formatMeetingReferenceHash } from "./meetingReferenceHash";
import MarkdownContent from "./common/MarkdownContent";
import {
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceNotice,
  SurfaceSection,
} from "./common/SurfacePrimitives";
import MeetingIssuePreview from "./MeetingIssuePreview";
import MeetingRecordActions from "./MeetingRecordActions";
import type { MeetingIssueProgress } from "./meetingIssueProgress";
import { normalizeSelectionReason } from "./meetingMinutesModel";

interface MeetingTimelineSectionProps {
  meetings: RoundTableMeeting[];
  showStartForm: boolean;
  locale: string;
  t: I18nContextValue["t"];
  expandedIssues: Set<string>;
  deleting: string | null;
  creatingIssue: string | null;
  discardingIssueIds: Record<string, boolean>;
  discardingMeetingIds: Record<string, boolean>;
  savingRepoIds: Record<string, boolean>;
  repoSaveErrors: Record<string, string>;
  loadingRepos: boolean;
  repoError: string | null;
  repoOwner: string;
  githubRepos: GitHubRepoOption[];
  inputStyle: CSSProperties;
  getIssueProgress: (meeting: RoundTableMeeting) => MeetingIssueProgress;
  getIssueProgressText: (issueProgress: MeetingIssueProgress) => string;
  getSelectedRepo: (meeting: RoundTableMeeting) => string;
  getRepoOptions: (selectedRepo: string) => GitHubRepoOption[];
  onToggleIssuePreview: (meetingId: string) => void;
  onDelete: (meetingId: string) => void;
  onOpenDetail: (meeting: RoundTableMeeting) => void;
  onCreateIssues: (meetingId: string, repo: string) => void;
  onDiscardIssue: (meetingId: string, issue: ProposedIssue) => void;
  onDiscardAllIssues: (meetingId: string) => void;
  onRepoChange: (meetingId: string, repo: string) => void;
}

function StatusBadge({
  status,
  t,
}: {
  status: string;
  t: I18nContextValue["t"];
}) {
  const map: Record<string, { bg: string; color: string; label: string }> = {
    completed: {
      bg: "rgba(16,185,129,0.15)",
      color: "#34d399",
      label: t({ ko: "완료", en: "Completed" }),
    },
    in_progress: {
      bg: "rgba(245,158,11,0.15)",
      color: "#fbbf24",
      label: t({ ko: "진행중", en: "In Progress" }),
    },
    cancelled: {
      bg: "rgba(239,68,68,0.15)",
      color: "#f87171",
      label: t({ ko: "취소", en: "Cancelled" }),
    },
  };
  const statusMeta = map[status] || map.completed;
  return (
    <span
      className="rounded-full px-2 py-0.5 text-xs font-medium"
      style={{ background: statusMeta.bg, color: statusMeta.color }}
    >
      {statusMeta.label}
    </span>
  );
}

export default function MeetingTimelineSection({
  meetings,
  showStartForm,
  locale,
  t,
  expandedIssues,
  deleting,
  creatingIssue,
  discardingIssueIds,
  discardingMeetingIds,
  savingRepoIds,
  repoSaveErrors,
  loadingRepos,
  repoError,
  repoOwner,
  githubRepos,
  inputStyle,
  getIssueProgress,
  getIssueProgressText,
  getSelectedRepo,
  getRepoOptions,
  onToggleIssuePreview,
  onDelete,
  onOpenDetail,
  onCreateIssues,
  onDiscardIssue,
  onDiscardAllIssues,
  onRepoChange,
}: MeetingTimelineSectionProps) {
  if (meetings.length === 0 && showStartForm) return null;

  if (meetings.length === 0) {
    return (
      <SurfaceSection
        eyebrow={t({ ko: "Archive", en: "Archive" })}
        title={t({ ko: "회의 타임라인", en: "Meeting Timeline" })}
        description={t({
          ko: "최근 회의가 쌓이면 여기서 흐름과 후속 작업을 이어서 관리합니다.",
          en: "Recent meetings accumulate here for follow-up tracking.",
        })}
      >
        <SurfaceEmptyState className="mt-4 py-16 text-center">
          <FileText size={48} className="mx-auto mb-4 opacity-30" />
          <p>{t({ ko: "회의 기록이 없습니다", en: "No meeting records" })}</p>
          <p className="mt-1 text-sm">
            {t({
              ko: "\"새 회의\" 버튼으로 라운드 테이블을 시작하세요",
              en: "Start a round table with the \"New Meeting\" button",
            })}
          </p>
        </SurfaceEmptyState>
      </SurfaceSection>
    );
  }

  return (
    <SurfaceSection
      eyebrow={t({ ko: "Archive", en: "Archive" })}
      title={t({ ko: "회의 타임라인", en: "Meeting Timeline" })}
      description={t({
        ko: "각 회의의 진행 상태, 참여자, 후속 일감 생성 흐름을 한 번에 확인합니다.",
        en: "Review meeting status, participants, and follow-up issue generation flow at a glance.",
      })}
      badge={t({
        ko: `${meetings.length}개 회의`,
        en: `${meetings.length} meetings`,
      })}
    >
      <div className="mt-4 space-y-4">
        {meetings.map((meeting) => {
          const hasProposedIssues =
            meeting.proposed_issues && meeting.proposed_issues.length > 0;
          const issuesExpanded = expandedIssues.has(meeting.id);
          const issueProgress = getIssueProgress(meeting);
          const selectedRepo = getSelectedRepo(meeting);
          const repoOptions = getRepoOptions(selectedRepo);
          const isSavingRepo = !!savingRepoIds[meeting.id];
          const meetingHashDisplay = formatMeetingReferenceHash(
            meeting.meeting_hash,
          );
          const threadHashDisplay = formatMeetingReferenceHash(
            meeting.thread_hash,
          );
          const selectionReason = normalizeSelectionReason(
            meeting.selection_reason,
          );
          const progressTone = issueProgress.allCreated
            ? "success"
            : issueProgress.failed > 0
              ? "warn"
              : issueProgress.discarded > 0
                ? "neutral"
                : "info";

          return (
            <SurfaceCard
              key={meeting.id}
              className="space-y-4 rounded-3xl p-4 sm:p-5"
              style={{
                background:
                  "color-mix(in srgb, var(--th-card-bg) 94%, transparent)",
                borderColor:
                  "color-mix(in srgb, var(--th-border) 70%, transparent)",
              }}
            >
              <div className="flex min-w-0 items-start justify-between gap-3">
                <div className="min-w-0 flex-1">
                  <h3
                    className="break-words text-base font-semibold [overflow-wrap:anywhere]"
                    style={{ color: "var(--th-text)" }}
                  >
                    {meeting.agenda}
                  </h3>
                  <div className="mt-1.5 flex min-w-0 flex-wrap items-center gap-2">
                    <StatusBadge status={meeting.status} t={t} />
                    {(meeting.primary_provider ||
                      meeting.reviewer_provider) && (
                      <MeetingProviderFlow
                        primaryProvider={meeting.primary_provider}
                        reviewerProvider={meeting.reviewer_provider}
                        compact
                      />
                    )}
                    <span
                      className="text-xs"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      {new Date(meeting.started_at).toLocaleDateString(locale)}
                    </span>
                    {meeting.total_rounds > 0 && (
                      <span
                        className="text-xs"
                        style={{ color: "var(--th-text-muted)" }}
                      >
                        {meeting.total_rounds}R
                      </span>
                    )}
                  </div>
                </div>
                <button
                  onClick={() => onDelete(meeting.id)}
                  disabled={deleting === meeting.id}
                  className="shrink-0 rounded-lg p-1.5 transition-colors hover:bg-red-500/10"
                  title={t({ ko: "삭제", en: "Delete" })}
                  aria-label={t({
                    ko: "회의록 삭제",
                    en: "Delete meeting record",
                  })}
                >
                  <Trash2
                    size={14}
                    style={{
                      color:
                        deleting === meeting.id
                          ? "var(--th-text-muted)"
                          : "#f87171",
                    }}
                  />
                </button>
              </div>

              {(meetingHashDisplay || threadHashDisplay) && (
                <div
                  className="space-y-1 rounded-xl px-3 py-2 text-xs"
                  style={{
                    background: "rgba(148,163,184,0.08)",
                    border: "1px solid rgba(148,163,184,0.14)",
                  }}
                >
                  {meetingHashDisplay && (
                    <div className="flex min-w-0 items-center gap-2">
                      <span
                        className="shrink-0 font-medium"
                        style={{ color: "var(--th-text-secondary)" }}
                      >
                        {t({ ko: "회의 해시 :", en: "Meeting Hash:" })}
                      </span>
                      <span
                        className="min-w-0 break-all font-mono"
                        style={{ color: "var(--th-text-muted)" }}
                      >
                        {meetingHashDisplay}
                      </span>
                    </div>
                  )}
                  {threadHashDisplay && (
                    <div className="flex min-w-0 items-center gap-2">
                      <span
                        className="shrink-0 font-medium"
                        style={{ color: "var(--th-text-secondary)" }}
                      >
                        {t({ ko: "스레드 해시 :", en: "Thread Hash:" })}
                      </span>
                      <span
                        className="min-w-0 break-all font-mono"
                        style={{ color: "var(--th-text-muted)" }}
                      >
                        {threadHashDisplay}
                      </span>
                    </div>
                  )}
                </div>
              )}

              <div className="flex min-w-0 flex-wrap items-center gap-1.5">
                {meeting.participant_names.map((name) => (
                  <span
                    key={name}
                    className="rounded-full px-2 py-0.5 text-xs font-medium"
                    style={{
                      background:
                        "color-mix(in srgb, var(--th-accent-primary-soft) 78%, transparent)",
                      color: "var(--th-text-primary)",
                    }}
                  >
                    {name}
                  </span>
                ))}
              </div>

              {selectionReason && (
                <div
                  className="min-w-0 rounded-xl px-3 py-2 text-xs"
                  style={{
                    background: "rgba(148,163,184,0.08)",
                    border: "1px solid rgba(148,163,184,0.14)",
                  }}
                >
                  <span
                    className="font-medium"
                    style={{ color: "var(--th-text-secondary)" }}
                  >
                    {t({ ko: "선정 사유:", en: "Selection Reason:" })}
                  </span>{" "}
                  <span
                    className="break-words [overflow-wrap:anywhere]"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {selectionReason}
                  </span>
                </div>
              )}

              {meeting.summary && (
                <SurfaceNotice
                  tone="accent"
                  className="items-start"
                  leading={
                    <div
                      className="mt-0.5 h-8 w-8 shrink-0 overflow-hidden rounded-xl"
                      style={{ background: "var(--th-bg-surface)" }}
                    >
                      <img
                        src="/sprites/7-D-1.png"
                        alt="PMD"
                        className="h-full w-full object-cover"
                        style={{ imageRendering: "pixelated" }}
                      />
                    </div>
                  }
                >
                  <div className="min-w-0">
                    <div className="mb-1 flex flex-wrap items-center justify-between gap-2">
                      <div
                        className="text-xs font-semibold"
                        style={{ color: "var(--th-text-primary)" }}
                      >
                        {t({ ko: "PMD 요약", en: "PMD Summary" })}
                      </div>
                    </div>
                    <div className="text-sm" style={{ color: "var(--th-text)" }}>
                      <MarkdownContent content={meeting.summary} />
                    </div>
                  </div>
                </SurfaceNotice>
              )}

              {hasProposedIssues && !issueProgress.allCreated && (
                <MeetingIssuePreview
                  meeting={meeting}
                  expanded={issuesExpanded}
                  t={t}
                  discardingIssueIds={discardingIssueIds}
                  onToggle={onToggleIssuePreview}
                  onDiscardIssue={onDiscardIssue}
                />
              )}

              {hasProposedIssues && (
                <SurfaceNotice tone={progressTone} compact>
                  {getIssueProgressText(issueProgress)}
                </SurfaceNotice>
              )}

              <MeetingRecordActions
                t={t}
                hasProposedIssues={!!hasProposedIssues}
                issuesCreated={meeting.issues_created || 0}
                issueProgress={issueProgress}
                selectedRepo={selectedRepo}
                repoOptions={repoOptions}
                githubRepos={githubRepos}
                loadingRepos={loadingRepos}
                isSavingRepo={isSavingRepo}
                repoSaveError={repoSaveErrors[meeting.id]}
                repoError={repoError}
                repoOwner={repoOwner}
                creatingIssue={creatingIssue === meeting.id}
                discardingAllIssues={!!discardingMeetingIds[meeting.id]}
                inputStyle={inputStyle}
                onOpenDetail={() => onOpenDetail(meeting)}
                onCreateIssues={() => onCreateIssues(meeting.id, selectedRepo)}
                onDiscardAllIssues={() => onDiscardAllIssues(meeting.id)}
                onRepoChange={(repo) => onRepoChange(meeting.id, repo)}
              />
            </SurfaceCard>
          );
        })}
      </div>
    </SurfaceSection>
  );
}
