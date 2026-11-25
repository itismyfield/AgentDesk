import { createContext, createElement, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { getCampaigns, type HomeLegacyCampaignSummary } from "../../api/campaigns";
import { healthDetailBlocked, noteHealthDetailError } from "../../api/healthDetailAccess";
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

/** Reads on mount and again every `intervalMs` while the page is visible. `load` must be stable.
 * Once a newer read settles, an older one cannot overwrite it. `enabled` skips the read when a parent already supplies the value. */
export function usePolled<T>(load: () => Promise<T>, intervalMs: number, enabled = true): Loaded<T> {
  const [state, setState] = useState<Loaded<T>>({ data: null, failed: false, at: null });
  useEffect(() => {
    if (!enabled) return;
    let active = true;
    let generation = 0;
    // The newest read that has settled wins. An older read can still paint until that happens,
    // so a retry that finishes first is kept, and a late failure cannot wipe a newer success.
    let settled = 0;
    const run = () => {
      const requestId = ++generation;
      const finish = (apply: () => void) => {
        if (!active || requestId < settled) return;
        settled = requestId;
        apply();
      };
      load()
        .then((data) => finish(() => setState({ data, failed: false, at: Date.now() })))
        .catch(() => finish(() => setState((previous) => ({ ...previous, failed: true }))));
    };
    run();
    const stop = pollWhileVisible(run, intervalMs);
    return () => { active = false; stop(); };
  }, [load, intervalMs, enabled]);
  return state;
}

/** Server campaigns whose stage track is read per poll, those under way first; it bounds the detail requests. */
export const DETAIL_CAMPAIGNS = 8;

export interface HomeCampaigns {
  legacy: HomeLegacyCampaignSummary[] | null;
  summaries: ServerCampaignSummary[] | null;
  documents: ServerCampaignDocument[];
  /** Open server campaigns whose detail read failed, and those past `DETAIL_CAMPAIGNS`: their to-dos are unknown. */
  detailFailed: number;
  detailsCut: number;
}

export async function loadHomeCampaigns(): Promise<HomeCampaigns> {
  const [legacy, summaries] = await Promise.allSettled([
    getCampaigns({ view: "home" }).then((result) => result.campaigns),
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

function toHealth(raw: unknown): HomeHealth {
  const source = (raw ?? {}) as HomeHealth;
  return {
    ...source,
    status: source.status ?? "unhealthy",
    providers: Array.isArray(source.providers) ? source.providers : [],
    degraded_reasons: Array.isArray(source.degraded_reasons) ? source.degraded_reasons : [],
    startup_degraded_reasons: Array.isArray(source.startup_degraded_reasons) ? source.startup_degraded_reasons : [],
  };
}

async function readHealth(path: string): Promise<HomeHealth> {
  try {
    return toHealth(await request<unknown>(path, { suppressErrorToast: true }));
  } catch (error) {
    const body = error instanceof ApiRequestError && error.status === 503 ? error.body : null;
    if (typeof (body as { status?: unknown } | null)?.status === "string") return toHealth(body);
    throw error;
  }
}

export async function loadHomeHealth(): Promise<HomeHealth> {
  if (!healthDetailBlocked()) {
    try {
      return await readHealth("/api/health/detail");
    } catch (error) {
      if (noteHealthDetailError(error) === "failed") return { ...(await readHealth("/api/health")), publicOnly: "failed" };
    }
  }
  return { ...(await readHealth("/api/health")), publicOnly: "forbidden" };
}

/** Explicit fresh read of `GET /api/health/detail` when the operator expands degraded server reasons. */
export async function loadHealthDetailOnDemand(): Promise<HomeHealth> {
  try {
    return await readHealth("/api/health/detail");
  } catch (error) {
    const forbidden = error instanceof ApiRequestError && error.status === 403;
    return { ...(await readHealth("/api/health")), publicOnly: forbidden ? "forbidden" : "failed" };
  }
}

export async function loadHomeRoutines(): Promise<RoutineRecord[]> {
  const data = await request<{ routines?: RoutineRecord[] }>("/api/routines", { suppressErrorToast: true });
  return data.routines ?? [];
}

export async function loadHomeUsage(): Promise<RawUsageProvider[]> {
  const data = await request<{ providers?: RawUsageProvider[] }>("/api/rate-limits", { suppressErrorToast: true });
  return data.providers ?? [];
}

export interface ReconcileStaleTurnResponse {
  ok?: boolean;
  session_key?: string;
  reconciled?: boolean;
  status?: string;
  reason?: string;
}

export function reconcileStaleSessionTurn(sessionKey: string): Promise<ReconcileStaleTurnResponse> {
  return request<ReconcileStaleTurnResponse>(
    `/api/sessions/${encodeURIComponent(sessionKey)}/reconcile-stale-turn`,
    { method: "POST", maxRetries: 0, suppressErrorToast: true },
  );
}

export interface StopAgentTurnResponse {
  ok?: boolean;
  agent_id?: string;
  session_key?: string;
  status?: string;
  error?: string;
}

export function stopAgentTurn(agentId: string): Promise<StopAgentTurnResponse> {
  return request<StopAgentTurnResponse>(
    `/api/agents/${encodeURIComponent(agentId)}/turn/stop`,
    { method: "POST", maxRetries: 0, suppressErrorToast: true },
  );
}

/** Live turns of the given agents; an agent whose read fails is left out and keeps its plain cell. */
export async function loadAgentTurns(ids: string[]): Promise<Map<string, HomeTurn>> {
  const reads = await Promise.allSettled(ids.map((id) => request<HomeTurn>(`/api/agents/${encodeURIComponent(id)}/turn`, { suppressErrorToast: true })));
  return new Map(reads.flatMap((read, index) => (read.status === "fulfilled" ? [[ids[index], read.value] as const] : [])));
}

interface AgentTurnSource {
  turns: Map<string, HomeTurn>;
  watched: ReadonlySet<string>;
}

const AgentTurnsContext = createContext<AgentTurnSource | null>(null);

/** Shell-level turn poll. Descendants reuse it instead of reading the same agents again. */
export function AgentTurnsProvider({ turns, watched, children }: {
  turns: Map<string, HomeTurn>;
  watched: readonly string[];
  children: ReactNode;
}) {
  const value = useMemo<AgentTurnSource>(() => ({ turns, watched: new Set(watched) }), [turns, watched]);
  return createElement(AgentTurnsContext.Provider, { value }, children);
}

/** Turns of the working agents, cached per agent: one that starts working is read at once and the rest keep the polling cycle.
 * Inside `AgentTurnsProvider`, ids the provider already watches are not read again. */
export function useAgentTurns(ids: string[], intervalMs = 30_000): Map<string, HomeTurn> {
  const source = useContext(AgentTurnsContext);
  const idsKey = ids.join("|");
  const key = (source ? ids.filter((id) => !source.watched.has(id)) : ids).join("|");
  const [turns, setTurns] = useState<Map<string, HomeTurn>>(() => new Map());
  const wanted = useRef<string[]>([]);
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
    setTurns((previous) => new Map([...previous].filter(([id]) => next.includes(id))));
    read.current(started);
  }, [key]);
  useEffect(() => pollWhileVisible(() => read.current(wanted.current), intervalMs), [intervalMs]);
  return useMemo(() => {
    if (!source) return turns;
    const merged = new Map<string, HomeTurn>();
    for (const id of idsKey ? idsKey.split("|") : []) {
      const turn = source.watched.has(id) ? source.turns.get(id) : turns.get(id);
      if (turn) merged.set(id, turn);
    }
    return merged;
  }, [source, turns, idsKey]);
}

export function loadRuntimeConfig(): Promise<RuntimeConfigResponse> {
  return request<RuntimeConfigResponse>("/api/settings/runtime-config", { suppressErrorToast: true });
}
