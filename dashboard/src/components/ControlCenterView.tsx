import { useCallback, useMemo } from "react";
import type { Agent, CompanySettings, Department, DispatchedSession, Office, RoundTableMeeting } from "../types";
import {
  NOTIFICATION_TYPE_COLORS,
  type Notification,
} from "./NotificationCenter";
import AgentManagerView from "./AgentManagerView";
import MeetingMinutesView from "./MeetingMinutesView";
import OfficeManagerView from "./OfficeManagerView";
import SettingsView from "./SettingsView";
import { SessionPanel } from "./session-panel/SessionPanel";

type ControlTab = "agents" | "departments" | "offices" | "settings" | "meetings";
type AgentsPane = "directory" | "dispatch";

interface ControlCenterViewProps {
  controlTab: ControlTab;
  onControlTabChange: (tab: ControlTab) => void;
  agentsPane: AgentsPane;
  onAgentsPaneChange: (pane: AgentsPane) => void;
  isKo: boolean;
  language: CompanySettings["language"];
  officeId: string | null;
  offices: Office[];
  selectedOfficeId: string | null;
  allAgents: Agent[];
  agents: Agent[];
  departments: Department[];
  sessions: DispatchedSession[];
  onAssign: (id: string, patch: Partial<DispatchedSession>) => Promise<void>;
  onAgentsChange: () => void;
  onDepartmentsChange: () => void;
  onOfficesChange: () => void;
  meetings: RoundTableMeeting[];
  onRefreshMeetings: () => void;
  settings: CompanySettings;
  onSaveSettings: (patch: Record<string, unknown>) => Promise<void>;
  notifications: Notification[];
  onDismissNotification: (id: string) => void;
}

function hasUnresolvedMeetingIssues(meeting: RoundTableMeeting): boolean {
  const totalIssues = meeting.proposed_issues?.length ?? 0;
  if (meeting.status !== "completed" || totalIssues === 0) return false;

  const results = meeting.issue_creation_results ?? [];
  if (results.length === 0) {
    return meeting.issues_created < totalIssues;
  }

  const created = results.filter((result) => result.ok && result.discarded !== true).length;
  const failed = results.filter((result) => !result.ok && result.discarded !== true).length;
  const discarded = results.filter((result) => result.discarded === true).length;
  const pending = Math.max(totalIssues - created - failed - discarded, 0);

  return pending > 0 || failed > 0;
}

