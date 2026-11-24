import { createElement, lazy, Suspense } from "react";

import { ToastOverlay } from "../components/NotificationCenter";
import { classifyAgents } from "../components/home/homeModel";
import { useWorkspace } from "../contexts/WorkspaceContext";
import { AppShortcutHelpModal } from "./AppShortcutHelpModal";
import { AppTweaksPanel } from "./AppTweaksPanel";
import { PRIMARY_ROUTES } from "./routes";
import { iconForRoute } from "./shellRouteIcons";

const AgentInfoCard = lazy(() => import("../components/agent-manager/AgentInfoCard"));
const CommandPalette = lazy(() => import("../components/CommandPalette"));

const PALETTE_ROUTES = PRIMARY_ROUTES.map((route) => ({
  id: route.path,
  labelKo: route.labelKo,
  labelEn: route.labelEn,
  icon: createElement(iconForRoute(route.id), { size: 16, "aria-hidden": true }),
}));

export default function AppShellOverlays({ ctx }: { ctx: any }) {
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
  const infoAgent = agentInfoAgent ? agents.find((agent: { id: string }) => agent.id === agentInfoAgent.id) ?? agentInfoAgent : null;
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
