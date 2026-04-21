import type { CSSProperties, ReactNode } from "react";

export interface DailyMissionViewModel {
  id: string;
  label: string;
  current: number;
  target: number;
  completed: boolean;
  description?: string;
  xp?: number;
}

export interface MissionResetCountdown {
  hours: number;
  minutes: number;
  totalMinutes: number;
}

export function getAgentLevelFromXp(xp: number) {
  const thresholds = [0, 100, 300, 600, 1000, 1600, 2500, 4000, 6000, 10000];
  let level = 1;
  for (let index = thresholds.length - 1; index >= 0; index -= 1) {
    if (xp >= thresholds[index]) {
      level = index + 1;
      break;
    }
  }
  const nextThreshold = thresholds[Math.min(level, thresholds.length - 1)] ?? Infinity;
  const currentThreshold = thresholds[level - 1] ?? 0;
  const progress =
    nextThreshold === Infinity
      ? 1
      : (xp - currentThreshold) / Math.max(1, nextThreshold - currentThreshold);
  return {
    level,
    progress: Math.min(1, Math.max(0, progress)),
    nextThreshold,
    currentThreshold,
  };
}

export function getMissionCompletionPercent(mission: DailyMissionViewModel) {
  if (mission.target <= 0) return 100;
  return Math.max(0, Math.min(100, Math.round((mission.current / mission.target) * 100)));
}

export function getMissionResetCountdown(now = new Date()): MissionResetCountdown {
  const nextMidnight = new Date(now);
  nextMidnight.setHours(24, 0, 0, 0);
  const diffMs = Math.max(0, nextMidnight.getTime() - now.getTime());
  const totalMinutes = Math.ceil(diffMs / 60000);
  return {
    hours: Math.floor(totalMinutes / 60),
    minutes: totalMinutes % 60,
    totalMinutes,
  };
}

export function getMissionTotalXp(missions: DailyMissionViewModel[]) {
  return missions.reduce((sum, mission) => sum + (mission.xp ?? 0), 0);
}

export function LevelRing({
  value,
  size = 88,
  stroke = 6,
  color = "var(--accent, var(--th-accent-primary))",
  trackColor = "var(--bg-3, var(--th-overlay-medium))",
  dataTestId,
  children,
}: {
  value: number;
  size?: number;
  stroke?: number;
  color?: string;
  trackColor?: string;
  dataTestId?: string;
  children?: ReactNode;
}) {
  const radius = (size - stroke) / 2;
  const circumference = 2 * Math.PI * radius;
  const offset = circumference - (Math.max(0, Math.min(100, value)) / 100) * circumference;

  return (
    <div data-testid={dataTestId} style={{ position: "relative", width: size, height: size }}>
      <svg width={size} height={size} aria-hidden="true">
        <circle
          cx={size / 2}
          cy={size / 2}
          r={radius}
          fill="none"
          stroke={trackColor}
          strokeWidth={stroke}
        />
        <circle
          cx={size / 2}
          cy={size / 2}
          r={radius}
          fill="none"
          stroke={color}
          strokeWidth={stroke}
          strokeDasharray={circumference}
          strokeDashoffset={offset}
          strokeLinecap="round"
          transform={`rotate(-90 ${size / 2} ${size / 2})`}
          style={{ transition: "stroke-dashoffset 0.6s ease" }}
        />
      </svg>
      {children ? (
        <div
          style={{
            position: "absolute",
            inset: 0,
            display: "grid",
            placeItems: "center",
          }}
        >
          {children}
        </div>
      ) : null}
    </div>
  );
}

export function StreakCounter({
  title,
  value,
  subtitle,
  detail,
  badgeLabel,
  icon,
  className,
  style,
  accent = "var(--th-accent-danger, var(--warn))",
  dataTestId,
}: {
  title: string;
  value: string;
  subtitle: string;
  detail?: string;
  badgeLabel?: string;
  icon?: ReactNode;
  className?: string;
  style?: CSSProperties;
  accent?: string;
  dataTestId?: string;
}) {
  return (
    <div
      data-testid={dataTestId}
      className={className}
      style={{
        borderRadius: 20,
        border: "1px solid var(--th-border-subtle, color-mix(in srgb, var(--line) 70%, transparent))",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 94%, transparent) 100%)",
        ...style,
      }}
    >
      <div className="flex items-center justify-between gap-3 px-4 py-4">
        <div className="flex min-w-0 items-center gap-3">
          <div
            className="flex h-11 w-11 items-center justify-center rounded-2xl border"
            style={{
              borderColor: "color-mix(in srgb, var(--th-border-subtle) 72%, transparent)",
              background: "color-mix(in srgb, var(--th-bg-surface) 88%, transparent)",
              color: accent,
            }}
          >
            {icon}
          </div>
          <div className="min-w-0">
            <div
              className="text-[11px] font-semibold uppercase tracking-[0.14em]"
              style={{ color: "var(--th-text-muted, var(--fg-muted))" }}
            >
              {title}
            </div>
            <div
              className="mt-1 text-2xl font-semibold tracking-tight"
              style={{ color: "var(--th-text-heading, var(--fg))" }}
            >
              {value}
            </div>
            <div
              className="mt-1 text-xs"
              style={{ color: "var(--th-text-secondary, var(--fg-muted))" }}
            >
              {subtitle}
            </div>
          </div>
        </div>
        {badgeLabel ? (
          <span
            className="shrink-0 rounded-full px-2.5 py-1 text-[11px] font-semibold"
            style={{
              background: "color-mix(in srgb, var(--th-overlay-medium, var(--bg-3)) 92%, transparent)",
              color: accent,
            }}
          >
            {badgeLabel}
          </span>
        ) : null}
      </div>
      {detail ? (
        <div
          className="border-t px-4 py-3 text-[11px]"
          style={{
            borderColor: "var(--th-border-subtle, color-mix(in srgb, var(--line) 70%, transparent))",
            color: "var(--th-text-muted, var(--fg-muted))",
          }}
        >
          {detail}
        </div>
      ) : null}
    </div>
  );
}

