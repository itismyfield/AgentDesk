import type { SkillRankingByAgentRow, SkillRankingOverallRow, SkillTrendPoint } from "../api/analytics";
import type { SkillCatalogEntry } from "../types";

export const SKILL_CATEGORIES = {
  all: { ko: "전체", en: "All" },
  workflow: { ko: "워크플로", en: "Workflow" },
  github: { ko: "GitHub", en: "GitHub" },
  meetings: { ko: "회의", en: "Meetings" },
  ops: { ko: "운영", en: "Ops" },
  knowledge: { ko: "메모리", en: "Memory" },
} as const;

export type SkillCategoryId = keyof typeof SKILL_CATEGORIES;
export type SkillCategory = Exclude<SkillCategoryId, "all">;

export function normalizeSkillText(skill: SkillCatalogEntry): string {
  return `${skill.name} ${skill.description} ${skill.description_ko}`
    .replace(/\s+/g, " ")
    .toLowerCase();
}

// English terms match whole words only, so "pr" no longer catches "profile" or "prefer".
const CATEGORY_RULES: Array<[SkillCategory, RegExp]> = [
  ["meetings", /\b(meetings?|round ?tables?|briefing|summary|calendar|schedule)\b|회의|라운드테이블/],
  ["github", /\b(github|repos?|repository|issues?|pull requests?|prs?|git|kanban|reviews?)\b/],
  ["ops", /\b(restart|runtime|deploy|release|recover|watch|verify|launcher|sync)/],
  ["knowledge", /\b(memory|digest|transcribe|speech|notebook|context|profile)/],
];

export function categorizeSkill(skill: SkillCatalogEntry): SkillCategory {
  const text = normalizeSkillText(skill);
  return CATEGORY_RULES.find(([, rule]) => rule.test(text))?.[0] ?? "workflow";
}

/** Most calls first, then the latest use, then the name. */
export function compareSkills(left: SkillCatalogEntry, right: SkillCatalogEntry) {
  return right.total_calls - left.total_calls
    || (right.last_used_at ?? 0) - (left.last_used_at ?? 0)
    || left.name.localeCompare(right.name);
}

/** One readable line from a skill description: markdown marks and fences go, the words stay as the server sent them. */
export function plainSkillText(text: string): string {
  return text
    .replace(/```[a-z]*/gi, " ")
    .replace(/\*\*|`/g, "")
    .replace(/^\s*(?:[>#-]+\s*)+/gm, "")
    .replace(/\s+/g, " ")
    .trim();
}

export function formatDateShort(ts: number | null, isKo: boolean, now = Date.now()): string {
  if (!ts) return isKo ? "기록 없음" : "No record";
  const hours = Math.floor((now - ts) / 3_600_000);
  const days = Math.floor(hours / 24);
  if (hours < 1) return isKo ? "방금" : "Just now";
  if (hours < 24) return isKo ? `${hours}시간 전` : `${hours}h ago`;
  if (days === 1) return isKo ? "어제" : "Yesterday";
  if (days < 7) return isKo ? `${days}일 전` : `${days}d ago`;
  return new Date(ts).toLocaleDateString(isKo ? "ko-KR" : "en-US", { month: "short", day: "numeric" });
}

const DAY_MS = 86_400_000;

/** Every UTC date from `days` back to today (the server buckets by UTC date and skips empty days), zero where it sent none. */
export function fillSkillTrend(points: SkillTrendPoint[], days: number, now = Date.now()): SkillTrendPoint[] {
  const counts = new Map(points.map((point) => [point.day, point.count]));
  return Array.from({ length: days + 1 }, (_, index) => {
    const day = new Date(now - (days - index) * DAY_MS).toISOString().slice(0, 10);
    return { day, count: counts.get(day) ?? 0 };
  });
}

/** `2026-11-23` as an axis label (`11/23`) or a reading label (`11월 23일`, `Nov 23`). */
export function trendDayLabel(day: string, isKo: boolean, long = false): string {
  const [, month, date] = day.split("-").map(Number);
  if (!long) return `${month}/${date}`;
  return isKo ? `${month}월 ${date}일` : new Date(Date.UTC(2000, month - 1, date)).toLocaleDateString("en-US", { month: "short", day: "numeric", timeZone: "UTC" });
}

/** Breakdown rows: the most called skills, with how many skills were called at all. */
export function topSkillItems(overall: SkillRankingOverallRow[], shown = 5) {
  const rows = overall.filter((row) => row.calls > 0);
  return { items: rows.slice(0, shown).map((row) => ({ id: row.skill_name, label: row.skill_name, value: row.calls })), called: rows.length };
}

/** The agents that called one skill, most calls first, each matched to a roster agent by role id or id when known. */
export function skillUsers<T extends { id: string; role_id?: string | null }>(byAgent: SkillRankingByAgentRow[], skill: string, agents: T[]) {
  return byAgent
    .filter((row) => row.skill_name === skill)
    .sort((left, right) => right.calls - left.calls)
    .map((row) => ({ row, agent: agents.find((agent) => agent.role_id === row.agent_role_id || agent.id === row.agent_role_id) ?? null }));
}
