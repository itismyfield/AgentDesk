import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import * as api from "../../api";
import type { AgentSkill, DiscordBinding } from "../../api/client";
import { ApiRequestError } from "../../api/httpClient";
import { getProviderAuthProfiles } from "../../api/providers";
import { mergeRawSessions, withInferredLinkedAgents, type RawSessionInput } from "../../contexts/session-overlay";
import { pollWhileVisible } from "../../lib/pollWhileVisible";
import type { DispatchedSession, WSEvent } from "../../types";
import type { ProviderAuthProfilesResponse } from "../settings/SettingsProvidersModel";
import { splitLinkedSessions } from "./AgentInfoCardModel";
import { isDiscordSnowflake } from "./discord-routing";

/** A read still in flight (undefined), failed (null) or done. */
export type Read<T> = T | null | undefined;

const settle = <T,>(promise: Promise<T>, set: (value: T | null) => void) => promise.then(set, () => set(null));

const DETAIL_SESSION_POLL_MS = 30_000;

interface RestSessionSnapshot {
  agentSessions: DispatchedSession[];
  globalSessions: DispatchedSession[];
}

interface WsOverlayEntry {
  seq: number;
  eventType: "dispatched_session_new" | "dispatched_session_update" | "dispatched_session_disconnect";
  id: string | null;
  sessionKey: string | null;
  session?: DispatchedSession;
}

const trimOrNull = (value: unknown): string | null => {
  if (typeof value === "string") {
    const trimmed = value.trim();
    return trimmed ? trimmed : null;
  }
  if (typeof value === "number" && Number.isFinite(value)) return String(value);
  return null;
};

function matchesSessionTarget(
  left: { id?: unknown; session_key?: string | null },
  right: { id?: unknown; session_key?: string | null },
): boolean {
  const leftKey = trimOrNull(left.session_key);
  const rightKey = trimOrNull(right.session_key);
  if (leftKey && rightKey && leftKey === rightKey) return true;
  const leftId = trimOrNull(left.id);
  const rightId = trimOrNull(right.id);
  return Boolean(leftId && rightId && leftId === rightId);
}

const MAX_WS_OVERLAY_ENTRIES = 512;

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

