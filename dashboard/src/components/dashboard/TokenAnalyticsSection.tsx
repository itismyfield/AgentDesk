import { type CSSProperties, useEffect, useMemo, useState } from "react";
import { getTokenAnalytics } from "../../api";
import type {
  ReceiptSnapshotAgentShare,
  ReceiptSnapshotModelLine,
  TokenAnalyticsDailyPoint,
  TokenAnalyticsHeatmapCell,
  TokenAnalyticsResponse,
} from "../../types";
import type { TFunction } from "./model";
import { cx, dashboardBadge, dashboardButton, dashboardCard } from "./ui";

type Period = "7d" | "30d" | "90d";

interface TokenAnalyticsSectionProps {
  t: TFunction;
  numberFormatter: Intl.NumberFormat;
}

interface CachedAnalyticsEntry {
  data: TokenAnalyticsResponse;
  fetchedAt: number;
}

interface ModelSegment {
  id: string;
  label: string;
  provider: string;
  tokens: number;
  percentage: number;
  color: string;
}

type TrendSeriesKey = "input_tokens" | "output_tokens" | "cache_read_tokens" | "cache_creation_tokens";
type TrendPattern = "diagonal" | "dots" | "horizontal" | "cross";

interface TrendLegendItem {
  key: TrendSeriesKey;
  color: string;
  label: string;
  pattern: TrendPattern;
}

const analyticsCache = new Map<Period, CachedAnalyticsEntry>();
const ANALYTICS_CACHE_TTL = 5 * 60_000;
const PERIOD_OPTIONS: Period[] = ["7d", "30d", "90d"];
const TREND_PLOT_HEIGHT_PX = 144;
const MODEL_PALETTES: Record<string, string[]> = {
  Claude: ["#f59e0b", "#fbbf24", "#fb7185", "#f97316"],
  Codex: ["#22c55e", "#14b8a6", "#06b6d4", "#0ea5e9"],
  Gemini: ["#60a5fa", "#818cf8", "#a855f7", "#38bdf8"],
  default: ["#94a3b8", "#c084fc", "#fb7185", "#2dd4bf"],
};
const HEATMAP_COLORS = [
  "rgba(148,163,184,0.08)",
  "rgba(14,165,233,0.24)",
  "rgba(34,197,94,0.38)",
  "rgba(245,158,11,0.52)",
  "rgba(249,115,22,0.72)",
];

function formatTokens(value: number): string {
  if (value >= 1e9) return `${(value / 1e9).toFixed(1)}B`;
  if (value >= 1e6) return `${(value / 1e6).toFixed(1)}M`;
  if (value >= 1e3) return `${(value / 1e3).toFixed(1)}K`;
  return String(value);
}

function formatCost(value: number): string {
  if (value >= 100) return `$${value.toFixed(0)}`;
  if (value >= 1) return `$${value.toFixed(2)}`;
  if (value >= 0.01) return `$${value.toFixed(3)}`;
  return `$${value.toFixed(4)}`;
}

function modelColor(provider: string, index: number): string {
  const palette = MODEL_PALETTES[provider] ?? MODEL_PALETTES.default;
  return palette[index % palette.length];
}

function buildModelSegments(models: ReceiptSnapshotModelLine[]): ModelSegment[] {
  const totalTokens = models.reduce((sum, model) => sum + model.total_tokens, 0);
  if (totalTokens === 0) return [];

  const sorted = [...models].sort((a, b) => b.total_tokens - a.total_tokens);
  const visible = sorted.slice(0, 6);
  const remainder = sorted.slice(6);
  const segments = visible.map((model, index) => ({
    id: `${model.provider}-${model.model}`,
    label: model.display_name,
    provider: model.provider,
    tokens: model.total_tokens,
    percentage: (model.total_tokens / totalTokens) * 100,
    color: modelColor(model.provider, index),
  }));

  if (remainder.length > 0) {
    const remainderTokens = remainder.reduce((sum, model) => sum + model.total_tokens, 0);
    segments.push({
      id: "other",
      label: "Other",
      provider: "Mixed",
      tokens: remainderTokens,
      percentage: (remainderTokens / totalTokens) * 100,
      color: "#94a3b8",
    });
  }

  return segments;
}

