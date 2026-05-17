import { useEffect, useMemo, useState } from "react";
import { CalendarClock, RefreshCw } from "lucide-react";
import {
  getRoutines,
  type RoutineRecord,
  type RoutineStatus,
} from "../../api";
import {
  SurfaceActionButton,
  SurfaceEmptyState,
  SurfaceListItem,
  SurfaceMetaBadge,
  SurfaceMetricPill,
  SurfaceSegmentButton,
  SurfaceSubsection,
} from "../common/SurfacePrimitives";
import type { TFunction } from "./model";
import { cx } from "./ui";

type RoutineFilter = "all" | RoutineStatus;

const FILTERS: RoutineFilter[] = ["all", "enabled", "paused", "detached"];

function parseTime(value: string | null | undefined): number | null {
  if (!value) return null;
  const time = Date.parse(value);
  return Number.isFinite(time) ? time : null;
}

function compareMaybeTime(left: number | null, right: number | null): number {
  if (left == null && right == null) return 0;
  if (left == null) return 1;
  if (right == null) return -1;
  return left - right;
}

export function sortRoutinesChronologically(
  routines: RoutineRecord[],
): RoutineRecord[] {
  return [...routines].sort((left, right) => {
    const dueCompare = compareMaybeTime(
      parseTime(left.next_due_at),
      parseTime(right.next_due_at),
    );
    if (dueCompare !== 0) return dueCompare;

    const lastRunCompare = compareMaybeTime(
      parseTime(right.last_run_at),
      parseTime(left.last_run_at),
    );
    if (lastRunCompare !== 0) return lastRunCompare;

    return left.name.localeCompare(right.name);
  });
}

function pad2(value: number): string {
  return value.toString().padStart(2, "0");
}

export function describeRoutineSchedule(
  schedule: string | null,
  language: "ko" | "en" | "ja" | "zh",
): string {
  const trimmed = schedule?.trim();
  if (!trimmed) {
    return language === "ko" ? "수동 실행" : "Manual run";
  }

  const every = trimmed.match(/^@every\s+(\d+)(ms|s|m|h|d)$/i);
  if (every) {
    const value = Number(every[1]);
    const unit = every[2].toLowerCase();
    const label =
      unit === "d"
        ? language === "ko"
          ? "일"
          : "d"
        : unit === "h"
          ? language === "ko"
            ? "시간"
            : "h"
          : unit === "m"
            ? language === "ko"
              ? "분"
              : "m"
            : unit;
    return language === "ko" ? `${value}${label}마다` : `Every ${value}${label}`;
  }

  const parts = trimmed.split(/\s+/);
  if (parts.length === 5) {
    const [minute, hour, dayOfMonth, month, dayOfWeek] = parts;
    const hourNum = Number(hour);
    const minuteNum = Number(minute);
    if (
      Number.isInteger(hourNum) &&
      Number.isInteger(minuteNum) &&
      hourNum >= 0 &&
      hourNum <= 23 &&
      minuteNum >= 0 &&
      minuteNum <= 59 &&
      dayOfMonth === "*" &&
      month === "*"
    ) {
      const clock = `${pad2(hourNum)}:${pad2(minuteNum)}`;
      if (dayOfWeek === "*") {
        return language === "ko" ? `매일 ${clock}` : `Daily ${clock}`;
      }
      if (dayOfWeek === "1-5") {
        return language === "ko" ? `평일 ${clock}` : `Weekdays ${clock}`;
      }
    }
  }

  return trimmed;
}

function statusTone(routine: RoutineRecord): "info" | "success" | "warn" | "neutral" | "danger" {
  if (routine.in_flight_run_id) return "info";
  if (routine.status === "enabled") return "success";
  if (routine.status === "paused") return "warn";
  if (routine.status === "detached") return "neutral";
  return "danger";
}

function statusLabel(routine: RoutineRecord, t: TFunction): string {
  if (routine.in_flight_run_id) {
    return t({ ko: "진행 중", en: "Running", ja: "実行中", zh: "运行中" });
  }
  if (routine.status === "enabled") {
    return t({ ko: "활성", en: "Active", ja: "有効", zh: "活跃" });
  }
  if (routine.status === "paused") {
    return t({ ko: "일시정지", en: "Paused", ja: "一時停止", zh: "已暂停" });
  }
  if (routine.status === "detached") {
    return t({ ko: "분리됨", en: "Detached", ja: "切り離し", zh: "已分离" });
  }
  return routine.status;
}

