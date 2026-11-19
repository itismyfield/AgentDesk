import { useId, useLayoutEffect, useRef, useState } from "react";
import { CalendarDays } from "lucide-react";
import { Popover } from "../../kit/free/popover-clean";
import type { TFunction } from "./model";
import {
  bucketRoutineMarkers,
  describeRoutinePurpose,
  formatClock,
  formatRelative,
  formatScheduleAbsolute,
  timelineHourTicks,
  TIMELINE_FUTURE_MS,
  TIMELINE_PAST_MS,
  type RoutineMarker,
} from "./routineModel";
import { RoutineSwatch } from "./RoutineScheduleRows";

const HEAD_Y = 30;
const BASE_Y = 86;
const TICK_Y = 106;
const CHART_HEIGHT = 116;

const markerColor: Record<RoutineMarker["state"], string> = {
  past: "var(--fg-muted)",
  failed: "var(--err)",
  running: "var(--info)",
  future: "var(--accent)",
};

function useWidth<T extends HTMLElement>() {
  const ref = useRef<T>(null);
  const [width, setWidth] = useState(0);
  useLayoutEffect(() => {
    const node = ref.current;
    if (!node) return;
    const update = () => setWidth(node.clientWidth);
    update();
    const observer = new ResizeObserver(update);
    observer.observe(node);
    return () => observer.disconnect();
  }, []);
  return [ref, width] as const;
}

export function routineMarkerLabel(marker: RoutineMarker, now: number, localeTag: string, t: TFunction): string {
  const title = describeRoutinePurpose(marker.routine, t);
  const when = formatScheduleAbsolute(marker.at, now, localeTag, t);
  const relative = formatRelative(marker.at, localeTag, now) ?? "";
  const state =
    marker.state === "running"
      ? t({ ko: "실행 중", en: "Running", ja: "実行中", zh: "运行中" })
      : marker.state === "failed"
        ? t({ ko: "실패", en: "Failed", ja: "失敗", zh: "失败" })
        : marker.state === "future"
          ? relative
          : `${t({ ko: "지난 실행", en: "Past run", ja: "過去の実行", zh: "过去运行" })} · ${relative}`;
  return `${title}, ${when}, ${state}`;
}

/** Choices for markers that share one 44px column (popover on wide screens, inside the sheet on phones). */
export function RoutineOverlapList({
  markers,
  now,
  localeTag,
  t,
  onPick,
}: {
  markers: RoutineMarker[];
  now: number;
  localeTag: string;
  t: TFunction;
  onPick: (marker: RoutineMarker) => void;
}) {
  return (
    <ul className="m-0 flex list-none flex-col gap-0.5 p-0" data-testid="routine-overlap-list">
      {markers.map((marker) => {
        const [title, ...rest] = routineMarkerLabel(marker, now, localeTag, t).split(", ");
        return (
          <li key={`${marker.routine.id}-${marker.at}-${marker.state}`}>
            <button
              type="button"
              onClick={() => onPick(marker)}
              className="flex min-h-11 w-full items-center gap-2.5 rounded-[var(--r-sm)] px-2.5 py-1.5 text-left text-[var(--fg)] transition-colors hover:bg-[color-mix(in_oklch,var(--fg)_6%,transparent)] focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-[var(--accent)] motion-reduce:transition-none"
            >
              <RoutineSwatch tone={marker.state === "failed" ? "danger" : marker.state === "running" ? "info" : marker.tone} outline={marker.state === "future"} />
              <span className="flex min-w-0 flex-col">
                <strong className="text-[13px] font-medium leading-5 [overflow-wrap:anywhere]">{title}</strong>
                <small className="text-xs leading-4 text-[var(--fg-muted)]">{rest.join(" · ")}</small>
              </span>
            </button>
          </li>
        );
      })}
    </ul>
  );
}

/**
 * Four hours back and eight ahead: the last run of each routine and its next due time as markers on one line.
 * Every marker target is 44×44; markers in the same 44px column open a list instead of guessing.
 */
