import { lazy, Suspense, useCallback, useEffect, useMemo, useState } from "react";
import {
  Bell,
  BellRing,
  Building2,
  ChevronLeft,
  ChevronRight,
  FolderKanban,
  Home,
  LayoutDashboard,
  Menu,
  Search,
  Settings,
  Sparkles,
  Trophy,
  Users,
  Wifi,
  WifiOff,
  Wrench,
  X,
} from "lucide-react";
import {
  Link,
  NavLink,
  Navigate,
  Route,
  Routes,
  useLocation,
  useNavigate,
} from "react-router-dom";
import type {
  Agent,
  CompanySettings,
  DashboardStats,
  KanbanCard,
  RoundTableMeeting,
} from "../types";
import { DEFAULT_SETTINGS } from "../types";
import * as api from "../api/client";
import { useKanban } from "../contexts/KanbanContext";
import { useOffice } from "../contexts/OfficeContext";
import { useSettings } from "../contexts/SettingsContext";
import { useSpriteMap } from "../components/AgentAvatar";
import {
  ToastOverlay,
  type Notification,
} from "../components/NotificationCenter";
import OfficeSelectorBar from "../components/OfficeSelectorBar";
import { MOBILE_LAYOUT_MEDIA_QUERY } from "./breakpoints";
import {
  APP_ROUTE_SECTIONS,
  DEFAULT_ROUTE_PATH,
  PALETTE_ROUTES,
  PRIMARY_ROUTES,
  findRouteByPath,
  getSectionById,
  type AppRouteEntry,
  type AppRouteId,
} from "./routes";
import type { DashboardTab } from "./dashboardTabs";
import {
  DEFAULT_ACCENT_PRESET,
  THEME_STORAGE_KEY,
  applyThemeAccentDataset,
  persistAccentPreset,
  persistThemePreference,
  readStoredAccentPreset,
  readStoredThemePreference,
  readThemePreferenceFromPatch,
  resolveThemePreference,
  type AccentPreset,
  type ThemePreference,
} from "./themePreferences";

const OfficeView = lazy(() => import("../components/OfficeView"));
const DashboardPageView = lazy(() => import("../components/DashboardPageView"));
const KanbanTab = lazy(() => import("../components/agent-manager/KanbanTab"));
const AgentManagerView = lazy(() => import("../components/AgentManagerView"));
const OfficeManagerView = lazy(() => import("../components/OfficeManagerView"));
const MeetingMinutesView = lazy(() => import("../components/MeetingMinutesView"));
const SettingsView = lazy(() => import("../components/SettingsView"));
const AgentInfoCard = lazy(() => import("../components/agent-manager/AgentInfoCard"));
const CommandPalette = lazy(() => import("../components/CommandPalette"));

interface AppShellProps {
  wsConnected: boolean;
  notifications: Notification[];
  pushNotification: (message: string, type?: Notification["type"]) => string;
  updateNotification: (
    id: string,
    message: string,
    type?: Notification["type"],
  ) => void;
  dismissNotification: (id: string) => void;
}

type AgentsPageTab = "agents" | "departments" | "dispatch";
type KanbanSignalFocus = "review" | "blocked" | "requested" | "stalled";

const SIDEBAR_COLLAPSED_STORAGE_KEY = "agentdesk.sidebar.collapsed";
const MOBILE_TABBAR_SAFE_AREA_HEIGHT = "calc(3.5rem + env(safe-area-inset-bottom))";
const MOBILE_PRIMARY_ROUTE_IDS: AppRouteId[] = [
  "home",
  "office",
  "kanban",
  "stats",
];
const MOBILE_MORE_ROUTE_IDS: AppRouteId[] = [
  "agents",
  "ops",
  "meetings",
  "achievements",
  "settings",
];

const THEME_OPTIONS: Array<{
  id: ThemePreference;
  labelKo: string;
  labelEn: string;
}> = [
  { id: "auto", labelKo: "자동", labelEn: "Auto" },
  { id: "dark", labelKo: "다크", labelEn: "Dark" },
  { id: "light", labelKo: "라이트", labelEn: "Light" },
];

const ACCENT_OPTIONS: Array<{
  id: AccentPreset;
  label: string;
  token: string;
}> = [
  { id: "indigo", label: "Indigo", token: "--accent-indigo" },
  { id: "violet", label: "Violet", token: "--accent-violet" },
  { id: "amber", label: "Amber", token: "--accent-amber" },
  { id: "rose", label: "Rose", token: "--accent-rose" },
  { id: "cyan", label: "Cyan", token: "--accent-cyan" },
  { id: "lime", label: "Lime", token: "--accent-lime" },
];