function formatDateTime(value: string | null, localeTag: string): string {
  const time = parseTime(value);
  if (time == null) return "-";
  return new Date(time).toLocaleString(localeTag, {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function formatRelative(value: string | null, localeTag: string): string | null {
  const time = parseTime(value);
  if (time == null) return null;
  const diffMs = time - Date.now();
  const absMs = Math.abs(diffMs);
  const formatter = new Intl.RelativeTimeFormat(localeTag, { numeric: "auto" });
  if (absMs < 60_000) return formatter.format(Math.round(diffMs / 1_000), "second");
  if (absMs < 3_600_000) return formatter.format(Math.round(diffMs / 60_000), "minute");
  if (absMs < 86_400_000) return formatter.format(Math.round(diffMs / 3_600_000), "hour");
  return formatter.format(Math.round(diffMs / 86_400_000), "day");
}

function filterLabel(filter: RoutineFilter, t: TFunction): string {
  switch (filter) {
    case "enabled":
      return t({ ko: "활성", en: "Active", ja: "有効", zh: "活跃" });
    case "paused":
      return t({ ko: "일시정지", en: "Paused", ja: "一時停止", zh: "已暂停" });
    case "detached":
      return t({ ko: "분리", en: "Detached", ja: "切り離し", zh: "分离" });
    case "all":
    default:
      return t({ ko: "전체", en: "All", ja: "すべて", zh: "全部" });
  }
}

interface RoutinesTimelineWidgetProps {
  t: TFunction;
  localeTag: string;
  language: "ko" | "en" | "ja" | "zh";
}

export function RoutinesTimelineWidget({
  t,
  localeTag,
  language,
}: RoutinesTimelineWidgetProps) {
  const [filter, setFilter] = useState<RoutineFilter>("all");
  const [routines, setRoutines] = useState<RoutineRecord[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState(false);
  const [reloadKey, setReloadKey] = useState(0);

  useEffect(() => {
    let mounted = true;
    const load = async () => {
      setError(false);
      setLoading(true);
      try {
        const next = await getRoutines(
          filter === "all" ? undefined : { status: filter },
        );
        if (!mounted) return;
        setRoutines(next);
      } catch {
        if (mounted) setError(true);
      } finally {
        if (mounted) setLoading(false);
      }
    };

    void load();
    const timer = window.setInterval(() => void load(), 60_000);
    return () => {
      mounted = false;
      window.clearInterval(timer);
    };
  }, [filter, reloadKey]);

  const sortedRoutines = useMemo(
    () => sortRoutinesChronologically(routines),
    [routines],
  );
  const activeCount = useMemo(
    () => routines.filter((routine) => routine.status === "enabled").length,
    [routines],
  );
  const runningCount = useMemo(
    () => routines.filter((routine) => Boolean(routine.in_flight_run_id)).length,
    [routines],
  );
  const nextRoutine = sortedRoutines.find((routine) => routine.next_due_at);

  return (
    <SurfaceSubsection
      data-testid="routines-timeline"
      title={t({ ko: "루틴 시간표", en: "Routines Timeline", ja: "ルーチン時系列", zh: "例程时间线" })}
      description={t({
        ko: "등록된 루틴을 다음 실행 시간 기준으로 정렬해 보여줍니다.",
        en: "Registered routines are sorted by their next run time.",
        ja: "登録済みルーチンを次回実行時刻順に表示します。",
        zh: "按下一次运行时间排序显示已注册例程。",
      })}
      actions={(
        <SurfaceActionButton
          compact
          tone="neutral"
          onClick={() => setReloadKey((value) => value + 1)}
          aria-label={t({ ko: "루틴 새로고침", en: "Refresh routines", ja: "ルーチンを再読み込み", zh: "刷新例程" })}
        >
          <RefreshCw size={12} className={cx(loading ? "animate-spin" : "")} />
        </SurfaceActionButton>
      )}
      style={{
        borderColor: "color-mix(in srgb, var(--th-accent-info) 24%, var(--th-border) 76%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-info) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 97%, transparent) 100%)",
      }}
    >
      <div className="grid gap-3 sm:grid-cols-3">
        <SurfaceMetricPill
          label={t({ ko: "등록", en: "Registered", ja: "登録", zh: "已注册" })}
          value={routines.length}
          tone="info"
          className="w-full"
        />
        <SurfaceMetricPill
          label={t({ ko: "활성", en: "Active", ja: "有効", zh: "活跃" })}
          value={activeCount}
          tone="success"
          className="w-full"
        />
        <SurfaceMetricPill
          label={t({ ko: "다음", en: "Next", ja: "次回", zh: "下次" })}
          value={nextRoutine ? formatDateTime(nextRoutine.next_due_at, localeTag) : "-"}
          tone={runningCount > 0 ? "warn" : "neutral"}
          className="w-full"
        />
      </div>

      <div
        className="mt-4 flex gap-2 overflow-x-auto pb-1"
        data-testid="routines-filter-controls"
      >
        {FILTERS.map((item) => (
          <SurfaceSegmentButton
            key={item}
            active={filter === item}
            onClick={() => setFilter(item)}
            aria-pressed={filter === item}
          >
            {filterLabel(item, t)}
          </SurfaceSegmentButton>
        ))}
      </div>

      {error ? (
        <SurfaceEmptyState className="mt-4 px-4 py-6 text-center text-sm">
          {t({
            ko: "루틴 목록을 불러오지 못했습니다.",
            en: "Routines could not be loaded.",
            ja: "ルーチン一覧を読み込めませんでした。",
            zh: "无法加载例程列表。",
          })}
        </SurfaceEmptyState>
      ) : loading && routines.length === 0 ? (
        <div className="mt-4 space-y-2" data-testid="routines-loading">
          {Array.from({ length: 3 }).map((_, index) => (
            <div
              key={index}
              className="h-20 animate-pulse rounded-2xl border"
              style={{
                borderColor: "color-mix(in srgb, var(--th-border) 62%, transparent)",
                background: "color-mix(in srgb, var(--th-card-bg) 86%, transparent)",
              }}
            />
          ))}
        </div>
      ) : sortedRoutines.length === 0 ? (
        <SurfaceEmptyState className="mt-4 px-4 py-8 text-center text-sm">
          {t({
            ko: "표시할 루틴이 없습니다.",
            en: "No routines to show.",
            ja: "表示するルーチンがありません。",
            zh: "没有可显示的例程。",
          })}
        </SurfaceEmptyState>
      ) : (
        <div className="mt-4 space-y-2" data-testid="routines-timeline-list">
          {sortedRoutines.map((routine) => {
            const relative = formatRelative(routine.next_due_at, localeTag);
            const lastRunLabel = formatDateTime(routine.last_run_at, localeTag);
            return (
              <div key={routine.id} data-testid={`routine-row-${routine.id}`}>
                <SurfaceListItem
                  tone={statusTone(routine)}
                  className="min-w-0"
                  trailing={(
                    <div className="flex min-w-[7.5rem] flex-col items-end gap-1 text-right">
                      <div
                        className="text-sm font-semibold tabular-nums"
                        style={{ color: "var(--th-text-heading)" }}
                      >
                        {formatDateTime(routine.next_due_at, localeTag)}
                      </div>
                      <div className="max-w-[9rem] truncate text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                        {relative ?? t({ ko: "수동", en: "Manual", ja: "手動", zh: "手动" })}
                      </div>
                    </div>
                  )}
                >
                  <div className="min-w-0">
                    <div className="flex flex-wrap items-center gap-2">
                      <CalendarClock size={14} style={{ color: "var(--th-accent-info)" }} />
                      <div
                        className="min-w-0 max-w-full truncate text-sm font-semibold"
                        style={{ color: "var(--th-text-heading)" }}
                      >
                        {routine.name}
                      </div>
                      <SurfaceMetaBadge tone={statusTone(routine)}>
                        {statusLabel(routine, t)}
                      </SurfaceMetaBadge>
                    </div>
                    <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
                      <span className="min-w-0 max-w-full truncate">
                        {describeRoutineSchedule(routine.schedule, language)}
                      </span>
                      {routine.agent_id ? (
                        <span className="min-w-0 max-w-full truncate">
                          {routine.agent_id}
                        </span>
                      ) : null}
                      <span className="min-w-0 max-w-full truncate">
                        {t({ ko: "최근", en: "Last", ja: "前回", zh: "上次" })} {lastRunLabel}
                      </span>
                    </div>
                  </div>
                </SurfaceListItem>
              </div>
            );
          })}
        </div>
      )}
    </SurfaceSubsection>
  );
}
