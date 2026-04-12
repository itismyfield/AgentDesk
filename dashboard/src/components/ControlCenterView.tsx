import { useCallback, useMemo } from "react";
import type { Agent, CompanySettings, Department, DispatchedSession, Office } from "../types";
import {
  NOTIFICATION_TYPE_COLORS,
  type Notification,
} from "./NotificationCenter";
import AgentManagerView from "./AgentManagerView";
import OfficeManagerView from "./OfficeManagerView";
import SettingsView from "./SettingsView";
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
            {t("설정은 조직 관리와 시스템 설정으로 분리되어 있습니다. 대량 편집은 데스크톱 사용을 권장합니다.", "Settings are split between organization work and system settings. Use desktop for bulk editing work.")}
          </div>
        </div>

        <div className="px-4 py-4">
          <div className="flex flex-wrap items-start justify-between gap-3">
            <div className="min-w-0">
              <h2 className="text-lg font-semibold" style={{ color: "var(--th-text-heading)" }}>
                {t("설정", "Settings")}
              </h2>
              <p className="text-sm" style={{ color: "var(--th-text-muted)" }}>
                {t("조직 편집과 런타임 설정을 분리해 운영 표면을 단순화합니다.", "Separate organization editing from runtime settings so the admin surface stays coherent.")}
              </p>
            </div>
            <div className="flex flex-wrap gap-2 text-xs">
              <span className="rounded-full px-2.5 py-1" style={{ background: "rgba(59,130,246,0.12)", color: "#60a5fa" }}>
                {t("선택 오피스", "Selected Office")}: {selectedOfficeName}
              </span>
              <span className="rounded-full px-2.5 py-1" style={{ background: "rgba(16,185,129,0.12)", color: "#34d399" }}>
                {t("활성 세션", "Live Sessions")}: {activeSessionCount}
              </span>
              <span className="rounded-full px-2.5 py-1" style={{ background: "rgba(245,158,11,0.12)", color: "#f59e0b" }}>
                {t("조직 자산", "Org Surface")}: {agents.length}/{departments.length}/{offices.length}
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
        {controlTab === "organization" && (
          <div className="h-full overflow-x-hidden overflow-y-auto">
            <div className="mx-auto max-w-6xl px-4 pt-4 sm:px-6">
              <div className="grid gap-2 sm:grid-cols-2 xl:grid-cols-4">
                {organizationSections.map((section) => (
                  <button
                    key={section.id}
                    onClick={() => onOrganizationPaneChange(section.id)}
                    className="rounded-2xl border px-4 py-3 text-left transition-colors"
                    style={{
                      borderColor: organizationPane === section.id ? "rgba(99,102,241,0.44)" : "rgba(148,163,184,0.18)",
                      background: organizationPane === section.id ? "rgba(99,102,241,0.12)" : "var(--th-surface)",
                    }}
                  >
                    <div className="flex items-center justify-between gap-2">
                      <div className="text-sm font-semibold" style={{ color: organizationPane === section.id ? "#a5b4fc" : "var(--th-text-heading)" }}>
                        {isKo ? section.labelKo : section.labelEn}
                      </div>
                      <span className="rounded-full px-2 py-0.5 text-[10px]" style={{ background: "rgba(148,163,184,0.12)", color: "var(--th-text-muted)" }}>
                        {section.count}
                      </span>
                    </div>
                    <div className="mt-1 text-xs leading-relaxed" style={{ color: "var(--th-text-muted)" }}>
                      {isKo ? section.descriptionKo : section.descriptionEn}
                    </div>
                  </button>
                ))}
              </div>
            </div>

            {organizationPane === "dispatch" && (
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
