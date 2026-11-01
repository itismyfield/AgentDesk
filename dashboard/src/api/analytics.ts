import {
  request,
  readCachedGet,
  type CachedGetEntry,
  type RequestOptions,
} from "./httpClient";

// ── Skill Ranking ──

export interface SkillRankingOverallRow {
  skill_name: string;
  skill_desc_ko: string;
  calls: number;
  last_used_at: number;
}

export interface SkillRankingByAgentRow {
  agent_role_id: string;
  agent_name: string;
  skill_name: string;
  skill_desc_ko: string;
  calls: number;
  last_used_at: number;
}

export interface SkillRankingResponse {
  window: string;
  overall: SkillRankingOverallRow[];
  byAgent: SkillRankingByAgentRow[];
}

const SKILL_ANALYTICS_TIMEOUT_MS = 60_000;

export async function getSkillRanking(
  window: "7d" | "30d" | "90d" | "all" = "7d",
  limit = 20,
  opts?: RequestOptions,
): Promise<SkillRankingResponse> {
  return request(`/api/skills/ranking?window=${window}&limit=${limit}`, {
    timeoutMs: SKILL_ANALYTICS_TIMEOUT_MS,
    suppressErrorToast: true,
    ...opts,
  });
}

export function getCachedSkillRanking(
  window: "7d" | "30d" | "90d" | "all" = "7d",
  limit = 20,
): CachedGetEntry<SkillRankingResponse> | null {
  return readCachedGet<SkillRankingResponse>(
    `/api/skills/ranking?window=${window}&limit=${limit}`,
  );
}

export interface SkillTrendPoint {
  day: string;
  count: number;
}

export async function getSkillTrend(days = 30): Promise<SkillTrendPoint[]> {
  const data = await request<{ trend: SkillTrendPoint[] }>(
    `/api/skills-trend?days=${days}`,
  );
  return data.trend;
}

// ── GitHub Issues ──

export interface GitHubIssue {
  number: number;
  title: string;
  body: string;
  state: string;
  url: string;
  labels: Array<{ name: string; color: string }>;
  assignees: Array<{ login: string }>;
  createdAt: string;
  updatedAt: string;
}

export interface GitHubIssuesResponse {
  issues: GitHubIssue[];
  repo: string;
  error?: string;
}

// ── Messages (Chat) ──

// #2050 P2 finding 8 — align ChatMessage with the server contract.
// `messages.created_at` is a TIMESTAMPTZ serialized as ISO via
// `created_at::TEXT`, and `before` keyset binds directly into that
// comparison. Both fields are strings.
