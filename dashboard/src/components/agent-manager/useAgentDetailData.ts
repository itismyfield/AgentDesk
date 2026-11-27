import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import * as api from "../../api";
import type { AgentSkill, DiscordBinding } from "../../api/client";
import { ApiRequestError } from "../../api/httpClient";
import { getProviderAuthProfiles } from "../../api/providers";
import { withInferredLinkedAgents } from "../../contexts/session-overlay";
import type { DispatchedSession, WSEvent } from "../../types";
import type { ProviderAuthProfilesResponse } from "../settings/SettingsProvidersModel";
import { splitLinkedSessions } from "./AgentInfoCardModel";
import { isDiscordSnowflake } from "./discord-routing";
import { coerceTimestampMs } from "./kanban-utils";

/** A read still in flight (undefined), failed (null) or done. */
export type Read<T> = T | null | undefined;

const settle = <T,>(promise: Promise<T>, set: (value: T | null) => void) => promise.then(set, () => set(null));

interface RawSessionState {
  agentSessions: DispatchedSession[];
  globalSessions: DispatchedSession[];
  wsKeys: Set<string>;
}

const sessionIdentityKey = (session: Pick<DispatchedSession, "id" | "session_key">): string =>
  session.session_key?.trim() || (session.id != null ? String(session.id).trim() : "");

function sameSession(
  left: Pick<DispatchedSession, "id" | "session_key">,
  right: { id?: string | number | null; session_key?: string | null },
): boolean {
  const leftKey = left.session_key?.trim();
  const rightKey = right.session_key?.trim();
  if (leftKey && rightKey && leftKey === rightKey) return true;
  const leftId = left.id != null ? String(left.id).trim() : "";
  const rightId = right.id != null ? String(right.id).trim() : "";
  return Boolean(leftId && rightId && leftId === rightId);
}

function mergeSessionPair(base: DispatchedSession, patch: Partial<DispatchedSession>): DispatchedSession {
  const next: DispatchedSession = { ...base };
  for (const [key, value] of Object.entries(patch) as Array<[keyof DispatchedSession, unknown]>) {
    if (value !== undefined && (value !== null || next[key] == null || key === "session_info" || key === "active_dispatch_id")) {
      (next as unknown as Record<string, unknown>)[key] = value;
    }
  }
  next.id = String(patch.id ?? base.id);
  return next;
}

function applySessionWsEvent(state: RawSessionState, event: WSEvent): RawSessionState {
  if (event.type === "dispatched_session_new" || event.type === "dispatched_session_update") {
    const raw = event.payload as DispatchedSession | undefined;
    if (!raw || (raw.id == null && !raw.session_key)) return state;
    const incoming: DispatchedSession = { ...raw, id: String(raw.id ?? raw.session_key) };
    const wsKeys = new Set(state.wsKeys);
    const key = sessionIdentityKey(incoming);
    if (key) wsKeys.add(key);
    let matchedGlobal = false;
    const nextGlobal = state.globalSessions.map((entry) => {
      if (!sameSession(entry, incoming)) return entry;
      matchedGlobal = true;
      return mergeSessionPair(entry, incoming);
    });
    const nextAgent = state.agentSessions.map((entry) =>
      sameSession(entry, incoming) ? mergeSessionPair(entry, incoming) : entry,
    );
    return {
      agentSessions: nextAgent,
      globalSessions: matchedGlobal ? nextGlobal : [incoming, ...nextGlobal],
      wsKeys,
    };
  }
  if (event.type === "dispatched_session_disconnect") {
    const payload = event.payload as { id?: string | number; session_key?: string } | undefined;
    if (!payload) return state;
    const wsKeys = new Set(state.wsKeys);
    const disconnectIfMatch = (entry: DispatchedSession): DispatchedSession => {
      if (!sameSession(entry, payload)) return entry;
      const key = sessionIdentityKey(entry);
      if (key) wsKeys.add(key);
      return { ...entry, status: "disconnected" as const };
    };
    return {
      agentSessions: state.agentSessions.map(disconnectIfMatch),
      globalSessions: state.globalSessions.map(disconnectIfMatch),
      wsKeys,
    };
  }
  return state;
}

/** What every agent's detail shares: all Discord routes, Provider accounts and 30-day skill calls. */
export interface AgentDetailShared {
  bindings: Read<DiscordBinding[]>;
  authProfiles: Read<ProviderAuthProfilesResponse>;
  skillCalls: Read<api.SkillRankingByAgentRow[]>;
  /** Keeps a saved account choice, so a reopened detail shows it without reading again. */
  rememberProfile: (agentId: string, provider: string, profileId: string | null) => void;
  retrySkillCalls?: () => void;
}

/** One read per page of what the details share, while `active`. Routes come at once (the list marks agents without
 * one) and again when `routesKey` changes; accounts and skill calls wait for the first open detail. */
