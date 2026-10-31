import type { Agent, DispatchedSession } from "./types";

const STALE_LINKED_SESSION_MS = 10 * 60_000;

export type AgentWarningCode =
  | "missing_work_detail"
  | "no_discord_route"
  | "stale_linked_session"
  | "unassigned";

export interface AgentWarning {
  code: AgentWarningCode;
  severity: "warning" | "error" | "info";
  ko: string;
  en: string;
}

function cleanText(value: string | null | undefined): string | null {
  const trimmed = value?.trim();
  return trimmed ? trimmed : null;
}

export function getAgentWorkSummary(
  agent: Agent,
  opts?: {
    activeTaskTitle?: string | null;
    linkedSessions?: DispatchedSession[];
  },
): string | null {
  const fromTask = cleanText(opts?.activeTaskTitle);
  if (fromTask) return fromTask;

  const fromAgent = cleanText(agent.session_info);
  if (fromAgent) return fromAgent;

  const linkedSessions = opts?.linkedSessions ?? [];
  for (const session of linkedSessions) {
    const sessionText = cleanText(session.session_info) ?? cleanText(session.name);
    if (sessionText) return sessionText;
  }

  if (agent.status !== "working") return null;

  return "Task in progress";
}

export function getAgentWorkElapsedMs(
  _agent: Agent,
  linkedSessions: DispatchedSession[] = [],
): number | null {
  const active = linkedSessions.filter((session) => session.status === "working");
  if (active.length === 0) return null;
  const startAt = Math.min(...active.map((session) => session.connected_at || Date.now()));
  return Math.max(0, Date.now() - startAt);
}

export function getStaleLinkedSessions(
  linkedSessions: DispatchedSession[] = [],
  now = Date.now(),
): DispatchedSession[] {
  return linkedSessions.filter((session) => {
    if (session.status !== "working" || !session.last_seen_at) return false;
    return now - session.last_seen_at > STALE_LINKED_SESSION_MS;
  });
}

export function getAgentWarnings(
  agent: Agent,
  opts?: {
    hasDiscordBindings?: boolean;
    activeTaskTitle?: string | null;
    linkedSessions?: DispatchedSession[];
  },
): AgentWarning[] {
  const warnings: AgentWarning[] = [];
  const linkedSessions = opts?.linkedSessions ?? [];
  const specificWorkSummary =
    cleanText(opts?.activeTaskTitle)
    ?? cleanText(agent.session_info)
    ?? linkedSessions
      .map((session) => cleanText(session.session_info) ?? cleanText(session.name))
      .find((value): value is string => Boolean(value))
    ?? null;

  if (!agent.department_id) {
    warnings.push({
      code: "unassigned",
      severity: "info",
      ko: "부서 미배정",
      en: "Department unassigned",
    });
  }

  if (agent.status === "working" && !specificWorkSummary) {
    warnings.push({
      code: "missing_work_detail",
      severity: "warning",
      ko: "작업 설명 없음",
      en: "No work detail",
    });
  }

  if (opts?.hasDiscordBindings === false) {
    warnings.push({
      code: "no_discord_route",
      severity: "warning",
      ko: "Discord 라우팅 없음",
      en: "No Discord route",
    });
  }

  if (getStaleLinkedSessions(linkedSessions).length > 0) {
    warnings.push({
      code: "stale_linked_session",
      severity: "error",
      ko: "오래된 working 세션",
      en: "Stale working session",
    });
  }

  return warnings;
}

export function formatElapsedCompact(ms: number, isKo: boolean): string {
  const totalSeconds = Math.max(1, Math.floor(ms / 1000));
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  if (hours > 0) return isKo ? `${hours}시간 ${minutes}분` : `${hours}h ${minutes}m`;
  if (minutes > 0) return isKo ? `${minutes}분` : `${minutes}m`;
  return isKo ? `${totalSeconds}초` : `${totalSeconds}s`;
}
