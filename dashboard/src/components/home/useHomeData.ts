import { useEffect, useRef, useState } from "react";
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
import { isOpenServerCampaign, type HomeHealth, type HomeTurn, type RawUsageProvider, type SourceState } from "./homeModel";

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
  /** Open server campaigns whose detail read failed, and those past `DETAIL_CAMPAIGNS`: their to-dos are unknown. */
  detailFailed: number;
  detailsCut: number;
}

export async function loadHomeCampaigns(): Promise<HomeCampaigns> {
  const [legacy, summaries] = await Promise.allSettled([
    getCampaigns().then((result) => result.campaigns),
    getServerCampaignSummaries(),
  ]);
  if (legacy.status === "rejected" && summaries.status === "rejected") throw legacy.reason;
  const open = summaries.status === "fulfilled"
    ? summaries.value.filter(isOpenServerCampaign).sort((left, right) => Number(left.view.display === "prepared") - Number(right.view.display === "prepared"))
    : [];
  const documents = await Promise.allSettled(open.slice(0, DETAIL_CAMPAIGNS).map((summary) => getServerCampaign(summary.view.campaign_id)));
  return {
    legacy: legacy.status === "fulfilled" ? legacy.value : null,
    summaries: summaries.status === "fulfilled" ? summaries.value : null,
    documents: documents.flatMap((result) => (result.status === "fulfilled" ? [result.value] : [])),
    detailFailed: documents.filter((result) => result.status === "rejected").length,
    detailsCut: Math.max(0, open.length - DETAIL_CAMPAIGNS),
  };
}

/** The campaign side of the to-dos: a failed list fails it, and unread campaign details leave it partial. */
export function campaignTodoSource({ data, failed }: Loaded<HomeCampaigns>): SourceState {
  if (failed || (data && !data.summaries)) return "failed";
  if (!data) return "loading";
  return data.detailFailed > 0 || data.detailsCut > 0 ? "partial" : "ok";
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

/** Turns of the working agents, cached per agent: one that starts working is read at once and the rest keep the
 * polling cycle. Each turn read captures tmux on the server, so a change in who works never rereads everyone. */
export function useAgentTurns(ids: string[], intervalMs = 30_000): Map<string, HomeTurn> {
  const key = ids.join("|");
  const [turns, setTurns] = useState<Map<string, HomeTurn>>(() => new Map());
  const wanted = useRef<string[]>([]);
  // Latest read per agent: an answer merges only for the agents whose newest read it still is.
  const generation = useRef(new Map<string, number>());
  const alive = useRef(true);
  const read = useRef((targets: string[]) => {
    if (targets.length === 0) return;
    const sent = targets.map((id) => {
      const next = (generation.current.get(id) ?? 0) + 1;
      generation.current.set(id, next);
      return next;
    });
    void loadAgentTurns(targets).then((answer) => {
      const fresh = targets.filter((id, index) => generation.current.get(id) === sent[index]);
      if (!alive.current || fresh.length === 0) return;
      setTurns((previous) => {
        const merged = new Map([...previous].filter(([id]) => !fresh.includes(id)));
        for (const id of fresh) if (answer.has(id)) merged.set(id, answer.get(id)!);
        return new Map([...merged].filter(([id]) => wanted.current.includes(id)));
      });
    });
  });
  useEffect(() => {
    alive.current = true;
    return () => { alive.current = false; };
  }, []);
  useEffect(() => {
    const next = key ? key.split("|") : [];
    const started = next.filter((id) => !wanted.current.includes(id));
    wanted.current = next;
    // An agent that stopped working leaves the cache, so its next turn never opens with the last one's line.
    setTurns((previous) => new Map([...previous].filter(([id]) => next.includes(id))));
    read.current(started);
  }, [key]);
  // The cycle runs apart from the key, so a working set that changes faster than it still rereads everyone.
  useEffect(() => pollWhileVisible(() => read.current(wanted.current), intervalMs), [intervalMs]);
  return turns;
}

export function loadRuntimeConfig(): Promise<RuntimeConfigResponse> {
  return request<RuntimeConfigResponse>("/api/settings/runtime-config", { suppressErrorToast: true });
}

