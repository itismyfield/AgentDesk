import { useCallback, useMemo } from "react";
  NOTIFICATION_TYPE_COLORS,
  type Notification,
import type { Agent, CompanySettings, Department, DispatchedSession, Office } from "../types";
import type { Agent, CompanySettings, Department, DispatchedSession, Office, RoundTableMeeting } from "../types";
import type { Notification } from "./NotificationCenter";
import {
} from "./NotificationCenter";
import AgentManagerView from "./AgentManagerView";
import OfficeManagerView from "./OfficeManagerView";
import SettingsView from "./SettingsView";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceMetricPill,
  SurfaceNotice,
  SurfaceSection,
  SurfaceSegmentButton,
  SurfaceTabCard,
  type SurfaceTone,
} from "./common/SurfacePrimitives";
import { SessionPanel } from "./session-panel/SessionPanel";

type ControlTab = "organization" | "settings";
type OrganizationPane = "agents" | "departments" | "offices" | "dispatch";

interface ControlCenterViewProps {
  controlTab: ControlTab;
  onControlTabChange: (tab: ControlTab) => void;
  organizationPane: OrganizationPane;
  onOrganizationPaneChange: (pane: OrganizationPane) => void;
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

function notificationTone(type: Notification["type"]): SurfaceTone {
  switch (type) {
    case "info":
      return "info";
    case "success":
      return "success";
    case "warning":
      return "warn";
    case "error":
      return "danger";
    default:
      return "neutral";
  }
}

export default function ControlCenterView({
  controlTab,
  onControlTabChange,
  organizationPane,
  onOrganizationPaneChange,
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
  settings,
  onSaveSettings,
  notifications,
  onDismissNotification,
}: ControlCenterViewProps) {
  const t = useCallback((ko: string, en: string) => (isKo ? ko : en), [isKo]);
  const recentNotifications = notifications.slice(0, 3);
  const selectedOfficeName = selectedOfficeId
    ? offices.find((office) => office.id === selectedOfficeId)?.name_ko
      || offices.find((office) => office.id === selectedOfficeId)?.name
      || selectedOfficeId
    : t("전체", "All");

  const sections = useMemo(() => [
    {
      id: "organization" as const,
      labelKo: "조직",
      labelEn: "Organization",
      descriptionKo: "에이전트, 부서, 오피스, 파견 세션을 한 흐름에서 관리합니다.",
      descriptionEn: "Manage agents, departments, offices, and dispatch sessions from one flow.",
      count: agents.length + departments.length + offices.length,
    },
    {
      id: "settings" as const,
      labelKo: "설정",
      labelEn: "Settings",
      descriptionKo: "일반, 런타임, 시스템 설정을 조정합니다.",
      descriptionEn: "Tune general, runtime, and system settings.",
      count: undefined,
    },
  ], [agents.length, departments.length, offices.length]);

  const activeSessionCount = useMemo(
    () => sessions.filter((session) => session.status !== "disconnected").length,
    [sessions],
  );

  const organizationSections = useMemo(() => [
    {
      id: "agents" as const,
      labelKo: "에이전트",
      labelEn: "Agents",
      descriptionKo: "프로필, XP, 소속, provider를 관리합니다.",
      descriptionEn: "Manage profiles, XP, memberships, and providers.",
      count: agents.length,
    },
    {
      id: "departments" as const,
      labelKo: "부서",
      labelEn: "Departments",
      descriptionKo: "순서, 프롬프트, 테마를 정리합니다.",
      descriptionEn: "Adjust order, prompts, and visual themes.",
      count: departments.length,
    },
    {
      id: "offices" as const,
      labelKo: "오피스",
      labelEn: "Offices",
      descriptionKo: "오피스 CRUD와 멤버 구성을 관리합니다.",
      descriptionEn: "Manage office CRUD and memberships.",
      count: offices.length,
    },
    {
      id: "dispatch" as const,
      labelKo: "파견 세션",
      labelEn: "Dispatch Sessions",
      descriptionKo: "감지된 세션을 오피스와 에이전트에 배치합니다.",
      descriptionEn: "Assign detected sessions into offices and agents.",
      count: activeSessionCount,
    },
  ], [activeSessionCount, agents.length, departments.length, offices.length]);

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="border-b" style={{ borderColor: "var(--th-border-subtle)" }}>
        <div className="px-4 py-4">
          <SurfaceSection
            eyebrow={t("운영 표면", "Operations Surface")}
            title={t("컨트롤", "Control")}
            description={t(
              "운영 구조를 섹션 단위로 분리해 junk drawer를 막습니다.",
              "Administrative surfaces are split by section to avoid a junk drawer.",
            )}
            badge={t("섹션 기반 편집", "Section-based editing")}
            className="rounded-[30px] p-4 sm:p-5"
            style={{
              borderColor: "color-mix(in srgb, var(--th-accent-info) 18%, var(--th-border) 82%)",
              background:
                "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, var(--th-accent-info) 4%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
            }}
          >
            <div className="sm:hidden">
              <SurfaceNotice tone="info" compact className="mt-4">
                <p className="break-words leading-relaxed">
                  {t(
                    "Control은 서브섹션별로 나뉘어 있습니다. 대량 편집은 데스크톱 사용을 권장합니다.",
                    "Control is split by section. Use desktop for bulk editing work.",
                  )}
                </p>
              </SurfaceNotice>
            </div>

            <div className="mt-4 flex flex-wrap gap-2">
              <SurfaceMetricPill
                tone="info"
                label={t("선택 오피스", "Selected Office")}
                value={
                  selectedOfficeId
                    ? offices.find((office) => office.id === selectedOfficeId)?.name_ko ||
                      offices.find((office) => office.id === selectedOfficeId)?.name ||
                      selectedOfficeId
                    : t("전체", "All")
                }
                className="flex-1 sm:flex-none"
              />
              <SurfaceMetricPill
                tone="success"
                label={t("활성 세션", "Live Sessions")}
                value={sessions.filter((session) => session.status !== "disconnected").length}
                className="flex-1 sm:flex-none"
              />
              <SurfaceMetricPill
                tone="warn"
                label={t("미해결 회의", "Open Meetings")}
                value={unresolvedMeetings}
                className="flex-1 sm:flex-none"
              />
            </div>

            {recentNotifications.length > 0 && (
              <div className="mt-4 space-y-2">
                {recentNotifications.map((notification) => (
                  <SurfaceNotice
                    key={notification.id}
                    tone={notificationTone(notification.type)}
                    compact
                    action={(
                      <SurfaceActionButton
                        onClick={() => onDismissNotification(notification.id)}
                        tone="neutral"
                        compact
                        className="shrink-0"
                      >
                        {t("닫기", "Dismiss")}
                      </SurfaceActionButton>
                    )}
                  >
                    <p className="break-words leading-relaxed">{notification.message}</p>
                  </SurfaceNotice>
                ))}
              </div>
            )}
          </SurfaceSection>
        </div>

        <div className="flex gap-2 overflow-x-auto px-4 pb-4">
          {sections.map((section) => (
            <SurfaceTabCard
              key={section.id}
              title={isKo ? section.labelKo : section.labelEn}
              description={isKo ? section.descriptionKo : section.descriptionEn}
              count={section.count}
              active={controlTab === section.id}
              tone={section.id === "settings" ? "accent" : section.id === "meetings" ? "info" : "neutral"}
              onClick={() => onControlTabChange(section.id)}
              className="rounded-lg px-3 py-1.5 text-xs font-medium whitespace-nowrap transition-colors"
              style={{
                background: controlTab === section.id ? "rgba(99,102,241,0.16)" : "transparent",
                color: controlTab === section.id ? "#a5b4fc" : "var(--th-text-muted)",
              }}
            >
              {isKo ? section.labelKo : section.labelEn}
              {section.count !== undefined && (
                <span className="ml-1 opacity-60">{section.count}</span>
              )}
            </button>
          ))}
        </div>
      </div>

      <div className="min-h-0 flex-1 overflow-hidden">
        {controlTab === "organization" && (
          <div className="h-full overflow-x-hidden overflow-y-auto">
            <div className="mx-auto max-w-5xl px-4 pt-4 sm:px-6">
              <SurfaceSection
                eyebrow={t("인력 운영", "Staff Ops")}
                title={t("에이전트 운영 표면", "Agent Operations Surface")}
                description={t(
                  "디렉터리 편집과 파견 세션 운영을 같은 흐름 안에서 전환합니다.",
                  "Switch between directory editing and dispatch session operations within one workflow.",
                )}
                badge={agentsPane === "dispatch" ? t("파견 세션", "Dispatch") : t("디렉터리", "Directory")}
                actions={(
                  <>
                    <SurfaceSegmentButton
                      onClick={() => onAgentsPaneChange("directory")}
                      active={agentsPane === "directory"}
                      tone="info"
                    >
                      {t("에이전트 디렉터리", "Agent Directory")}
                    </SurfaceSegmentButton>
                    <SurfaceSegmentButton
                      onClick={() => onAgentsPaneChange("dispatch")}
                      active={agentsPane === "dispatch"}
                      tone="success"
                    >
                      {t("파견 세션", "Dispatch Sessions")}
                    </SurfaceSegmentButton>
                  </>
                )}
                className="rounded-[28px] p-4 sm:p-5"
                style={{
                  borderColor: agentsPane === "dispatch"
                    ? "color-mix(in srgb, var(--th-accent-primary) 20%, var(--th-border) 80%)"
                    : "color-mix(in srgb, var(--th-accent-info) 20%, var(--th-border) 80%)",
                  background: agentsPane === "dispatch"
                    ? "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, var(--th-accent-primary-soft) 5%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)"
                    : "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, var(--th-badge-sky-bg) 5%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
                }}
              >
                <p className="mt-4 text-sm leading-6 break-words" style={{ color: "var(--th-text-muted)" }}>
                  {agentsPaneSummary}
                </p>
              </SurfaceSection>
            </div>

            {organizationPane === "dispatch" && (
              <div className="mx-auto max-w-5xl px-4 pb-40 pt-4 sm:px-6">
                <SurfaceCard
                  className="rounded-3xl p-4 sm:p-5"
                  style={{
                    borderColor: "color-mix(in srgb, var(--th-border) 74%, transparent)",
                    background: "color-mix(in srgb, var(--th-card-bg) 94%, transparent)",
                  }}
                >
                  <SessionPanel
                    sessions={sessions}
                    departments={departments}
                    agents={agents}
                    onAssign={onAssign}
                  />
                </SurfaceCard>
              </div>
            )}

            {organizationPane === "agents" && (
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
                title={t("조직 · 에이전트", "Organization · Agents")}
                subtitle={t("프로필, XP, 스킬, provider, 오피스 소속을 관리합니다.", "Manage profiles, XP, skills, providers, and office membership.")}
              />
            )}

            {organizationPane === "departments" && (
              <AgentManagerView
                agents={agents}
                departments={departments}
                language={language}
                officeId={officeId}
                onAgentsChange={onAgentsChange}
                onDepartmentsChange={onDepartmentsChange}
                activeTab="departments"
                showTabBar={false}
                title={t("조직 · 부서", "Organization · Departments")}
                subtitle={t("부서 순서, 프롬프트, 테마를 관리합니다.", "Manage department order, prompts, and visual themes.")}
              />
            )}

            {organizationPane === "offices" && (
              <OfficeManagerView
                offices={offices}
                allAgents={allAgents}
                selectedOfficeId={selectedOfficeId}
                isKo={isKo}
                onChanged={onOfficesChange}
              />
            )}
          </div>
        )}

        {controlTab === "settings" && (
          <SettingsView settings={settings} onSave={onSaveSettings} isKo={isKo} />
        )}
      </div>
    </div>
  );
}
