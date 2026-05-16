import type {
  SessionTranscript,
  SkillRankingByAgentRow,
} from "../../api";
import type {
  Agent,
  DispatchedSession,
  KanbanCard,
  UiLanguage,
} from "../../types";

export interface OfficeSkillRow {
  skillName: string;
  description: string | null;
  calls: number;
  lastUsedAt: number | null;
}

const SKILL_MARKDOWN_RE = /([A-Za-z0-9][A-Za-z0-9._-]*)\/SKILL\.md/g;
const SKILL_WINDOW_MS = 7 * 24 * 60 * 60 * 1000;

export function t(isKo: boolean, ko: string, en: string): string {
  return isKo ? ko : en;
}

export function formatDateTime(value: number | null, locale: UiLanguage): string {
  if (!value) return "-";
  return new Intl.DateTimeFormat(locale, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  }).format(value);
}

export function formatElapsed(value: number | null, isKo: boolean): string | null {
  if (!value) return null;
  const diff = Math.max(0, Date.now() - value);
  const mins = Math.floor(diff / 60000);
  if (mins < 1) return t(isKo, "방금 시작", "Started just now");
  if (mins < 60) return t(isKo, `${mins}분째`, `${mins}m running`);
  const hours = Math.floor(mins / 60);
  const remainMins = mins % 60;
  if (hours < 24) {
    return remainMins > 0
      ? t(isKo, `${hours}시간 ${remainMins}분째`, `${hours}h ${remainMins}m running`)
      : t(isKo, `${hours}시간째`, `${hours}h running`);
  }
  const days = Math.floor(hours / 24);
  return t(isKo, `${days}일째`, `${days}d running`);
}

export function normalizeTimestampMs(value: number | string | null | undefined): number | null {
  if (value == null || value === "") return null;
  if (typeof value === "number") {
    return value < 1e12 ? value * 1000 : value;
  }
  const numeric = Number(value);
  if (Number.isFinite(numeric)) {
    return numeric < 1e12 ? numeric * 1000 : numeric;
  }
  const parsed = Date.parse(value);
  return Number.isNaN(parsed) ? null : parsed;
}

function extractSkillNameFromEventContent(content: string): string | null {
  try {
    const parsed = JSON.parse(content) as Record<string, unknown>;
    for (const key of ["skill", "name", "command"]) {
      const value = parsed[key];
      if (typeof value === "string" && value.trim()) {
        return value.trim().replace(/^\/+/, "");
      }
    }
  } catch {
    // Ignore malformed payloads and fall back to regex scanning.
  }
  return null;
}

function deriveFallbackSkillRows(transcripts: SessionTranscript[]): OfficeSkillRow[] {
  const cutoff = Date.now() - SKILL_WINDOW_MS;
  const aggregates = new Map<string, OfficeSkillRow>();

  for (const transcript of transcripts) {
    const usedAt = normalizeTimestampMs(transcript.created_at);
    if (!usedAt || usedAt < cutoff) continue;

    const used = new Set<string>();
    const searchableChunks = [transcript.assistant_message];

    for (const event of transcript.events) {
      searchableChunks.push(event.summary ?? "");
      searchableChunks.push(event.content);
      if (
        event.kind === "tool_use"
        && event.tool_name?.toLowerCase() === "skill"
      ) {
        const skillName = extractSkillNameFromEventContent(event.content);
        if (skillName) used.add(skillName);
      }
    }

    const searchable = searchableChunks.join("\n");
    for (const match of searchable.matchAll(SKILL_MARKDOWN_RE)) {
      if (match[1]) used.add(match[1]);
    }

    for (const skillName of used) {
      const current = aggregates.get(skillName);
      if (current) {
        current.calls += 1;
        current.lastUsedAt = current.lastUsedAt == null ? usedAt : Math.max(current.lastUsedAt, usedAt);
        continue;
      }
      aggregates.set(skillName, {
        skillName,
        description: null,
        calls: 1,
        lastUsedAt: usedAt,
      });
    }
  }

  return Array.from(aggregates.values())
    .sort((left, right) => {
      if (right.calls !== left.calls) return right.calls - left.calls;
      return (right.lastUsedAt ?? 0) - (left.lastUsedAt ?? 0);
    })
    .slice(0, 5);
}

