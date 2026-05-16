import { Suspense, useCallback, useEffect, useMemo, useState } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import type { Agent, CompanySettings } from "../types";
import { DEFAULT_SETTINGS } from "../types";
import * as api from "../api/client";
import { useKanban } from "../contexts/KanbanContext";
import { useOffice } from "../contexts/OfficeContext";
import { useSettings } from "../contexts/SettingsContext";
import { useSpriteMap } from "../components/AgentAvatar";
import type { Notification } from "../components/NotificationCenter";
import { deriveOfficeAgentState } from "../components/office-view/officeAgentState";
import OfficeSelectorBar from "../components/OfficeSelectorBar";
import { useFocusTrap } from "../components/common/overlay";
import { MOBILE_LAYOUT_MEDIA_QUERY } from "./breakpoints";
import { PRIMARY_ROUTES, findRouteByPath, type AppRouteEntry, type AppRouteId } from "./routes";
import { AppMobileNavigation } from "./AppMobileNavigation";
import { AppSidebar } from "./AppSidebar";
import { AppTopBar } from "./AppTopBar";
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
import AppShellRoutes from "./AppShellRoutes";
import AppShellOverlays from "./AppShellOverlays";
import AppViewSkeleton from "./AppViewSkeleton";
import { countOpenMeetingIssues } from "./meetingSummary";
import { selectedOfficeLabel } from "./shellLabels";
import { MOBILE_PRIMARY_ROUTE_IDS, SIDEBAR_SECTION_ORDER } from "./shellNavigationConfig";
import { iconForRoute } from "./shellRouteIcons";
import { getOperatorLevelTitle } from "./HomeOverviewConfig";
import { getAgentLevelFromXp } from "../components/gamification/GamificationShared";

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

const MOBILE_TABBAR_SAFE_AREA_HEIGHT = "calc(4rem + env(safe-area-inset-bottom))";

// Keep persistent shell chrome below route-level backdrops and modals.
const ROUTE_OVERLAY_BASE_Z_INDEX = 50;
const SHELL_HEADER_Z_INDEX = ROUTE_OVERLAY_BASE_Z_INDEX - 30;
const SHELL_POPOVER_Z_INDEX = ROUTE_OVERLAY_BASE_Z_INDEX - 10;
const SHELL_TABBAR_Z_INDEX = ROUTE_OVERLAY_BASE_Z_INDEX - 20;
const SHELL_BOTTOM_SHEET_Z_INDEX = ROUTE_OVERLAY_BASE_Z_INDEX - 5;
const SHELL_TOAST_Z_INDEX = 95;
const SHELL_MODAL_Z_INDEX = 100;
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
  const mobileMoreMenuRef = useFocusTrap(showMobileMoreMenu);
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
    (meeting) => countOpenMeetingIssues(meeting) > 0,
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
        <AppSidebar
          currentRouteId={currentRoute?.id ?? null}
          currentUserDetail={currentUserDetail}
          currentUserLabel={currentUserLabel}
          currentUserProgress={currentUserLevel.progress}
          iconForRoute={iconForRoute}
          isKo={isKo}
          navigateToRoute={navigateToRoute}
          routeBadge={sidebarBadgeForRoute}
          routes={PRIMARY_ROUTES}
          sections={SIDEBAR_SECTION_ORDER}
          setAgentsPageTab={setAgentsPageTab}
          wsConnected={wsConnected}
        />
      )}

      <div className="flex min-w-0 flex-1 flex-col overflow-hidden">
        <AppTopBar
          currentRoute={currentRoute ?? null}
            dismissNotification={dismissNotification}
            headerZIndex={SHELL_HEADER_Z_INDEX}
            isKo={isKo}
          navigateToRoute={navigateToRoute}
          notificationBadgeCount={notificationBadgeCount}
          popoverZIndex={SHELL_POPOVER_Z_INDEX}
          recentNotifications={recentNotifications}
          resolvedTheme={resolvedTheme}
          setShowCommandPalette={setShowCommandPalette}
          setShowNotificationPanel={setShowNotificationPanel}
          setShowTweaksPanel={setShowTweaksPanel}
          showNotificationPanel={showNotificationPanel}
          toggleShellTheme={toggleShellTheme}
          unresolvedMeetingsCount={unresolvedMeetingsCount}
        />

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
              <AppViewSkeleton
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
<AppShellRoutes
              ctx={{
                agents,
                agentsPageTab,
                agentsWithDispatched,
                allAgents,
                allDepartments,
                auditLogs,
                departments,
                handleSettingsSave,
                isKo,
                isMobileViewport,
                kanbanCards,
                kanbanSignalFocus,
                navigateToRoute,
                notifications,
                openDefaultAgentInfo,
                openOfficeAgentInfo,
                pushNotification,
                resolvedTheme,
                roundTableMeetings,
                selectedOfficeId,
                setAgentsPageTab,
                setKanbanCards,
                setKanbanSignalFocus,
                setRoundTableMeetings,
                setSessions,
                settings,
                stats,
                subAgents,
                taskDispatches,
                updateNotification,
                upsertKanbanCard,
                visibleDispatchedSessions,
                wsConnected,
                currentOfficeName,
                refreshAgents,
                refreshAllAgents,
                refreshAllDepartments,
                refreshDepartments,
                refreshOffices,
              }}
            />
          </Suspense>
        </main>
      </div>

      {isMobileViewport && (
        <AppMobileNavigation
          activeRouteId={activeMobileRouteId}
          bottomSheetZIndex={SHELL_BOTTOM_SHEET_Z_INDEX}
          iconForRoute={iconForRoute}
          isKo={isKo}
          moreMenuRef={mobileMoreMenuRef}
          moreOpen={showMobileMoreMenu}
          moreSections={mobileOverflowSections}
          navigateToRoute={navigateToRoute}
          primaryRoutes={mobilePrimaryRoutes}
          routeBadge={sidebarBadgeForRoute}
          setMoreOpen={setShowMobileMoreMenu}
          tabbarHeight={MOBILE_TABBAR_SAFE_AREA_HEIGHT}
          tabbarZIndex={SHELL_TABBAR_Z_INDEX}
        />
      )}

      <AppShellOverlays
        ctx={{
          accentPreset,
          allAgents,
          closeOfficeInfo,
          departments,
          dismissNotification,
          isKo,
          locale,
          modalZIndex: SHELL_MODAL_Z_INDEX,
          navigateToRoute,
          notifications,
          officeAgentState,
          officeInfoAgent,
          officeInfoMode,
          openDefaultAgentInfo,
          popoverZIndex: SHELL_POPOVER_Z_INDEX,
          refreshAgents,
          refreshAllAgents,
          refreshAuditLogs,
          refreshOffices,
          setAccentPreset,
          setShowCommandPalette,
          setShowShortcutHelp,
          setShowTweaksPanel,
          setThemePreference,
          shellToastZIndex: SHELL_TOAST_Z_INDEX,
          showCommandPalette,
          showShortcutHelp,
          showTweaksPanel,
          spriteMap,
          themePreference,
          tr,
          wsConnected,
        }}
      />
    </div>
  );
}
