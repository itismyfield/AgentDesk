import { z } from "zod";
import { request } from "../../api/httpClient";
import type { Agent, DispatchedSession } from "../../types";
import type { PersonaState } from "../AgentPersona";
import { parseChannelNameFromSessionKey } from "../agent-manager/discord-routing";
import type { Translator } from "../agent-manager/types";
import { agentStateLabel, type HomeAgentState } from "../home/homeModel";
import { cleanupStaleIdleSession, reconcileStaleSessionTurn, type ReconcileStaleTurnResponse } from "../home/useHomeData";

export { cleanupStaleIdleSession, reconcileStaleSessionTurn };
export type ReconcileStaleResult = ReconcileStaleTurnResponse;

export const STALE_IDLE_MS = 7 * 24 * 60 * 60 * 1000;
export const STALE_ACTIVE_MS = 30 * 60 * 1000;
export const LIVE_SESSION_STATUSES = new Set(["turn_active", "working", "awaiting_bg", "awaiting_user", "idle"]);

const sessionOutputSchema = z.object({
  recent_output: z.string(), backend: z.string(), available: z.boolean(),
  unavailable_reason: z.string().nullish(), output_format: z.string(), captured_at_ms: z.number(),
});

export type SessionOutputSnapshot = z.infer<typeof sessionOutputSchema>;

export const fetchSessionOutput = (id: number | string, lines = 80, signal?: AbortSignal) =>
  request(`/api/sessions/${encodeURIComponent(id)}/output?lines=${lines}`, { signal, cache: "no-store", suppressErrorToast: true, maxRetries: 0 }, sessionOutputSchema);

export function normalizeTimestamp(value: unknown): number | null {
  if (value == null) return null;
  const toMs = (epoch: number) => (!Number.isFinite(epoch) || epoch <= 0 ? null : epoch < 1e12 ? Math.trunc(epoch * 1000) : Math.trunc(epoch));
  if (typeof value === "number") return toMs(value);
  if (value instanceof Date) return toMs(value.getTime());
  if (typeof value === "string") {
    const trimmed = value.trim();
    if (!trimmed) return null;
    const numeric = Number(trimmed);
    if (Number.isFinite(numeric)) return toMs(numeric);
    const parsed = Date.parse(trimmed);
    return Number.isFinite(parsed) ? parsed : null;
  }
  return null;
}

export const isLiveSessionStatus = (status: string) => LIVE_SESSION_STATUSES.has(status);
export type SessionDisplayState = Extract<HomeAgentState, "working" | "background" | "resting"> | "closed";

/** Home's words for live sessions; statuses outside `LIVE_SESSION_STATUSES` (`disconnected`, `aborted`, etc.) stay `closed`. */
export function sessionState(s: DispatchedSession): SessionDisplayState {
  if (!isLiveSessionStatus(s.status)) return "closed";
  if (s.status === "working" || s.status === "turn_active") return "working";
  return s.status === "awaiting_bg" ? "background" : "resting";
}

export function sessionStateText(s: DispatchedSession, tr: Translator): string {
  const state = sessionState(s);
  if (state !== "closed") return agentStateLabel(state, tr);
  return s.status === "aborted" ? tr("중단됨", "Aborted") : tr("연결 끊김", "Disconnected");
}

const PERSONA: Record<SessionDisplayState, PersonaState> = { working: "working", background: "waiting", resting: "idle", closed: "offline" };

export function sessionPersona(s: DispatchedSession): { agent: Pick<Agent, "id" | "name" | "avatar_emoji" | "department_color">; state: PersonaState } {
  return {
    agent: { id: s.id, name: s.name || s.session_key, avatar_emoji: s.avatar_emoji, department_color: s.department_color ?? null },
    state: PERSONA[sessionState(s)],
  };
}

/** Short name from the session key; background children (`<parent>:child:<id>`) append a short child token so sibling rows stay distinct. */
export function sessionTitle(s: DispatchedSession): { title: string; child: boolean } {
  const [parent, ...childParts] = s.session_key.split(":child:");
  const raw = childParts.join(":child:").trim();
  const suffix = raw ? (raw.length <= 14 ? raw : `${raw.slice(0, 8)}…${raw.slice(-4)}`) : "";
  const channel = parseChannelNameFromSessionKey(parent);
  const base = channel ? `#${channel}` : (s.name && s.name !== s.session_key ? s.name : parent.slice(parent.lastIndexOf("/") + 1)) || s.session_key;
  return { title: suffix ? `${base} · ${suffix}` : base, child: childParts.length > 0 };
}

export const sessionLastActivityTs = (s: DispatchedSession): number | null => normalizeTimestamp(s.last_seen_at) ?? normalizeTimestamp(s.connected_at);

/** Idle for 7+ days by its own signal; reconciled notes alone never re-classify a recently reconciled idle session as stale. */
export function isStaleIdleSession(s: DispatchedSession, now = Date.now()): boolean {
  if (!isLiveSessionStatus(s.status)) return false;
  const last = sessionLastActivityTs(s);
  return sessionState(s) === "resting" && last !== null && now - last >= STALE_IDLE_MS;
}

/** Active turns with no heartbeat for 30+ minutes. */
export function isStaleActiveTurn(s: DispatchedSession, now = Date.now()): boolean {
  if (s.status !== "turn_active" && s.status !== "working") return false;
  const last = sessionLastActivityTs(s);
  return last !== null && now - last >= STALE_ACTIVE_MS;
}

export function canReconcileStaleSession(s: DispatchedSession, now = Date.now()): boolean {
  return isStaleActiveTurn(s, now);
}

export function canCleanupStaleIdleSession(s: DispatchedSession, now = Date.now()): boolean {
  return s.status === "idle" && !s.active_dispatch_id && isStaleIdleSession(s, now);
}

export type SessionSummaryBucket = "working" | "background" | "stale" | "resting" | "closed";

export function sessionBucket(s: DispatchedSession, now = Date.now()): SessionSummaryBucket {
  const state = sessionState(s);
  if (state === "closed") return "closed";
  if (state === "working") return isStaleActiveTurn(s, now) ? "stale" : "working";
  if (state === "background") return "background";
  return isStaleIdleSession(s, now) ? "stale" : "resting";
}

export function sessionSummaryCounts(sessions: DispatchedSession[], tr: Translator, now = Date.now()): string {
  const counts: Record<SessionSummaryBucket, number> = { working: 0, background: 0, stale: 0, resting: 0, closed: 0 };
  for (const session of sessions) counts[sessionBucket(session, now)] += 1;
  const labels: Record<SessionSummaryBucket, string> = {
    working: agentStateLabel("working", tr), background: agentStateLabel("background", tr),
    stale: tr("멈춤", "Stale"), resting: agentStateLabel("resting", tr), closed: tr("연결 끊김", "Disconnected"),
  };
  return (["working", "background", "stale", "resting", "closed"] as const)
    .filter((key) => counts[key] > 0)
    .map((key) => `${labels[key]} ${counts[key]}`)
    .join(" · ");
}

const BUCKET_ORDER: Record<SessionSummaryBucket, number> = { working: 0, background: 1, resting: 2, stale: 3, closed: 4 };
export function compareSessions(a: DispatchedSession, b: DispatchedSession): number {
  return BUCKET_ORDER[sessionBucket(a)] - BUCKET_ORDER[sessionBucket(b)] || (sessionLastActivityTs(b) ?? 0) - (sessionLastActivityTs(a) ?? 0);
}
