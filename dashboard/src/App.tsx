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
import { type Notification, useNotifications, ToastOverlay } from "./components/NotificationCenter";
import { useDashboardSocket } from "./app/useDashboardSocket";
import {
  Building2,
  LayoutDashboard,
  Users,
  FileText,
  Puzzle,
  Wifi,
  WifiOff,
  Settings,
  KanbanSquare,
  Menu,
  ChevronRight,
} from "lucide-react";
const PulseView = lazy(() => import("./components/PulseView"));
const CommandPalette = lazy(() => import("./components/CommandPalette"));

import { VIEW_REGISTRY, NAV_ROUTES, type ViewMode } from "./app/routes";

// SM breakpoint (640px) matches Tailwind's `sm:` — used to switch Pulse vs full Dashboard
function useIsMobile(breakpoint = 640) {
  const [mobile, setMobile] = useState(() => window.matchMedia(`(max-width: ${breakpoint - 1}px)`).matches);
  useEffect(() => {
    const mql = window.matchMedia(`(max-width: ${breakpoint - 1}px)`);
    const handler = (e: MediaQueryListEvent) => setMobile(e.matches);
    mql.addEventListener("change", handler);
    return () => mql.removeEventListener("change", handler);
  }, [breakpoint]);
  return mobile;
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
    (async () => {
      const partial: string[] = [];
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
          api.getRoundTableMeetings().catch(() => { partial.push("meetings"); return [] as RoundTableMeeting[]; }),
          api.getAuditLogs(12).catch(() => { partial.push("audit logs"); return [] as AuditLogEntry[]; }),
          api.getKanbanCards().catch(() => { partial.push("kanban"); return [] as KanbanCard[]; }),
          api.getTaskDispatches({ limit: 200 }).catch(() => { partial.push("dispatches"); return [] as TaskDispatch[]; }),
        ]);
        if (partial.length > 0) {
          pushNotification(`Failed to load: ${partial.join(", ")}`, "warning");
        }
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
        const msg = e instanceof Error ? e.message : String(e);
        console.error("Bootstrap failed:", e);
        setBootstrapError(msg);
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
      <div className="flex items-center justify-center h-screen bg-th-bg-primary text-th-text-muted">
        <div className="text-center">
          <div className="text-4xl mb-4">🐾</div>
          <div>{bootstrapError
            ? t({ ko: `로딩 실패: ${bootstrapError}`, en: `Load failed: ${bootstrapError}` })
            : t({ ko: "AgentDesk 대시보드 로딩 중...", en: "Loading AgentDesk Dashboard..." })
          }</div>
          {bootstrapError && (
            <button
              className="mt-4 px-4 py-2 rounded-lg bg-surface-medium text-th-text-primary text-sm"
              onClick={() => window.location.reload()}
            >
              {t({ ko: "새로고침", en: "Reload" })}
            </button>
          )}
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

// Views reachable from the mobile "More" tab
const MORE_VIEWS = new Set<ViewMode>(["agents", "meetings", "skills", "settings"]);

function AppShell({ wsConnected, wsRef, notifications, pushNotification, dismissNotification }: AppShellProps) {
  const [view, setView] = useState<ViewMode>("office");
  const [showOfficeManager, setShowOfficeManager] = useState(false);
  const [officeInfoAgent, setOfficeInfoAgent] = useState<Agent | null>(null);
  const [showCmdPalette, setShowCmdPalette] = useState(false);
  const [showMobileMore, setShowMobileMore] = useState(false);
  const isMobile = useIsMobile();

  // Reset mobile-more overlay when viewport switches to desktop
  useEffect(() => {
    if (!isMobile) setShowMobileMore(false);
  }, [isMobile]);

  const { settings, setSettings, stats, refreshStats, refreshingStats, isKo, locale, tr } = useSettings();
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
    refreshing,
    datasetStates,
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
  const viewFallbackLabel = useMemo(() =>
    Object.fromEntries(VIEW_REGISTRY.map((r) => [r.id, isKo ? r.loadingKo : r.loadingEn])) as Record<ViewMode, string>,
  [isKo]);

  // Desktop sidebar icon map (all views)
  const navIconMap: Record<string, React.ReactNode> = {
    office: <Building2 size={20} />,
    dashboard: <LayoutDashboard size={20} />,
    kanban: <KanbanSquare size={20} />,
    agents: <Users size={20} />,
    meetings: <FileText size={20} />,
    skills: <Puzzle size={20} />,
    settings: <Settings size={20} />,
  };
  const navBadges: Record<string, { badge?: number; badgeColor?: string }> = {
    meetings: { badge: newMeetingsCount || undefined, badgeColor: "bg-amber-500" },
  };
  const navItems = NAV_ROUTES.map((r) => ({
    id: r.id,
    icon: navIconMap[r.id] ?? <span>{r.icon}</span>,
    label: isKo ? r.labelKo : r.labelEn,
    ...navBadges[r.id],
  }));

  // Mobile 4-tab definitions
  const mobileActiveTab = showMobileMore ? "more" : MORE_VIEWS.has(view) ? "more" : view;
  const mobileMoreBadge = newMeetingsCount || undefined;

  const handleMobileNav = useCallback((tabId: string) => {
    if (tabId === "more") {
      setShowMobileMore((v) => !v);
    } else {
      setShowMobileMore(false);
      setView(tabId as ViewMode);
      if (tabId === "dashboard") refreshStats();
    }
  }, [refreshStats]);

  const handleMobileMoreSelect = useCallback((viewId: ViewMode) => {
    setShowMobileMore(false);
    setView(viewId);
    if (viewId === "settings") return;
  }, []);

  return (
    <div className="flex sm:fixed sm:inset-0 min-h-dvh bg-th-bg-primary">
      {/* Sidebar (hidden on mobile) */}
      <nav className="hidden sm:flex w-[4.5rem] bg-th-nav-bg border-r border-th-card-border flex-col items-center py-4 gap-1">
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
        <div
          className="w-10 h-10 flex items-center justify-center rounded-lg"
          title={wsConnected ? (isKo ? "서버 연결됨" : "Server connected") : (isKo ? "서버 연결 끊김" : "Server disconnected")}
        >
          {(refreshing || refreshingStats)
            ? <div className="w-4 h-4 rounded-full border-2 border-th-text-muted border-t-indigo-400 animate-spin" title={isKo ? "데이터 갱신 중..." : "Refreshing..."} />
            : wsConnected
              ? <Wifi size={16} className="text-emerald-500" />
              : <WifiOff size={16} className="text-red-400 animate-pulse" />}
        </div>
      </nav>

      {/* Main content */}
      <div className="flex-1 flex flex-col overflow-hidden">
        {/* Office selector bar — hide on settings/kanban views */}
        {offices.length > 0 && view !== "settings" && view !== "kanban" && (
          <OfficeSelectorBar
            offices={offices}
            selectedOfficeId={selectedOfficeId}
            onSelectOffice={setSelectedOfficeId}
            onManageOffices={() => setShowOfficeManager(true)}
            isKo={isKo}
          />
        )}

        {/* Dataset status banners — loading and error per dataset */}
        {Object.entries(datasetStates).some(([, s]) => s.loading) && (
          <div className="px-3 py-1 text-xs bg-indigo-500/10 text-indigo-400 border-b border-indigo-500/20 flex items-center gap-2">
            <div className="w-3 h-3 rounded-full border-2 border-indigo-400/40 border-t-indigo-400 animate-spin shrink-0" />
            <span>{isKo ? "갱신 중:" : "Refreshing:"}</span>
            {Object.entries(datasetStates)
              .filter(([, s]) => s.loading)
              .map(([key]) => <span key={key} className="px-1.5 py-0.5 rounded bg-indigo-500/20">{key}</span>)}
          </div>
        )}
        {Object.entries(datasetStates).some(([, s]) => s.error) && (
          <div className="px-3 py-1 text-xs bg-red-500/10 text-red-400 border-b border-red-500/20 flex items-center gap-2">
            <span>{isKo ? "로드 실패:" : "Failed:"}</span>
            {Object.entries(datasetStates)
              .filter(([, s]) => s.error)
              .map(([key, s]) => <span key={key} className="px-1.5 py-0.5 rounded bg-red-500/20" title={s.error ?? undefined}>{key}</span>)}
          </div>
        )}

        <main className="flex-1 min-h-0 flex flex-col overflow-hidden mb-14 sm:mb-0">
          {/* Mobile "More" menu overlay */}
          {showMobileMore && (
            <div className="sm:hidden flex-1 overflow-y-auto p-4">
              <MobileMoreMenu
                isKo={isKo}
                activeView={view}
                meetingBadge={newMeetingsCount}
                onSelect={handleMobileMoreSelect}
                wsConnected={wsConnected}
              />
            </div>
          )}
          {!showMobileMore && (
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
              {view === "settings" && (
                <SettingsView settings={settings} onSave={async (patch) => {
                  await api.saveSettings(patch);
                  setSettings((prev) => ({ ...prev, ...patch } as CompanySettings));
                  refreshAuditLogs();
                }} isKo={isKo} />
              )}
            </Suspense>
          )}
        </main>

      </div>

      {/* Mobile bottom tab bar: 4 tabs (Office / Pulse / Kanban / More) */}
      <nav className="sm:hidden fixed bottom-0 left-0 right-0 bg-th-nav-bg border-t border-th-card-border flex justify-around items-center h-14 z-50">
        {(refreshing || refreshingStats) && (
          <div className="absolute top-0 left-0 right-0 h-0.5 overflow-hidden">
            <div className="h-full bg-indigo-400 animate-[loading-bar_1.5s_ease-in-out_infinite]" />
          </div>
        )}
        <MobileTabBtn id="office" icon={<Building2 size={20} />} label={isKo ? "오피스" : "Office"} active={mobileActiveTab === "office"} onClick={handleMobileNav} />
        <MobileTabBtn id="dashboard" icon={<LayoutDashboard size={20} />} label={isKo ? "대시보드" : "Dashboard"} active={mobileActiveTab === "dashboard"} onClick={handleMobileNav} />
        <MobileTabBtn id="kanban" icon={<KanbanSquare size={20} />} label={isKo ? "칸반" : "Kanban"} active={mobileActiveTab === "kanban"} onClick={handleMobileNav} />
        <MobileTabBtn id="more" icon={<Menu size={20} />} label={isKo ? "더보기" : "More"} active={mobileActiveTab === "more"} badge={mobileMoreBadge} badgeColor="bg-amber-500" onClick={handleMobileNav} />
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

      {/* Toast overlay for API errors and warnings */}
      <ToastOverlay notifications={notifications} onDismiss={dismissNotification} />
    </div>
  );
}

// ── Desktop sidebar NavBtn ──

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
          : "text-th-text-muted hover:text-th-text-primary hover:bg-surface-hover"
      }`}
    >
      {icon}
      <span className="text-xs leading-tight">{label}</span>
      {badge !== undefined && badge > 0 && (
        <span className={`absolute -top-1 -right-0.5 ${badgeColor || "bg-emerald-500"} text-white text-xs w-4 h-4 rounded-full flex items-center justify-center`}>
          {badge}
        </span>
      )}
    </button>
  );
}

// ── Mobile bottom tab button ──

function MobileTabBtn({
  id,
  icon,
  label,
  active,
  badge,
  badgeColor,
  onClick,
}: {
  id: string;
  icon: React.ReactNode;
  label: string;
  active: boolean;
  badge?: number;
  badgeColor?: string;
  onClick: (id: string) => void;
}) {
  return (
    <button
      onClick={() => onClick(id)}
      className={`relative flex flex-col items-center justify-center flex-1 h-full text-xs ${
        active ? "text-indigo-400" : "text-th-text-muted"
      }`}
    >
      {icon}
      <span className="mt-0.5">{label}</span>
      {badge !== undefined && badge > 0 && (
        <span className={`absolute top-1 right-1/4 ${badgeColor || "bg-emerald-500"} text-white text-xs w-3.5 h-3.5 rounded-full flex items-center justify-center`}>
          {badge}
        </span>
      )}
    </button>
  );
}

// ── Mobile "More" menu ──

const MORE_MENU_ITEMS: Array<{ id: ViewMode; icon: React.ReactNode; labelKo: string; labelEn: string; desc_ko: string; desc_en: string }> = [
  { id: "agents", icon: <Users size={20} />, labelKo: "직원", labelEn: "Staff", desc_ko: "에이전트·부서 관리", desc_en: "Agents & departments" },
  { id: "meetings", icon: <FileText size={20} />, labelKo: "회의", labelEn: "Meetings", desc_ko: "라운드테이블 회의록", desc_en: "Round-table minutes" },
  { id: "skills", icon: <Puzzle size={20} />, labelKo: "스킬", labelEn: "Skills", desc_ko: "스킬 카탈로그", desc_en: "Skill catalog" },
  { id: "settings", icon: <Settings size={20} />, labelKo: "설정", labelEn: "Settings", desc_ko: "테마·언어·런타임 설정", desc_en: "Theme, language & runtime" },
];

function MobileMoreMenu({
  isKo,
  activeView,
  meetingBadge,
  onSelect,
  wsConnected,
}: {
  isKo: boolean;
  activeView: ViewMode;
  meetingBadge: number;
  onSelect: (view: ViewMode) => void;
  wsConnected: boolean;
}) {
  return (
    <div className="space-y-2">
      <h2 className="text-xs font-semibold uppercase tracking-widest px-1 mb-3" style={{ color: "var(--th-text-muted)" }}>
        {isKo ? "더보기" : "More"}
      </h2>
      {MORE_MENU_ITEMS.map((item) => (
        <button
          key={item.id}
          onClick={() => onSelect(item.id)}
          className="w-full flex items-center gap-3 rounded-xl px-4 py-3 text-left transition-colors"
          style={{
            background: activeView === item.id ? "var(--th-card-bg)" : "transparent",
            border: `1px solid ${activeView === item.id ? "var(--th-card-border)" : "transparent"}`,
          }}
        >
          <span className="text-th-text-muted">{item.icon}</span>
          <div className="flex-1 min-w-0">
            <div className="flex items-center gap-2">
              <span className="text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>
                {isKo ? item.labelKo : item.labelEn}
              </span>
              {item.id === "meetings" && meetingBadge > 0 && (
                <span className="bg-amber-500 text-white text-xs w-4 h-4 rounded-full flex items-center justify-center">
                  {meetingBadge}
                </span>
              )}
            </div>
            <div className="text-xs mt-0.5" style={{ color: "var(--th-text-muted)" }}>
              {isKo ? item.desc_ko : item.desc_en}
            </div>
          </div>
          <ChevronRight size={16} className="text-th-text-muted shrink-0" />
        </button>
      ))}
      <div className="mt-4 px-1 flex items-center gap-2 text-xs" style={{ color: "var(--th-text-muted)" }}>
        {wsConnected
          ? <><Wifi size={12} className="text-emerald-500" /> <span>{isKo ? "서버 연결됨" : "Connected"}</span></>
          : <><WifiOff size={12} className="text-red-400" /> <span>{isKo ? "연결 끊김" : "Disconnected"}</span></>}
      </div>
    </div>
  );
}
