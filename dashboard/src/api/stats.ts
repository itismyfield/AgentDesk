import type { DashboardStats, TokenAnalyticsResponse } from "../types";
import { request, readCachedGet, TOKEN_ANALYTICS_TIMEOUT_MS, type CachedGetEntry } from "./httpClient";

export async function getStats(officeId?: string): Promise<DashboardStats> {
  const q = officeId ? `?officeId=${officeId}` : "";
  return request(`/api/stats${q}`);
}

export async function getTokenAnalytics(
  period: "7d" | "30d" | "90d" = "30d",
  opts?: { signal?: AbortSignal; forceRefresh?: boolean },
): Promise<TokenAnalyticsResponse> {
  // The endpoint now ships with `Cache-Control: max-age=15, swr=300` so a
  // background re-entry to /stats can paint instantly. The Refresh button
  // and any other "user explicitly asked for fresh data" caller passes
  // forceRefresh=true so we bypass:
  //   - the browser cache via fetch's `cache: "reload"` directive
  //   - the new server-side in-process cache via `&fresh=1` query param
  //     (the server short-circuits to its 30s memo without it).
  // Together these restore the explicit-refresh contract Codex flagged on
  // PR #1258 while still letting background traffic skip the ~9s scan.
  const force = opts?.forceRefresh ? "&fresh=1" : "";
  return request(`/api/token-analytics?period=${period}${force}`, {
    signal: opts?.signal,
    timeoutMs: TOKEN_ANALYTICS_TIMEOUT_MS,
    cache: opts?.forceRefresh ? "reload" : "default",
  });
}

export function getCachedTokenAnalytics(
  period: "7d" | "30d" | "90d" = "30d",
): CachedGetEntry<TokenAnalyticsResponse> | null {
  return readCachedGet<TokenAnalyticsResponse>(
    `/api/token-analytics?period=${period}`,
  );
}

// ── Home KPI sparklines (#1242) ──
export interface HomeKpiSeries {
  label: string;
  unit: string;
  values: number[];
}

export interface HomeKpiRateLimitProvider {
  provider: string;
  current_pct: number | null;
  unsupported: boolean;
  stale: boolean;
  reason: string | null;
  values: number[];
}

export interface HomeKpiRateLimit {
  label: string;
  unit: string;
  providers: HomeKpiRateLimitProvider[];
}

export interface HomeKpiTrendsResponse {
  days: number;
  generated_at: string;
  dates: string[];
  tokens: HomeKpiSeries;
  cost: HomeKpiSeries;
  in_progress: HomeKpiSeries;
  rate_limit: HomeKpiRateLimit;
}

export async function getHomeKpiTrends(
  days: number = 14,
  opts?: { signal?: AbortSignal },
): Promise<HomeKpiTrendsResponse> {
  return request(`/api/home/kpi-trends?days=${days}`, {
    signal: opts?.signal,
  });
}

// ── Kanban & Dispatches ──

// #2050 P2 finding 5 — surface server-side filters (status / repo_id /
// assignee_agent_id) the API already supports, so callers no longer have to
