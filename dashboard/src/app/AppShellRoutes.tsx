import { lazy } from "react";
import { Navigate, Route, Routes } from "react-router-dom";

import * as api from "../api/client";
import { DEFAULT_ROUTE_PATH, PRIMARY_ROUTES } from "./routes";
import HomeOverviewPage from "./HomeOverviewPage";

const OfficeView = lazy(() => import("../components/OfficeView"));
const AchievementsPage = lazy(() => import("../components/AchievementsPage"));
const StatsPageView = lazy(() => import("../components/StatsPageView"));
const OpsPageView = lazy(() => import("../components/OpsPageView"));
const KanbanTab = lazy(() => import("../components/agent-manager/KanbanTab"));
const AgentManagerView = lazy(() => import("../components/AgentManagerView"));
const MeetingsAndSkillsPage = lazy(() => import("../components/MeetingsAndSkillsPage"));
const SettingsView = lazy(() => import("../components/SettingsView"));

export default function AppShellRoutes({ ctx }: { ctx: any }) {
  const {
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
  } = ctx;

  return (
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
                        (meeting: any) => meeting.status === "in_progress",
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
                      setSessions((prev: any[]) =>
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
                path="/agents/new"
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
                      setSessions((prev: any[]) =>
                        prev.map((session) =>
                          session.id === updated.id ? updated : session,
                        ),
                      );
                    }}
                    onSelectAgent={openDefaultAgentInfo}
                    activeTab={agentsPageTab}
                    onTabChange={setAgentsPageTab}
                    autoOpenCreate
                    onAutoOpenConsumed={() => navigateToRoute("/agents")}
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
                        const result = await api.assignKanbanIssue(payload);
                        upsertKanbanCard(result.card);
                        assertAssignIssueTransitionComplete(result);
                      }}
                      onUpdateCard={async (id, patch) => {
                        const updated = await api.updateKanbanCard(id, patch);
                        upsertKanbanCard(updated);
                      }}
                      onRetryCard={async (id, payload) => {
                        const result = await api.retryKanbanCard(id, payload);
                        upsertKanbanCard(result.card);
                        assertKanbanDispatchMutationComplete(result, "Retry");
                      }}
                      onRedispatchCard={async (id, payload) => {
                        const result = await api.redispatchKanbanCard(id, payload);
                        upsertKanbanCard(result.card);
                        assertKanbanDispatchMutationComplete(result, "Redispatch");
                      }}
                      onDeleteCard={async (id: string) => {
                        await api.deleteKanbanCard(id);
                        setKanbanCards((prev: any[]) =>
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
                    isKo={isKo}
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
  );
}

function assertKanbanDispatchMutationComplete(
  result: api.KanbanDispatchMutationResponse,
  label: string,
) {
  if (result.next_action !== "none_required") {
    throw new Error(label + " requires follow-up: " + result.next_action);
  }
  if (!result.new_dispatch_id) {
    throw new Error(label + " response did not include a new dispatch id.");
  }
}

function assertAssignIssueTransitionComplete(
  result: api.AssignKanbanIssueResponse,
) {
  if (result.transition.ok && result.transition.next_action === "none_required") {
    return;
  }
  if (result.transition.error) {
    throw new Error("Issue assigned, but transition failed: " + result.transition.error);
  }
  throw new Error("Issue assigned, but follow-up is required: " + result.transition.next_action);
}