export default function AppShell({
  wsConnected,
  notifications,
  pushNotification,
  updateNotification,
  dismissNotification,
}: AppShellProps) {
  const navigate = useNavigate();
  const location = useLocation();
  const currentRoute = useMemo(
    () => findRouteByPath(location.pathname),
    [location.pathname],
  );
  const { settings, setSettings, stats, refreshStats, isKo, locale, tr } =
    useSettings();
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
  } = useOffice();
  const { kanbanCards, taskDispatches, upsertKanbanCard, setKanbanCards } =
    useKanban();

  const [officeInfoAgent, setOfficeInfoAgent] = useState<Agent | null>(null);
  const [showCommandPalette, setShowCommandPalette] = useState(false);
  const [showShortcutHelp, setShowShortcutHelp] = useState(false);
  const [showNotificationPanel, setShowNotificationPanel] = useState(false);
  const [showMobileMoreMenu, setShowMobileMoreMenu] = useState(false);
  const [agentsPageTab, setAgentsPageTab] = useState<AgentsPageTab>("agents");
  const [kanbanSignalFocus, setKanbanSignalFocus] =
    useState<KanbanSignalFocus | null>(null);
  const [sidebarCollapsed, setSidebarCollapsed] = useState<boolean>(() => {
    if (typeof window === "undefined") return false;
    return (
      window.localStorage.getItem(SIDEBAR_COLLAPSED_STORAGE_KEY) === "true"
    );
  });
  const [themePreference, setThemePreference] = useState<ThemePreference>(() =>
    readStoredThemePreference(window.localStorage, settings.theme),
  );
  const [accentPreset, setAccentPreset] = useState<AccentPreset>(() =>
    readStoredAccentPreset(window.localStorage, DEFAULT_ACCENT_PRESET),
  );
  const [prefersDarkScheme, setPrefersDarkScheme] = useState(() =>
    window.matchMedia("(prefers-color-scheme: dark)").matches,
  );
  const [isMobileViewport, setIsMobileViewport] = useState(() => {
    if (typeof window === "undefined") return false;
    return window.matchMedia(MOBILE_LAYOUT_MEDIA_QUERY).matches;
  });

  const spriteMap = useSpriteMap(agents);
  const unresolvedMeetingsCount = roundTableMeetings.filter(
    hasUnresolvedMeetingIssues,
  ).length;
  const unreadCount = notifications.filter(
    (notification) => Date.now() - notification.ts < 60_000,
  ).length;
  const notificationBadgeCount = unresolvedMeetingsCount + unreadCount;
  const resolvedTheme = useMemo(
    () => resolveThemePreference(themePreference, prefersDarkScheme),
    [prefersDarkScheme, themePreference],
  );
  const recentNotifications = notifications.slice(0, 6);

  useEffect(() => {
    window.localStorage.setItem(
      SIDEBAR_COLLAPSED_STORAGE_KEY,
      String(sidebarCollapsed),
    );
  }, [sidebarCollapsed]);

  useEffect(() => {
    const query = window.matchMedia("(prefers-color-scheme: dark)");
    const sync = (matches: boolean) => setPrefersDarkScheme(matches);

    sync(query.matches);

    const listener = (event: MediaQueryListEvent) => sync(event.matches);
    if (typeof query.addEventListener === "function") {
      query.addEventListener("change", listener);
      return () => query.removeEventListener("change", listener);
    }

    query.addListener(listener);
    return () => query.removeListener(listener);
  }, []);

  useEffect(() => {
    if (window.localStorage.getItem(THEME_STORAGE_KEY) == null) {
      setThemePreference(settings.theme);
    }
  }, [settings.theme]);

  useEffect(() => {
    persistThemePreference(window.localStorage, themePreference);
    persistAccentPreset(window.localStorage, accentPreset);
    applyThemeAccentDataset(
      document.documentElement,
      resolvedTheme,
      accentPreset,
    );
  }, [accentPreset, resolvedTheme, themePreference]);

  useEffect(() => {
    const media = window.matchMedia(MOBILE_LAYOUT_MEDIA_QUERY);
    const sync = () => setIsMobileViewport(media.matches);
    sync();
    media.addEventListener("change", sync);
    return () => media.removeEventListener("change", sync);
  }, []);

  useEffect(() => {
    setShowNotificationPanel(false);
    setShowMobileMoreMenu(false);
  }, [location.pathname]);

  useEffect(() => {
    if (!isMobileViewport) {
      setShowMobileMoreMenu(false);
    }
  }, [isMobileViewport]);

  useEffect(() => {
    if (currentRoute?.id === "home" || currentRoute?.id === "stats") {
      refreshStats();
    }
  }, [currentRoute?.id, refreshStats]);

  const persistSettingsPatch = useCallback(
    async (patch: Record<string, unknown>) => {
      const mergedSettings = { ...settings, ...patch } as CompanySettings;
      await api.saveSettings(mergedSettings);
      const refreshed = await api.getSettings();
      setSettings({ ...DEFAULT_SETTINGS, ...refreshed } as CompanySettings);
      refreshAuditLogs();
    },
    [refreshAuditLogs, setSettings, settings],
  );

  const navigateToRoute = useCallback(
    (
      path: string,
      options?: { agentsTab?: AgentsPageTab; kanbanFocus?: KanbanSignalFocus },
    ) => {
      setShowMobileMoreMenu(false);
      if (options?.agentsTab) {
        setAgentsPageTab(options.agentsTab);
      }
      if (options?.kanbanFocus) {
        setKanbanSignalFocus(options.kanbanFocus);
      }
      navigate(path);
    },
    [navigate],
  );

  const handleSettingsSave = useCallback(
    async (patch: Record<string, unknown>) => {
      const requestedThemePreference = readThemePreferenceFromPatch(patch);
      await persistSettingsPatch(patch);
      if (requestedThemePreference) {
        setThemePreference(requestedThemePreference);
      }
    },
    [persistSettingsPatch],
  );

  const handleOfficeChanged = useCallback(() => {
    refreshOffices();
    refreshAgents();
    refreshAllAgents();
    refreshDepartments();
    refreshAllDepartments();
    refreshAuditLogs();
  }, [
    refreshAgents,
    refreshAllAgents,
    refreshAllDepartments,
    refreshAuditLogs,
    refreshDepartments,
    refreshOffices,
  ]);

  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const tag = target?.tagName;
      const isEditable = Boolean(
        target?.isContentEditable ||
          tag === "INPUT" ||
          tag === "TEXTAREA" ||
          tag === "SELECT",
      );
      if (isEditable) return;

      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setShowCommandPalette((prev) => !prev);
        return;
      }

      if (
        event.key === "?" &&
        !event.metaKey &&
        !event.ctrlKey &&
        !event.altKey
      ) {
        event.preventDefault();
        setShowShortcutHelp((prev) => !prev);
        return;
      }

      if (event.altKey && !event.metaKey && !event.ctrlKey) {
        const route = PRIMARY_ROUTES.find(
          (item) => item.shortcutKey === event.key,
        );
        if (!route) return;
        event.preventDefault();
        navigateToRoute(route.path);
      }
    };

    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [navigateToRoute]);

  const breadcrumbSection = getSectionById(
    currentRoute?.section ?? APP_ROUTE_SECTIONS[0].id,
  );
  const mobilePrimaryRoutes = useMemo(
    () =>
      PRIMARY_ROUTES.filter((route) =>
        MOBILE_PRIMARY_ROUTE_IDS.includes(route.id),
      ),
    [],
  );
  const mobileMoreRoutes = useMemo(
    () =>
      PRIMARY_ROUTES.filter((route) =>
        MOBILE_MORE_ROUTE_IDS.includes(route.id),
      ),
    [],
  );
  const activeMobileRouteId =
    showMobileMoreMenu ||
    (currentRoute && MOBILE_MORE_ROUTE_IDS.includes(currentRoute.id))
      ? "more"
      : currentRoute && MOBILE_PRIMARY_ROUTE_IDS.includes(currentRoute.id)
        ? currentRoute.id
        : "home";

  return (
    <div
      className="fixed inset-0 flex overflow-hidden"
      style={{ background: "var(--th-bg-primary)" }}
    >
      {!isMobileViewport && (
        <aside
          data-testid="app-sidebar-nav"
          className="flex flex-col border-r"
          style={{
            width: sidebarCollapsed ? "5.5rem" : "15rem",
            borderColor: "var(--th-border-subtle)",
            background:
              "linear-gradient(180deg, color-mix(in srgb, var(--th-nav-bg) 96%, black 4%) 0%, color-mix(in srgb, var(--th-bg-surface) 94%, transparent) 100%)",
          }}
        >
          <div
            className={`flex items-center gap-3 border-b px-4 py-4 ${
              sidebarCollapsed ? "justify-center" : ""
            }`}
            style={{ borderColor: "var(--th-border-subtle)" }}
          >
            <div className="flex h-11 w-11 items-center justify-center rounded-2xl bg-emerald-500/15 text-xl text-emerald-300">
              🐾
            </div>
            {!sidebarCollapsed && (
              <div className="min-w-0">
                <div
                  className="truncate text-sm font-semibold"
                  style={{ color: "var(--th-text-heading)" }}
                >
                  AgentDesk
                </div>
                <div
                  className="truncate text-xs"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {isKo ? "앱 셸 v2" : "App shell v2"}
                </div>
              </div>
            )}
            <button
              type="button"
              onClick={() => setSidebarCollapsed((prev) => !prev)}
              className="ml-auto hidden h-9 w-9 items-center justify-center rounded-xl border text-[var(--th-text-secondary)] transition-colors hover:bg-white/5 md:flex"
              style={{ borderColor: "var(--th-border-subtle)" }}
              aria-label={
                sidebarCollapsed
                  ? tr("사이드바 펼치기", "Expand sidebar")
                  : tr("사이드바 접기", "Collapse sidebar")
              }
              title={
                sidebarCollapsed
                  ? tr("사이드바 펼치기", "Expand sidebar")
                  : tr("사이드바 접기", "Collapse sidebar")
              }
            >
              {sidebarCollapsed ? (
                <ChevronRight size={16} />
              ) : (
                <ChevronLeft size={16} />
              )}
            </button>
          </div>

          <div className="flex-1 overflow-y-auto px-3 py-4">
            {APP_ROUTE_SECTIONS.map((section) => {
              const routes = PRIMARY_ROUTES.filter(
                (route) => route.section === section.id,
              );
              return (
                <div key={section.id} className="mb-5">
                  {!sidebarCollapsed && (
                    <div
                      className="px-3 pb-2 text-[11px] font-semibold uppercase tracking-[0.18em]"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      {isKo ? section.labelKo : section.labelEn}
                    </div>
                  )}
                  <div className="space-y-1">
                    {routes.map((route) => (
                      <SidebarRouteButton
                        key={route.id}
                        route={route}
                        currentRouteId={currentRoute?.id ?? null}
                        collapsed={sidebarCollapsed}
                        isKo={isKo}
                        badge={
                          route.id === "meetings"
                            ? unresolvedMeetingsCount || undefined
                            : route.id === "settings"
                              ? unreadCount || undefined
                              : undefined
                        }
                        onNavigate={() => {
                          if (route.id === "agents") {
                            setAgentsPageTab("agents");
                          }
                          navigateToRoute(route.path);
                        }}
                      />
                    ))}
                  </div>
                </div>
              );
            })}
          </div>

          <div
            className="border-t px-3 py-3"
            style={{ borderColor: "var(--th-border-subtle)" }}
          >
            <div
              className={`flex items-center gap-3 rounded-2xl border px-3 py-3 ${
                sidebarCollapsed ? "justify-center" : ""
              }`}
              style={{
                borderColor: wsConnected ? "#1f9d66" : "#9f3f3f",
                background: wsConnected
                  ? "rgba(16, 185, 129, 0.08)"
                  : "rgba(239, 68, 68, 0.08)",
              }}
            >
              {wsConnected ? (
                <Wifi size={16} className="text-emerald-400" />
              ) : (
                <WifiOff size={16} className="text-red-400" />
              )}
              {!sidebarCollapsed && (
                <div className="min-w-0">
                  <div
                    className="text-xs font-semibold"
                    style={{ color: "var(--th-text-primary)" }}
                  >
                    {wsConnected
                      ? tr("서버 연결됨", "Server connected")
                      : tr("재연결 중", "Reconnecting")}
                  </div>
                  <div
                    className="truncate text-[11px]"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {wsConnected
                      ? tr("실시간 업데이트 수신 중", "Realtime updates active")
                      : tr("웹소켓 상태를 확인하세요", "Check websocket status")}
                  </div>
                </div>
              )}
            </div>
          </div>
        </aside>
      )}

      <div className="flex min-w-0 flex-1 flex-col overflow-hidden">
        <header
          data-testid="topbar"
          className="relative z-[60] shrink-0 border-b px-4 py-3 sm:px-5"
          style={{
            borderColor: "var(--th-border-subtle)",
            background:
              "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 94%, transparent) 100%)",
          }}
        >
          <div className="flex flex-wrap items-center gap-3">
            <div className="min-w-0 flex-1">
              <div
                className="flex items-center gap-2 text-xs font-medium"
                style={{ color: "var(--th-text-muted)" }}
              >
                <span>{isKo ? breadcrumbSection.labelKo : breadcrumbSection.labelEn}</span>
                <ChevronRight size={12} />
                <span>{currentRoute ? (isKo ? currentRoute.labelKo : currentRoute.labelEn) : (isKo ? "홈" : "Home")}</span>
              </div>
              <div
                className="mt-1 text-lg font-semibold tracking-tight"
                style={{ color: "var(--th-text-heading)" }}
              >
                {currentRoute
                  ? isKo
                    ? currentRoute.labelKo
                    : currentRoute.labelEn
                  : tr("홈", "Home")}
              </div>
            </div>

            <label
              data-testid="topbar-search"
              className="order-3 flex min-w-[14rem] flex-1 items-center gap-2 rounded-2xl border px-3 py-2 text-sm sm:order-none sm:max-w-md"
              style={{
                borderColor: "var(--th-border-subtle)",
                background: "color-mix(in srgb, var(--th-bg-surface) 92%, transparent)",
              }}
            >
              <Search size={16} style={{ color: "var(--th-text-muted)" }} />
              <input
                type="search"
                readOnly
                value=""
                onFocus={() => setShowCommandPalette(true)}
                onClick={() => setShowCommandPalette(true)}
                placeholder={tr(
                  "페이지, 에이전트, 부서 검색",
                  "Search pages, agents, departments",
                )}
                className="w-full bg-transparent text-sm outline-none"
                style={{ color: "var(--th-text-primary)" }}
                aria-label={tr("검색 열기", "Open search")}
              />
              <kbd
                className="hidden rounded-lg px-2 py-1 text-[11px] sm:inline-flex"
                style={{
                  background: "var(--th-overlay-subtle)",
                  color: "var(--th-text-muted)",
                }}
              >
                ⌘K
              </kbd>
            </label>

            <div className="ml-auto flex items-center gap-2 sm:ml-0">
              <div className="relative">
                <button
                  type="button"
                  onClick={() =>
                    setShowNotificationPanel((prev) => !prev)
                  }
                  className="relative flex h-10 w-10 items-center justify-center rounded-xl border transition-colors hover:bg-white/5"
                  style={{ borderColor: "var(--th-border-subtle)" }}
                  aria-label={tr("알림 보기", "View notifications")}
                  title={tr("알림 보기", "View notifications")}
                >
                  {notificationBadgeCount > 0 ? (
                    <BellRing size={18} />
                  ) : (
                    <Bell size={18} />
                  )}
                  {notificationBadgeCount > 0 && (
                    <span className="absolute -right-1 -top-1 flex h-5 min-w-5 items-center justify-center rounded-full bg-emerald-500 px-1 text-[10px] font-semibold text-white">
                      {notificationBadgeCount > 9 ? "9+" : notificationBadgeCount}
                    </span>
                  )}
                </button>

                {showNotificationPanel && (
                  <div
                    className="absolute right-0 top-12 z-[90] w-[min(22rem,calc(100vw-2rem))] rounded-3xl border p-3 shadow-2xl"
                    style={{
                      borderColor: "var(--th-border-subtle)",
                      background:
                        "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 95%, transparent) 100%)",
                    }}
                  >
                    <div className="flex items-center justify-between gap-3 px-1 pb-2">
                      <div>
                        <div
                          className="text-sm font-semibold"
                          style={{ color: "var(--th-text-heading)" }}
                        >
                          {tr("알림", "Notifications")}
                        </div>
                        <div
                          className="text-xs"
                          style={{ color: "var(--th-text-muted)" }}
                        >
                          {tr(
                            "최근 이벤트와 회의 후속 상태",
                            "Recent events and meeting follow-ups",
                          )}
                        </div>
                      </div>
                      <button
                        type="button"
                        onClick={() => setShowNotificationPanel(false)}
                        className="flex h-8 w-8 items-center justify-center rounded-xl text-[var(--th-text-muted)]"
                      >
                        <X size={14} />
                      </button>
                    </div>

                    <div className="space-y-2">
                      <NotificationSummaryRow
                        label={tr("미해결 회의", "Open meetings")}
                        value={unresolvedMeetingsCount}
                        accent="var(--th-accent-warn)"
                      />
                      <NotificationSummaryRow
                        label={tr("최근 토스트", "Recent toasts")}
                        value={recentNotifications.length}
                        accent="var(--th-accent-info)"
                      />
                    </div>

                    <div className="mt-3 space-y-2">
                      {recentNotifications.length === 0 ? (
                        <div
                          className="rounded-2xl border px-3 py-4 text-sm"
                          style={{
                            borderColor: "var(--th-border-subtle)",
                            color: "var(--th-text-muted)",
                            background: "var(--th-overlay-subtle)",
                          }}
                        >
                          {tr("새 알림이 없습니다.", "No recent notifications.")}
                        </div>
                      ) : (
                        recentNotifications.map((notification) => (
                          <div
                            key={notification.id}
                            className="rounded-2xl border px-3 py-3"
                            style={{
                              borderColor: "var(--th-border-subtle)",
                              background: "var(--th-overlay-subtle)",
                            }}
                          >
                            <div className="flex items-start gap-3">
                              <span
                                className="mt-1 h-2.5 w-2.5 rounded-full"
                                style={{
                                  background: notificationColor(notification.type),
                                }}
                              />
                              <div className="min-w-0 flex-1">
                                <div
                                  className="text-sm leading-relaxed"
                                  style={{ color: "var(--th-text-primary)" }}
                                >
                                  {notification.message}
                                </div>
                                <div
                                  className="mt-1 text-[11px]"
                                  style={{ color: "var(--th-text-muted)" }}
                                >
                                  {formatRelativeTime(notification.ts, isKo)}
                                </div>
                              </div>
                              <button
                                type="button"
                                onClick={() => dismissNotification(notification.id)}
                                className="flex h-8 w-8 items-center justify-center rounded-xl text-[var(--th-text-muted)]"
                              >
                                <X size={12} />
                              </button>
                            </div>
                          </div>
                        ))
                      )}
                    </div>

                    <button
                      type="button"
                      onClick={() => {
                        setShowNotificationPanel(false);
                        navigateToRoute("/meetings");
                      }}
                      className="mt-3 flex w-full items-center justify-center gap-2 rounded-2xl border px-3 py-2 text-sm font-medium transition-colors hover:bg-white/5"
                      style={{ borderColor: "var(--th-border-subtle)" }}
                    >
                      <Sparkles size={15} />
                      {tr("회의 페이지로 이동", "Open meetings page")}
                    </button>
                  </div>
                )}
              </div>

              <button
                type="button"
                onClick={() => navigateToRoute("/settings")}
                className="flex h-10 w-10 items-center justify-center rounded-xl border transition-colors hover:bg-white/5"
                style={{ borderColor: "var(--th-border-subtle)" }}
                aria-label={tr("설정으로 이동", "Open settings")}
                title={tr("설정으로 이동", "Open settings")}
              >
                <Settings size={18} />
              </button>
            </div>
          </div>

          <div
            className="mt-3 flex flex-col gap-3 rounded-2xl border px-3 py-3 xl:flex-row xl:items-center xl:justify-between"
            style={{
              borderColor: "var(--th-border-subtle)",
              background:
                "linear-gradient(180deg, color-mix(in oklch, var(--bg-1) 94%, transparent) 0%, color-mix(in oklch, var(--bg-2) 98%, transparent) 100%)",
            }}
          >
            <div className="flex min-w-0 flex-wrap items-center gap-2">
              <span
                className="font-display text-[11px] font-semibold uppercase tracking-[0.18em]"
                style={{ color: "var(--fg-faint)" }}
              >
                Theme
              </span>
              <div
                className="flex items-center gap-1 rounded-full p-1"
                style={{
                  background:
                    "color-mix(in oklch, var(--bg-3) 72%, transparent)",
                }}
              >
                {THEME_OPTIONS.map((option) => {
                  const active = themePreference === option.id;
                  return (
                    <button
                      key={option.id}
                      type="button"
                      onClick={() => setThemePreference(option.id)}
                      aria-pressed={active}
                      className="rounded-full px-3 py-1 text-xs font-medium transition-colors"
                      style={
                        active
                          ? {
                              background: "var(--accent-soft)",
                              color: "var(--accent)",
                            }
                          : { color: "var(--fg-muted)" }
                      }
                    >
                      {isKo ? option.labelKo : option.labelEn}
                    </button>
                  );
                })}
              </div>
              <span
                className="rounded-full px-2 py-1 text-[11px]"
                style={{
                  background: "var(--th-overlay-medium)",
                  color: "var(--fg-muted)",
                }}
              >
                {isKo ? `현재 ${resolvedTheme}` : `Live ${resolvedTheme}`}
              </span>
            </div>

            <div className="flex min-w-0 flex-wrap items-center gap-2">
              <span
                className="font-display text-[11px] font-semibold uppercase tracking-[0.18em]"
                style={{ color: "var(--fg-faint)" }}
              >
                Accent
              </span>
              <div className="flex items-center gap-1.5">
                {ACCENT_OPTIONS.map((option) => {
                  const active = accentPreset === option.id;
                  return (
                    <button
                      key={option.id}
                      type="button"
                      title={option.label}
                      aria-label={option.label}
                      aria-pressed={active}
                      onClick={() => setAccentPreset(option.id)}
                      className="flex h-8 w-8 items-center justify-center rounded-full transition-transform"
                      style={{
                        border: active
                          ? "2px solid var(--fg)"
                          : "1px solid color-mix(in oklch, var(--line) 74%, transparent)",
                        background:
                          "color-mix(in oklch, var(--bg-2) 92%, transparent)",
                        transform: active ? "translateY(-1px)" : undefined,
                      }}
                    >
                      <span
                        className="h-4 w-4 rounded-full"
                        style={{ background: `var(${option.token})` }}
                      />
                    </button>
                  );
                })}
              </div>
            </div>
          </div>
        </header>

        {currentRoute?.showOfficeSelector && offices.length > 0 && (
          <OfficeSelectorBar
            offices={offices}
            selectedOfficeId={selectedOfficeId}
            onSelectOffice={setSelectedOfficeId}
            onManageOffices={() => navigateToRoute("/ops")}
            isKo={isKo}
          />
        )}

        <main
          data-testid="app-main-scroll"
          className="min-h-0 flex-1 overflow-hidden"
          style={{
            marginBottom: isMobileViewport
              ? MOBILE_TABBAR_SAFE_AREA_HEIGHT
              : undefined,
          }}
        >
          <Suspense
            fallback={
              <ViewSkeleton
                label={
                  currentRoute
                    ? isKo
                      ? currentRoute.labelKo
                      : currentRoute.labelEn
                    : tr("로딩 중...", "Loading...")
                }
              />
            }
          >
            <Routes>
              <Route path="/" element={<Navigate replace to={DEFAULT_ROUTE_PATH} />} />
              <Route
                path="/home"
                element={
                  <HomeOverviewPage
                    isKo={isKo}
                    currentOfficeLabel={selectedOfficeLabel(
                      offices,
                      selectedOfficeId,
                      tr,
                    )}
                    stats={stats}
                    meetings={roundTableMeetings}
                    notifications={notifications}
                    kanbanCards={kanbanCards}
                  />
                }
              />
              <Route
                path="/office"
                element={
                  <OfficeView
                    agents={agentsWithDispatched}
                    departments={departments}
                    language={settings.language}
                    theme={resolvedTheme}
                    subAgents={subAgents}
                    notifications={notifications}
                    auditLogs={auditLogs}
                    activeMeeting={
                      roundTableMeetings.find(
                        (meeting) => meeting.status === "in_progress",
                      ) ?? null
                    }
                    kanbanCards={kanbanCards}
                    onNavigateToKanban={() => navigateToRoute("/kanban")}
                    onSelectAgent={(agent) => setOfficeInfoAgent(agent)}
                    onSelectDepartment={() =>
                      navigateToRoute("/agents", { agentsTab: "departments" })
                    }
                    customDeptThemes={settings.roomThemes}
                  />
                }
              />
              <Route
                path="/agents"
                element={
                  <AgentManagerView
                    agents={agents}
                    departments={departments}
                    language={settings.language}
                    officeId={selectedOfficeId}
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
                    sessions={visibleDispatchedSessions}
                    onAssign={async (id, patch) => {
                      const updated = await api.assignDispatchedSession(id, patch);
                      setSessions((prev) =>
                        prev.map((session) =>
                          session.id === updated.id ? updated : session,
                        ),
                      );
                    }}
                    activeTab={agentsPageTab}
                    onTabChange={setAgentsPageTab}
                  />
                }
              />
              <Route
                path="/kanban"
                element={
                  <div className="h-full overflow-auto p-4 pb-36 sm:p-6">
                    <KanbanTab
                      tr={(ko: string, en: string) =>
                        settings.language === "ko" ? ko : en
                      }
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
                      onDeleteCard={async (id: string) => {
                        await api.deleteKanbanCard(id);
                        setKanbanCards((prev) =>
                          prev.filter((card) => card.id !== id),
                        );
                      }}
                      onPatchDeferDod={async (id, payload) => {
                        const updated = await api.patchKanbanDeferDod(id, payload);
                        upsertKanbanCard(updated);
                      }}
                      externalStatusFocus={kanbanSignalFocus}
                      onClearSignalFocus={() => setKanbanSignalFocus(null)}
                    />
                  </div>
                }
              />
              <Route
                path="/stats"
                element={
                  <DashboardPageView
                    stats={stats}
                    agents={agents}
                    sessions={visibleDispatchedSessions}
                    meetings={roundTableMeetings}
                    settings={settings}
                    onSelectAgent={(agent) => setOfficeInfoAgent(agent)}
                    onOpenKanbanSignal={(signal) =>
                      navigateToRoute("/kanban", {
                        kanbanFocus: signal,
                      })
                    }
                    onOpenDispatchSessions={() =>
                      navigateToRoute("/agents", { agentsTab: "dispatch" })
                    }
                    onOpenSettings={() => navigateToRoute("/settings")}
                    onRefreshMeetings={() =>
                      api
                        .getRoundTableMeetings()
                        .then(setRoundTableMeetings)
                        .catch(() => {})
                    }
                  />
                }
              />
              <Route
                path="/ops"
                element={
                  <OfficeManagerView
                    offices={offices}
                    allAgents={allAgents}
                    selectedOfficeId={selectedOfficeId}
                    isKo={isKo}
                    onChanged={handleOfficeChanged}
                  />
                }
              />
              <Route
                path="/meetings"
                element={
                  <MeetingMinutesView
                    meetings={roundTableMeetings}
                    onRefresh={() =>
                      api
                        .getRoundTableMeetings()
                        .then(setRoundTableMeetings)
                        .catch(() => {})
                    }
                    onNotify={pushNotification}
                    onUpdateNotification={updateNotification}
                  />
                }
              />
              <Route
                path="/achievements"
                element={
                  <DashboardPageView
                    key="dashboard-achievements"
                    stats={stats}
                    agents={agents}
                    sessions={visibleDispatchedSessions}
                    meetings={roundTableMeetings}
                    settings={settings}
                    requestedTab={"achievements" satisfies DashboardTab}
                    onSelectAgent={(agent) => setOfficeInfoAgent(agent)}
                    onOpenKanbanSignal={(signal) =>
                      navigateToRoute("/kanban", {
                        kanbanFocus: signal,
                      })
                    }
                    onOpenDispatchSessions={() =>
                      navigateToRoute("/agents", { agentsTab: "dispatch" })
                    }
                    onOpenSettings={() => navigateToRoute("/settings")}
                    onRefreshMeetings={() =>
                      api
                        .getRoundTableMeetings()
                        .then(setRoundTableMeetings)
                        .catch(() => {})
                    }
                  />
                }
              />
              <Route
                path="/settings"
                element={
                  <SettingsView
                    settings={settings}
                    onSave={handleSettingsSave}
                    isKo={isKo}
                  />
                }
              />
              {PRIMARY_ROUTES.flatMap((route) =>
                (route.aliases ?? []).map((alias) => (
                  <Route
                    key={`${route.id}:${alias}`}
                    path={alias}
                    element={<Navigate replace to={route.path} />}
                  />
                )),
              )}
              <Route
                path="*"
                element={<Navigate replace to={DEFAULT_ROUTE_PATH} />}
              />
            </Routes>
          </Suspense>
        </main>
      </div>

      {isMobileViewport && (
        <>
          <nav
            data-testid="app-mobile-tabbar"
            className="fixed bottom-0 left-0 right-0 z-[70] flex items-start justify-around border-t"
            style={{
              height: MOBILE_TABBAR_SAFE_AREA_HEIGHT,
              borderColor: "var(--th-border-subtle)",
              background:
                "linear-gradient(180deg, color-mix(in srgb, var(--th-nav-bg) 98%, black 2%) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
              paddingBottom: "env(safe-area-inset-bottom)",
              paddingLeft: "env(safe-area-inset-left)",
              paddingRight: "env(safe-area-inset-right)",
            }}
          >
            {mobilePrimaryRoutes.map((route) => {
              const Icon = iconForRoute(route.id);
              const isActive = activeMobileRouteId === route.id;
              return (
                <button
                  key={route.id}
                  type="button"
                  data-testid={`app-mobile-tab-${route.id}`}
                  onClick={() => navigateToRoute(route.path)}
                  className="relative flex h-14 flex-1 flex-col items-center justify-center gap-0.5 text-[10px] font-medium"
                  style={{
                    color: isActive
                      ? "var(--th-accent-primary)"
                      : "var(--th-text-muted)",
                  }}
                >
                  <Icon size={18} />
                  <span>{isKo ? route.labelKo : route.labelEn}</span>
                </button>
              );
            })}
            <button
              type="button"
              data-testid="app-mobile-more-button"
              onClick={() => setShowMobileMoreMenu((prev) => !prev)}
              className="relative flex h-14 flex-1 flex-col items-center justify-center gap-0.5 text-[10px] font-medium"
              style={{
                color:
                  activeMobileRouteId === "more"
                    ? "var(--th-accent-primary)"
                    : "var(--th-text-muted)",
              }}
            >
              <Menu size={18} />
              <span>{tr("더보기", "More")}</span>
              {(unresolvedMeetingsCount > 0 || unreadCount > 0) && (
                <span className="absolute right-[28%] top-1 flex h-4 min-w-4 items-center justify-center rounded-full bg-emerald-500 px-1 text-[8px] font-semibold text-white">
                  {unresolvedMeetingsCount + unreadCount > 9
                    ? "9+"
                    : unresolvedMeetingsCount + unreadCount}
                </span>
              )}
            </button>
          </nav>

          {showMobileMoreMenu && (
            <div
              className="fixed inset-0 z-[90] flex items-end justify-center"
              onClick={() => setShowMobileMoreMenu(false)}
            >
              <div className="absolute inset-0 bg-black/55 backdrop-blur-sm" />
              <div
                data-testid="app-mobile-more-menu"
                role="dialog"
                aria-modal="true"
                aria-label={tr("더보기 메뉴", "More menu")}
                className="relative w-full rounded-t-[32px] border px-4 pb-4 pt-3 shadow-2xl animate-in fade-in slide-in-from-bottom-4 duration-200"
                style={{
                  borderColor: "var(--th-border-subtle)",
                  background:
                    "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
                  paddingBottom:
                    "max(1rem, calc(1rem + env(safe-area-inset-bottom)))",
                }}
                onClick={(event) => event.stopPropagation()}
              >
                <div className="mx-auto mb-3 h-1.5 w-14 rounded-full bg-white/10" />
                <div className="mb-3 flex items-center justify-between gap-3">
                  <div>
                    <div
                      className="text-[11px] font-semibold uppercase tracking-[0.2em]"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      {tr("더보기", "More")}
                    </div>
                    <div
                      className="mt-1 text-base font-semibold"
                      style={{ color: "var(--th-text-heading)" }}
                    >
                      {tr("숨겨진 페이지 바로가기", "Jump to secondary pages")}
                    </div>
                  </div>
                  <button
                    type="button"
                    onClick={() => setShowMobileMoreMenu(false)}
                    className="flex h-10 w-10 items-center justify-center rounded-xl border text-[var(--th-text-muted)]"
                    style={{
                      borderColor:
                        "color-mix(in srgb, var(--th-border) 64%, transparent)",
                      background:
                        "color-mix(in srgb, var(--th-card-bg) 88%, transparent)",
                    }}
                    aria-label={tr("더보기 닫기", "Close more menu")}
                  >
                    <X size={16} />
                  </button>
                </div>

                <div className="grid gap-2">
                  {mobileMoreRoutes.map((route) => {
                    const Icon = iconForRoute(route.id);
                    const badge =
                      route.id === "meetings"
                        ? unresolvedMeetingsCount || undefined
                        : route.id === "settings"
                          ? unreadCount || undefined
                          : undefined;
                    return (
                      <button
                        key={route.id}
                        type="button"
                        onClick={() =>
                          navigateToRoute(
                            route.path,
                            route.id === "agents"
                              ? { agentsTab: "agents" }
                              : undefined,
                          )
                        }
                        className="flex items-start gap-3 rounded-2xl border px-3 py-3 text-left"
                        style={{
                          borderColor:
                            "color-mix(in srgb, var(--th-border) 70%, transparent)",
                          background:
                            "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
                        }}
                      >
                        <span className="flex h-10 w-10 items-center justify-center rounded-2xl bg-[var(--th-overlay-subtle)]">
                          <Icon size={18} />
                        </span>
                        <span className="min-w-0 flex-1">
                          <span
                            className="flex items-center gap-2 text-sm font-semibold"
                            style={{ color: "var(--th-text-heading)" }}
                          >
                            {isKo ? route.labelKo : route.labelEn}
                            {badge !== undefined && badge > 0 && (
                              <span className="inline-flex h-5 min-w-5 items-center justify-center rounded-full bg-emerald-500 px-1.5 text-[10px] text-white">
                                {badge > 9 ? "9+" : badge}
                              </span>
                            )}
                          </span>
                          <span
                            className="mt-1 block text-xs leading-relaxed"
                            style={{ color: "var(--th-text-muted)" }}
                          >
                            {isKo ? route.descriptionKo : route.descriptionEn}
                          </span>
                        </span>
                      </button>
                    );
                  })}
                </div>
              </div>
            </div>
          )}
        </>
      )}

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
        {showCommandPalette && (
          <CommandPalette
            agents={allAgents}
            departments={departments}
            isKo={isKo}
            onSelectAgent={(agent) => setOfficeInfoAgent(agent)}
            onNavigate={(path) => navigateToRoute(path)}
            onClose={() => setShowCommandPalette(false)}
            routes={PALETTE_ROUTES}
            departmentRouteId="/agents"
          />
        )}
      </Suspense>

      <ToastOverlay notifications={notifications} onDismiss={dismissNotification} />

      {showShortcutHelp && (
        <ShortcutHelpModal
          isKo={isKo}
          onClose={() => setShowShortcutHelp(false)}
        />
      )}

      {!wsConnected && (
        <div className="pointer-events-none fixed left-4 right-4 top-4 z-[95] flex justify-center md:left-auto md:right-6">
          <div className="flex items-center gap-2 rounded-full border border-red-500/30 bg-red-500/15 px-4 py-2 text-xs text-red-300 shadow-lg">
            <WifiOff size={12} />
            <span>
              {tr(
                "서버 연결 끊김, 재연결 중입니다.",
                "Server disconnected, reconnecting.",
              )}
            </span>
          </div>
        </div>
      )}
    </div>
  );
}

