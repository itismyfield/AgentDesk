import { useState, useEffect, useCallback, lazy, Suspense, useMemo } from "react";
import type {
  Agent,
  AuditLogEntry,
  CompanySettings,
  DashboardStats,
  Department,
  DispatchedSession,
  KanbanCard,
  Office,
  RoundTableMeeting,
  TaskDispatch,
  WSEvent,
} from "./types";
import { DEFAULT_SETTINGS } from "./types";
import * as api from "./api/client";
import { onApiError } from "./api/client";
import { KanbanProvider, useKanban } from "./contexts/KanbanContext";
import { OfficeProvider, useOffice } from "./contexts/OfficeContext";
import { SettingsProvider, useSettings } from "./contexts/SettingsContext";

const OfficeView = lazy(() => import("./components/OfficeView"));
const DashboardPageView = lazy(() => import("./components/DashboardPageView"));
const KanbanTab = lazy(() => import("./components/agent-manager/KanbanTab"));
const ControlCenterView = lazy(() => import("./components/ControlCenterView"));
import OfficeSelectorBar from "./components/OfficeSelectorBar";
const AgentInfoCard = lazy(() => import("./components/agent-manager/AgentInfoCard"));
import { useSpriteMap } from "./components/AgentAvatar";
import { useI18n } from "./i18n";
import {
  ToastOverlay,
  type Notification,
  useNotifications,
} from "./components/NotificationCenter";
import { useDashboardSocket } from "./app/useDashboardSocket";
import {
  Activity,
  Building2,
  KanbanSquare,
  SlidersHorizontal,
  Wifi,
  WifiOff,
} from "lucide-react";
const CommandPalette = lazy(() => import("./components/CommandPalette"));

type ViewMode = "office" | "pulse" | "kanban" | "more";
type ControlTab = "agents" | "departments" | "offices" | "settings" | "meetings";
type AgentsPane = "directory" | "dispatch";
type KanbanPulseFocus = "review" | "blocked" | "requested" | "stalled";

interface ShellRoute {
  id: ViewMode;
  labelKo: string;
  labelEn: string;
  shortcutKey: string;
  loadingKo: string;
  loadingEn: string;
}

interface PaletteRoute {
  id: string;
  labelKo: string;
  labelEn: string;
  icon: string;
}

const VIEW_ROUTES: ShellRoute[] = [
  { id: "office", labelKo: "오피스", labelEn: "Office", shortcutKey: "o", loadingKo: "오피스 로딩 중...", loadingEn: "Loading Office..." },
  { id: "pulse", labelKo: "펄스", labelEn: "Pulse", shortcutKey: "p", loadingKo: "펄스 로딩 중...", loadingEn: "Loading Pulse..." },
  { id: "kanban", labelKo: "칸반", labelEn: "Kanban", shortcutKey: "b", loadingKo: "칸반 로딩 중...", loadingEn: "Loading Kanban..." },
  { id: "more", labelKo: "컨트롤", labelEn: "Control", shortcutKey: "m", loadingKo: "컨트롤 로딩 중...", loadingEn: "Loading Control..." },
];

const PALETTE_ROUTES: PaletteRoute[] = [
  { id: "office", labelKo: "오피스", labelEn: "Office", icon: "🏢" },
  { id: "pulse", labelKo: "펄스", labelEn: "Pulse", icon: "📊" },
  { id: "kanban", labelKo: "칸반", labelEn: "Kanban", icon: "📋" },
  { id: "more", labelKo: "컨트롤", labelEn: "Control", icon: "🎛️" },
  { id: "control_agents", labelKo: "에이전트", labelEn: "Agents", icon: "👥" },
  { id: "control_departments", labelKo: "부서", labelEn: "Departments", icon: "🏢" },
  { id: "control_offices", labelKo: "오피스 관리", labelEn: "Offices", icon: "🏬" },
  { id: "control_meetings", labelKo: "회의 기록", labelEn: "Meeting Records", icon: "📝" },
  { id: "control_settings", labelKo: "설정", labelEn: "Settings", icon: "⚙️" },
];

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

