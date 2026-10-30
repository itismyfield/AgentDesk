import { BookOpen, Search } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { getSkillCatalog } from "../api/client";
import { useI18n } from "../i18n";
import type { SkillCatalogEntry } from "../types";
import {
  SKILL_ACTIVITY_META,
  SKILL_CATEGORY_META,
  categorizeSkill,
  compareSkillEntries,
  formatDateShort,
  getSkillActivity,
  normalizeSkillText,
  type SkillCategoryId,
} from "./SkillCatalogModel";

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
    <div className="grid min-w-0 grid-cols-1 gap-3">
      {filtered.map((entry) => {
        const categoryMeta = SKILL_CATEGORY_META[entry.category];
        const activityMeta = SKILL_ACTIVITY_META[entry.activity];
        const description = isKo
          ? entry.skill.description_ko
          : entry.skill.description;

        return (
          <div key={entry.skill.name} className="min-w-0 rounded-lg border border-th-border-subtle bg-th-bg-surface p-4">
            <div className="flex min-w-0 flex-wrap items-start justify-between gap-2">
              <div className="min-w-0">
                <div className="text-sm font-semibold leading-6 text-th-text-heading [overflow-wrap:anywhere]">{entry.skill.name}</div>
              </div>
              <span
                className="inline-flex max-w-full shrink-0 items-center gap-1 text-xs leading-5 text-th-text-muted"
                style={{
                  color: "var(--th-text-muted)",
                }}
              >
                <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-current" />
                {isKo ? categoryMeta.label.ko : categoryMeta.label.en}
              </span>
            </div>

            <div className="mt-2 text-xs leading-5 text-th-text-secondary [overflow-wrap:anywhere]">{description}</div>

            <div className="mt-3 grid grid-cols-2 gap-4 text-xs">
              <div className="min-w-0">
                <div className="text-th-text-muted">
                  {t({ ko: "호출", en: "Calls" })}
                </div>
                <div className="mt-1 font-medium tabular-nums text-th-text-primary">
                  {entry.skill.total_calls > 0
                    ? entry.skill.total_calls.toLocaleString(
                        isKo ? "ko-KR" : "en-US",
                      )
                    : t({ ko: "미사용", en: "Unused" })}
                </div>
              </div>
              <div className="min-w-0">
                <div className="text-th-text-muted">
                  {t({ ko: "마지막", en: "Last Used" })}
                </div>
                <div className="mt-1 font-medium tabular-nums text-th-text-primary">
                  {formatDateShort(entry.skill.last_used_at, isKo)}
                </div>
              </div>
            </div>

            <div className="mt-3">
              <div className="flex flex-wrap justify-between gap-2 text-xs leading-5 text-th-text-muted">
                <span>{isKo ? activityMeta.label.ko : activityMeta.label.en}</span>
                <span>{entry.usagePercent}%</span>
              </div>
              <div className="mt-1 h-1 overflow-hidden rounded-full bg-th-border-subtle">
                <div
                  className="h-full rounded-full"
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
    <div className="flex flex-wrap gap-1 border-b border-th-border-subtle">
      {(Object.keys(SKILL_CATEGORY_META) as SkillCategoryId[]).map((categoryId) => {
        const categoryMeta = SKILL_CATEGORY_META[categoryId];
        const count =
          categoryId === "all" ? catalog.length : categoryCounts[categoryId];

        return (
          <button
            key={categoryId}
            type="button"
            className={`inline-flex min-h-11 min-w-11 items-center gap-2 border-b-2 px-3 text-xs font-medium ${activeCategory === categoryId ? "border-th-text-primary text-th-text-primary" : "border-transparent text-th-text-muted"}`}
            aria-pressed={activeCategory === categoryId}
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
      className="min-w-0 rounded-lg border border-th-border-subtle bg-th-bg-surface"
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
      <div className="flex min-w-0 flex-col gap-4">
        <div className="relative min-w-0 w-full">
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
            type="search"
            aria-label={t({ ko: "스킬 검색", en: "Search skills" })}
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder={t({ ko: "스킬 검색…", en: "Search skills…" })}
            className="min-h-11 w-full min-w-0 rounded-md border border-th-input-border bg-th-input-bg pr-3 pl-8 text-sm text-th-text-primary"
          />
        </div>
        {categoryButtons}
        {filtered.length === 0 ? emptyState : cards}
      </div>
    );
  }

  return (
    <div
      className="mx-auto h-full w-full max-w-[88rem] min-w-0 overflow-x-hidden overflow-y-auto p-4 pb-8 sm:p-8"
      style={{ paddingBottom: "max(2rem, env(safe-area-inset-bottom))" }}
    >
      <div className="flex min-w-0 flex-col gap-4">
        <div className="flex flex-wrap items-start justify-between gap-4 border-b border-th-border-subtle pb-4">
          <div>
            <div className="text-2xl font-semibold tracking-tight text-th-text-heading">
              {t({ ko: "스킬 카탈로그", en: "Skill Catalog" })}
            </div>
            <div className="mt-2 max-w-2xl text-xs leading-5 text-th-text-muted">
              {t({
                ko: `에이전트가 호출할 수 있는 툴·훅. ${catalog.length}개 · 누적 ${(
                  totalCalls / 1000
                ).toFixed(1)}K 호출`,
                en: `${catalog.length} callable tools and hooks · ${(totalCalls / 1000).toFixed(1)}K total invocations`,
              })}
            </div>
          </div>
          <div className="relative min-w-0 w-full">
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
              className="min-h-11 w-full min-w-0 rounded-md border border-th-input-border bg-th-input-bg pr-3 pl-8 text-sm text-th-text-primary"
            />
          </div>
        </div>
        {categoryButtons}
        {filtered.length === 0 ? emptyState : cards}
      </div>
    </div>
  );
}