function buildDonutBackground(segments: ModelSegment[]): string {
  if (segments.length === 0) {
    return "conic-gradient(rgba(148,163,184,0.18) 0deg 360deg)";
  }
  let cursor = 0;
  const stops = segments.map((segment) => {
    const start = cursor;
    cursor += segment.percentage * 3.6;
    return `${segment.color} ${start}deg ${cursor}deg`;
  });
  if (cursor < 360) stops.push(`rgba(148,163,184,0.08) ${cursor}deg 360deg`);
  return `conic-gradient(${stops.join(", ")})`;
}

function buildWeekLabels(cells: TokenAnalyticsHeatmapCell[]): Array<{ week: number; label: string }> {
  const formatter = new Intl.DateTimeFormat("en-US", { month: "short" });
  const byWeek = new Map<number, TokenAnalyticsHeatmapCell[]>();
  for (const cell of cells) {
    const list = byWeek.get(cell.week_index) ?? [];
    list.push(cell);
    byWeek.set(cell.week_index, list);
  }

  let lastMonth = "";
  return Array.from(byWeek.entries())
    .sort((a, b) => a[0] - b[0])
    .map(([week, entries]) => {
      const first = [...entries].sort((a, b) => a.weekday - b.weekday)[0];
      const month = formatter.format(new Date(`${first.date}T00:00:00`));
      const label = month !== lastMonth ? month : "";
      lastMonth = month;
      return { week, label };
    });
}

function periodLabel(period: Period, t: TFunction): string {
  switch (period) {
    case "7d":
      return t({ ko: "7일", en: "7d", ja: "7日", zh: "7天" });
    case "90d":
      return t({ ko: "90일", en: "90d", ja: "90日", zh: "90天" });
    default:
      return t({ ko: "30일", en: "30d", ja: "30日", zh: "30天" });
  }
}

function patternFillStyle(color: string, pattern: TrendPattern): CSSProperties {
  switch (pattern) {
    case "dots":
      return {
        backgroundColor: color,
        backgroundImage: "radial-gradient(circle at 2px 2px, rgba(255,255,255,0.38) 0 1.25px, transparent 1.35px)",
        backgroundSize: "7px 7px",
      };
    case "horizontal":
      return {
        backgroundColor: color,
        backgroundImage: "repeating-linear-gradient(0deg, rgba(255,255,255,0.34) 0 2px, transparent 2px 6px)",
      };
    case "cross":
      return {
        backgroundColor: color,
        backgroundImage: [
          "repeating-linear-gradient(45deg, rgba(255,255,255,0.26) 0 2px, transparent 2px 7px)",
          "repeating-linear-gradient(-45deg, rgba(255,255,255,0.18) 0 2px, transparent 2px 7px)",
        ].join(", "),
      };
    case "diagonal":
    default:
      return {
        backgroundColor: color,
        backgroundImage: "repeating-linear-gradient(135deg, rgba(255,255,255,0.34) 0 2px, transparent 2px 7px)",
      };
  }
}

function ChartTooltip({ lines }: { lines: string[] }) {
  return (
    <div className="dash-chart-tooltip dash-card-pad-compact">
      <div className="space-y-1 text-[11px] leading-4">
        {lines.map((line, index) => (
          <div key={`${index}-${line}`}>{line}</div>
        ))}
      </div>
    </div>
  );
}

