import * as api from "../api/client";
import type { Agent } from "../types";
import type { DailyMissionViewModel } from "./gamification/GamificationShared";

export type MilestoneId =
  | "first_task"
  | "getting_started"
  | "centurion"
  | "veteran"
  | "expert"
  | "master";

export type CardSection = "earned" | "progress" | "locked";
export type Rarity = "common" | "rare" | "epic" | "legendary";

export interface MilestoneMeta {
  id: MilestoneId;
  threshold: number;
  hue: number;
  glyph: string;
  name: {
    ko: string;
    en: string;
  };
  desc: {
    ko: string;
    en: string;
  };
}

export interface LeaderboardEntry {
  id: string;
  name: string;
  avatarEmoji: string;
  xp: number;
  tasksDone: number;
  agent?: Agent;
}

export interface AchievementCardModel {
  id: string;
  section: CardSection;
  milestoneId: string;
  name: string;
  desc: string;
  xp: number;
  rarity: Rarity;
  progress?: number;
  currentXp?: number;
  remainingXp?: number;
  at?: string;
  agentName?: string;
  avatarEmoji?: string;
  agent?: Agent;
  detailLines: string[];
  timelineLines: string[];
}

export const MILESTONES: MilestoneMeta[] = [
  {
    id: "first_task",
    threshold: 10,
    hue: 150,
    glyph: "★",
    name: { ko: "첫 번째 작업 완료", en: "First Task" },
    desc: { ko: "첫 번째 작업을 성공적으로 완료했습니다", en: "Completed the first task" },
  },
  {
    id: "getting_started",
    threshold: 50,
    hue: 210,
    glyph: "✦",
    name: { ko: "본격적인 시작", en: "Getting Started" },
    desc: { ko: "운영 리듬이 안정적으로 시작되었습니다", en: "Settled into the operating rhythm" },
  },
  {
    id: "centurion",
    threshold: 100,
    hue: 85,
    glyph: "100",
    name: { ko: "100 XP 달성", en: "Centurion" },
    desc: { ko: "100 XP를 돌파해 첫 이정표를 넘었습니다", en: "Reached the first 100 XP milestone" },
  },
  {
    id: "veteran",
    threshold: 250,
    hue: 25,
    glyph: "V",
    name: { ko: "베테랑", en: "Veteran" },
    desc: { ko: "꾸준한 운영으로 베테랑 단계에 도달했습니다", en: "Reached the veteran tier through steady work" },
  },
  {
    id: "expert",
    threshold: 500,
    hue: 295,
    glyph: "E",
    name: { ko: "전문가", en: "Expert" },
    desc: { ko: "고난도 운영을 소화하는 전문가 구간입니다", en: "Reached the expert operating tier" },
  },
  {
    id: "master",
    threshold: 1000,
    hue: 50,
    glyph: "∞",
    name: { ko: "마스터", en: "Master" },
    desc: { ko: "최상위 운영 숙련도에 도달했습니다", en: "Reached the master tier" },
  },
];

export const MILESTONE_BY_ID = new Map(MILESTONES.map((milestone) => [milestone.id, milestone]));

export function formatCompact(value: number, locale: string): string {
  return new Intl.NumberFormat(locale, {
    notation: "compact",
    maximumFractionDigits: value >= 1000 ? 1 : 0,
  }).format(value);
}

export function formatExact(value: number, locale: string): string {
  return new Intl.NumberFormat(locale).format(value);
}

export function formatAchievementDate(timestamp: number | null | undefined, locale: string) {
  if (!timestamp || timestamp <= 0) return undefined;
  return new Intl.DateTimeFormat(locale, {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  }).format(new Date(timestamp));
}

export function nextMilestone(xp: number) {
  return MILESTONES.find((milestone) => xp < milestone.threshold) ?? null;
}

export function progressToMilestone(xp: number, milestone: MilestoneMeta): number {
  const index = MILESTONES.findIndex((item) => item.id === milestone.id);
  const previousThreshold = index > 0 ? MILESTONES[index - 1].threshold : 0;
  const total = Math.max(1, milestone.threshold - previousThreshold);
  return Math.max(0, Math.min(1, (xp - previousThreshold) / total));
}

