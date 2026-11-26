import { useEffect, useRef } from "react";
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
  dashboardBootstrapSessionsQueryKey,
  dashboardBootstrapStatsQueryKey,
  fetchBootstrapSessions,
  fetchBootstrapStats,
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
  const sessionsQuery = useQuery({
    queryKey: dashboardBootstrapSessionsQueryKey,
    queryFn: fetchBootstrapSessions,
  });
  const statsQuery = useQuery({
    queryKey: dashboardBootstrapStatsQueryKey,
    queryFn: fetchBootstrapStats,
  });
  const data = bootstrapQuery.data;
  const { t } = useI18n();
  const { t: bootT } = useI18n(data?.settings.language);
  const tRef = useRef(bootT);
  tRef.current = data ? t : bootT;

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
      const prefix = tRef.current({ ko: "API 오류", en: "API error", ja: "APIエラー", zh: "API 错误" });
      pushNotification(`${prefix}: ${apiPath} - ${error.message}`, "error");
    });
    return () => onApiError(null);
  }, [pushNotification]);

  const handleWsEvent = (event: WSEvent) => {
    if (event.type === "kanban_card_created") {
      const card = event.payload as KanbanCard;
      if (card.status === "requested") {
        const prefix = tRef.current({ ko: "새 작업 접수", en: "New task requested", ja: "新しいタスクを受付", zh: "已接收新任务" });
        pushNotification(`${prefix}: ${card.title}`, "info");
      }
    }
  };

  const { wsConnected, lastEventTs } = useDashboardSocket(handleWsEvent);

  if (bootstrapQuery.isError) {
    return (
      <div className="flex h-screen items-center justify-center bg-[var(--bg)] text-[var(--fg-muted)]">
        <div role="alert" data-testid="bootstrap-error" className="max-w-md px-6 text-center">
          <p className="m-0 text-sm">
            {t({
              ko: "대시보드를 불러오지 못했어요.",
              en: "Could not load the dashboard.",
            })}
          </p>
          <button
            type="button"
            data-testid="bootstrap-retry"
            className="mt-4 inline-flex min-h-11 items-center justify-center rounded-md border border-[var(--line)] bg-[var(--bg-2)] px-4 text-sm text-[var(--fg)]"
            disabled={bootstrapQuery.isFetching}
            onClick={() => { void bootstrapQuery.refetch(); }}
          >
            {t({ ko: "다시 시도", en: "Try again" })}
          </button>
        </div>
      </div>
    );
  }

  if (!data) {
    return (
      <div className="flex h-screen items-center justify-center bg-[var(--bg)] text-[var(--fg-muted)]">
        <div role="status" data-testid="bootstrap-loading" className="text-center">
          {t({
            ko: "AgentDesk 대시보드 로딩 중...",
            en: "Loading AgentDesk Dashboard...",
          })}
        </div>
      </div>
    );
  }

  const resolvedSessions = data.sessions !== undefined ? data.sessions : sessionsQuery.data;
  const resolvedStats = data.stats !== undefined ? data.stats : (statsQuery.data ?? null);
  const initialStatsPending = data.stats === undefined && statsQuery.isPending;

  return (
    <WorkspaceProvider
      initialAgents={data.agents}
      initialDepartments={data.departments}
      initialSessions={resolvedSessions}
      pushNotification={pushNotification}
      language={data.settings.language}
    >
      <SettingsProvider
        initialSettings={data.settings}
        initialStats={resolvedStats}
        initialStatsPending={initialStatsPending}
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
