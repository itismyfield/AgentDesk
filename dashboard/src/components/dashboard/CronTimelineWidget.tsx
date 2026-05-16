import { useEffect, useMemo, useState } from "react";
import * as api from "../../api/client";
import type { TFunction } from "./model";
import { cx, dashboardBadge, dashboardCard } from "./ui";

const DEFAULT_CRON_TIMELINE_WINDOW_MS = 60 * 60_000;
export function formatCompactDuration(ms: number): string {
  const safeMs = Math.max(ms, 1_000);
  if (safeMs % 86_400_000 === 0) return `${safeMs / 86_400_000}d`;
  if (safeMs % 3_600_000 === 0) return `${safeMs / 3_600_000}h`;
  if (safeMs >= 3_600_000) return `${Math.round(safeMs / 3_600_000)}h`;
  if (safeMs % 60_000 === 0) return `${safeMs / 60_000}m`;
  if (safeMs >= 60_000) return `${Math.round(safeMs / 60_000)}m`;
  return `${Math.round(safeMs / 1_000)}s`;
}

export function describeCronSchedule(
  schedule: api.CronSchedule,
  localeTag = "en-US",
): string {
  if (schedule.kind === "every" && schedule.everyMs) {
    return `Every ${formatCompactDuration(schedule.everyMs)}`;
  }
  if (schedule.kind === "cron" && schedule.cron) {
    return schedule.cron;
  }
  if (schedule.kind === "at" && schedule.atMs) {
    return new Date(schedule.atMs).toLocaleString(localeTag, {
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
    });
  }
  return "Schedule unavailable";
}

function clampPercent(value: number): number {
  return Math.min(100, Math.max(0, value));
}

export interface CronTimelineMetrics {
  windowStartMs: number;
  windowEndMs: number;
  lastRunAtMs: number | null;
  nextRunAtMs: number | null;
  nowPercent: number;
  lastPercent: number | null;
  nextPercent: number | null;
  overdue: boolean;
  intervalLabel: string;
}

export function buildCronTimelineMetrics(
  job: api.CronJobGlobal,
  now = Date.now(),
  localeTag = "en-US",
): CronTimelineMetrics {
  const intervalMs =
    job.schedule.kind === "every" && job.schedule.everyMs && job.schedule.everyMs > 0
      ? job.schedule.everyMs
      : undefined;
  const lastRunAtMs = job.state?.lastRunAtMs ?? null;
  const nextRunAtMs = job.state?.nextRunAtMs ?? null;

  let windowStartMs =
    lastRunAtMs ??
    (nextRunAtMs != null && intervalMs ? nextRunAtMs - intervalMs : now - (intervalMs ?? DEFAULT_CRON_TIMELINE_WINDOW_MS));
  let windowEndMs =
    nextRunAtMs ??
    (lastRunAtMs != null && intervalMs ? lastRunAtMs + intervalMs : now + (intervalMs ?? DEFAULT_CRON_TIMELINE_WINDOW_MS));

  if (windowEndMs <= windowStartMs) {
    const fallbackWindow = intervalMs ?? DEFAULT_CRON_TIMELINE_WINDOW_MS;
    windowStartMs = now - fallbackWindow / 2;
    windowEndMs = now + fallbackWindow / 2;
  }

  const windowSize = Math.max(windowEndMs - windowStartMs, 1);
  const toPercent = (value: number) => clampPercent(((value - windowStartMs) / windowSize) * 100);

  return {
    windowStartMs,
    windowEndMs,
    lastRunAtMs,
    nextRunAtMs,
    nowPercent: toPercent(now),
    lastPercent: lastRunAtMs != null ? toPercent(lastRunAtMs) : null,
    nextPercent: nextRunAtMs != null ? toPercent(nextRunAtMs) : null,
    overdue: nextRunAtMs != null && nextRunAtMs < now,
    intervalLabel: describeCronSchedule(job.schedule, localeTag),
  };
}

// ── Cron Timeline Widget ──

interface CronTimelineWidgetProps {
  t: TFunction;
  localeTag: string;
}

