import { useEffect, useState } from "react";
import type {
  Agent,
  AuditLogEntry,
  CompanySettings,
  DashboardStats,
  Department,
  DispatchedSession,
  KanbanCard,
  Office,
  RoundTableMeeting,
  TaskDispatch,
  WSEvent,
} from "./types";
import { DEFAULT_SETTINGS } from "./types";
import * as api from "./api/client";
import { onApiError } from "./api/client";
import { KanbanProvider } from "./contexts/KanbanContext";
import { OfficeProvider } from "./contexts/OfficeContext";
import { SettingsProvider } from "./contexts/SettingsContext";
import {
  type Notification,
  useNotifications,
} from "./components/NotificationCenter";
import { useDashboardSocket } from "./app/useDashboardSocket";
import AppShell from "./app/AppShell";
import { useI18n } from "./i18n";

interface BootstrapData {
  offices: Office[];
  agents: Agent[];
  allAgents: Agent[];
  departments: Department[];
  allDepartments: Department[];
  sessions: DispatchedSession[];
  stats: DashboardStats | null;
  settings: CompanySettings;
  roundTableMeetings: RoundTableMeeting[];
  auditLogs: AuditLogEntry[];
  kanbanCards: KanbanCard[];
  taskDispatches: TaskDispatch[];
  selectedOfficeId: string | null;
}

export default function App() {
  const [data, setData] = useState<BootstrapData | null>(null);
  const { notifications, pushNotification, updateNotification, dismissNotification } =
    useNotifications();

  useEffect(() => {
    const lastFired = new Map<string, number>();
    onApiError((url, error) => {
      const apiPath = url.replace(/^\/api\//, "");
      const now = Date.now();
      const last = lastFired.get(url) ?? 0;
      if (now - last < 3000) return;
      lastFired.set(url, now);
      pushNotification(`API error: ${apiPath} - ${error.message}`, "error");
    });
    return () => onApiError(null);
  }, [pushNotification]);

  useEffect(() => {
    (async () => {
      // #2050 P2 finding 7 — guard every bootstrap call individually so a
      // single API hiccup (e.g. /api/settings 500) no longer wipes the
      // whole dashboard.
      const safe = <T,>(p: Promise<T>, fallback: T): Promise<T> =>
        p.catch((err) => {
          console.warn("[bootstrap] partial failure:", err);
          return fallback;
        });

      try {
        await safe(api.getSession(), { ok: false, csrf_token: "" });
        const offices = await safe(api.getOffices(), [] as Office[]);
        const defaultOfficeId = offices.length > 0 ? offices[0].id : undefined;
        const [
          allAgents,
          agents,
          allDepartments,
          departments,
          sessions,
          stats,
          settings,
          meetings,
          logs,
          cards,
          dispatches,
        ] = await Promise.all([
          safe(api.getAgents(), [] as Agent[]),
          safe(api.getAgents(defaultOfficeId), [] as Agent[]),
          safe(api.getDepartments(), [] as Department[]),
          safe(api.getDepartments(defaultOfficeId), [] as Department[]),
          safe(api.getDispatchedSessions(true), [] as DispatchedSession[]),
          safe(api.getStats(defaultOfficeId), null as DashboardStats | null),
          safe(api.getSettings(), DEFAULT_SETTINGS as Partial<CompanySettings>),
          safe(api.getRoundTableMeetings(), [] as RoundTableMeeting[]),
          safe(api.getAuditLogs(12), [] as AuditLogEntry[]),
          safe(api.getKanbanCards(), [] as KanbanCard[]),
          safe(api.getTaskDispatches({ limit: 200 }), [] as TaskDispatch[]),
        ]);
        const resolvedSettings = {
          ...DEFAULT_SETTINGS,
          ...settings,
        } as CompanySettings;
        setData({
          offices,
          agents,
          allAgents,
          departments,
          allDepartments,
          sessions,
          stats,
          settings: resolvedSettings,
          roundTableMeetings: meetings,
          auditLogs: logs,
          kanbanCards: cards,
          taskDispatches: dispatches,
          selectedOfficeId: defaultOfficeId ?? null,
        });
      } catch (error) {
        console.error("Bootstrap failed:", error);
        setData({
          offices: [],
          agents: [],
          allAgents: [],
          departments: [],
          allDepartments: [],
          sessions: [],
          stats: null,
          settings: DEFAULT_SETTINGS,
          roundTableMeetings: [],
          auditLogs: [],
          kanbanCards: [],
          taskDispatches: [],
          selectedOfficeId: null,
        });
      }
    })();
  }, []);

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
