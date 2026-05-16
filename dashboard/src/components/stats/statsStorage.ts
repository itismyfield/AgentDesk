import type { SkillRankingResponse } from "../../api";
import type { TokenAnalyticsResponse } from "../../types";
import type { Period } from "./statsModel";

// SWR persistence (#1250). sessionStorage so cross-tab leakage is avoided
// and reload-after-deploy doesn't render the analytics empty.
const ANALYTICS_STORAGE_PREFIX = "stats:token-analytics:";
const SKILL_RANKING_STORAGE_PREFIX = "stats:skill-ranking:";

export function readPersistedAnalytics(period: Period): TokenAnalyticsResponse | null {
  if (typeof sessionStorage === "undefined") return null;
  try {
    const raw = sessionStorage.getItem(ANALYTICS_STORAGE_PREFIX + period);
    return raw ? (JSON.parse(raw) as TokenAnalyticsResponse) : null;
  } catch {
    return null;
  }
}

export function writePersistedAnalytics(period: Period, value: TokenAnalyticsResponse): void {
  if (typeof sessionStorage === "undefined") return;
  try {
    sessionStorage.setItem(ANALYTICS_STORAGE_PREFIX + period, JSON.stringify(value));
  } catch {
    // quota or serialization failures are fine; next fetch refills.
  }
}

export function readPersistedSkillRanking(period: Period): SkillRankingResponse | null {
  if (typeof sessionStorage === "undefined") return null;
  try {
    const raw = sessionStorage.getItem(SKILL_RANKING_STORAGE_PREFIX + period);
    return raw ? (JSON.parse(raw) as SkillRankingResponse) : null;
  } catch {
    return null;
  }
}

export function writePersistedSkillRanking(period: Period, value: SkillRankingResponse): void {
  if (typeof sessionStorage === "undefined") return;
  try {
    sessionStorage.setItem(SKILL_RANKING_STORAGE_PREFIX + period, JSON.stringify(value));
  } catch {
    // quota or serialization failures are fine; next fetch refills.
  }
}
