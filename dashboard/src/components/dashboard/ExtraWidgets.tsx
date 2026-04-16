import { useEffect, useState, useMemo } from "react";
import type { Agent } from "../../types";
import * as api from "../../api/client";
import type { TFunction } from "./model";
import AgentAvatar from "../AgentAvatar";
import { cx, dashboardBadge, dashboardCard } from "./ui";
import {
  DEFAULT_BOTTLENECK_THRESHOLDS,
  LONG_BLOCKED_DAYS,
  REVIEW_DELAY_DAYS,
  REWORK_ALERT_THRESHOLD,
  buildBottleneckGroups,
  type BottleneckThresholds,
  type BottleneckRow,
} from "./dashboardInsights";

const DEFAULT_CRON_TIMELINE_WINDOW_MS = 60 * 60_000;
const BOTTLE_NECK_THRESHOLDS_STORAGE_KEY = "agentdesk:dashboard:bottleneck-thresholds";
const AUTO_QUEUE_HISTORY_LIMIT = 24;
const AUTO_QUEUE_HISTORY_PREVIEW_COUNT = 8;

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

function formatRelativeAge(days: number, t: TFunction): string {
  if (days <= 0) return t({ ko: "오늘", en: "today", ja: "今日", zh: "今天" });
  return t({
    ko: `${days}일`,
    en: `${days}d`,
    ja: `${days}日`,
    zh: `${days}天`,
  });
}

function formatDurationShort(ms: number): string {
  const totalMinutes = Math.max(0, Math.round(ms / 60_000));
  const hours = Math.floor(totalMinutes / 60);
  const minutes = totalMinutes % 60;
  if (hours > 0) return `${hours}h ${minutes}m`;
  return `${minutes}m`;
}

function formatPercent(value: number): string {
  return `${Math.round(value * 100)}%`;
}

function sanitizeThreshold(value: number, fallback: number, min = 1, max = 30): number {
  if (!Number.isFinite(value)) return fallback;
  return Math.min(max, Math.max(min, Math.round(value)));
}

function readStoredBottleneckThresholds(): BottleneckThresholds {
  if (typeof window === "undefined") return DEFAULT_BOTTLENECK_THRESHOLDS;
  try {
    const raw = window.localStorage.getItem(BOTTLE_NECK_THRESHOLDS_STORAGE_KEY);
    if (!raw) return DEFAULT_BOTTLENECK_THRESHOLDS;
    const parsed = JSON.parse(raw) as Partial<BottleneckThresholds>;
    return {
      review_delay_days: sanitizeThreshold(parsed.review_delay_days ?? NaN, REVIEW_DELAY_DAYS),
      long_blocked_days: sanitizeThreshold(parsed.long_blocked_days ?? NaN, LONG_BLOCKED_DAYS),
      rework_alert_threshold: sanitizeThreshold(parsed.rework_alert_threshold ?? NaN, REWORK_ALERT_THRESHOLD, 1, 20),
    };
  } catch {
    return DEFAULT_BOTTLENECK_THRESHOLDS;
  }
}

function persistBottleneckThresholds(thresholds: BottleneckThresholds) {
  if (typeof window === "undefined") return;
  try {
    window.localStorage.setItem(BOTTLE_NECK_THRESHOLDS_STORAGE_KEY, JSON.stringify(thresholds));
  } catch {
    // Ignore localStorage failures and keep the current in-memory values.
  }
}

function buildWeightedSuccessRate(runs: api.AutoQueueHistoryRun[]): number {
  const totalEntries = runs.reduce((sum, run) => sum + Math.max(run.entry_count, 0), 0);
  if (totalEntries <= 0) return 0;
  const successfulEntries = runs.reduce(
    (sum, run) => sum + Math.max(run.entry_count, 0) * run.success_rate,
    0,
  );
  return successfulEntries / totalEntries;
}

// ── Bottleneck Widget ──

