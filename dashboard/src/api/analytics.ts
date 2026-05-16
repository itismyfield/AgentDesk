import { request } from "./httpClient";

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

export async function getSkillRanking(
  window: "7d" | "30d" | "90d" | "all" = "7d",
  limit = 20,
): Promise<SkillRankingResponse> {
  return request(`/api/skills/ranking?window=${window}&limit=${limit}`);
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

// ── Streaks ──

export interface AgentStreak {
  agent_id: string;
  name: string;
  avatar_emoji: string;
  streak: number;
  last_active: string;
}

export async function getStreaks(): Promise<{ streaks: AgentStreak[] }> {
  return request("/api/streaks");
}

// ── Achievements ──

export interface Achievement {
  id: string;
  agent_id: string;
  type: string;
  name: string;
  description: string | null;
  earned_at: number;
  agent_name: string;
  agent_name_ko: string;
  avatar_emoji: string;
  avatar_seed?: number | null;
  rarity?: string | null;
  progress?: AchievementProgress | null;
}

export interface AchievementProgress {
  current_xp: number;
  threshold: number;
  next_threshold: number | null;
  percent: number;
}

export interface DailyMission {
  id: string;
  label: string;
  current: number;
  target: number;
  completed: boolean;
}

export interface AchievementsResponse {
  achievements: Achievement[];
  daily_missions: DailyMission[];
}

function normalizeAchievement(raw: unknown): Achievement {
  const source = (raw ?? {}) as Record<string, unknown>;
  const rawEarnedAt = source.earned_at;
  const earnedAt =
    typeof rawEarnedAt === "number"
      ? rawEarnedAt
      : typeof rawEarnedAt === "string"
        ? Date.parse(rawEarnedAt) || 0
        : 0;
  const rawProgress =
    source.progress && typeof source.progress === "object"
      ? (source.progress as Record<string, unknown>)
      : null;

  return {
    id: String(source.id ?? ""),
    agent_id: String(source.agent_id ?? ""),
    type: String(source.type ?? ""),
    name: String(source.name ?? ""),
    description: typeof source.description === "string" ? source.description : null,
    earned_at: earnedAt,
    agent_name: String(source.agent_name ?? ""),
    agent_name_ko: String(source.agent_name_ko ?? ""),
    avatar_emoji: typeof source.avatar_emoji === "string" ? source.avatar_emoji : "🤖",
    avatar_seed:
      typeof source.avatar_seed === "number" ? source.avatar_seed : null,
    rarity: typeof source.rarity === "string" ? source.rarity : null,
    progress: rawProgress
      ? {
          current_xp:
            typeof rawProgress.current_xp === "number" ? rawProgress.current_xp : 0,
          threshold:
            typeof rawProgress.threshold === "number" ? rawProgress.threshold : 0,
          next_threshold:
            typeof rawProgress.next_threshold === "number"
              ? rawProgress.next_threshold
              : null,
          percent:
            typeof rawProgress.percent === "number" ? rawProgress.percent : 0,
        }
      : null,
  };
}

function normalizeDailyMission(raw: unknown): DailyMission {
  const source = (raw ?? {}) as Record<string, unknown>;
  return {
    id: String(source.id ?? ""),
    label: String(source.label ?? ""),
    current: typeof source.current === "number" ? source.current : 0,
    target: typeof source.target === "number" ? source.target : 0,
    completed: Boolean(source.completed),
  };
}

export async function getAchievements(
  agentId?: string,
): Promise<AchievementsResponse> {
  const q = agentId ? `?agentId=${agentId}` : "";
  const data = await request<{
    achievements?: unknown[];
    daily_missions?: unknown[];
  }>(`/api/v1/achievements${q}`);
  return {
    achievements: Array.isArray(data.achievements)
      ? data.achievements.map(normalizeAchievement)
      : [],
    daily_missions: Array.isArray(data.daily_missions)
      ? data.daily_missions.map(normalizeDailyMission)
      : [],
  };
}

// ── Messages (Chat) ──

// #2050 P2 finding 8 — align ChatMessage with the server contract.
// `messages.created_at` is a TIMESTAMPTZ serialized as ISO via
// `created_at::TEXT`, and `before` keyset binds directly into that
// comparison. Both fields are strings.
