import { lazy } from "react";
import { Navigate, Route, Routes } from "react-router";

import * as api from "../api/client";
import type { Agent, CompanySettings, DashboardStats, Department, DispatchedSession } from "../types";
import type { Notification } from "../components/NotificationCenter";
import type { AgentManagerTab } from "../components/agent-manager/useAgentManagerController";
import { DEFAULT_ROUTE_PATH, LEGACY_ROUTE_REDIRECTS, PRIMARY_ROUTES } from "./routes";
import HomeOverviewPage from "./HomeOverviewPage";

export interface AppShellRoutesContext {
  agents: Agent[];
  agentsPageTab: AgentManagerTab;
  agentsWithDispatched: Agent[];
  departments: Department[];
  handleSettingsSave: (patch: Record<string, unknown>) => Promise<void>;
  isKo: boolean;
  isMobileViewport: boolean;
  navigateToRoute: (path: string, options?: { agentsTab?: AgentManagerTab }) => void;
  notifications: Notification[];
  openDefaultAgentInfo: (agent: Agent) => void;
  pushNotification: (message: string, type?: Notification["type"]) => string;
  setAgentsPageTab: (tab: AgentManagerTab) => void;
  setSessions: React.Dispatch<React.SetStateAction<DispatchedSession[]>>;
  settings: CompanySettings;
  stats: DashboardStats | null;
  updateNotification: (id: string, message: string, type?: Notification["type"]) => void;
  visibleDispatchedSessions: DispatchedSession[];
  wsConnected: boolean;
  /** Wall-clock ms timestamp of the most recent WS event, or null pre-first. */
  wsLastEventTs: number | null;
  refreshAgents: () => void;
  refreshDepartments: () => void;
}

const StatsPageView = lazy(() => import("../components/StatsPageView"));
const OpsPageView = lazy(() => import("../components/OpsPageView"));
const RoutinesPageView = lazy(() => import("../components/RoutinesPageView"));
const CampaignsPanel = lazy(() => import("../components/campaigns/CampaignsPanel"));
const AgentManagerView = lazy(() => import("../components/AgentManagerView"));
const SettingsView = lazy(() => import("../components/SettingsView"));

export default function AppShellRoutes({ ctx }: { ctx: AppShellRoutesContext }) {
  const {
    agents,
    agentsPageTab,
    agentsWithDispatched,
    departments,
    handleSettingsSave,
    isKo,
    isMobileViewport,
    navigateToRoute,
    openDefaultAgentInfo,
    pushNotification,
    setAgentsPageTab,
    setSessions,
    settings,
    stats,
    visibleDispatchedSessions,
    wsConnected,
    wsLastEventTs,
    refreshAgents,
    refreshDepartments,
  } = ctx;

  const agentManager = (autoOpenCreate: boolean) => (
    <AgentManagerView
      agents={agents}
      departments={departments}
      language={settings.language}
      onAgentsChange={refreshAgents}
      onDepartmentsChange={refreshDepartments}
      sessions={visibleDispatchedSessions}
      onAssign={async (id, patch) => {
        const updated = await api.assignDispatchedSession(id, patch);
        setSessions((prev) => prev.map((session) => (session.id === updated.id ? updated : session)));
      }}
      onSelectAgent={openDefaultAgentInfo}
      activeTab={agentsPageTab}
      onTabChange={setAgentsPageTab}
      autoOpenCreate={autoOpenCreate}
      onAutoOpenConsumed={autoOpenCreate ? () => navigateToRoute("/agents") : undefined}
    />
  );

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
            wsLastEventTs={wsLastEventTs}
            stats={stats}
            agents={agentsWithDispatched}
            onOpenAgent={openDefaultAgentInfo}
          />
        }
      />
      <Route path="/agents" element={agentManager(false)} />
      <Route path="/agents/new" element={agentManager(true)} />
      <Route
        path="/stats"
        element={
          <StatsPageView
            settings={settings}
            stats={stats}
            agents={agents}
            sessions={visibleDispatchedSessions}
          />
        }
      />
      <Route path="/routines" element={<RoutinesPageView />} />
      <Route
        path="/campaigns"
        element={
          <div className="h-full overflow-auto p-4 pb-36 sm:p-6">
            <CampaignsPanel language={settings.language} />
          </div>
        }
      />
      <Route path="/ops" element={<OpsPageView wsConnected={wsConnected} isKo={isKo} />} />
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
      {LEGACY_ROUTE_REDIRECTS.map((redirect) => (
        <Route key={redirect.path} path={redirect.path} element={<Navigate replace to={redirect.to} />} />
      ))}
      {PRIMARY_ROUTES.flatMap((route) =>
        (route.aliases ?? []).map((alias) => (
          <Route key={`${route.id}:${alias}`} path={alias} element={<Navigate replace to={route.path} />} />
        )),
      )}
      <Route path="*" element={<Navigate replace to={DEFAULT_ROUTE_PATH} />} />
    </Routes>
  );
}
