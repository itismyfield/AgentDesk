import { useEffect } from "react";
import { useQuery } from "@tanstack/react-query";
import type { KanbanCard, WSEvent } from "./types";
import { onApiError } from "./api/client";
import { KanbanProvider } from "./contexts/KanbanContext";
import { OfficeProvider } from "./contexts/OfficeContext";
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
      // #2050 P3 finding 23 — normalize throttle key to endpoint without
      // query so the same base path under different filters shares a
      // throttle slot. Previously /api/dispatches?status=A and
      // /api/dispatches?status=B were independent keys, allowing a single
      // server outage to flood the toast stack.
      const apiPath = url.replace(/^\/api\//, "");
      const throttleKey = url.replace(/\?.*$/, "");
      const now = Date.now();
      const last = lastFired.get(throttleKey) ?? 0;
      if (now - last < 3000) return;
      lastFired.set(throttleKey, now);
      pushNotification(`API error: ${apiPath} - ${error.message}`, "error");
    });
    return () => onApiError(null);
  }, [pushNotification]);

  const handleWsEvent = (event: WSEvent) => {
    if (event.type === "kanban_card_created") {
      const card = event.payload as KanbanCard;
      if (card.status === "requested") {
        pushNotification(`칸반 요청 발사: ${card.title}`, "info");
      }
    }
  };

  const { wsConnected } = useDashboardSocket(handleWsEvent);
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
    <OfficeProvider
      initialOffices={data.offices}
      initialAgents={data.agents}
      initialAllAgents={data.allAgents}
      initialDepartments={data.departments}
      initialAllDepartments={data.allDepartments}
      initialSessions={data.sessions}
      initialRoundTableMeetings={data.roundTableMeetings}
      initialAuditLogs={data.auditLogs}
      initialSelectedOfficeId={data.selectedOfficeId}
      pushNotification={pushNotification}
    >
      <SettingsProvider
        initialSettings={data.settings}
        initialStats={data.stats}
      >
        <KanbanProvider
          initialCards={data.kanbanCards}
          initialDispatches={data.taskDispatches}
        >
          <AppShell
            wsConnected={wsConnected}
            notifications={notifications}
            pushNotification={pushNotification}
            updateNotification={updateNotification}
            dismissNotification={dismissNotification}
          />
        </KanbanProvider>
      </SettingsProvider>
    </OfficeProvider>
  );
}