interface BootstrapData {
  offices: Office[];
  agents: Agent[];
  allAgents: Agent[];
  departments: Department[];
  allDepartments: Department[];
  sessions: DispatchedSession[];
  stats: DashboardStats | null;
  settings: CompanySettings;
  roundTableMeetings: RoundTableMeeting[];
  auditLogs: AuditLogEntry[];
  kanbanCards: KanbanCard[];
  taskDispatches: TaskDispatch[];
  selectedOfficeId: string | null;
}

export default function App() {
  const [data, setData] = useState<BootstrapData | null>(null);
  const [bootstrapError, setBootstrapError] = useState<string | null>(null);
  const { notifications, pushNotification, dismissNotification } = useNotifications();

  // Wire up API error → toast notifications (throttled: max 1 per 3s per endpoint)
  useEffect(() => {
    const lastFired = new Map<string, number>();
    onApiError((url, error) => {
      const now = Date.now();
      const last = lastFired.get(url) ?? 0;
      if (now - last < 3000) return;
      lastFired.set(url, now);
      pushNotification(`API error: ${error.message}`, "error");
    });
    return () => onApiError(null);
  }, [pushNotification]);

  useEffect(() => {
    api.onApiError((url, error) => {
      const apiPath = url.replace(/^\/api\//, "");
      pushNotification(`API error: ${apiPath} - ${error.message}`, "error");
    });
    return () => api.onApiError(null);
  }, [pushNotification]);

  useEffect(() => {
    (async () => {
      const partial: string[] = [];
      try {
        await api.getSession();
        const offices = await api.getOffices();
        const defaultOfficeId = offices.length > 0 ? offices[0].id : undefined;
        const [allAgents, agents, allDepartments, departments, sessions, stats, settings, meetings, logs, cards, dispatches] =
          await Promise.all([
            api.getAgents(),
            api.getAgents(defaultOfficeId),
            api.getDepartments(),
            api.getDepartments(defaultOfficeId),
            api.getDispatchedSessions(true),
            api.getStats(defaultOfficeId),
            api.getSettings(),
            api.getRoundTableMeetings().catch(() => [] as RoundTableMeeting[]),
            api.getAuditLogs(12).catch(() => [] as AuditLogEntry[]),
            api.getKanbanCards().catch(() => [] as KanbanCard[]),
            api.getTaskDispatches({ limit: 200 }).catch(() => [] as TaskDispatch[]),
          ]);
        const resolvedSettings = settings.companyName
          ? ({ ...DEFAULT_SETTINGS, ...settings } as CompanySettings)
          : DEFAULT_SETTINGS;
        setData({
          offices,
          agents,
          allAgents,
          departments,
          allDepartments,
          sessions,
          stats,
          settings: resolvedSettings,
          roundTableMeetings: meetings,
          auditLogs: logs,
          kanbanCards: cards,
          taskDispatches: dispatches,
          selectedOfficeId: defaultOfficeId ?? null,
        });
      } catch (error) {
        console.error("Bootstrap failed:", error);
        setData({
          offices: [],
          agents: [],
          allAgents: [],
          departments: [],
          allDepartments: [],
          sessions: [],
          stats: null,
          settings: DEFAULT_SETTINGS,
          roundTableMeetings: [],
          auditLogs: [],
          kanbanCards: [],
          taskDispatches: [],
          selectedOfficeId: null,
        });
      }
    })();
  }, [pushNotification]);

  const handleWsEvent = useCallback(
    (event: WSEvent) => {
      switch (event.type) {
        case "kanban_card_created": {
          const card = event.payload as KanbanCard;
          if (card.status === "requested") {
            pushNotification(`칸반 요청 발사: ${card.title}`, "info");
          }
          break;
        }
        case "kanban_card_updated":
          break;
      }
    },
    [pushNotification],
  );

  const { wsConnected } = useDashboardSocket(handleWsEvent);
  const { t } = useI18n();

  if (!data) {
    return (
      <div className="flex h-screen items-center justify-center bg-gray-900 text-gray-400">
        <div className="text-center">
          <div className="mb-4 text-4xl">🐾</div>
          <div>{t({ ko: "AgentDesk 대시보드 로딩 중...", en: "Loading AgentDesk Dashboard..." })}</div>
        </div>
      </div>
    );
  }

  return (
    <OfficeProvider
      initialOffices={data.offices}
      initialAgents={data.agents}
      initialAllAgents={data.allAgents}
      initialDepartments={data.departments}
      initialAllDepartments={data.allDepartments}
      initialSessions={data.sessions}
      initialRoundTableMeetings={data.roundTableMeetings}
      initialAuditLogs={data.auditLogs}
      initialSelectedOfficeId={data.selectedOfficeId}
      pushNotification={pushNotification}
    >
      <SettingsProvider initialSettings={data.settings} initialStats={data.stats}>
        <KanbanProvider initialCards={data.kanbanCards} initialDispatches={data.taskDispatches}>
          <AppShell
            wsConnected={wsConnected}
            notifications={notifications}
            dismissNotification={dismissNotification}
          />
        </KanbanProvider>
      </SettingsProvider>
    </OfficeProvider>
  );
}

interface AppShellProps {
  wsConnected: boolean;
  notifications: Notification[];
  dismissNotification: (id: string) => void;
}

function AppShell({ wsConnected, notifications, dismissNotification }: AppShellProps) {
  const [view, setView] = useState<ViewMode>("office");
  const [controlTab, setControlTab] = useState<ControlTab>("agents");
  const [agentsPane, setAgentsPane] = useState<AgentsPane>("directory");
  const [kanbanPulseFocus, setKanbanPulseFocus] = useState<KanbanPulseFocus | null>(null);
  const [officeInfoAgent, setOfficeInfoAgent] = useState<Agent | null>(null);
  const [showCmdPalette, setShowCmdPalette] = useState(false);
  const [showShortcutHelp, setShowShortcutHelp] = useState(false);

  const { settings, setSettings, stats, refreshStats, refreshingStats, isKo, locale, tr } = useSettings();
  const {
    offices,
    selectedOfficeId,
    setSelectedOfficeId,
    agents,
    allAgents,
    departments,
    allDepartments,
    setSessions,
    roundTableMeetings,
    setRoundTableMeetings,
    auditLogs,
    visibleDispatchedSessions,
    subAgents,
    agentsWithDispatched,
    refreshOffices,
    refreshAgents,
    refreshAllAgents,
    refreshDepartments,
    refreshAllDepartments,
    refreshAuditLogs,
    refreshing,
    datasetStates,
  } = useOffice();
  const { kanbanCards, taskDispatches, upsertKanbanCard, setKanbanCards } = useKanban();

  const spriteMap = useSpriteMap(agents);
  const unreadCount = notifications.filter((notification) => Date.now() - notification.ts < 60_000).length;
  const unresolvedMeetingsCount = roundTableMeetings.filter(hasUnresolvedMeetingIssues).length;

  const resolveTheme = useCallback(() => {
    if (settings.theme !== "auto") return settings.theme;
    return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  }, [settings.theme]);

  const viewFallbackLabel = useMemo(
    () =>
      Object.fromEntries(
        VIEW_ROUTES.map((route) => [route.id, isKo ? route.loadingKo : route.loadingEn]),
      ) as Record<ViewMode, string>,
    [isKo],
  );

  const moreBadge = unresolvedMeetingsCount > 0 ? unresolvedMeetingsCount : unreadCount || undefined;
  const moreBadgeColor = unresolvedMeetingsCount > 0 ? "bg-amber-500" : unreadCount > 0 ? "bg-red-500" : undefined;

  const navItems: Array<{ id: ViewMode; icon: React.ReactNode; label: string; badge?: number; badgeColor?: string }> = [
    { id: "office", icon: <Building2 size={20} />, label: isKo ? "오피스" : "Office" },
    { id: "pulse", icon: <Activity size={20} />, label: isKo ? "펄스" : "Pulse" },
    { id: "kanban", icon: <KanbanSquare size={20} />, label: isKo ? "칸반" : "Kanban" },
    { id: "more", icon: <SlidersHorizontal size={20} />, label: isKo ? "컨트롤" : "Control", badge: moreBadge, badgeColor: moreBadgeColor },
  ];

  const handleNavigate = useCallback(
    (nextView: ViewMode) => {
      setView(nextView);
      if (nextView === "pulse") refreshStats();
    },
    [refreshStats],
  );

  const handlePaletteNavigate = useCallback(
    (routeId: string) => {
      if (routeId === "office" || routeId === "pulse" || routeId === "kanban" || routeId === "more") {
        handleNavigate(routeId);
        return;
      }

      setView("more");
      if (routeId === "control_departments") {
        setControlTab("departments");
      } else if (routeId === "control_offices") {
        setControlTab("offices");
      } else if (routeId === "control_settings") {
        setControlTab("settings");
      } else if (routeId === "control_meetings") {
        setControlTab("meetings");
      } else {
        setControlTab("agents");
        setAgentsPane("directory");
      }
    },
    [handleNavigate],
  );

  const openPulseKanbanSignal = useCallback((signal: KanbanPulseFocus) => {
    setKanbanPulseFocus(signal);
    setView("kanban");
  }, []);

  const openDispatchSessions = useCallback(() => {
    setControlTab("agents");
    setAgentsPane("dispatch");
    setView("more");
  }, []);

  const openMeetingsView = useCallback(() => {
    setControlTab("meetings");
    setView("more");
  }, []);

  const openSettingsView = useCallback(() => {
    setControlTab("settings");
    setView("more");
  }, []);

  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      const tag = (event.target as HTMLElement | null)?.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return;

      if ((event.metaKey || event.ctrlKey) && event.key === "k") {
        event.preventDefault();
        setShowCmdPalette((prev) => !prev);
        return;
      }

      if (event.key === "?" && !event.metaKey && !event.ctrlKey && !event.altKey) {
        event.preventDefault();
        setShowShortcutHelp((prev) => !prev);
        return;
      }

      if (event.altKey && !event.metaKey && !event.ctrlKey) {
        const route = VIEW_ROUTES.find((item) => item.shortcutKey === event.key.toLowerCase());
        if (!route) return;
        event.preventDefault();
        handleNavigate(route.id);
      }
    };

    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [handleNavigate]);

  const handleOfficeChanged = useCallback(() => {
    refreshOffices();
    refreshAgents();
    refreshAllAgents();
    refreshDepartments();
    refreshAllDepartments();
    refreshAuditLogs();
  }, [refreshOffices, refreshAgents, refreshAllAgents, refreshDepartments, refreshAllDepartments, refreshAuditLogs]);

  const showOfficeSelector =
    offices.length > 0 && (view === "office" || (view === "more" && controlTab !== "settings" && controlTab !== "meetings"));

  return (
    <div className="flex fixed inset-0 bg-gray-900">
      <nav className="hidden w-[4.5rem] flex-col items-center gap-1 border-r border-gray-800 bg-gray-950 py-4 sm:flex">
        <div className="mb-4 text-2xl">🐾</div>
        {navItems.map((item) => (
          <NavBtn
            key={item.id}
            icon={item.icon}
            active={view === item.id}
            badge={item.badge}
            badgeColor={item.badgeColor}
            onClick={() => handleNavigate(item.id)}
            label={item.label}
          />
        ))}
        <div className="flex-1" />
        <div
          className="flex h-10 w-10 items-center justify-center rounded-lg"
          title={wsConnected ? (isKo ? "서버 연결됨" : "Server connected") : (isKo ? "서버 연결 끊김" : "Server disconnected")}
        >
          {wsConnected ? <Wifi size={16} className="text-emerald-500" /> : <WifiOff size={16} className="animate-pulse text-red-400" />}
        </div>
      </nav>

      <div className="flex min-w-0 flex-1 flex-col overflow-hidden">
        {showOfficeSelector && (
          <OfficeSelectorBar
            offices={offices}
            selectedOfficeId={selectedOfficeId}
            onSelectOffice={setSelectedOfficeId}
            onManageOffices={() => {
              setView("more");
              setControlTab("offices");
            }}
            isKo={isKo}
          />
        )}

        <main className="mb-14 flex min-h-0 flex-1 flex-col overflow-hidden sm:mb-0">
          <Suspense fallback={<ViewSkeleton label={viewFallbackLabel[view]} />}>
            {view === "office" && (
              <OfficeView
                agents={agentsWithDispatched}
                departments={departments}
                language={settings.language}
                theme={resolveTheme()}
                subAgents={subAgents}
                notifications={notifications}
                auditLogs={auditLogs}
                activeMeeting={roundTableMeetings.find((meeting) => meeting.status === "in_progress") ?? null}
                kanbanCards={kanbanCards}
                onNavigateToKanban={() => handleNavigate("kanban")}
                onSelectAgent={(agent) => setOfficeInfoAgent(agent)}
                onSelectDepartment={() => {
                  setView("more");
                  setControlTab("departments");
                }}
                customDeptThemes={settings.roomThemes}
              />
            )}

            {view === "pulse" && (
              <DashboardPageView
                stats={stats}
                agents={agents}
                sessions={visibleDispatchedSessions}
                meetings={roundTableMeetings}
                settings={settings}
                onSelectAgent={(agent) => setOfficeInfoAgent(agent)}
                onOpenKanbanSignal={openPulseKanbanSignal}
                onOpenDispatchSessions={openDispatchSessions}
                onOpenMeetings={openMeetingsView}
                onOpenSettings={openSettingsView}
              />
            )}

            {view === "kanban" && (
              <div className="h-full overflow-auto p-4 pb-40 sm:p-6">
                <KanbanTab
                  tr={(ko: string, en: string) => (settings.language === "ko" ? ko : en)}
                  locale={settings.language}
                  cards={kanbanCards}
                  dispatches={taskDispatches}
                  agents={allAgents}
                  departments={allDepartments}
                  onAssignIssue={async (payload: {
                    github_repo: string;
                    github_issue_number: number;
                    github_issue_url?: string | null;
                    title: string;
                    description?: string | null;
                    assignee_agent_id: string;
                  }) => {
                    const assigned = await api.assignKanbanIssue(payload);
                    upsertKanbanCard(assigned);
                  }}
                  onUpdateCard={async (id: string, patch: Partial<KanbanCard> & { before_card_id?: string | null }) => {
                    const updated = await api.updateKanbanCard(id, patch);
                    upsertKanbanCard(updated);
                  }}
                  onRetryCard={async (id: string, payload?: { assignee_agent_id?: string | null; request_now?: boolean }) => {
                    const updated = await api.retryKanbanCard(id, payload);
                    upsertKanbanCard(updated);
                  }}
                  onRedispatchCard={async (id: string, payload?: { reason?: string | null }) => {
                    const updated = await api.redispatchKanbanCard(id, payload);
                    upsertKanbanCard(updated);
                  }}
                onDeleteCard={async (id: string) => {
                  await api.deleteKanbanCard(id);
                  setKanbanCards((prev) => prev.filter((card) => card.id !== id));
                }}
                onPatchDeferDod={async (id, payload) => {
                  const updated = await api.patchKanbanDeferDod(id, payload);
                  upsertKanbanCard(updated);
                }}
                externalStatusFocus={kanbanPulseFocus}
                onClearPulseFocus={() => setKanbanPulseFocus(null)}
              />
            </div>
            )}

            {view === "more" && (
              <ControlCenterView
                controlTab={controlTab}
                onControlTabChange={setControlTab}
                agentsPane={agentsPane}
                onAgentsPaneChange={setAgentsPane}
                isKo={isKo}
                language={settings.language}
                officeId={selectedOfficeId}
                offices={offices}
                selectedOfficeId={selectedOfficeId}
                allAgents={allAgents}
                agents={agents}
                departments={departments}
                sessions={visibleDispatchedSessions}
                onAssign={async (id, patch) => {
                  const updated = await api.assignDispatchedSession(id, patch);
                  setSessions((prev) => prev.map((session) => (session.id === updated.id ? updated : session)));
                }}
                onAgentsChange={() => {
                  refreshAgents();
                  refreshAllAgents();
                  refreshOffices();
                }}
                onDepartmentsChange={() => {
                  refreshDepartments();
                  refreshAllDepartments();
                  refreshOffices();
                }}
                onOfficesChange={handleOfficeChanged}
                meetings={roundTableMeetings}
                onRefreshMeetings={() => api.getRoundTableMeetings().then(setRoundTableMeetings).catch(() => {})}
                settings={settings}
                onSaveSettings={async (patch) => {
                  await api.saveSettings(patch);
                  setSettings((prev) => ({ ...prev, ...patch } as CompanySettings));
                  refreshAuditLogs();
                }}
                notifications={notifications}
                onDismissNotification={dismissNotification}
              />
            )}
          </Suspense>
        </main>
      </div>

      {!wsConnected && (
        <div className="fixed left-0 right-0 top-0 z-[90] flex items-center justify-center gap-2 border-b border-red-500/20 bg-red-500/15 px-3 py-1.5 text-center text-xs text-red-400 sm:left-[4.5rem]">
          <WifiOff size={12} className="animate-pulse" />
          <span>{isKo ? "서버 연결 끊김 — 재연결 시도 중..." : "Server disconnected — reconnecting..."}</span>
        </div>
      )}

      <nav className="fixed bottom-0 left-0 right-0 z-50 flex h-14 items-center justify-around border-t border-gray-800 bg-gray-950 sm:hidden">
        {navItems.map((item) => (
          <button
            key={item.id}
            onClick={() => handleNavigate(item.id)}
            className={`relative flex h-full flex-1 flex-col items-center justify-center text-[10px] ${
              view === item.id ? "text-indigo-400" : "text-gray-500"
            }`}
          >
            {item.icon}
            <span className="mt-0.5">{item.label}</span>
            {item.badge !== undefined && item.badge > 0 && (
              <span className={`absolute right-1/4 top-1 flex h-3.5 w-3.5 items-center justify-center rounded-full text-[8px] text-white ${item.badgeColor || "bg-emerald-500"}`}>
                {item.badge > 9 ? "9+" : item.badge}
              </span>
            )}
          </button>
        ))}
      </nav>

      <Suspense fallback={null}>
        {officeInfoAgent && (
          <AgentInfoCard
            agent={officeInfoAgent}
            spriteMap={spriteMap}
            isKo={isKo}
            locale={locale}
            tr={tr}
            departments={departments}
            onClose={() => setOfficeInfoAgent(null)}
            onAgentUpdated={() => {
              refreshAgents();
              refreshAllAgents();
              refreshOffices();
              refreshAuditLogs();
            }}
          />
        )}
      </Suspense>

      <Suspense fallback={null}>
        {showCmdPalette && (
          <CommandPalette
            agents={allAgents}
            departments={departments}
            isKo={isKo}
            onSelectAgent={(agent) => setOfficeInfoAgent(agent)}
            onNavigate={handlePaletteNavigate}
            onClose={() => setShowCmdPalette(false)}
            routes={PALETTE_ROUTES}
            departmentRouteId="control_departments"
          />
        )}
      </Suspense>

      <ToastOverlay notifications={notifications} onDismiss={dismissNotification} />

      {showShortcutHelp && (
        <div className="fixed inset-0 z-[100] flex items-center justify-center" onClick={() => setShowShortcutHelp(false)}>
          <div className="fixed inset-0 bg-black/50 backdrop-blur-sm" />
          <div
            role="dialog"
            aria-modal="true"
            aria-label="Keyboard shortcuts"
            className="relative mx-4 w-full max-w-md space-y-4 rounded-2xl border border-[var(--th-border)] bg-[var(--th-surface)] p-6 shadow-2xl"
            onClick={(event) => event.stopPropagation()}
          >
            <div className="flex items-center justify-between">
              <h3 className="text-lg font-bold" style={{ color: "var(--th-text-heading)" }}>
                {isKo ? "키보드 단축키" : "Keyboard Shortcuts"}
              </h3>
              <button
                onClick={() => setShowShortcutHelp(false)}
                className="flex h-11 w-11 items-center justify-center rounded-lg text-[var(--th-text-muted)] hover:bg-white/5"
                aria-label="Close"
              >
                ✕
              </button>
            </div>
            <div className="space-y-2 text-sm">
              <div className="flex justify-between" style={{ color: "var(--th-text-muted)" }}>
                <span>{isKo ? "명령 팔레트" : "Command Palette"}</span>
                <kbd className="rounded bg-black/10 px-2 py-0.5 text-xs">⌘K</kbd>
              </div>
              <div className="flex justify-between" style={{ color: "var(--th-text-muted)" }}>
                <span>{isKo ? "이 도움말" : "This help"}</span>
                <kbd className="rounded bg-black/10 px-2 py-0.5 text-xs">?</kbd>
              </div>
              <hr style={{ borderColor: "var(--th-border)" }} />
              <div className="text-xs font-semibold uppercase" style={{ color: "var(--th-text-muted)" }}>
                {isKo ? "뷰 전환" : "View Navigation"}
              </div>
              {VIEW_ROUTES.map((route) => (
                <div key={route.id} className="flex justify-between" style={{ color: "var(--th-text-muted)" }}>
                  <span>{isKo ? route.labelKo : route.labelEn}</span>
                  <kbd className="rounded bg-black/10 px-2 py-0.5 text-xs">Alt+{route.shortcutKey.toUpperCase()}</kbd>
                </div>
              ))}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

function NavBtn({
  icon,
  active,
  badge,
  badgeColor,
  onClick,
  label,
}: {
  icon: React.ReactNode;
  active: boolean;
  badge?: number;
  badgeColor?: string;
  onClick: () => void;
  label: string;
}) {
  return (
    <button
      onClick={onClick}
      title={label}
      className={`relative flex w-14 flex-col items-center justify-center gap-0.5 rounded-lg py-1.5 transition-colors ${
        active ? "bg-indigo-600 text-white" : "text-gray-500 hover:bg-gray-800 hover:text-gray-300"
      }`}
    >
      {icon}
      <span className="text-xs leading-tight">{label}</span>
      {badge !== undefined && badge > 0 && (
        <span className={`absolute -right-0.5 -top-1 flex h-4 w-4 items-center justify-center rounded-full text-[10px] text-white ${badgeColor || "bg-emerald-500"}`}>
          {badge > 9 ? "9+" : badge}
        </span>
      )}
    </button>
  );
}

function ViewSkeleton({ label }: { label: string }) {
  return (
    <div className="flex h-full items-center justify-center text-gray-500">
      {label}
    </div>
  );
}
