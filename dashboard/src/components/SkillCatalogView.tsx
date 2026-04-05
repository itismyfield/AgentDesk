import { useEffect, useState, useMemo } from "react";
import type { SkillCatalogEntry } from "../types";
import { getSkillCatalog } from "../api/client";
import { BookOpen, Search } from "lucide-react";
import { useI18n } from "../i18n";

export default function SkillCatalogView({ embedded = false }: { embedded?: boolean }) {
  const [catalog, setCatalog] = useState<SkillCatalogEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [search, setSearch] = useState("");
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
    return () => { mounted = false; };
  }, []);

  const filtered = useMemo(() => {
    if (!search.trim()) return catalog;
    const q = search.toLowerCase();
    return catalog.filter(
      (s) =>
        s.name.toLowerCase().includes(q) ||
        s.description_ko.toLowerCase().includes(q) ||
        s.description.toLowerCase().includes(q),
    );
  }, [catalog, search]);

  if (loading) {
    return (
      <div className={embedded ? "py-8 text-center" : "flex items-center justify-center h-full"} style={{ color: "var(--th-text-muted)" }}>
        <div className="text-center">
          <BookOpen size={40} className="mx-auto mb-4 opacity-30" />
          <div>{t({ ko: "스킬 로딩 중...", en: "Loading skills..." })}</div>
        </div>
      </div>
    );
  }

  const content = (
    <>
      <div className="flex items-center gap-3 mb-4">
        <BookOpen className="text-blue-400" size={embedded ? 20 : 24} />
        <h1 className={embedded ? "text-base font-semibold" : "text-xl font-bold"} style={{ color: "var(--th-text-heading)" }}>
          {t({ ko: "스킬 카탈로그", en: "Skill Catalog" })}
        </h1>
        <span className="text-xs px-2 py-0.5 rounded-full" style={{ background: "rgba(59,130,246,0.15)", color: "#60a5fa" }}>
          {isKo ? `${catalog.length}개` : catalog.length}
        </span>
      </div>

      {/* Search bar */}
      <div className="relative mb-4">
        <Search size={16} className="absolute left-3 top-1/2 -translate-y-1/2" style={{ color: "var(--th-text-muted)" }} />
        <input
          type="text"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          placeholder={t({ ko: "스킬 검색...", en: "Search skills..." })}
          className="w-full pl-9 pr-3 py-2 rounded-xl text-sm"
          style={{
            background: "var(--th-bg-surface)",
            border: "1px solid var(--th-border)",
            color: "var(--th-text)",
          }}
        />
      </div>

      {filtered.length === 0 && (
        <div className="text-center py-12" style={{ color: "var(--th-text-muted)" }}>
          <BookOpen size={40} className="mx-auto mb-3 opacity-30" />
          <p>{search ? t({ ko: "검색 결과가 없습니다", en: "No search results" }) : t({ ko: "등록된 스킬이 없습니다", en: "No skills registered" })}</p>
        </div>
      )}

      {/* Card grid */}
      <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-3">
        {filtered.map((skill) => (
          <div
            key={skill.name}
            className="rounded-xl border p-4 hover:border-blue-500/30 transition-colors"
            style={{ background: "var(--th-surface)", borderColor: "var(--th-border)" }}
          >
            <div className="font-semibold text-sm mb-1" style={{ color: "var(--th-text)" }}>
              {skill.name}
            </div>
            <div className="text-xs leading-relaxed mb-3" style={{ color: "var(--th-text-muted)" }}>
              {isKo ? skill.description_ko : skill.description}
            </div>
            <div className="flex items-center justify-between text-xs" style={{ color: "var(--th-text-muted)" }}>
              <span>
                {skill.total_calls > 0
                  ? (isKo ? `${skill.total_calls}회 호출` : `${skill.total_calls} calls`)
                  : t({ ko: "미사용", en: "Unused" })}
              </span>
              {skill.last_used_at && (
                <span>
                  {formatDateShort(skill.last_used_at, isKo)}
                </span>
              )}
            </div>
          </div>
        ))}
      </div>
    </>
  );

  if (embedded) return content;

  return (
    <div
      className="p-4 sm:p-6 max-w-5xl mx-auto overflow-auto h-full pb-40"
      style={{ paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      {content}
    </div>
  );
}

function formatDateShort(ts: number, isKo = true): string {
  const d = new Date(ts);
  const now = new Date();
  const diff = now.getTime() - d.getTime();
  const days = Math.floor(diff / (1000 * 60 * 60 * 24));
  if (days === 0) return isKo ? "오늘" : "Today";
  if (days === 1) return isKo ? "어제" : "Yesterday";
  if (days < 7) return isKo ? `${days}일 전` : `${days}d ago`;
  return d.toLocaleDateString(isKo ? "ko-KR" : "en-US", { month: "short", day: "numeric" });
}
