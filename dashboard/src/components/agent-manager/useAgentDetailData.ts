import { useCallback, useEffect, useRef, useState } from "react";
import * as api from "../../api";
import type { AgentSkill, DiscordBinding } from "../../api/client";
import { ApiRequestError } from "../../api/httpClient";
import { getProviderAuthProfiles } from "../../api/providers";
import type { ProviderAuthProfilesResponse } from "../settings/SettingsProvidersModel";
import { splitLinkedSessions } from "./AgentInfoCardModel";
import { isDiscordSnowflake } from "./discord-routing";

/** A read still in flight (undefined), failed (null) or done. */
export type Read<T> = T | null | undefined;

const settle = <T,>(promise: Promise<T>, set: (value: T | null) => void) => promise.then(set, () => set(null));

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

/** The agent's own reads when its detail opens: sessions (live listed, finished counted) and skills, with per-read retry. */
export function useAgentDetailData(agentId: string) {
  const [sessions, setSessions] = useState<Read<ReturnType<typeof splitLinkedSessions>>>();
  const [skills, setSkills] = useState<Read<{ skills: AgentSkill[]; sharedSkills: AgentSkill[] }>>();
  const [sessNonce, setSessNonce] = useState(0);
  const [skillNonce, setSkillNonce] = useState(0);
  useEffect(() => {
    let active = true;
    setSessions(undefined);
    void settle(api.getAgentDispatchedSessions(agentId).then(splitLinkedSessions), (v) => { if (active) setSessions(v); });
    return () => { active = false; };
  }, [agentId, sessNonce]);
  useEffect(() => {
    let active = true;
    setSkills(undefined);
    void settle(api.getAgentSkills(agentId), (v) => { if (active) setSkills(v); });
    return () => { active = false; };
  }, [agentId, skillNonce]);
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
