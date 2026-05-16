import {
  AlertTriangle,
  BookOpen,
  Check,
  Clock3,
  Minus,
  Plus,
  Search,
  Users,
  Workflow,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import {
  countOpenMeetingIssues,
  summarizeMeetings,
} from "../app/meetingSummary";
import { useI18n } from "../i18n";
import {
  getMeetingIssueResult,
  getMeetingIssueState,
  getMeetingIssueTone,
  getProposedIssueKey,
  type MeetingIssueState,
} from "../lib/meetingHelpers";
import type {
  RoundTableMeeting,
  RoundTableMeetingChannelOption,
} from "../types";
import MeetingMinutesView from "./MeetingMinutesView";
import SkillCatalogView from "./SkillCatalogView";

type MeetingNotificationType = "info" | "success" | "warning" | "error";
type MeetingNotifier = (
  message: string,
  type?: MeetingNotificationType,
) => string | void;
type MeetingNotificationUpdater = (
  id: string,
  message: string,
  type?: MeetingNotificationType,
) => void;
type MobilePane = "meetings" | "skills";
type MeetingStatusFilter = "all" | RoundTableMeeting["status"] | "open_issues";

const DESKTOP_SPLIT_QUERY = "(min-width: 1024px)";
const MEETING_TOGGLE_PATTERNS = [
  /new meeting/i,
  /새 회의/u,
  /close form/i,
  /입력 닫기/u,
];

interface MeetingsAndSkillsPageProps {
  meetings: RoundTableMeeting[];
  onRefresh: () => void;
  onNotify?: MeetingNotifier;
  onUpdateNotification?: MeetingNotificationUpdater;
  initialShowStartForm?: boolean;
  initialMeetingChannels?: RoundTableMeetingChannelOption[];
  initialChannelId?: string;
}

function normalizeNodeLabel(node: Element): string {
  const text = node.textContent ?? "";
  const title = node.getAttribute("title") ?? "";
  const ariaLabel = node.getAttribute("aria-label") ?? "";
  return `${text} ${title} ${ariaLabel}`.replace(/\s+/g, " ").trim();
}

function clickMatchingButton(
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

function useDesktopSplitLayout(): boolean {
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

function formatMeetingDate(timestamp: number, locale: string): string {
  return new Intl.DateTimeFormat(locale, {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  }).format(new Date(timestamp));
}

function formatRelativeTime(
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

function formatProvider(provider: string | null | undefined): string {
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

function formatMeetingDuration(
  meeting: RoundTableMeeting,
  language: string,
  t: ReturnType<typeof useI18n>["t"],
): string {
  if (meeting.completed_at && meeting.completed_at > meeting.started_at) {
    return formatDurationCompact(meeting.completed_at - meeting.started_at, language);
  }
  if (meeting.status === "in_progress") {
    return t({ ko: "진행 중", en: "In progress" });
  }
  return "—";
}

function getParticipantInitials(name: string): string {
  const parts = name
    .trim()
    .split(/\s+/)
    .filter(Boolean);
  if (parts.length === 0) return "?";
  if (parts.length === 1) return parts[0].slice(0, 2).toUpperCase();
  return `${parts[0][0] ?? ""}${parts[1][0] ?? ""}`.toUpperCase();
}

function getMeetingStatusLabel(
  meeting: RoundTableMeeting | null,
  t: ReturnType<typeof useI18n>["t"],
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

function getMeetingStatusTone(
  meeting: RoundTableMeeting | null,
): "ok" | "warn" | "err" {
  if (!meeting) return "warn";
  if (meeting.status === "completed") return "ok";
  if (meeting.status === "in_progress") return "warn";
  return "err";
}

function getMeetingIssueStateLabel(
  state: MeetingIssueState,
  t: ReturnType<typeof useI18n>["t"],
): string {
  if (state === "created") return t({ ko: "생성됨", en: "Created" });
  if (state === "failed") return t({ ko: "실패", en: "Failed" });
  if (state === "discarded") return t({ ko: "폐기", en: "Discarded" });
  return t({ ko: "대기", en: "Pending" });
}

function meetingSearchText(meeting: RoundTableMeeting): string {
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

export default function MeetingsAndSkillsPage({
  meetings,
  onRefresh,
  onNotify,
  onUpdateNotification,
  initialShowStartForm = false,
  initialMeetingChannels = [],
  initialChannelId,
}: MeetingsAndSkillsPageProps) {
  const { t, language, locale } = useI18n();
  const [selectedMeetingId, setSelectedMeetingId] = useState("");
  const [mobilePane, setMobilePane] = useState<MobilePane>("meetings");
  const [meetingQuery, setMeetingQuery] = useState("");
  const [meetingStatusFilter, setMeetingStatusFilter] = useState<MeetingStatusFilter>("all");
  const isDesktopSplit = useDesktopSplitLayout();
  const meetingShellRef = useRef<HTMLDivElement | null>(null);

  const meetingSummary = useMemo(() => summarizeMeetings(meetings), [meetings]);
  const sortedMeetings = useMemo(
    () => [...meetings].sort((left, right) => right.started_at - left.started_at),
    [meetings],
  );
  const filteredMeetings = useMemo(() => {
    const normalizedQuery = meetingQuery.trim().toLocaleLowerCase();
    return sortedMeetings.filter((meeting) => {
      const statusMatches =
        meetingStatusFilter === "all"
        || (meetingStatusFilter === "open_issues"
          ? countOpenMeetingIssues(meeting) > 0
          : meeting.status === meetingStatusFilter);
      if (!statusMatches) return false;
      if (!normalizedQuery) return true;
      return meetingSearchText(meeting).includes(normalizedQuery);
    });
  }, [meetingQuery, meetingStatusFilter, sortedMeetings]);
  const selectedMeeting = useMemo(() => {
    if (filteredMeetings.length === 0) return null;
    return (
      filteredMeetings.find((meeting) => meeting.id === selectedMeetingId) ??
      filteredMeetings[0]
    );
  }, [filteredMeetings, selectedMeetingId]);
  const totalParticipants = meetings.reduce(
    (sum, meeting) => sum + meeting.participant_names.length,
    0,
  );
  const providerCounts = useMemo(() => {
    const counts = new Map<string, number>();
    meetings.forEach((meeting) => {
      [meeting.primary_provider, meeting.reviewer_provider]
        .filter((provider): provider is string => Boolean(provider))
        .forEach((provider) => {
          counts.set(provider, (counts.get(provider) ?? 0) + 1);
        });
    });

    return [...counts.entries()].sort((left, right) => right[1] - left[1]);
  }, [meetings]);
  const selectedActionCount = selectedMeeting?.proposed_issues?.length ?? 0;
  const selectedOpenIssues = selectedMeeting
    ? countOpenMeetingIssues(selectedMeeting)
    : 0;
  const selectedDurationLabel = selectedMeeting
    ? formatMeetingDuration(selectedMeeting, language, t)
    : "—";
  const selectedTranscript = (selectedMeeting?.entries ?? [])
    .filter((entry) => entry.is_summary === 0)
    .slice(0, 4);

  useEffect(() => {
    if (!filteredMeetings.length) {
      if (selectedMeetingId) setSelectedMeetingId("");
      return;
    }

    if (
      !selectedMeetingId ||
      !filteredMeetings.some((meeting) => meeting.id === selectedMeetingId)
    ) {
      setSelectedMeetingId(filteredMeetings[0].id);
    }
  }, [filteredMeetings, selectedMeetingId]);

  const handleLaunchMeeting = () => {
    if (!isDesktopSplit) {
      setMobilePane("meetings");
    }
    if (clickMatchingButton(meetingShellRef.current, MEETING_TOGGLE_PATTERNS)) {
      meetingShellRef.current?.scrollIntoView({
        behavior: "smooth",
        block: "start",
      });
      return;
    }

    meetingShellRef.current?.scrollIntoView({ behavior: "smooth", block: "start" });
  };

  const meetingStatusOptions: Array<{ id: MeetingStatusFilter; label: string }> = [
    { id: "all", label: t({ ko: "전체", en: "All" }) },
    { id: "in_progress", label: t({ ko: "진행", en: "Active" }) },
    { id: "completed", label: t({ ko: "완료", en: "Done" }) },
    { id: "open_issues", label: t({ ko: "미완료 액션", en: "Open actions" }) },
  ];

  const meetingListCard = (
    <div data-testid="meetings-page-timeline" className="meetings-list card">
      <div className="meeting-list-toolbar">
        <label className="meeting-search">
          <Search size={14} />
          <input
            type="search"
            aria-label={t({ ko: "회의 검색", en: "Search meetings" })}
            value={meetingQuery}
            onChange={(event) => setMeetingQuery(event.target.value)}
            placeholder={t({ ko: "회의, 참석자, 후속 액션 검색", en: "Search meetings, people, actions" })}
          />
        </label>
        <div className="meeting-filter-tabs" aria-label={t({ ko: "회의 상태 필터", en: "Meeting status filter" })}>
          {meetingStatusOptions.map((option) => (
            <button
              key={option.id}
              type="button"
              className={meetingStatusFilter === option.id ? "active" : ""}
              aria-pressed={meetingStatusFilter === option.id}
              onClick={() => setMeetingStatusFilter(option.id)}
            >
              {option.label}
            </button>
          ))}
        </div>
        <div className="meeting-result-count">
          {t({
            ko: `${filteredMeetings.length}/${sortedMeetings.length}건`,
            en: `${filteredMeetings.length}/${sortedMeetings.length} shown`,
          })}
        </div>
      </div>
      {sortedMeetings.length === 0 ? (
        <div className="empty-state">
          {t({
            ko: "아직 등록된 회의가 없습니다.",
            en: "No meetings have been recorded yet.",
          })}
        </div>
      ) : filteredMeetings.length === 0 ? (
        <div className="empty-state">
          {t({
            ko: "검색 조건에 맞는 회의가 없습니다.",
            en: "No meetings match the current filters.",
          })}
        </div>
      ) : (
        filteredMeetings.map((meeting) => {
          const issueTotal = meeting.proposed_issues?.length ?? 0;
          const issueTone =
            issueTotal === 0 || meeting.issues_created === issueTotal
              ? "ok"
              : meeting.issues_created > 0
                ? "warn"
                : "err";
          const statusTone = getMeetingStatusTone(meeting);
          const durationLabel = formatMeetingDuration(meeting, language, t);

          return (
            <button
              key={meeting.id}
              type="button"
              className={`meeting-item ${
                selectedMeeting?.id === meeting.id ? "active" : ""
              }`}
              aria-label={t({
                ko: `${meeting.agenda} 회의 선택`,
                en: `Select meeting: ${meeting.agenda}`,
              })}
              aria-pressed={selectedMeeting?.id === meeting.id}
              onClick={() => setSelectedMeetingId(meeting.id)}
            >
              <div className="mi-head">
                <div className="min-w-0">
                  <div className="mi-title">{meeting.agenda}</div>
                  <div className="mi-meta">
                    <span
                      className="mono"
                      title={formatMeetingDate(meeting.started_at, locale)}
                    >
                      {formatRelativeTime(meeting.started_at, language, locale)}
                    </span>
                    <span>· {durationLabel}</span>
                    <span>
                      ·{" "}
                      {t({
                        ko: `${meeting.participant_names.length}명`,
                        en: `${meeting.participant_names.length} attendees`,
                      })}
                    </span>
                  </div>
                </div>
                <span className={`chip ${issueTone}`}>
                  <span className="dot" />
                  {issueTotal > 0 ? `${meeting.issues_created}/${issueTotal}` : "0/0"}
                </span>
              </div>
              <div className="mi-summary">
                {meeting.summary ??
                  t({
                    ko: "요약이 아직 없습니다.",
                    en: "No summary yet.",
                  })}
              </div>
              <div className="mi-foot">
                <span className={`chip ${statusTone}`}>
                  <span className="dot" />
                  {getMeetingStatusLabel(meeting, t)}
                </span>
                <span className="chip">
                  <span className="dot" />
                  {formatProvider(meeting.primary_provider)}
                </span>
              </div>
            </button>
          );
        })
      )}
    </div>
  );

  const meetingDetailCard = (
    <div className="meeting-detail card">
      <div className="md-head">
        <div className="min-w-0">
          <div className="section-kicker">{t({ ko: "회의록", en: "Meeting" })}</div>
          {selectedMeeting ? (
            <div className="md-status-row">
              <span className={`chip ${getMeetingStatusTone(selectedMeeting)}`}>
                <span className="dot" />
                {getMeetingStatusLabel(selectedMeeting, t)}
              </span>
              <span className="md-status-copy">
                {formatMeetingDate(selectedMeeting.started_at, locale)}
              </span>
            </div>
          ) : null}
          <h2 className="md-title">
            {selectedMeeting?.agenda ??
              t({
                ko: "선택된 회의가 없습니다",
                en: "No meeting selected",
              })}
          </h2>
        </div>
        <div className="md-toolbar">
          <button type="button" className="btn sm" onClick={onRefresh}>
            {t({ ko: "새로고침", en: "Refresh" })}
          </button>
          <button type="button" className="btn sm" onClick={handleLaunchMeeting}>
            <Plus size={11} />
            {t({ ko: "열기", en: "Open" })}
          </button>
        </div>
      </div>

      {selectedMeeting ? (
        <>
          <div className="md-meta-row">
            <div className="md-meta">
              <span>{t({ ko: "시작", en: "Started" })}</span>
              <b className="mono">
                {formatMeetingDate(selectedMeeting.started_at, locale)}
              </b>
            </div>
            <div className="md-meta">
              <span>{t({ ko: "길이", en: "Length" })}</span>
              <b>{selectedDurationLabel}</b>
            </div>
            <div className="md-meta">
              <span>{t({ ko: "프로바이더", en: "Provider" })}</span>
              <div className="md-meta-stack">
                <span className="chip">
                  <span className="dot" />
                  {formatProvider(selectedMeeting.primary_provider)}
                </span>
                <span className="chip neutral">
                  <span className="dot" />
                  {formatProvider(selectedMeeting.reviewer_provider)}
                </span>
              </div>
            </div>
            <div className="md-meta">
              <span>{t({ ko: "참석", en: "Attendees" })}</span>
              <div className="md-attendees">
                {selectedMeeting.participant_names.slice(0, 4).map((name, index) => (
                  <span
                    key={`${selectedMeeting.id}-${name}-${index}`}
                    className="md-attendee"
                    title={name}
                    style={{ marginLeft: index === 0 ? 0 : -6 }}
                  >
                    {getParticipantInitials(name)}
                  </span>
                ))}
                {selectedMeeting.participant_names.length > 4 ? (
                  <span className="md-attendee md-attendee-count">
                    +{selectedMeeting.participant_names.length - 4}
                  </span>
                ) : null}
              </div>
            </div>
          </div>

          <div className="md-section">
            <div className="md-section-head">{t({ ko: "요약", en: "Summary" })}</div>
            <p className="md-copy">
              {selectedMeeting.summary ??
                t({
                  ko: "요약이 아직 없습니다.",
                  en: "No summary yet.",
                })}
            </p>
          </div>

          <div className="md-section">
            <div className="md-section-head">
              {t({ ko: "후속 액션", en: "Follow-up Actions" })}
              <span className="md-inline-note">
                {t({
                  ko: `${selectedOpenIssues}/${selectedActionCount || 0} 미완료`,
                  en: `${selectedOpenIssues}/${selectedActionCount || 0} open`,
                })}
              </span>
            </div>
            {selectedMeeting.proposed_issues?.length ? (
              <div className="md-actions">
                {selectedMeeting.proposed_issues.slice(0, 6).map((issue) => {
                  const result = getMeetingIssueResult(selectedMeeting, issue);
                  const state = getMeetingIssueState(result);
                  const tone = getMeetingIssueTone(state);
                  const stateLabel = getMeetingIssueStateLabel(state, t);
                  return (
                    <div key={getProposedIssueKey(issue)} className="md-action">
                      <span className={`md-action-check ${state}`}>
                        {state === "created" ? (
                          <Check size={10} />
                        ) : state === "failed" ? (
                          <AlertTriangle size={10} />
                        ) : state === "discarded" ? (
                          <Minus size={10} />
                        ) : (
                          <Clock3 size={10} />
                        )}
                      </span>
                      <div className="md-action-copy">
                        <div className={`md-action-title ${state}`}>
                          {issue.title}
                        </div>
                        <div className="md-action-sub">
                          {issue.assignee}
                          {result?.error ? ` · ${result.error}` : ""}
                        </div>
                      </div>
                      <span className={`chip ${tone}`}>
                        <span className="dot" />
                        {stateLabel}
                      </span>
                    </div>
                  );
                })}
              </div>
            ) : (
              <div className="md-empty-copy">
                {t({
                  ko: "후속 액션이 아직 없습니다.",
                  en: "No follow-up actions yet.",
                })}
              </div>
            )}
          </div>

          <div className="md-section">
            <div className="md-section-head">
              {t({ ko: "전사 (발췌)", en: "Transcript Excerpt" })}
            </div>
            {selectedTranscript.length > 0 ? (
              <div className="md-transcript">
                {selectedTranscript.map((entry) => (
                  <div key={entry.id} className="md-transcript-line">
                    <span className="md-who">{entry.speaker_name}</span>
                    <span className="md-said">{entry.content}</span>
                  </div>
                ))}
              </div>
            ) : (
              <div className="md-empty-copy">
                {t({
                  ko: "전사 내용이 아직 없습니다.",
                  en: "No transcript yet.",
                })}
              </div>
            )}
          </div>
        </>
      ) : (
        <div className="md-empty-copy">
          {t({
            ko: "회의를 선택하면 상세 카드가 갱신됩니다.",
            en: "Select a meeting to refresh the detail card.",
          })}
        </div>
      )}
    </div>
  );

  const meetingWorkbenchCard = (
    <div className="meeting-workbench card">
      <div className="section-head">
        <div className="min-w-0">
          <div className="section-kicker">
            {t({ ko: "실시간 작성", en: "Live Workbench" })}
          </div>
          <div className="section-title">
            {t({
              ko: "회의 생성과 후속 흐름",
              en: "Meeting creation and follow-up flow",
            })}
          </div>
          <div className="section-copy">
            {t({
              ko: "기존 대시보드의 작성 워크플로우는 유지하되, Claude 시안의 카드 톤 안에서 연결합니다.",
              en: "Keep the original authoring workflow, but attach it inside the Claude card system.",
            })}
          </div>
        </div>
        <button type="button" className="btn sm" onClick={handleLaunchMeeting}>
          <Plus size={11} />
          {t({ ko: "열기", en: "Open" })}
        </button>
      </div>

      <div className="workbench-meta">
        <span className="chip">
          <Users size={11} />
          {t({
            ko: `${totalParticipants}명 누적`,
            en: `${totalParticipants} people total`,
          })}
        </span>
        <span className="chip">
          <Workflow size={11} />
          {t({
            ko: `${meetingSummary.unresolvedCount}건 미해결`,
            en: `${meetingSummary.unresolvedCount} unresolved`,
          })}
        </span>
        {providerCounts.slice(0, 2).map(([provider]) => (
          <span key={provider} className="chip neutral">
            <span className="dot" />
            {formatProvider(provider)}
          </span>
        ))}
      </div>

      <div ref={meetingShellRef} className="workbench-shell">
        <MeetingMinutesView
          meetings={meetings}
          onRefresh={onRefresh}
          embedded
          onNotify={onNotify}
          onUpdateNotification={onUpdateNotification}
          initialShowStartForm={initialShowStartForm}
          initialMeetingChannels={initialMeetingChannels}
          initialChannelId={initialChannelId}
        />
      </div>
    </div>
  );

  const skillRailCard = (
    <div data-testid="meetings-page-skills" className="skill-rail card">
      <div className="section-head">
        <div className="min-w-0">
          <div className="section-kicker">
            {t({ ko: "관련 스킬", en: "Related Skills" })}
          </div>
          <div className="section-title">
            {t({ ko: "회의 후속 자동화", en: "Meeting follow-up automation" })}
          </div>
          <div className="section-copy">
            {t({
              ko: "회의에서 나온 후속 액션을 실행·정리할 때 연결되는 스킬만 한곳에 모았습니다.",
              en: "Skills connected to meeting follow-up actions stay close to the meeting detail.",
            })}
          </div>
        </div>
        <div className="section-icon">
          <BookOpen size={17} />
        </div>
      </div>
      <SkillCatalogView embedded />
    </div>
  );

  const desktopDetailColumn = (
    <div className="detail-column rail-sticky">
      {meetingDetailCard}
      {meetingWorkbenchCard}
      {skillRailCard}
    </div>
  );

  const mobileDetailColumn = (
    <div className="detail-column">
      {meetingDetailCard}
      {meetingWorkbenchCard}
    </div>
  );

  return (
    <div
      data-testid="meetings-page"
      className="mx-auto h-full w-full max-w-[1440px] min-w-0 overflow-x-hidden overflow-y-auto p-4 pb-40 sm:p-6"
      style={{
        paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))",
      }}
    >
      <style>{`
        .meetings-page-shell {
          display: flex;
          flex-direction: column;
          gap: 16px;
        }

        .meetings-page-shell .page-header {
          display: flex;
          align-items: flex-end;
          justify-content: space-between;
          gap: 16px;
          margin-bottom: 2px;
          flex-wrap: wrap;
        }

        .meetings-page-shell .page-title {
          font-size: 22px;
          font-weight: 600;
          letter-spacing: -0.5px;
          line-height: 1.2;
          color: var(--th-text-heading);
        }

        .meetings-page-shell .page-sub {
          margin-top: 4px;
          max-width: 68ch;
          font-size: 13px;
          line-height: 1.65;
          color: var(--th-text-muted);
        }

        .meetings-page-shell .meeting-pane-tabs {
          display: inline-flex;
          align-items: center;
          gap: 4px;
          padding: 4px;
          border-radius: 999px;
          border: 1px solid color-mix(in srgb, var(--th-border) 72%, transparent);
          background: color-mix(in srgb, var(--th-card-bg) 92%, transparent);
        }

        .meetings-page-shell .meeting-pane-tab {
          border: 0;
          background: transparent;
          color: var(--th-text-muted);
          border-radius: 999px;
          padding: 6px 12px;
          font-size: 11.5px;
          font-weight: 500;
          transition: background 0.12s ease, color 0.12s ease;
        }

        .meetings-page-shell .meeting-pane-tab.active {
          color: var(--th-text);
          background: color-mix(in srgb, var(--th-overlay-medium) 92%, transparent);
        }

        .meetings-page-shell .btn {
          display: inline-flex;
          align-items: center;
          justify-content: center;
          gap: 6px;
          padding: 6px 12px;
          border-radius: 7px;
          font-size: 12.5px;
          font-weight: 500;
          color: var(--th-text-dim);
          background: color-mix(in srgb, var(--th-bg-surface) 88%, transparent);
          border: 1px solid color-mix(in srgb, var(--th-border) 70%, transparent);
          transition: background 0.12s ease, border-color 0.12s ease, color 0.12s ease;
        }

        .meetings-page-shell .btn:hover {
          background: color-mix(in srgb, var(--th-bg-surface) 96%, transparent);
          color: var(--th-text);
          border-color: var(--th-border);
        }

        .meetings-page-shell .btn.primary {
          background: color-mix(
            in srgb,
            var(--th-accent-primary-soft) 68%,
            transparent
          );
          color: var(--th-text-primary);
          border-color: color-mix(
            in srgb,
            var(--th-accent-primary) 28%,
            var(--th-border) 72%
          );
        }

        .meetings-page-shell .btn.sm {
          padding: 4px 9px;
          font-size: 11.5px;
        }

        .meetings-page-shell .card {
          overflow: hidden;
          border-radius: 18px;
          border: 1px solid color-mix(in srgb, var(--th-border) 72%, transparent);
          background: linear-gradient(
            180deg,
            color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%,
            color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%
          );
          box-shadow: 0 1px 0 color-mix(in srgb, var(--th-text-primary) 4%, transparent) inset;
        }

        .meetings-page-shell .chip {
          display: inline-flex;
          align-items: center;
          gap: 5px;
          padding: 2px 8px;
          border-radius: 999px;
          border: 1px solid color-mix(in srgb, var(--th-border) 70%, transparent);
          background: color-mix(in srgb, var(--th-bg-surface) 90%, transparent);
          font-size: 11px;
          font-weight: 500;
          color: var(--th-text-dim);
          font-variant-numeric: tabular-nums;
        }

        .meetings-page-shell .chip .dot {
          width: 6px;
          height: 6px;
          border-radius: 999px;
          background: currentColor;
          opacity: 0.9;
        }

        .meetings-page-shell .chip.ok {
          color: var(--th-accent-success);
          border-color: color-mix(
            in srgb,
            var(--th-accent-success) 30%,
            var(--th-border) 70%
          );
          background: color-mix(in srgb, var(--th-accent-success) 10%, transparent);
        }

        .meetings-page-shell .chip.warn {
          color: var(--th-accent-warn);
          border-color: color-mix(
            in srgb,
            var(--th-accent-warn) 30%,
            var(--th-border) 70%
          );
          background: color-mix(in srgb, var(--th-accent-warn) 10%, transparent);
        }

        .meetings-page-shell .chip.err {
          color: var(--th-accent-danger);
          border-color: color-mix(
            in srgb,
            var(--th-accent-danger) 30%,
            var(--th-border) 70%
          );
          background: color-mix(in srgb, rgba(255, 107, 107, 0.12) 85%, transparent);
        }

        .meetings-page-shell .chip.neutral {
          color: var(--th-text-muted);
        }

        .meetings-page-shell .meetings-split {
          display: grid;
          gap: 14px;
        }

        @media (min-width: 1024px) {
          .meetings-page-shell .meetings-split {
            grid-template-columns: minmax(0, 1.02fr) minmax(360px, 0.98fr);
            align-items: start;
          }

          .meetings-page-shell .rail-sticky {
            position: sticky;
            top: 1.5rem;
          }
        }

        .meetings-page-shell .meetings-list {
          padding: 0;
        }

        .meetings-page-shell .meeting-list-toolbar {
          display: grid;
          gap: 10px;
          padding: 14px;
          border-bottom: 1px solid color-mix(
            in srgb,
            var(--th-border) 62%,
            transparent
          );
        }

        .meetings-page-shell .meeting-search {
          display: flex;
          min-height: 42px;
          align-items: center;
          gap: 9px;
          min-width: 0;
          border-radius: 12px;
          border: 1px solid color-mix(in srgb, var(--th-border) 70%, transparent);
          background: color-mix(in srgb, var(--th-bg-surface) 90%, transparent);
          padding: 0 12px;
          color: var(--th-text-muted);
        }

        .meetings-page-shell .meeting-search input {
          min-width: 0;
          width: 100%;
          border: 0;
          outline: none;
          background: transparent;
          color: var(--th-text);
          font-size: 13px;
          line-height: 1.4;
        }

        .meetings-page-shell .meeting-search input::placeholder {
          color: var(--th-text-muted);
        }

        .meetings-page-shell .meeting-filter-tabs {
          display: flex;
          flex-wrap: wrap;
          gap: 6px;
        }

        .meetings-page-shell .meeting-filter-tabs button {
          min-height: 34px;
          border: 1px solid color-mix(in srgb, var(--th-border) 70%, transparent);
          border-radius: 999px;
          background: color-mix(in srgb, var(--th-bg-surface) 88%, transparent);
          color: var(--th-text-muted);
          padding: 0 11px;
          font-size: 11.5px;
          font-weight: 600;
        }

        .meetings-page-shell .meeting-filter-tabs button.active {
          border-color: color-mix(
            in srgb,
            var(--th-accent-primary) 30%,
            var(--th-border) 70%
          );
          background: color-mix(in srgb, var(--th-accent-primary-soft) 58%, transparent);
          color: var(--th-text);
        }

        .meetings-page-shell .meeting-result-count {
          font-size: 11px;
          color: var(--th-text-muted);
          font-variant-numeric: tabular-nums;
        }

        @media (min-width: 680px) {
          .meetings-page-shell .meeting-list-toolbar {
            grid-template-columns: minmax(220px, 1fr) auto;
            align-items: center;
          }

          .meetings-page-shell .meeting-filter-tabs {
            justify-content: flex-end;
          }

          .meetings-page-shell .meeting-result-count {
            grid-column: 1 / -1;
          }
        }

        .meetings-page-shell .empty-state {
          padding: 48px 20px;
          text-align: center;
          font-size: 13px;
          color: var(--th-text-muted);
        }

        .meetings-page-shell .meeting-item {
          width: 100%;
          padding: 14px 16px;
          text-align: left;
          background: transparent;
          border-bottom: 1px solid color-mix(
            in srgb,
            var(--th-border) 62%,
            transparent
          );
          transition: background 0.12s ease, border-color 0.12s ease;
        }

        .meetings-page-shell .meeting-item:last-child {
          border-bottom: 0;
        }

        .meetings-page-shell .meeting-item:hover {
          background: color-mix(in srgb, var(--th-bg-surface) 76%, transparent);
        }

        .meetings-page-shell .meeting-item.active {
          background: color-mix(
            in srgb,
            var(--th-accent-primary-soft) 38%,
            transparent
          );
        }

        .meetings-page-shell .mi-head {
          display: flex;
          align-items: flex-start;
          justify-content: space-between;
          gap: 12px;
        }

        .meetings-page-shell .mi-title {
          font-size: 13.5px;
          font-weight: 600;
          letter-spacing: -0.1px;
          line-height: 1.3;
          color: var(--th-text-heading);
        }

        .meetings-page-shell .mi-meta {
          display: flex;
          flex-wrap: wrap;
          gap: 8px;
          margin-top: 8px;
          font-size: 11.5px;
          color: var(--th-text-muted);
          font-variant-numeric: tabular-nums;
        }

        .meetings-page-shell .mono {
          font-family: var(--font-mono);
        }

        .meetings-page-shell .mi-summary {
          margin-top: 8px;
          display: -webkit-box;
          overflow: hidden;
          font-size: 12.5px;
          line-height: 1.55;
          color: var(--th-text-muted);
          -webkit-box-orient: vertical;
          -webkit-line-clamp: 2;
        }

        .meetings-page-shell .mi-foot {
          margin-top: 12px;
          display: flex;
          flex-wrap: wrap;
          gap: 8px;
        }

        .meetings-page-shell .detail-column {
          display: grid;
          gap: 14px;
          align-content: start;
        }

        .meetings-page-shell .meeting-detail,
        .meetings-page-shell .meeting-workbench,
        .meetings-page-shell .skill-rail {
          padding: 16px;
        }

        .meetings-page-shell .md-head,
        .meetings-page-shell .section-head {
          display: flex;
          align-items: flex-start;
          justify-content: space-between;
          gap: 12px;
        }

        .meetings-page-shell .section-kicker {
          font-size: 10px;
          font-weight: 600;
          letter-spacing: 0.16em;
          text-transform: uppercase;
          color: var(--th-text-muted);
        }

        .meetings-page-shell .section-title {
          margin-top: 8px;
          font-size: 18px;
          font-weight: 600;
          letter-spacing: -0.2px;
          color: var(--th-text-heading);
        }

        .meetings-page-shell .section-copy {
          margin-top: 8px;
          font-size: 13px;
          line-height: 1.65;
          color: var(--th-text-muted);
        }

        .meetings-page-shell .section-icon {
          display: inline-flex;
          align-items: center;
          justify-content: center;
          width: 40px;
          height: 40px;
          flex: 0 0 auto;
          border-radius: 16px;
          border: 1px solid
            color-mix(in srgb, var(--th-accent-info) 28%, var(--th-border) 72%);
          background: color-mix(in srgb, var(--th-accent-info) 14%, transparent);
          color: var(--th-accent-info);
        }

        .meetings-page-shell .md-title {
          margin-top: 8px;
          font-size: 18px;
          font-weight: 600;
          letter-spacing: -0.2px;
          color: var(--th-text-heading);
        }

        .meetings-page-shell .md-status-row {
          margin-top: 6px;
          display: flex;
          flex-wrap: wrap;
          align-items: center;
          gap: 8px;
        }

        .meetings-page-shell .md-status-copy {
          font-size: 11px;
          color: var(--th-text-muted);
        }

        .meetings-page-shell .md-toolbar {
          display: flex;
          gap: 6px;
          flex-wrap: wrap;
          justify-content: flex-end;
        }

        .meetings-page-shell .md-meta-row {
          display: grid;
          gap: 10px;
          grid-template-columns: repeat(2, minmax(0, 1fr));
          margin-top: 14px;
        }

        @media (min-width: 768px) {
          .meetings-page-shell .md-meta-row {
            grid-template-columns: repeat(4, minmax(0, 1fr));
          }
        }

        .meetings-page-shell .md-meta {
          display: flex;
          flex-direction: column;
          gap: 8px;
          min-width: 0;
          padding: 12px 13px;
          border-radius: 14px;
          border: 1px solid color-mix(in srgb, var(--th-border) 70%, transparent);
          background: color-mix(in srgb, var(--th-bg-surface) 90%, transparent);
        }

        .meetings-page-shell .md-meta > span:first-child {
          font-size: 10px;
          font-weight: 600;
          letter-spacing: 0.14em;
          text-transform: uppercase;
          color: var(--th-text-muted);
        }

        .meetings-page-shell .md-meta b {
          font-size: 13px;
          font-weight: 600;
          color: var(--th-text-heading);
        }

        .meetings-page-shell .md-meta-stack {
          display: flex;
          flex-wrap: wrap;
          gap: 6px;
        }

        .meetings-page-shell .md-attendees {
          display: flex;
          align-items: center;
          min-height: 22px;
          padding-left: 6px;
        }

        .meetings-page-shell .md-attendee {
          display: inline-flex;
          align-items: center;
          justify-content: center;
          width: 22px;
          height: 22px;
          flex: 0 0 auto;
          border-radius: 999px;
          border: 1px solid color-mix(in srgb, var(--th-border) 70%, transparent);
          background: color-mix(in srgb, var(--th-card-bg) 94%, transparent);
          color: var(--th-text-secondary);
          font-size: 9.5px;
          font-weight: 700;
          letter-spacing: 0.02em;
          box-shadow: 0 0 0 1px color-mix(in srgb, var(--th-card-bg) 96%, transparent);
        }

        .meetings-page-shell .md-attendee-count {
          width: auto;
          min-width: 24px;
          padding: 0 6px;
          font-size: 10px;
        }

        .meetings-page-shell .md-section {
          margin-top: 16px;
        }

        .meetings-page-shell .md-section-head {
          display: flex;
          align-items: center;
          gap: 6px;
          font-size: 11px;
          font-weight: 600;
          letter-spacing: 0.14em;
          text-transform: uppercase;
          color: var(--th-text-muted);
        }

        .meetings-page-shell .md-inline-note {
          font-size: 10px;
          font-weight: 500;
          letter-spacing: normal;
          text-transform: none;
          color: var(--th-text-muted);
        }

        .meetings-page-shell .md-copy,
        .meetings-page-shell .md-empty-copy {
          margin-top: 10px;
          font-size: 13px;
          line-height: 1.65;
          color: var(--th-text-muted);
        }

        .meetings-page-shell .md-actions {
          margin-top: 10px;
          display: grid;
          gap: 8px;
        }

        .meetings-page-shell .md-action {
          display: flex;
          align-items: flex-start;
          gap: 10px;
          padding: 10px 11px;
          border-radius: 14px;
          border: 1px solid color-mix(in srgb, var(--th-border) 68%, transparent);
          background: color-mix(in srgb, var(--th-bg-surface) 88%, transparent);
        }

        .meetings-page-shell .md-action-check {
          display: inline-flex;
          align-items: center;
          justify-content: center;
          width: 18px;
          height: 18px;
          margin-top: 2px;
          flex: 0 0 auto;
          border-radius: 999px;
          border: 1px solid color-mix(in srgb, var(--th-border) 68%, transparent);
          background: color-mix(in srgb, var(--th-card-bg) 92%, transparent);
          color: var(--th-text-muted);
        }

        .meetings-page-shell .md-action-check.created {
          color: var(--th-accent-success);
          border-color: color-mix(
            in srgb,
            var(--th-accent-success) 30%,
            var(--th-border) 70%
          );
        }

        .meetings-page-shell .md-action-check.pending {
          color: var(--th-accent-warn);
          border-color: color-mix(
            in srgb,
            var(--th-accent-warn) 30%,
            var(--th-border) 70%
          );
        }

        .meetings-page-shell .md-action-check.failed {
          color: var(--th-accent-danger);
          border-color: color-mix(
            in srgb,
            var(--th-accent-danger) 30%,
            var(--th-border) 70%
          );
        }

        .meetings-page-shell .md-action-check.discarded {
          color: var(--th-text-muted);
        }

        .meetings-page-shell .md-action-copy {
          min-width: 0;
          flex: 1;
        }

        .meetings-page-shell .md-action-title {
          font-size: 12.5px;
          font-weight: 600;
          line-height: 1.5;
          color: var(--th-text-heading);
        }

        .meetings-page-shell .md-action-title.created,
        .meetings-page-shell .md-action-title.discarded {
          color: var(--th-text-muted);
          text-decoration: line-through;
        }

        .meetings-page-shell .md-action-sub {
          margin-top: 3px;
          font-size: 12px;
          line-height: 1.5;
          color: var(--th-text-muted);
        }

        .meetings-page-shell .md-transcript {
          margin-top: 10px;
          display: grid;
          gap: 8px;
        }

        .meetings-page-shell .md-transcript-line {
          display: flex;
          align-items: flex-start;
          gap: 10px;
          padding: 10px 11px;
          border-radius: 14px;
          border: 1px solid color-mix(in srgb, var(--th-border) 68%, transparent);
          background: color-mix(in srgb, var(--th-card-bg) 92%, transparent);
        }

        .meetings-page-shell .md-who {
          min-width: 72px;
          flex: 0 0 auto;
          font-size: 11px;
          font-weight: 600;
          letter-spacing: 0.12em;
          text-transform: uppercase;
          color: var(--th-text-muted);
        }

        .meetings-page-shell .md-said {
          min-width: 0;
          flex: 1;
          font-size: 12.5px;
          line-height: 1.55;
          color: var(--th-text-muted);
        }

        .meetings-page-shell .workbench-meta {
          margin-top: 12px;
          display: flex;
          flex-wrap: wrap;
          gap: 8px;
        }

        .meetings-page-shell .workbench-shell {
          margin-top: 14px;
          min-width: 0;
        }
      `}</style>

      <div className="page fade-in meetings-page-shell">
        <div className="page-header">
          <div className="min-w-0">
            <div className="page-title">{t({ ko: "회의록", en: "Meetings" })}</div>
            <div className="page-sub">
              {t({
                ko: "회의 기록·요약·후속 액션을 칸반과 연동합니다",
                en: "Meeting records, summaries, and follow-ups stay linked to kanban.",
              })}
            </div>
          </div>
          <button type="button" onClick={handleLaunchMeeting} className="btn primary">
            <Plus size={14} />
            {t({ ko: "새 회의 기록", en: "New Meeting Record" })}
          </button>
        </div>

        {!isDesktopSplit && (
          <div
            className="meeting-pane-tabs"
            aria-label={t({ ko: "회의 및 스킬 보기", en: "Meetings and skills views" })}
          >
            <button
              type="button"
              className={`meeting-pane-tab ${mobilePane === "meetings" ? "active" : ""}`}
              aria-pressed={mobilePane === "meetings"}
              onClick={() => setMobilePane("meetings")}
            >
              {t({ ko: "회의", en: "Meetings" })}
            </button>
            <button
              type="button"
              className={`meeting-pane-tab ${mobilePane === "skills" ? "active" : ""}`}
              aria-pressed={mobilePane === "skills"}
              onClick={() => setMobilePane("skills")}
            >
              {t({ ko: "스킬", en: "Skills" })}
            </button>
          </div>
        )}

        {(isDesktopSplit || mobilePane === "meetings") && (
          <div className="meetings-split">
            {meetingListCard}
            {isDesktopSplit ? desktopDetailColumn : mobileDetailColumn}
          </div>
        )}

        {!isDesktopSplit && mobilePane === "skills" && skillRailCard}
      </div>
    </div>
  );
}
