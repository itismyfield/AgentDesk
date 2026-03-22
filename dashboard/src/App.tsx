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
import { KanbanProvider, useKanban } from "./contexts/KanbanContext";
import { OfficeProvider, useOffice } from "./contexts/OfficeContext";
import { SettingsProvider, useSettings } from "./contexts/SettingsContext";

const OfficeView = lazy(() => import("./components/OfficeView"));
const DashboardPageView = lazy(() => import("./components/DashboardPageView"));
const AgentManagerView = lazy(() => import("./components/AgentManagerView"));
const MeetingMinutesView = lazy(() => import("./components/MeetingMinutesView"));
const SkillCatalogView = lazy(() => import("./components/SkillCatalogView"));
const KanbanTab = lazy(() => import("./components/agent-manager/KanbanTab"));
const SettingsView = lazy(() => import("./components/SettingsView"));
const OnboardingWizard = lazy(() => import("./components/OnboardingWizard"));
import OfficeSelectorBar from "./components/OfficeSelectorBar";
const OfficeManagerModal = lazy(() => import("./components/OfficeManagerModal"));
const AgentInfoCard = lazy(() => import("./components/agent-manager/AgentInfoCard"));
import { useSpriteMap } from "./components/AgentAvatar";
import { useI18n } from "./i18n";
import NotificationCenter, { type Notification, useNotifications } from "./components/NotificationCenter";
import { useDashboardSocket } from "./app/useDashboardSocket";
import {
  Building2,
  LayoutDashboard,
  Users,
  FileText,
  Wifi,
  WifiOff,
  Settings,
  KanbanSquare,
} from "lucide-react";
const ChatView = lazy(() => import("./components/ChatView"));
const CommandPalette = lazy(() => import("./components/CommandPalette"));

type ViewMode = "office" | "dashboard" | "agents" | "meetings" | "chat" | "skills" | "kanban" | "settings";

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

// ── Bootstrap data shape ──

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

// ── Root component: bootstrap then render providers ──

