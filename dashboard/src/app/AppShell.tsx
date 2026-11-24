import { Suspense, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useLocation, useNavigate } from "react-router";
import type { Agent, CompanySettings } from "../types";
import { DEFAULT_SETTINGS } from "../types";
import * as api from "../api/client";
import { useWorkspace } from "../contexts/WorkspaceContext";
import { useSettings } from "../contexts/SettingsContext";
import type { Notification } from "../components/NotificationCenter";
import { useFocusTrap } from "../components/common/overlay";
import { agentName, buildRunningNowTasks, classifyAgents } from "../components/home/homeModel";
import { reconcileStaleSessionTurn, stopAgentTurn, useAgentTurns } from "../components/home/useHomeData";
import { MOBILE_LAYOUT_MEDIA_QUERY, TABLET_RAIL_MEDIA_QUERY } from "./breakpoints";
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
import { ShellFocusContext, ShellWindowFocusContext, type ShellFocus } from "./shellFocus";
import { MOBILE_PRIMARY_ROUTE_IDS, SIDEBAR_SECTION_ORDER } from "./shellNavigationConfig";
import { iconForRoute } from "./shellRouteIcons";
import type { AgentManagerTab } from "../components/agent-manager/useAgentManagerController";

interface AppShellProps {
  wsConnected: boolean;
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
    sessions,
    setSessions,
    sessionsFailed,
    visibleDispatchedSessions,
    agentsWithDispatched,
    refreshAgents,
    refreshDepartments,
  } = useWorkspace();

  const mainRef = useRef<HTMLElement>(null);
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
  const [isTabletRailViewport, setIsTabletRailViewport] = useState(() => {
    if (typeof window === "undefined") return false;
    return window.matchMedia(TABLET_RAIL_MEDIA_QUERY).matches;
  });
  const [sidebarOverride, setSidebarOverride] = useState<"collapsed" | "expanded" | null>(null);

  const [focusRequest, setFocusRequest] = useState<ShellFocus | null>(null);
  const focused = focusRequest === "window" && !isMobileViewport;
  const showTopBar = !focused && !(focusRequest === "topBar" && isMobileViewport);
  const sidebarCollapsed = sidebarOverride !== null ? sidebarOverride === "collapsed" : isTabletRailViewport;
  const modalOverlayOpen = showCommandPalette || Boolean(agentInfoAgent) || showShortcutHelp || showMobileMoreMenu;

  const workingAgentIds = useMemo(
    () =>
      classifyAgents(agents, sessions, sessionsFailed)
        .filter((entry) => entry.state === "working")
        .map((entry) => entry.agent.id),
    [agents, sessions, sessionsFailed],
  );
  const runningTurns = useAgentTurns(workingAgentIds);
  const runningTasks = useMemo(
    () =>
      buildRunningNowTasks({
        agents,
        sessions,
        sessionsFailed,
        turns: runningTurns,
        tr,
        isKo,
      }),
    [agents, sessions, sessionsFailed, runningTurns, tr, isKo],
  );

  const unreadCount = notifications.filter(
    (notification) => Date.now() - notification.ts < 60_000,
  ).length;
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
    const media = window.matchMedia(TABLET_RAIL_MEDIA_QUERY);
    const sync = () => {
      setIsTabletRailViewport(media.matches);
      setSidebarOverride(null);
    };
    setIsTabletRailViewport(media.matches);
    media.addEventListener("change", sync);
    return () => media.removeEventListener("change", sync);
  }, []);

  // #8369: While a full-window editor holds the window, mark background siblings outside the editor inert.
  useLayoutEffect(() => {
    if (!focused || !mainRef.current) return;
    const root = mainRef.current;
    const editor = root.querySelector<HTMLElement>(".campaign-pipeline.fixed, .campaign-relations-editor");
    if (!editor) return;
    const marked: HTMLElement[] = [];
    let current: HTMLElement | null = editor;
    while (current && current !== root) {
      const parent: HTMLElement | null = current.parentElement;
      if (!parent) break;
      for (const child of Array.from(parent.children)) {
        if (child !== current && child instanceof HTMLElement && child.tagName !== "DIALOG" && !child.inert) {
          child.inert = true;
          marked.push(child);
        }
      }
      current = parent;
    }
    return () => {
      for (const node of marked) node.inert = false;
    };
  }, [focused, location.key]);

  useEffect(() => {
    setShowNotificationPanel(false);
    setShowTweaksPanel(false);
    setShowMobileMoreMenu(false);
  }, [location.key]);

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
      const latest = await api.getSettings();
      await api.saveSettings({ ...latest, ...patch } as CompanySettings);
      const refreshed = await api.getSettings();
      setSettings({ ...DEFAULT_SETTINGS, ...refreshed } as CompanySettings);
    },
    [setSettings],
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

  const handleOpenRunningTask = useCallback((taskId: string) => {
    if (taskId.startsWith("agent:")) {
      const targetId = taskId.slice("agent:".length);
      const found = agents.find((agent) => agent.id === targetId);
      if (found) {
        openDefaultAgentInfo(found);
        return;
      }
    }
    navigateToRoute("/agents");
  }, [agents, navigateToRoute, openDefaultAgentInfo]);

  const handleStopRunningTask = useCallback(async (taskId: string) => {
    if (taskId.startsWith("agent:")) {
      const agentId = taskId.slice("agent:".length);
      const target = agents.find((agent) => agent.id === agentId);
      const label = target ? agentName(target, isKo) : agentId;
      const confirmed = typeof window === "undefined" || typeof window.confirm !== "function"
        ? true
        : window.confirm(tr(`${label}의 실행 중인 턴을 중단할까요?`, `Stop ${label}'s running turn?`));
      if (!confirmed) return;
      try {
        const res = await stopAgentTurn(agentId);
        if (res.session_key) {
          setSessions((prev) =>
            prev.map((s) => (s.session_key === res.session_key ? { ...s, status: "disconnected" } : s)),
          );
        }
        refreshAgents();
        pushNotification(tr(`${label} 턴을 중단했어요.`, `Stopped ${label}'s turn.`), "success");
      } catch (error) {
        pushNotification(
          error instanceof Error ? error.message : tr("턴 중단에 실패했어요.", "Failed to stop turn."),
          "error",
        );
      }
      return;
    }
    if (taskId.startsWith("session:")) {
      const sessionKey = taskId.slice("session:".length);
      const confirmed = typeof window === "undefined" || typeof window.confirm !== "function"
        ? true
        : window.confirm(tr("이 미연결 세션의 멈춘 턴 정리를 시도할까요?", "Try reconciling this unlinked session's turn?"));
      if (!confirmed) return;
      try {
        const res = await reconcileStaleSessionTurn(sessionKey);
        if (res.reconciled) {
          setSessions((prev) =>
            prev.map((s) => (s.session_key === sessionKey ? { ...s, status: "idle", last_seen_at: Date.now() } : s)),
          );
          pushNotification(tr("세션 턴을 정리했어요.", "Reconciled session turn."), "success");
        } else {
          pushNotification(
            tr("현재 살아 있는 미연결 세션이에요. 에이전트 화면에서 확인해요.", "This unlinked lane is actively running; opening Agents."),
            "info",
          );
          navigateToRoute("/agents");
        }
      } catch (error) {
        pushNotification(
          error instanceof Error ? error.message : tr("세션 정리에 실패했어요.", "Failed to reconcile session."),
          "error",
        );
      }
    }
  }, [agents, isKo, navigateToRoute, pushNotification, refreshAgents, setSessions, tr]);

  const toggleShellTheme = useCallback(() => {
    setThemePreference((currentPreference) => {
      const activeTheme =
        currentPreference === "auto" ? resolvedTheme : currentPreference;
      return activeTheme === "dark" ? "light" : "dark";
    });
  }, [resolvedTheme]);

  const toggleSidebarCollapsed = useCallback(() => {
    setSidebarOverride((prev) => {
      const currentlyCollapsed = prev !== null ? prev === "collapsed" : isTabletRailViewport;
      return currentlyCollapsed ? "expanded" : "collapsed";
    });
  }, [isTabletRailViewport]);

  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      if (focused) return;
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
  }, [focused, navigateToRoute]);

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
      <div
        data-testid="app-shell-frame"
        inert={modalOverlayOpen || undefined}
        className="flex min-w-0 flex-1 overflow-hidden"
      >
        {!isMobileViewport && !focused && (
          <AppSidebar
            collapsed={sidebarCollapsed}
            onToggleCollapsed={toggleSidebarCollapsed}
            currentRouteId={currentRoute?.id ?? null}
            iconForRoute={iconForRoute}
            isKo={isKo}
            routeBadge={sidebarBadgeForRoute}
            routes={PRIMARY_ROUTES}
            sections={SIDEBAR_SECTION_ORDER}
            setAgentsPageTab={setAgentsPageTab}
            wsConnected={wsConnected}
            wsLastEventTs={wsLastEventTs}
          />
        )}

        <div className="flex min-w-0 flex-1 flex-col overflow-hidden">
          {showTopBar && (
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
              runningTasks={runningTasks}
              onOpenRunningTask={handleOpenRunningTask}
              onStopRunningTask={(taskId) => void handleStopRunningTask(taskId)}
              onViewAllRunning={() => navigateToRoute(currentRoute?.id === "home" ? "/agents" : "/home")}
              setShowCommandPalette={setShowCommandPalette}
              setShowNotificationPanel={setShowNotificationPanel}
              setShowTweaksPanel={setShowTweaksPanel}
              showNotificationPanel={showNotificationPanel}
              toggleShellTheme={toggleShellTheme}
            />
          )}

          <main
            ref={mainRef}
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
              <ShellFocusContext.Provider value={setFocusRequest}>
                <ShellWindowFocusContext.Provider value={focused}>
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
                </ShellWindowFocusContext.Provider>
              </ShellFocusContext.Provider>
            </Suspense>
          </main>
        </div>
      </div>

      {isMobileViewport && (
        <AppMobileNavigation
          activeRouteId={activeMobileRouteId}
          currentRouteId={currentRoute?.id ?? null}
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
          focused,
          isKo,
          locale,
          navigateToRoute,
          notifications,
          openDefaultAgentInfo,
          popoverZIndex: SHELL_POPOVER_Z_INDEX,
          pushNotification,
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