function SidebarRouteButton({
  route,
  currentRouteId,
  collapsed,
  isKo,
  badge,
  onNavigate,
}: {
  route: AppRouteEntry;
  currentRouteId: AppRouteId | null;
  collapsed: boolean;
  isKo: boolean;
  badge?: number;
  onNavigate: () => void;
}) {
  const active = currentRouteId === route.id;
  const Icon = iconForRoute(route.id);

  return (
    <button
      type="button"
      onClick={onNavigate}
      className={`group relative flex w-full items-center gap-3 rounded-2xl px-3 py-3 text-left transition-colors ${
        collapsed ? "justify-center" : ""
      }`}
      style={{
        background: active
          ? "color-mix(in srgb, var(--th-accent-primary-soft) 80%, transparent)"
          : "transparent",
        color: active ? "var(--th-text-primary)" : "var(--th-text-secondary)",
      }}
      title={collapsed ? (isKo ? route.labelKo : route.labelEn) : undefined}
    >
      <span
        className="flex h-10 w-10 shrink-0 items-center justify-center rounded-2xl"
        style={{
          background: active
            ? "color-mix(in srgb, var(--th-accent-primary) 18%, transparent)"
            : "var(--th-overlay-subtle)",
        }}
      >
        <Icon size={18} />
      </span>
      {!collapsed && (
        <span className="min-w-0 flex-1">
          <span className="block truncate text-sm font-medium">
            {isKo ? route.labelKo : route.labelEn}
          </span>
          <span
            className="mt-0.5 block truncate text-[11px]"
            style={{ color: "var(--th-text-muted)" }}
          >
            {isKo ? route.descriptionKo : route.descriptionEn}
          </span>
        </span>
      )}
      {badge !== undefined && badge > 0 && (
        <span className="absolute right-3 top-3 flex h-5 min-w-5 items-center justify-center rounded-full bg-emerald-500 px-1 text-[10px] font-semibold text-white">
          {badge > 9 ? "9+" : badge}
        </span>
      )}
    </button>
  );
}