interface BottleneckWidgetProps {
  t: TFunction;
}

export function BottleneckWidget({ t }: BottleneckWidgetProps) {
  const [cards, setCards] = useState<api.KanbanCard[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [showThresholdControls, setShowThresholdControls] = useState(false);
  const [thresholds, setThresholds] = useState<BottleneckThresholds>(() => readStoredBottleneckThresholds());

  useEffect(() => {
    persistBottleneckThresholds(thresholds);
  }, [thresholds]);

  useEffect(() => {
    let mounted = true;

    const load = async () => {
      if (mounted) setLoading(true);
      try {
        const next = await api.getKanbanCards();
        if (!mounted) return;
        setCards(next);
        setError(null);
      } catch (nextError) {
        if (!mounted) return;
        setError(nextError instanceof Error ? nextError.message : String(nextError));
      } finally {
        if (mounted) setLoading(false);
      }
    };

    void load();
    const timer = setInterval(() => void load(), 60_000);
    return () => {
      mounted = false;
      clearInterval(timer);
    };
  }, []);

  const groups = useMemo(() => buildBottleneckGroups(cards, Date.now(), thresholds), [cards, thresholds]);
  const totalAlerts = useMemo(() => {
    const ids = new Set<string>();
    for (const row of groups.review_delay) ids.add(row.id);
    for (const row of groups.repeat_rework) ids.add(row.id);
    for (const row of groups.long_blocked) ids.add(row.id);
    return ids.size;
  }, [groups]);

  const updateThreshold = (
    key: keyof BottleneckThresholds,
    nextValue: number,
    fallback: number,
    min = 1,
    max = 30,
  ) => {
    setThresholds((current) => ({
      ...current,
      [key]: sanitizeThreshold(nextValue, fallback, min, max),
    }));
  };

  return (
    <div
      className="rounded-2xl border p-4 sm:p-5"
      style={{
        borderColor: "var(--th-border)",
        background: "linear-gradient(145deg, color-mix(in srgb, var(--th-surface) 91%, #ef4444 9%), var(--th-surface))",
      }}
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="min-w-0">
          <h3 className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {t({ ko: "병목 감지", en: "Bottleneck Detection", ja: "ボトルネック検知", zh: "瓶颈检测" })}
          </h3>
          <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "리뷰 지연, 반복 리워크, 장기 블로킹 카드를 바로 추려냅니다",
              en: "Pull delayed reviews, repeated reworks, and long blocks into one action board",
              ja: "レビュー遅延、反復リワーク、長期ブロックを一つのアクションボードに集約します",
              zh: "将审查延迟、反复返工、长期阻塞集中到一个动作面板",
            })}
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            className="rounded-full px-3 py-1 text-[11px] font-semibold"
            style={{
              color: "var(--th-text)",
              background: "rgba(148,163,184,0.14)",
              border: "1px solid rgba(148,163,184,0.2)",
            }}
            onClick={() => setShowThresholdControls((current) => !current)}
          >
            {showThresholdControls
              ? t({ ko: "기준 닫기", en: "Hide thresholds", ja: "基準を閉じる", zh: "收起阈值" })
              : t({ ko: "기준 조정", en: "Tune thresholds", ja: "基準調整", zh: "调整阈值" })}
          </button>
          <span
            className="rounded-full px-3 py-1 text-xs font-semibold"
            style={{ color: "#fca5a5", background: "rgba(239,68,68,0.14)" }}
          >
            {totalAlerts} {t({ ko: "경고", en: "alerts", ja: "警告", zh: "警报" })}
          </span>
        </div>
      </div>

      {showThresholdControls ? (
        <div className="mt-4 grid gap-3 rounded-2xl border p-3 text-[11px] sm:grid-cols-3" style={{ borderColor: "rgba(255,255,255,0.08)", background: "rgba(15,23,42,0.18)" }}>
          <label className="flex min-w-0 flex-col gap-1">
            <span style={{ color: "var(--th-text-muted)" }}>
              {t({ ko: "리뷰 지연 일수", en: "Review delay days", ja: "レビュー遅延日数", zh: "审查延迟天数" })}
            </span>
            <input
              type="number"
              min={1}
              max={30}
              value={thresholds.review_delay_days}
              onChange={(event) => updateThreshold("review_delay_days", Number(event.target.value), REVIEW_DELAY_DAYS)}
              className="rounded-xl border px-3 py-2 text-sm"
              style={{ borderColor: "rgba(255,255,255,0.1)", background: "var(--th-bg-surface)", color: "var(--th-text)" }}
            />
          </label>
          <label className="flex min-w-0 flex-col gap-1">
            <span style={{ color: "var(--th-text-muted)" }}>
              {t({ ko: "장기 블록 일수", en: "Blocked days", ja: "長期ブロック日数", zh: "长期阻塞天数" })}
            </span>
            <input
              type="number"
              min={1}
              max={30}
              value={thresholds.long_blocked_days}
              onChange={(event) => updateThreshold("long_blocked_days", Number(event.target.value), LONG_BLOCKED_DAYS)}
              className="rounded-xl border px-3 py-2 text-sm"
              style={{ borderColor: "rgba(255,255,255,0.1)", background: "var(--th-bg-surface)", color: "var(--th-text)" }}
            />
          </label>
          <label className="flex min-w-0 flex-col gap-1">
            <span style={{ color: "var(--th-text-muted)" }}>
              {t({ ko: "리워크 경고 횟수", en: "Rework threshold", ja: "リワーク閾値", zh: "返工阈值" })}
            </span>
            <input
              type="number"
              min={1}
              max={20}
              value={thresholds.rework_alert_threshold}
              onChange={(event) => updateThreshold("rework_alert_threshold", Number(event.target.value), REWORK_ALERT_THRESHOLD, 1, 20)}
              className="rounded-xl border px-3 py-2 text-sm"
              style={{ borderColor: "rgba(255,255,255,0.1)", background: "var(--th-bg-surface)", color: "var(--th-text)" }}
            />
          </label>
        </div>
      ) : null}

      {error ? (
        <div className="mt-4 rounded-2xl border px-3 py-2 text-xs" style={{ borderColor: "rgba(251,191,36,0.28)", background: "rgba(251,191,36,0.12)", color: "#fde68a" }}>
          {cards.length > 0
            ? t({
                ko: `최근 카드 스냅샷을 유지 중이며 새 동기화에 실패했습니다. (${error})`,
                en: `Keeping the last card snapshot because refresh failed. (${error})`,
                ja: `最新同期に失敗したため、直近のカードスナップショットを維持しています。(${error})`,
                zh: `最新同步失败，正在保留最近一次卡片快照。(${error})`,
              })
            : t({
                ko: `칸반 카드를 불러오지 못했습니다. (${error})`,
                en: `Unable to load kanban cards. (${error})`,
                ja: `kanban カードを読み込めませんでした。(${error})`,
                zh: `无法加载 kanban 卡片。(${error})`,
              })}
        </div>
      ) : null}

      {loading && totalAlerts === 0 ? (
        <div className="py-10 text-center text-sm" style={{ color: "var(--th-text-muted)" }}>
          {t({ ko: "운영 병목을 확인하는 중입니다", en: "Scanning bottlenecks", ja: "ボトルネックを確認中", zh: "正在扫描瓶颈" })}
        </div>
      ) : (
        <div className="mt-4 grid grid-cols-1 gap-4 xl:grid-cols-3">
          <BottleneckColumn
            title={t({ ko: "리뷰 지연", en: "Review Delay", ja: "レビュー遅延", zh: "审查延迟" })}
            hint={t({
              ko: `${thresholds.review_delay_days}일 이상 review`,
              en: `${thresholds.review_delay_days}+ days in review`,
              ja: `${thresholds.review_delay_days}日以上 review`,
              zh: `review 超过 ${thresholds.review_delay_days} 天`,
            })}
            rows={groups.review_delay}
            emptyLabel={t({ ko: "지연된 review 카드가 없습니다", en: "No delayed review cards", ja: "遅延レビューカードはありません", zh: "暂无延迟审查卡片" })}
            accent="#f59e0b"
            t={t}
          />
          <BottleneckColumn
            title={t({ ko: "반복 리워크", en: "Repeat Rework", ja: "反復リワーク", zh: "重复返工" })}
            hint={t({
              ko: `오늘 완료, ${thresholds.rework_alert_threshold}회 이상 rework`,
              en: `Closed today, ${thresholds.rework_alert_threshold}+ reworks`,
              ja: `本日完了、${thresholds.rework_alert_threshold}回以上リワーク`,
              zh: `今日完成、${thresholds.rework_alert_threshold} 次以上返工`,
            })}
            rows={groups.repeat_rework}
            emptyLabel={t({ ko: "오늘 완료된 반복 리워크 카드가 없습니다", en: "No repeat rework cards closed today", ja: "本日完了の反復リワークカードはありません", zh: "今日暂无完成的重复返工卡片" })}
            accent="#a78bfa"
            t={t}
          />
          <BottleneckColumn
            title={t({ ko: "장기 블로킹", en: "Long Blocked", ja: "長期ブロック", zh: "长期阻塞" })}
            hint={t({
              ko: `${thresholds.long_blocked_days}일 이상 blocked`,
              en: `${thresholds.long_blocked_days}+ days blocked`,
              ja: `${thresholds.long_blocked_days}日以上 blocked`,
              zh: `blocked 超过 ${thresholds.long_blocked_days} 天`,
            })}
            rows={groups.long_blocked}
            emptyLabel={t({ ko: "장기 블로킹 카드는 없습니다", en: "No long blocked cards", ja: "長期ブロックカードはありません", zh: "暂无长期阻塞卡片" })}
            accent="#f87171"
            t={t}
          />
        </div>
      )}
    </div>
  );
}

