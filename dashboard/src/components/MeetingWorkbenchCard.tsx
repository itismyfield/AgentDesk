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
        <button type="button" className="btn sm" onClick={onLaunchMeeting}>
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
}
