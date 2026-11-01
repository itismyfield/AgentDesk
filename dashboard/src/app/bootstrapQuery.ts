import * as api from "../api/client";
import type {
  Agent,
  CompanySettings,
  DashboardStats,
  Department,
  DispatchedSession,
} from "../types";
import { DEFAULT_SETTINGS } from "../types";

// Only what the shell needs on every page; screens load their own heavy data.
export interface BootstrapData {
  agents: Agent[];
  departments: Department[];
  sessions: DispatchedSession[];
  stats: DashboardStats | null;
  settings: CompanySettings;
}

export const dashboardBootstrapQueryKey = ["dashboard", "bootstrap"] as const;

function safe<T>(promise: Promise<T>, fallback: T): Promise<T> {
  return promise.catch((error) => {
    console.warn("[bootstrap] partial failure:", error);
    return fallback;
  });
}

export async function fetchDashboardBootstrap(): Promise<BootstrapData> {
  await safe(api.getSession(), { ok: false, csrf_token: "" });
  const [agents, departments, sessions, stats, settings] = await Promise.all([
    safe(api.getAgents(), [] as Agent[]),
    safe(api.getDepartments(), [] as Department[]),
    safe(api.getDispatchedSessions(true), [] as DispatchedSession[]),
    safe(api.getStats(), null as DashboardStats | null),
    safe(api.getSettings(), DEFAULT_SETTINGS as Partial<CompanySettings>),
  ]);
  return {
    agents,
    departments,
    sessions,
    stats,
    settings: { ...DEFAULT_SETTINGS, ...settings } as CompanySettings,
  };
}