/** The agent's own reads when its detail opens: sessions (raw merge -> thread inference -> owner filter, WS + poll/focus synced) and skills. */
export function useAgentDetailData(agentId: string, bindings?: Read<DiscordBinding[]>) {
  const [restSnapshot, setRestSnapshot] = useState<Read<RestSessionSnapshot>>();
  const [overlayTick, setOverlayTick] = useState(0);
  const [skills, setSkills] = useState<Read<{ skills: AgentSkill[]; sharedSkills: AgentSkill[] }>>();
  const [sessNonce, setSessNonce] = useState(0);
  const [skillNonce, setSkillNonce] = useState(0);

  const wsOverlayRef = useRef<Map<number, WsOverlayEntry>>(new Map());
  const mutationSeqRef = useRef(0);
  const requestSeqRef = useRef(0);
  const appliedRequestSeqRef = useRef(0);
  const prevAgentIdRef = useRef<string | null>(null);

  useEffect(() => {
    let active = true;
    if (prevAgentIdRef.current !== agentId) {
      prevAgentIdRef.current = agentId;
      wsOverlayRef.current.clear();
    }
    appliedRequestSeqRef.current = requestSeqRef.current;

    const loadSessions = (background: boolean) => {
      const requestSeq = ++requestSeqRef.current;
      const fetchStartSeq = mutationSeqRef.current;
      if (!background) {
        setRestSnapshot(undefined);
      }
      // Bypass in-flight GET sharing so reconciliation never adopts a pre-WS Promise and prunes newer WS state.
      const agentOpts = background
        ? { suppressErrorToast: true, maxRetries: 0, shareInflight: false }
        : { shareInflight: false };
      const globalOpts = { suppressErrorToast: true, maxRetries: 0, shareInflight: false };
      const readGlobal = resolveGlobalSessionsReader();
      const globalRead = readGlobal
        ? readGlobal(true, globalOpts)
        : Promise.resolve([] as DispatchedSession[]);

      void Promise.allSettled([api.getAgentDispatchedSessions(agentId, agentOpts), globalRead]).then(([agentRes, globalRes]) => {
        if (!active || requestSeq < appliedRequestSeqRef.current) return;
        if (background) {
          if (agentRes.status !== "fulfilled" || globalRes.status !== "fulfilled") return;
        } else if (agentRes.status === "rejected" && (!readGlobal || globalRes.status === "rejected")) {
          appliedRequestSeqRef.current = requestSeq;
          setRestSnapshot(null);
          return;
        }

        appliedRequestSeqRef.current = requestSeq;
        for (const seq of wsOverlayRef.current.keys()) {
          if (seq <= fetchStartSeq) {
            wsOverlayRef.current.delete(seq);
          }
        }
        setRestSnapshot({
          agentSessions: agentRes.status === "fulfilled" ? agentRes.value : [],
          globalSessions: globalRes.status === "fulfilled" ? globalRes.value : [],
        });
      });
    };

    loadSessions(false);
    const stopPoll = pollWhileVisible(() => loadSessions(true), DETAIL_SESSION_POLL_MS);
    const onFocus = () => loadSessions(true);
    if (typeof window !== "undefined") {
      window.addEventListener("focus", onFocus);
    }
    return () => {
      active = false;
      stopPoll();
      if (typeof window !== "undefined") {
        window.removeEventListener("focus", onFocus);
      }
    };
  }, [agentId, sessNonce]);

  useEffect(() => {
    if (typeof window === "undefined") return undefined;
    const trimOverlayCap = () => {
      while (wsOverlayRef.current.size > MAX_WS_OVERLAY_ENTRIES) {
        const oldest = wsOverlayRef.current.keys().next().value;
        if (oldest === undefined) break;
        wsOverlayRef.current.delete(oldest);
      }
    };
    const onWs = (e: Event) => {
      const detail = (e as CustomEvent<WSEvent>).detail;
      if (!detail) return;
      if (detail.type === "dispatched_session_new" || detail.type === "dispatched_session_update") {
        const raw = detail.payload as DispatchedSession | undefined;
        const id = trimOrNull(raw?.id);
        const sessionKey = trimOrNull(raw?.session_key);
        if (!raw || (!id && !sessionKey)) return;
        const seq = ++mutationSeqRef.current;
        wsOverlayRef.current.set(seq, {
          seq,
          eventType: detail.type,
          id,
          sessionKey,
          session: raw,
        });
        trimOverlayCap();
        setOverlayTick((n) => n + 1);
        return;
      }
      if (detail.type === "dispatched_session_disconnect") {
        const payload = detail.payload as { id?: string | number; session_key?: string } | undefined;
        const id = trimOrNull(payload?.id);
        const sessionKey = trimOrNull(payload?.session_key);
        if (!payload || (!id && !sessionKey)) return;
        const seq = ++mutationSeqRef.current;
        wsOverlayRef.current.set(seq, {
          seq,
          eventType: "dispatched_session_disconnect",
          id,
          sessionKey,
        });
        trimOverlayCap();
        setOverlayTick((n) => n + 1);
      }
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
    void overlayTick;
    if (restSnapshot === undefined) return undefined;
    if (restSnapshot === null) return null;

    const rawAgent: RawSessionInput[] = restSnapshot.agentSessions.map((s) => {
      const hasOwnerField = s.linked_agent_id !== undefined || s.agent_id !== undefined;
      const explicitOwner = hasOwnerField
        ? (trimOrNull(s.linked_agent_id) ?? trimOrNull(s.agent_id))
        : agentId;
      return {
        ...s,
        id: trimOrNull(s.id) ?? trimOrNull(s.session_key) ?? "",
        linked_agent_id: explicitOwner,
        inferred_linked_agent: false,
        __authoritySeq: 0,
      };
    });

    const rawGlobal: RawSessionInput[] = restSnapshot.globalSessions.map((s) => ({
      ...s,
      id: trimOrNull(s.id) ?? trimOrNull(s.session_key) ?? "",
      inferred_linked_agent: false,
      __authoritySeq: 0,
    }));

    const overlayEntries = Array.from(wsOverlayRef.current.values());
    const upsertEntries = overlayEntries.filter(
      (entry): entry is WsOverlayEntry & { session: DispatchedSession } =>
        entry.eventType !== "dispatched_session_disconnect" && entry.session !== undefined,
    );
    const wsUpserts: RawSessionInput[] = upsertEntries.map((entry) => ({
      ...entry.session,
      id: trimOrNull(entry.session.id) ?? trimOrNull(entry.session.session_key) ?? "",
      inferred_linked_agent: false,
      __authoritySeq: entry.seq,
    }));

    let mergedRaw = mergeRawSessions([...rawAgent, ...rawGlobal, ...wsUpserts]);

    const disconnects = overlayEntries.filter((entry) => entry.eventType === "dispatched_session_disconnect");
    if (disconnects.length > 0) {
      mergedRaw = mergedRaw.map((session) => {
        let maxUpsertSeq = 0;
        for (const up of upsertEntries) {
          if (up.seq > maxUpsertSeq && matchesSessionTarget(session, { id: up.id, session_key: up.sessionKey })) {
            maxUpsertSeq = up.seq;
          }
        }
        const disconnected = disconnects.some(
          (disc) => disc.seq > maxUpsertSeq && matchesSessionTarget(session, { id: disc.id, session_key: disc.sessionKey }),
        );
        return disconnected ? { ...session, status: "disconnected" as const } : session;
      });
    }

    const projected = withInferredLinkedAgents(mergedRaw, bindings ?? null);
    const owned = projected.filter((session) => session.linked_agent_id === agentId);
    return splitLinkedSessions(owned);
  }, [restSnapshot, overlayTick, agentId, bindings]);

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
