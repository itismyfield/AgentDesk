import { useEffect, useState } from "react";
import { getCampaigns, type Campaign } from "../../api/campaigns";
import { ApiRequestError, request } from "../../api/httpClient";
import type { RoutineRecord } from "../../api/routines";
import {
  getServerCampaign,
  getServerCampaignSummaries,
  type ServerCampaignDocument,
  type ServerCampaignSummary,
} from "../../api/serverCampaigns";
import type { RuntimeConfigResponse } from "../../api/settingsRuntime";
import { pollWhileVisible } from "../../lib/pollWhileVisible";
import { isOpenServerCampaign, type HomeHealth, type HomeTurn, type RawUsageProvider } from "./homeModel";

export interface Loaded<T> {
  data: T | null;
  /** The latest read failed; `data` still holds the last answer, if any. */
  failed: boolean;
  /** Wall-clock ms of the last successful read. */
  at: number | null;
}

/** Reads on mount and again every `intervalMs` while the page is visible. `load` must be stable. */
export function usePolled<T>(load: () => Promise<T>, intervalMs: number): Loaded<T> {
  const [state, setState] = useState<Loaded<T>>({ data: null, failed: false, at: null });
  useEffect(() => {
    let active = true;
    const run = () => {
      load()
        .then((data) => { if (active) setState({ data, failed: false, at: Date.now() }); })
        .catch(() => { if (active) setState((previous) => ({ ...previous, failed: true })); });
    };
    run();
    const stop = pollWhileVisible(run, intervalMs);
    return () => { active = false; stop(); };
  }, [load, intervalMs]);
  return state;
}

/** Server campaigns whose stage track is read per poll, those under way first; it bounds the detail requests. */
export const DETAIL_CAMPAIGNS = 8;

export interface HomeCampaigns {
  legacy: Campaign[] | null;
  summaries: ServerCampaignSummary[] | null;
  documents: ServerCampaignDocument[];
}

export async function loadHomeCampaigns(): Promise<HomeCampaigns> {
  const [legacy, summaries] = await Promise.allSettled([
    getCampaigns().then((result) => result.campaigns),
    getServerCampaignSummaries(),
  ]);
  if (legacy.status === "rejected" && summaries.status === "rejected") throw legacy.reason;
  const open = summaries.status === "fulfilled"
    ? summaries.value.filter(isOpenServerCampaign).sort((left, right) => Number(left.view.display === "prepared") - Number(right.view.display === "prepared")).slice(0, DETAIL_CAMPAIGNS)
    : [];
  const documents = await Promise.allSettled(open.map((summary) => getServerCampaign(summary.view.campaign_id)));
  return {
    legacy: legacy.status === "fulfilled" ? legacy.value : null,
    summaries: summaries.status === "fulfilled" ? summaries.value : null,
    documents: documents.flatMap((result) => (result.status === "fulfilled" ? [result.value] : [])),
  };
}

// Without a server auth token, detail answers loopback peers only, so tailnet devices get 403 every time.
let healthDetailForbidden = false;

function toHealth(raw: unknown): HomeHealth {
  const source = (raw ?? {}) as HomeHealth;
  return { ...source, status: source.status ?? "unhealthy", providers: Array.isArray(source.providers) ? source.providers : [] };
}

async function readHealth(path: string): Promise<HomeHealth> {
  try {
    return toHealth(await request<unknown>(path, { suppressErrorToast: true }));
  } catch (error) {
    // An unhealthy server answers 503 with its status, which home shows instead of a lost connection.
    const body = error instanceof ApiRequestError && error.status === 503 ? error.body : null;
    if (typeof (body as { status?: unknown } | null)?.status === "string") return toHealth(body);
    throw error;
  }
}

/** Like `getHealthDetail`, but a 503 status body counts as an answer and failures stay quiet; only a read with no
 * status body throws. */
export async function loadHomeHealth(): Promise<HomeHealth> {
  if (!healthDetailForbidden) {
    try {
      return await readHealth("/api/health/detail");
    } catch (error) {
      healthDetailForbidden = error instanceof ApiRequestError && error.status === 403;
      if (!healthDetailForbidden) return { ...(await readHealth("/api/health")), publicOnly: "failed" };
    }
  }
  return { ...(await readHealth("/api/health")), publicOnly: "forbidden" };
}

export async function loadHomeRoutines(): Promise<RoutineRecord[]> {
  const data = await request<{ routines?: RoutineRecord[] }>("/api/routines", { suppressErrorToast: true });
  return data.routines ?? [];
}

export async function loadHomeUsage(): Promise<RawUsageProvider[]> {
  const data = await request<{ providers?: RawUsageProvider[] }>("/api/rate-limits", { suppressErrorToast: true });
  return data.providers ?? [];
}

/** Live turns of the given agents; an agent whose read fails is left out and keeps its plain cell. */
export async function loadAgentTurns(ids: string[]): Promise<Map<string, HomeTurn>> {
  const reads = await Promise.allSettled(ids.map((id) => request<HomeTurn>(`/api/agents/${encodeURIComponent(id)}/turn`, { suppressErrorToast: true })));
  return new Map(reads.flatMap((read, index) => (read.status === "fulfilled" ? [[ids[index], read.value] as const] : [])));
}

export function loadRuntimeConfig(): Promise<RuntimeConfigResponse> {
  return request<RuntimeConfigResponse>("/api/settings/runtime-config", { suppressErrorToast: true });
}