function NotificationSummaryRow({
  label,
  value,
  accent,
}: {
  label: string;
  value: number;
  accent: string;
}) {
  return (
    <div
      className="flex items-center justify-between rounded-2xl border px-3 py-2 text-sm"
      style={{
        borderColor: "var(--th-border-subtle)",
        background: "var(--th-overlay-subtle)",
      }}
    >
      <span style={{ color: "var(--th-text-muted)" }}>{label}</span>
      <span className="font-semibold" style={{ color: accent }}>
        {value}
      </span>
    </div>
  );
}

function HomeOverviewPage({
  isKo,
  currentOfficeLabel,
  stats,
  meetings,
  notifications,
  kanbanCards,
}: {
  isKo: boolean;
  currentOfficeLabel: string;
  stats: DashboardStats | null;
  meetings: RoundTableMeeting[];
  notifications: Notification[];
  kanbanCards: KanbanCard[];
}) {
  const tr = useCallback((ko: string, en: string) => (isKo ? ko : en), [isKo]);
  const quickRoutes = PRIMARY_ROUTES.filter((route) =>
    ["office", "kanban", "stats", "meetings", "settings"].includes(route.id),
  );
  const outstandingMeetings = meetings.filter(hasUnresolvedMeetingIssues).length;
  const liveNotifications = notifications.filter(
    (notification) => Date.now() - notification.ts < 60_000,
  ).length;
  const requestedCards = kanbanCards.filter((card) => card.status === "requested").length;
  const inProgressCards = kanbanCards.filter(
    (card) => card.status === "in_progress" || card.status === "review",
  ).length;

  return (
    <div className="mx-auto h-full w-full max-w-6xl overflow-auto px-4 py-5 pb-32 sm:px-6">
      <div
        className="rounded-[2rem] border p-5 sm:p-6"
        style={{
          borderColor: "var(--th-border-subtle)",
          background:
            "radial-gradient(circle at top left, rgba(110,242,163,0.16) 0%, transparent 35%), linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 94%, transparent) 100%)",
        }}
      >
        <div className="flex flex-wrap items-start justify-between gap-4">
          <div className="max-w-2xl">
            <div
              className="text-[11px] font-semibold uppercase tracking-[0.2em]"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr("오늘의 개요", "Today's overview")}
            </div>
            <h1
              className="mt-3 text-3xl font-semibold tracking-tight sm:text-4xl"
              style={{ color: "var(--th-text-heading)" }}
            >
              {tr("한 번에 운영 흐름을 정리하는 홈", "A home page for the whole operating flow")}
            </h1>
            <p
              className="mt-3 max-w-2xl text-sm leading-7 sm:text-base"
              style={{ color: "var(--th-text-secondary)" }}
            >
              {tr(
                `현재 범위는 ${currentOfficeLabel} 기준입니다. 오피스, 칸반, 회의, 설정까지 새 앱 셸에서 바로 이동할 수 있습니다.`,
                `The current scope is ${currentOfficeLabel}. Jump straight into office, kanban, meetings, and settings from the new shell.`,
              )}
            </p>
          </div>

          <div className="grid w-full gap-3 sm:w-auto sm:min-w-[19rem]">
            <MetricCard
              title={tr("활성 에이전트", "Active agents")}
              value={stats?.agents.working ?? 0}
              detail={tr("현재 작업 중", "Currently working")}
              tone="emerald"
            />
            <MetricCard
              title={tr("오픈 워크", "Open work")}
              value={requestedCards + inProgressCards}
              detail={tr("요청 + 진행 + 리뷰", "Requested + in progress + review")}
              tone="sky"
            />
          </div>
        </div>
      </div>

      <div className="mt-5 grid gap-4 md:grid-cols-2 xl:grid-cols-4">
        <MetricCard
          title={tr("전체 에이전트", "Total agents")}
          value={stats?.agents.total ?? 0}
          detail={tr("워크스페이스 등록 수", "Registered in workspace")}
          tone="neutral"
        />
        <MetricCard
          title={tr("실시간 세션", "Live sessions")}
          value={stats?.dispatched_count ?? 0}
          detail={tr("연결 유지 중", "Currently connected")}
          tone="sky"
        />
        <MetricCard
          title={tr("미해결 회의", "Open meetings")}
          value={outstandingMeetings}
          detail={tr("후속 이슈 확인 필요", "Need follow-up review")}
          tone="amber"
        />
        <MetricCard
          title={tr("최근 알림", "Recent alerts")}
          value={liveNotifications}
          detail={tr("최근 1분 기준", "Within the last minute")}
          tone="rose"
        />
      </div>

      <div className="mt-5 grid gap-4 lg:grid-cols-[1.25fr_0.75fr]">
        <div
          className="rounded-[1.75rem] border p-4 sm:p-5"
          style={{
            borderColor: "var(--th-border-subtle)",
            background: "var(--th-card-bg)",
          }}
        >
          <div className="flex items-center justify-between gap-3">
            <div>
              <div
                className="text-sm font-semibold"
                style={{ color: "var(--th-text-heading)" }}
              >
                {tr("빠른 진입", "Quick access")}
              </div>
              <div
                className="mt-1 text-sm"
                style={{ color: "var(--th-text-muted)" }}
              >
                {tr("우선 작업 영역으로 바로 이동합니다.", "Jump to the surfaces you need next.")}
              </div>
            </div>
            <LayoutDashboard size={18} style={{ color: "var(--th-text-muted)" }} />
          </div>

          <div className="mt-4 grid gap-3 md:grid-cols-2">
            {quickRoutes.map((route) => (
              <Link
                key={route.id}
                to={route.path}
                className="rounded-[1.5rem] border p-4 transition-transform hover:-translate-y-0.5"
                style={{
                  borderColor: "var(--th-border-subtle)",
                  background: "color-mix(in srgb, var(--th-bg-surface) 92%, transparent)",
                }}
              >
                <div
                  className="text-sm font-semibold"
                  style={{ color: "var(--th-text-primary)" }}
                >
                  {isKo ? route.labelKo : route.labelEn}
                </div>
                <div
                  className="mt-2 text-sm leading-6"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {isKo ? route.descriptionKo : route.descriptionEn}
                </div>
              </Link>
            ))}
          </div>
        </div>

        <div
          className="rounded-[1.75rem] border p-4 sm:p-5"
          style={{
            borderColor: "var(--th-border-subtle)",
            background: "var(--th-card-bg)",
          }}
        >
          <div
            className="text-sm font-semibold"
            style={{ color: "var(--th-text-heading)" }}
          >
            {tr("워크 큐", "Work queue")}
          </div>
          <div
            className="mt-1 text-sm"
            style={{ color: "var(--th-text-muted)" }}
          >
            {tr("현재 카드 상태를 요약합니다.", "A snapshot of current card status.")}
          </div>

          <div className="mt-4 space-y-3">
            <QueueRow
              label={tr("요청됨", "Requested")}
              value={requestedCards}
              accent="#66b3ff"
            />
            <QueueRow
              label={tr("진행/리뷰", "In progress / review")}
              value={inProgressCards}
              accent="#6ef2a3"
            />
            <QueueRow
              label={tr("완료", "Done")}
              value={kanbanCards.filter((card) => card.status === "done").length}
              accent="#f5bd47"
            />
          </div>
        </div>
      </div>
    </div>
  );
}