export default function App() {
  const [data, setData] = useState<BootstrapData | null>(null);
  const { notifications, pushNotification, dismissNotification } = useNotifications();

  useEffect(() => {
    (async () => {
      try {
        await api.getSession();
        const off = await api.getOffices();
        const defaultOfficeId = off.length > 0 ? off[0].id : undefined;
        const [allAg, ag, allDep, dep, ses, st, set, rtm, logs, cards, dispatches] = await Promise.all([
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
        const resolvedSettings = set.companyName
          ? ({ ...DEFAULT_SETTINGS, ...set } as CompanySettings)
          : DEFAULT_SETTINGS;
        setData({
          offices: off,
          agents: ag,
          allAgents: allAg,
          departments: dep,
          allDepartments: allDep,
          sessions: ses,
          stats: st,
          settings: resolvedSettings,
          roundTableMeetings: rtm,
          auditLogs: logs,
          kanbanCards: cards,
          taskDispatches: dispatches,
          selectedOfficeId: defaultOfficeId ?? null,
        });
      } catch (e) {
        console.error("Bootstrap failed:", e);
        // Allow rendering even on failure so user sees something
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
  }, []);

  // WS connection — kept at root so wsRef is available early
  // The handler is a no-op pass-through: each context listens via the
  // CustomEvent("pcd-ws-event") that useDashboardSocket already dispatches.
  // We only handle notification-only events here (kanban card notifications).
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
        case "kanban_card_updated": {
          break;
        }
      }
    },
    [pushNotification],
  );

  const { wsConnected, wsRef } = useDashboardSocket(handleWsEvent);

  const { t } = useI18n();

  if (!data) {
    return (
      <div className="flex items-center justify-center h-screen bg-gray-900 text-gray-400">
        <div className="text-center">
          <div className="text-4xl mb-4">🐾</div>
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
            wsRef={wsRef}
            notifications={notifications}
            pushNotification={pushNotification}
            dismissNotification={dismissNotification}
          />
        </KanbanProvider>
      </SettingsProvider>
    </OfficeProvider>
  );
}

// ── Shell: view routing + layout ──

interface AppShellProps {
  wsConnected: boolean;
  wsRef: React.RefObject<WebSocket | null>;
  notifications: Notification[];
  pushNotification: (msg: string, level: Notification["type"]) => void;
  dismissNotification: (id: string) => void;
}

function AppShell({ wsConnected, wsRef, notifications, pushNotification, dismissNotification }: AppShellProps) {
  const [view, setView] = useState<ViewMode>("office");
  const [showOfficeManager, setShowOfficeManager] = useState(false);
  const [officeInfoAgent, setOfficeInfoAgent] = useState<Agent | null>(null);
  const [showCmdPalette, setShowCmdPalette] = useState(false);

  const { settings, setSettings, stats, refreshStats, isKo, locale, tr } = useSettings();
  const {
    offices,
    selectedOfficeId,
    setSelectedOfficeId,
    agents,
    allAgents,
    departments,
    allDepartments,
    sessions,
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
  } = useOffice();
  const { kanbanCards, taskDispatches, upsertKanbanCard, setKanbanCards } = useKanban();

  const spriteMap = useSpriteMap(agents);

  // I7: Global command palette (Cmd+K)
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === "k") {
        e.preventDefault();
        setShowCmdPalette((v) => !v);
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, []);

  const handleOfficeChanged = useCallback(() => {
    refreshOffices();
    refreshAgents();
    refreshAllAgents();
    refreshDepartments();
    refreshAllDepartments();
    refreshAuditLogs();
  }, [refreshOffices, refreshAgents, refreshAllAgents, refreshDepartments, refreshAllDepartments, refreshAuditLogs]);

  const newMeetingsCount = roundTableMeetings.filter(hasUnresolvedMeetingIssues).length;
  const viewFallbackLabel = useMemo(() => ({
    office: isKo ? "오피스 로딩 중..." : "Loading Office...",
    dashboard: isKo ? "대시보드 로딩 중..." : "Loading Dashboard...",
    agents: isKo ? "직원 로딩 중..." : "Loading Agents...",
    kanban: isKo ? "칸반 로딩 중..." : "Loading Kanban...",
    meetings: isKo ? "회의 로딩 중..." : "Loading Meetings...",
    chat: isKo ? "채팅 로딩 중..." : "Loading Chat...",
    skills: isKo ? "스킬 로딩 중..." : "Loading Skills...",
    settings: isKo ? "설정 로딩 중..." : "Loading Settings...",
  } satisfies Record<ViewMode, string>), [isKo]);

  const navItems: Array<{ id: ViewMode; icon: React.ReactNode; label: string; badge?: number; badgeColor?: string }> = [
    { id: "office", icon: <Building2 size={20} />, label: isKo ? "오피스" : "Office" },
    { id: "dashboard", icon: <LayoutDashboard size={20} />, label: isKo ? "대시보드" : "Dashboard" },
    { id: "kanban", icon: <KanbanSquare size={20} />, label: isKo ? "칸반" : "Kanban" },
    { id: "agents", icon: <Users size={20} />, label: isKo ? "직원" : "Staff" },
    { id: "meetings", icon: <FileText size={20} />, label: isKo ? "회의" : "Meetings", badge: newMeetingsCount || undefined, badgeColor: "bg-amber-500" },
    { id: "settings", icon: <Settings size={20} />, label: isKo ? "설정" : "Settings" },
  ];

  return (
    <div className="flex fixed inset-0 bg-gray-900">
      {/* Sidebar (hidden on mobile) */}
      <nav className="hidden sm:flex w-[4.5rem] bg-gray-950 border-r border-gray-800 flex-col items-center py-4 gap-1">
        <div className="text-2xl mb-4">🐾</div>
        {navItems.map((item) => (
          <NavBtn
            key={item.id}
            icon={item.icon}
            active={view === item.id}
            badge={item.badge}
            badgeColor={item.badgeColor}
            onClick={() => { setView(item.id); if (item.id === "dashboard") refreshStats(); }}
            label={item.label}
          />
        ))}
        <div className="flex-1" />
        <NotificationCenter notifications={notifications} onDismiss={dismissNotification} />
        <div
          className="w-10 h-10 flex items-center justify-center rounded-lg"
          title={wsConnected ? (isKo ? "서버 연결됨" : "Server connected") : (isKo ? "서버 연결 끊김" : "Server disconnected")}
        >
          {wsConnected
            ? <Wifi size={16} className="text-emerald-500" />
            : <WifiOff size={16} className="text-red-400 animate-pulse" />}
        </div>
      </nav>

      {/* Main content */}
      <div className="flex-1 flex flex-col overflow-hidden">
        {/* Office selector bar — hide on chat/settings views */}
        {offices.length > 0 && view !== "chat" && view !== "settings" && view !== "kanban" && (
          <OfficeSelectorBar
            offices={offices}
            selectedOfficeId={selectedOfficeId}
            onSelectOffice={setSelectedOfficeId}
            onManageOffices={() => setShowOfficeManager(true)}
            isKo={isKo}
          />
        )}

        <main className="flex-1 min-h-0 flex flex-col overflow-hidden mb-14 sm:mb-0">
          <Suspense
            fallback={
              <div className="flex items-center justify-center h-full text-gray-500">
                {viewFallbackLabel[view]}
              </div>
            }
          >
            {view === "office" && (
              <OfficeView
                agents={agentsWithDispatched}
                departments={departments}
                language={settings.language}
                theme={settings.theme === "auto" ? (window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light") : settings.theme}
                subAgents={subAgents}
                notifications={notifications}
                auditLogs={auditLogs}
                activeMeeting={roundTableMeetings.find((m) => m.status === "in_progress") ?? null}
                kanbanCards={kanbanCards}
                onNavigateToKanban={() => setView("kanban")}
                onSelectAgent={(agent) => setOfficeInfoAgent(agent)}
                onSelectDepartment={() => { setView("agents"); }}
                customDeptThemes={settings.roomThemes}
              />
            )}
            {view === "dashboard" && (
              <DashboardPageView
                stats={stats}
                agents={agents}
                settings={settings}
                onSelectAgent={(agent) => setOfficeInfoAgent(agent)}
              />
            )}
            {view === "agents" && (
              <AgentManagerView
                agents={agents}
                departments={departments}
                language={settings.language}
                officeId={selectedOfficeId}
                onAgentsChange={() => { refreshAgents(); refreshAllAgents(); refreshOffices(); }}
                onDepartmentsChange={() => { refreshDepartments(); refreshAllDepartments(); refreshOffices(); }}
                sessions={visibleDispatchedSessions}
                onAssign={async (id, patch) => {
                  const updated = await api.assignDispatchedSession(id, patch);
                  setSessions((prev) =>
                    prev.map((s) => (s.id === updated.id ? updated : s)),
                  );
                }}
              />
            )}
            {view === "kanban" && (
              <div className="h-full overflow-auto p-4 sm:p-6 pb-40">
                <KanbanTab
                  tr={(ko: string, en: string) => settings.language === "ko" ? ko : en}
                  locale={settings.language}
                  cards={kanbanCards}
                  dispatches={taskDispatches}
                  agents={allAgents}
                  departments={allDepartments}
                  onAssignIssue={async (payload) => {
                    const assigned = await api.assignKanbanIssue(payload);
                    upsertKanbanCard(assigned);
                  }}
                  onUpdateCard={async (id, patch) => {
                    const updated = await api.updateKanbanCard(id, patch);
                    upsertKanbanCard(updated);
                  }}
                  onRetryCard={async (id, payload) => {
                    const updated = await api.retryKanbanCard(id, payload);
                    upsertKanbanCard(updated);
                  }}
                  onRedispatchCard={async (id, payload) => {
                    const updated = await api.redispatchKanbanCard(id, payload);
                    upsertKanbanCard(updated);
                  }}
                  onDeleteCard={async (id) => {
                    await api.deleteKanbanCard(id);
                    setKanbanCards((prev) => prev.filter((card) => card.id !== id));
                  }}
                />
              </div>
            )}
            {view === "meetings" && (
              <MeetingMinutesView
                meetings={roundTableMeetings}
                onRefresh={() => api.getRoundTableMeetings().then(setRoundTableMeetings).catch(() => {})}
              />
            )}
            {view === "skills" && <SkillCatalogView />}
            {view === "chat" && (
              <ChatView
                agents={allAgents}
                departments={departments}
                notifications={notifications}
                auditLogs={auditLogs}
                isKo={isKo}
                wsRef={wsRef}
                onMessageSent={refreshAuditLogs}
              />
            )}
            {view === "settings" && (
              <SettingsView settings={settings} onSave={async (patch) => {
                await api.saveSettings(patch);
                setSettings((prev) => ({ ...prev, ...patch } as CompanySettings));
                refreshAuditLogs();
              }} isKo={isKo} />
            )}
          </Suspense>
        </main>

      </div>

      {/* G1: Mobile bottom tab bar */}
      <nav className="sm:hidden fixed bottom-0 left-0 right-0 bg-gray-950 border-t border-gray-800 flex justify-around items-center h-14 z-50">
        {navItems.map((item) => (
          <button
            key={item.id}
            onClick={() => { setView(item.id); if (item.id === "dashboard") refreshStats(); }}
            className={`relative flex flex-col items-center justify-center flex-1 h-full text-[10px] ${
              view === item.id ? "text-indigo-400" : "text-gray-500"
            }`}
          >
            {item.icon}
            <span className="mt-0.5">{item.label}</span>
            {item.badge !== undefined && item.badge > 0 && (
              <span className={`absolute top-1 right-1/4 ${item.badgeColor || "bg-emerald-500"} text-white text-[8px] w-3.5 h-3.5 rounded-full flex items-center justify-center`}>
                {item.badge}
              </span>
            )}
          </button>
        ))}
      </nav>

      {/* Agent Info Card (from Office View click) */}
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
            onAgentUpdated={() => { refreshAgents(); refreshAllAgents(); refreshOffices(); refreshAuditLogs(); }}
          />
        )}
      </Suspense>

      {/* I7: Command Palette */}
      <Suspense fallback={null}>
        {showCmdPalette && (
          <CommandPalette
            agents={allAgents}
            departments={departments}
            isKo={isKo}
            onSelectAgent={(agent) => setOfficeInfoAgent(agent)}
            onNavigate={(v) => setView(v as ViewMode)}
            onClose={() => setShowCmdPalette(false)}
          />
        )}
      </Suspense>

      {/* Office Manager Modal */}
      <Suspense fallback={null}>
        {showOfficeManager && (
          <OfficeManagerModal
            offices={offices}
            allAgents={allAgents}
            isKo={isKo}
            onClose={() => setShowOfficeManager(false)}
            onChanged={handleOfficeChanged}
          />
        )}
      </Suspense>
    </div>
  );
}

// ── NavBtn ──

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
      className={`relative w-14 rounded-lg flex flex-col items-center justify-center gap-0.5 py-1.5 transition-colors ${
        active
          ? "bg-indigo-600 text-white"
          : "text-gray-500 hover:text-gray-300 hover:bg-gray-800"
      }`}
    >
      {icon}
      <span className="text-[10px] leading-tight">{label}</span>
      {badge !== undefined && badge > 0 && (
        <span className={`absolute -top-1 -right-0.5 ${badgeColor || "bg-emerald-500"} text-white text-[10px] w-4 h-4 rounded-full flex items-center justify-center`}>
          {badge}
        </span>
      )}
    </button>
  );
}
