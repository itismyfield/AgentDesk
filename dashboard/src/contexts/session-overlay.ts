import { parseChannelNameFromSessionKey } from "../components/agent-manager/discord-routing";
import { coerceTimestampMs } from "../components/agent-manager/kanban-utils";
import type { Agent, DispatchedSession } from "../types";

const THREAD_SUFFIX_RE = /-t\d{15,22}$/;
const PARENT_CHANNEL_FROM_THREAD_RE = /^(.+)-t\d{15,22}$/;

export interface SessionAgentBindingHint {
  agentId: string;
  channelId: string;
  channelName?: string | null;
  counterModelChannelId?: string | null;
  discord_channel_id?: string | null;
  discord_channel_alt?: string | null;
  discord_channel_cc?: string | null;
  discord_channel_cdx?: string | null;
}

function sessionIsWorking(status: DispatchedSession["status"]): boolean {
  return status === "turn_active" || status === "awaiting_bg" || status === "working";
}

function nonEmpty(value: string | null | undefined): string | null {
  const trimmed = value?.trim();
  return trimmed ? trimmed : null;
}

function stripChildFromSessionKey(sessionKey: string | null | undefined): string | null {
  if (typeof sessionKey !== "string") return null;
  const [parent] = sessionKey.split(":child:");
  return nonEmpty(parent);
}

