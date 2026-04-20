import { BookOpen, Search } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { getSkillCatalog } from "../api/client";
import { useI18n } from "../i18n";
import type { SkillCatalogEntry } from "../types";

const SKILL_CATALOG_SHELL_STYLES = `
  .skill-catalog-shell {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  .skill-catalog-shell .page-header {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: 14px;
    flex-wrap: wrap;
  }

  .skill-catalog-shell .page-title {
    font-size: 22px;
    font-weight: 600;
    letter-spacing: -0.5px;
    line-height: 1.2;
    color: var(--th-text-heading);
  }

  .skill-catalog-shell .page-sub {
    margin-top: 4px;
    max-width: 68ch;
    font-size: 13px;
    color: var(--th-text-muted);
    line-height: 1.65;
  }

  .skill-catalog-shell .card {
    border-radius: 18px;
    border: 1px solid color-mix(in srgb, var(--th-border) 72%, transparent);
    background: color-mix(in srgb, var(--th-card-bg) 94%, transparent);
    overflow: hidden;
  }

  .skill-catalog-shell .chip {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    padding: 2px 8px;
    border-radius: 999px;
    border: 1px solid color-mix(in srgb, var(--th-border) 70%, transparent);
    background: color-mix(in srgb, var(--th-bg-surface) 90%, transparent);
    font-size: 11px;
    font-weight: 500;
    color: var(--th-text-dim);
    font-variant-numeric: tabular-nums;
  }

  .skill-catalog-shell .chip .dot {
    width: 6px;
    height: 6px;
    border-radius: 999px;
    background: currentColor;
    opacity: 0.9;
  }

  .skill-catalog-shell .search-wrap {
    position: relative;
    width: min(100%, 260px);
  }

  .skill-catalog-shell.embedded .search-wrap {
    width: 100%;
  }

  .skill-catalog-shell .search-input {
    width: 100%;
    padding: 7px 10px 7px 30px;
    border-radius: 8px;
    border: 1px solid color-mix(in srgb, var(--th-border) 72%, transparent);
    background: color-mix(in srgb, var(--th-bg-surface) 88%, transparent);
    color: var(--th-text);
    font-size: 12.5px;
  }

  .skill-catalog-shell .metric-grid {
    display: grid;
    gap: 10px;
    grid-template-columns: repeat(4, minmax(0, 1fr));
  }

  .skill-catalog-shell .metric-card {
    padding: 12px 14px;
  }

  .skill-catalog-shell .metric-label {
    font-size: 10px;
    letter-spacing: 0.16em;
    text-transform: uppercase;
    color: var(--th-text-muted);
    font-weight: 600;
  }

  .skill-catalog-shell .metric-value {
    margin-top: 8px;
    font-size: 20px;
    font-weight: 700;
    letter-spacing: -0.03em;
    color: var(--th-text-heading);
  }

  .skill-catalog-shell .skill-tag-row {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }

  .skill-catalog-shell .skill-tag {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 6px 10px;
    border-radius: 999px;
    border: 1px solid color-mix(in srgb, var(--th-border) 72%, transparent);
    background: color-mix(in srgb, var(--th-bg-surface) 86%, transparent);
    font-size: 12px;
    color: var(--th-text-dim);
    transition: background 0.12s ease, border-color 0.12s ease, color 0.12s ease;
  }

  .skill-catalog-shell .skill-tag.active {
    border-color: color-mix(in srgb, var(--th-accent-info) 28%, var(--th-border) 72%);
    background: color-mix(in srgb, var(--th-accent-info) 12%, transparent);
    color: var(--th-text-heading);
  }

  .skill-catalog-shell .skill-layout {
    display: grid;
    gap: 14px;
    grid-template-columns: minmax(0, 1.45fr) minmax(260px, 0.78fr);
  }

  .skill-catalog-shell .skill-grid {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: 12px;
  }

  .skill-catalog-shell.embedded .skill-grid {
    grid-template-columns: 1fr;
  }

  .skill-catalog-shell .skill-card {
    padding: 14px;
  }

  .skill-catalog-shell .skill-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 10px;
  }

  .skill-catalog-shell .skill-name {
    font-size: 14px;
    font-weight: 600;
    color: var(--th-text-heading);
    line-height: 1.35;
    word-break: break-word;
  }

  .skill-catalog-shell .skill-desc {
    margin-top: 10px;
    font-size: 12.5px;
    line-height: 1.65;
    color: var(--th-text-muted);
  }

  .skill-catalog-shell .skill-foot {
    margin-top: 14px;
    display: grid;
    gap: 10px;
    grid-template-columns: repeat(2, minmax(0, 1fr));
  }

  .skill-catalog-shell .skill-stat {
    border-radius: 14px;
    border: 1px solid color-mix(in srgb, var(--th-border) 70%, transparent);
    background: color-mix(in srgb, var(--th-card-bg) 88%, transparent);
    padding: 10px 12px;
  }

  .skill-catalog-shell .skill-stat-label {
    font-size: 10px;
    letter-spacing: 0.16em;
    text-transform: uppercase;
    color: var(--th-text-muted);
    font-weight: 600;
  }

  .skill-catalog-shell .skill-stat-value {
    margin-top: 8px;
    font-size: 13px;
    font-weight: 600;
    color: var(--th-text-heading);
  }

  .skill-catalog-shell .skill-usage {
    margin-top: 12px;
  }

  .skill-catalog-shell .skill-usage-meta {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    font-size: 11px;
    color: var(--th-text-muted);
  }

  .skill-catalog-shell .skill-usage-bar {
    margin-top: 8px;
    height: 5px;
    border-radius: 999px;
    background: color-mix(in srgb, var(--th-border) 72%, transparent);
    overflow: hidden;
  }

  .skill-catalog-shell .skill-usage-fill {
    height: 100%;
    border-radius: inherit;
  }

  .skill-catalog-shell .highlights {
    padding: 14px;
    border-color: color-mix(in srgb, var(--th-accent-primary) 20%, var(--th-border) 80%);
    background: linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, var(--th-accent-primary-soft) 4%) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%);
  }

  .skill-catalog-shell .section-eyebrow {
    font-size: 10px;
    font-weight: 600;
    letter-spacing: 0.16em;
    text-transform: uppercase;
    color: var(--th-text-muted);
  }

  .skill-catalog-shell .section-copy {
    margin-top: 8px;
    font-size: 13px;
    line-height: 1.65;
    color: var(--th-text-muted);
  }

  .skill-catalog-shell .featured-list {
    margin-top: 14px;
    display: grid;
    gap: 10px;
  }

  .skill-catalog-shell .featured-item {
    border-radius: 14px;
    border: 1px solid color-mix(in srgb, var(--th-border) 70%, transparent);
    background: color-mix(in srgb, var(--th-card-bg) 88%, transparent);
    padding: 10px 12px;
  }

  @media (max-width: 1279px) {
    .skill-catalog-shell .skill-layout {
      grid-template-columns: 1fr;
    }

    .skill-catalog-shell .skill-grid {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
  }

  @media (max-width: 767px) {
    .skill-catalog-shell .metric-grid,
    .skill-catalog-shell .skill-grid,
    .skill-catalog-shell .skill-foot {
      grid-template-columns: 1fr;
    }

    .skill-catalog-shell .search-wrap {
      width: 100%;
    }
  }
`;