function MetricCard({
  title,
  value,
  detail,
  tone,
}: {
  title: string;
  value: number;
  detail: string;
  tone: "neutral" | "emerald" | "sky" | "amber" | "rose";
}) {
  const theme = metricToneTheme(tone);
  return (
    <div
      className="rounded-[1.5rem] border p-4"
      style={{
        borderColor: theme.border,
        background: theme.background,
      }}
    >
      <div className="text-sm font-medium" style={{ color: "var(--th-text-muted)" }}>
        {title}
      </div>
      <div
        className="mt-3 text-3xl font-semibold tracking-tight"
        style={{ color: "var(--th-text-heading)" }}
      >
        {value}
      </div>
      <div className="mt-2 text-sm" style={{ color: theme.detail }}>
        {detail}
      </div>
    </div>
  );
}

function QueueRow({
  label,
  value,
  accent,
}: {
  label: string;
  value: number;
  accent: string;
}) {
  return (
    <div
      className="flex items-center justify-between rounded-2xl border px-3 py-3"
      style={{
        borderColor: "var(--th-border-subtle)",
        background: "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)",
      }}
    >
      <span style={{ color: "var(--th-text-secondary)" }}>{label}</span>
      <span className="font-semibold" style={{ color: accent }}>
        {value}
      </span>
    </div>
  );
}

