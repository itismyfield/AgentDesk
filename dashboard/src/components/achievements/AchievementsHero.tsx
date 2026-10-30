import { LevelRing } from "../gamification/GamificationShared";
import { formatCompact, formatExact } from "../achievementsModel";

interface AchievementsHeroProps {
  isKo: boolean;
  locale: string;
  heroName?: string;
  heroXp: number;
  heroTitle: string;
  heroNextXp: number;
  heroLevel: {
    level: number;
    progress: number;
    nextThreshold: number;
  };
  earnedCount: number;
  totalCount: number;
  uniqueEarners: number;
  totalOrgXp: number;
  title: string;
  subtitle: string;
  boardLabel: string;
  unlockedLabel: string;
  agentsLabel: string;
}

export function AchievementsHero({
  isKo,
  locale,
  heroName,
  heroXp,
  heroTitle,
  heroNextXp,
  heroLevel,
  earnedCount,
  totalCount,
  uniqueEarners,
  totalOrgXp,
  title,
  subtitle,
  boardLabel,
  unlockedLabel,
  agentsLabel,
}: AchievementsHeroProps) {
  return (
    <>
      <header className="flex flex-wrap items-start justify-between gap-4 border-b border-th-border-subtle pb-5">
        <div className="min-w-0">
          <p className="text-xs text-th-text-muted">{boardLabel}</p>
          <h1 className="mt-2 text-2xl font-semibold tracking-tight text-th-text-heading">{title}</h1>
          <p className="mt-2 max-w-2xl text-xs leading-5 text-th-text-muted">{subtitle}</p>
        </div>
        <span className="text-xs leading-5 tabular-nums text-th-text-muted">{earnedCount} / {totalCount} {unlockedLabel}</span>
      </header>
      <section className="grid min-w-0 gap-4 rounded-lg border border-th-border-subtle bg-th-bg-surface p-4 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-center sm:px-5" aria-label={isKo ? "성과 요약" : "Achievement summary"}>
        <div className="flex min-w-0 items-center gap-4">
          <LevelRing value={heroLevel.progress * 100} size={60} stroke={4} color="var(--th-accent-primary)" trackColor="var(--th-border-subtle)">
            <span className="text-lg font-semibold tabular-nums text-th-text-heading">{heroLevel.level}</span>
          </LevelRing>
          <div className="min-w-0">
            <h2 className="text-sm font-semibold text-th-text-heading [overflow-wrap:anywhere]">{heroTitle}{heroName ? ` · ${heroName}` : ""}</h2>
            <p className="mt-1 text-xs leading-5 tabular-nums text-th-text-muted">XP {formatExact(heroXp,locale)}</p>
            <p className="text-xs leading-5 tabular-nums text-th-text-muted">{Number.isFinite(heroLevel.nextThreshold) ? isKo ? `다음 레벨까지 ${formatExact(heroNextXp,locale)} XP` : `${formatExact(heroNextXp,locale)} XP to next level` : isKo ? "최상위 레벨" : "Max level"}</p>
          </div>
        </div>
        <dl className="grid grid-cols-3 gap-4 border-t border-th-border-subtle pt-4 sm:border-t-0 sm:border-l sm:pl-6 sm:pt-0">
          {[{value:earnedCount,label:unlockedLabel},{value:uniqueEarners,label:agentsLabel},{value:formatCompact(totalOrgXp,locale),label:"Total XP"}].map(metric=><div key={metric.label} className="min-w-0"><dt className="text-xs leading-5 text-th-text-muted">{metric.label}</dt><dd className="mt-1 text-xl font-semibold tabular-nums text-th-text-heading [overflow-wrap:anywhere]">{metric.value}</dd></div>)}
        </dl>
      </section>
    </>
  );
}