function buildSkillLookupKeys(agent: Agent): Set<string> {
  return new Set(
    [
      agent.id,
      agent.role_id,
      agent.name,
      agent.name_ko,
      agent.alias,
    ]
      .filter((value): value is string => Boolean(value?.trim()))
      .map((value) => value.trim().toLowerCase()),
  );
}

export function buildSkillRows(
  agent: Agent,
  rankingRows: SkillRankingByAgentRow[],
  transcripts: SessionTranscript[],
): OfficeSkillRow[] {
  const lookupKeys = buildSkillLookupKeys(agent);
  const agentRows = rankingRows
    .filter((row) => {
      const roleId = row.agent_role_id.trim().toLowerCase();
      const agentName = row.agent_name.trim().toLowerCase();
      return lookupKeys.has(roleId) || lookupKeys.has(agentName);
    })
    .slice(0, 5)
    .map((row) => ({
      skillName: row.skill_name,
      description: row.skill_desc_ko || null,
      calls: row.calls,
      lastUsedAt: row.last_used_at ?? null,
    }));

  if (agentRows.length > 0) return agentRows;
  return deriveFallbackSkillRows(transcripts);
}

export function hydrateSession(raw: DispatchedSession, agent: Agent): DispatchedSession {
  return {
    ...raw,
    session_key: raw.session_key ?? `agent:${agent.id}`,
    name: raw.name ?? agent.alias ?? agent.name_ko ?? agent.name,
    department_id: raw.department_id ?? agent.department_id ?? null,
    linked_agent_id: raw.linked_agent_id ?? agent.id,
    provider: raw.provider ?? agent.cli_provider ?? "claude",
    model: raw.model ?? null,
    status: raw.status ?? (agent.status === "working" ? "working" : "idle"),
    session_info: raw.session_info ?? agent.session_info ?? null,
    sprite_number: raw.sprite_number ?? agent.sprite_number ?? null,
    avatar_emoji: raw.avatar_emoji ?? agent.avatar_emoji,
    stats_xp: raw.stats_xp ?? agent.stats_xp,
    tokens: raw.tokens ?? 0,
    connected_at: raw.connected_at ?? agent.created_at,
    last_seen_at: raw.last_seen_at ?? null,
    department_name: raw.department_name ?? agent.department_name ?? null,
    department_name_ko: raw.department_name_ko ?? agent.department_name_ko ?? null,
    department_color: raw.department_color ?? agent.department_color ?? null,
    thread_channel_id: raw.thread_channel_id ?? agent.current_thread_channel_id ?? null,
  };
}

export function ensurePrimarySession(agent: Agent, sessions: DispatchedSession[]): DispatchedSession[] {
  if (!agent.current_thread_channel_id) return sessions;
  if (sessions.some((session) => session.thread_channel_id === agent.current_thread_channel_id)) {
    return sessions;
  }
  return [
    {
      id: `office-primary:${agent.id}`,
      session_key: `office:${agent.id}`,
      name: agent.alias ?? agent.name_ko ?? agent.name,
      department_id: agent.department_id ?? null,
      linked_agent_id: agent.id,
      provider: agent.cli_provider ?? "claude",
      model: null,
      status: agent.status === "working" ? "working" : "idle",
      session_info: agent.session_info ?? null,
      sprite_number: agent.sprite_number ?? null,
      avatar_emoji: agent.avatar_emoji,
      stats_xp: agent.stats_xp,
      tokens: 0,
      connected_at: agent.created_at,
      last_seen_at: null,
      department_name: agent.department_name ?? null,
      department_name_ko: agent.department_name_ko ?? null,
      department_color: agent.department_color ?? null,
      thread_channel_id: agent.current_thread_channel_id,
    },
    ...sessions,
  ];
}

export function sessionSortValue(session: DispatchedSession): number {
  return session.last_seen_at ?? session.connected_at ?? 0;
}

export function buildCardIssueUrl(card: KanbanCard): string | null {
  if (card.github_issue_url) return card.github_issue_url;
  if (card.github_repo && card.github_issue_number) {
    return `https://github.com/${card.github_repo}/issues/${card.github_issue_number}`;
  }
  return null;
}