function ShortcutHelpModal({
  isKo,
  onClose,
}: {
  isKo: boolean;
  onClose: () => void;
}) {
  return (
    <div className="fixed inset-0 z-[100] flex items-center justify-center px-4" onClick={onClose}>
      <div className="fixed inset-0 bg-black/50 backdrop-blur-sm" />
      <div
        role="dialog"
        aria-modal="true"
        className="relative w-full max-w-md rounded-[2rem] border p-6 shadow-2xl"
        style={{
          borderColor: "var(--th-border-subtle)",
          background:
            "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 95%, transparent) 100%)",
        }}
        onClick={(event) => event.stopPropagation()}
      >
        <div className="flex items-center justify-between">
          <div>
            <div
              className="text-lg font-semibold"
              style={{ color: "var(--th-text-heading)" }}
            >
              {isKo ? "키보드 단축키" : "Keyboard Shortcuts"}
            </div>
            <div className="mt-1 text-sm" style={{ color: "var(--th-text-muted)" }}>
              {isKo ? "새 라우팅 셸 기준" : "For the new route shell"}
            </div>
          </div>
          <button
            type="button"
            onClick={onClose}
            className="flex h-9 w-9 items-center justify-center rounded-xl text-[var(--th-text-muted)]"
          >
            <X size={16} />
          </button>
        </div>

        <div className="mt-5 space-y-3 text-sm">
          <ShortcutRow
            label={isKo ? "명령 팔레트" : "Command palette"}
            combo="⌘K"
          />
          <ShortcutRow label={isKo ? "도움말" : "Help"} combo="?" />
          {PRIMARY_ROUTES.map((route) => (
            <ShortcutRow
              key={route.id}
              label={isKo ? route.labelKo : route.labelEn}
              combo={`Alt+${route.shortcutKey}`}
            />
          ))}
        </div>
      </div>
    </div>
  );
}

