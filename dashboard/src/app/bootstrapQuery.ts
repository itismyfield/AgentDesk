import * as api from "../api/client";
import type {
  Agent,
  AuditLogEntry,
  CompanySettings,
  DashboardStats,
  Department,
  DispatchedSession,
  KanbanCard,
  RoundTableMeeting,
  TaskDispatch,
} from "../types";
import { DEFAULT_SETTINGS } from "../types";

export interface BootstrapData {
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
}

export const dashboardBootstrapQueryKey = ["dashboard", "bootstrap"] as const;

const FALLBACK_BOOTSTRAP_DATA: BootstrapData = {
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
};

function safe<T>(promise: Promise<T>, fallback: T): Promise<T> {
  return promise.catch((error) => {
    console.warn("[bootstrap] partial failure:", error);
    return fallback;
  });
}

export async function fetchDashboardBootstrap(): Promise<BootstrapData> {
  try {
    await safe(api.getSession(), { ok: false, csrf_token: "" });
    const [
      agents,
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
      safe(api.getDepartments(), [] as Department[]),
      safe(api.getDispatchedSessions(true), [] as DispatchedSession[]),
      safe(api.getStats(), null as DashboardStats | null),
      safe(api.getSettings(), DEFAULT_SETTINGS as Partial<CompanySettings>),
      safe(api.getRoundTableMeetings(), [] as RoundTableMeeting[]),
      safe(api.getAuditLogs(12), [] as AuditLogEntry[]),
      safe(api.getKanbanCards(), [] as KanbanCard[]),
      safe(api.getTaskDispatches({ limit: 200 }), [] as TaskDispatch[]),
    ]);

    return {
      agents,
      allAgents: agents,
      departments,
      allDepartments: departments,
      sessions,
      stats,
      settings: {
        ...DEFAULT_SETTINGS,
        ...settings,
      } as CompanySettings,
      roundTableMeetings: meetings,
      auditLogs: logs,
      kanbanCards: cards,
      taskDispatches: dispatches,
    };
  } catch (error) {
    console.error("Bootstrap failed:", error);
    return FALLBACK_BOOTSTRAP_DATA;
  }
}
