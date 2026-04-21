import { lazy, Suspense, useCallback, useEffect, useMemo, useState } from "react";
import {
  Bell,
  BellRing,
  Building2,
  ChevronRight,
  Flame,
  FolderKanban,
  GripVertical,
  Home,
  LayoutDashboard,
  Menu,
  Moon,
  Sun,
  Search,
  Settings,
  Sparkles,
  Target,
  Trophy,
  Users,
  WifiOff,
  Wrench,
  X,
  Zap,
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
  TokenAnalyticsResponse,
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
import { deriveOfficeAgentState } from "../components/office-view/officeAgentState";
import OfficeSelectorBar from "../components/OfficeSelectorBar";
import { MOBILE_LAYOUT_MEDIA_QUERY } from "./breakpoints";
import {
  DEFAULT_ROUTE_PATH,
  PALETTE_ROUTES,
  PRIMARY_ROUTES,
  findRouteByPath,
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
import { STORAGE_KEYS } from "../lib/storageKeys";
import { useLocalStorage } from "../lib/useLocalStorage";
import {
  DailyMissions,
  LevelRing,
  StreakCounter,
  getAgentLevelFromXp,
  getMissionResetCountdown,
  getMissionTotalXp,
  type DailyMissionViewModel,
} from "../components/gamification/GamificationShared";

const OfficeView = lazy(() => import("../components/OfficeView"));
const AchievementsPage = lazy(() => import("../components/AchievementsPage"));
const StatsPageView = lazy(() => import("../components/StatsPageView"));
const OpsPageView = lazy(() => import("../components/OpsPageView"));
const KanbanTab = lazy(() => import("../components/agent-manager/KanbanTab"));
const AgentManagerView = lazy(() => import("../components/AgentManagerView"));
const OfficeManagerView = lazy(() => import("../components/OfficeManagerView"));
const MeetingsAndSkillsPage = lazy(() => import("../components/MeetingsAndSkillsPage"));
const SettingsView = lazy(() => import("../components/SettingsView"));
const AgentInfoCard = lazy(() => import("../components/agent-manager/AgentInfoCard"));
const OfficeAgentDrawer = lazy(() => import("../components/office-view/OfficeAgentDrawer"));
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

type AgentsPageTab = "agents" | "departments" | "backlog" | "dispatch";
type KanbanSignalFocus = "review" | "blocked" | "requested" | "stalled";

const MOBILE_TABBAR_SAFE_AREA_HEIGHT = "calc(3.5rem + env(safe-area-inset-bottom))";
const HOME_DEFAULT_WIDGETS = [
  "m_tokens",
  "m_cost",
  "m_progress",
  "m_streak",
  "office",
  "missions",
  "roster",
  "activity",
  "kanban",
];
const MOBILE_PRIMARY_ROUTE_IDS: AppRouteId[] = [
  "home",
  "office",
  "kanban",
  "stats",
];
const SIDEBAR_SECTION_ORDER: Array<{
  id: "workspace" | "extensions" | "me";
  labelKo: string;
  labelEn: string;
}> = [
  {
    id: "workspace",
    labelKo: "워크스페이스",
    labelEn: "Workspace",
  },
  {
    id: "extensions",
    labelKo: "확장",
    labelEn: "Extensions",
  },
  {
    id: "me",
    labelKo: "나",
    labelEn: "Me",
  },
];
// Keep persistent shell chrome below route-level backdrops and modals.
const ROUTE_OVERLAY_BASE_Z_INDEX = 50;
const SHELL_HEADER_Z_INDEX = ROUTE_OVERLAY_BASE_Z_INDEX - 30;
const SHELL_POPOVER_Z_INDEX = ROUTE_OVERLAY_BASE_Z_INDEX - 10;
const SHELL_TABBAR_Z_INDEX = ROUTE_OVERLAY_BASE_Z_INDEX - 20;
const SHELL_BOTTOM_SHEET_Z_INDEX = ROUTE_OVERLAY_BASE_Z_INDEX - 5;
const SHELL_TOAST_Z_INDEX = 95;
const SHELL_MODAL_Z_INDEX = 100;
const OPERATOR_LEVEL_TITLES_KO = [
  "신입",
  "수습",
  "사원",
  "주임",
  "대리",
  "과장",
  "차장",
  "부장",
  "이사",
  "사장",
];
const OPERATOR_LEVEL_TITLES_EN = [
  "Newbie",
  "Trainee",
  "Staff",
  "Associate",
  "Sr. Associate",
  "Manager",
  "Asst. Dir.",
  "Director",
  "VP",
  "President",
];

function areStringArraysEqual(left: readonly string[], right: readonly string[]) {
  if (left.length !== right.length) return false;
  return left.every((value, index) => value === right[index]);
}

function normalizeHomeWidgetOrder(
  value: unknown,
  defaults: readonly string[] = HOME_DEFAULT_WIDGETS,
) {
  const allowed = new Set(defaults);
  const normalized: string[] = [];
  if (Array.isArray(value)) {
    value.forEach((entry) => {
      if (typeof entry !== "string" || !allowed.has(entry) || normalized.includes(entry)) {
        return;
      }
      normalized.push(entry);
    });
  }
  defaults.forEach((widgetId) => {
    if (!normalized.includes(widgetId)) {
      normalized.push(widgetId);
    }
  });
  return normalized;
}

function getOperatorLevelTitle(level: number, isKo: boolean) {
  const titles = isKo ? OPERATOR_LEVEL_TITLES_KO : OPERATOR_LEVEL_TITLES_EN;
  const index = Math.max(0, Math.min(level - 1, titles.length - 1));
  return titles[index];
}

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
  const [officeInfoMode, setOfficeInfoMode] = useState<"default" | "office">(
    "default",
  );
  const [showCommandPalette, setShowCommandPalette] = useState(false);
  const [showShortcutHelp, setShowShortcutHelp] = useState(false);
  const [showNotificationPanel, setShowNotificationPanel] = useState(false);
  const [showTweaksPanel, setShowTweaksPanel] = useState(false);
  const [showMobileMoreMenu, setShowMobileMoreMenu] = useState(false);
  const [agentsPageTab, setAgentsPageTab] = useState<AgentsPageTab>("agents");
  const [kanbanSignalFocus, setKanbanSignalFocus] =
    useState<KanbanSignalFocus | null>(null);
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
  const officeAgentState = useMemo(
    () => deriveOfficeAgentState(agentsWithDispatched, kanbanCards),
    [agentsWithDispatched, kanbanCards],
  );
  const unresolvedMeetingsCount = roundTableMeetings.filter(
    hasUnresolvedMeetingIssues,
  ).length;
  const unreadCount = notifications.filter(
    (notification) => Date.now() - notification.ts < 60_000,
  ).length;
  const kanbanBadgeCount = kanbanCards.filter(
    (card) =>
      card.status === "requested" ||
      card.status === "in_progress" ||
      card.status === "review" ||
      card.status === "blocked",
  ).length;
  const notificationBadgeCount = unresolvedMeetingsCount + unreadCount;
  const resolvedTheme = useMemo(
    () => resolveThemePreference(themePreference, prefersDarkScheme),
    [prefersDarkScheme, themePreference],
  );
  const recentNotifications = notifications.slice(0, 6);
  const currentOfficeName = useMemo(
    () => selectedOfficeLabel(offices, selectedOfficeId, tr),
    [offices, selectedOfficeId, tr],
  );
  const currentUserXp = useMemo(() => {
    if (stats?.top_agents?.length) {
      const samples = stats.top_agents.slice(0, 3);
      return Math.round(
        samples.reduce((sum, agent) => sum + agent.stats_xp, 0) /
          Math.max(samples.length, 1),
      );
    }
    if (agentsWithDispatched.length === 0) {
      return 0;
    }
    const rankedAgents = [...agentsWithDispatched]
      .sort((left, right) => right.stats_xp - left.stats_xp)
      .slice(0, 3);
    return Math.round(
      rankedAgents.reduce((sum, agent) => sum + agent.stats_xp, 0) /
        Math.max(rankedAgents.length, 1),
    );
  }, [agentsWithDispatched, stats?.top_agents]);
  const currentUserLevel = useMemo(
    () => getAgentLevelFromXp(currentUserXp),
    [currentUserXp],
  );
  const currentUserLabel = "you";
  const currentUserDetail = `lv.${currentUserLevel.level} · ${getOperatorLevelTitle(
    currentUserLevel.level,
    isKo,
  )}`;

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
    setShowTweaksPanel(false);
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

  const openDefaultAgentInfo = useCallback((agent: Agent) => {
    setOfficeInfoMode("default");
    setOfficeInfoAgent(agent);
  }, []);

  const openOfficeAgentInfo = useCallback((agent: Agent) => {
    setOfficeInfoMode("office");
    setOfficeInfoAgent(agent);
  }, []);

  const closeOfficeInfo = useCallback(() => {
    setOfficeInfoAgent(null);
    setOfficeInfoMode("default");
  }, []);

  const toggleShellTheme = useCallback(() => {
    setThemePreference((currentPreference) => {
      const activeTheme =
        currentPreference === "auto" ? resolvedTheme : currentPreference;
      return activeTheme === "dark" ? "light" : "dark";
    });
  }, [resolvedTheme]);

  useEffect(() => {
    if (officeInfoMode === "office" && currentRoute?.id !== "office") {
      closeOfficeInfo();
    }
  }, [closeOfficeInfo, currentRoute?.id, officeInfoMode]);

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

  const mobilePrimaryRoutes = useMemo(
    () =>
      MOBILE_PRIMARY_ROUTE_IDS.map((routeId) =>
        PRIMARY_ROUTES.find((route) => route.id === routeId),
      ).filter((route): route is AppRouteEntry => route !== undefined),
    [],
  );
  const mobileOverflowSections = useMemo(
    () =>
      SIDEBAR_SECTION_ORDER.map((section) => ({
        ...section,
        routes: PRIMARY_ROUTES.filter(
          (route) =>
            route.section === section.id &&
            !MOBILE_PRIMARY_ROUTE_IDS.includes(route.id),
        ),
      })).filter((section) => section.routes.length > 0),
    [],
  );
  const activeMobileRouteId =
    showMobileMoreMenu ||
    (currentRoute && !MOBILE_PRIMARY_ROUTE_IDS.includes(currentRoute.id))
      ? "more"
      : currentRoute?.id ?? "home";
  const sidebarBadgeForRoute = useCallback(
    (routeId: AppRouteId): number | undefined => {
      switch (routeId) {
        case "kanban":
          return kanbanBadgeCount || undefined;
        case "meetings":
          return unresolvedMeetingsCount || undefined;
        case "settings":
          return unreadCount || undefined;
        default:
          return undefined;
      }
    },
    [kanbanBadgeCount, unreadCount, unresolvedMeetingsCount],
  );

  return (
    <div
      className="fixed inset-0 flex overflow-hidden"
      style={{
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-bg-primary) 98%, black 2%) 0%, var(--th-bg-primary) 100%)",
      }}
    >
      {!isMobileViewport && (
        <aside
          data-testid="app-sidebar-nav"
          className="flex w-[236px] shrink-0 flex-col border-r"
          style={{
            borderColor: "var(--th-border-subtle)",
            background:
              "linear-gradient(180deg, color-mix(in srgb, var(--th-nav-bg) 98%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 92%, transparent) 100%)",
          }}
        >
          <div
            className="border-b px-4 py-5"
            style={{ borderColor: "var(--th-border-subtle)" }}
          >
            <div className="flex items-center gap-3">
              <div
                className="flex h-11 w-11 items-center justify-center rounded-2xl text-sm font-semibold"
                style={{
                  background: "var(--th-accent-primary-soft)",
                  color: "var(--th-accent-primary)",
                }}
              >
                AD
              </div>
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
                  v2.4.1
                </div>
              </div>
            </div>
          </div>

          <div className="flex-1 overflow-y-auto px-3 py-4">
            {SIDEBAR_SECTION_ORDER.map((section) => {
              const routes = PRIMARY_ROUTES.filter(
                (route) => route.section === section.id,
              );
              return (
                <div key={section.id} className="mb-5">
                  <div
                    className="px-3 pb-2 text-[11px] font-semibold uppercase tracking-[0.18em]"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {isKo ? section.labelKo : section.labelEn}
                  </div>
                  <div className="space-y-1">
                    {routes.map((route) => (
                      <SidebarRouteButton
                        key={route.id}
                        route={route}
                        currentRouteId={currentRoute?.id ?? null}
                        isKo={isKo}
                        badge={sidebarBadgeForRoute(route.id)}
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
            className="border-t px-3 py-4"
            style={{ borderColor: "var(--th-border-subtle)" }}
          >
            <div className="space-y-2">
              <div
                className="flex items-center gap-2 rounded-2xl border px-3 py-2"
                style={{
                  borderColor: "var(--th-border-subtle)",
                  background: "var(--th-overlay-subtle)",
                  color: "var(--th-text-muted)",
                }}
              >
                <span
                  className={`h-2 w-2 rounded-full ${wsConnected ? "animate-pulse" : ""}`}
                  style={{
                    background: wsConnected ? "var(--th-accent-success)" : "var(--th-accent-danger)",
                  }}
                />
                <span className="font-mono text-[11px]">
                  {wsConnected
                    ? tr("2/2 providers", "2/2 providers")
                    : tr("0/2 providers", "0/2 providers")}
                </span>
              </div>

              <div
                className="flex items-center gap-3 rounded-2xl border px-3 py-3"
                style={{
                  borderColor: "var(--th-border-subtle)",
                  background:
                    "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
                }}
              >
                <div className="shrink-0">
                  <LevelRing
                    dataTestId="sidebar-user-level-ring"
                    value={Math.round(currentUserLevel.progress * 100)}
                    size={40}
                    stroke={3}
                    color="var(--th-accent-primary)"
                    trackColor="color-mix(in srgb, var(--th-overlay-medium) 86%, transparent)"
                  >
                    <div
                      className="flex h-8 w-8 items-center justify-center rounded-full text-[11px] font-semibold"
                      style={{
                        background: "var(--th-overlay-subtle)",
                        color: "var(--th-text-primary)",
                      }}
                    >
                      AD
                    </div>
                  </LevelRing>
                </div>
                <div className="min-w-0">
                  <div
                    className="truncate text-sm font-medium"
                    style={{ color: "var(--th-text-heading)" }}
                  >
                    {currentUserLabel}
                  </div>
                  <div
                    className="truncate text-[11px]"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {currentUserDetail}
                  </div>
                </div>
              </div>
            </div>
          </div>
        </aside>
      )}

      <div className="flex min-w-0 flex-1 flex-col overflow-hidden">
        <header
          data-testid="topbar"
          className="relative shrink-0 border-b px-4 py-2.5 sm:px-5"
          style={{
            zIndex: SHELL_HEADER_Z_INDEX,
            borderColor: "var(--th-border-subtle)",
            background:
              "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 93%, transparent) 100%)",
            backdropFilter: "blur(14px)",
          }}
        >
          <div className="flex flex-wrap items-center gap-2 sm:flex-nowrap">
            <div className="min-w-0 flex-1">
              <div
                className="flex items-center gap-2 text-[12px] font-medium"
                style={{ color: "var(--th-text-muted)" }}
              >
                <span>AgentDesk</span>
                <ChevronRight size={12} />
                <span>{currentRoute ? (isKo ? currentRoute.labelKo : currentRoute.labelEn) : (isKo ? "홈" : "Home")}</span>
              </div>
            </div>

            <label
              data-testid="topbar-search"
              className="order-3 flex min-w-[11rem] flex-1 items-center gap-2 rounded-2xl border px-3 py-2 text-sm sm:order-none sm:max-w-[18rem]"
              style={{
                borderColor: "var(--th-border-subtle)",
                background:
                  "color-mix(in srgb, var(--th-bg-surface) 82%, transparent)",
              }}
            >
              <Search size={15} style={{ color: "var(--th-text-muted)" }} />
              <input
                type="search"
                readOnly
                value=""
                onFocus={() => setShowCommandPalette(true)}
                onClick={() => setShowCommandPalette(true)}
                placeholder={tr(
                  "검색…",
                  "Search…",
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

            <div className="ml-auto flex flex-wrap items-center justify-end gap-2 sm:ml-0">
              <button
                type="button"
                onClick={toggleShellTheme}
                className="flex h-9 w-9 items-center justify-center rounded-2xl border transition-colors hover:bg-white/5"
                style={{ borderColor: "var(--th-border-subtle)" }}
                aria-label={tr(
                  resolvedTheme === "dark"
                    ? "라이트 테마로 전환"
                    : "다크 테마로 전환",
                  resolvedTheme === "dark"
                    ? "Switch to light theme"
                    : "Switch to dark theme",
                )}
                title={tr(
                  resolvedTheme === "dark"
                    ? "라이트 테마로 전환"
                    : "다크 테마로 전환",
                  resolvedTheme === "dark"
                    ? "Switch to light theme"
                    : "Switch to dark theme",
                )}
              >
                {resolvedTheme === "dark" ? (
                  <Sun size={18} />
                ) : (
                  <Moon size={18} />
                )}
              </button>

              <div className="relative">
                <button
                  type="button"
                  onClick={() =>
                    setShowNotificationPanel((prev) => {
                      if (!prev) setShowTweaksPanel(false);
                      return !prev;
                    })
                  }
                  className="relative flex h-9 w-9 items-center justify-center rounded-2xl border transition-colors hover:bg-white/5"
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
                    className="absolute right-0 top-12 w-[min(22rem,calc(100vw-2rem))] rounded-3xl border p-3 shadow-2xl"
                    style={{
                      zIndex: SHELL_POPOVER_Z_INDEX,
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
                onClick={() =>
                  setShowTweaksPanel((prev) => {
                    if (!prev) setShowNotificationPanel(false);
                    return !prev;
                  })
                }
                className="flex h-9 w-9 items-center justify-center rounded-2xl border transition-colors hover:bg-white/5"
                style={{ borderColor: "var(--th-border-subtle)" }}
                aria-label={tr("디자인 설정 열기", "Open tweaks")}
                title={tr("디자인 설정 열기", "Open tweaks")}
              >
                <Settings size={18} />
              </button>
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
                    isMobileViewport={isMobileViewport}
                    isKo={isKo}
                    wsConnected={wsConnected}
                    currentOfficeLabel={currentOfficeName}
                    stats={stats}
                    agents={agentsWithDispatched}
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
                    onSelectAgent={openOfficeAgentInfo}
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
                    kanbanCards={kanbanCards}
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
                    onSelectAgent={openDefaultAgentInfo}
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
                  <StatsPageView
                    settings={settings}
                    stats={stats}
                    agents={allAgents}
                    sessions={visibleDispatchedSessions}
                    meetings={roundTableMeetings}
                  />
                }
              />
              <Route
                path="/ops"
                element={
                  <OpsPageView
                    wsConnected={wsConnected}
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
                  <MeetingsAndSkillsPage
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
                  <AchievementsPage
                    key="achievements"
                    settings={settings}
                    stats={stats}
                    agents={allAgents}
                    onSelectAgent={openDefaultAgentInfo}
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
                    onNotify={pushNotification}
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
            className="fixed bottom-0 left-0 right-0 flex items-start justify-around border-t"
            style={{
              height: MOBILE_TABBAR_SAFE_AREA_HEIGHT,
              zIndex: SHELL_TABBAR_Z_INDEX,
              borderColor: "var(--th-border-subtle)",
              background:
                "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 98%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
              backdropFilter: "blur(18px)",
              boxShadow: "0 -10px 30px -20px color-mix(in srgb, black 70%, transparent)",
              paddingBottom: "env(safe-area-inset-bottom)",
              paddingLeft: "env(safe-area-inset-left)",
              paddingRight: "env(safe-area-inset-right)",
            }}
          >
            {mobilePrimaryRoutes.map((route) => {
              const Icon = iconForRoute(route.id);
              const isActive = activeMobileRouteId === route.id;
              const badge = route.id === "kanban" ? kanbanBadgeCount || undefined : undefined;
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
                  {badge !== undefined && badge > 0 && (
                    <span className="absolute right-[28%] top-1 flex h-4 min-w-4 items-center justify-center rounded-full bg-emerald-500 px-1 text-[8px] font-semibold text-white">
                      {badge > 9 ? "9+" : badge}
                    </span>
                  )}
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
              <Settings size={18} />
              <span>{tr("설정", "Settings")}</span>
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
              className="fixed inset-0 flex items-end justify-center"
              style={{ zIndex: SHELL_BOTTOM_SHEET_Z_INDEX }}
              onClick={() => setShowMobileMoreMenu(false)}
            >
              <div className="absolute inset-0 bg-black/55 backdrop-blur-sm" />
              <div
                data-testid="app-mobile-more-menu"
                role="dialog"
                aria-modal="true"
                aria-label={tr("확장 메뉴", "Extensions menu")}
                className="relative w-full max-h-[80vh] overflow-y-auto rounded-t-[2rem] border px-4 pb-4 pt-3 shadow-2xl animate-in fade-in slide-in-from-bottom-4 duration-200"
                style={{
                  borderColor: "var(--th-border-subtle)",
                  background:
                    "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 98%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 95%, transparent) 100%)",
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
                      {tr("확장", "Extensions")}
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

                <div className="space-y-4">
                  {mobileOverflowSections.map((section) => (
                    <div key={section.id} className="space-y-2">
                      <div
                        className="px-1 text-[11px] font-semibold uppercase tracking-[0.18em]"
                        style={{ color: "var(--th-text-muted)" }}
                      >
                        {isKo ? section.labelKo : section.labelEn}
                      </div>
                      <div className="grid gap-2">
                        {section.routes.map((route) => {
                          const Icon = iconForRoute(route.id);
                          const badge = sidebarBadgeForRoute(route.id);
                          return (
                            <button
                              key={route.id}
                              type="button"
                              aria-label={isKo ? route.labelKo : route.labelEn}
                              onClick={() =>
                                navigateToRoute(
                                  route.path,
                                  route.id === "agents"
                                    ? { agentsTab: "agents" }
                                    : undefined,
                                )
                              }
                              className="flex items-start gap-3 rounded-xl border px-3 py-3 text-left"
                              style={{
                                borderColor: "var(--th-border-subtle)",
                                background: "var(--th-overlay-subtle)",
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
                  ))}
                </div>
              </div>
            </div>
          )}
        </>
      )}

      <Suspense fallback={null}>
        {officeInfoAgent && (
          officeInfoMode === "office" ? (
            <OfficeAgentDrawer
              open
              agent={officeInfoAgent}
              departments={departments}
              locale={locale}
              isKo={isKo}
              spriteMap={spriteMap}
              currentCard={
                officeAgentState.primaryCardByAgent.get(officeInfoAgent.id) ??
                null
              }
              manualIntervention={
                officeAgentState.manualInterventionByAgent.get(
                  officeInfoAgent.id,
                ) ?? null
              }
              onClose={closeOfficeInfo}
            />
          ) : (
            <AgentInfoCard
              agent={officeInfoAgent}
              spriteMap={spriteMap}
              isKo={isKo}
              locale={locale}
              tr={tr}
              departments={departments}
              onClose={closeOfficeInfo}
              onAgentUpdated={() => {
                refreshAgents();
                refreshAllAgents();
                refreshOffices();
                refreshAuditLogs();
              }}
            />
          )
        )}
      </Suspense>

      <Suspense fallback={null}>
        {showCommandPalette && (
          <CommandPalette
            agents={allAgents}
            departments={departments}
            isKo={isKo}
            onSelectAgent={openDefaultAgentInfo}
            onNavigate={(path) => navigateToRoute(path)}
            onClose={() => setShowCommandPalette(false)}
            routes={PALETTE_ROUTES}
            departmentRouteId="/agents"
          />
        )}
      </Suspense>

      {showTweaksPanel && (
        <div
          className="pointer-events-none fixed right-4 top-[5.25rem] w-[min(22rem,calc(100vw-2rem))]"
          style={{ zIndex: SHELL_POPOVER_Z_INDEX }}
        >
          <div
            className="pointer-events-auto rounded-[1.75rem] border p-4 shadow-2xl"
            style={{
              borderColor: "var(--th-border-subtle)",
              background:
                "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 95%, transparent) 100%)",
            }}
          >
            <div className="flex items-center justify-between gap-3">
              <div>
                <div
                  className="text-sm font-semibold"
                  style={{ color: "var(--th-text-heading)" }}
                >
                  Tweaks
                </div>
                <div
                  className="mt-1 text-xs"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {tr("셸 테마와 강조색을 조정합니다.", "Tune shell theme and accent.")}
                </div>
              </div>
              <button
                type="button"
                onClick={() => setShowTweaksPanel(false)}
                className="flex h-8 w-8 items-center justify-center rounded-xl text-[var(--th-text-muted)]"
                aria-label={tr("패널 닫기", "Close panel")}
              >
                <X size={14} />
              </button>
            </div>

            <div className="mt-4 space-y-4">
              <div>
                <div
                  className="mb-2 text-[11px] font-semibold uppercase tracking-[0.18em]"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  Theme
                </div>
                <div
                  className="flex items-center gap-1 rounded-full p-1"
                  style={{ background: "var(--th-overlay-subtle)" }}
                >
                  {THEME_OPTIONS.map((option) => {
                    const active = themePreference === option.id;
                    return (
                      <button
                        key={option.id}
                        type="button"
                        onClick={() => setThemePreference(option.id)}
                        aria-pressed={active}
                        className="flex-1 rounded-full px-3 py-1.5 text-xs font-medium transition-colors"
                        style={
                          active
                            ? {
                                background: "var(--th-accent-primary-soft)",
                                color: "var(--th-accent-primary)",
                              }
                            : { color: "var(--th-text-muted)" }
                        }
                      >
                        {isKo ? option.labelKo : option.labelEn}
                      </button>
                    );
                  })}
                </div>
              </div>

              <div>
                <div
                  className="mb-2 text-[11px] font-semibold uppercase tracking-[0.18em]"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  Accent
                </div>
                <div className="flex flex-wrap items-center gap-2">
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
                        className="flex h-9 w-9 items-center justify-center rounded-full transition-transform"
                        style={{
                          border: active
                            ? "2px solid var(--th-text-heading)"
                            : "1px solid color-mix(in srgb, var(--th-border-subtle) 80%, transparent)",
                          background:
                            "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
                          transform: active ? "translateY(-1px)" : undefined,
                        }}
                      >
                        <span
                          className="h-4.5 w-4.5 rounded-full"
                          style={{ background: `var(${option.token})` }}
                        />
                      </button>
                    );
                  })}
                </div>
              </div>

              <div
                className="rounded-2xl border px-3 py-3 text-xs"
                style={{
                  borderColor: "var(--th-border-subtle)",
                  background: "var(--th-overlay-subtle)",
                }}
              >
                <div style={{ color: "var(--th-text-muted)" }}>
                  {tr("현재 페이지", "Current page")}
                </div>
                <div
                  className="mt-1 font-mono"
                  style={{ color: "var(--th-text-primary)" }}
                >
                  /{currentRoute?.id ?? "home"}
                </div>
              </div>
            </div>
          </div>
        </div>
      )}

      <ToastOverlay notifications={notifications} onDismiss={dismissNotification} />

      {showShortcutHelp && (
        <ShortcutHelpModal
          isKo={isKo}
          onClose={() => setShowShortcutHelp(false)}
        />
      )}

      {!wsConnected && (
        <div
          className="pointer-events-none fixed left-4 right-4 top-4 flex justify-center md:left-auto md:right-6"
          style={{ zIndex: SHELL_TOAST_Z_INDEX }}
        >
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
  isKo,
  badge,
  onNavigate,
}: {
  route: AppRouteEntry;
  currentRouteId: AppRouteId | null;
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
      className="group relative flex w-full items-center gap-3 rounded-xl px-3 py-2.5 text-left transition-colors"
      style={{
        background: active
          ? "color-mix(in srgb, var(--th-accent-primary-soft) 80%, transparent)"
          : "transparent",
        color: active ? "var(--th-text-primary)" : "var(--th-text-secondary)",
      }}
      title={isKo ? route.labelKo : route.labelEn}
    >
      <span
        className="flex h-9 w-9 shrink-0 items-center justify-center rounded-xl"
        style={{
          background: active
            ? "color-mix(in srgb, var(--th-accent-primary) 18%, transparent)"
            : "var(--th-overlay-subtle)",
        }}
      >
        <Icon size={18} />
      </span>
      <span className="min-w-0 flex-1">
        <span className="block truncate text-sm font-medium">
          {isKo ? route.labelKo : route.labelEn}
        </span>
      </span>
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
  isMobileViewport,
  isKo,
  wsConnected,
  currentOfficeLabel,
  stats,
  agents,
  meetings,
  notifications,
  kanbanCards,
}: {
  isMobileViewport: boolean;
  isKo: boolean;
  wsConnected: boolean;
  currentOfficeLabel: string;
  stats: DashboardStats | null;
  agents: Agent[];
  meetings: RoundTableMeeting[];
  notifications: Notification[];
  kanbanCards: KanbanCard[];
}) {
  const tr = useCallback((ko: string, en: string) => (isKo ? ko : en), [isKo]);
  const [editing, setEditing] = useLocalStorage<boolean>(STORAGE_KEYS.homeEditing, false);
  const [dragIndex, setDragIndex] = useState<number | null>(null);
  const [overIndex, setOverIndex] = useState<number | null>(null);
  const [analytics, setAnalytics] = useState<TokenAnalyticsResponse | null>(null);
  const [gamification, setGamification] = useState<api.AchievementsResponse | null>(null);
  const [streaks, setStreaks] = useState<api.AgentStreak[]>([]);
  const defaultWidgets = useMemo(
    () => [...HOME_DEFAULT_WIDGETS],
    [],
  );
  const [widgets, setWidgets] = useLocalStorage<string[]>(
    STORAGE_KEYS.homeOrder,
    () => [...HOME_DEFAULT_WIDGETS],
  );
  const outstandingMeetings = meetings.filter(hasUnresolvedMeetingIssues).length;
  const liveNotifications = notifications.filter(
    (notification) => Date.now() - notification.ts < 60_000,
  ).length;
  const requestedCards = kanbanCards.filter((card) => card.status === "requested").length;
  const inProgressCards = kanbanCards.filter(
    (card) => card.status === "in_progress" || card.status === "review",
  ).length;
  const topAgents = useMemo(
    () =>
      (stats?.top_agents?.length
        ? stats.top_agents
        : [...agents]
            .sort(
              (left, right) =>
                right.stats_xp - left.stats_xp ||
                right.stats_tasks_done - left.stats_tasks_done,
            )
            .map((agent) => ({
              id: agent.id,
              name: agent.name,
              alias: agent.alias ?? null,
              name_ko: agent.name_ko,
              avatar_emoji: agent.avatar_emoji,
              stats_tasks_done: agent.stats_tasks_done,
              stats_xp: agent.stats_xp,
              stats_tokens: agent.stats_tokens,
            })))
        .slice(0, 6),
    [agents, stats?.top_agents],
  );
  const doneCards = kanbanCards.filter((card) => card.status === "done").length;
  const blockedCards = kanbanCards.filter((card) => card.status === "blocked").length;
  const totalActionableCards = requestedCards + inProgressCards + blockedCards;
  const totalMeetings = meetings.length;
  const reviewQueue = stats?.kanban.review_queue ?? kanbanCards.filter((card) => card.status === "review").length;
  const agentTotal = stats?.agents.total ?? topAgents.length;
  const liveSessions = stats?.dispatched_count ?? 0;
  const providerSummary = tr("2/2 프로바이더 연결", "2/2 providers connected");
  const operationalMissionRows: DailyMissionViewModel[] = [
    {
      id: "review",
      label: tr("리뷰 대기 비우기", "Clear review queue"),
      current: reviewQueue === 0 ? 1 : 0,
      target: 1,
      completed: reviewQueue === 0,
      description: tr("우선 확인이 필요한 카드", "Cards waiting for reviewer action"),
      xp: 35,
    },
    {
      id: "blocked",
      label: tr("블록 카드 줄이기", "Reduce blocked cards"),
      current: Math.max(0, 1 - Math.min(stats?.kanban.blocked ?? blockedCards, 1)),
      target: 1,
      completed: blockedCards === 0,
      description: tr("의존성/외부 응답 대기", "Waiting on dependencies or replies"),
      xp: 30,
    },
    {
      id: "dispatch",
      label: tr("실시간 세션 유지", "Keep live sessions healthy"),
      current: Math.min(stats?.dispatched_count ?? 0, 3),
      target: 3,
      completed: (stats?.dispatched_count ?? 0) >= 3 && wsConnected,
      description: tr("현재 연결된 작업 세션", "Currently connected working sessions"),
      xp: 40,
    },
    {
      id: "meetings",
      label: tr("회의 후속 정리", "Close meeting follow-ups"),
      current: Math.max(0, totalMeetings - outstandingMeetings),
      target: Math.max(totalMeetings, 1),
      completed: outstandingMeetings === 0,
      description: tr("정리/이슈화가 필요한 회의", "Meetings still needing wrap-up"),
      xp: 25,
    },
  ];
  const activityItems = notifications.slice(0, 4).map((notification) => ({
    id: notification.id,
    title: notification.message,
    meta: formatRelativeTime(notification.ts, isKo),
    accent: notificationColor(notification.type),
  }));
  const fallbackActivity = meetings.slice(0, 4).map((meeting) => ({
    id: meeting.id,
    title: meeting.agenda,
    meta: meeting.status === "completed"
      ? tr("회의 종료", "Meeting completed")
      : tr("회의 진행 중", "Meeting in progress"),
    accent:
      meeting.status === "completed"
        ? "var(--th-accent-primary)"
        : "var(--th-accent-warn)",
  }));
  const kanbanColumns = [
    { id: "requested", label: tr("요청", "Requested"), accent: "#7dd3fc" },
    { id: "in_progress", label: tr("진행", "In progress"), accent: "#6ef2a3" },
    { id: "review", label: tr("리뷰", "Review"), accent: "#f5bd47" },
    { id: "done", label: tr("완료", "Done"), accent: "#c084fc" },
  ] as const;

  useEffect(() => {
    const controller = new AbortController();
    let active = true;
    api
      .getTokenAnalytics("7d", { signal: controller.signal })
      .then((next) => {
        if (!active) return;
        setAnalytics(next);
      })
      .catch((error) => {
        if (!active || controller.signal.aborted) return;
        console.error("Failed to load token analytics for home overview", error);
      });

    return () => {
      active = false;
      controller.abort();
    };
  }, []);

  useEffect(() => {
    if (typeof window === "undefined") return;
    if (window.localStorage.getItem(STORAGE_KEYS.homeOrder) !== null) return;
    try {
      const legacyRaw =
        window.localStorage.getItem("agentdesk.widgets") ??
        window.localStorage.getItem("agentdesk.home.widgets");
      const parsed = legacyRaw ? (JSON.parse(legacyRaw) as unknown) : null;
      if (Array.isArray(parsed) && parsed.length > 0) {
        const migrated = normalizeHomeWidgetOrder(parsed);
        if (migrated.length > 0) {
          setWidgets(migrated);
        }
      }
    } catch {
      // Ignore malformed legacy storage and keep the default order.
    }
  }, [setWidgets]);

  useEffect(() => {
    const normalized = normalizeHomeWidgetOrder(widgets);
    if (!areStringArraysEqual(widgets, normalized)) {
      setWidgets(normalized);
    }
  }, [setWidgets, widgets]);

  useEffect(() => {
    if (!isMobileViewport || !editing) return;
    setEditing(false);
  }, [editing, isMobileViewport, setEditing]);

  useEffect(() => {
    let active = true;
    Promise.all([
      api.getAchievements().catch(() => ({ achievements: [], daily_missions: [] })),
      api.getStreaks().catch(() => ({ streaks: [] })),
    ]).then(([achievementResponse, streakResponse]) => {
      if (!active) return;
      setGamification(achievementResponse);
      setStreaks(
        [...streakResponse.streaks].sort((left, right) => right.streak - left.streak),
      );
    });
    return () => {
      active = false;
    };
  }, []);

  const todayLabel = useMemo(
    () =>
      new Intl.DateTimeFormat(isKo ? "ko-KR" : "en-US", {
        weekday: "long",
        month: "short",
        day: "numeric",
      }).format(new Date()),
    [isKo],
  );
  const latestAnalyticsDay = analytics?.daily.at(-1) ?? null;
  const tokenTrend = analytics?.daily.slice(-7).map((day) => day.total_tokens) ?? [];
  const costTrend = analytics?.daily.slice(-7).map((day) => day.cost) ?? [];
  const activityStreak = useMemo(() => {
    const daily = [...(analytics?.daily ?? [])].sort((left, right) =>
      left.date.localeCompare(right.date),
    );
    let streak = 0;
    for (let index = daily.length - 1; index >= 0; index -= 1) {
      if (daily[index].total_tokens <= 0) break;
      streak += 1;
    }
    return streak;
  }, [analytics]);
  const formatCompact = useCallback(
    (value: number) =>
      new Intl.NumberFormat(isKo ? "ko-KR" : "en-US", {
        notation: "compact",
        maximumFractionDigits: value >= 1_000_000 ? 1 : 0,
      }).format(value),
    [isKo],
  );
  const formatCurrency = useCallback(
    (value: number) =>
      new Intl.NumberFormat(isKo ? "en-US" : "en-US", {
        style: "currency",
        currency: "USD",
        maximumFractionDigits: value >= 100 ? 0 : 2,
      }).format(value),
    [],
  );
  const streakLeader = streaks[0] ?? null;
  const gamificationLeader = topAgents[0] ?? null;
  const gamificationLevel = getAgentLevelFromXp(gamificationLeader?.stats_xp ?? 0);
  const dailyMissions = useMemo<DailyMissionViewModel[]>(() => {
    if (gamification?.daily_missions?.length) {
      return gamification.daily_missions.map((mission) => {
        switch (mission.id) {
          case "dispatches_today":
            return {
              id: mission.id,
              label: tr("오늘 디스패치 5건 완료", "Complete 5 dispatches today"),
              current: mission.current,
              target: mission.target,
              completed: mission.completed,
              description: tr("오늘 실제 완료된 디스패치 수", "Completed dispatches today"),
              xp: 40,
            };
          case "active_agents_today":
            return {
              id: mission.id,
              label: tr("오늘 3명 이상 출항", "Get 3 agents shipping today"),
              current: mission.current,
              target: mission.target,
              completed: mission.completed,
              description: tr("오늘 완료 기록이 있는 에이전트 수", "Agents with completed work today"),
              xp: 35,
            };
          case "review_queue_zero":
            return {
              id: mission.id,
              label: tr("리뷰 큐 비우기", "Drain the review queue"),
              current: mission.current,
              target: mission.target,
              completed: mission.completed,
              description: tr("리뷰 대기 카드를 0으로 유지", "Keep the review queue empty"),
              xp: 40,
            };
          default:
            return {
              id: mission.id,
              label: mission.label,
              current: mission.current,
              target: mission.target,
              completed: mission.completed,
            };
        }
      });
    }
    return operationalMissionRows;
  }, [gamification?.daily_missions, operationalMissionRows, tr]);
  const missionReset = useMemo(() => getMissionResetCountdown(), []);
  const missionResetLabel = tr(
    `리셋까지 ${missionReset.hours}시간 ${missionReset.minutes}분`,
    `Resets in ${missionReset.hours}h ${missionReset.minutes}m`,
  );
  const missionXpLabel = dailyMissions.length > 0 ? `+${getMissionTotalXp(dailyMissions)} XP` : undefined;

  const widgetSpecs = useMemo(
    () => ({
      m_tokens: {
        className: "lg:col-span-3",
        render: () => (
          <HomeMetricTile
            icon={<Zap size={14} />}
            title={tr("오늘 토큰", "Today's tokens")}
            value={formatCompact(latestAnalyticsDay?.total_tokens ?? 0)}
            sub={tr(
              `7일 평균 ${formatCompact(Math.round(analytics?.summary.average_daily_tokens ?? 0))}`,
              `7d avg ${formatCompact(Math.round(analytics?.summary.average_daily_tokens ?? 0))}`,
            )}
            delta={
              analytics?.summary.total_tokens
                ? tr(`7일 ${formatCompact(analytics.summary.total_tokens)}`, `7d ${formatCompact(analytics.summary.total_tokens)}`)
                : undefined
            }
            deltaTone="flat"
            accent="var(--th-accent-primary)"
            trend={tokenTrend}
          />
        ),
      },
      m_cost: {
        className: "lg:col-span-3",
        render: () => (
          <HomeMetricTile
            icon={<Sparkles size={14} />}
            title={tr("API 비용", "API cost")}
            value={formatCurrency(latestAnalyticsDay?.cost ?? 0)}
            sub={tr(
              `캐시 절감 ${formatCurrency(analytics?.summary.cache_discount ?? 0)}`,
              `Cache saved ${formatCurrency(analytics?.summary.cache_discount ?? 0)}`,
            )}
            delta={
              analytics?.summary.total_cost != null
                ? tr(`7일 ${formatCurrency(analytics.summary.total_cost)}`, `7d ${formatCurrency(analytics.summary.total_cost)}`)
                : undefined
            }
            deltaTone="flat"
            accent="var(--th-accent-success)"
            trend={costTrend}
          />
        ),
      },
      m_progress: {
        className: "lg:col-span-3",
        render: () => (
          <HomeMetricTile
            icon={<Target size={14} />}
            title={tr("진행 중", "In progress")}
            value={`${inProgressCards}`}
            sub={tr(
              `${requestedCards} 요청 · ${reviewQueue} 리뷰 · ${blockedCards} 블록`,
              `${requestedCards} requested · ${reviewQueue} review · ${blockedCards} blocked`,
            )}
            delta={tr(`${totalActionableCards} 전체`, `${totalActionableCards} total`)}
            deltaTone="flat"
            accent="var(--th-accent-warn)"
          />
        ),
      },
      m_streak: {
        className: "lg:col-span-3",
        render: () => (
          <StreakCounter
            dataTestId="home-streak-counter"
            className="h-full"
            icon={<Flame size={18} />}
            title={tr("연속 활동", "Current streak")}
            value={tr(`${streakLeader?.streak ?? activityStreak}일`, `${streakLeader?.streak ?? activityStreak}d`)}
            subtitle={tr(
              gamificationLeader
                ? `lv.${gamificationLevel.level} · XP ${formatCompact(Math.round(gamificationLeader.stats_xp))}`
                : `${analytics?.summary.active_days ?? 0}일 활성`,
              gamificationLeader
                ? `lv.${gamificationLevel.level} · XP ${formatCompact(Math.round(gamificationLeader.stats_xp))}`
                : `${analytics?.summary.active_days ?? 0} active days`,
            )}
            badgeLabel={tr("streak", "streak")}
            detail={
              streakLeader
                ? tr(`${streakLeader.name} 최장`, `${streakLeader.name} best`)
                : analytics?.summary.active_days
                  ? tr(`${analytics.summary.active_days}/7 활성`, `${analytics.summary.active_days}/7 active`)
                : undefined
            }
            accent="var(--th-accent-danger)"
          />
        ),
      },
      office: {
        className: "lg:col-span-8",
        render: () => (
          <HomeWidgetShell
            title={tr("오피스 뷰", "Office view")}
            subtitle={tr(
              `${currentOfficeLabel} 기준으로 지금 일하는 에이전트를 요약합니다.`,
              `Summarized live roster for ${currentOfficeLabel}.`,
            )}
            action={
              <Link
                to="/office"
                className="inline-flex items-center gap-2 rounded-full border px-3 py-1.5 text-xs font-medium transition-colors hover:bg-white/5"
                style={{ borderColor: "var(--th-border-subtle)", color: "var(--th-text-primary)" }}
              >
                {tr("전체 보기", "Open office")}
                <ChevronRight size={14} />
              </Link>
            }
          >
            <div className="relative overflow-hidden rounded-[1.5rem] border p-4 sm:p-5" style={{ borderColor: "var(--th-border-subtle)", background: "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 92%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 92%, transparent) 100%)" }}>
              <div
                className="pointer-events-none absolute inset-0 opacity-30"
                style={{
                  backgroundImage:
                    "radial-gradient(circle, color-mix(in srgb, var(--th-text-muted) 38%, transparent) 1px, transparent 1px)",
                  backgroundSize: "14px 14px",
                }}
              />
              <div className="relative grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-6">
                {topAgents.length === 0 ? (
                  <div className="col-span-full rounded-2xl border px-4 py-8 text-center text-sm" style={{ borderColor: "var(--th-border-subtle)", color: "var(--th-text-muted)", background: "var(--th-overlay-subtle)" }}>
                    {tr("표시할 활성 에이전트가 없습니다.", "No active agents to show right now.")}
                  </div>
                ) : (
                  topAgents.map((agent) => {
                    const progress = Math.min(100, Math.max(12, Math.round(agent.stats_tokens / 100_000)));
                    return (
                      <div key={agent.id} className="rounded-2xl border px-3 py-3 text-center" style={{ borderColor: "var(--th-border-subtle)", background: "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)" }}>
                        <div className="mx-auto flex h-12 w-12 items-center justify-center rounded-2xl border text-xl" style={{ borderColor: "var(--th-border-subtle)", background: "var(--th-card-bg)" }}>
                          {agent.avatar_emoji || "🤖"}
                        </div>
                        <div className="mt-3 truncate text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                          {isKo ? agent.name_ko : agent.name}
                        </div>
                        <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                          {tr(`${agent.stats_tasks_done}건 완료`, `${agent.stats_tasks_done} tasks done`)}
                        </div>
                        <div className="mt-3 h-1.5 rounded-full" style={{ background: "color-mix(in srgb, var(--th-border-subtle) 70%, transparent)" }}>
                          <div className="h-full rounded-full" style={{ width: `${progress}%`, background: "var(--th-accent-primary)" }} />
                        </div>
                      </div>
                    );
                  })
                )}
              </div>
            </div>
          </HomeWidgetShell>
        ),
      },
      missions: {
        className: "lg:col-span-4",
        render: () => (
          <HomeWidgetShell
            title={tr("데일리 미션", "Daily missions")}
            subtitle={tr(
              "오늘 바로 확인해야 할 운영 우선순위를 정리합니다.",
              "Keep today's operational priorities in view.",
            )}
          >
            <DailyMissions
              dataTestId="home-daily-missions"
              itemTestIdPrefix="home-daily-mission"
              missions={dailyMissions}
              emptyLabel={tr("표시할 데일리 미션이 없습니다.", "No daily missions available.")}
              doneLabel={tr("완료", "Done")}
              progressLabel={tr("진행", "Progress")}
              resetLabel={missionResetLabel}
              totalXpLabel={missionXpLabel}
            />
          </HomeWidgetShell>
        ),
      },
      roster: {
        className: "lg:col-span-7",
        render: () => (
          <HomeWidgetShell
            title={tr("에이전트 현황", "Agent roster")}
            subtitle={tr("상위 작업 에이전트를 빠르게 훑어봅니다.", "Quick scan of the most active agents.")}
          >
            <div className="space-y-2">
              {topAgents.length === 0 ? (
                <div className="rounded-2xl border px-4 py-8 text-center text-sm" style={{ borderColor: "var(--th-border-subtle)", color: "var(--th-text-muted)", background: "var(--th-overlay-subtle)" }}>
                  {tr("에이전트 통계가 아직 없습니다.", "Agent statistics are not available yet.")}
                </div>
              ) : (
                topAgents.map((agent) => (
                  <div key={agent.id} className="grid grid-cols-[auto_1fr_auto] items-center gap-3 rounded-2xl border px-3 py-3" style={{ borderColor: "var(--th-border-subtle)", background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)" }}>
                    <div className="flex h-10 w-10 items-center justify-center rounded-2xl border text-lg" style={{ borderColor: "var(--th-border-subtle)", background: "var(--th-bg-surface)" }}>
                      {agent.avatar_emoji || "🤖"}
                    </div>
                    <div className="min-w-0">
                      <div className="truncate text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                        {isKo ? agent.name_ko : agent.name}
                      </div>
                      <div className="mt-1 truncate text-xs" style={{ color: "var(--th-text-muted)" }}>
                        {tr(
                          `${agent.stats_tasks_done}건 완료 · XP ${Math.round(agent.stats_xp).toLocaleString()}`,
                          `${agent.stats_tasks_done} tasks done · XP ${Math.round(agent.stats_xp).toLocaleString()}`,
                        )}
                      </div>
                    </div>
                    <div className="text-right text-xs" style={{ color: "var(--th-text-muted)" }}>
                      <div className="font-semibold" style={{ color: "var(--th-text-primary)" }}>
                        {agent.stats_tokens > 0 ? `${Math.round(agent.stats_tokens / 1000).toLocaleString()}K` : "0"}
                      </div>
                      <div>{tr("tokens", "tokens")}</div>
                    </div>
                  </div>
                ))
              )}
            </div>
          </HomeWidgetShell>
        ),
      },
      activity: {
        className: "lg:col-span-5",
        render: () => {
          const items = activityItems.length > 0 ? activityItems : fallbackActivity;
          return (
            <HomeWidgetShell
              title={tr("최근 활동", "Recent activity")}
              subtitle={tr("알림과 회의 후속을 우선적으로 보여줍니다.", "Prioritizes alerts and meeting follow-ups.")}
            >
              <div className="space-y-2">
                {items.length === 0 ? (
                  <div className="rounded-2xl border px-4 py-8 text-center text-sm" style={{ borderColor: "var(--th-border-subtle)", color: "var(--th-text-muted)", background: "var(--th-overlay-subtle)" }}>
                    {tr("표시할 최근 활동이 없습니다.", "No recent activity to show.")}
                  </div>
                ) : (
                  items.map((item) => (
                    <div key={item.id} className="grid grid-cols-[auto_1fr_auto] items-start gap-3 rounded-2xl border px-3 py-3" style={{ borderColor: "var(--th-border-subtle)", background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)" }}>
                      <span className="mt-1 h-2.5 w-2.5 rounded-full" style={{ background: item.accent }} />
                      <div className="min-w-0">
                        <div className="text-sm leading-6" style={{ color: "var(--th-text-primary)" }}>
                          {item.title}
                        </div>
                      </div>
                      <div className="text-[11px] whitespace-nowrap" style={{ color: "var(--th-text-muted)" }}>
                        {item.meta}
                      </div>
                    </div>
                  ))
                )}
              </div>
            </HomeWidgetShell>
          );
        },
      },
      kanban: {
        className: "lg:col-span-12",
        render: () => (
          <HomeWidgetShell
            title={tr("칸반 스냅샷", "Kanban snapshot")}
            subtitle={tr("현재 카드 흐름을 한 번에 살피는 요약 보드입니다.", "A wide snapshot of the current card flow.")}
            action={
              <Link
                to="/kanban"
                className="inline-flex items-center gap-2 rounded-full border px-3 py-1.5 text-xs font-medium transition-colors hover:bg-white/5"
                style={{ borderColor: "var(--th-border-subtle)", color: "var(--th-text-primary)" }}
              >
                {tr("칸반 열기", "Open kanban")}
                <ChevronRight size={14} />
              </Link>
            }
          >
            <div className="grid gap-3 lg:grid-cols-4">
              {kanbanColumns.map((column) => {
                const cards = kanbanCards.filter((card) => card.status === column.id).slice(0, 3);
                return (
                  <div key={column.id} className="rounded-[1.5rem] border p-3" style={{ borderColor: "var(--th-border-subtle)", background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)" }}>
                    <div className="flex items-center justify-between gap-2">
                      <div className="text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                        {column.label}
                      </div>
                      <span className="rounded-full px-2 py-1 text-[11px] font-semibold" style={{ background: "var(--th-overlay-medium)", color: column.accent }}>
                        {column.id === "requested"
                          ? requestedCards
                          : column.id === "in_progress"
                            ? kanbanCards.filter((card) => card.status === "in_progress").length
                            : column.id === "review"
                              ? kanbanCards.filter((card) => card.status === "review").length
                              : doneCards}
                      </span>
                    </div>
                    <div className="mt-3 space-y-2">
                      {cards.length === 0 ? (
                        <div className="rounded-2xl border px-3 py-4 text-sm" style={{ borderColor: "var(--th-border-subtle)", color: "var(--th-text-muted)", background: "var(--th-overlay-subtle)" }}>
                          {tr("표시할 카드 없음", "No cards")}
                        </div>
                      ) : (
                        cards.map((card) => (
                          <div key={card.id} className="rounded-2xl border px-3 py-3" style={{ borderColor: "var(--th-border-subtle)", background: "color-mix(in srgb, var(--th-bg-surface) 92%, transparent)" }}>
                            <div className="line-clamp-2 text-sm font-medium leading-6" style={{ color: "var(--th-text-primary)" }}>
                              {card.title}
                            </div>
                            <div className="mt-2 flex items-center justify-between gap-2 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                              <span className="truncate">
                                {card.github_repo ?? tr("repo 미지정", "No repo")}
                              </span>
                              <span className="whitespace-nowrap">
                                #{card.github_issue_number ?? "—"}
                              </span>
                            </div>
                          </div>
                        ))
                      )}
                    </div>
                  </div>
                );
              })}
            </div>
          </HomeWidgetShell>
        ),
      },
    }),
    [
      analytics,
      blockedCards,
      costTrend,
      currentOfficeLabel,
      doneCards,
      fallbackActivity,
      inProgressCards,
      isKo,
      kanbanCards,
      meetings.length,
      notifications.length,
      outstandingMeetings,
      requestedCards,
      stats,
      tokenTrend,
      topAgents,
      tr,
      totalActionableCards,
      wsConnected,
      activityItems,
      activityStreak,
      dailyMissions,
      formatCompact,
      formatCurrency,
      gamificationLeader,
      gamificationLevel.level,
      latestAnalyticsDay,
      missionResetLabel,
      missionXpLabel,
      reviewQueue,
      streakLeader,
    ],
  );

  return (
    <div className="mx-auto h-full w-full max-w-[92rem] overflow-auto px-4 py-6 pb-32 sm:px-6">
      <div className="flex flex-wrap items-end justify-between gap-4">
        <div className="max-w-3xl">
          <div className="mb-1.5 flex flex-wrap items-center gap-2 text-[11px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
            <span>{todayLabel}</span>
            <span className="h-1 w-1 rounded-full" style={{ background: "var(--th-text-muted)" }} />
            <span className="inline-flex items-center gap-1.5" style={{ color: wsConnected ? "var(--th-accent-primary)" : "var(--th-accent-danger)" }}>
              <span className="h-2 w-2 rounded-full" style={{ background: wsConnected ? "var(--th-accent-primary)" : "var(--th-accent-danger)" }} />
              {wsConnected ? "all systems normal" : tr("연결 상태 확인 필요", "connection degraded")}
            </span>
          </div>
          <h1 className="text-3xl font-semibold tracking-tight sm:text-4xl" style={{ color: "var(--th-text-heading)" }}>
            {tr("오늘의 AgentDesk", "Today's AgentDesk")}
          </h1>
          <p className="mt-2 max-w-2xl text-sm leading-7 sm:text-base" style={{ color: "var(--th-text-secondary)" }}>
            {tr(
              `에이전트 ${agentTotal}명 · 세션 ${liveSessions} 활성 · ${providerSummary}`,
              `${agentTotal} agents · ${liveSessions} live sessions · ${providerSummary}`,
            )}
          </p>
        </div>

        <div className="flex flex-wrap items-center gap-2">
          {!isMobileViewport && editing && (
            <button
              type="button"
              onClick={() => setWidgets([...defaultWidgets])}
              data-testid="home-reset-order"
              className="rounded-full border px-3 py-2 text-xs font-medium transition-colors hover:bg-white/5"
              style={{ borderColor: "var(--th-border-subtle)", color: "var(--th-text-muted)" }}
            >
              {tr("기본값", "Reset")}
            </button>
          )}
          {!isMobileViewport ? (
            <button
              type="button"
              onClick={() => setEditing((prev) => !prev)}
              data-testid="home-edit-toggle"
              className="inline-flex items-center gap-2 rounded-full border px-3 py-2 text-xs font-medium transition-colors hover:bg-white/5"
              style={{
                borderColor: editing ? "var(--th-accent-primary)" : "var(--th-border-subtle)",
                background: editing ? "var(--th-accent-primary-soft)" : "transparent",
                color: editing ? "var(--th-text-heading)" : "var(--th-text-primary)",
              }}
            >
              <GripVertical size={14} />
              {editing ? tr("완료", "Done") : tr("편집", "Edit")}
            </button>
          ) : null}
        </div>
      </div>

      {!isMobileViewport && editing && (
        <div className="mt-4 rounded-2xl border px-4 py-3 text-sm" style={{ borderColor: "color-mix(in srgb, var(--th-accent-primary) 26%, var(--th-border) 74%)", background: "var(--th-accent-primary-soft)", color: "var(--th-text-secondary)" }}>
          <span className="inline-flex items-center gap-2">
            <GripVertical size={14} />
            {tr(
              "위젯을 드래그해서 순서를 바꿀 수 있습니다. 완료를 누르면 현재 배치가 유지됩니다.",
              "Drag widgets to reorder them. The current layout will persist when you press done.",
            )}
          </span>
        </div>
      )}

      <div className="mt-5 grid grid-cols-1 gap-4 lg:grid-cols-12">
        {widgets.map((widgetId, index) => {
          const spec = widgetSpecs[widgetId as keyof typeof widgetSpecs];
          if (!spec) return null;
          return (
            <div
              key={widgetId}
              data-testid={`home-widget-${widgetId}`}
              draggable={editing && !isMobileViewport}
              onDragStart={(event) => {
                if (!editing || isMobileViewport) return;
                setDragIndex(index);
                event.dataTransfer.effectAllowed = "move";
                try {
                  event.dataTransfer.setData("text/plain", String(index));
                } catch {
                  // no-op
                }
              }}
              onDragOver={(event) => {
                if (!editing || isMobileViewport) return;
                event.preventDefault();
                if (overIndex !== index) setOverIndex(index);
              }}
              onDrop={(event) => {
                if (!editing || isMobileViewport) return;
                event.preventDefault();
                if (dragIndex == null || dragIndex === index) {
                  setDragIndex(null);
                  setOverIndex(null);
                  return;
                }
                const next = [...widgets];
                const [moved] = next.splice(dragIndex, 1);
                next.splice(index, 0, moved);
                setWidgets(next);
                setDragIndex(null);
                setOverIndex(null);
              }}
              onDragEnd={() => {
                setDragIndex(null);
                setOverIndex(null);
              }}
              className={[
                spec.className,
                dragIndex === index ? "opacity-70" : "",
                overIndex === index && dragIndex !== index ? "rounded-[2rem] ring-2 ring-[color:var(--th-accent-primary)] ring-offset-2 ring-offset-transparent" : "",
              ]
                .filter(Boolean)
                .join(" ")}
            >
              <div className="relative">
                {editing && (
                  <div className="pointer-events-none absolute right-4 top-4 z-10 flex h-8 w-8 items-center justify-center rounded-full border" style={{ borderColor: "var(--th-border-subtle)", background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)", color: "var(--th-text-muted)" }}>
                    <GripVertical size={14} />
                  </div>
                )}
                {spec.render()}
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}

function HomeMetricTile({
  icon,
  title,
  value,
  sub,
  delta,
  deltaTone = "flat",
  accent,
  trend,
}: {
  icon: React.ReactNode;
  title: string;
  value: string;
  sub: string;
  delta?: string;
  deltaTone?: "up" | "down" | "flat";
  accent: string;
  trend?: number[];
}) {
  const strokePoints =
    trend && trend.length > 1
      ? trend
          .map((point, index) => {
            const max = Math.max(...trend, 1);
            const min = Math.min(...trend, 0);
            const x = (index / (trend.length - 1)) * 100;
            const normalized = max === min ? 0.5 : (point - min) / (max - min);
            const y = 26 - normalized * 20;
            return `${x},${y}`;
          })
          .join(" ")
      : null;
  return (
    <div
      className="h-full overflow-hidden rounded-[1.15rem] border"
      style={{
        borderColor: "var(--th-border-subtle)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      <div className="px-4 py-4 sm:px-5">
        <div className="flex items-center justify-between gap-3">
          <div className="flex items-center gap-2 text-[11.5px] font-medium uppercase tracking-[0.08em]" style={{ color: "var(--th-text-muted)" }}>
            {icon}
            <span>{title}</span>
          </div>
          {delta ? (
            <span
              className="rounded-md px-1.5 py-0.5 text-[11px] font-medium"
              style={{
                background:
                  deltaTone === "up"
                    ? "color-mix(in srgb, var(--th-accent-success) 14%, transparent)"
                    : deltaTone === "down"
                      ? "color-mix(in srgb, var(--th-accent-danger) 14%, transparent)"
                      : "var(--th-overlay-medium)",
                color:
                  deltaTone === "up"
                    ? "var(--th-accent-success)"
                    : deltaTone === "down"
                      ? "var(--th-accent-danger)"
                      : "var(--th-text-muted)",
              }}
            >
              {delta}
            </span>
          ) : null}
        </div>
        <div
          className="mt-3 text-[26px] font-semibold tracking-tight"
          style={{ color: "var(--th-text-heading)" }}
        >
          {value}
        </div>
        <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
          {sub}
        </div>
        {strokePoints ? (
          <svg
            viewBox="0 0 100 30"
            preserveAspectRatio="none"
            className="mt-3 h-8 w-full"
            aria-hidden="true"
          >
            <polyline
              fill="none"
              stroke={accent}
              strokeWidth="2"
              strokeLinejoin="round"
              strokeLinecap="round"
              points={strokePoints}
            />
          </svg>
        ) : (
          <div className="mt-3 h-1.5 rounded-full" style={{ background: "color-mix(in srgb, var(--th-border-subtle) 68%, transparent)" }}>
            <div className="h-full rounded-full" style={{ width: "100%", background: accent }} />
          </div>
        )}
      </div>
    </div>
  );
}

function HomeWidgetShell({
  title,
  subtitle,
  action,
  children,
}: {
  title: string;
  subtitle: string;
  action?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <div
      className="h-full overflow-hidden rounded-[1.15rem] border"
      style={{
        borderColor: "var(--th-border-subtle)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      <div className="flex items-start justify-between gap-3 border-b px-4 py-3 sm:px-5" style={{ borderColor: "var(--th-border-subtle)" }}>
        <div className="min-w-0">
          <div className="text-[12.5px] font-medium" style={{ color: "var(--th-text-secondary)" }}>
            {title}
          </div>
          <div className="mt-1 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
            {subtitle}
          </div>
        </div>
        {action}
      </div>
      <div className="px-4 py-4 sm:px-5">{children}</div>
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
    <div
      data-testid="shortcut-help-modal"
      className="fixed inset-0 flex items-center justify-center px-4"
      style={{ zIndex: SHELL_MODAL_Z_INDEX }}
      onClick={onClose}
    >
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
              {isKo ? "자주 쓰는 조작을 빠르게 확인하세요" : "Quick access to the controls you use most"}
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