function ShortcutRow({ label, combo }: { label: string; combo: string }) {
  return (
    <div className="flex items-center justify-between rounded-2xl border px-3 py-2" style={{ borderColor: "var(--th-border-subtle)" }}>
      <span style={{ color: "var(--th-text-secondary)" }}>{label}</span>
      <kbd
        className="rounded-lg px-2 py-1 text-xs"
        style={{
          background: "var(--th-overlay-subtle)",
          color: "var(--th-text-primary)",
        }}
      >
        {combo}
      </kbd>
    </div>
  );
}

function ViewSkeleton({ label }: { label: string }) {
  return (
    <div className="flex h-full items-center justify-center">
      <div className="text-center">
        <div className="text-3xl opacity-40">🐾</div>
        <div className="mt-3 text-sm" style={{ color: "var(--th-text-muted)" }}>
          {label}
        </div>
      </div>
    </div>
  );
}

function hasUnresolvedMeetingIssues(meeting: RoundTableMeeting): boolean {
  const totalIssues = meeting.proposed_issues?.length ?? 0;
  if (meeting.status !== "completed" || totalIssues === 0) return false;

  const results = meeting.issue_creation_results ?? [];
  if (results.length === 0) {
    return meeting.issues_created < totalIssues;
  }

  const created = results.filter(
    (result) => result.ok && result.discarded !== true,
  ).length;
  const failed = results.filter(
    (result) => !result.ok && result.discarded !== true,
  ).length;
  const discarded = results.filter((result) => result.discarded === true).length;
  const pending = Math.max(totalIssues - created - failed - discarded, 0);

  return pending > 0 || failed > 0;
}

