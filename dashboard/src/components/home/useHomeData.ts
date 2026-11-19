import { useEffect, useState } from "react";
import { getCampaigns, type Campaign } from "../../api/campaigns";
import { request } from "../../api/httpClient";
import type { RoutineRecord } from "../../api/routines";
import {
  getServerCampaign,
  getServerCampaignSummaries,
  type ServerCampaignDocument,
  type ServerCampaignSummary,
} from "../../api/serverCampaigns";
import type { RuntimeConfigResponse } from "../../api/settingsRuntime";
import { pollWhileVisible } from "../../lib/pollWhileVisible";
import { isOpenServerCampaign, type RawUsageProvider } from "./homeModel";

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

/** Server campaigns whose stage track is read in full; the rest of the card only needs summaries. */
export const SHOWN_CAMPAIGNS = 4;

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
  const open = summaries.status === "fulfilled" ? summaries.value.filter(isOpenServerCampaign).slice(0, SHOWN_CAMPAIGNS) : [];
  const documents = await Promise.allSettled(open.map((summary) => getServerCampaign(summary.view.campaign_id)));
  return {
    legacy: legacy.status === "fulfilled" ? legacy.value : null,
    summaries: summaries.status === "fulfilled" ? summaries.value : null,
    documents: documents.flatMap((result) => (result.status === "fulfilled" ? [result.value] : [])),
  };
}

export async function loadHomeRoutines(): Promise<RoutineRecord[]> {
  const data = await request<{ routines?: RoutineRecord[] }>("/api/routines", { suppressErrorToast: true });
  return data.routines ?? [];
}

export async function loadHomeUsage(): Promise<RawUsageProvider[]> {
  const data = await request<{ providers?: RawUsageProvider[] }>("/api/rate-limits", { suppressErrorToast: true });
  return data.providers ?? [];
}

export function loadRuntimeConfig(): Promise<RuntimeConfigResponse> {
  return request<RuntimeConfigResponse>("/api/settings/runtime-config", { suppressErrorToast: true });
}