function BottleneckColumn({
  title,
  hint,
  rows,
  emptyLabel,
  accent,
  t,
}: {
  title: string;
  hint: string;
  rows: BottleneckRow[];
  emptyLabel: string;
  accent: string;
  t: TFunction;
}) {
  const [expanded, setExpanded] = useState(false);
  const visibleRows = expanded ? rows : rows.slice(0, 4);
  const hiddenCount = Math.max(0, rows.length - visibleRows.length);

  return (
    <div
      className="rounded-2xl border p-3"
      style={{ borderColor: `${accent}33`, background: "rgba(15,23,42,0.18)" }}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {title}
          </div>
          <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
            {hint}
          </div>
        </div>
        <span
          className="rounded-full px-2 py-1 text-[11px] font-semibold"
          style={{ color: accent, background: `${accent}1f` }}
        >
          {rows.length}
        </span>
      </div>

      {rows.length === 0 ? (
        <div className="py-8 text-center text-xs" style={{ color: "var(--th-text-muted)" }}>
          {emptyLabel}
        </div>
      ) : (
        <div className="mt-3 space-y-2">
          {visibleRows.map((row) => (
            <div
              key={row.id}
              className="rounded-xl border px-3 py-2"
              style={{ borderColor: "rgba(255,255,255,0.06)", background: "var(--th-bg-surface)" }}
            >
              <div className="flex items-start justify-between gap-2">
                <div className="min-w-0">
                  <div className="truncate text-sm font-medium" style={{ color: "var(--th-text)" }}>
                    {row.title}
                  </div>
                  <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                    {row.repo || "global"}
                    {row.github_issue_number ? ` · #${row.github_issue_number}` : ""}
                  </div>
                </div>
                <span className="text-[11px] shrink-0" style={{ color: accent }}>
                  {formatRelativeAge(row.age_days, t)}
                </span>
              </div>
              {row.rework_count > 0 && (
                <div className="mt-2 text-[11px]" style={{ color: accent }}>
                  {t({ ko: "리워크", en: "Rework", ja: "リワーク", zh: "返工" })} {row.rework_count}
                </div>
              )}
            </div>
          ))}
          {hiddenCount > 0 ? (
            <button
              type="button"
              className="w-full rounded-xl border px-3 py-2 text-xs font-medium"
              style={{ borderColor: `${accent}33`, color: accent, background: "transparent" }}
              onClick={() => setExpanded(true)}
            >
              {t({
                ko: `${hiddenCount}건 더 보기`,
                en: `Show ${hiddenCount} more`,
                ja: `${hiddenCount}件をさらに表示`,
                zh: `再显示 ${hiddenCount} 条`,
              })}
            </button>
          ) : rows.length > 4 ? (
            <button
              type="button"
              className="w-full rounded-xl border px-3 py-2 text-xs font-medium"
              style={{ borderColor: `${accent}33`, color: "var(--th-text-muted)", background: "transparent" }}
              onClick={() => setExpanded(false)}
            >
              {t({
                ko: "접기",
                en: "Collapse",
                ja: "折りたたむ",
                zh: "收起",
              })}
            </button>
          ) : null}
        </div>
      )}
    </div>
  );
}

