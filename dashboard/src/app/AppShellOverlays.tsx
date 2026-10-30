import { lazy, Suspense } from "react";

import { ToastOverlay } from "../components/NotificationCenter";
import { AppShortcutHelpModal } from "./AppShortcutHelpModal";
import { AppTweaksPanel } from "./AppTweaksPanel";
import { PALETTE_ROUTES } from "./routes";

const AgentInfoCard = lazy(() => import("../components/agent-manager/AgentInfoCard"));
const CommandPalette = lazy(() => import("../components/CommandPalette"));

export default function AppShellOverlays({ ctx }: { ctx: any }) {
  const {
    accentPreset,
    agentInfoAgent,
    allAgents,
    closeAgentInfo,
    departments,
    dismissNotification,
    isKo,
    locale,
    navigateToRoute,
    notifications,
    openDefaultAgentInfo,
    popoverZIndex,
    refreshAgents,
    refreshAllAgents,
    refreshAuditLogs,
    setAccentPreset,
    setShowCommandPalette,
    setShowShortcutHelp,
    setShowTweaksPanel,
    setThemePreference,
    showCommandPalette,
    showShortcutHelp,
    showTweaksPanel,
    spriteMap,
    themePreference,
    tr,
  } = ctx;

  return (
    <>
      <Suspense fallback={null}>
        {agentInfoAgent && (
          <AgentInfoCard
            agent={agentInfoAgent}
            spriteMap={spriteMap}
            isKo={isKo}
            locale={locale}
            tr={tr}
            departments={departments}
            onClose={closeAgentInfo}
            onAgentUpdated={() => {
              refreshAgents();
              refreshAllAgents();
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
            onSelectAgent={openDefaultAgentInfo}
            onNavigate={(path) => navigateToRoute(path)}
            onClose={() => setShowCommandPalette(false)}
            routes={PALETTE_ROUTES}
            departmentRouteId="/agents"
          />
        )}
      </Suspense>

      {showTweaksPanel && (
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