function notificationColor(type: Notification["type"]): string {
  switch (type) {
    case "success":
      return "#34d399";
    case "warning":
      return "#fbbf24";
    case "error":
      return "#f87171";
    default:
      return "#60a5fa";
  }
}

function formatRelativeTime(timestamp: number, isKo: boolean): string {
  const diffMs = Date.now() - timestamp;
  const seconds = Math.max(1, Math.floor(diffMs / 1000));
  if (seconds < 60) return isKo ? `${seconds}초 전` : `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return isKo ? `${minutes}분 전` : `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return isKo ? `${hours}시간 전` : `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return isKo ? `${days}일 전` : `${days}d ago`;
}

function selectedOfficeLabel(
  offices: { id: string; name: string; name_ko: string }[],
  selectedOfficeId: string | null,
  tr: (ko: string, en: string) => string,
): string {
  if (!selectedOfficeId) return tr("전체", "All");
  const office = offices.find((candidate) => candidate.id === selectedOfficeId);
  if (!office) return selectedOfficeId;
  return office.name_ko || office.name;
}

function iconForRoute(routeId: AppRouteId) {
  switch (routeId) {
    case "home":
      return Home;
    case "office":
      return Building2;
    case "agents":
      return Users;
    case "kanban":
      return FolderKanban;
    case "stats":
      return LayoutDashboard;
    case "ops":
      return Wrench;
    case "meetings":
      return Bell;
    case "achievements":
      return Trophy;
    case "settings":
      return Settings;
  }
}

function metricToneTheme(tone: "neutral" | "emerald" | "sky" | "amber" | "rose") {
  switch (tone) {
    case "emerald":
      return {
        border: "rgba(16, 185, 129, 0.24)",
        background: "rgba(16, 185, 129, 0.09)",
        detail: "#6ef2a3",
      };
    case "sky":
      return {
        border: "rgba(96, 165, 250, 0.24)",
        background: "rgba(96, 165, 250, 0.09)",
        detail: "#93c5fd",
      };
    case "amber":
      return {
        border: "rgba(245, 189, 71, 0.24)",
        background: "rgba(245, 189, 71, 0.08)",
        detail: "#f5bd47",
      };
    case "rose":
      return {
        border: "rgba(244, 114, 182, 0.22)",
        background: "rgba(244, 114, 182, 0.08)",
        detail: "#f472b6",
      };
    default:
      return {
        border: "var(--th-border-subtle)",
        background: "color-mix(in srgb, var(--th-bg-surface) 92%, transparent)",
        detail: "var(--th-text-muted)",
      };
  }
}