export function normalizeRarity(raw: string | null | undefined, threshold: number): Rarity {
  const normalized = raw?.toLowerCase();
  if (
    normalized === "common" ||
    normalized === "rare" ||
    normalized === "epic" ||
    normalized === "legendary"
  ) {
    return normalized;
  }
  if (threshold >= 1000) return "legendary";
  if (threshold >= 500) return "epic";
  if (threshold >= 100) return "rare";
  return "common";
}

export function getRarityTheme(rarity: Rarity) {
  switch (rarity) {
    case "legendary":
      return {
        accent: "var(--th-accent-warn, var(--warn))",
        border: "color-mix(in srgb, var(--th-accent-warn, var(--warn)) 26%, var(--th-border-subtle, var(--line)) 74%)",
        badgeBg: "color-mix(in srgb, var(--th-accent-warn, var(--warn)) 12%, transparent)",
        badgeText: "var(--th-accent-warn, var(--warn))",
      };
    case "epic":
      return {
        accent: "var(--th-accent-danger, var(--codex))",
        border: "color-mix(in srgb, var(--th-accent-danger, var(--codex)) 24%, var(--th-border-subtle, var(--line)) 76%)",
        badgeBg: "color-mix(in srgb, var(--th-accent-danger, var(--codex)) 12%, transparent)",
        badgeText: "var(--th-accent-danger, var(--codex))",
      };
    case "rare":
      return {
        accent: "var(--th-accent-primary, var(--accent))",
        border: "color-mix(in srgb, var(--th-accent-primary, var(--accent)) 22%, var(--th-border-subtle, var(--line)) 78%)",
        badgeBg: "color-mix(in srgb, var(--th-accent-primary, var(--accent)) 12%, transparent)",
        badgeText: "var(--th-accent-primary, var(--accent))",
      };
    case "common":
    default:
      return {
        accent: "var(--th-text-muted, var(--fg-muted))",
        border: "var(--th-border-subtle, color-mix(in srgb, var(--line) 70%, transparent))",
        badgeBg: "var(--th-overlay-medium, color-mix(in srgb, var(--bg-3) 90%, transparent))",
        badgeText: "var(--th-text-muted, var(--fg-muted))",
      };
  }
}

export function rarityLabel(rarity: Rarity, isKo: boolean) {
  switch (rarity) {
    case "legendary":
      return isKo ? "전설" : "Legendary";
    case "epic":
      return isKo ? "에픽" : "Epic";
    case "rare":
      return isKo ? "레어" : "Rare";
    case "common":
    default:
      return isKo ? "기본" : "Common";
  }
}

export function buildDailyMissions(
  missions: api.DailyMission[],
  isKo: boolean,
): DailyMissionViewModel[] {
  return missions.map((mission) => {
    switch (mission.id) {
      case "dispatches_today":
        return {
          id: mission.id,
          label: isKo ? "오늘 디스패치 5건 완료" : "Complete 5 dispatches today",
          current: mission.current,
          target: mission.target,
          completed: mission.completed,
          description: isKo ? "오늘 실제 완료된 디스패치 수" : "Completed dispatches today",
          xp: 40,
        };
      case "active_agents_today":
        return {
          id: mission.id,
          label: isKo ? "오늘 3명 이상 출항" : "Get 3 agents shipping today",
          current: mission.current,
          target: mission.target,
          completed: mission.completed,
          description: isKo
            ? "오늘 완료 기록이 있는 에이전트 수"
            : "Agents with completed work today",
          xp: 35,
        };
      case "review_queue_zero":
        return {
          id: mission.id,
          label: isKo ? "리뷰 큐 비우기" : "Drain the review queue",
          current: mission.current,
          target: mission.target,
          completed: mission.completed,
          description: isKo
            ? "리뷰 대기 카드를 0으로 유지"
            : "Keep the review queue empty",
          xp: 40,
        };
      default:
        return {
          id: mission.id,
          label: mission.label,
          current: mission.current,
          target: mission.target,
          completed: mission.completed,
        };
    }
  });
}