export default function ControlCenterView({
  controlTab,
  onControlTabChange,
  agentsPane,
  onAgentsPaneChange,
  isKo,
  language,
  officeId,
  offices,
  selectedOfficeId,
  allAgents,
  agents,
  departments,
  sessions,
  onAssign,
  onAgentsChange,
  onDepartmentsChange,
  onOfficesChange,
  meetings,
  onRefreshMeetings,
  settings,
  onSaveSettings,
  notifications,
  onDismissNotification,
}: ControlCenterViewProps) {
  const t = useCallback((ko: string, en: string) => (isKo ? ko : en), [isKo]);
  const recentNotifications = notifications.slice(0, 3);
  const unresolvedMeetings = meetings.filter(hasUnresolvedMeetingIssues).length;

  const sections = useMemo(() => [
    {
      id: "agents" as const,
      labelKo: "에이전트",
      labelEn: "Agents",
      descriptionKo: "에이전트 프로필과 파견 세션을 관리합니다.",
      descriptionEn: "Manage agent profiles and dispatched sessions.",
      count: agents.length,
    },
    {
      id: "departments" as const,
      labelKo: "부서",
      labelEn: "Departments",
      descriptionKo: "부서 구조, 순서, 테마를 조정합니다.",
      descriptionEn: "Adjust department structure, order, and theme.",
      count: departments.length,
    },
    {
      id: "offices" as const,
      labelKo: "오피스",
      labelEn: "Offices",
      descriptionKo: "오피스와 멤버 구성을 한 화면에서 관리합니다.",
      descriptionEn: "Manage offices and memberships from one place.",
      count: offices.length,
    },
    {
      id: "settings" as const,
      labelKo: "설정",
      labelEn: "Settings",
      descriptionKo: "일반, 런타임, 시스템 설정을 조정합니다.",
      descriptionEn: "Tune general, runtime, and system settings.",
      count: undefined,
    },
    {
      id: "meetings" as const,
      labelKo: "회의 기록",
      labelEn: "Meeting Records",
      descriptionKo: "회의 상세와 후속 일감 상태를 확인합니다.",
      descriptionEn: "Review meeting details and follow-up issue status.",
      count: meetings.length,
    },
  ], [agents.length, departments.length, meetings.length, offices.length]);

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="border-b" style={{ borderColor: "var(--th-border)" }}>
        <div className="px-4 pt-4 sm:hidden">
          <div
            className="rounded-xl border px-3 py-2 text-xs"
            style={{
              borderColor: "rgba(96,165,250,0.32)",
              background: "rgba(96,165,250,0.08)",
              color: "var(--th-text-muted)",
            }}
          >
            {t("Control은 서브섹션별로 나뉘어 있습니다. 대량 편집은 데스크톱 사용을 권장합니다.", "Control is split by section. Use desktop for bulk editing work.")}
          </div>
        </div>

        <div className="px-4 py-4">
          <div className="flex flex-wrap items-start justify-between gap-3">
            <div className="min-w-0">
              <h2 className="text-lg font-semibold" style={{ color: "var(--th-text-heading)" }}>
                {t("컨트롤", "Control")}
              </h2>
              <p className="text-sm" style={{ color: "var(--th-text-muted)" }}>
                {t("운영 구조를 섹션 단위로 분리해 junk drawer를 막습니다.", "Administrative surfaces are split by section to avoid a junk drawer.")}
              </p>
            </div>
            <div className="flex flex-wrap gap-2 text-xs">
              <span className="rounded-full px-2.5 py-1" style={{ background: "rgba(59,130,246,0.12)", color: "#60a5fa" }}>
                {t("선택 오피스", "Selected Office")}: {selectedOfficeId ? offices.find((office) => office.id === selectedOfficeId)?.name_ko || offices.find((office) => office.id === selectedOfficeId)?.name || selectedOfficeId : t("전체", "All")}
              </span>
              <span className="rounded-full px-2.5 py-1" style={{ background: "rgba(16,185,129,0.12)", color: "#34d399" }}>
                {t("활성 세션", "Live Sessions")}: {sessions.filter((session) => session.status !== "disconnected").length}
              </span>
              <span className="rounded-full px-2.5 py-1" style={{ background: "rgba(245,158,11,0.12)", color: "#f59e0b" }}>
                {t("미해결 회의", "Open Meetings")}: {unresolvedMeetings}
              </span>
            </div>
          </div>

          {recentNotifications.length > 0 && (
            <div className="mt-3 space-y-2">
              {recentNotifications.map((notification) => (
                <div
                  key={notification.id}
                  className="flex items-start gap-2 rounded-xl border px-3 py-2 text-xs"
                  style={{
                    borderColor: `${NOTIFICATION_TYPE_COLORS[notification.type]}44`,
                    background: `color-mix(in srgb, ${NOTIFICATION_TYPE_COLORS[notification.type]} 10%, var(--th-surface))`,
                    color: "var(--th-text)",
                  }}
                >
                  <span
                    className="mt-1 h-2 w-2 shrink-0 rounded-full"
                    style={{ background: NOTIFICATION_TYPE_COLORS[notification.type] }}
                  />
                  <div className="min-w-0 flex-1">
                    <p className="break-words leading-relaxed">{notification.message}</p>
                  </div>
                  <button
                    onClick={() => onDismissNotification(notification.id)}
                    className="shrink-0 rounded-md px-2 py-1 text-[10px]"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {t("닫기", "Dismiss")}
                  </button>
                </div>
              ))}
            </div>
          )}
        </div>

        <div className="flex gap-2 overflow-x-auto px-4 pb-4">
          {sections.map((section) => (
            <button
              key={section.id}
              onClick={() => onControlTabChange(section.id)}
              className="min-w-[180px] rounded-2xl border px-4 py-3 text-left transition-colors"
              style={{
                borderColor: controlTab === section.id ? "rgba(99,102,241,0.44)" : "rgba(148,163,184,0.18)",
                background: controlTab === section.id ? "rgba(99,102,241,0.12)" : "var(--th-surface)",
              }}
            >
              <div className="flex items-center justify-between gap-2">
                <div className="text-sm font-semibold" style={{ color: controlTab === section.id ? "#a5b4fc" : "var(--th-text-heading)" }}>
                  {isKo ? section.labelKo : section.labelEn}
                </div>
                {section.count !== undefined && (
                  <span className="rounded-full px-2 py-0.5 text-[10px]" style={{ background: "rgba(148,163,184,0.12)", color: "var(--th-text-muted)" }}>
                    {section.count}
                  </span>
                )}
              </div>
              <div className="mt-1 text-xs leading-relaxed" style={{ color: "var(--th-text-muted)" }}>
                {isKo ? section.descriptionKo : section.descriptionEn}
              </div>
            </button>
          ))}
        </div>
      </div>

      <div className="min-h-0 flex-1 overflow-hidden">
        {controlTab === "agents" && (
          <div className="h-full overflow-x-hidden overflow-y-auto">
            <div className="mx-auto flex max-w-5xl flex-wrap gap-2 px-4 pt-4 sm:px-6">
              <button
                onClick={() => onAgentsPaneChange("directory")}
                className="rounded-full px-3 py-1.5 text-xs font-medium transition-colors"
                style={{
                  background: agentsPane === "directory" ? "rgba(59,130,246,0.14)" : "var(--th-bg-surface)",
                  color: agentsPane === "directory" ? "#60a5fa" : "var(--th-text-muted)",
                  border: "1px solid rgba(96,165,250,0.24)",
                }}
              >
                {t("에이전트 디렉터리", "Agent Directory")}
              </button>
              <button
                onClick={() => onAgentsPaneChange("dispatch")}
                className="rounded-full px-3 py-1.5 text-xs font-medium transition-colors"
                style={{
                  background: agentsPane === "dispatch" ? "rgba(16,185,129,0.14)" : "var(--th-bg-surface)",
                  color: agentsPane === "dispatch" ? "#34d399" : "var(--th-text-muted)",
                  border: "1px solid rgba(16,185,129,0.24)",
                }}
              >
                {t("파견 세션", "Dispatch Sessions")}
              </button>
            </div>

            {agentsPane === "dispatch" ? (
              <div className="mx-auto max-w-5xl px-4 pb-40 pt-4 sm:px-6">
                <div className="rounded-3xl border p-4 sm:p-5" style={{ borderColor: "rgba(148,163,184,0.18)", background: "var(--th-surface)" }}>
                  <SessionPanel
                    sessions={sessions}
                    departments={departments}
                    agents={agents}
                    onAssign={onAssign}
                  />
                </div>
              </div>
            ) : (
              <AgentManagerView
                agents={agents}
                departments={departments}
                language={language}
                officeId={officeId}
                onAgentsChange={onAgentsChange}
                onDepartmentsChange={onDepartmentsChange}
                sessions={sessions}
                onAssign={onAssign}
                activeTab="agents"
                showTabBar={false}
                title={t("에이전트 디렉터리", "Agent Directory")}
                subtitle={t("프로필, XP, 스킬, provider, 오피스 소속을 관리합니다.", "Manage profiles, XP, skills, providers, and office membership.")}
              />
            )}
          </div>
        )}

        {controlTab === "departments" && (
          <AgentManagerView
            agents={agents}
            departments={departments}
            language={language}
            officeId={officeId}
            onAgentsChange={onAgentsChange}
            onDepartmentsChange={onDepartmentsChange}
            activeTab="departments"
            showTabBar={false}
            title={t("부서 관리", "Departments")}
            subtitle={t("부서 순서, 프롬프트, 테마를 관리합니다.", "Manage department order, prompts, and visual themes.")}
          />
        )}

        {controlTab === "offices" && (
          <OfficeManagerView
            offices={offices}
            allAgents={allAgents}
            selectedOfficeId={selectedOfficeId}
            isKo={isKo}
            onChanged={onOfficesChange}
          />
        )}

        {controlTab === "settings" && (
          <SettingsView settings={settings} onSave={onSaveSettings} isKo={isKo} />
        )}

        {controlTab === "meetings" && (
          <MeetingMinutesView meetings={meetings} onRefresh={onRefreshMeetings} />
        )}
      </div>
    </div>
  );
}
