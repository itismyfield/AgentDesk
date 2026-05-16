import {
  Bar,
  BarChart,
  CartesianGrid,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts";
import type { TokenAnalyticsDailyPoint } from "../../types";
import type { TFunction } from "../dashboard/model";
import {
  formatCompactDate,
  formatDateLabel,
  formatTokens,
  msg,
  type DailySeriesDescriptor,
} from "./statsModel";

const NUMERIC_STYLE = {
  fontFamily: "var(--font-mono)",
  fontVariantNumeric: "tabular-nums",
  fontFeatureSettings: '"tnum" 1',
} as const;

function DailyTokenTooltip({
  active,
  payload,
}: {
  active?: boolean;
  payload?: Array<{
    color?: string;
    dataKey?: string | number;
    name?: string;
    value?: number;
    payload?: { fullDate?: string; total_tokens?: number };
  }>;
}) {
  if (!active || !payload?.length) return null;
  const point = payload[0]?.payload;
  const rows = payload.filter((item) => Number(item.value ?? 0) > 0);

  return (
    <div
      className="rounded-xl border px-3 py-2 text-[11px] shadow-xl"
      style={{
        background: "var(--th-card-bg)",
        borderColor: "var(--th-border-subtle)",
        color: "var(--th-text)",
        ...NUMERIC_STYLE,
      }}
    >
      <div className="mb-1 font-semibold" style={{ color: "var(--th-text-heading)" }}>
        {point?.fullDate}
      </div>
      <div className="mb-2" style={{ color: "var(--th-text-muted)" }}>
        {formatTokens(point?.total_tokens ?? 0)} tokens
      </div>
      <div className="space-y-1">
        {rows.map((item) => (
          <div key={String(item.dataKey)} className="flex items-center justify-between gap-4">
            <span className="inline-flex min-w-0 items-center gap-1.5">
              <span className="h-2 w-2 shrink-0 rounded-[2px]" style={{ background: item.color }} />
              <span className="truncate">{item.name}</span>
            </span>
            <span style={{ color: "var(--th-text-heading)" }}>{formatTokens(item.value ?? 0)}</span>
          </div>
        ))}
      </div>
    </div>
  );
}

export default function DailyTokenCompositionChart({
  t,
  daily,
  localeTag,
  series,
}: {
  t: TFunction;
  daily: TokenAnalyticsDailyPoint[];
  localeTag: string;
  series: DailySeriesDescriptor[];
}) {
  const chartData = daily.map((day) => ({
    ...day,
    fullDate: formatDateLabel(day.date, localeTag),
    label: formatCompactDate(day.date),
  }));
  const chartMinWidth = Math.max(420, daily.length * 34);

  return (
    <div className="overflow-x-auto overflow-y-hidden">
      <div
        className="h-[246px] pb-1"
        role="img"
        aria-label={t(
          msg(
            "일별 토큰 구성을 보여주는 누적 막대 차트",
            "Stacked bar chart of daily token composition.",
          ),
        )}
        style={{ minWidth: `${chartMinWidth}px` }}
      >
        <ResponsiveContainer width="100%" height="100%">
          <BarChart data={chartData} margin={{ top: 8, right: 12, bottom: 0, left: 0 }}>
            <CartesianGrid
              stroke="var(--th-border-subtle)"
              strokeDasharray="3 6"
              vertical={false}
            />
            <XAxis
              dataKey="label"
              interval="preserveStartEnd"
              minTickGap={18}
              tick={{ fill: "var(--th-text-muted)", fontSize: 10 }}
              tickLine={false}
              axisLine={{ stroke: "var(--th-border-subtle)" }}
            />
            <YAxis
              width={46}
              tickFormatter={formatTokens}
              tick={{ fill: "var(--th-text-muted)", fontSize: 10 }}
              tickLine={false}
              axisLine={false}
            />
            <Tooltip
              content={<DailyTokenTooltip />}
              cursor={{ fill: "color-mix(in srgb, var(--th-overlay-subtle) 75%, transparent)" }}
              wrapperStyle={{ outline: "none" }}
            />
            {series.map((item, index) => (
              <Bar
                key={item.key}
                dataKey={item.key}
                fill={item.color}
                name={item.label}
                stackId="tokens"
                radius={index === series.length - 1 ? [6, 6, 0, 0] : [0, 0, 0, 0]}
                isAnimationActive={false}
              />
            ))}
          </BarChart>
        </ResponsiveContainer>
      </div>
    </div>
  );
}
