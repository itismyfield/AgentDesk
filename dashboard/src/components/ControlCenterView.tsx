import { useCallback, useMemo } from "react";
import type { Agent, CompanySettings, Department, DispatchedSession, Office } from "../types";
import {
  NOTIFICATION_TYPE_COLORS,
  type Notification,
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
  const organizationPaneSummary = organizationPane === "dispatch"
    ? t(
      "실시간 파견 세션을 확인하고, 부서 연결과 상태 점검을 한 흐름에서 처리합니다.",
      "Review live dispatched sessions, department links, and runtime status from one flow.",
    )
    : t(
      "에이전트 프로필, XP, 스킬, provider, 오피스 소속을 한 곳에서 관리합니다.",
      "Manage agent profiles, XP, skills, providers, and office membership from one surface.",
    );

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
  const activeOrganizationSection = organizationSections.find((section) => section.id === organizationPane);
  const selectedOfficeLabel = selectedOfficeId
    ? offices.find((office) => office.id === selectedOfficeId)?.name_ko ||
      offices.find((office) => office.id === selectedOfficeId)?.name ||
      selectedOfficeId
    : t("전체", "All");

  const controlHeader = (
    <div className="border-b" style={{ borderColor: "var(--th-border-subtle)" }}>
      <div className="px-4 py-3 sm:px-6 sm:py-3">
        <SurfaceSection
          eyebrow={t("운영 표면", "Operations Surface")}
          title={t("설정", "Settings")}
          description={t(
            "조직 관리와 런타임 설정을 섹션 단위로 분리해 한 화면에서 정리합니다.",
            "Keep organization management and runtime settings separated into clear sections.",
          )}
          badge={t("섹션 기반 편집", "Section-based editing")}
          className="rounded-[28px] p-3 sm:p-4"
          style={{
            borderColor: "color-mix(in srgb, var(--th-accent-info) 18%, var(--th-border) 82%)",
            background:
              "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, var(--th-accent-info) 4%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
          }}
        >
          <div className="sm:hidden">
            <SurfaceNotice tone="info" compact className="mt-3">
              <p className="break-words leading-relaxed">
                {t(
                  "설정은 조직과 일반 설정 섹션으로 나뉘어 있습니다. 대량 편집은 데스크톱 사용을 권장합니다.",
                  "Settings are split into organization and general sections. Use desktop for bulk editing work.",
                )}
              </p>
            </SurfaceNotice>
          </div>

          <div className="mt-3 flex flex-wrap gap-2">
            <SurfaceMetricPill
              tone="info"
              label={t("선택 오피스", "Selected Office")}
              value={selectedOfficeLabel}
              className="flex-1 sm:flex-none"
            />
            <SurfaceMetricPill
              tone="success"
              label={t("활성 세션", "Live Sessions")}
              value={activeSessionCount}
              className="flex-1 sm:flex-none"
            />
          </div>

          {recentNotifications.length > 0 && (
            <div className="mt-3 space-y-2">
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

      <div className="grid grid-cols-1 gap-2 px-4 pb-3 sm:px-6 sm:flex sm:overflow-x-auto">
        {sections.map((section) => (
          <SurfaceTabCard
            key={section.id}
            title={isKo ? section.labelKo : section.labelEn}
            description={isKo ? section.descriptionKo : section.descriptionEn}
            count={section.count}
            active={controlTab === section.id}
            tone={section.id === "settings" ? "accent" : "neutral"}
            onClick={() => onControlTabChange(section.id)}
          />
        ))}
      </div>
    </div>
  );

  return (
    <div className="flex min-h-full flex-col sm:h-full sm:min-h-0">
      <div className="sm:min-h-0 sm:flex-1">
        {controlTab === "organization" && (
          <div
            className="flex min-h-full flex-col overflow-x-hidden overflow-y-auto sm:h-full"
            style={{
              WebkitOverflowScrolling: "touch",
              touchAction: "pan-y",
            }}
          >
            {controlHeader}
            <div className="mx-auto w-full max-w-5xl min-w-0 px-4 pt-4 sm:px-6">
              <SurfaceSection
                eyebrow={t("인력 운영", "Staff Ops")}
                title={t("조직 운영 표면", "Organization Operations Surface")}
                description={t(
                  "에이전트, 부서, 오피스, 파견 세션을 같은 조직 표면 안에서 전환합니다.",
                  "Switch between agents, departments, offices, and dispatch sessions within one organization surface.",
                )}
                badge={activeOrganizationSection ? (isKo ? activeOrganizationSection.labelKo : activeOrganizationSection.labelEn) : undefined}
                actions={(
                  <>
                    {organizationSections.map((section) => (
                      <SurfaceSegmentButton
                        key={section.id}
                        onClick={() => onOrganizationPaneChange(section.id)}
                        active={organizationPane === section.id}
                        tone={section.id === "dispatch" ? "success" : "info"}
                      >
                        {isKo ? section.labelKo : section.labelEn}
                      </SurfaceSegmentButton>
                    ))}
                  </>
                )}
                className="rounded-[28px] p-4 sm:p-5"
                style={{
                  borderColor: organizationPane === "dispatch"
                    ? "color-mix(in srgb, var(--th-accent-primary) 20%, var(--th-border) 80%)"
                    : "color-mix(in srgb, var(--th-accent-info) 20%, var(--th-border) 80%)",
                  background: organizationPane === "dispatch"
                    ? "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, var(--th-accent-primary-soft) 5%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)"
                    : "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, var(--th-badge-sky-bg) 5%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
                }}
              >
                <p className="mt-4 text-sm leading-6 break-words" style={{ color: "var(--th-text-muted)" }}>
                  {organizationPaneSummary}
                </p>
              </SurfaceSection>
            </div>

            {organizationPane === "dispatch" && (
              <div className="mx-auto w-full max-w-5xl min-w-0 px-4 pb-40 pt-4 sm:px-6">
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
                scrollable={false}
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
          <div
            className="flex min-h-full flex-col overflow-x-hidden overflow-y-auto sm:h-full"
            style={{
              WebkitOverflowScrolling: "touch",
              touchAction: "pan-y",
            }}
          >
            {controlHeader}
            <SettingsView settings={settings} onSave={onSaveSettings} isKo={isKo} />
          </div>
        )}
      </div>
    </div>
  );
}
