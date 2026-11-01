import { useEffect } from "react";
import { useQuery } from "@tanstack/react-query";
import type { KanbanCard, WSEvent } from "./types";
import { onApiError } from "./api/client";
import { KanbanProvider } from "./contexts/KanbanContext";
import { WorkspaceProvider } from "./contexts/WorkspaceContext";
import { SettingsProvider } from "./contexts/SettingsContext";
import { useNotifications } from "./components/NotificationCenter";
import { useDashboardSocket } from "./app/useDashboardSocket";
import {
  dashboardBootstrapQueryKey,
  fetchDashboardBootstrap,
} from "./app/bootstrapQuery";
import AppShell from "./app/AppShell";
import { useI18n } from "./i18n";

export default function App() {
  const { notifications, pushNotification, updateNotification, dismissNotification } =
    useNotifications();
  const bootstrapQuery = useQuery({
    queryKey: dashboardBootstrapQueryKey,
    queryFn: fetchDashboardBootstrap,
  });

  useEffect(() => {
    const lastFired = new Map<string, number>();
    onApiError((url, error) => {
      // Throttle per endpoint (query stripped) so one outage can't flood toasts.
      const apiPath = url.replace(/^\/api\//, "");
      const throttleKey = url.replace(/\?.*$/, "");
      const now = Date.now();
      const last = lastFired.get(throttleKey) ?? 0;
      if (now - last < 3000) return;
      lastFired.set(throttleKey, now);
      pushNotification(`API 오류: ${apiPath} - ${error.message}`, "error");
    });
    return () => onApiError(null);
  }, [pushNotification]);

  const handleWsEvent = (event: WSEvent) => {
    if (event.type === "kanban_card_created") {
      const card = event.payload as KanbanCard;
      if (card.status === "requested") {
        pushNotification(`새 작업 접수: ${card.title}`, "info");
      }
    }
  };

  const { wsConnected, lastEventTs } = useDashboardSocket(handleWsEvent);
  const { t } = useI18n();
  const data = bootstrapQuery.data;

  if (!data) {
    return (
      <div className="flex h-screen items-center justify-center bg-gray-900 text-gray-400">
        <div className="text-center">
          <div className="mb-4 text-4xl">🐾</div>
          <div>
            {t({
              ko: "AgentDesk 대시보드 로딩 중...",
              en: "Loading AgentDesk Dashboard...",
            })}
          </div>
        </div>
      </div>
    );
  }

  return (
    <WorkspaceProvider
      initialAgents={data.agents}
      initialDepartments={data.departments}
      initialSessions={data.sessions}
      pushNotification={pushNotification}
    >
      <SettingsProvider
        initialSettings={data.settings}
        initialStats={data.stats}
      >
        <KanbanProvider>
          <AppShell
            wsConnected={wsConnected}
            wsLastEventTs={lastEventTs}
            notifications={notifications}
            pushNotification={pushNotification}
            updateNotification={updateNotification}
            dismissNotification={dismissNotification}
          />
        </KanbanProvider>
      </SettingsProvider>
    </WorkspaceProvider>
  );
}
