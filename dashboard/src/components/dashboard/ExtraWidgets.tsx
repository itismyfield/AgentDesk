import { useEffect, useState, useMemo } from "react";
import type { Agent, DashboardStats } from "../../types";
import * as api from "../../api/client";
import { localeName, type UiLanguage } from "../../i18n";
import { getFontFamilyForText } from "../../lib/fonts";
import { getRankTier, type TFunction } from "./model";
import AgentAvatar from "../AgentAvatar";
import { cx, dashboardBadge, dashboardCard, DashboardEmptyState } from "./ui";
export { BottleneckWidget } from "./BottleneckWidget";

const DEFAULT_CRON_TIMELINE_WINDOW_MS = 60 * 60_000;
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

function buildWeightedSuccessRate(runs: api.AutoQueueHistoryRun[]): number {
  const totalEntries = runs.reduce((sum, run) => sum + Math.max(run.entry_count, 0), 0);
  if (totalEntries <= 0) return 0;
  const successfulEntries = runs.reduce(
    (sum, run) => sum + Math.max(run.entry_count, 0) * run.success_rate,
    0,
  );
  return successfulEntries / totalEntries;
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

// ── Agent Quality Widget ──

interface AgentQualityWidgetProps {
  agents: Agent[];
  t: TFunction;
  localeTag: string;
  compact?: boolean;
}

type AgentQualityMetric = "turn" | "review";

function agentQualityUnavailableLabel(t: TFunction): string {
  return t({
    ko: "측정 불가",
    en: "Not enough data",
    ja: "測定不可",
    zh: "无法测量",
  });
}

function formatQualityRate(
  window: api.AgentQualityWindow | null | undefined,
  metric: AgentQualityMetric,
  t: TFunction,
): string {
  if (!window || window.measurementUnavailable) {
    return window?.measurementLabel || agentQualityUnavailableLabel(t);
  }
  const value = metric === "turn" ? window.turnSuccessRate : window.reviewPassRate;
  if (value == null || !Number.isFinite(value)) return agentQualityUnavailableLabel(t);
  return `${Math.round(value * 100)}%`;
}

function qualityRateValue(
  window: api.AgentQualityWindow | null | undefined,
  metric: AgentQualityMetric,
): number | null {
  if (!window || window.measurementUnavailable) return null;
  const value = metric === "turn" ? window.turnSuccessRate : window.reviewPassRate;
  return value != null && Number.isFinite(value) ? value : null;
}

function formatQualityDelta(
  current: api.AgentQualityWindow | null | undefined,
  baseline: api.AgentQualityWindow | null | undefined,
  metric: AgentQualityMetric,
): string {
  const currentValue = qualityRateValue(current, metric);
  const baselineValue = qualityRateValue(baseline, metric);
  if (currentValue == null || baselineValue == null) return "-";
  const delta = Math.round((currentValue - baselineValue) * 100);
  return `${delta >= 0 ? "+" : ""}${delta}pp`;
}

function dailyQualityValue(record: api.AgentQualityDailyRecord, metric: AgentQualityMetric): number | null {
  const value = metric === "turn" ? record.turnSuccessRate : record.reviewPassRate;
  return value != null && Number.isFinite(value) ? value : null;
}

function formatDailyQualityRate(
  record: api.AgentQualityDailyRecord,
  metric: AgentQualityMetric,
  t: TFunction,
): string {
  if (record.sampleSize < 5) return agentQualityUnavailableLabel(t);
  const value = dailyQualityValue(record, metric);
  if (value == null) return agentQualityUnavailableLabel(t);
  return `${Math.round(value * 100)}%`;
}

function formatQualityDay(day: string, localeTag: string): string {
  const timestamp = Date.parse(`${day}T00:00:00`);
  if (Number.isNaN(timestamp)) return day;
  return new Date(timestamp).toLocaleDateString(localeTag, {
    month: "2-digit",
    day: "2-digit",
  });
}

function qualityAgentLabel(
  agent: Agent | undefined,
  entry: api.AgentQualityRankingEntry | undefined,
  fallbackId: string,
): string {
  return agent?.alias?.trim() || agent?.name_ko || agent?.name || entry?.agentName || fallbackId;
}

function fallbackQualitySummary(entry: api.AgentQualityRankingEntry | undefined): api.AgentQualitySummary | null {
  if (!entry) return null;
  return {
    generatedAt: new Date().toISOString(),
    agentId: entry.agentId,
    latest: null,
    daily: [],
  };
}

function QualitySparkline({
  records,
  metric,
  accent,
}: {
  records: api.AgentQualityDailyRecord[];
  metric: AgentQualityMetric;
  accent: string;
}) {
  const values = records
    .slice()
    .sort((a, b) => a.day.localeCompare(b.day))
    .map((record) => dailyQualityValue(record, metric))
    .filter((value): value is number => value != null);

  if (values.length === 0) {
    return (
      <div className="h-9 w-full rounded-lg" style={{ background: "rgba(148,163,184,0.12)" }} />
    );
  }

  const width = 144;
  const height = 36;
  const points = values
    .map((value, index) => {
      const x = values.length === 1 ? width / 2 : (index / (values.length - 1)) * width;
      const y = height - Math.min(1, Math.max(0, value)) * (height - 4) - 2;
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");

  return (
    <svg className="h-9 w-full overflow-visible" viewBox={`0 0 ${width} ${height}`} role="img" aria-hidden="true">
      <polyline
        points={points}
        fill="none"
        stroke={accent}
        strokeLinecap="round"
        strokeLinejoin="round"
        strokeWidth="2.5"
      />
      <polyline
        points={points}
        fill="none"
        stroke={accent}
        strokeLinecap="round"
        strokeLinejoin="round"
        strokeOpacity="0.18"
        strokeWidth="8"
      />
    </svg>
  );
}

function QualityMetricCell({
  label,
  value,
  sub,
  accent,
}: {
  label: string;
  value: string;
  sub: string;
  accent: string;
}) {
  return (
    <div className="rounded-xl border px-3 py-2" style={{ borderColor: "rgba(148,163,184,0.14)", background: "rgba(15,23,42,0.16)" }}>
      <div className="text-[11px] font-medium" style={{ color: "var(--th-text-muted)" }}>
        {label}
      </div>
      <div className="mt-1 text-lg font-semibold" style={{ color: accent }}>
        {value}
      </div>
      <div className="mt-0.5 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
        {sub}
      </div>
    </div>
  );
}

export function AgentQualityWidget({ agents, t, localeTag, compact = false }: AgentQualityWidgetProps) {
  const [ranking, setRanking] = useState<api.AgentQualityRankingEntry[]>([]);
  const [summaries, setSummaries] = useState<Record<string, api.AgentQualitySummary>>({});
  const [selectedAgentId, setSelectedAgentId] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const agentMap = useMemo(() => new Map(agents.map((agent) => [agent.id, agent])), [agents]);

  useEffect(() => {
    let mounted = true;

    const load = async () => {
      if (mounted) setLoading(true);
      try {
        const nextRanking = await api.getAgentQualityRanking(compact ? 6 : 10);
        const rankedIds = nextRanking.agents.slice(0, compact ? 5 : 8).map((entry) => entry.agentId);
        const summaryPairs = await Promise.all(
          rankedIds.map(async (agentId) => {
            try {
              const summary = await api.getAgentQuality(agentId, 30, 30);
              return [agentId, summary] as const;
            } catch {
              return null;
            }
          }),
        );

        if (!mounted) return;
        const nextSummaries: Record<string, api.AgentQualitySummary> = {};
        for (const pair of summaryPairs) {
          if (pair) nextSummaries[pair[0]] = pair[1];
        }
        setRanking(nextRanking.agents);
        setSummaries(nextSummaries);
        setSelectedAgentId((current) =>
          current && nextRanking.agents.some((entry) => entry.agentId === current)
            ? current
            : nextRanking.agents[0]?.agentId ?? null,
        );
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
  }, [compact]);

  const selectedRanking = ranking.find((entry) => entry.agentId === selectedAgentId);
  const selectedSummary =
    (selectedAgentId ? summaries[selectedAgentId] : null) ?? fallbackQualitySummary(selectedRanking);
  const selectedAgent = selectedAgentId ? agentMap.get(selectedAgentId) : undefined;
  const selectedLabel = selectedAgentId
    ? qualityAgentLabel(selectedAgent, selectedRanking, selectedAgentId)
    : t({ ko: "에이전트", en: "Agent", ja: "エージェント", zh: "代理" });
  const selectedWindow7d = selectedRanking?.rolling7d ?? selectedSummary?.latest?.rolling7d ?? null;
  const selectedWindow30d = selectedRanking?.rolling30d ?? selectedSummary?.latest?.rolling30d ?? null;
  const visibleRanking = ranking.slice(0, compact ? 4 : 6);
  const dailyRows = (selectedSummary?.daily ?? []).slice(0, compact ? 5 : 9);

  return (
    <div
      className={cx(dashboardCard.standard, "space-y-4")}
      style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
    >
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <h3 className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {t({ ko: "에이전트 품질", en: "Agent Quality", ja: "エージェント品質", zh: "代理质量" })}
          </h3>
          <p className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "최근 7일 성과와 30일 기준선을 함께 봅니다",
              en: "Seven-day performance against the 30-day baseline",
              ja: "直近7日の成績と30日基準を並べて確認します",
              zh: "查看最近 7 天表现与 30 天基线",
            })}
          </p>
        </div>
        <span className={dashboardBadge.default} style={{ background: "var(--th-overlay-medium)" }}>
          {error
            ? t({ ko: "동기화 오류", en: "sync error", ja: "同期エラー", zh: "同步错误" })
            : loading
              ? t({ ko: "갱신 중", en: "refreshing", ja: "更新中", zh: "刷新中" })
              : t({ ko: "7d / 30d", en: "7d / 30d", ja: "7d / 30d", zh: "7d / 30d" })}
        </span>
      </div>

      {visibleRanking.length === 0 ? (
        <div className={cx(dashboardCard.nestedCompact, "text-sm")} style={{ color: "var(--th-text-muted)", background: "var(--th-bg-surface)" }}>
          {error || t({ ko: "아직 품질 집계가 없습니다", en: "No quality rollup yet", ja: "品質集計はまだありません", zh: "尚无质量汇总" })}
        </div>
      ) : (
        <div className="grid gap-2 sm:grid-cols-2">
          {visibleRanking.map((entry) => {
            const agent = agentMap.get(entry.agentId);
            const summary = summaries[entry.agentId];
            const isSelected = entry.agentId === selectedAgentId;
            const label = qualityAgentLabel(agent, entry, entry.agentId);
            const accent = isSelected ? "#38bdf8" : "#34d399";
            return (
              <button
                key={entry.agentId}
                type="button"
                className={cx(
                  dashboardCard.interactiveNestedCompact,
                  "min-w-0 text-left transition",
                )}
                style={{
                  background: isSelected
                    ? "color-mix(in srgb, var(--th-accent-info) 12%, var(--th-bg-surface) 88%)"
                    : "var(--th-bg-surface)",
                  borderColor: isSelected
                    ? "color-mix(in srgb, var(--th-accent-info) 42%, var(--th-border) 58%)"
                    : "rgba(148,163,184,0.14)",
                }}
                onClick={() => setSelectedAgentId(entry.agentId)}
              >
                <div className="flex items-center gap-2">
                  <AgentAvatar agent={agent} agents={agents} size={30} rounded="xl" className="shrink-0 shadow-sm" />
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-2">
                      <span className="text-xs font-semibold" style={{ color: accent }}>
                        #{entry.rank}
                      </span>
                      <span className="truncate text-sm font-semibold" style={{ color: "var(--th-text)" }}>
                        {label}
                      </span>
                    </div>
                    <div className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                      {entry.provider || t({ ko: "provider 미상", en: "provider n/a", ja: "provider 不明", zh: "provider 未知" })}
                    </div>
                  </div>
                  <div className="text-right">
                    <div className="text-sm font-semibold" style={{ color: accent }}>
                      {formatQualityRate(entry.rolling7d, "turn", t)}
                    </div>
                    <div className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                      {t({ ko: "성공", en: "success", ja: "成功", zh: "成功" })}
                    </div>
                  </div>
                </div>
                <div className="mt-3">
                  <QualitySparkline records={summary?.daily ?? []} metric="turn" accent={accent} />
                </div>
              </button>
            );
          })}
        </div>
      )}

      {selectedAgentId ? (
        <div className={cx(dashboardCard.nestedCompact, "space-y-3")} style={{ background: "var(--th-bg-surface)" }}>
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div className="flex min-w-0 items-center gap-2">
              <AgentAvatar agent={selectedAgent} agents={agents} size={36} rounded="xl" className="shrink-0 shadow-sm" />
              <div className="min-w-0">
                <div className="truncate text-sm font-semibold" style={{ color: "var(--th-text)" }}>
                  {selectedLabel}
                </div>
                <div className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                  {selectedSummary?.latest?.day
                    ? formatQualityDay(selectedSummary.latest.day, localeTag)
                    : t({ ko: "일별 상세 대기", en: "daily detail pending", ja: "日別詳細待ち", zh: "每日详情待定" })}
                </div>
              </div>
            </div>
            <span className={dashboardBadge.default} style={{ background: "var(--th-overlay-medium)" }}>
              {selectedWindow7d?.sampleSize ?? 0} samples
            </span>
          </div>

          <div className="grid gap-2 sm:grid-cols-2">
            <QualityMetricCell
              label={t({ ko: "턴 성공률", en: "Turn Success", ja: "ターン成功率", zh: "回合成功率" })}
              value={formatQualityRate(selectedWindow7d, "turn", t)}
              sub={`${t({ ko: "30일 대비", en: "vs 30d", ja: "30日比", zh: "对比 30 天" })} ${formatQualityDelta(selectedWindow7d, selectedWindow30d, "turn")}`}
              accent="#34d399"
            />
            <QualityMetricCell
              label={t({ ko: "리뷰 통과율", en: "Review Pass", ja: "レビュー通過率", zh: "审查通过率" })}
              value={formatQualityRate(selectedWindow7d, "review", t)}
              sub={`${t({ ko: "30일 대비", en: "vs 30d", ja: "30日比", zh: "对比 30 天" })} ${formatQualityDelta(selectedWindow7d, selectedWindow30d, "review")}`}
              accent="#38bdf8"
            />
          </div>

          <div className="grid gap-3 lg:grid-cols-[minmax(0,0.8fr)_minmax(0,1.2fr)]">
            <div className="rounded-xl border px-3 py-2" style={{ borderColor: "rgba(148,163,184,0.14)", background: "rgba(15,23,42,0.12)" }}>
              <div className="mb-2 text-[11px] font-medium" style={{ color: "var(--th-text-muted)" }}>
                {t({ ko: "추세", en: "Trend", ja: "推移", zh: "趋势" })}
              </div>
              <QualitySparkline records={selectedSummary?.daily ?? []} metric="turn" accent="#34d399" />
              <QualitySparkline records={selectedSummary?.daily ?? []} metric="review" accent="#38bdf8" />
            </div>
            <div className="max-h-56 space-y-1.5 overflow-y-auto">
              {dailyRows.length === 0 ? (
                <div className="rounded-xl px-3 py-3 text-xs" style={{ color: "var(--th-text-muted)", background: "rgba(148,163,184,0.08)" }}>
                  {t({ ko: "일별 상세 없음", en: "No daily detail", ja: "日別詳細なし", zh: "无每日详情" })}
                </div>
              ) : (
                dailyRows.map((record) => (
                  <div
                    key={`${record.agentId}-${record.day}`}
                    className="grid grid-cols-[4.5rem_1fr_1fr_3rem] items-center gap-2 rounded-xl px-3 py-2 text-xs"
                    style={{ background: "rgba(148,163,184,0.08)", color: "var(--th-text)" }}
                  >
                    <span style={{ color: "var(--th-text-muted)" }}>{formatQualityDay(record.day, localeTag)}</span>
                    <span>{formatDailyQualityRate(record, "turn", t)}</span>
                    <span>{formatDailyQualityRate(record, "review", t)}</span>
                    <span className="text-right" style={{ color: "var(--th-text-muted)" }}>{record.sampleSize}</span>
                  </div>
                ))
              )}
            </div>
          </div>
        </div>
      ) : null}
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
      <h3
        className="font-pixel mb-3 text-sm font-semibold"
        style={{
          color: "var(--th-text)",
          fontFamily: getFontFamilyForText(
            t({ ko: "업적", en: "Achievements", ja: "実績", zh: "成就" }),
            "pixel",
          ),
        }}
      >
        🏆 {t({ ko: "업적", en: "Achievements", ja: "実績", zh: "成就" })}
      </h3>
      <div className="space-y-1.5 max-h-48 overflow-y-auto">
        {achievements.slice(0, 15).map((ach) => {
          const agent = agentMap.get(ach.agent_id) ?? fallbackAgentFromAchievement(ach);
          const agentLabel = ach.agent_name_ko || ach.agent_name;
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
                <div
                  className="font-pixel truncate text-xs font-medium"
                  style={{
                    color: "var(--th-text)",
                    fontFamily: getFontFamilyForText(agentLabel, "pixel"),
                  }}
                >
                  {agentLabel}
                </div>
                <div
                  className="font-pixel text-xs"
                  style={{
                    color: "var(--th-text-muted)",
                    fontFamily: getFontFamilyForText(ach.name, "pixel"),
                  }}
                >
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

// ── Department Performance + Squad Roster (#1254 audit restore) ─────────────────

const DEPT_BAR_COLORS = [
  { bar: "from-blue-500 to-cyan-400", badge: "bg-blue-500/20 text-blue-200 border-blue-400/30" },
  { bar: "from-lime-500 to-emerald-400", badge: "bg-lime-500/20 text-lime-100 border-lime-400/30" },
  { bar: "from-emerald-500 to-teal-400", badge: "bg-emerald-500/20 text-emerald-200 border-emerald-400/30" },
  { bar: "from-amber-500 to-orange-400", badge: "bg-amber-500/20 text-amber-100 border-amber-400/30" },
  { bar: "from-rose-500 to-pink-400", badge: "bg-rose-500/20 text-rose-100 border-rose-400/30" },
  { bar: "from-cyan-500 to-sky-400", badge: "bg-cyan-500/20 text-cyan-100 border-cyan-400/30" },
  { bar: "from-orange-500 to-red-400", badge: "bg-orange-500/20 text-orange-100 border-orange-400/30" },
  { bar: "from-teal-500 to-lime-400", badge: "bg-teal-500/20 text-teal-100 border-teal-400/30" },
];

export interface DepartmentPerformanceRow {
  id: string;
  name: string;
  icon: string;
  done: number;
  total: number;
  ratio: number;
  color: { bar: string; badge: string };
}

export function buildDepartmentPerformanceRows(
  departments: DashboardStats["departments"],
  language: UiLanguage,
): DepartmentPerformanceRow[] {
  const totalXp = departments.reduce((sum, d) => sum + (d.sum_xp ?? 0), 0);
  return departments.map((dept, index) => ({
    id: dept.id,
    name: language === "ko" ? dept.name_ko || dept.name : dept.name,
    icon: dept.icon,
    done: dept.sum_xp ?? 0,
    total: totalXp,
    ratio: totalXp > 0 ? Math.round(((dept.sum_xp ?? 0) / totalXp) * 100) : 0,
    color: DEPT_BAR_COLORS[index % DEPT_BAR_COLORS.length],
  }));
}

interface DashboardDeptAndSquadProps {
  deptRows: DepartmentPerformanceRow[];
  workingAgents: Agent[];
  idleAgentsList: Agent[];
  agents: Agent[];
  language: UiLanguage;
  numberFormatter: Intl.NumberFormat;
  t: TFunction;
  onSelectAgent?: (agent: Agent) => void;
}

export function DashboardDeptAndSquad({
  deptRows,
  workingAgents,
  idleAgentsList,
  agents,
  language,
  numberFormatter,
  t,
  onSelectAgent,
}: DashboardDeptAndSquadProps) {
  return (
    <div className="grid grid-cols-1 gap-4 xl:grid-cols-[1.2fr_1fr]">
      <div
        className={dashboardCard.standard}
        style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
      >
        <div className="mb-4 flex items-center gap-2">
          <h3 className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {t({ ko: "부서 성과", en: "Dept. Performance", ja: "部署パフォーマンス", zh: "部门绩效" })}
          </h3>
          <span className="ml-auto text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({ ko: "부서별 XP 비율", en: "XP ratio by department", ja: "部署別 XP 比率", zh: "按部门 XP 占比" })}
          </span>
        </div>

        {deptRows.length === 0 ? (
          <DashboardEmptyState
            icon="🏰"
            title={t({ ko: "데이터 없음", en: "No data", ja: "データなし", zh: "暂无数据" })}
            description={t({
              ko: "표시할 부서 성과 집계가 아직 없습니다.",
              en: "Department performance metrics are not available yet.",
              ja: "表示できる部署パフォーマンス集計がまだありません。",
              zh: "暂时没有可显示的部门绩效统计。",
            })}
            className="min-h-[180px]"
          />
        ) : (
          <div className="space-y-2.5">
            {deptRows.map((dept) => (
              <article
                key={dept.id}
                className={cx(dashboardCard.nestedCompact, "group")}
                style={{ background: "var(--th-bg-surface)" }}
              >
                <div className="flex items-center justify-between">
                  <div className="flex items-center gap-2.5">
                    <span
                      className="flex h-8 w-8 items-center justify-center rounded-lg text-base"
                      style={{ background: "var(--th-surface)" }}
                    >
                      {dept.icon}
                    </span>
                    <span className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
                      {dept.name}
                    </span>
                  </div>
                  <span className={cx(dashboardBadge.default, dept.color.badge, "border")}>
                    {dept.ratio}%
                  </span>
                </div>

                <div
                  className="mt-2.5 relative h-2 overflow-hidden rounded-full"
                  style={{ background: "var(--th-overlay-medium)" }}
                >
                  <div
                    className={`h-full rounded-full bg-gradient-to-r ${dept.color.bar} transition-all duration-700`}
                    style={{ width: `${dept.ratio}%` }}
                  />
                </div>

                <div
                  className="mt-1.5 flex justify-between text-xs"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  <span>XP {numberFormatter.format(dept.done)}</span>
                  <span>
                    {t({ ko: "전체", en: "total", ja: "全体", zh: "总计" })}
                    {" "}XP {numberFormatter.format(dept.total)}
                  </span>
                </div>
              </article>
            ))}
          </div>
        )}
      </div>

      <div
        className={dashboardCard.standard}
        style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
      >
        <div className="mb-4 flex items-center justify-between">
          <h3 className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {t({ ko: "스쿼드", en: "Squad", ja: "スクワッド", zh: "小队" })}
          </h3>
          <div className="flex items-center gap-2 text-xs">
            <span
              className={dashboardBadge.default}
              style={{ background: "rgba(16,185,129,0.15)", color: "#34d399", borderColor: "rgba(16,185,129,0.3)" }}
            >
              ON {numberFormatter.format(workingAgents.length)}
            </span>
            <span
              className={dashboardBadge.default}
              style={{ background: "var(--th-overlay-medium)", color: "var(--th-text-muted)" }}
            >
              OFF {numberFormatter.format(idleAgentsList.length)}
            </span>
          </div>
        </div>

        {agents.length === 0 ? (
          <DashboardEmptyState
            icon="🤖"
            title={t({ ko: "에이전트 없음", en: "No agents", ja: "エージェントなし", zh: "暂无代理" })}
            description={t({
              ko: "등록된 에이전트가 없습니다.",
              en: "There are no agents registered.",
              ja: "登録されたエージェントがありません。",
              zh: "没有注册的代理。",
            })}
            className="min-h-[180px]"
          />
        ) : (
          <div className="flex flex-wrap gap-3">
            {agents.map((agent) => {
              const isWorking = agent.status === "working";
              const tier = getRankTier(agent.stats_xp);
              return (
                <button
                  key={agent.id}
                  type="button"
                  title={`${localeName(language, agent)} — ${
                    isWorking
                      ? t({ ko: "작업 중", en: "Working", ja: "作業中", zh: "工作中" })
                      : t({ ko: "대기 중", en: "Idle", ja: "待機中", zh: "空闲" })
                  } — ${tier.name}`}
                  className="group flex flex-col items-center gap-1.5"
                  onClick={() => onSelectAgent?.(agent)}
                >
                  <div className="relative">
                    <div
                      className="overflow-hidden rounded-2xl transition-transform duration-200 group-hover:scale-110"
                      style={{
                        boxShadow: isWorking ? `0 0 12px ${tier.glow}` : "none",
                        border: isWorking
                          ? `2px solid ${tier.color}60`
                          : "1px solid rgba(148,163,184,0.18)",
                      }}
                    >
                      <AgentAvatar agent={agent} agents={agents} size={40} rounded="2xl" />
                    </div>
                    <span
                      className={`absolute -bottom-0.5 -right-0.5 h-3 w-3 rounded-full border-2 ${
                        isWorking ? "bg-emerald-400" : "bg-slate-600"
                      }`}
                      style={{ borderColor: "var(--th-bg-primary)" }}
                    />
                  </div>
                  <span
                    className="max-w-[60px] truncate text-center text-xs font-medium leading-tight"
                    style={{ color: isWorking ? "var(--th-text)" : "var(--th-text-muted)" }}
                  >
                    {localeName(language, agent)}
                  </span>
                </button>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}

// ── GitHub Issues Widget (#1254 audit restore) ──────────────────────────────────

interface GitHubIssuesWidgetProps {
  t: TFunction;
  repo?: string;
}

export function GitHubIssuesWidget({ t, repo }: GitHubIssuesWidgetProps) {
  const [data, setData] = useState<api.GitHubIssuesResponse | null>(null);

  useEffect(() => {
    let mounted = true;
    api
      .getGitHubIssues(repo, "open", 8)
      .then((next) => {
        if (mounted) setData(next);
      })
      .catch(() => {
        // GitHub fetch failures are non-blocking — leave the widget empty.
      });
    return () => {
      mounted = false;
    };
  }, [repo]);

  if (!data || data.issues.length === 0) return null;

  return (
    <div
      className={dashboardCard.standard}
      style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
    >
      <div className="mb-3 flex items-center justify-between">
        <h3 className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
          {t({ ko: "GitHub 이슈", en: "GitHub Issues", ja: "GitHub Issues", zh: "GitHub Issues" })}
        </h3>
        <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
          {data.repo}
        </span>
      </div>
      <div className="max-h-48 space-y-1.5 overflow-y-auto">
        {data.issues.map((issue) => (
          <div
            key={issue.number}
            className={cx(dashboardCard.nestedCompact, "flex items-start gap-2")}
            style={{ background: "var(--th-bg-surface)" }}
          >
            <span className="mt-0.5 shrink-0 text-xs" style={{ color: "#34d399" }}>
              #{issue.number}
            </span>
            <div className="min-w-0 flex-1">
              <div className="truncate text-xs font-medium" style={{ color: "var(--th-text)" }}>
                {issue.title}
              </div>
              <div className="mt-0.5 flex flex-wrap gap-1">
                {issue.labels.slice(0, 3).map((label) => (
                  <span
                    key={label.name}
                    className={dashboardBadge.default}
                    style={{ background: `#${label.color}33`, color: `#${label.color}` }}
                  >
                    {label.name}
                  </span>
                ))}
                {issue.assignees.length > 0 && (
                  <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                    → {issue.assignees.map((a) => a.login).join(", ")}
                  </span>
                )}
              </div>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

// ── Activity Heatmap Widget (#1254 audit restore) ──────────────────────────────

interface HeatmapWidgetProps {
  t: TFunction;
}

export function HeatmapWidget({ t }: HeatmapWidgetProps) {
  const [data, setData] = useState<api.HeatmapData | null>(null);

  useEffect(() => {
    let mounted = true;
    api
      .getActivityHeatmap()
      .then((next) => {
        if (mounted) setData(next);
      })
      .catch(() => {
        // Heatmap is best-effort — render nothing on transient failure.
      });
    return () => {
      mounted = false;
    };
  }, []);

  if (!data || data.hours.length === 0) return null;

  const maxCount = Math.max(
    1,
    ...data.hours.map((hour) => Object.values(hour.agents).reduce((sum, value) => sum + value, 0)),
  );
  const currentHour = new Date().getHours();

  return (
    <div
      className={dashboardCard.standard}
      style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
    >
      <h3 className="mb-3 text-sm font-semibold" style={{ color: "var(--th-text)" }}>
        {t({
          ko: "오늘의 활동 히트맵",
          en: "Today's Activity Heatmap",
          ja: "今日の活動ヒートマップ",
          zh: "今日活动热力图",
        })}
      </h3>
      <div className="flex h-16 items-end gap-[2px]">
        {data.hours.map((hour) => {
          const total = Object.values(hour.agents).reduce((sum, value) => sum + value, 0);
          const height = Math.max(2, (total / maxCount) * 100);
          const isCurrent = hour.hour === currentHour;
          return (
            <div
              key={hour.hour}
              className="relative flex-1 cursor-default rounded-t"
              style={{
                height: `${height}%`,
                background:
                  total === 0
                    ? "rgba(100,116,139,0.15)"
                    : isCurrent
                      ? "var(--th-accent-primary)"
                      : `color-mix(in srgb, var(--th-accent-primary) ${Math.round(
                          (0.2 + (total / maxCount) * 0.6) * 100,
                        )}%, transparent)`,
                minWidth: 0,
              }}
              title={`${hour.hour}:00 — ${total} events`}
            />
          );
        })}
      </div>
      <div className="mt-1 flex justify-between text-xs" style={{ color: "var(--th-text-muted)" }}>
        <span>0h</span>
        <span>6h</span>
        <span>12h</span>
        <span>18h</span>
        <span>24h</span>
      </div>
    </div>
  );
}