// ── Auto-Queue History Widget ──

interface AutoQueueHistoryWidgetProps {
  t: TFunction;
}

export function AutoQueueHistoryWidget({ t }: AutoQueueHistoryWidgetProps) {
  const [data, setData] = useState<api.AutoQueueHistoryResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [expanded, setExpanded] = useState(false);

  useEffect(() => {
    let mounted = true;

    const load = async () => {
      if (mounted) setLoading(true);
      try {
        const next = await api.getAutoQueueHistory(AUTO_QUEUE_HISTORY_LIMIT);
        if (!mounted) return;
        setData(next);
        setError(null);
      } catch (nextError) {
        if (!mounted) return;
        setError(nextError instanceof Error ? nextError.message : String(nextError));
      } finally {
        if (mounted) setLoading(false);
      }
    };

    void load();
    const timer = setInterval(() => void load(), 60_000);
    return () => {
      mounted = false;
      clearInterval(timer);
    };
  }, []);

  const runs = data?.runs ?? [];
  const visibleRuns = expanded ? runs : runs.slice(0, AUTO_QUEUE_HISTORY_PREVIEW_COUNT);
  const weightedSuccessRate = useMemo(() => buildWeightedSuccessRate(runs), [runs]);
  const hiddenRuns = Math.max(0, runs.length - visibleRuns.length);

  return (
    <div
      className="rounded-2xl border p-4"
      style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="min-w-0">
          <h3 className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {t({ ko: "자동큐 실행 이력", en: "Auto-Queue History", ja: "自動キュー履歴", zh: "自动队列历史" })}
          </h3>
          <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "최근 런의 성공률, 소요시간, 엔트리 규모를 한눈에 봅니다",
              en: "Track recent run success rates, durations, and entry volume at a glance",
              ja: "最近のランの成功率、所要時間、エントリ規模を一目で確認します",
              zh: "一眼查看最近运行的成功率、耗时和条目规模",
            })}
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2 text-[11px]">
          <span className="rounded-full px-2 py-1" style={{ color: "#86efac", background: "rgba(34,197,94,0.12)" }}>
            {(data?.summary.completed_runs ?? 0)}/{data?.summary.total_runs ?? 0} {t({ ko: "완료", en: "completed", ja: "完了", zh: "完成" })}
          </span>
          <span className="rounded-full px-2 py-1" style={{ color: "#38bdf8", background: "rgba(56,189,248,0.12)" }}>
            {formatPercent(weightedSuccessRate)} {t({ ko: "성공", en: "success", ja: "成功", zh: "成功" })}
          </span>
          {runs.length > 0 ? (
            <span className="rounded-full px-2 py-1" style={{ color: "var(--th-text-muted)", background: "rgba(148,163,184,0.12)" }}>
              {t({
                ko: `최근 ${runs.length}건 기준`,
                en: `Based on ${runs.length} recent runs`,
                ja: `直近 ${runs.length} 件 기준`,
                zh: `基于最近 ${runs.length} 次运行`,
              })}
            </span>
          ) : null}
        </div>
      </div>

      {error ? (
        <div className="mt-4 rounded-2xl border px-3 py-2 text-xs" style={{ borderColor: "rgba(251,191,36,0.28)", background: "rgba(251,191,36,0.12)", color: "#fde68a" }}>
          {runs.length > 0
            ? t({
                ko: `최근 실행 이력은 유지 중이며 새 동기화에 실패했습니다. (${error})`,
                en: `Keeping the recent history while refresh failed. (${error})`,
                ja: `最新同期に失敗したため、直近の履歴を維持しています。(${error})`,
                zh: `最新刷新失败，正在保留最近的历史记录。(${error})`,
              })
            : t({
                ko: `자동큐 이력을 불러오지 못했습니다. (${error})`,
                en: `Unable to load auto-queue history. (${error})`,
                ja: `自動キュー履歴を読み込めませんでした。(${error})`,
                zh: `无法加载自动队列历史。(${error})`,
              })}
        </div>
      ) : null}

      {loading && runs.length === 0 ? (
        <div className="py-10 text-center text-sm" style={{ color: "var(--th-text-muted)" }}>
          {t({ ko: "자동큐 이력을 불러오는 중입니다", en: "Loading auto-queue history", ja: "自動キュー履歴を読み込み中", zh: "正在加载自动队列历史" })}
        </div>
      ) : runs.length === 0 ? (
        <div className="mt-4 rounded-2xl border border-dashed px-4 py-8 text-center text-sm" style={{ borderColor: "rgba(148,163,184,0.24)", color: "var(--th-text-muted)" }}>
          {t({
            ko: "아직 기록된 자동큐 실행이 없습니다. 실행이 시작되면 최근 성공률과 엔트리 규모가 이곳에 표시됩니다.",
            en: "No auto-queue runs have been recorded yet. Recent success rates and entry volume will appear here once runs start.",
            ja: "まだ記録された自動キュー実行はありません。実行が始まると、最近の成功率とエントリ規模がここに表示されます。",
            zh: "尚无自动队列运行记录。开始运行后，最近的成功率和条目规模会显示在这里。",
          })}
        </div>
      ) : (
        <div className="mt-4 space-y-2 max-h-80 overflow-y-auto">
          {visibleRuns.map((run) => {
          const statusColor =
            run.status === "completed"
              ? "#22c55e"
              : run.status === "cancelled"
                ? "#f87171"
                : "#fbbf24";
          return (
            <div
              key={run.id}
              className="rounded-xl border px-3 py-3"
              style={{ borderColor: "rgba(255,255,255,0.06)", background: "var(--th-bg-surface)" }}
            >
              <div className="flex flex-wrap items-start justify-between gap-3">
                <div className="min-w-0">
                  <div className="flex items-center gap-2 flex-wrap">
                    <span
                      className="rounded-full px-2 py-0.5 text-[11px] font-semibold uppercase"
                      style={{ color: statusColor, background: `${statusColor}1f` }}
                    >
                      {run.status}
                    </span>
                    <span className="truncate text-sm font-medium" style={{ color: "var(--th-text)" }}>
                      {run.repo || run.agent_id || run.id}
                    </span>
                  </div>
                  <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                    {new Date(run.created_at).toLocaleString(undefined, {
                      month: "2-digit",
                      day: "2-digit",
                      hour: "2-digit",
                      minute: "2-digit",
                    })}
                    {" · "}
                    {run.agent_id || "global"}
                  </div>
                </div>
                <div className="text-right text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                  <div>{formatDurationShort(run.duration_ms)}</div>
                  <div>{run.entry_count} {t({ ko: "엔트리", en: "entries", ja: "件", zh: "条目" })}</div>
                </div>
              </div>

              <div className="mt-3 grid grid-cols-2 gap-2 text-[11px] sm:grid-cols-4">
                <MetricChip label={t({ ko: "성공", en: "Success", ja: "成功", zh: "成功" })} value={formatPercent(run.success_rate)} accent="#22c55e" />
                <MetricChip label={t({ ko: "실패", en: "Failure", ja: "失敗", zh: "失败" })} value={formatPercent(run.failure_rate)} accent="#f87171" />
                <MetricChip label={t({ ko: "Done", en: "Done", ja: "Done", zh: "完成" })} value={String(run.done_count)} accent="#38bdf8" />
                <MetricChip label={t({ ko: "Skip/Pending", en: "Skip/Pending", ja: "Skip/Pending", zh: "跳过/待处理" })} value={String(run.skipped_count + run.pending_count + run.dispatched_count)} accent="#fbbf24" />
              </div>
            </div>
          );
          })}

          {hiddenRuns > 0 ? (
            <button
              type="button"
              className="w-full rounded-xl border px-3 py-2 text-xs font-medium"
              style={{ borderColor: "rgba(56,189,248,0.24)", color: "#38bdf8", background: "transparent" }}
              onClick={() => setExpanded(true)}
            >
              {t({
                ko: `${hiddenRuns}건 더 보기`,
                en: `Show ${hiddenRuns} more`,
                ja: `${hiddenRuns}件をさらに表示`,
                zh: `再显示 ${hiddenRuns} 条`,
              })}
            </button>
          ) : runs.length > AUTO_QUEUE_HISTORY_PREVIEW_COUNT ? (
            <button
              type="button"
              className="w-full rounded-xl border px-3 py-2 text-xs font-medium"
              style={{ borderColor: "rgba(148,163,184,0.24)", color: "var(--th-text-muted)", background: "transparent" }}
              onClick={() => setExpanded(false)}
            >
              {t({
                ko: "접기",
                en: "Collapse",
                ja: "折りたたむ",
                zh: "收起",
              })}
            </button>
          ) : null}
        </div>
      )}
    </div>
  );
}