export function useAgentDetailShared(active: boolean, detailOpen = active, routesKey = ""): AgentDetailShared {
  const [bindings, setBindings] = useState<Read<DiscordBinding[]>>();
  const [authProfiles, setAuthProfiles] = useState<Read<ProviderAuthProfilesResponse>>();
  const [skillCalls, setSkillCalls] = useState<Read<api.SkillRankingByAgentRow[]>>();
  const started = useRef<{ routes: string | null; reads: number; shown: number; details: boolean }>({ routes: null, reads: 0, shown: 0, details: false });
  useEffect(() => {
    if (!active || started.current.routes === routesKey) return;
    started.current.routes = routesKey;
    const read = ++started.current.reads;
    void settle(api.getDiscordBindings({ suppressErrorToast: true }), (value) => {
      if (read < started.current.shown || (value === null && read > 1)) return;
      started.current.shown = read;
      setBindings(value);
    });
  }, [active, routesKey]);
  const loadSkillCalls = useCallback(() => {
    setSkillCalls(undefined);
    void settle(api.getSkillRanking("30d").then((ranking) => ranking.byAgent), setSkillCalls);
  }, []);
  useEffect(() => {
    if (!active || !detailOpen || started.current.details) return;
    started.current.details = true;
    void settle(getProviderAuthProfiles(), setAuthProfiles);
    loadSkillCalls();
  }, [active, detailOpen, loadSkillCalls]);
  const rememberProfile = useCallback((agentId: string, provider: string, profileId: string | null) => setAuthProfiles((current) => current && {
    ...current,
    agent_profile_overrides: [
      ...(current.agent_profile_overrides ?? []).filter((entry) => entry.agent_id !== agentId),
      ...(profileId ? [{ agent_id: agentId, provider, profile_id: profileId }] : []),
    ],
  }), []);
  return { bindings, authProfiles, skillCalls, rememberProfile, retrySkillCalls: loadSkillCalls };
}

function resolveGlobalSessionsReader(): typeof api.getDispatchedSessions | null {
  try {
    return typeof api.getDispatchedSessions === "function" ? api.getDispatchedSessions : null;
  } catch {
    return null;
  }
}

/** The agent's own reads when its detail opens: sessions (explicit + inferred thread sessions, live WS-synced) and skills. */
export function useAgentDetailData(agentId: string, bindings?: Read<DiscordBinding[]>) {
  const [rawSessions, setRawSessions] = useState<Read<RawSessionState>>();
  const [skills, setSkills] = useState<Read<{ skills: AgentSkill[]; sharedSkills: AgentSkill[] }>>();
  const [sessNonce, setSessNonce] = useState(0);
  const [skillNonce, setSkillNonce] = useState(0);
  const bufferedWsEvents = useRef<WSEvent[]>([]);

  useEffect(() => {
    let active = true;
    bufferedWsEvents.current = [];
    setRawSessions(undefined);
    const readGlobal = resolveGlobalSessionsReader();
    const globalRead = readGlobal
      ? readGlobal(true, { suppressErrorToast: true })
      : Promise.resolve([] as DispatchedSession[]);
    void Promise.allSettled([api.getAgentDispatchedSessions(agentId), globalRead]).then(([agentRes, globalRes]) => {
      if (!active) return;
      if (agentRes.status === "rejected" && (!readGlobal || globalRes.status === "rejected")) {
        setRawSessions(null);
        return;
      }
      let next: RawSessionState = {
        agentSessions: agentRes.status === "fulfilled" ? agentRes.value : [],
        globalSessions: globalRes.status === "fulfilled" ? globalRes.value : [],
        wsKeys: new Set<string>(),
      };
      for (const event of bufferedWsEvents.current) {
        next = applySessionWsEvent(next, event);
      }
      bufferedWsEvents.current = [];
      setRawSessions(next);
    });
    return () => { active = false; };
  }, [agentId, sessNonce]);

  useEffect(() => {
    if (typeof window === "undefined") return undefined;
    const onWs = (e: Event) => {
      const detail = (e as CustomEvent<WSEvent>).detail;
      if (
        !detail
        || (detail.type !== "dispatched_session_new"
          && detail.type !== "dispatched_session_update"
          && detail.type !== "dispatched_session_disconnect")
      ) {
        return;
      }
      setRawSessions((current) => {
        if (!current) {
          if (current === undefined) bufferedWsEvents.current.push(detail);
          return current;
        }
        return applySessionWsEvent(current, detail);
      });
    };
    window.addEventListener("pcd-ws-event", onWs);
    return () => window.removeEventListener("pcd-ws-event", onWs);
  }, []);

  useEffect(() => {
    let active = true;
    setSkills(undefined);
    void settle(api.getAgentSkills(agentId), (v) => { if (active) setSkills(v); });
    return () => { active = false; };
  }, [agentId, skillNonce]);

  const sessions = useMemo(() => {
    if (rawSessions === undefined) return undefined;
    if (rawSessions === null) return null;
    const normalizedAgent = rawSessions.agentSessions.map((s) => ({
      ...s,
      id: String(s.id ?? s.session_key ?? ""),
      linked_agent_id: s.linked_agent_id ?? s.agent_id ?? agentId,
    }));
    const normalizedGlobal = rawSessions.globalSessions.map((s) => ({
      ...s,
      id: String(s.id ?? s.session_key ?? ""),
    }));
    const inferredAll = withInferredLinkedAgents([...normalizedAgent, ...normalizedGlobal], bindings ?? null);
    const agentSlice = inferredAll.slice(0, normalizedAgent.length);
    const globalSlice = inferredAll.slice(normalizedAgent.length);

    const byKey = new Map<string, DispatchedSession>();
    for (const session of agentSlice) {
      const key = sessionIdentityKey(session);
      if (key) byKey.set(key, session);
    }
    for (const session of globalSlice) {
      const owner = session.linked_agent_id ?? session.agent_id ?? null;
      if (owner !== agentId) continue;
      const key = sessionIdentityKey(session);
      if (!key) continue;
      const existing = byKey.get(key);
      if (!existing) {
        byKey.set(key, session);
        continue;
      }
      const isWsFresh = rawSessions.wsKeys.has(key);
      const existingTs =
        coerceTimestampMs(existing.last_seen_at ?? (existing as { last_heartbeat?: string | null }).last_heartbeat)
        ?? coerceTimestampMs(existing.connected_at)
        ?? 0;
      const globalTs =
        coerceTimestampMs(session.last_seen_at ?? (session as { last_heartbeat?: string | null }).last_heartbeat)
        ?? coerceTimestampMs(session.connected_at)
        ?? 0;
      byKey.set(key, isWsFresh || globalTs >= existingTs ? mergeSessionPair(existing, session) : mergeSessionPair(session, existing));
    }
    return splitLinkedSessions(Array.from(byKey.values()));
  }, [rawSessions, agentId, bindings]);

  const retrySessions = useCallback(() => setSessNonce((n) => n + 1), []);
  const retrySkills = useCallback(() => setSkillNonce((n) => n + 1), []);
  return { sessions, skills, retrySessions, retrySkills };
}

