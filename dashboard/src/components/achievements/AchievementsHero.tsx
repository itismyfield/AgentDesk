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
      <div className="flex flex-col gap-4 sm:flex-row sm:items-end sm:justify-between">
        <div>
          <div
            className="text-[11px] font-semibold uppercase tracking-[0.16em]"
            style={{ color: "var(--th-text-muted, var(--fg-muted))" }}
          >
            {boardLabel}
          </div>
          <h1
            className="mt-2 text-3xl font-semibold tracking-tight sm:text-4xl"
            style={{ color: "var(--th-text-heading, var(--fg))" }}
          >
            {title}
          </h1>
          <p
            className="mt-2 max-w-2xl text-sm leading-7 sm:text-base"
            style={{ color: "var(--th-text-secondary, var(--fg-muted))" }}
          >
            {subtitle}
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <span
            className="inline-flex items-center gap-2 rounded-full border px-3 py-1.5 text-xs font-semibold"
            style={{
              borderColor:
                "color-mix(in srgb, var(--th-accent-success, var(--ok)) 26%, var(--th-border-subtle, var(--line)) 74%)",
              background:
                "color-mix(in srgb, var(--th-accent-success, var(--ok)) 10%, transparent)",
              color: "var(--th-accent-success, var(--ok))",
            }}
          >
            <span
              className="h-2 w-2 rounded-full"
              style={{ background: "var(--th-accent-success, var(--ok))" }}
            />
            {earnedCount} / {totalCount} {unlockedLabel}
          </span>
        </div>
      </div>

      <div
        className="rounded-[1.5rem] border p-5 sm:p-6"
        style={{
          borderColor: "var(--th-border-subtle, var(--line))",
          background:
            "linear-gradient(135deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 55%, color-mix(in srgb, var(--th-accent-primary, var(--accent)) 8%, transparent) 100%)",
        }}
      >
        <div className="flex flex-col gap-5 lg:flex-row lg:items-center">
          <div className="relative">
            <LevelRing
              value={heroLevel.progress * 100}
              size={94}
              stroke={7}
              color="var(--th-accent-primary, var(--accent))"
              trackColor="color-mix(in srgb, var(--th-border-subtle, var(--line)) 70%, transparent)"
            >
              <div className="text-center">
                <div
                  className="text-[10px] font-semibold uppercase tracking-[0.14em]"
                  style={{ color: "var(--th-text-muted, var(--fg-muted))" }}
                >
                  LEVEL
                </div>
                <div
                  className="mt-1 text-[26px] font-semibold leading-none"
                  style={{ color: "var(--th-text-heading, var(--fg))" }}
                >
                  {heroLevel.level}
                </div>
              </div>
            </LevelRing>
          </div>

          <div className="min-w-0 flex-1">
            <div
              className="text-xl font-semibold"
              style={{ color: "var(--th-text-heading, var(--fg))" }}
            >
              {heroTitle}
            </div>
            <div
              className="mt-1 text-sm"
              style={{ color: "var(--th-text-secondary, var(--fg-muted))" }}
            >
              {heroName
                ? `${heroName} · XP ${formatExact(heroXp, locale)}`
                : `XP ${formatExact(heroXp, locale)}`}
              {Number.isFinite(heroLevel.nextThreshold)
                ? isKo
                  ? ` · 다음 레벨까지 ${formatExact(heroNextXp, locale)}`
                  : ` · ${formatExact(heroNextXp, locale)} to next level`
                : isKo
                  ? " · 최상위 레벨"
                  : " · Max level"}
            </div>
            <div
              className="mt-4 overflow-hidden rounded-full"
              style={{
                height: 6,
                background:
                  "color-mix(in srgb, var(--th-border-subtle, var(--line)) 68%, transparent)",
              }}
            >
              <div
                style={{
                  width: `${Math.round(heroLevel.progress * 100)}%`,
                  height: "100%",
                  background:
                    "linear-gradient(90deg, var(--th-accent-primary, var(--accent)), var(--th-accent-danger, var(--codex)))",
                }}
              />
            </div>
          </div>

          <div className="grid min-w-[min(100%,340px)] grid-cols-3 gap-3">
            {[
              {
                value: earnedCount,
                label: unlockedLabel,
              },
              {
                value: uniqueEarners,
                label: agentsLabel,
              },
              {
                value: formatCompact(totalOrgXp, locale),
                label: "TOTAL XP",
              },
            ].map((metric) => (
              <div
                key={metric.label}
                className="rounded-2xl border px-3 py-3 text-center"
                style={{
                  borderColor: "var(--th-border-subtle, var(--line))",
                  background: "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)",
                }}
              >
                <div
                  className="text-[21px] font-semibold"
                  style={{ color: "var(--th-text-heading, var(--fg))" }}
                >
                  {metric.value}
                </div>
                <div
                  className="mt-1 text-[10.5px] font-semibold uppercase tracking-[0.08em]"
                  style={{ color: "var(--th-text-muted, var(--fg-muted))" }}
                >
                  {metric.label}
                </div>
              </div>
            ))}
          </div>
        </div>
      </div>
    </>
  );
}