const SKILL_CATEGORY_META = {
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

const SKILL_ACTIVITY_META = {
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

type SkillCategoryId = keyof typeof SKILL_CATEGORY_META;
type SkillActivityId = keyof typeof SKILL_ACTIVITY_META;
type DerivedSkillEntry = {
  skill: SkillCatalogEntry;
  category: Exclude<SkillCategoryId, "all">;
  activity: SkillActivityId;
  usagePercent: number;
  searchText: string;
};

function normalizeSkillText(skill: SkillCatalogEntry): string {
  return `${skill.name} ${skill.description} ${skill.description_ko}`
    .replace(/\s+/g, " ")
    .toLowerCase();
}

function categorizeSkill(
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

function getSkillActivity(
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

function compareSkillEntries(left: DerivedSkillEntry, right: DerivedSkillEntry) {
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

function formatDateShort(ts: number | null, isKo: boolean): string {
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

export default function SkillCatalogView({
  embedded = false,
}: {
  embedded?: boolean;
}) {
  const [catalog, setCatalog] = useState<SkillCatalogEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [search, setSearch] = useState("");
  const [activeCategory, setActiveCategory] = useState<SkillCategoryId>("all");
  const { t, language } = useI18n();
  const isKo = language === "ko";

  useEffect(() => {
    let mounted = true;

    (async () => {
      try {
        const data = await getSkillCatalog();
        if (mounted) setCatalog(data);
      } catch {
        // ignore
      } finally {
        if (mounted) setLoading(false);
      }
    })();

    return () => {
      mounted = false;
    };
  }, []);

  const derivedCatalog = useMemo(() => {
    const maxCalls = Math.max(1, ...catalog.map((skill) => skill.total_calls));
    return catalog
      .map((skill) => {
        const category = categorizeSkill(skill);
        const activity = getSkillActivity(skill, maxCalls);
        const usagePercent =
          skill.total_calls <= 0
            ? 6
            : Math.max(10, Math.round((skill.total_calls / maxCalls) * 100));

        return {
          skill,
          category,
          activity,
          usagePercent,
          searchText: normalizeSkillText(skill),
        };
      })
      .sort(compareSkillEntries);
  }, [catalog]);

  const categoryCounts = useMemo(() => {
    return derivedCatalog.reduce<Record<Exclude<SkillCategoryId, "all">, number>>(
      (counts, entry) => {
        counts[entry.category] += 1;
        return counts;
      },
      {
        workflow: 0,
        github: 0,
        meetings: 0,
        ops: 0,
        knowledge: 0,
      },
    );
  }, [derivedCatalog]);

  const filtered = useMemo(() => {
    const query = search.trim().toLowerCase();
    return derivedCatalog.filter((entry) => {
      if (activeCategory !== "all" && entry.category !== activeCategory) {
        return false;
      }
      if (!query) return true;

      const categoryMeta = SKILL_CATEGORY_META[entry.category];
      const searchableCategoryText = `${categoryMeta.label.ko} ${categoryMeta.label.en} ${categoryMeta.summary.ko} ${categoryMeta.summary.en}`.toLowerCase();
      return (
        entry.searchText.includes(query) ||
        searchableCategoryText.includes(query)
      );
    });
  }, [activeCategory, derivedCatalog, search]);

  const totalCalls = derivedCatalog.reduce(
    (sum, entry) => sum + entry.skill.total_calls,
    0,
  );

  if (loading) {
    return (
      <div
        className={
          embedded
            ? "py-8 text-center"
            : "flex h-full items-center justify-center"
        }
        style={{ color: "var(--th-text-muted)" }}
      >
        <div className="text-center">
          <BookOpen size={40} className="mx-auto mb-4 opacity-30" />
          <div>{t({ ko: "스킬 로딩 중...", en: "Loading skills..." })}</div>
        </div>
      </div>
    );
  }

  const cards = (
    <div className="skill-grid">
      {filtered.map((entry) => {
        const categoryMeta = SKILL_CATEGORY_META[entry.category];
        const activityMeta = SKILL_ACTIVITY_META[entry.activity];
        const description = isKo
          ? entry.skill.description_ko
          : entry.skill.description;

        return (
          <div key={entry.skill.name} className="card skill-card">
            <div className="skill-head">
              <div className="min-w-0">
                <div className="skill-name">{entry.skill.name}</div>
              </div>
              <span
                className="chip"
                style={{
                  fontSize: 10,
                  borderColor: activityMeta.borderColor,
                  background: activityMeta.background,
                  color: activityMeta.accent,
                }}
              >
                <span className="dot" />
                {isKo ? categoryMeta.label.ko : categoryMeta.label.en}
              </span>
            </div>

            <div className="skill-desc">{description}</div>

            <div className="skill-foot">
              <div className="skill-stat">
                <div className="skill-stat-label">
                  {t({ ko: "호출", en: "Calls" })}
                </div>
                <div className="skill-stat-value">
                  {entry.skill.total_calls > 0
                    ? entry.skill.total_calls.toLocaleString(
                        isKo ? "ko-KR" : "en-US",
                      )
                    : t({ ko: "미사용", en: "Unused" })}
                </div>
              </div>
              <div className="skill-stat">
                <div className="skill-stat-label">
                  {t({ ko: "마지막", en: "Last Used" })}
                </div>
                <div className="skill-stat-value">
                  {formatDateShort(entry.skill.last_used_at, isKo)}
                </div>
              </div>
            </div>

            <div className="skill-usage">
              <div className="skill-usage-meta">
                <span>{isKo ? activityMeta.label.ko : activityMeta.label.en}</span>
                <span>{entry.usagePercent}%</span>
              </div>
              <div className="skill-usage-bar">
                <div
                  className="skill-usage-fill"
                  style={{
                    width: `${entry.usagePercent}%`,
                    background: activityMeta.accent,
                  }}
                />
              </div>
            </div>
          </div>
        );
      })}
    </div>
  );

  const categoryButtons = (
    <div className="skill-tag-row">
      {(Object.keys(SKILL_CATEGORY_META) as SkillCategoryId[]).map((categoryId) => {
        const categoryMeta = SKILL_CATEGORY_META[categoryId];
        const count =
          categoryId === "all" ? catalog.length : categoryCounts[categoryId];

        return (
          <button
            key={categoryId}
            type="button"
            className={`skill-tag${activeCategory === categoryId ? " active" : ""}`}
            onClick={() => setActiveCategory(categoryId)}
          >
            {isKo ? categoryMeta.label.ko : categoryMeta.label.en}
            <span style={{ fontFamily: "var(--font-mono)", opacity: 0.6 }}>
              {count}
            </span>
          </button>
        );
      })}
    </div>
  );

  const emptyState = (
    <div
      className="card"
      style={{
        textAlign: "center",
        padding: "40px 20px",
        color: "var(--th-text-muted)",
      }}
    >
      <BookOpen size={32} style={{ margin: "0 auto 12px", opacity: 0.3 }} />
      <div style={{ fontSize: 13 }}>
        {search
          ? t({ ko: "검색 결과가 없습니다", en: "No search results" })
          : t({ ko: "등록된 스킬이 없습니다", en: "No skills registered" })}
      </div>
    </div>
  );

  if (embedded) {
    return (
      <div className="skill-catalog-shell embedded">
        <style>{SKILL_CATALOG_SHELL_STYLES}</style>
        <div className="search-wrap">
          <Search
            size={14}
            style={{
              position: "absolute",
              left: 10,
              top: "50%",
              transform: "translateY(-50%)",
              color: "var(--th-text-muted)",
            }}
          />
          <input
            type="text"
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder={t({ ko: "스킬 검색…", en: "Search skills…" })}
            className="search-input"
          />
        </div>
        {categoryButtons}
        {filtered.length === 0 ? emptyState : cards}
      </div>
    );
  }

  return (
    <div
      className="page fade-in mx-auto h-full w-full max-w-6xl min-w-0 overflow-x-hidden overflow-y-auto p-4 pb-40 sm:p-6"
      style={{ paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      <div className="page skill-catalog-shell">
        <style>{SKILL_CATALOG_SHELL_STYLES}</style>
        <div className="page-header">
          <div>
            <div className="page-title">
              {t({ ko: "스킬 카탈로그", en: "Skill Catalog" })}
            </div>
            <div className="page-sub">
              {t({
                ko: `에이전트가 호출할 수 있는 툴·훅. ${catalog.length}개 · 누적 ${(
                  totalCalls / 1000
                ).toFixed(1)}K 호출`,
                en: `${catalog.length} callable tools and hooks · ${(totalCalls / 1000).toFixed(1)}K total invocations`,
              })}
            </div>
          </div>
          <div className="search-wrap">
            <Search
              size={14}
              style={{
                position: "absolute",
                left: 10,
                top: "50%",
                transform: "translateY(-50%)",
                color: "var(--th-text-muted)",
              }}
            />
            <input
              type="text"
              value={search}
              onChange={(event) => setSearch(event.target.value)}
              placeholder={t({ ko: "스킬 검색…", en: "Search skills…" })}
              className="search-input"
            />
          </div>
        </div>
        {categoryButtons}
        {filtered.length === 0 ? emptyState : cards}
      </div>
    </div>
  );
}
