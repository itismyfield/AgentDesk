import {
  AlertTriangle,
  Check,
  Clock3,
  Minus,
  Plus,
  Search,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { countOpenMeetingIssues } from "../app/meetingSummary";
import { useI18n } from "../i18n";
import {
  getMeetingIssueResult,
  getMeetingIssueState,
  getMeetingIssueTone,
  getProposedIssueKey,
} from "../lib/meetingHelpers";
import type {
  RoundTableMeeting,
  RoundTableMeetingChannelOption,
} from "../types";
import MeetingSkillRailCard from "./MeetingSkillRailCard";
import MeetingWorkbenchCard from "./MeetingWorkbenchCard";
import {
  MEETING_TOGGLE_PATTERNS,
  clickMatchingButton,
  formatMeetingDate,
  formatMeetingDuration,
  formatProvider,
  formatRelativeTime,
  getMeetingIssueStateLabel,
  getMeetingStatusLabel,
  getMeetingStatusTone,
  getParticipantInitials,
  meetingSearchText,
  useDesktopSplitLayout,
  type MeetingNotificationUpdater,
  type MeetingNotifier,
  type MeetingStatusFilter,
  type MobilePane,
} from "./meetingsAndSkillsModel";
import "./MeetingsAndSkillsPage.css";
import "./MeetingsAndSkillsPage.detail.css";

interface MeetingsAndSkillsPageProps {
  meetings: RoundTableMeeting[];
  onRefresh: () => void;
  onNotify?: MeetingNotifier;
  onUpdateNotification?: MeetingNotificationUpdater;
  initialShowStartForm?: boolean;
  initialMeetingChannels?: RoundTableMeetingChannelOption[];
  initialChannelId?: string;
}

type MeetingPageTranslator = ReturnType<typeof useI18n>["t"];

function MeetingIssueCountChip({
  created,
  total,
  tone,
  t,
}: {
  created: number;
  total: number;
  tone: "ok" | "warn" | "err";
  t: MeetingPageTranslator;
}) {
  const Icon = tone === "ok" ? Check : AlertTriangle;
  const label = total > 0 ? `${created}/${total}` : "0/0";
  return (
    <span
      className={`chip ${tone}`}
      aria-label={t({ ko: `후속 액션 ${label}`, en: `Follow-up actions ${label}` })}
    >
      <Icon size={10} aria-hidden="true" />
      {label}
    </span>
  );
}

function MeetingStatusChip({
  meeting,
  t,
}: {
  meeting: RoundTableMeeting;
  t: MeetingPageTranslator;
}) {
  const label = getMeetingStatusLabel(meeting, t);
  const tone = getMeetingStatusTone(meeting);
  const Icon = meeting.status === "completed" ? Check : Clock3;
  return (
    <span
      className={`chip ${tone}`}
      aria-label={t({ ko: `회의 상태: ${label}`, en: `Meeting status: ${label}` })}
    >
      <Icon size={10} aria-hidden="true" />
      {label}
    </span>
  );
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
                <MeetingIssueCountChip
                  created={meeting.issues_created}
                  total={issueTotal}
                  tone={issueTone}
                  t={t}
                />
              </div>
              <div className="mi-summary">
                {meeting.summary ??
                  t({
                    ko: "요약이 아직 없습니다.",
                    en: "No summary yet.",
                  })}
              </div>
              <div className="mi-foot">
                <MeetingStatusChip meeting={meeting} t={t} />
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
              <MeetingStatusChip meeting={selectedMeeting} t={t} />
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
    <MeetingWorkbenchCard
      t={t}
      meetings={meetings}
      meetingShellRef={meetingShellRef}
      onRefresh={onRefresh}
      onNotify={onNotify}
      onUpdateNotification={onUpdateNotification}
      initialShowStartForm={initialShowStartForm}
      initialMeetingChannels={initialMeetingChannels}
      initialChannelId={initialChannelId}
      onLaunchMeeting={handleLaunchMeeting}
    />
  );

  const skillRailCard = <MeetingSkillRailCard t={t} />;

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