export default function TokenAnalyticsSection({
  t,
  numberFormatter,
}: TokenAnalyticsSectionProps) {
  const [period, setPeriod] = useState<Period>("30d");
  const [data, setData] = useState<TokenAnalyticsResponse | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    let mounted = true;

    const load = async () => {
      const cached = analyticsCache.get(period);
      const fresh = cached ? Date.now() - cached.fetchedAt < ANALYTICS_CACHE_TTL : false;
      if (cached && mounted) setData(cached.data);
      if (fresh) return;

      if (mounted) setLoading(true);
      try {
        const next = await getTokenAnalytics(period);
        analyticsCache.set(period, { data: next, fetchedAt: Date.now() });
        if (mounted) setData(next);
      } catch {
        // Ignore transient dashboard fetch failures and keep cached state.
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
  }, [period]);

  const segments = useMemo(() => buildModelSegments(data?.receipt.models ?? []), [data]);
  const donutBackground = useMemo(() => buildDonutBackground(segments), [segments]);
  const weekLabels = useMemo(() => buildWeekLabels(data?.heatmap ?? []), [data]);
  const trendMax = useMemo(
    () => Math.max(1, ...(data?.daily.map((day) => day.total_tokens) ?? [1])),
    [data],
  );
  const topAgents = useMemo(
    () => [...(data?.receipt.agents ?? [])].sort((a, b) => b.cost - a.cost).slice(0, 8),
    [data],
  );

  return (
    <section className="space-y-4">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div className="min-w-0">
          <h2 className="text-lg font-semibold" style={{ color: "var(--th-text-heading)" }}>
            {t({
              ko: "토큰 컨트롤 센터",
              en: "Token Control Center",
              ja: "トークンコントロールセンター",
              zh: "Token 控制中心",
            })}
          </h2>
          <p className="text-sm" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "13주 활동 히트맵, 비용 흐름, 모델 분포를 한 섹션에서 봅니다",
              en: "Keep the 13-week heatmap, spend flow, and model mix in one section",
              ja: "13週ヒートマップ、コスト推移、モデル分布を一つのセクションに集約します",
              zh: "在一个专区中查看 13 周热力图、成本走势与模型分布",
            })}
          </p>
        </div>

        <div className="flex flex-wrap items-center gap-2">
          {PERIOD_OPTIONS.map((option) => (
            <button
              key={option}
              type="button"
              onClick={() => setPeriod(option)}
              className={dashboardButton.sm}
              style={
                period === option
                  ? {
                      color: "#0f172a",
                      background: "linear-gradient(135deg, #f59e0b, #fb7185)",
                    }
                  : {
                      color: "var(--th-text-muted)",
                      border: "1px solid rgba(255,255,255,0.08)",
                    }
              }
            >
              {periodLabel(option, t)}
            </button>
          ))}
          {loading && (
            <span
              className={cx(dashboardBadge.default, "font-semibold uppercase tracking-[0.18em]")}
              style={{ color: "#fbbf24", background: "rgba(245,158,11,0.12)" }}
            >
              {t({ ko: "동기화", en: "Syncing", ja: "同期中", zh: "同步中" })}
            </span>
          )}
        </div>
      </div>

      <div className="space-y-4">
        <div
          className={dashboardCard.accentHero}
          style={{
            borderColor: "rgba(245,158,11,0.22)",
            background: "linear-gradient(145deg, color-mix(in srgb, var(--th-surface) 90%, #f97316 10%), var(--th-surface))",
          }}
        >
          <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
            <MetricCard
              label={t({ ko: "총 토큰", en: "Total Tokens", ja: "総トークン", zh: "总代币" })}
              value={data ? formatTokens(data.summary.total_tokens) : "…"}
              sub={data ? data.period_label : t({ ko: "로딩 중", en: "Loading", ja: "読み込み中", zh: "加载中" })}
              accent="#f59e0b"
            />
            <MetricCard
              label={t({ ko: "API 비용", en: "API Spend", ja: "API コスト", zh: "API 成本" })}
              value={data ? formatCost(data.summary.total_cost) : "…"}
              sub={data ? `-${formatCost(data.summary.cache_discount)}` : ""}
              accent="#22c55e"
            />
            <MetricCard
              label={t({ ko: "활성 일수", en: "Active Days", ja: "稼働日数", zh: "活跃天数" })}
              value={data ? numberFormatter.format(data.summary.active_days) : "…"}
              sub={data ? `${formatTokens(data.summary.average_daily_tokens)} / day` : ""}
              accent="#38bdf8"
            />
            <MetricCard
              label={t({ ko: "피크 데이", en: "Peak Day", ja: "ピーク日", zh: "峰值日" })}
              value={data?.summary.peak_day?.date.slice(5) ?? "—"}
              sub={data?.summary.peak_day ? formatTokens(data.summary.peak_day.total_tokens) : ""}
              accent="#fb7185"
            />
          </div>
        </div>

        <div
          className={dashboardCard.standard}
          style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
        >
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div>
              <h3 className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
                {t({
                  ko: "토큰 활동 히트맵",
                  en: "Token Activity Heatmap",
                  ja: "トークン活動ヒートマップ",
                  zh: "Token 活动热力图",
                })}
              </h3>
              <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                {t({
                  ko: "최근 13주를 GitHub 스타일로 표시합니다",
                  en: "Last 13 weeks in a GitHub-style grid",
                  ja: "直近 13 週間を GitHub スタイルのグリッドで表示します",
                  zh: "用 GitHub 风格网格展示最近 13 周",
                })}
              </p>
            </div>
            <div className="flex items-center gap-1">
              {HEATMAP_COLORS.map((color, index) => (
                <span
                  key={color}
                  className="h-2.5 w-2.5 rounded-[4px] border"
                  style={{
                    background: color,
                    borderColor: index === 0 ? "rgba(148,163,184,0.12)" : "transparent",
                  }}
                />
              ))}
            </div>
          </div>

          {!data ? (
            <div className="py-12 text-center text-sm" style={{ color: "var(--th-text-muted)" }}>
              {t({ ko: "토큰 분석을 불러오는 중입니다", en: "Loading token analytics", ja: "トークン分析を読み込み中", zh: "正在加载 Token 分析" })}
            </div>
          ) : (
            <div className="mt-4 overflow-x-hidden">
              <div className="min-w-0">
                <div className="mb-2 ml-9 grid grid-cols-13 gap-1 text-[10px]" style={{ color: "var(--th-text-muted)" }}>
                  {weekLabels.map((item) => (
                    <span key={item.week} className="truncate">
                      {item.label}
                    </span>
                  ))}
                </div>

                <div className="flex gap-3">
                  <div
                    className="grid grid-rows-7 gap-1 pt-1 text-[10px]"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {["M", "", "W", "", "F", "", "S"].map((label, index) => (
                      <span key={`${label}-${index}`} className="h-3 leading-3">
                        {label}
                      </span>
                    ))}
                  </div>

                  <div className="grid grid-flow-col grid-rows-7 gap-1">
                    {data.heatmap.map((cell) => (
                      <div
                        key={cell.date}
                        className="group relative h-3 w-3 outline-none"
                        role="img"
                        tabIndex={0}
                        aria-label={`${cell.date}, ${formatTokens(cell.total_tokens)} tokens, ${formatCost(cell.cost)}`}
                      >
                        <ChartTooltip
                          lines={[
                            cell.date,
                            `${formatTokens(cell.total_tokens)} tokens`,
                            formatCost(cell.cost),
                          ]}
                        />
                        <span
                          className="block h-3 w-3 rounded-[4px] border transition-transform group-hover:scale-110 group-focus-within:scale-110"
                          style={{
                            background: cell.future ? "rgba(148,163,184,0.04)" : HEATMAP_COLORS[cell.level] ?? HEATMAP_COLORS[0],
                            borderColor: cell.future ? "rgba(148,163,184,0.05)" : "rgba(255,255,255,0.04)",
                            opacity: cell.future ? 0.35 : 1,
                          }}
                        />
                      </div>
                    ))}
                  </div>
                </div>
              </div>
            </div>
          )}
        </div>

        <DailyTrendCard daily={data?.daily ?? []} trendMax={trendMax} t={t} />
      </div>

      <div className="grid gap-4 lg:grid-cols-[minmax(0,0.9fr)_minmax(0,1.1fr)]">
        <ModelDistributionCard
          t={t}
          segments={segments}
          donutBackground={donutBackground}
          totalTokens={data?.summary.total_tokens ?? 0}
        />
        <AgentSpendCard
          t={t}
          agents={topAgents}
          numberFormatter={numberFormatter}
        />
      </div>
    </section>
  );
}

