import type { Agent, DispatchedSession } from "../../types";
import type { PersonaState } from "../AgentPersona";
import { parseChannelNameFromSessionKey } from "../agent-manager/discord-routing";
import type { HomeAgentState } from "../home/homeModel";

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

/** Home's words for a live session: a turn is working, a background wait is background, anything else rests. */
export function sessionState(s: DispatchedSession): Extract<HomeAgentState, "working" | "background" | "resting"> {
  if (s.status === "working" || s.status === "turn_active") return "working";
  return s.status === "awaiting_bg" ? "background" : "resting";
}

const PERSONA: Record<ReturnType<typeof sessionState>, PersonaState> = { working: "working", background: "waiting", resting: "idle" };

export function sessionPersona(s: DispatchedSession): {
  agent: Pick<Agent, "id" | "name" | "avatar_emoji" | "department_color">;
  state: PersonaState;
} {
  return {
    agent: { id: s.id, name: s.name || s.session_key, avatar_emoji: s.avatar_emoji, department_color: s.department_color ?? null },
    state: PERSONA[sessionState(s)],
  };
}

/** Short name from the session key (the name is often the whole key): the channel for AgentDesk sessions, and whether
 * it is a background child (`<parent>:child:<id>`). */
export function sessionTitle(s: DispatchedSession): { title: string; child: boolean } {
  const [parent, ...child] = s.session_key.split(":child:");
  const channel = parseChannelNameFromSessionKey(parent);
  if (channel) return { title: `#${channel}`, child: child.length > 0 };
  const name = s.name && s.name !== s.session_key ? s.name : parent.slice(parent.lastIndexOf("/") + 1);
  return { title: name || s.session_key, child: child.length > 0 };
}

export function sessionLastActivityTs(s: DispatchedSession): number | null {
  return normalizeTimestamp(s.last_seen_at) ?? normalizeTimestamp(s.connected_at);
}

/** Idle for a week by its own signal; a session without any signal is not called stale. */
export function isStaleIdleSession(s: DispatchedSession, now = Date.now()): boolean {
  const last = sessionLastActivityTs(s);
  return sessionState(s) === "resting" && last !== null && now - last >= STALE_IDLE_MS;
}

/** Working, then background, then resting (stale ones last), newest signal first within each. */
export function compareSessions(a: DispatchedSession, b: DispatchedSession): number {
  const rank = (session: DispatchedSession) => ["working", "background", "resting"].indexOf(sessionState(session)) + (isStaleIdleSession(session) ? 1 : 0);
  return rank(a) - rank(b) || (sessionLastActivityTs(b) ?? 0) - (sessionLastActivityTs(a) ?? 0);
}
