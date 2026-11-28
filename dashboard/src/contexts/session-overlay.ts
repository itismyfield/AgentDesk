import { parseChannelNameFromSessionKey } from "../components/agent-manager/discord-routing";
import { coerceTimestampMs } from "../components/agent-manager/kanban-utils";
import type { Agent, DispatchedSession } from "../types";

const THREAD_SUFFIX_RE = /-t\d{15,22}$/;
const PARENT_CHANNEL_FROM_THREAD_RE = /^(.+)-t\d{15,22}$/;

const MUTABLE_RUNTIME_KEYS = new Set<keyof DispatchedSession>([
  "status",
  "session_info",
  "tokens",
  "active_dispatch_id",
  "last_seen_at",
  "model",
  "provider",
  "name",
]);

const STATUS_TIE_RANK: Record<DispatchedSession["status"], number> = {
  turn_active: 6,
  working: 5,
  awaiting_user: 4,
  awaiting_bg: 3,
  idle: 2,
  aborted: 1,
  disconnected: 0,
};

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

export type RawSessionInput = DispatchedSession & {
  __authoritySeq?: number;
  __ownerSeq?: number;
  __ownerTs?: number;
  last_heartbeat?: string | number | null;
};

const INTERNAL_MERGE_KEYS = new Set<string>([
  "linked_agent_id",
  "agent_id",
  "inferred_linked_agent",
  "__authoritySeq",
  "__ownerSeq",
  "__ownerTs",
]);

function sessionIsWorking(status: DispatchedSession["status"]): boolean {
  return status === "turn_active" || status === "awaiting_bg" || status === "working";
}

