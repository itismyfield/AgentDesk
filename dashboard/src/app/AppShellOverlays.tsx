import { createElement, lazy, Suspense, type Dispatch, type SetStateAction } from "react";

import type { AgentManagerTab } from "../components/agent-manager/useAgentManagerController";
import { ToastOverlay, type Notification } from "../components/NotificationCenter";
import { classifyAgents } from "../components/home/homeModel";
import { useWorkspace } from "../contexts/WorkspaceContext";
import type { UiLanguage } from "../i18n";
import type { Agent, Department } from "../types";
import { AppShortcutHelpModal } from "./AppShortcutHelpModal";
import { AppTweaksPanel } from "./AppTweaksPanel";
import { PRIMARY_ROUTES } from "./routes";
import { iconForRoute } from "./shellRouteIcons";
import type { AccentPreset, ThemePreference } from "./themePreferences";

const AgentInfoCard = lazy(() => import("../components/agent-manager/AgentInfoCard"));
const CommandPalette = lazy(() => import("../components/CommandPalette"));

const PALETTE_ROUTES = PRIMARY_ROUTES.map((route) => ({
  id: route.path,
  labelKo: route.labelKo,
  labelEn: route.labelEn,
  icon: createElement(iconForRoute(route.id), { size: 16, "aria-hidden": true }),
}));

export interface AppShellOverlaysContext {
  accentPreset: AccentPreset;
  agentInfoAgent: Agent | null;
  agents: Agent[];
  closeAgentInfo: () => void;
  departments: Department[];
  dismissNotification: (id: string) => void;
  focused: boolean;
  isKo: boolean;
  locale: UiLanguage;
  navigateToRoute: (path: string, options?: { agentsTab?: AgentManagerTab }) => void;
  notifications: Notification[];
  openDefaultAgentInfo: (agent: Agent) => void;
  popoverZIndex: number;
  pushNotification: (message: string, type?: Notification["type"]) => string;
  refreshAgents: () => void;
  setAccentPreset: Dispatch<SetStateAction<AccentPreset>>;
  setShowCommandPalette: Dispatch<SetStateAction<boolean>>;
  setShowShortcutHelp: Dispatch<SetStateAction<boolean>>;
  setShowTweaksPanel: Dispatch<SetStateAction<boolean>>;
  setThemePreference: Dispatch<SetStateAction<ThemePreference>>;
  showCommandPalette: boolean;
  showShortcutHelp: boolean;
  showTweaksPanel: boolean;
  themePreference: ThemePreference;
  tr: (ko: string, en: string) => string;
  wsConnected: boolean;
}

export default function AppShellOverlays({ ctx }: { ctx: AppShellOverlaysContext }) {
  const {
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
    popoverZIndex,
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
  } = ctx;
  const { sessions, sessionsFailed, visibleDispatchedSessions } = useWorkspace();
  const infoAgent = agentInfoAgent ? agents.find((agent) => agent.id === agentInfoAgent.id) ?? agentInfoAgent : null;
  const infoState = infoAgent ? classifyAgents([infoAgent], sessions, sessionsFailed)[0]?.state : undefined;

  return (
    <>
      <Suspense fallback={null}>
        {infoAgent && (
          <AgentInfoCard
            agent={infoAgent}
            state={infoState}
            isKo={isKo}
            locale={locale}
            tr={tr}
            departments={departments}
            onClose={closeAgentInfo}
            onAgentUpdated={refreshAgents}
          />
        )}
      </Suspense>

      <Suspense fallback={null}>
        {showCommandPalette && (
          <CommandPalette
            agents={agents}
            departments={departments}
            sessions={visibleDispatchedSessions}
            isKo={isKo}
            onSelectAgent={openDefaultAgentInfo}
            onNavigate={(path) => navigateToRoute(path)}
            onNotify={pushNotification}
            onClose={() => setShowCommandPalette(false)}
            routes={PALETTE_ROUTES}
            departmentRouteId="/agents"
          />
        )}
      </Suspense>

      {showTweaksPanel && !focused && (
        <AppTweaksPanel
          accentPreset={accentPreset}
          isKo={isKo}
          popoverZIndex={popoverZIndex}
          setAccentPreset={setAccentPreset}
          setShowTweaksPanel={setShowTweaksPanel}
          setThemePreference={setThemePreference}
          themePreference={themePreference}
        />
      )}

      <ToastOverlay notifications={notifications} onDismiss={dismissNotification} />

      {showShortcutHelp && (
        <AppShortcutHelpModal
          isKo={isKo}
          onClose={() => setShowShortcutHelp(false)}
        />
      )}
    </>
  );
}
