import type { Agent, DispatchedSession } from "../../types";

export const STALE_IDLE_MS = 7 * 24 * 60 * 60 * 1000;

export function normalizeTimestamp(value: unknown): number | null {
  if (value == null) return null;

  const normalizeEpoch = (epoch: number): number | null => {
    if (!Number.isFinite(epoch) || epoch <= 0) return null;
    return epoch < 1e12 ? Math.trunc(epoch * 1000) : Math.trunc(epoch);
  };

  if (typeof value === "number") return normalizeEpoch(value);

  if (typeof value === "string") {
    const trimmed = value.trim();
    if (!trimmed) return null;

    const numeric = Number(trimmed);
    if (Number.isFinite(numeric)) return normalizeEpoch(numeric);

    const parsed = Date.parse(trimmed);
    return Number.isFinite(parsed) ? parsed : null;
  }

  if (value instanceof Date) {
    return normalizeEpoch(value.getTime());
  }

  return null;
}

export function sessionSpriteNum(s: DispatchedSession): number {
  if (s.sprite_number != null && s.sprite_number > 0) return s.sprite_number;
  const idStr = String(s.id);
  let hash = 0;
  for (let i = 0; i < idStr.length; i += 1) {
    hash = (hash * 31 + idStr.charCodeAt(i)) >>> 0;
  }
  return (hash % 12) + 1;
}

/** Display name for a session; full key used as tooltip */
export function sessionDisplayName(s: DispatchedSession): { label: string; full: string } {
  if (s.name) return { label: s.name, full: s.name };
  return { label: `Session ${s.session_key.slice(0, 8)}`, full: s.session_key };
}

export function sessionLastActivityTs(s: DispatchedSession): number {
  const lastSeenAt = normalizeTimestamp(s.last_seen_at);
  if (lastSeenAt != null) return lastSeenAt;
  const connectedAt = normalizeTimestamp(s.connected_at);
  if (connectedAt != null) return connectedAt;
  return 0;
}

export function isStaleIdleSession(s: DispatchedSession): boolean {
  return s.status === "idle" && Date.now() - sessionLastActivityTs(s) >= STALE_IDLE_MS;
}

export function compareSessions(a: DispatchedSession, b: DispatchedSession): number {
  const rank = (session: DispatchedSession): number => {
    if (session.status === "working") return 0;
    if (session.status === "idle" && !isStaleIdleSession(session)) return 1;
    if (session.status === "idle") return 2;
    return 3;
  };

  const rankDiff = rank(a) - rank(b);
  if (rankDiff !== 0) return rankDiff;
  return sessionLastActivityTs(b) - sessionLastActivityTs(a);
}

export function linkedAgentLabel(s: DispatchedSession, agents: Agent[]): string | null {
  const linked = agents.find((agent) => agent.id === s.linked_agent_id);
  if (linked) return linked.name_ko || linked.name;
  return s.linked_agent_id;
}

export function sessionProviderLabel(provider?: string): string {
  switch (provider) {
    case "codex":
      return "Codex";
    case "gemini":
      return "Gemini";
    case "qwen":
      return "Qwen";
    case "claude":
      return "Claude";
    default:
      return provider || "Unknown";
  }
}

export function sessionProviderTone(provider?: string): "neutral" | "info" | "accent" | "success" {
  switch (provider) {
    case "codex":
      return "info";
    case "gemini":
      return "accent";
    case "qwen":
      return "success";
    case "claude":
      return "accent";
    default:
      return "neutral";
  }
}

export function formatTimeAgo(ts: unknown, isKo = true): string {
  const normalized = normalizeTimestamp(ts);
  if (normalized == null) return isKo ? "알 수 없음" : "Unknown";

  const diff = Math.max(0, Date.now() - normalized);
  const sec = Math.floor(diff / 1000);
  if (sec < 60) return isKo ? `${sec}초 전` : `${sec}s ago`;
  const min = Math.floor(sec / 60);
  if (min < 60) return isKo ? `${min}분 전` : `${min}m ago`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return isKo ? `${hr}시간 전` : `${hr}h ago`;
  const days = Math.floor(hr / 24);
  return isKo ? `${days}일 전` : `${days}d ago`;
}

export function formatDuration(ms: number, isKo = true): string {
  const sec = Math.floor(ms / 1000);
  if (sec < 60) return isKo ? `${sec}초` : `${sec}s`;
  const min = Math.floor(sec / 60);
  const hr = Math.floor(min / 60);
  if (hr > 0) return isKo ? `${hr}시간 ${min % 60}분` : `${hr}h ${min % 60}m`;
  return isKo ? `${min}분` : `${min}m`;
}