export function CronTimelineWidget({ t, localeTag }: CronTimelineWidgetProps) {
  const [jobs, setJobs] = useState<api.CronJobGlobal[]>([]);
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    let mounted = true;
    const load = async () => {
      try {
        const nextJobs = await api.getCronJobs();
        if (mounted) setJobs(nextJobs);
      } catch {
        // Ignore transient cron fetch failures in the dashboard.
      } finally {
        if (mounted) setNow(Date.now());
      }
    };

    void load();
    const timer = setInterval(() => void load(), 60_000);
    return () => {
      mounted = false;
      clearInterval(timer);
    };
  }, []);

  const enabledJobs = useMemo(() => jobs.filter((j) => j.enabled), [jobs]);

  if (enabledJobs.length === 0) return null;

  return (
    <div
      className={dashboardCard.standard}
      style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
    >
      <div className="flex items-center justify-between mb-3">
        <h3 className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
          {t({ ko: "크론잡 타임라인", en: "Cron Timeline", ja: "クロンタイムライン", zh: "定时任务时间线" })}
        </h3>
        <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
          <span className={dashboardBadge.default} style={{ background: "var(--th-overlay-medium)" }}>
            {enabledJobs.length} {t({ ko: "활성", en: "active", ja: "アクティブ", zh: "活跃" })}
          </span>
        </span>
      </div>
      <div className="space-y-2 max-h-72 overflow-y-auto">
        {enabledJobs
          .sort((a, b) => {
            const aNext = a.state?.nextRunAtMs ?? Infinity;
            const bNext = b.state?.nextRunAtMs ?? Infinity;
            return aNext - bNext;
          })
          .map((job) => {
            const lastRun = job.state?.lastRunAtMs ?? null;
            const nextRun = job.state?.nextRunAtMs ?? null;
            const metrics = buildCronTimelineMetrics(job, now, localeTag);
            const isOk = job.state?.lastStatus === "ok";
            const accent = metrics.overdue ? "#fb7185" : isOk ? "#34d399" : "#f59e0b";
            const stateLabel = metrics.overdue
              ? t({ ko: "지연", en: "Overdue", ja: "遅延", zh: "延迟" })
              : isOk
                ? t({ ko: "정상", en: "Healthy", ja: "正常", zh: "正常" })
                : t({ ko: "확인 필요", en: "Needs check", ja: "要確認", zh: "需检查" });
            const formatClock = (value: number | null) =>
              value == null
                ? "—"
                : new Date(value).toLocaleTimeString(localeTag, {
                    hour: "2-digit",
                    minute: "2-digit",
                  });

            return (
              <div
                key={job.id}
                className={cx(dashboardCard.nestedCompact, "flex items-center gap-2")}
                style={{
                  background: "var(--th-bg-surface)",
                  borderColor: `color-mix(in srgb, ${accent} 20%, transparent)`,
                }}
              >
                <div className="flex items-start justify-between gap-3">
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-2">
                      <span
                        className="h-2.5 w-2.5 rounded-full shrink-0"
                        style={{ background: accent }}
                      />
                      <div className="text-sm font-medium truncate" style={{ color: "var(--th-text)" }}>
                        {job.description_ko || job.name}
                      </div>
                    </div>
                    <div className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                      {job.agentId && <span>{job.agentId}</span>}
                      <span>{metrics.intervalLabel}</span>
                      <span style={{ color: accent }}>{stateLabel}</span>
                    </div>
                  </div>
                  {nextRun != null && (
                    <span
                      className="text-[11px] px-2 py-1 rounded-full shrink-0"
                      style={{
                        background: `color-mix(in srgb, ${accent} 12%, transparent)`,
                        color: accent,
                      }}
                    >
                      {metrics.overdue
                        ? t({ ko: "예정 지남", en: "Past due", ja: "期限超過", zh: "已逾期" })
                        : `${t({ ko: "다음", en: "Next", ja: "次回", zh: "下次" })} ${formatClock(nextRun)}`}
                    </span>
                  )}
                </div>

                <div className="mt-3">
                  <div className="relative h-10">
                    <div
                      className="absolute inset-x-0 top-1/2 h-1 -translate-y-1/2 rounded-full"
                      style={{ background: "rgba(148,163,184,0.16)" }}
                    />
                    <div
                      className="absolute left-0 top-1/2 h-1 -translate-y-1/2 rounded-full"
                      style={{
                        width: `${metrics.nowPercent}%`,
                        background: accent,
                        opacity: 0.28,
                      }}
                    />
                    {metrics.lastPercent != null && (
                      <div
                        className="absolute top-1/2 -translate-y-1/2"
                        style={{ left: `calc(${metrics.lastPercent}% - 6px)` }}
                      >
                        <span className="block h-3 w-3 rounded-full border-2" style={{ borderColor: accent, background: "var(--th-surface)" }} />
                      </div>
                    )}
                    <div
                      className="absolute top-1/2 -translate-y-1/2"
                      style={{ left: `calc(${metrics.nowPercent}% - 1px)` }}
                    >
                      <span className="block h-4 w-[2px] rounded-full" style={{ background: "#f8fafc" }} />
                    </div>
                    {metrics.nextPercent != null && (
                      <div
                        className="absolute top-1/2 -translate-y-1/2"
                        style={{ left: `calc(${metrics.nextPercent}% - 5px)` }}
                      >
                        <span className="block h-[10px] w-[10px] rotate-45 rounded-[2px]" style={{ background: accent }} />
                      </div>
                    )}
                  </div>
                </div>

                <div className="mt-1 grid grid-cols-3 gap-2 text-[10px]" style={{ color: "var(--th-text-muted)" }}>
                  <span className="truncate">
                    {t({ ko: "최근", en: "Last", ja: "前回", zh: "上次" })} {formatClock(lastRun)}
                  </span>
                  <span className="text-center">
                    {t({ ko: "지금", en: "Now", ja: "現在", zh: "现在" })} {formatClock(now)}
                  </span>
                  <span className="truncate text-right">
                    {t({ ko: "다음", en: "Next", ja: "次回", zh: "下次" })} {formatClock(nextRun)}
                  </span>
                </div>
              </div>
            );
          })}
      </div>
    </div>
  );
}