function normalizeChannelSlug(value: string | null | undefined): string | null {
  const trimmed = value?.trim().replace(/^#+/, "").toLowerCase();
  return trimmed ? trimmed : null;
}

function explicitSessionAgentId(session: Pick<DispatchedSession, "linked_agent_id" | "agent_id">): string | null {
  return nonEmpty(session.linked_agent_id) ?? nonEmpty(session.agent_id);
}

function sessionMapKey(session: Pick<DispatchedSession, "id" | "session_key">): string | null {
  if (session.id != null && String(session.id).trim() !== "") return String(session.id).trim();
  return nonEmpty(session.session_key);
}

function recordUnambiguous(map: Map<string, string | null>, key: string | null | undefined, agentId: string): void {
  if (!key) return;
  const prev = map.get(key);
  if (prev === undefined) map.set(key, agentId);
  else if (prev !== agentId) map.set(key, null);
}

export function isThreadSession(
  session: Pick<DispatchedSession, "session_key" | "thread_channel_id" | "thread_id" | "resolved_thread_channel_id">,
): boolean {
  if (
    nonEmpty(session.thread_channel_id)
    || nonEmpty(session.thread_id)
    || nonEmpty(session.resolved_thread_channel_id)
  ) {
    return true;
  }
  const baseKey = stripChildFromSessionKey(session.session_key);
  return Boolean(baseKey && THREAD_SUFFIX_RE.test(baseKey));
}

export function stripThreadSuffixFromSessionKey(sessionKey: string | null | undefined): string | null {
  const baseKey = stripChildFromSessionKey(sessionKey);
  return baseKey ? baseKey.replace(THREAD_SUFFIX_RE, "") : null;
}

export function extractParentChannelSlugFromThreadSessionKey(sessionKey: string | null | undefined): string | null {
  const baseKey = stripChildFromSessionKey(sessionKey);
  if (!baseKey) return null;
  const normalized = baseKey.replace(/^([^:]+):tmux:/, "$1:");
  const channel = parseChannelNameFromSessionKey(normalized);
  if (!channel) return null;
  const match = channel.match(PARENT_CHANNEL_FROM_THREAD_RE);
  return match?.[1] ?? null;
}

/** Resolves explicit or parent-channel-inferred agent IDs for ad-hoc Discord thread sessions (`-t<snowflake>`). */
export function buildEffectiveSessionAgentMap(
  sessions: readonly DispatchedSession[],
  bindings?: ReadonlyArray<SessionAgentBindingHint> | null,
): Map<string, string> {
  const byParentKey = new Map<string, string | null>();
  const byChannelSlug = new Map<string, string | null>();
  const byBindingChannelId = new Map<string, string | null>();
  const byBindingSlug = new Map<string, string | null>();

  for (const session of sessions) {
    const agentId = explicitSessionAgentId(session);
    if (!agentId) continue;
    const baseKey = stripChildFromSessionKey(session.session_key);
    if (!baseKey) continue;
    recordUnambiguous(byParentKey, baseKey, agentId);
    if (!THREAD_SUFFIX_RE.test(baseKey)) {
      const normalized = baseKey.replace(/^([^:]+):tmux:/, "$1:");
      recordUnambiguous(byChannelSlug, normalizeChannelSlug(parseChannelNameFromSessionKey(normalized)), agentId);
    }
  }

  for (const binding of bindings ?? []) {
    const agentId = nonEmpty(binding.agentId);
    if (!agentId) continue;
    const rawChannels = [
      binding.channelId,
      binding.counterModelChannelId,
      binding.discord_channel_id,
      binding.discord_channel_alt,
      binding.discord_channel_cc,
      binding.discord_channel_cdx,
    ];
    for (const raw of rawChannels) {
      const id = nonEmpty(raw);
      if (!id) continue;
      recordUnambiguous(byBindingChannelId, id, agentId);
      recordUnambiguous(byBindingSlug, normalizeChannelSlug(id), agentId);
    }
    if (binding.channelName) {
      recordUnambiguous(byBindingSlug, normalizeChannelSlug(binding.channelName), agentId);
    }
  }

  const resolved = new Map<string, string>();
  for (const session of sessions) {
    const key = sessionMapKey(session);
    if (!key) continue;
    const explicit = explicitSessionAgentId(session);
    if (explicit) {
      resolved.set(key, explicit);
      continue;
    }
    if (!isThreadSession(session)) continue;
    const parentKey = stripThreadSuffixFromSessionKey(session.session_key);
    const parentSlug = normalizeChannelSlug(extractParentChannelSlugFromThreadSessionKey(session.session_key));
    const channelId = nonEmpty(session.channel_id);
    const inferred =
      (parentKey ? byParentKey.get(parentKey) ?? null : null)
      ?? (parentSlug ? byChannelSlug.get(parentSlug) ?? null : null)
      ?? (channelId ? byBindingChannelId.get(channelId) ?? null : null)
      ?? (parentSlug ? byBindingSlug.get(parentSlug) ?? null : null);
    if (inferred) {
      resolved.set(key, inferred);
    }
  }
  return resolved;
}

export function withInferredLinkedAgents(
  sessions: readonly DispatchedSession[],
  bindings?: ReadonlyArray<SessionAgentBindingHint> | null,
): DispatchedSession[] {
  if (sessions.length === 0) return [];
  const effectiveMap = buildEffectiveSessionAgentMap(sessions, bindings);
  let changed = false;
  const next = sessions.map((session) => {
    const key = sessionMapKey(session);
    const inferred = key ? effectiveMap.get(key) ?? null : null;
    if (!inferred || session.linked_agent_id === inferred) return session;
    changed = true;
    return { ...session, linked_agent_id: inferred };
  });
  return changed ? next : [...sessions];
}

export function deriveDispatchedAsAgents(_sessions: DispatchedSession[]): Agent[] {
  return [];
}

/** The server sends agent_status after each of the agent's session changes, without its visual status, so the
 * agent list's snapshot of that status is dropped rather than kept stale. */
export function mergeAgentStatus(agents: Agent[], update: Agent): Agent[] {
  return agents.map((agent) => (agent.id === update.id ? { ...agent, ...update, visual_status_code: undefined } : agent));
}

export function applySessionOverlay(baseAgents: Agent[], sessions: DispatchedSession[]): Agent[] {
  const normalizedSessions = withInferredLinkedAgents(sessions);
  const overlay = new Map<string, {
    workingCount: number;
    sessionInfo: string | null;
    currentThreadChannelId: string | null;
    newestTs: number;
  }>();

  for (const session of normalizedSessions) {
    if (session.status === "disconnected") continue;
    const agentId = explicitSessionAgentId(session);
    const isWorking = sessionIsWorking(session.status);
    if (!agentId || !isWorking) continue;

    const ts = coerceTimestampMs(session.last_seen_at) ?? coerceTimestampMs(session.connected_at) ?? 0;
    const prev = overlay.get(agentId);
    // Always keep the most recent session's values by comparing timestamps,
    // regardless of array ordering (bootstrap: old→new, WS prepend: new→old).
    const prevIsNewer = prev && prev.newestTs > ts;
    overlay.set(agentId, {
      workingCount: (prev?.workingCount ?? 0) + 1,
      sessionInfo: prevIsNewer
        ? prev.sessionInfo
        : (session.session_info ?? session.name ?? "작업 중"),
      currentThreadChannelId: prevIsNewer
        ? prev.currentThreadChannelId
        : (session.thread_channel_id ?? session.resolved_thread_channel_id ?? session.thread_id ?? null),
      newestTs: prevIsNewer ? prev.newestTs : ts,
    });
  }

  if (overlay.size === 0) return baseAgents;

  return baseAgents.map((agent) => {
    const sessionState = overlay.get(agent.id);
    if (!sessionState) return agent;
    return {
      ...agent,
      status: "working",
      session_info: sessionState.sessionInfo ?? agent.session_info ?? "작업 중",
      activity_source: "agentdesk",
      agentdesk_working_count: sessionState.workingCount,
      current_thread_channel_id:
        sessionState.currentThreadChannelId ?? agent.current_thread_channel_id ?? null,
    };
  });
}
