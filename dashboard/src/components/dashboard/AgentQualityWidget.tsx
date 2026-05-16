import { useEffect, useMemo, useState } from "react";
import type { Agent } from "../../types";
import * as api from "../../api/client";
import { localeName } from "../../i18n";
import AgentAvatar from "../AgentAvatar";
import { getRankTier, type TFunction } from "./model";
import { cx, dashboardBadge, dashboardCard, DashboardEmptyState } from "./ui";

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
