import { lazy, Suspense } from "react";
import { WifiOff } from "lucide-react";

import { ToastOverlay } from "../components/NotificationCenter";
import { AppShortcutHelpModal } from "./AppShortcutHelpModal";
import { AppTweaksPanel } from "./AppTweaksPanel";
import { PALETTE_ROUTES } from "./routes";

const AgentInfoCard = lazy(() => import("../components/agent-manager/AgentInfoCard"));
const OfficeAgentDrawer = lazy(() => import("../components/office-view/OfficeAgentDrawer"));
const CommandPalette = lazy(() => import("../components/CommandPalette"));

export default function AppShellOverlays({ ctx }: { ctx: any }) {
  const {
    accentPreset,
    allAgents,
    closeOfficeInfo,
    departments,
    dismissNotification,
    isKo,
    locale,
    modalZIndex,
    navigateToRoute,
    notifications,
    officeAgentState,
    officeInfoAgent,
    officeInfoMode,
    openDefaultAgentInfo,
    popoverZIndex,
    refreshAgents,
    refreshAllAgents,
    refreshAuditLogs,
    refreshOffices,
    setAccentPreset,
    setShowCommandPalette,
    setShowShortcutHelp,
    setShowTweaksPanel,
    setThemePreference,
    shellToastZIndex,
    showCommandPalette,
    showShortcutHelp,
    showTweaksPanel,
    spriteMap,
    themePreference,
    tr,
    wsConnected,
  } = ctx;

  return (
    <>
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
          modalZIndex={modalZIndex}
            onClose={() => setShowShortcutHelp(false)}
          />
      )}

      {!wsConnected && (
        <div
          className="pointer-events-none fixed left-4 right-4 top-4 flex justify-center md:left-auto md:right-6"
          style={{ zIndex: shellToastZIndex }}
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
    </>
  );
}