export function DailyMissionList({
  missions,
  emptyLabel,
  doneLabel,
  progressLabel,
  className,
  style,
  itemTestIdPrefix,
}: {
  missions: DailyMissionViewModel[];
  emptyLabel: string;
  doneLabel: string;
  progressLabel: string;
  className?: string;
  style?: CSSProperties;
  itemTestIdPrefix?: string;
}) {
  return (
    <div className={className} style={style}>
      {missions.length === 0 ? (
        <div
          className="rounded-2xl border px-4 py-6 text-sm"
          style={{
            borderColor: "var(--th-border-subtle, color-mix(in srgb, var(--line) 70%, transparent))",
            color: "var(--th-text-muted, var(--fg-muted))",
            background: "var(--th-overlay-subtle, color-mix(in srgb, var(--bg-2) 92%, transparent))",
          }}
        >
          {emptyLabel}
        </div>
      ) : (
        <div className="space-y-3">
          {missions.map((mission) => {
            const percent = getMissionCompletionPercent(mission);
            return (
              <div
                key={mission.id}
                data-testid={
                  itemTestIdPrefix ? `${itemTestIdPrefix}-${mission.id}` : undefined
                }
                className="rounded-2xl border px-3 py-3"
                style={{
                  borderColor: "var(--th-border-subtle, color-mix(in srgb, var(--line) 70%, transparent))",
                  background:
                    "var(--th-card-bg, color-mix(in srgb, var(--bg-2) 92%, transparent))",
                }}
              >
                <div className="flex items-start justify-between gap-3">
                  <div className="min-w-0">
                    <div
                      className="text-sm font-medium"
                      style={{ color: "var(--th-text-heading, var(--fg))" }}
                    >
                      {mission.label}
                    </div>
                    {mission.description ? (
                      <div
                        className="mt-1 text-xs leading-5"
                        style={{ color: "var(--th-text-muted, var(--fg-muted))" }}
                      >
                        {mission.description}
                      </div>
                    ) : null}
                  </div>
                  <div className="flex shrink-0 items-center gap-2">
                    {typeof mission.xp === "number" ? (
                      <span
                        className="rounded-full px-2 py-1 text-[10px] font-semibold"
                        style={{
                          background:
                            "var(--th-overlay-medium, color-mix(in srgb, var(--bg-3) 90%, transparent))",
                          color: "var(--th-text-muted, var(--fg-muted))",
                        }}
                      >
                        +{mission.xp} XP
                      </span>
                    ) : null}
                    <span
                      className="rounded-full px-2.5 py-1 text-[11px] font-semibold"
                      style={{
                        background:
                          "var(--th-overlay-medium, color-mix(in srgb, var(--bg-3) 90%, transparent))",
                        color: mission.completed
                          ? "var(--th-accent-success, var(--ok))"
                          : "var(--th-accent-primary, var(--accent))",
                      }}
                    >
                      {mission.completed ? doneLabel : `${mission.current}/${mission.target}`}
                    </span>
                  </div>
                </div>
                <div
                  className="mt-3 h-1.5 rounded-full"
                  style={{
                    background:
                      "color-mix(in srgb, var(--th-border-subtle, var(--line)) 68%, transparent)",
                  }}
                >
                  <div
                    className="h-full rounded-full"
                    style={{
                      width: `${percent}%`,
                      background: mission.completed
                        ? "var(--th-accent-success, var(--ok))"
                        : "var(--th-accent-primary, var(--accent))",
                    }}
                  />
                </div>
                <div
                  className="mt-2 text-[11px]"
                  style={{ color: "var(--th-text-muted, var(--fg-muted))" }}
                >
                  {progressLabel} {percent}%
                </div>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

export function DailyMissions({
  missions,
  emptyLabel,
  doneLabel,
  progressLabel,
  resetLabel,
  totalXpLabel,
  className,
  style,
  dataTestId,
  itemTestIdPrefix,
}: {
  missions: DailyMissionViewModel[];
  emptyLabel: string;
  doneLabel: string;
  progressLabel: string;
  resetLabel?: string;
  totalXpLabel?: string;
  className?: string;
  style?: CSSProperties;
  dataTestId?: string;
  itemTestIdPrefix?: string;
}) {
  const fallbackTotalXpLabel =
    missions.length > 0 ? `+${getMissionTotalXp(missions)} XP` : undefined;

  return (
    <div data-testid={dataTestId} className={className} style={style}>
      {resetLabel || totalXpLabel || fallbackTotalXpLabel ? (
        <div className="mb-3 flex items-center justify-between gap-3">
          <div
            className="text-[11px]"
            style={{ color: "var(--th-text-muted, var(--fg-muted))" }}
          >
            {resetLabel}
          </div>
          {totalXpLabel || fallbackTotalXpLabel ? (
            <span
              className="rounded-full px-2.5 py-1 text-[11px] font-semibold"
              style={{
                background:
                  "var(--th-overlay-medium, color-mix(in srgb, var(--bg-3) 90%, transparent))",
                color: "var(--th-text-secondary, var(--fg))",
              }}
            >
              {totalXpLabel ?? fallbackTotalXpLabel}
            </span>
          ) : null}
        </div>
      ) : null}
      <DailyMissionList
        missions={missions}
        emptyLabel={emptyLabel}
        doneLabel={doneLabel}
        progressLabel={progressLabel}
        itemTestIdPrefix={itemTestIdPrefix}
      />
    </div>
  );
}