function MetricChip({ label, value, accent }: { label: string; value: string; accent: string }) {
  return (
    <div className="rounded-xl px-2.5 py-2" style={{ background: "rgba(15,23,42,0.18)" }}>
      <div className="text-[10px] uppercase tracking-[0.16em]" style={{ color: "var(--th-text-muted)" }}>
        {label}
      </div>
      <div className="mt-1 text-sm font-semibold" style={{ color: accent }}>
        {value}
      </div>
    </div>
  );
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

// ── Achievement Wall Widget ──

interface AchievementWidgetProps {
  t: TFunction;
  agents: Agent[];
}

function fallbackAgentFromAchievement(achievement: api.Achievement): Agent {
  return {
    id: achievement.agent_id,
    name: achievement.agent_name,
    alias: null,
    name_ko: achievement.agent_name_ko || achievement.agent_name,
    department_id: null,
    avatar_emoji: achievement.avatar_emoji,
    personality: null,
    status: "idle",
    stats_tasks_done: 0,
    stats_xp: 0,
    stats_tokens: 0,
    created_at: 0,
  };
}

export function AchievementWidget({ t, agents }: AchievementWidgetProps) {
  const [achievements, setAchievements] = useState<api.Achievement[]>([]);
  const agentMap = useMemo(() => new Map(agents.map((agent) => [agent.id, agent])), [agents]);

  useEffect(() => {
    api.getAchievements().then((d) => setAchievements(d.achievements)).catch(() => {});
  }, []);

  if (achievements.length === 0) return null;

  const badgeIcon: Record<string, string> = {
    xp_100: "⭐", xp_500: "🌟", xp_1000: "💫", xp_5000: "🏅",
    tasks_10: "🐝", tasks_50: "👑", tasks_100: "🎖️",
    streak_7: "🔥", streak_30: "💎",
  };

  return (
    <div
      className={dashboardCard.accentStandard}
      style={{ borderColor: "var(--th-border)", background: "linear-gradient(145deg, color-mix(in srgb, var(--th-surface) 90%, #eab308 10%), var(--th-surface))" }}
    >
      <h3 className="text-sm font-semibold mb-3" style={{ color: "var(--th-text)" }}>
        🏆 {t({ ko: "업적", en: "Achievements", ja: "実績", zh: "成就" })}
      </h3>
      <div className="space-y-1.5 max-h-48 overflow-y-auto">
        {achievements.slice(0, 15).map((ach) => {
          const agent = agentMap.get(ach.agent_id) ?? fallbackAgentFromAchievement(ach);
          return (
            <div
              key={ach.id}
              className={cx(dashboardCard.nestedCompact, "flex items-center gap-2")}
              style={{ background: "var(--th-bg-surface)" }}
            >
              <div className="relative shrink-0">
                <AgentAvatar agent={agent} agents={agents} size={30} rounded="xl" className="shadow-sm" />
                <span
                  className="absolute -right-1 -top-1 flex h-5 w-5 items-center justify-center rounded-full text-[10px]"
                  style={{ background: "rgba(15,23,42,0.82)" }}
                >
                  {badgeIcon[ach.type] || "🎯"}
                </span>
              </div>
              <div className="flex-1 min-w-0">
                <div className="text-xs font-medium truncate" style={{ color: "var(--th-text)" }}>
                  {ach.agent_name_ko || ach.agent_name}
                </div>
                <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                  {ach.name} — {ach.description}
                </div>
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}

// ── Skill Trend Chart (simple sparkline) ──

interface SkillTrendWidgetProps {
  t: TFunction;
}

export function SkillTrendWidget({ t }: SkillTrendWidgetProps) {
  const [trend, setTrend] = useState<api.SkillTrendPoint[] | null>(null);

  useEffect(() => {
    let mounted = true;
    const load = async () => {
      try {
        const nextTrend = await api.getSkillTrend(30);
        if (mounted) setTrend(nextTrend);
      } catch {
        // Ignore transient skill trend failures in the dashboard.
      }
    };

    void load();
    const timer = setInterval(() => void load(), 60_000);
    return () => {
      mounted = false;
      clearInterval(timer);
    };
  }, []);

  if (!trend || trend.length === 0) return null;

  const days = trend.map((entry) => entry.day);
  const dailyTotals = trend.map((entry) => entry.count);
  const max = Math.max(1, ...dailyTotals);

  return (
    <div
      className={dashboardCard.standard}
      style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
    >
      <h3 className="text-sm font-semibold mb-3" style={{ color: "var(--th-text)" }}>
        {t({ ko: "스킬 사용 추이 (30일)", en: "Skill Usage Trend (30d)", ja: "スキル使用推移 (30日)", zh: "技能使用趋势 (30天)" })}
      </h3>
      <div className="flex items-end gap-[3px] h-12">
        {dailyTotals.map((total, i) => (
          <div
            key={days[i]}
            className="flex-1 rounded-t"
            style={{
              height: `${Math.max(4, (total / max) * 100)}%`,
              background: `rgba(245,158,11,${0.3 + (total / max) * 0.5})`,
              minWidth: 0,
            }}
            title={`${days[i]}: ${total} calls`}
          />
        ))}
      </div>
      <div className="flex justify-between mt-1">
        {days.length > 0 && (
          <>
            <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
              {days[0].slice(5)}
            </span>
            <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
              {days[days.length - 1].slice(5)}
            </span>
          </>
        )}
      </div>
    </div>
  );
}
