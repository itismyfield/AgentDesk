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
      className={`inline-flex max-w-full items-center gap-1.5 text-xs leading-5 ${tone === "err" ? "text-[var(--th-badge-rose-text)]" : tone === "warn" ? "text-[var(--th-badge-amber-text)]" : "text-th-text-muted"}`}
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
      className={`inline-flex max-w-full items-center gap-1.5 text-xs leading-5 ${tone === "err" ? "text-[var(--th-badge-rose-text)]" : tone === "warn" ? "text-[var(--th-badge-amber-text)]" : "text-th-text-muted"}`}
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
    <div data-testid="meetings-page-timeline" className="min-w-0 overflow-hidden rounded-lg border border-th-border-subtle bg-th-bg-surface">
      <div className="grid min-w-0 gap-3 border-b border-th-border-subtle p-4">
        <label className="flex min-h-11 min-w-0 items-center gap-2 rounded-md border border-th-input-border bg-th-input-bg px-3 text-th-text-muted [&_input]:min-w-0 [&_input]:w-full [&_input]:bg-transparent [&_input]:text-sm [&_input]:text-th-text-primary [&_input]:outline-none">
          <Search size={14} />
          <input
            type="search"
            aria-label={t({ ko: "회의 검색", en: "Search meetings" })}
            value={meetingQuery}
            onChange={(event) => setMeetingQuery(event.target.value)}
            placeholder={t({ ko: "회의, 참석자, 후속 액션 검색", en: "Search meetings, people, actions" })}
          />
        </label>
        <div className="flex flex-wrap gap-1" aria-label={t({ ko: "회의 상태 필터", en: "Meeting status filter" })}>
          {meetingStatusOptions.map((option) => (
            <button
              key={option.id}
              type="button"
              className={`min-h-11 min-w-11 border-b-2 px-3 text-xs font-medium ${meetingStatusFilter === option.id ? "border-th-text-primary text-th-text-primary" : "border-transparent text-th-text-muted"}`}
              aria-pressed={meetingStatusFilter === option.id}
              onClick={() => setMeetingStatusFilter(option.id)}
            >
              {option.label}
            </button>
          ))}
        </div>
        <div className="text-xs tabular-nums text-th-text-muted">
          {t({
            ko: `${filteredMeetings.length}/${sortedMeetings.length}건`,
            en: `${filteredMeetings.length}/${sortedMeetings.length} shown`,
          })}
        </div>
      </div>
      {sortedMeetings.length === 0 ? (
        <div className="px-4 py-10 text-center text-sm leading-6 text-th-text-muted">
          {t({
            ko: "아직 등록된 회의가 없습니다.",
            en: "No meetings have been recorded yet.",
          })}
        </div>
      ) : filteredMeetings.length === 0 ? (
        <div className="px-4 py-10 text-center text-sm leading-6 text-th-text-muted">
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
              className={`block min-h-11 w-full min-w-0 border-b border-th-border-subtle p-4 text-left transition-colors last:border-b-0 hover:bg-surface-subtle focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-[var(--th-accent-primary)] ${
                selectedMeeting?.id === meeting.id ? "bg-surface-subtle" : ""
              }`}
              aria-label={t({
                ko: `${meeting.agenda} 회의 선택`,
                en: `Select meeting: ${meeting.agenda}`,
              })}
              aria-pressed={selectedMeeting?.id === meeting.id}
              onClick={() => setSelectedMeetingId(meeting.id)}
            >
              <div className="flex min-w-0 items-start justify-between gap-3">
                <div className="min-w-0">
                  <div className="text-sm font-semibold leading-6 text-th-text-heading [overflow-wrap:anywhere]">{meeting.agenda}</div>
                  <div className="mt-1 flex flex-wrap gap-x-2 gap-y-1 text-xs leading-5 tabular-nums text-th-text-muted">
                    <span
                      className="font-mono tabular-nums"
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
              <div className="mt-2 text-xs leading-5 text-th-text-muted [overflow-wrap:anywhere]">
                {meeting.summary ??
                  t({
                    ko: "요약이 아직 없습니다.",
                    en: "No summary yet.",
                  })}
              </div>
              <div className="mt-3 flex flex-wrap gap-x-3 gap-y-1">
                <MeetingStatusChip meeting={meeting} t={t} />
                <span className="inline-flex max-w-full items-center gap-1.5 text-xs leading-5 text-th-text-muted [overflow-wrap:anywhere]">
                  <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-current" />
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
    <div className="p-4 sm:p-5 min-w-0 overflow-hidden rounded-lg border border-th-border-subtle bg-th-bg-surface">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="text-xs leading-5 text-th-text-muted">{t({ ko: "회의록", en: "Meeting" })}</div>
          {selectedMeeting ? (
            <div className="mt-1 flex flex-wrap items-center gap-2">
              <MeetingStatusChip meeting={selectedMeeting} t={t} />
              <span className="text-xs text-th-text-muted">
                {formatMeetingDate(selectedMeeting.started_at, locale)}
              </span>
            </div>
          ) : null}
          <h2 className="mt-2 text-base font-semibold leading-6 text-th-text-heading [overflow-wrap:anywhere]">
            {selectedMeeting?.agenda ??
              t({
                ko: "선택된 회의가 없습니다",
                en: "No meeting selected",
              })}
          </h2>
        </div>
        <div className="flex shrink-0 flex-wrap justify-end gap-2">
          <button type="button" className="inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center gap-2 rounded-md border border-th-border-subtle px-3 text-xs font-medium text-th-text-primary transition-colors hover:bg-surface-subtle focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--th-accent-primary)]" onClick={onRefresh}>
            {t({ ko: "새로고침", en: "Refresh" })}
          </button>
          <button type="button" className="inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center gap-2 rounded-md border border-th-border-subtle px-3 text-xs font-medium text-th-text-primary transition-colors hover:bg-surface-subtle focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--th-accent-primary)]" onClick={handleLaunchMeeting}>
            <Plus size={11} />
            {t({ ko: "열기", en: "Open" })}
          </button>
        </div>
      </div>

      {selectedMeeting ? (
        <>
          <div className="mt-4 grid grid-cols-2 gap-4 border-y border-th-border-subtle py-4">
            <div className="min-w-0 text-xs leading-5 text-th-text-muted [&_b]:font-medium [&_b]:text-th-text-primary [&_span]:block">
              <span>{t({ ko: "시작", en: "Started" })}</span>
              <b className="font-mono tabular-nums">
                {formatMeetingDate(selectedMeeting.started_at, locale)}
              </b>
            </div>
            <div className="min-w-0 text-xs leading-5 text-th-text-muted [&_b]:font-medium [&_b]:text-th-text-primary [&_span]:block">
              <span>{t({ ko: "길이", en: "Length" })}</span>
              <b>{selectedDurationLabel}</b>
            </div>
            <div className="min-w-0 text-xs leading-5 text-th-text-muted [&_b]:font-medium [&_b]:text-th-text-primary [&_span]:block">
              <span>{t({ ko: "프로바이더", en: "Provider" })}</span>
              <div className="mt-1 flex flex-wrap gap-2">
                <span className="inline-flex max-w-full items-center gap-1.5 text-xs leading-5 text-th-text-muted [overflow-wrap:anywhere]">
                  <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-current" />
                  {formatProvider(selectedMeeting.primary_provider)}
                </span>
                <span className="inline-flex max-w-full items-center gap-1.5 text-xs leading-5 text-th-text-muted [overflow-wrap:anywhere]">
                  <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-current" />
                  {formatProvider(selectedMeeting.reviewer_provider)}
                </span>
              </div>
            </div>
            <div className="min-w-0 text-xs leading-5 text-th-text-muted [&_b]:font-medium [&_b]:text-th-text-primary [&_span]:block">
              <span>{t({ ko: "참석", en: "Attendees" })}</span>
              <div className="mt-1 flex flex-wrap items-center">
                {selectedMeeting.participant_names.slice(0, 4).map((name, index) => (
                  <span
                    key={`${selectedMeeting.id}-${name}-${index}`}
                    className="inline-flex h-7 w-7 shrink-0 items-center justify-center rounded-full border border-th-border-subtle bg-th-bg-primary text-[10px] text-th-text-secondary"
                    title={name}
                    style={{ marginLeft: index === 0 ? 0 : -6 }}
                  >
                    {getParticipantInitials(name)}
                  </span>
                ))}
                {selectedMeeting.participant_names.length > 4 ? (
                  <span className="inline-flex h-7 w-7 shrink-0 items-center justify-center rounded-full border border-th-border-subtle bg-th-bg-primary text-[10px] text-th-text-secondary">
                    +{selectedMeeting.participant_names.length - 4}
                  </span>
                ) : null}
              </div>
            </div>
          </div>

          <div className="mt-5">
            <div className="flex flex-wrap items-center gap-2 text-xs font-medium text-th-text-secondary">{t({ ko: "요약", en: "Summary" })}</div>
            <p className="mt-2 text-sm leading-6 text-th-text-secondary [overflow-wrap:anywhere]">
              {selectedMeeting.summary ??
                t({
                  ko: "요약이 아직 없습니다.",
                  en: "No summary yet.",
                })}
            </p>
          </div>

          <div className="mt-5">
            <div className="flex flex-wrap items-center gap-2 text-xs font-medium text-th-text-secondary">
              {t({ ko: "후속 액션", en: "Follow-up Actions" })}
              <span className="text-xs font-normal text-th-text-muted">
                {t({
                  ko: `${selectedOpenIssues}/${selectedActionCount || 0} 미완료`,
                  en: `${selectedOpenIssues}/${selectedActionCount || 0} open`,
                })}
              </span>
            </div>
            {selectedMeeting.proposed_issues?.length ? (
              <div className="mt-2 grid divide-y divide-th-border-subtle">
                {selectedMeeting.proposed_issues.slice(0, 6).map((issue) => {
                  const result = getMeetingIssueResult(selectedMeeting, issue);
                  const state = getMeetingIssueState(result);
                  const tone = getMeetingIssueTone(state);
                  const stateLabel = getMeetingIssueStateLabel(state, t);
                  return (
                    <div key={getProposedIssueKey(issue)} className="flex min-w-0 items-start gap-2 py-3">
                      <span className={`mt-0.5 grid h-5 w-5 shrink-0 place-items-center rounded border border-th-border-subtle ${state === "failed" ? "text-[var(--th-badge-rose-text)]" : state === "pending" ? "text-[var(--th-badge-amber-text)]" : "text-th-text-muted"}`}>
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
                      <div className="min-w-0 flex-1">
                        <div className={`text-sm font-medium leading-6 [overflow-wrap:anywhere] ${state === "created" || state === "discarded" ? "text-th-text-muted line-through" : "text-th-text-primary"}`}>
                          {issue.title}
                        </div>
                        <div className="mt-1 text-xs leading-5 text-th-text-muted [overflow-wrap:anywhere]">
                          {issue.assignee}
                          {result?.error ? ` · ${result.error}` : ""}
                        </div>
                      </div>
                      <span className={`inline-flex max-w-full items-center gap-1.5 text-xs leading-5 ${tone === "err" ? "text-[var(--th-badge-rose-text)]" : tone === "warn" ? "text-[var(--th-badge-amber-text)]" : "text-th-text-muted"}`}>
                        <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-current" />
                        {stateLabel}
                      </span>
                    </div>
                  );
                })}
              </div>
            ) : (
              <div className="mt-2 text-xs leading-5 text-th-text-muted">
                {t({
                  ko: "후속 액션이 아직 없습니다.",
                  en: "No follow-up actions yet.",
                })}
              </div>
            )}
          </div>

          <div className="mt-5">
            <div className="flex flex-wrap items-center gap-2 text-xs font-medium text-th-text-secondary">
              {t({ ko: "전사 (발췌)", en: "Transcript Excerpt" })}
            </div>
            {selectedTranscript.length > 0 ? (
              <div className="mt-2 grid divide-y divide-th-border-subtle">
                {selectedTranscript.map((entry) => (
                  <div key={entry.id} className="grid min-w-0 gap-1 py-3 sm:grid-cols-[5rem_minmax(0,1fr)] sm:gap-3">
                    <span className="min-w-0 text-xs font-medium leading-5 text-th-text-secondary [overflow-wrap:anywhere]">{entry.speaker_name}</span>
                    <span className="min-w-0 text-sm leading-6 text-th-text-secondary [overflow-wrap:anywhere]">{entry.content}</span>
                  </div>
                ))}
              </div>
            ) : (
              <div className="mt-2 text-xs leading-5 text-th-text-muted">
                {t({
                  ko: "전사 내용이 아직 없습니다.",
                  en: "No transcript yet.",
                })}
              </div>
            )}
          </div>
        </>
      ) : (
        <div className="mt-2 text-xs leading-5 text-th-text-muted">
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
    <div className="grid min-w-0 gap-4">
      {selectedMeeting ? meetingDetailCard : null}
      {meetingWorkbenchCard}
      {skillRailCard}
    </div>
  );

  const mobileDetailColumn = (
    <div className="grid min-w-0 gap-4">
      {selectedMeeting ? meetingDetailCard : null}
      {meetingWorkbenchCard}
    </div>
  );

  return (
    <div
      data-testid="meetings-page"
      className="mx-auto h-full w-full max-w-[88rem] min-w-0 overflow-x-hidden overflow-y-auto p-4 pb-8 sm:p-8"
      style={{
        paddingBottom: "max(2rem, env(safe-area-inset-bottom))",
      }}
    >
      <div className="flex min-w-0 flex-col gap-5">
        <div className="flex flex-wrap items-start justify-between gap-4 border-b border-th-border-subtle pb-5">
          <div className="min-w-0">
            <div className="text-2xl font-semibold tracking-tight text-th-text-heading">{t({ ko: "회의록", en: "Meetings" })}</div>
            <div className="mt-2 max-w-2xl text-xs leading-5 text-th-text-muted">
              {t({
                ko: "회의 기록·요약·후속 액션을 칸반과 연동합니다",
                en: "Meeting records, summaries, and follow-ups stay linked to kanban.",
              })}
            </div>
          </div>
          <button type="button" onClick={handleLaunchMeeting} className="inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center gap-2 rounded-md border border-th-border-subtle px-3 text-xs font-medium transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--th-accent-primary)] bg-th-text-primary text-th-bg-primary hover:bg-th-text-secondary">
            <Plus size={14} />
            {t({ ko: "새 회의 기록", en: "New Meeting Record" })}
          </button>
        </div>

        {!isDesktopSplit && (
          <div
            className="flex gap-1 border-b border-th-border-subtle"
            aria-label={t({ ko: "회의 및 스킬 보기", en: "Meetings and skills views" })}
          >
            <button
              type="button"
              className={`min-h-11 min-w-11 border-b-2 px-4 text-xs font-medium ${mobilePane === "meetings" ? "border-th-text-primary text-th-text-primary" : "border-transparent text-th-text-muted"}`}
              aria-pressed={mobilePane === "meetings"}
              onClick={() => setMobilePane("meetings")}
            >
              {t({ ko: "회의", en: "Meetings" })}
            </button>
            <button
              type="button"
              className={`min-h-11 min-w-11 border-b-2 px-4 text-xs font-medium ${mobilePane === "skills" ? "border-th-text-primary text-th-text-primary" : "border-transparent text-th-text-muted"}`}
              aria-pressed={mobilePane === "skills"}
              onClick={() => setMobilePane("skills")}
            >
              {t({ ko: "스킬", en: "Skills" })}
            </button>
          </div>
        )}

        {(isDesktopSplit || mobilePane === "meetings") && (
          <div className="grid min-w-0 items-start gap-4 lg:grid-cols-[minmax(0,1fr)_minmax(0,1fr)]">
            {meetingListCard}
            {isDesktopSplit ? desktopDetailColumn : mobileDetailColumn}
          </div>
        )}

        {!isDesktopSplit && mobilePane === "skills" && skillRailCard}
      </div>
    </div>
  );
}
