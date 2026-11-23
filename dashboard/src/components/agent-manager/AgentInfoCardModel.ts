import type { CronJob, DiscordBinding } from "../../api/client";
import type { DispatchedSession } from "../../types";

// Same live set as home's `classifyAgents`; aborted and disconnected sessions are finished.
const LIVE_SESSION_STATUSES = new Set(["turn_active", "working", "awaiting_bg", "awaiting_user", "idle"]);
const TURN_STATUSES = new Set(["turn_active", "working"]);

export const isTurnSession = (session: DispatchedSession) => TURN_STATUSES.has(session.status);

/** The agent's live sessions and how many finished ones were left out. */
export function splitLinkedSessions(sessions: DispatchedSession[]): { live: DispatchedSession[]; closed: number } {
  const live = sessions.filter((session) => LIVE_SESSION_STATUSES.has(session.status));
  return { live, closed: sessions.length - live.length };
}

export function formatSchedule(schedule: CronJob["schedule"], isKo: boolean): string {
  if (schedule.kind === "every" && schedule.everyMs) {
    const mins = Math.round(schedule.everyMs / 60000);
    if (mins >= 60) {
      const hrs = Math.round(mins / 60);
      return isKo ? `${hrs}시간마다` : `Every ${hrs}h`;
    }
    return isKo ? `${mins}분마다` : `Every ${mins}m`;
  }
  if (schedule.kind === "cron" && schedule.cron) {
    return schedule.cron;
  }
  if (schedule.kind === "at" && schedule.atMs) {
    return new Date(schedule.atMs).toLocaleString();
  }
  return schedule.kind;
}

export function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

export function timeAgo(ms: number, isKo: boolean): string {
  const diff = Date.now() - ms;
  const mins = Math.floor(diff / 60000);
  if (mins < 1) return isKo ? "방금" : "just now";
  if (mins < 60) return isKo ? `${mins}분 전` : `${mins}m ago`;
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return isKo ? `${hrs}시간 전` : `${hrs}h ago`;
  const days = Math.floor(hrs / 24);
  return isKo ? `${days}일 전` : `${days}d ago`;
}

export function inferBindingSource(binding: DiscordBinding): string {
  if (binding.channelId.startsWith("dm:")) return "dm";
  if (binding.source) return binding.source;
  return "channel";
}

export function bindingSourceLabel(source: string): string {
  switch (source) {
    case "role-map":
      return "RoleMap";
    case "primary":
      return "Primary";
    case "alt":
      return "Alt";
    case "codex":
      return "Codex";
    case "dm":
      return "DM";
    default:
      return "Channel";
  }
}

export function compactToken(value: string, head = 8, tail = 4): string {
  if (value.length <= head + tail + 3) return value;
  return `${value.slice(0, head)}...${value.slice(-tail)}`;
}