function MetricCard({
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
    <div
      className={dashboardCard.nested}
      style={{
        borderColor: `${accent}26`,
        background: "rgba(15,23,42,0.16)",
      }}
    >
      <div className="text-[11px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
        {label}
      </div>
      <div className="mt-2 text-2xl font-black tracking-tight" style={{ color: accent }}>
        {value}
      </div>
      <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
        {sub}
      </div>
    </div>
  );
}

function DailyTrendCard({
  daily,
  trendMax,
  t,
}: {
  daily: TokenAnalyticsDailyPoint[];
  trendMax: number;
  t: TFunction;
}) {
  const legend: TrendLegendItem[] = [
    { key: "input_tokens", color: "#38bdf8", pattern: "diagonal", label: t({ ko: "입력", en: "Input", ja: "入力", zh: "输入" }) },
    { key: "output_tokens", color: "#f97316", pattern: "dots", label: t({ ko: "출력", en: "Output", ja: "出力", zh: "输出" }) },
    { key: "cache_read_tokens", color: "#22c55e", pattern: "horizontal", label: t({ ko: "캐시 읽기", en: "Cache Read", ja: "キャッシュ読取", zh: "缓存读取" }) },
    { key: "cache_creation_tokens", color: "#a855f7", pattern: "cross", label: t({ ko: "캐시 쓰기", en: "Cache Write", ja: "キャッシュ書込", zh: "缓存写入" }) },
  ];

  return (
    <div
      className={dashboardCard.standard}
      style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h3 className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {t({
              ko: "일별 토큰 추이",
              en: "Daily Token Trend",
              ja: "日次トークン推移",
              zh: "每日 Token 趋势",
            })}
          </h3>
          <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "입력, 출력, 캐시 읽기/쓰기를 스택 바 형태로 봅니다",
              en: "Stacked bars for input, output, cache read, and cache write",
              ja: "入力・出力・キャッシュ読取/書込を積み上げバーで表示します",
              zh: "用堆叠柱显示输入、输出、缓存读写",
            })}
          </p>
        </div>
        <div className="flex flex-wrap gap-2 text-[11px]">
          {legend.map((item) => (
            <span key={item.key} className="inline-flex items-center gap-1.5" style={{ color: "var(--th-text-muted)" }}>
              <span
                className="h-3.5 w-3.5 rounded-lg border"
                style={{
                  ...patternFillStyle(item.color, item.pattern),
                  borderColor: "rgba(255,255,255,0.12)",
                }}
              />
              {item.label}
            </span>
          ))}
        </div>
      </div>

      {daily.length === 0 ? (
        <div className="py-10 text-center text-sm" style={{ color: "var(--th-text-muted)" }}>
          {t({ ko: "표시할 토큰 추이가 없습니다", en: "No token trend to show", ja: "表示する推移がありません", zh: "暂无可显示的趋势" })}
        </div>
      ) : (
        <div className="mt-4 overflow-x-hidden">
          <div className="min-w-0">
            <div className="flex h-44 items-end gap-px sm:gap-1.5">
              {daily.map((day, index) => {
                const segments = legend
                  .map((item) => ({
                    ...item,
                    value: day[item.key],
                  }))
                  .filter((segment) => segment.value > 0);
                const totalHeight = Math.max(
                  8,
                  Math.round((day.total_tokens / trendMax) * TREND_PLOT_HEIGHT_PX),
                );
                const breakdown = segments.map(
                  (segment) => `${segment.label} ${formatTokens(segment.value)}`,
                );
                const compactLabel = index === 0 || index === daily.length - 1 || index % 5 === 0;

                return (
                  <div
                    key={day.date}
                    className="group relative flex w-2 shrink-0 flex-col items-center gap-1 outline-none sm:min-w-0 sm:flex-1 sm:gap-2"
                    tabIndex={0}
                    role="img"
                    aria-label={[
                      day.date,
                      `${formatTokens(day.total_tokens)} total tokens`,
                      formatCost(day.cost),
                      ...breakdown,
                    ].join(", ")}
                  >
                    <ChartTooltip
                      lines={[
                        day.date,
                        `${formatTokens(day.total_tokens)} tokens`,
                        formatCost(day.cost),
                        ...breakdown,
                      ]}
                    />
                    <div className="flex h-36 w-full items-end">
                      <div
                        className="flex w-full min-w-[6px] flex-col-reverse overflow-hidden rounded-t-xl border sm:min-w-[10px]"
                        style={{
                          height: totalHeight,
                          minHeight: 8,
                          maxHeight: TREND_PLOT_HEIGHT_PX,
                          borderColor: "rgba(255,255,255,0.06)",
                          background: "rgba(255,255,255,0.03)",
                        }}
                      >
                        {segments.map((segment, index) => (
                          <div
                            key={`${day.date}-${segment.color}-${index}`}
                            style={{
                              height: `${(segment.value / day.total_tokens) * 100}%`,
                              ...patternFillStyle(segment.color, segment.pattern),
                            }}
                          />
                        ))}
                      </div>
                    </div>
                    <span
                      className="min-h-[1.8rem] text-center text-[9px] leading-3 sm:min-h-0 sm:text-[10px]"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      <span className="hidden sm:inline">{day.date.slice(5)}</span>
                      <span className="sm:hidden">{compactLabel ? day.date.slice(5) : ""}</span>
                    </span>
                  </div>
                );
              })}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

function ModelDistributionCard({
  t,
  segments,
  donutBackground,
  totalTokens,
}: {
  t: TFunction;
  segments: ModelSegment[];
  donutBackground: string;
  totalTokens: number;
}) {
  return (
    <div
      className={dashboardCard.standard}
      style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h3 className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {t({
              ko: "모델 분포",
              en: "Model Distribution",
              ja: "モデル分布",
              zh: "模型分布",
            })}
          </h3>
          <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "Claude/Codex 모델이 토큰을 어떻게 나눠 쓰는지 확인합니다",
              en: "See how Claude and Codex models split token volume",
              ja: "Claude/Codex モデルのトークン構成を確認します",
              zh: "查看 Claude/Codex 模型如何分摊 Token 量",
            })}
          </p>
        </div>
        <span className={dashboardBadge.large} style={{ color: "#f59e0b", background: "rgba(245,158,11,0.12)" }}>
          {formatTokens(totalTokens)}
        </span>
      </div>

      {segments.length === 0 ? (
        <div className="py-10 text-center text-sm" style={{ color: "var(--th-text-muted)" }}>
          {t({ ko: "모델 분포 데이터가 없습니다", en: "No model distribution data", ja: "モデル分布データがありません", zh: "暂无模型分布数据" })}
        </div>
      ) : (
        <div className="mt-5 grid gap-5 md:grid-cols-[180px_minmax(0,1fr)] md:items-center">
          <div className="mx-auto flex w-full max-w-[180px] items-center justify-center">
            <div
              className="relative h-40 w-40 rounded-full"
              style={{ background: donutBackground }}
            >
              <div
                className="absolute inset-[18%] rounded-full border"
                style={{
                  background: "color-mix(in srgb, var(--th-surface) 88%, #0f172a 12%)",
                  borderColor: "rgba(255,255,255,0.06)",
                }}
              />
              <div className="absolute inset-0 flex flex-col items-center justify-center text-center">
                <div className="text-[11px] uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
                  Mix
                </div>
                <div className="mt-1 text-xl font-black" style={{ color: "var(--th-text)" }}>
                  {segments.length}
                </div>
              </div>
            </div>
          </div>

          <div className="space-y-2">
            {segments.map((segment) => (
              <div
                key={segment.id}
                className={dashboardCard.nestedCompact}
                style={{ borderColor: "rgba(255,255,255,0.06)", background: "var(--th-bg-surface)" }}
              >
                <div className="flex items-center justify-between gap-3">
                  <div className="min-w-0">
                    <div className="flex items-center gap-2">
                      <span className="h-2.5 w-2.5 rounded-full" style={{ background: segment.color }} />
                      <span className="truncate text-sm font-semibold" style={{ color: "var(--th-text)" }}>
                        {segment.label}
                      </span>
                    </div>
                    <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                      {segment.provider}
                    </div>
                  </div>
                  <div className="text-right">
                    <div className="text-sm font-bold" style={{ color: "var(--th-text)" }}>
                      {segment.percentage.toFixed(1)}%
                    </div>
                    <div className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                      {formatTokens(segment.tokens)}
                    </div>
                  </div>
                </div>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

function AgentSpendCard({
  t,
  agents,
  numberFormatter,
}: {
  t: TFunction;
  agents: ReceiptSnapshotAgentShare[];
  numberFormatter: Intl.NumberFormat;
}) {
  const maxCost = Math.max(0.01, ...agents.map((agent) => agent.cost));

  return (
    <div
      className={dashboardCard.standard}
      style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h3 className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {t({
              ko: "에이전트별 비용 비교",
              en: "Agent Cost Comparison",
              ja: "エージェント別コスト比較",
              zh: "按代理比较成本",
            })}
          </h3>
          <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "상위 에이전트의 토큰 소비와 비용을 함께 봅니다",
              en: "Compare token volume and spend for the busiest agents",
              ja: "主要エージェントのトークン量とコストを並べて確認します",
              zh: "对比主要代理的 Token 量与成本",
            })}
          </p>
        </div>
      </div>

      {agents.length === 0 ? (
        <div className="py-10 text-center text-sm" style={{ color: "var(--th-text-muted)" }}>
          {t({ ko: "에이전트 사용량 데이터가 없습니다", en: "No agent usage data", ja: "エージェント使用量データがありません", zh: "暂无代理使用数据" })}
        </div>
      ) : (
        <div className="mt-4 space-y-2.5">
          {agents.map((agent, index) => (
            <div key={agent.agent} className={dashboardCard.nested} style={{ borderColor: "rgba(255,255,255,0.06)", background: "var(--th-bg-surface)" }}>
              <div className="flex items-center justify-between gap-3">
                <div className="min-w-0">
                  <div className="flex items-center gap-2">
                    <span className="flex h-6 w-6 items-center justify-center rounded-full text-xs font-bold" style={{ color: "#0f172a", background: modelColor("default", index) }}>
                      {index + 1}
                    </span>
                    <span className="truncate text-sm font-semibold" style={{ color: "var(--th-text)" }}>
                      {agent.agent}
                    </span>
                  </div>
                  <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                    {formatTokens(agent.tokens)} tokens · {numberFormatter.format(Math.round(agent.percentage * 10) / 10)}%
                  </div>
                </div>
                <div className="text-right">
                  <div className="text-sm font-bold" style={{ color: "#22c55e" }}>
                    {formatCost(agent.cost)}
                  </div>
                </div>
              </div>

              <div className="mt-3 h-2 rounded-full bg-slate-800/50">
                <div
                  className="h-full rounded-full"
                  style={{
                    width: `${Math.max(6, (agent.cost / maxCost) * 100)}%`,
                    background: "linear-gradient(90deg, #22c55e, #14b8a6)",
                  }}
                />
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
