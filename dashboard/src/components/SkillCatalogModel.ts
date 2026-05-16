import type { SkillCatalogEntry } from "../types";

export const SKILL_CATEGORY_META = {
  all: {
    label: { ko: "all", en: "All" },
    summary: {
      ko: "전체 스킬을 호출 빈도와 최근성 순으로 정렬합니다.",
      en: "All skills ranked by usage and recency.",
    },
  },
  workflow: {
    label: { ko: "workflow", en: "Workflow" },
    summary: {
      ko: "에이전트 전달, 브리핑, 자동화 루틴에 가까운 스킬.",
      en: "Skills oriented around coordination, briefing, and operational flow.",
    },
  },
  github: {
    label: { ko: "github", en: "GitHub" },
    summary: {
      ko: "이슈, PR, 저장소 흐름과 맞닿은 스킬.",
      en: "Skills tied to issues, PRs, and repository flow.",
    },
  },
  meetings: {
    label: { ko: "meeting", en: "Meeting" },
    summary: {
      ko: "라운드테이블, 회의 요약, 일정 맥락에 가까운 스킬.",
      en: "Skills close to round-tables, summaries, and scheduling context.",
    },
  },
  ops: {
    label: { ko: "ops", en: "Ops" },
    summary: {
      ko: "재시작, 배포, 복구, 런타임 운용 계열.",
      en: "Restart, deploy, recovery, and runtime operations.",
    },
  },
  knowledge: {
    label: { ko: "memory", en: "Memory" },
    summary: {
      ko: "메모리, 다이제스트, 전사, 음성화처럼 지식 축적에 가까운 스킬.",
      en: "Memory, digest, transcription, and knowledge-shaping skills.",
    },
  },
} as const;

export const SKILL_ACTIVITY_META = {
  core: {
    label: { ko: "core", en: "Core" },
    accent: "var(--th-accent-primary)",
    background: "color-mix(in srgb, var(--th-accent-primary-soft) 76%, transparent)",
    borderColor:
      "color-mix(in srgb, var(--th-accent-primary) 26%, var(--th-border) 74%)",
  },
  recent: {
    label: { ko: "recent", en: "Recent" },
    accent: "var(--th-accent-success)",
    background: "color-mix(in srgb, var(--th-accent-success) 12%, transparent)",
    borderColor:
      "color-mix(in srgb, var(--th-accent-success) 22%, var(--th-border) 78%)",
  },
  steady: {
    label: { ko: "steady", en: "Steady" },
    accent: "var(--th-accent-info)",
    background: "color-mix(in srgb, var(--th-accent-info) 12%, transparent)",
    borderColor:
      "color-mix(in srgb, var(--th-accent-info) 22%, var(--th-border) 78%)",
  },
  dormant: {
    label: { ko: "quiet", en: "Quiet" },
    accent: "var(--th-text-muted)",
    background: "color-mix(in srgb, var(--th-bg-surface) 88%, transparent)",
    borderColor:
      "color-mix(in srgb, var(--th-border) 72%, transparent)",
  },
} as const;

export type SkillCategoryId = keyof typeof SKILL_CATEGORY_META;
export type SkillActivityId = keyof typeof SKILL_ACTIVITY_META;
export type DerivedSkillEntry = {
  skill: SkillCatalogEntry;
  category: Exclude<SkillCategoryId, "all">;
  activity: SkillActivityId;
  usagePercent: number;
  searchText: string;
};

export function normalizeSkillText(skill: SkillCatalogEntry): string {
  return `${skill.name} ${skill.description} ${skill.description_ko}`
    .replace(/\s+/g, " ")
    .toLowerCase();
}

export function categorizeSkill(
  skill: SkillCatalogEntry,
): Exclude<SkillCategoryId, "all"> {
  const text = normalizeSkillText(skill);
  if (
    /meeting|round table|회의|라운드테이블|briefing|summary|calendar|schedule/.test(
      text,
    )
  ) {
    return "meetings";
  }
  if (/github|repo|repository|issue|pull request|pr|git|kanban|review/.test(text)) {
    return "github";
  }
  if (/restart|runtime|deploy|release|recover|watch|verify|launcher|sync/.test(text)) {
    return "ops";
  }
  if (/memory|digest|transcribe|speech|notebook|context|profile|summary/.test(text)) {
    return "knowledge";
  }
  return "workflow";
}

export function getSkillActivity(
  skill: SkillCatalogEntry,
  maxCalls: number,
): SkillActivityId {
  const now = Date.now();
  const threeDaysMs = 3 * 24 * 60 * 60 * 1000;
  if (skill.total_calls <= 0) return "dormant";
  if (skill.last_used_at && now - skill.last_used_at <= threeDaysMs) {
    return "recent";
  }
  if (skill.total_calls >= Math.max(10, maxCalls * 0.45)) {
    return "core";
  }
  return "steady";
}

export function compareSkillEntries(left: DerivedSkillEntry, right: DerivedSkillEntry) {
  if (left.skill.total_calls !== right.skill.total_calls) {
    return right.skill.total_calls - left.skill.total_calls;
  }
  const rightLast = right.skill.last_used_at ?? 0;
  const leftLast = left.skill.last_used_at ?? 0;
  if (leftLast !== rightLast) {
    return rightLast - leftLast;
  }
  return left.skill.name.localeCompare(right.skill.name);
}

export function formatDateShort(ts: number | null, isKo: boolean): string {
  if (!ts) return isKo ? "기록 없음" : "No record";

  const date = new Date(ts);
  const now = new Date();
  const diff = now.getTime() - date.getTime();
  const hours = Math.floor(diff / (1000 * 60 * 60));
  const days = Math.floor(hours / 24);

  if (hours < 1) return isKo ? "방금" : "Just now";
  if (hours < 24) return isKo ? `${hours}시간 전` : `${hours}h ago`;
  if (days === 1) return isKo ? "어제" : "Yesterday";
  if (days < 7) return isKo ? `${days}일 전` : `${days}d ago`;

  return date.toLocaleDateString(isKo ? "ko-KR" : "en-US", {
    month: "short",
    day: "numeric",
  });
}
