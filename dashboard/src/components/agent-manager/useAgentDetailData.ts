import { useCallback, useEffect, useRef, useState } from "react";
import * as api from "../../api";
import type { AgentSkill, DiscordBinding } from "../../api/client";
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
    // The last list stays on screen while it is read again; an answer lands unless a newer one already did.
    const read = ++started.current.reads;
    void settle(api.getDiscordBindings({ suppressErrorToast: true }), (value) => {
      if (read < started.current.shown) return;
      started.current.shown = read;
      setBindings(value);
    });
  }, [active, routesKey]);
  useEffect(() => {
    if (!active || !detailOpen || started.current.details) return;
    started.current.details = true;
    void settle(getProviderAuthProfiles(), setAuthProfiles);
    void settle(api.getSkillRanking("30d").then((ranking) => ranking.byAgent), setSkillCalls);
  }, [active, detailOpen]);
  const rememberProfile = useCallback((agentId: string, provider: string, profileId: string | null) => setAuthProfiles((current) => current && {
    ...current,
    agent_profile_overrides: [
      ...(current.agent_profile_overrides ?? []).filter((entry) => entry.agent_id !== agentId),
      ...(profileId ? [{ agent_id: agentId, provider, profile_id: profileId }] : []),
    ],
  }), []);
  return { bindings, authProfiles, skillCalls, rememberProfile };
}

/** The agent's own reads when its detail opens: sessions (live listed, finished counted) and skills. */
export function useAgentDetailData(agentId: string) {
  const [sessions, setSessions] = useState<Read<ReturnType<typeof splitLinkedSessions>>>();
  const [skills, setSkills] = useState<Read<{ skills: AgentSkill[]; sharedSkills: AgentSkill[] }>>();
  useEffect(() => {
    let active = true;
    const keep = <T,>(set: (value: T | null) => void) => (value: T | null) => { if (active) set(value); };
    setSessions(undefined);
    setSkills(undefined);
    void settle(api.getAgentDispatchedSessions(agentId).then(splitLinkedSessions), keep(setSessions));
    void settle(api.getAgentSkills(agentId), keep(setSkills));
    return () => { active = false; };
  }, [agentId]);
  return { sessions, skills };
}

/** Reads once the section first opens, so a folded section costs no request. `read` must be stable. */
export function useReadWhenOpen<T>(open: boolean, agentId: string, read: (agentId: string) => Promise<T>): Read<T> {
  const [state, setState] = useState<{ id: string; value: T | null } | null>(null);
  const asked = useRef<string | null>(null);
  useEffect(() => {
    if (!open || asked.current === agentId) return;
    asked.current = agentId;
    void settle(read(agentId), (value) => { if (asked.current === agentId) setState({ id: agentId, value }); });
  }, [open, agentId, read]);
  return state?.id === agentId ? state.value : undefined;
}

/** Discord names of the given channels and their parents; an id that fails keeps showing as the id. A set already read
 * is not read again, so folding a section and opening it again sends nothing. */
export function useDiscordChannels(ids: Array<string | null | undefined> | null): Record<string, api.DiscordChannelInfo> {
  const key = ids ? Array.from(new Set(ids.filter(isDiscordSnowflake))).sort().join(",") : "";
  const [byId, setById] = useState<Record<string, api.DiscordChannelInfo>>({});
  const asked = useRef("");
  useEffect(() => {
    if (!key || key === asked.current) return;
    asked.current = key;
    const load = async (list: string[]) => Object.fromEntries((await Promise.all(list.map((id) =>
      api.getDiscordChannelInfo(id).then((info) => (info?.id ? [[id, info] as const] : []), () => []),
    ))).flat());
    void (async () => {
      const found = await load(key.split(","));
      const parents = Array.from(new Set(Object.values(found).map((info) => info.parent_id).filter((id): id is string => isDiscordSnowflake(id) && !(id in found))));
      const more = parents.length > 0 ? await load(parents) : {};
      if (asked.current === key) setById({ ...found, ...more });
    })();
  }, [key]);
  return byId;
}