function nonEmpty(value: string | null | undefined): string | null {
  if (typeof value !== "string") return null;
  const trimmed = value.trim();
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

function extractSessionNamespace(sessionKey: string | null | undefined): string | null {
  const baseKey = stripChildFromSessionKey(sessionKey);
  if (!baseKey) return null;
  const normalized = baseKey.replace(/^([^:]+):tmux:/, "$1:");
  const colonIdx = normalized.indexOf(":");
  if (colonIdx <= 0) return null;
  return nonEmpty(normalized.slice(0, colonIdx).toLowerCase());
}

/** Server-explicit agent ID only (`null` when `inferred_linked_agent` is true). */
export function explicitSessionAgentId(
  session: Pick<DispatchedSession, "linked_agent_id" | "agent_id" | "inferred_linked_agent">,
): string | null {
  if (session.inferred_linked_agent) return null;
  return nonEmpty(session.linked_agent_id) ?? nonEmpty(session.agent_id);
}

function canonicalSessionId(rawId: unknown): string | null {
  if (rawId == null) return null;
  const trimmed = String(rawId).trim();
  return trimmed ? trimmed : null;
}

function distinctSessionId(rawId: unknown, sessionKey: string | null | undefined): string | null {
  const id = canonicalSessionId(rawId);
  if (!id) return null;
  const key = nonEmpty(sessionKey);
  return key && id === key ? null : id;
}

function sessionMapKey(session: Pick<DispatchedSession, "id" | "session_key">): string | null {
  return canonicalSessionId(session.id) ?? nonEmpty(session.session_key);
}

function sessionRecordTimestampMs(session: RawSessionInput): number {
  return (
    coerceTimestampMs(session.last_seen_at)
    ?? coerceTimestampMs(session.last_heartbeat)
    ?? coerceTimestampMs(session.connected_at)
    ?? -1
  );
}

function stripInternalFields(session: RawSessionInput): DispatchedSession {
  const copy = { ...session };
  delete copy.__authoritySeq;
  delete copy.__ownerSeq;
  delete copy.__ownerTs;
  copy.id = canonicalSessionId(copy.id) ?? nonEmpty(copy.session_key) ?? "";
  return copy;
}

function mergeTwoRawEntries(left: RawSessionInput, right: RawSessionInput): RawSessionInput {
  const seqL = left.__authoritySeq ?? 0;
  const seqR = right.__authoritySeq ?? 0;
  const tsL = sessionRecordTimestampMs(left);
  const tsR = sessionRecordTimestampMs(right);
  const expL = explicitSessionAgentId(left);
  const expR = explicitSessionAgentId(right);

  // Resolve explicit owner using its own provenance so unattributed copies never inflate stale owner timestamps.
  let resolvedExplicit: string | null = null;
  let resolvedOwnerSeq: number | undefined;
  let resolvedOwnerTs: number | undefined;
  if (expL !== null && expR !== null) {
    const oSeqL = left.__ownerSeq ?? seqL;
    const oSeqR = right.__ownerSeq ?? seqR;
    const oTsL = left.__ownerTs ?? tsL;
    const oTsR = right.__ownerTs ?? tsR;
    const rightOwnerWins =
      oSeqR !== oSeqL
        ? oSeqR > oSeqL
        : oTsR !== oTsL
          ? oTsR > oTsL
          : expR < expL;
    resolvedExplicit = rightOwnerWins ? expR : expL;
    resolvedOwnerSeq = rightOwnerWins ? oSeqR : oSeqL;
    resolvedOwnerTs = rightOwnerWins ? oTsR : oTsL;
  } else if (expL !== null) {
    resolvedExplicit = expL;
    resolvedOwnerSeq = left.__ownerSeq ?? seqL;
    resolvedOwnerTs = left.__ownerTs ?? tsL;
  } else if (expR !== null) {
    resolvedExplicit = expR;
    resolvedOwnerSeq = right.__ownerSeq ?? seqR;
    resolvedOwnerTs = right.__ownerTs ?? tsR;
  }

  let merged: RawSessionInput;
  if (seqL !== seqR || tsL !== tsR) {
    const rightIsNewer = seqR !== seqL ? seqR > seqL : tsR > tsL;
    const newer = rightIsNewer ? right : left;
    const older = rightIsNewer ? left : right;

    merged = { ...older };
    for (const [k, v] of Object.entries(newer) as Array<[keyof RawSessionInput, unknown]>) {
      if (INTERNAL_MERGE_KEYS.has(k as string)) continue;
      if (v === undefined) continue;
      if (MUTABLE_RUNTIME_KEYS.has(k as keyof DispatchedSession) || v !== null || merged[k] == null) {
        (merged as unknown as Record<string, unknown>)[k] = v;
      }
    }
  } else {
    const allKeys = new Set<string>([...Object.keys(left), ...Object.keys(right)]);
    const out: Record<string, unknown> = {};
    for (const k of allKeys) {
      if (INTERNAL_MERGE_KEYS.has(k)) continue;
      const vL = (left as unknown as Record<string, unknown>)[k];
      const vR = (right as unknown as Record<string, unknown>)[k];
      if (vL == null) {
        out[k] = vR !== undefined ? vR : vL;
      } else if (vR == null) {
        out[k] = vL;
      } else if (k === "status") {
        const rL = STATUS_TIE_RANK[vL as DispatchedSession["status"]] ?? -1;
        const rR = STATUS_TIE_RANK[vR as DispatchedSession["status"]] ?? -1;
        out[k] = rR > rL ? vR : vL;
      } else if (typeof vL === "number" && typeof vR === "number") {
        out[k] = Math.max(vL, vR);
      } else {
        out[k] = String(vL) >= String(vR) ? vL : vR;
      }
    }
    merged = out as unknown as RawSessionInput;
  }

  const keyL = nonEmpty(left.session_key);
  const keyR = nonEmpty(right.session_key);
  const realIdL = distinctSessionId(left.id, left.session_key);
  const realIdR = distinctSessionId(right.id, right.session_key);
  const idL = realIdL ?? (realIdR === null ? canonicalSessionId(left.id) : null);
  const idR = realIdR ?? (realIdL === null ? canonicalSessionId(right.id) : null);
  const canonicalKey =
    seqR > seqL && keyR ? keyR : seqL > seqR && keyL ? keyL : keyL && keyR ? (keyL <= keyR ? keyL : keyR) : (keyL ?? keyR);
  const canonicalId =
    seqR > seqL && idR ? idR : seqL > seqR && idL ? idL : idL && idR ? (idL <= idR ? idL : idR) : (idL ?? idR);

  merged.id = canonicalId ?? canonicalKey ?? "";
  if (canonicalKey !== null) merged.session_key = canonicalKey;
  merged.linked_agent_id = resolvedExplicit;
  if (merged.agent_id !== undefined || resolvedExplicit !== null) {
    merged.agent_id = resolvedExplicit;
  }
  merged.inferred_linked_agent = false;
  merged.__authoritySeq = Math.max(seqL, seqR);
  merged.__ownerSeq = resolvedOwnerSeq;
  merged.__ownerTs = resolvedOwnerTs;
  return merged;
}

function compareRawForCanonicalFold(a: RawSessionInput, b: RawSessionInput): number {
  const seqDiff = (a.__authoritySeq ?? 0) - (b.__authoritySeq ?? 0);
  if (seqDiff !== 0) return seqDiff;
  const tsDiff = sessionRecordTimestampMs(a) - sessionRecordTimestampMs(b);
  if (tsDiff !== 0) return tsDiff;
  const expA = explicitSessionAgentId(a) ?? "";
  const expB = explicitSessionAgentId(b) ?? "";
  if (expA !== expB) return expA < expB ? -1 : 1;
  const keyA = nonEmpty(a.session_key) ?? "";
  const keyB = nonEmpty(b.session_key) ?? "";
  if (keyA !== keyB) return keyA < keyB ? -1 : 1;
  const idA = canonicalSessionId(a.id) ?? "";
  const idB = canonicalSessionId(b.id) ?? "";
  if (idA !== idB) return idA < idB ? -1 : 1;
  const stDiff = (STATUS_TIE_RANK[a.status] ?? -1) - (STATUS_TIE_RANK[b.status] ?? -1);
  if (stDiff !== 0) return stDiff;
  const infoA = a.session_info ?? "";
  const infoB = b.session_info ?? "";
  return infoA === infoB ? 0 : infoA < infoB ? -1 : 1;
}

/** Order-independent raw session merge unifying `by id` and `by session_key`; explicit `linked_agent_id` wins over null. */
export function mergeRawSessions(sessions: readonly RawSessionInput[]): DispatchedSession[] {
  if (sessions.length === 0) return [];
  const slots: Array<RawSessionInput | null> = [];
  const slotGroups: Array<RawSessionInput[] | null> = [];
  const byId = new Map<string, number>();
  const byKey = new Map<string, number>();

  const redirectSlot = (fromIdx: number, toIdx: number) => {
    for (const [k, v] of byId.entries()) {
      if (v === fromIdx) byId.set(k, toIdx);
    }
    for (const [k, v] of byKey.entries()) {
      if (v === fromIdx) byKey.set(k, toIdx);
    }
  };

  for (const raw of sessions) {
    const exp = explicitSessionAgentId(raw);
    const rawSeq = raw.__authoritySeq ?? 0;
    const rawTs = sessionRecordTimestampMs(raw);
    const entry: RawSessionInput = {
      ...raw,
      id: canonicalSessionId(raw.id) ?? nonEmpty(raw.session_key) ?? "",
      linked_agent_id: exp,
      inferred_linked_agent: false,
      __authoritySeq: rawSeq,
      __ownerSeq: exp !== null ? (raw.__ownerSeq ?? rawSeq) : undefined,
      __ownerTs: exp !== null ? (raw.__ownerTs ?? rawTs) : undefined,
    };
    const id = canonicalSessionId(raw.id);
    const sKey = nonEmpty(raw.session_key);
    const idxById = id ? byId.get(id) : undefined;
    const rawIdxByKey = sKey ? byKey.get(sKey) : undefined;
    const idxByKey = (() => {
      if (rawIdxByKey === undefined) return undefined;
      const existing = slots[rawIdxByKey];
      if (!existing) return undefined;
      const existingId = canonicalSessionId(existing.id);
      const existingThread = nonEmpty(existing.thread_channel_id ?? existing.resolved_thread_channel_id ?? existing.thread_id);
      const incomingThread = nonEmpty(raw.thread_channel_id ?? raw.resolved_thread_channel_id ?? raw.thread_id);
      if (existingId && id && existingId !== id && existingThread && incomingThread && existingThread !== incomingThread) {
        return undefined;
      }
      return rawIdxByKey;
    })();

    let targetIdx: number;
    if (idxById !== undefined && idxByKey !== undefined && idxById !== idxByKey) {
      const mergedExisting = mergeTwoRawEntries(slots[idxById]!, slots[idxByKey]!);
      slots[idxById] = mergeTwoRawEntries(mergedExisting, entry);
      slotGroups[idxById] = [...slotGroups[idxById]!, ...slotGroups[idxByKey]!, entry];
      slots[idxByKey] = null;
      slotGroups[idxByKey] = null;
      redirectSlot(idxByKey, idxById);
      targetIdx = idxById;
    } else if (idxById !== undefined || idxByKey !== undefined) {
      targetIdx = (idxById ?? idxByKey)!;
      slots[targetIdx] = mergeTwoRawEntries(slots[targetIdx]!, entry);
      slotGroups[targetIdx]!.push(entry);
    } else {
      targetIdx = slots.length;
      slots.push(entry);
      slotGroups.push([entry]);
    }

    if (id) byId.set(id, targetIdx);
    if (sKey) byKey.set(sKey, targetIdx);
    const mergedId = canonicalSessionId(slots[targetIdx]!.id);
    const mergedKey = nonEmpty(slots[targetIdx]!.session_key);
    if (mergedId) byId.set(mergedId, targetIdx);
    if (mergedKey) byKey.set(mergedKey, targetIdx);
  }

  const out: DispatchedSession[] = [];
  for (const group of slotGroups) {
    if (!group || group.length === 0) continue;
    if (group.length === 1) {
      out.push(stripInternalFields(group[0]));
      continue;
    }
    const ordered = [...group].sort(compareRawForCanonicalFold);
    out.push(stripInternalFields(ordered.reduce(mergeTwoRawEntries)));
  }
  return out;
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

/** Resolves explicit or namespace-scoped parent-channel-inferred agent IDs for ad-hoc Discord thread sessions. */
export function buildEffectiveSessionAgentMap(
  sessions: readonly DispatchedSession[],
  bindings?: ReadonlyArray<SessionAgentBindingHint> | null,
): Map<string, string> {
  const merged = mergeRawSessions(sessions);
  const byParentKey = new Map<string, string | null>();
  const byNamespacedChannelSlug = new Map<string, string | null>();
  const byGlobalChannelSlug = new Map<string, string | null>();
  const byBindingChannelId = new Map<string, string | null>();
  const byBindingSlug = new Map<string, string | null>();

  for (const session of merged) {
    const agentId = explicitSessionAgentId(session);
    if (!agentId) continue;
    const baseKey = stripChildFromSessionKey(session.session_key);
    if (!baseKey) continue;
    recordUnambiguous(byParentKey, baseKey, agentId);
    if (!THREAD_SUFFIX_RE.test(baseKey)) {
      const normalized = baseKey.replace(/^([^:]+):tmux:/, "$1:");
      const slug = normalizeChannelSlug(parseChannelNameFromSessionKey(normalized));
      const namespace = extractSessionNamespace(session.session_key);
      if (namespace && slug) {
        recordUnambiguous(byNamespacedChannelSlug, `${namespace}::${slug}`, agentId);
      }
      recordUnambiguous(byGlobalChannelSlug, slug, agentId);
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
  for (const session of merged) {
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
    const namespace = extractSessionNamespace(session.session_key);
    const channelId = nonEmpty(session.channel_id);
    const inferred =
      (parentKey ? byParentKey.get(parentKey) ?? null : null)
      ?? (namespace && parentSlug ? byNamespacedChannelSlug.get(`${namespace}::${parentSlug}`) ?? null : null)
      ?? (parentSlug ? byGlobalChannelSlug.get(parentSlug) ?? null : null)
      ?? (channelId ? byBindingChannelId.get(channelId) ?? null : null)
      ?? (parentSlug ? byBindingSlug.get(parentSlug) ?? null : null);
    if (inferred) {
      resolved.set(key, inferred);
    }
  }
  return resolved;
}

/** Pure derived projection: merges raw sessions first, then infers `linked_agent_id` ONLY when missing (`inferred_linked_agent: true`). */
export function withInferredLinkedAgents(
  sessions: readonly RawSessionInput[],
  bindings?: ReadonlyArray<SessionAgentBindingHint> | null,
): DispatchedSession[] {
  if (sessions.length === 0) return [];
  const merged = mergeRawSessions(sessions);
  const effectiveMap = buildEffectiveSessionAgentMap(merged, bindings);
  return merged.map((session) => {
    const explicit = explicitSessionAgentId(session);
    if (explicit) {
      return { ...session, linked_agent_id: explicit, inferred_linked_agent: false };
    }
    const key = sessionMapKey(session);
    const inferred = key ? effectiveMap.get(key) ?? null : null;
    if (!inferred) {
      return { ...session, linked_agent_id: null, inferred_linked_agent: false };
    }
    return { ...session, linked_agent_id: inferred, inferred_linked_agent: true };
  });
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
    const agentId = nonEmpty(session.linked_agent_id) ?? nonEmpty(session.agent_id);
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
