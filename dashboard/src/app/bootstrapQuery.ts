import * as api from "../api/client";
import type {
  Agent,
  CompanySettings,
  DashboardStats,
  DispatchedSession,
} from "../types";
import { DEFAULT_SETTINGS } from "../types";

// Shell essentials that gate the first frame; heavy session/stats payloads stream in parallel without blocking AppShell.
export interface BootstrapData {
  agents: Agent[];
  /** Present when supplied by a caller/mock; otherwise loaded in parallel via fetchBootstrapSessions. */
  sessions?: DispatchedSession[] | null;
  /** Present when supplied by a caller/mock; otherwise loaded in parallel via fetchBootstrapStats. */
  stats?: DashboardStats | null;
  settings: CompanySettings;
}

export const dashboardBootstrapQueryKey = ["dashboard", "bootstrap"] as const;
export const dashboardBootstrapSessionsQueryKey = ["dashboard", "bootstrap", "sessions"] as const;
export const dashboardBootstrapStatsQueryKey = ["dashboard", "bootstrap", "stats"] as const;

function safe<T>(promise: Promise<T>, fallback: T): Promise<T> {
  return promise.catch((error) => {
    console.warn("[bootstrap] partial failure:", error);
    return fallback;
  });
}

export async function fetchDashboardBootstrap(): Promise<BootstrapData> {
  const [agents, settings] = await Promise.all([
    safe(api.getAgents(), [] as Agent[]),
    safe(api.getSettings(), DEFAULT_SETTINGS as Partial<CompanySettings>),
  ]);
  return {
    agents,
    settings: { ...DEFAULT_SETTINGS, ...settings } as CompanySettings,
  };
}

export function fetchBootstrapSessions(): Promise<DispatchedSession[] | null> {
  return safe(api.getDispatchedSessions(true), null as DispatchedSession[] | null);
}

export function fetchBootstrapStats(): Promise<DashboardStats | null> {
  return safe(api.getStats(), null as DashboardStats | null);
}
