import { useEffect, useMemo, useState } from "react";
import * as api from "../../api/client";
import type { TFunction } from "./model";

const AUTO_QUEUE_HISTORY_LIMIT = 24;
const AUTO_QUEUE_HISTORY_PREVIEW_COUNT = 8;

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
