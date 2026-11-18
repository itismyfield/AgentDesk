import {
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type CSSProperties,
  type KeyboardEvent,
  type PointerEvent as ReactPointerEvent,
} from "react";
import { ChevronLeft, ChevronRight } from "lucide-react";
import {
  Bar,
  BarChart,
  CartesianGrid,
  Rectangle,
  ReferenceLine,
  ResponsiveContainer,
  XAxis,
  YAxis,
  type BarShapeProps,
} from "recharts";
import { Button } from "../../kit/free/button-clean";
import type { TokenAnalyticsDailyPoint } from "../../types";
import type { TFunction } from "../dashboard/model";
import {
  DAILY_CHART_RIGHT,
  DAILY_CHART_Y_AXIS,
  dailyChartMinWidth,
  dailyColumn,
  dailyIndexAt,
  formatCompactDate,
  formatCurrency,
  formatDateLabel,
  formatTokens,
  msg,
  stepDailySelection,
  type DailySeriesDescriptor,
  type DailyStep,
} from "./statsModel";

const NUMERIC_STYLE = {
  fontFamily: "var(--font-mono)",
  fontVariantNumeric: "tabular-nums",
  fontFeatureSettings: '"tnum" 1',
} as const;

// Readout step buttons stay 44×44 on every pointer (kit readout rule).
const STEP_BUTTON_STYLE = { "--aui-button-clean-h": "44px" } as CSSProperties;

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
  const count = daily.length;
  const [selected, setSelected] = useState<number | null>(null);
  const [overflowing, setOverflowing] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);
  const plotRef = useRef<HTMLDivElement>(null);
  const press = useRef<{ x: number; index: number; moved: boolean } | null>(null);
  const readId = useId();
  const firstDate = daily[0]?.date ?? "";

  // A new range starts unselected; a refresh of the same range keeps the day when it still exists.
  useEffect(() => setSelected(null), [firstDate]);
  useEffect(() => {
    setSelected((current) => (current !== null && current >= count ? null : current));
  }, [count]);

  useLayoutEffect(() => {
    const scroller = scrollRef.current;
    if (!scroller) return;
    const update = () => setOverflowing(scroller.scrollWidth > scroller.clientWidth + 1);
    update();
    const observer = new ResizeObserver(update);
    observer.observe(scroller);
    return () => observer.disconnect();
  }, [count]);

  const chartData = daily.map((day) => ({
    ...day,
    fullDate: formatDateLabel(day.date, localeTag),
    label: formatCompactDate(day.date),
  }));
  const minWidth = dailyChartMinWidth(count);

  const reveal = (index: number) => {
    const scroller = scrollRef.current;
    const plot = plotRef.current;
    if (!scroller || !plot || scroller.scrollWidth <= scroller.clientWidth) return;
    const column = dailyColumn(index, plot.clientWidth, count);
    if (column.left < scroller.scrollLeft) {
      scroller.scrollLeft = Math.max(0, column.left - DAILY_CHART_Y_AXIS);
    } else if (column.left + column.width > scroller.scrollLeft + scroller.clientWidth) {
      scroller.scrollLeft = column.left + column.width + DAILY_CHART_RIGHT - scroller.clientWidth;
    }
  };
  const choose = (next: number | null) => {
    setSelected(next);
    if (next !== null) reveal(next);
  };
  const step = (kind: DailyStep) => choose(stepDailySelection(selected, kind, count));

  const indexAt = (clientX: number) => {
    const plot = plotRef.current;
    if (!plot) return -1;
    const rect = plot.getBoundingClientRect();
    return dailyIndexAt(clientX - rect.left, rect.width, count);
  };
  const onPointerDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.pointerType === "mouse") return;
    press.current = { x: event.clientX, index: indexAt(event.clientX), moved: false };
  };
  const onPointerMove = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.pointerType === "mouse") {
      const index = indexAt(event.clientX);
      if (index >= 0) setSelected(index);
      return;
    }
    const start = press.current;
    if (!start) return;
    if (Math.abs(event.clientX - start.x) > 6) start.moved = true;
    if (start.moved) {
      const index = indexAt(event.clientX);
      if (index >= 0) setSelected(index);
    }
  };
  const onPointerUp = (event: ReactPointerEvent<HTMLDivElement>) => {
    const start = press.current;
    press.current = null;
    if (event.pointerType === "mouse") {
      const index = indexAt(event.clientX);
      if (index >= 0) setSelected(index);
      return;
    }
    if (!start || start.moved || start.index < 0) return;
    setSelected((current) => (current === start.index ? null : start.index));
  };
  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    const kind: DailyStep | null =
      event.key === "ArrowLeft" ? "prev"
        : event.key === "ArrowRight" ? "next"
          : event.key === "Home" ? "first"
            : event.key === "End" ? "last"
              : event.key === "Escape" && selected !== null ? "clear"
                : null;
    if (!kind || event.altKey || event.metaKey || event.ctrlKey) return;
    event.preventDefault();
    step(kind);
  };

  const picked = selected !== null ? chartData[selected] : null;

  return (
    <>
      {count > 7 && <p className="mb-2 text-xs leading-5 text-[var(--fg-muted)]">{t(msg("날짜가 가려지면 차트를 좌우로 움직여 확인하세요.", "Scroll horizontally to see dates outside the view."))}</p>}
      <div
        ref={scrollRef}
        data-testid="stats-daily-token-scroll"
        role="group"
        aria-label={t(msg(
          "일별 토큰 차트: 좌우 방향키·Home·End로 날짜를 고르고 Esc로 해제해요.",
          "Daily token chart: pick a day with the arrow keys, Home and End; Esc clears it.",
        ))}
        aria-describedby={readId}
        tabIndex={0}
        onKeyDown={onKeyDown}
        className="overflow-x-auto overflow-y-hidden rounded-[var(--r-sm)] overscroll-x-contain"
      >
        <div
          ref={plotRef}
          className="h-[232px] cursor-crosshair select-none"
          role="img"
          aria-label={t(msg("일별 토큰 구성을 보여주는 누적 막대 차트", "Stacked bar chart of daily token composition."))}
          style={{ minWidth: `${minWidth}px`, touchAction: overflowing ? "pan-x pan-y" : "pan-y" }}
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          onPointerCancel={() => { press.current = null; }}
        >
          <ResponsiveContainer width="100%" height="100%">
            <BarChart data={chartData} margin={{ top: 8, right: DAILY_CHART_RIGHT, bottom: 0, left: 0 }} barCategoryGap="18%">
              <CartesianGrid stroke="var(--line-soft)" strokeDasharray="3 6" vertical={false} />
              <XAxis
                dataKey="label"
                interval="preserveStartEnd"
                minTickGap={18}
                tick={{ fill: "var(--fg-muted)", fontSize: 10 }}
                tickLine={false}
                axisLine={{ stroke: "var(--line-soft)" }}
              />
              <YAxis
                width={DAILY_CHART_Y_AXIS}
                tickFormatter={formatTokens}
                tick={{ fill: "var(--fg-muted)", fontSize: 10 }}
                tickLine={false}
                axisLine={false}
              />
              {picked ? <ReferenceLine x={picked.label} stroke="var(--fg-faint)" strokeDasharray="3 3" ifOverflow="visible" /> : null}
              {series.map((item, index) => (
                <Bar
                  key={item.key}
                  dataKey={item.key}
                  fill={item.color}
                  name={item.label}
                  stackId="tokens"
                  radius={index === series.length - 1 ? [4, 4, 0, 0] : [0, 0, 0, 0]}
                  isAnimationActive={false}
                  shape={(props: BarShapeProps) => (
                    <Rectangle {...props} fillOpacity={selected === null || selected === props.index ? 1 : 0.32} />
                  )}
                />
              ))}
            </BarChart>
          </ResponsiveContainer>
        </div>
      </div>
      <div className="mt-2 flex min-h-11 items-center gap-2" data-testid="stats-daily-readout">
        <Button
          variant="secondary"
          icon={<ChevronLeft />}
          aria-label={t(msg("이전 날짜 읽기", "Read previous day"))}
          data-testid="stats-daily-prev"
          style={STEP_BUTTON_STYLE}
          onClick={() => step("prev")}
        />
        <div
          id={readId}
          aria-live="polite"
          data-testid="stats-daily-readout-text"
          className="min-w-0 flex-1 text-center text-xs leading-5 text-[var(--fg-muted)] [word-break:keep-all]"
        >
          {picked ? (
            <>
              <span className="block" style={NUMERIC_STYLE}>
                <strong className="font-semibold text-[var(--fg)]">{picked.fullDate}</strong>
                {" · "}
                <span className="whitespace-nowrap">{formatTokens(picked.total_tokens)} tokens</span>
                {" · "}
                <span className="whitespace-nowrap">{formatCurrency(picked.cost)}</span>
              </span>
              <span className="mt-0.5 flex flex-wrap justify-center gap-x-3 gap-y-0.5" style={NUMERIC_STYLE}>
                {series.map((item) => (
                  <span key={item.key} className="inline-flex items-center gap-1 whitespace-nowrap">
                    <span aria-hidden="true" className="size-2 shrink-0 rounded-[2px]" style={{ background: item.color }} />
                    {item.label} {formatTokens(picked[item.key])}
                  </span>
                ))}
              </span>
            </>
          ) : (
            t(msg(
              "막대를 누르거나 가로로 훑고, 버튼·방향키로 날짜를 옮겨요.",
              "Tap or scrub the bars, or move between days with the buttons and arrow keys.",
            ))
          )}
        </div>
        <Button
          variant="secondary"
          icon={<ChevronRight />}
          aria-label={t(msg("다음 날짜 읽기", "Read next day"))}
          data-testid="stats-daily-next"
          style={STEP_BUTTON_STYLE}
          onClick={() => step("next")}
        />
      </div>
    </>
  );
}
