import { useMemo } from "react";
import type { RefObject } from "react";
import { Plus, Users, Workflow } from "lucide-react";
import { summarizeMeetings } from "../app/meetingSummary";
import type { I18nContextValue } from "../i18n";
import type {
  RoundTableMeeting,
  RoundTableMeetingChannelOption,
} from "../types";
import MeetingMinutesView from "./MeetingMinutesView";
import type {
  MeetingNotificationUpdater,
  MeetingNotifier,
} from "./meetingsAndSkillsModel";
import { formatProvider } from "./meetingsAndSkillsModel";

interface MeetingWorkbenchCardProps {
  t: I18nContextValue["t"];
  meetings: RoundTableMeeting[];
  meetingShellRef: RefObject<HTMLDivElement | null>;
  onRefresh: () => void;
  onNotify?: MeetingNotifier;
  onUpdateNotification?: MeetingNotificationUpdater;
  initialShowStartForm: boolean;
  initialMeetingChannels: RoundTableMeetingChannelOption[];
  initialChannelId?: string;
  onLaunchMeeting: () => void;
}

export default function MeetingWorkbenchCard({
  t,
  meetings,
  meetingShellRef,
  onRefresh,
  onNotify,
  onUpdateNotification,
  initialShowStartForm,
  initialMeetingChannels,
  initialChannelId,
  onLaunchMeeting,
}: MeetingWorkbenchCardProps) {
  const meetingSummary = useMemo(() => summarizeMeetings(meetings), [meetings]);
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

  return (
    <div className="p-4 sm:p-5 min-w-0 overflow-hidden rounded-lg border border-th-border-subtle bg-th-bg-surface">
      <div className="flex min-w-0 items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="text-xs leading-5 text-th-text-muted">
            {t({ ko: "실시간 작성", en: "Live Workbench" })}
          </div>
          <div className="mt-1 text-base font-semibold leading-6 text-th-text-heading">
            {t({
              ko: "회의 생성과 후속 흐름",
              en: "Meeting creation and follow-up flow",
            })}
          </div>
          <div className="mt-2 text-xs leading-5 text-th-text-muted [overflow-wrap:anywhere]">
            {t({
              ko: "회의를 시작하고 기록을 검토하거나 후속 액션을 정리합니다.",
              en: "Start a meeting, review its record, and manage follow-up actions.",
            })}
          </div>
        </div>
        <button type="button" className="inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center gap-2 rounded-md border border-th-border-subtle px-3 text-xs font-medium text-th-text-primary transition-colors hover:bg-surface-subtle focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--th-accent-primary)]" onClick={onLaunchMeeting}>
          <Plus size={11} />
          {t({ ko: "열기", en: "Open" })}
        </button>
      </div>

      <div className="mt-3 flex flex-wrap gap-x-3 gap-y-1">
        <span className="inline-flex max-w-full items-center gap-1.5 text-xs leading-5 text-th-text-muted [overflow-wrap:anywhere]">
          <Users size={11} />
          {t({
            ko: `${totalParticipants}명 누적`,
            en: `${totalParticipants} people total`,
          })}
        </span>
        <span className="inline-flex max-w-full items-center gap-1.5 text-xs leading-5 text-th-text-muted [overflow-wrap:anywhere]">
          <Workflow size={11} />
          {t({
            ko: `${meetingSummary.unresolvedCount}건 미해결`,
            en: `${meetingSummary.unresolvedCount} unresolved`,
          })}
        </span>
        {providerCounts.slice(0, 2).map(([provider]) => (
          <span key={provider} className="inline-flex max-w-full items-center gap-1.5 text-xs leading-5 text-th-text-muted [overflow-wrap:anywhere]">
            <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-current" />
            {formatProvider(provider)}
          </span>
        ))}
      </div>

      <div ref={meetingShellRef} className="mt-4 min-w-0">
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
}