export function RoutineRunsTimeline({
  markers,
  now,
  t,
  localeTag,
  isMobile,
  onOpen,
  onOpenCluster,
}: {
  markers: RoutineMarker[];
  now: number;
  t: TFunction;
  localeTag: string;
  /** Below 900px the overlap list opens in the page sheet instead of a popover. */
  isMobile: boolean;
  onOpen: (routineId: string, trigger: HTMLElement) => void;
  onOpenCluster: (markers: RoutineMarker[], trigger: HTMLElement) => void;
}) {
  const id = useId();
  const [box, width] = useWidth<HTMLDivElement>();
  const W = Math.max(1, width);
  const [cluster, setCluster] = useState<{ index: number; markers: RoutineMarker[] } | null>(null);
  const anchorRef = useRef<HTMLButtonElement | null>(null);
  const x = (time: number) => (W * (time - (now - TIMELINE_PAST_MS))) / (TIMELINE_PAST_MS + TIMELINE_FUTURE_MS);
  const nowX = x(now);
  const ticks = timelineHourTicks(now, W).filter((tick) => Math.abs(x(tick) - nowX) > 26 && x(tick) > 18 && x(tick) < W - 18);
  const buckets = width > 0 ? bucketRoutineMarkers(markers, W) : [];
  const running = markers.filter((marker) => marker.state === "running");
  const upcoming = markers.find((marker) => marker.state === "future");

  return (
    <section className="flex min-w-0 flex-col gap-2" aria-labelledby={`${id}-around`} data-testid="routines-run-timeline">
      <h2 id={`${id}-around`} className="m-0 flex items-center gap-1.5 text-xs font-medium leading-4 text-[var(--fg-muted)]">
        <CalendarDays size={13} aria-hidden />
        {t({ ko: "지금 전후", en: "Around now", ja: "現在の前後", zh: "当前前后" })}
      </h2>
      <div className="flex min-w-0 flex-col gap-2.5 rounded-[var(--r-md)] border border-[var(--line-soft)] bg-[var(--bg-2)] px-4 py-3.5">
        <div className="flex flex-wrap items-center justify-between gap-x-3 gap-y-1 text-xs leading-4 text-[var(--fg-muted)]">
          <h3 className="m-0 text-xs font-medium text-[var(--fg-dim)]">
            {t({ ko: "실행 시간선", en: "Run timeline", ja: "実行タイムライン", zh: "运行时间线" })}
          </h3>
          <span className="flex items-center gap-3" aria-hidden>
            <span className="inline-flex items-center gap-1.5">
              <span className="inline-block h-1.5 w-1.5 rounded-[1px] bg-[var(--fg-muted)]" />
              {t({ ko: "지난 실행", en: "Past", ja: "過去", zh: "过去" })}
            </span>
            <span className="inline-flex items-center gap-1.5">
              <span className="inline-block h-2 w-2 rounded-full border-[1.5px] border-[var(--accent)]" />
              {t({ ko: "다음 실행", en: "Next", ja: "次回", zh: "下次" })}
            </span>
          </span>
        </div>
        <div ref={box} className="relative min-w-0" style={{ height: CHART_HEIGHT }}>
          {width > 0 ? (
            <svg width={W} height={CHART_HEIGHT} viewBox={`0 0 ${W} ${CHART_HEIGHT}`} aria-hidden className="block overflow-visible">
              <rect x={0} y={12} width={Math.max(0, nowX)} height={BASE_Y - 12} className="fill-[color-mix(in_oklch,var(--fg)_4%,transparent)]" />
              <line x1={0} x2={W} y1={BASE_Y} y2={BASE_Y} className="stroke-[var(--line)]" strokeWidth={1} />
              {ticks.map((tick) => (
                <g key={tick}>
                  <line x1={x(tick)} x2={x(tick)} y1={BASE_Y} y2={BASE_Y + 4} className="stroke-[var(--line)]" strokeWidth={1} />
                  <text x={x(tick)} y={TICK_Y} textAnchor="middle" className="fill-[var(--fg-faint)] text-[11px] tabular-nums">
                    {formatClock(tick)}
                  </text>
                </g>
              ))}
              {markers.map((marker) => {
                const mx = x(marker.at);
                const color = markerColor[marker.state];
                return (
                  <g key={`${marker.routine.id}-${marker.at}-${marker.state}`} data-state={marker.state} style={{ color }}>
                    <line
                      x1={mx}
                      x2={mx}
                      y1={HEAD_Y}
                      y2={BASE_Y}
                      stroke="currentColor"
                      strokeWidth={1}
                      strokeDasharray={marker.state === "future" ? "2 3" : undefined}
                      opacity={marker.state === "running" ? 0.9 : 0.5}
                    />
                    {marker.state === "future" ? (
                      <circle cx={mx} cy={HEAD_Y} r={3.5} stroke="currentColor" strokeWidth={1.5} className="fill-[var(--bg-2)]" />
                    ) : marker.state === "running" ? (
                      <circle cx={mx} cy={HEAD_Y} r={4} fill="currentColor" className="motion-safe:animate-pulse" />
                    ) : (
                      <rect x={mx - 3} y={HEAD_Y - 3} width={6} height={6} rx={1} fill="currentColor" />
                    )}
                  </g>
                );
              })}
              <line x1={nowX} x2={nowX} y1={10} y2={BASE_Y} className="stroke-[var(--accent)]" strokeWidth={1.5} />
              <text x={nowX} y={TICK_Y} textAnchor="middle" className="fill-[var(--accent)] text-[11px] font-medium">
                {t({ ko: "지금", en: "Now", ja: "現在", zh: "现在" })}
              </text>
            </svg>
          ) : null}
          {buckets.map((bucket) => {
            const single = bucket.markers.length === 1;
            const style = { left: bucket.center - 22, top: HEAD_Y - 22 };
            const className =
              "absolute grid h-11 w-11 place-items-center rounded-[var(--r-sm)] transition-colors hover:bg-[color-mix(in_oklch,var(--fg)_7%,transparent)] focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-[var(--accent)] motion-reduce:transition-none";
            return single ? (
              <button
                key={`b-${bucket.index}`}
                type="button"
                data-testid="routine-marker"
                className={className}
                style={style}
                aria-label={routineMarkerLabel(bucket.markers[0], now, localeTag, t)}
                title={routineMarkerLabel(bucket.markers[0], now, localeTag, t)}
                onClick={(event) => onOpen(bucket.markers[0].routine.id, event.currentTarget)}
              />
            ) : (
              <button
                key={`b-${bucket.index}`}
                type="button"
                data-testid="routine-marker-cluster"
                className={className}
                style={style}
                aria-haspopup="dialog"
                aria-expanded={isMobile ? undefined : cluster?.index === bucket.index}
                aria-label={t({
                  ko: `가까운 실행 ${bucket.markers.length}개 목록 열기`,
                  en: `Open the list of ${bucket.markers.length} nearby runs`,
                  ja: `近い実行 ${bucket.markers.length} 件の一覧を開く`,
                  zh: `打开相近的 ${bucket.markers.length} 次运行列表`,
                })}
                onClick={(event) => {
                  if (isMobile) {
                    onOpenCluster(bucket.markers, event.currentTarget);
                    return;
                  }
                  anchorRef.current = event.currentTarget;
                  setCluster({ index: bucket.index, markers: bucket.markers });
                }}
              >
                <span
                  aria-hidden
                  className="absolute right-0.5 top-0 grid h-4 min-w-4 place-items-center rounded-full border border-[var(--line)] bg-[var(--bg-2)] px-1 text-[10px] font-medium leading-none text-[var(--fg)]"
                >
                  {bucket.markers.length}
                </span>
              </button>
            );
          })}
        </div>
        <p
          className="m-0 flex min-w-0 items-center gap-1.5 text-[13px] leading-5 text-[var(--fg-muted)]"
          role="status"
          data-testid="routines-timeline-readout"
        >
          {running.length ? (
            <>
              <i aria-hidden className="inline-block h-[7px] w-[7px] shrink-0 rounded-full bg-[var(--info)] motion-safe:animate-pulse" />
              <strong className="min-w-0 truncate font-medium text-[var(--fg)]">
                {running.map((marker) => describeRoutinePurpose(marker.routine, t)).join(", ")}
              </strong>
              <span className="shrink-0">{t({ ko: "실행 중", en: "running", ja: "実行中", zh: "运行中" })}</span>
            </>
          ) : upcoming ? (
            <>
              <span className="shrink-0">{t({ ko: "다음 실행", en: "Next up", ja: "次回実行", zh: "下次运行" })}</span>
              <strong className="min-w-0 truncate font-medium text-[var(--fg)]">{describeRoutinePurpose(upcoming.routine, t)}</strong>
              <span className="shrink-0 tabular-nums">
                {formatScheduleAbsolute(upcoming.at, now, localeTag, t)} · {formatRelative(upcoming.at, localeTag, now)}
              </span>
            </>
          ) : (
            <span className="[word-break:keep-all]">
              {t({
                ko: "앞으로 8시간 안에 예정된 실행이 없어요",
                en: "No runs are scheduled in the next 8 hours",
                ja: "今後8時間に予定された実行はありません",
                zh: "未来 8 小时内没有计划运行",
              })}
            </span>
          )}
        </p>
        <Popover
          open={!isMobile && cluster !== null}
          onClose={() => setCluster(null)}
          anchor={anchorRef}
          side="bottom"
          align="center"
          width={280}
          title={t({ ko: "겹친 실행", en: "Overlapping runs", ja: "重なった実行", zh: "重叠的运行" })}
        >
          {cluster ? (
            <RoutineOverlapList
              markers={cluster.markers}
              now={now}
              localeTag={localeTag}
              t={t}
              onPick={(marker) => {
                const anchor = anchorRef.current;
                setCluster(null);
                if (anchor) onOpen(marker.routine.id, anchor);
              }}
            />
          ) : null}
        </Popover>
      </div>
    </section>
  );
}