/** Reads once the section first opens, with an explicit `retry` so a transient failure can be retried while open. */
export function useReadWhenOpen<T>(open: boolean, agentId: string, read: (agentId: string) => Promise<T>): { value: Read<T>; retry: () => void } {
  const [state, setState] = useState<{ key: string; value: T | null } | null>(null);
  const [nonce, setNonce] = useState(0);
  const asked = useRef<string | null>(null);
  const key = `${agentId}:${nonce}`;
  useEffect(() => {
    if (!open || asked.current === key) return;
    asked.current = key;
    void settle(read(agentId), (value) => { if (asked.current === key) setState({ key, value }); });
  }, [open, agentId, key, read]);
  const retry = useCallback(() => setNonce((n) => n + 1), []);
  return { value: state?.key === key ? state.value : undefined, retry };
}

/** Definite refusal (403 outside role-map, 404). 401 is excluded so expired auth can recover and re-query. */
const refused = (error: unknown) =>
  error instanceof ApiRequestError
  && error.status >= 400
  && error.status < 500
  && error.status !== 401
  && error.status !== 408
  && error.status !== 429;

/** Discord names of the given channels and their parents; 401/timeout/5xx stay uncached and retry on re-open or window focus (auth recovery). */
export function useDiscordChannels(ids: Array<string | null | undefined> | null): Record<string, api.DiscordChannelInfo> {
  const key = ids ? Array.from(new Set(ids.filter(isDiscordSnowflake))).sort().join(",") : "";
  const [byId, setById] = useState<Record<string, api.DiscordChannelInfo>>({});
  const [authTick, setAuthTick] = useState(0);
  const asked = useRef(new Set<string>());
  const hadTransientFailure = useRef(false);

  useEffect(() => {
    if (typeof window === "undefined") return undefined;
    const onRecover = () => {
      if (hadTransientFailure.current) setAuthTick((n) => n + 1);
    };
    window.addEventListener("focus", onRecover);
    return () => window.removeEventListener("focus", onRecover);
  }, []);

  useEffect(() => {
    const load = async (list: string[]) => Object.fromEntries((await Promise.all(list.map((id) => {
      asked.current.add(id);
      return api.getDiscordChannelInfo(id).then((info) => (info?.id ? [[id, info] as const] : []), (error) => {
        if (!refused(error)) {
          asked.current.delete(id);
          hadTransientFailure.current = true;
        }
        return [];
      });
    }))).flat());
    const wanted = key ? key.split(",").filter((id) => !asked.current.has(id)) : [];
    if (!wanted.length) return;
    hadTransientFailure.current = false;
    void (async () => {
      const found = await load(wanted);
      const parents = Array.from(new Set(Object.values(found).map((info) => info.parent_id).filter((id): id is string => isDiscordSnowflake(id) && !asked.current.has(id))));
      const more = parents.length > 0 ? await load(parents) : {};
      setById((current) => ({ ...current, ...found, ...more }));
    })();
  }, [key, authTick]);
  return byId;
}
