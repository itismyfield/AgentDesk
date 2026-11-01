import { Suspense, useCallback, useEffect, useMemo, useState } from "react";
import { useLocation, useNavigate } from "react-router";
import type { Agent, CompanySettings } from "../types";
import { DEFAULT_SETTINGS } from "../types";
import * as api from "../api/client";
import { useWorkspace } from "../contexts/WorkspaceContext";
import { useSettings } from "../contexts/SettingsContext";
import type { Notification } from "../components/NotificationCenter";
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
import { MOBILE_PRIMARY_ROUTE_IDS, SIDEBAR_SECTION_ORDER } from "./shellNavigationConfig";
import { iconForRoute } from "./shellRouteIcons";
import type { AgentManagerTab } from "../components/agent-manager/useAgentManagerController";

interface AppShellProps {
  wsConnected: boolean;
  /**
   * Wall-clock ms timestamp of the latest WS event (`null` before first event).
   * Drives the dashboard freshness indicator.
   */
  wsLastEventTs: number | null;
  notifications: Notification[];
  pushNotification: (message: string, type?: Notification["type"]) => string;
  updateNotification: (
    id: string,
    message: string,
    type?: Notification["type"],
  ) => void;
  dismissNotification: (id: string) => void;
}

const MOBILE_TABBAR_SAFE_AREA_HEIGHT = "calc(4rem + env(safe-area-inset-bottom))";

// Keep persistent shell chrome below route-level backdrops and modals.
const ROUTE_OVERLAY_BASE_Z_INDEX = 50;
const SHELL_HEADER_Z_INDEX = ROUTE_OVERLAY_BASE_Z_INDEX - 30;
const SHELL_POPOVER_Z_INDEX = ROUTE_OVERLAY_BASE_Z_INDEX - 10;
const SHELL_TABBAR_Z_INDEX = ROUTE_OVERLAY_BASE_Z_INDEX - 20;
const SHELL_BOTTOM_SHEET_Z_INDEX = ROUTE_OVERLAY_BASE_Z_INDEX - 5;
export default function AppShell({
  wsConnected,
  wsLastEventTs,
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
    agents,
    departments,
    setSessions,
    visibleDispatchedSessions,
    agentsWithDispatched,
    refreshAgents,
    refreshDepartments,
  } = useWorkspace();

  const [agentInfoAgent, setAgentInfoAgent] = useState<Agent | null>(null);
  const [showCommandPalette, setShowCommandPalette] = useState(false);
  const [showShortcutHelp, setShowShortcutHelp] = useState(false);
  const [showNotificationPanel, setShowNotificationPanel] = useState(false);
  const [showTweaksPanel, setShowTweaksPanel] = useState(false);
  const [showMobileMoreMenu, setShowMobileMoreMenu] = useState(false);
  const mobileMoreMenuRef = useFocusTrap(showMobileMoreMenu);
  const [agentsPageTab, setAgentsPageTab] = useState<AgentManagerTab>("agents");
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

  const unreadCount = notifications.filter(
    (notification) => Date.now() - notification.ts < 60_000,
  ).length;
  // Cards waiting on a person (review or blocked) badge the campaigns tab.
  const attentionCount = (stats?.kanban.review_queue ?? 0) + (stats?.kanban.blocked ?? 0);
  const resolvedTheme = useMemo(
    () => resolveThemePreference(themePreference, prefersDarkScheme),
    [prefersDarkScheme, themePreference],
  );
  const recentNotifications = notifications.slice(0, 6);

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
    },
    [setSettings, settings],
  );

  const navigateToRoute = useCallback(
    (path: string, options?: { agentsTab?: AgentManagerTab }) => {
      setShowMobileMoreMenu(false);
      if (options?.agentsTab) {
        setAgentsPageTab(options.agentsTab);
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

  const openDefaultAgentInfo = useCallback((agent: Agent) => {
    setAgentInfoAgent(agent);
  }, []);

  const closeAgentInfo = useCallback(() => {
    setAgentInfoAgent(null);
  }, []);

  const toggleShellTheme = useCallback(() => {
    setThemePreference((currentPreference) => {
      const activeTheme =
        currentPreference === "auto" ? resolvedTheme : currentPreference;
      return activeTheme === "dark" ? "light" : "dark";
    });
  }, [resolvedTheme]);

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
  const mobileOverflowRoutes = useMemo(
    () => PRIMARY_ROUTES.filter((route) => !MOBILE_PRIMARY_ROUTE_IDS.includes(route.id)),
    [],
  );
  const activeMobileRouteId =
    showMobileMoreMenu ||
    (currentRoute && !MOBILE_PRIMARY_ROUTE_IDS.includes(currentRoute.id))
      ? "more"
      : currentRoute?.id ?? "home";
  const sidebarBadgeForRoute = useCallback(
    (routeId: AppRouteId): number | undefined => {
      return routeId === "campaigns" && attentionCount > 0 ? attentionCount : undefined;
    },
    [attentionCount],
  );

  return (
    <div
      className="fixed inset-0 flex overflow-hidden"
      style={{
        background: "var(--th-bg-primary)",
      }}
    >
      {!isMobileViewport && (
        <AppSidebar
          currentRouteId={currentRoute?.id ?? null}
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
          wsConnected={wsConnected}
          currentRoute={currentRoute ?? null}
          dismissNotification={dismissNotification}
          headerZIndex={SHELL_HEADER_Z_INDEX}
          isKo={isKo}
          isMobileViewport={isMobileViewport}
          notificationBadgeCount={unreadCount}
          popoverZIndex={SHELL_POPOVER_Z_INDEX}
          recentNotifications={recentNotifications}
          resolvedTheme={resolvedTheme}
          setShowCommandPalette={setShowCommandPalette}
          setShowNotificationPanel={setShowNotificationPanel}
          setShowTweaksPanel={setShowTweaksPanel}
          showNotificationPanel={showNotificationPanel}
          toggleShellTheme={toggleShellTheme}
        />

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
                departments,
                handleSettingsSave,
                isKo,
                isMobileViewport,
                navigateToRoute,
                notifications,
                openDefaultAgentInfo,
                pushNotification,
                setAgentsPageTab,
                setSessions,
                settings,
                stats,
                updateNotification,
                visibleDispatchedSessions,
                wsConnected,
                wsLastEventTs,
                refreshAgents,
                refreshDepartments,
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
          moreRoutes={mobileOverflowRoutes}
          navigateToRoute={navigateToRoute}
          primaryRoutes={mobilePrimaryRoutes}
          resolvedTheme={resolvedTheme}
          routeBadge={sidebarBadgeForRoute}
          setMoreOpen={setShowMobileMoreMenu}
          setShowTweaksPanel={setShowTweaksPanel}
          toggleShellTheme={toggleShellTheme}
          tabbarHeight={MOBILE_TABBAR_SAFE_AREA_HEIGHT}
          tabbarZIndex={SHELL_TABBAR_Z_INDEX}
        />
      )}

      <AppShellOverlays
        ctx={{
          accentPreset,
          agentInfoAgent,
          agents,
          closeAgentInfo,
          departments,
          dismissNotification,
          isKo,
          locale,
          navigateToRoute,
          notifications,
          openDefaultAgentInfo,
          popoverZIndex: SHELL_POPOVER_Z_INDEX,
          refreshAgents,
          setAccentPreset,
          setShowCommandPalette,
          setShowShortcutHelp,
          setShowTweaksPanel,
          setThemePreference,
          showCommandPalette,
          showShortcutHelp,
          showTweaksPanel,
          themePreference,
          tr,
          wsConnected,
        }}
      />
    </div>
  );
}
