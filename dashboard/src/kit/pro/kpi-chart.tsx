import { useEffect, useId, useLayoutEffect, useRef, useState, type KeyboardEvent, type PointerEvent } from 'react';

// Clean-room implementation from clean-specs/kpi-chart (11/17 revision).
// Non-finite values are gaps; values past the label count are ignored.
export interface KpiSeries {
  id: string;
  label: string;
  values: (number | null)[];
  color: string;
}

export interface KpiChartProps {
  title: string;
  /** Already formatted by the host; never recalculated from a selected point. */
  value: string;
  delta?: number;
  labels: string[];
  tooltipLabels?: string[];
  series: KpiSeries[];
  format?: (value: number) => string;
  footer?: string;
  /** Short notice centred in an empty plot (no finite values, or all zero on a non-percentage axis). */
  emptyLabel?: string;
  /** Idle selection, rounded and clamped to the available points. Non-finite values mean no idle selection. */
  defaultIndex?: number;
  className?: string;
}

const styles = `
.aui-kpi-chart {
  --aui-kpi-chart-surface: var(--aui-bg-1, #ffffff);
  --aui-kpi-chart-fg: var(--aui-fg, #171717);
  --aui-kpi-chart-dim: var(--aui-fg-dim, #525252);
  --aui-kpi-chart-muted: var(--aui-fg-muted, #8f8f8f);
  --aui-kpi-chart-grid: var(--aui-line-soft, #f0f0f0);
  --aui-kpi-chart-border: var(--aui-line, #ebebeb);
  --aui-kpi-chart-guide: var(--aui-kpi-chart-guide-color, #d9d9d9);
  --aui-kpi-chart-focus: var(--aui-kpi-chart-focus-color, var(--aui-focus, rgb(0 96 226 / .7)));
  --aui-kpi-chart-popup: var(--aui-bg-2, #ffffff);
  --aui-kpi-chart-popup-border: var(--aui-line, #ebebeb);
  --aui-kpi-chart-shadow: var(--aui-shadow-sm, 0 1px 2px #00000008, 0 8px 24px #00000009);
  --aui-kpi-chart-popup-shadow: var(--aui-shadow-md, 0 8px 24px -6px #0000001f, 0 2px 6px #0000000a);
  --aui-kpi-chart-ok: var(--aui-ok, #1e8a4f);
  --aui-kpi-chart-err: var(--aui-err, #f04438);
  --aui-kpi-chart-ease: var(--aui-ease, cubic-bezier(.22,1,.36,1));
  --aui-kpi-chart-r: var(--aui-r-lg, 16px);
  width: 100%; max-width: 460px; min-width: 0; container-type: inline-size;
  color: var(--aui-kpi-chart-fg); font: 13px/1.5 var(--aui-font-sans, Inter, ui-sans-serif, system-ui, sans-serif);
  word-break: keep-all; overflow-wrap: anywhere; font-variant-numeric: tabular-nums;
}
:where(.dark, [data-theme="dark"]) .aui-kpi-chart {
  --aui-kpi-chart-surface: var(--aui-bg-1, #232326);
  --aui-kpi-chart-fg: var(--aui-fg, #ededed);
  --aui-kpi-chart-dim: var(--aui-fg-dim, #c4c4c4);
  --aui-kpi-chart-muted: var(--aui-fg-muted, #8f8f8f);
  --aui-kpi-chart-grid: var(--aui-line-soft, #ffffff0f);
  --aui-kpi-chart-border: var(--aui-line, #ffffff14);
  --aui-kpi-chart-guide: var(--aui-kpi-chart-guide-color, #ffffff33);
  --aui-kpi-chart-focus: var(--aui-kpi-chart-focus-color, var(--aui-focus, rgb(77 141 255 / .8)));
  --aui-kpi-chart-popup: var(--aui-bg-2, #2a2a2d);
  --aui-kpi-chart-popup-border: var(--aui-line, #ffffff1a);
  --aui-kpi-chart-shadow: var(--aui-shadow-sm, 0 1px 2px #00000066, 0 8px 24px #00000059);
  --aui-kpi-chart-popup-shadow: var(--aui-shadow-md, 0 8px 24px -6px #00000080, 0 2px 6px #00000059, 0 0 0 .5px #ffffff0a);
  --aui-kpi-chart-ok: var(--aui-ok, #5fc88a);
  --aui-kpi-chart-err: var(--aui-err, #ff6b5f);
}
.aui-kpi-chart *, .aui-kpi-chart *::before, .aui-kpi-chart *::after { box-sizing: border-box; }
.aui-kpi-chart-card { padding: 20px 20px 18px; border: 1px solid var(--aui-kpi-chart-border); border-radius: calc(var(--aui-kpi-chart-r) + 4px); background: var(--aui-kpi-chart-surface); box-shadow: var(--aui-kpi-chart-shadow); }
.aui-kpi-chart-title { margin: 0; overflow-wrap: anywhere; color: var(--aui-kpi-chart-muted); font-size: 14px; line-height: 20px; font-weight: 400; }
.aui-kpi-chart-summary { display: flex; align-items: baseline; flex-wrap: wrap; gap: 10px; margin-top: 8px; line-height: 34px; }
.aui-kpi-chart-number { min-width: 0; overflow-wrap: anywhere; font-size: 28px; line-height: 34px; letter-spacing: -.02em; font-weight: 500; }
.aui-kpi-chart-delta { color: var(--aui-kpi-chart-ok); font-size: 14px; font-weight: 450; letter-spacing: 0; white-space: nowrap; }
.aui-kpi-chart-delta[data-negative="true"] { color: var(--aui-kpi-chart-err); }
.aui-kpi-chart-graph { display: grid; grid-template-columns: max-content minmax(0, 1fr); grid-template-rows: minmax(var(--aui-kpi-chart-plot-height, 116px), 1fr) 20px; gap: 8px 12px; margin-top: 24px; min-width: 0; }
.aui-kpi-chart-yaxis { display: flex; flex-direction: column; justify-content: space-between; margin-block: -9px; color: var(--aui-kpi-chart-muted); font-size: 12px; line-height: 18px; text-align: right; }
.aui-kpi-chart-plot-wrap { position: relative; min-width: 0; min-height: 116px; }
.aui-kpi-chart-interactive { position: relative; width: 100%; height: 100%; min-height: 116px; min-width: 44px; border-radius: 4px; touch-action: pan-y; -webkit-tap-highlight-color: transparent; cursor: crosshair; }
.aui-kpi-chart-interactive:focus-visible { outline: 2px solid var(--aui-kpi-chart-focus); outline-offset: 2px; }
.aui-kpi-chart-svg { display: block; width: 100%; height: 100%; overflow: visible; }
.aui-kpi-chart-grid { stroke: var(--aui-kpi-chart-grid); stroke-width: 1; vector-effect: non-scaling-stroke; }
.aui-kpi-chart-lines { animation: aui-kpi-chart-reveal 1100ms var(--aui-kpi-chart-ease) both; }
.aui-kpi-chart-series { fill: none; stroke-width: 2; stroke-linejoin: round; stroke-linecap: round; vector-effect: non-scaling-stroke; }
.aui-kpi-chart-guide, .aui-kpi-chart-point { position: absolute; pointer-events: none; transition: left 220ms var(--aui-kpi-chart-ease), top 220ms var(--aui-kpi-chart-ease), opacity 160ms ease; opacity: 0; }
.aui-kpi-chart-dot { position: absolute; width: 8px; height: 8px; margin: -4px 0 0 -4px; border-radius: 50%; box-shadow: 0 0 0 2px var(--aui-kpi-chart-surface); pointer-events: none; animation: aui-kpi-chart-dot-in 400ms var(--aui-kpi-chart-ease) both; }
.aui-kpi-chart-empty { position: absolute; top: 50%; left: 50%; max-width: 100%; margin: 0; padding: 0 10px; transform: translate(-50%, -50%); overflow: hidden; color: var(--aui-kpi-chart-muted); background: var(--aui-kpi-chart-surface); font-size: 13px; line-height: 18px; white-space: nowrap; text-overflow: ellipsis; }
.aui-kpi-chart-graph[data-empty] .aui-kpi-chart-interactive { cursor: default; }
.aui-kpi-chart-guide { top: -4px; bottom: 0; width: 1px; transform: translateX(-.5px); background: var(--aui-kpi-chart-guide); }
.aui-kpi-chart-point { width: 10px; height: 10px; margin: -5px 0 0 -5px; border-radius: 50%; box-shadow: 0 0 0 2px var(--aui-kpi-chart-surface); }
.aui-kpi-chart-interactive[data-selected="true"] .aui-kpi-chart-guide, .aui-kpi-chart-interactive[data-selected="true"] .aui-kpi-chart-point { opacity: 1; }
.aui-kpi-chart-xaxis { position: relative; grid-column: 2; height: 20px; color: var(--aui-kpi-chart-muted); font-size: 12px; line-height: 20px; }
.aui-kpi-chart-xaxis span { position: absolute; max-width: 34%; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.aui-kpi-chart-tooltip { position: absolute; z-index: 2; top: -12px; left: 0; min-width: 176px; width: max-content; max-width: calc(100cqw - 56px); padding: 10px 12px 12px; border: 1px solid var(--aui-kpi-chart-popup-border); border-radius: calc(var(--aui-kpi-chart-r) - 4px); background: var(--aui-kpi-chart-popup); box-shadow: var(--aui-kpi-chart-popup-shadow); pointer-events: none; opacity: 0; transform: scale(.97); transform-origin: top left; transition: left 220ms var(--aui-kpi-chart-ease), transform 220ms var(--aui-kpi-chart-ease), opacity 160ms ease; }
.aui-kpi-chart-tooltip[data-side="left"] { transform-origin: top right; }
.aui-kpi-chart-tooltip[data-visible="true"] { opacity: 1; transform: scale(1); }
.aui-kpi-chart-tooltip-heading { min-height: 18px; margin: 0 0 6px; overflow: hidden; color: var(--aui-kpi-chart-muted); font-size: 12px; line-height: 18px; font-weight: 400; white-space: nowrap; text-overflow: ellipsis; }
.aui-kpi-chart-tooltip-list { display: grid; gap: 4px; margin: 0; padding: 0; list-style: none; }
.aui-kpi-chart-tooltip-row { display: grid; grid-template-columns: 10px minmax(0, 1fr) auto; align-items: center; column-gap: 8px; font-size: 13px; line-height: 20px; }
.aui-kpi-chart-tooltip-swatch { width: 10px; height: 2px; flex: none; border-radius: 1px; }
.aui-kpi-chart-tooltip-label { min-width: 0; overflow: hidden; color: var(--aui-kpi-chart-dim); white-space: nowrap; text-overflow: ellipsis; }
.aui-kpi-chart-tooltip-value { padding-left: 16px; text-align: right; font-weight: 500; white-space: nowrap; }
.aui-kpi-chart-reading { display: none; position: relative; margin-top: 12px; padding: 12px; border: 1px solid var(--aui-kpi-chart-border); border-radius: calc(var(--aui-kpi-chart-r) - 4px); }
.aui-kpi-chart-reading-content { visibility: hidden; }
.aui-kpi-chart-reading[data-visible="true"] .aui-kpi-chart-reading-content { visibility: visible; }
.aui-kpi-chart-reading .aui-kpi-chart-tooltip-heading { min-height: 36px; white-space: normal; overflow-wrap: anywhere; }
.aui-kpi-chart-reading .aui-kpi-chart-tooltip-label { white-space: normal; overflow-wrap: anywhere; }
.aui-kpi-chart-reading-hint { position: absolute; inset: 12px; margin: 0; color: var(--aui-kpi-chart-muted); font-size: 13px; line-height: 20px; }
.aui-kpi-chart-reading[data-visible="true"] .aui-kpi-chart-reading-hint { visibility: hidden; }
.aui-kpi-chart[data-expanded-reading="true"] .aui-kpi-chart-tooltip { display: none; }
.aui-kpi-chart[data-expanded-reading="true"] .aui-kpi-chart-reading { display: block; }
.aui-kpi-chart-footer { margin: 16px 0 0; overflow-wrap: anywhere; color: var(--aui-kpi-chart-muted); font-size: 13px; line-height: 18px; }
.aui-kpi-chart-sr { position: absolute; width: 1px; height: 1px; margin: -1px; padding: 0; overflow: hidden; clip-path: inset(50%); white-space: nowrap; }
.aui-kpi-chart-demo { display: grid; gap: 12px; width: 100%; min-width: 0; max-width: 460px; }
.aui-kpi-chart-demo-controls { display: flex; flex-wrap: wrap; gap: 8px; }
.aui-kpi-chart-demo button { min-height: 44px; min-width: 44px; padding: 8px 12px; border: 1px solid var(--aui-line, #d5d5dc); border-radius: var(--aui-r-sm, 8px); background: var(--aui-bg-1, #fff); color: var(--aui-fg, #171717); font: 13px/20px var(--aui-font-sans, Inter, system-ui, sans-serif); cursor: pointer; touch-action: manipulation; }
:where(.dark, [data-theme="dark"]) .aui-kpi-chart-demo button { background: var(--aui-bg-1, #232326); color: var(--aui-fg, #ededed); border-color: var(--aui-line, #ffffff24); }
.aui-kpi-chart-demo button[aria-pressed="true"] { border-color: currentColor; font-weight: 600; }
.aui-kpi-chart-demo button:focus-visible { outline: 2px solid var(--aui-focus, rgb(0 96 226 / .7)); outline-offset: 2px; }
.aui-kpi-chart-demo-note { margin: 0; color: var(--aui-fg-muted, #8f8f8f); font: 12px/18px var(--aui-font-sans, Inter, system-ui, sans-serif); }
@container (max-width: 420px) {
  .aui-kpi-chart-tooltip { display: none; }
  .aui-kpi-chart-reading { display: block; }
}
@keyframes aui-kpi-chart-dot-in { from { opacity: 0; transform: scale(.4); } to { opacity: 1; transform: scale(1); } }
@keyframes aui-kpi-chart-reveal {
  from { clip-path: polygon(0 -10%, 0 -10%, 0 110%, 0 110%); }
  to { clip-path: polygon(0 -10%, 102% -10%, 102% 110%, 0 110%); }
}
@media (prefers-reduced-motion: reduce) {
  .aui-kpi-chart *, .aui-kpi-chart *::before, .aui-kpi-chart *::after { animation: none !important; transition: none !important; }
}
`;

const finite = (value: number | null | undefined): value is number => typeof value === 'number' && Number.isFinite(value);
const trimZeros = (text: string) => text.includes('.') ? text.replace(/\.?0+$/, '') : text;
/** Default axis/readout format: two significant digits below 1, integers below 1,000, then k/M/B. */
export const compactNumber = (value: number) => {
  const absolute = Math.abs(value);
  if (absolute < 1000) return absolute > 0 && absolute < 1 ? String(Number(value.toPrecision(2))) : String(Math.round(value) || 0);
  const [unit, suffix] = absolute < 1e6 ? [1e3, 'k'] : absolute < 1e9 ? [1e6, 'M'] : [1e9, 'B'];
  const scaled = value / unit;
  // Abbreviated integers round exact halves away from zero so −12.5k reads −13k like 12.5k reads 13k.
  return `${Math.abs(scaled) < 10 ? trimZeros(scaled.toFixed(1)) : Math.sign(scaled) * Math.round(Math.abs(scaled))}${suffix}`;
};
/** Positive nice ceiling: 2 × the first step in the 1…10 ladder that covers half of the value. */
const niceCeiling = (value: number) => {
  const half = (!Number.isFinite(value) || value < 0 ? 0 : value) / 2 || 1;
  const magnitude = 10 ** Math.floor(Math.log10(half));
  return 2 * ([1, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10].find((factor) => factor * magnitude >= half) ?? 10) * magnitude;
};
/** Returns the 100% base (1 or 100) when the formatter renders percentages, otherwise null. */
function percentBase(format: (value: number) => string) {
  try {
    const one = String(format(1)).trim(); const fifty = String(format(50)).trim();
    if (!one.endsWith('%') || !fifty.endsWith('%')) return null;
    const numeric = Number(one.replace(/[^0-9.-]/g, ''));
    return Math.abs(numeric - 100) < 0.000001 ? 1 : 100;
  } catch { return null; }
}

export function KpiChart({ title, value, delta, labels, tooltipLabels, series, format = compactNumber, footer, emptyLabel = '표시할 실행 지표가 아직 없습니다.', defaultIndex, className = '' }: KpiChartProps) {
  const id = useId();
  const [userIndex, setUserIndex] = useState<number | null>(null);
  const press = useRef<{ id: number; x: number } | null>(null);
  const card = useRef<HTMLDivElement>(null);
  const wrap = useRef<HTMLDivElement>(null);
  const tip = useRef<HTMLDivElement>(null);
  const count = labels?.length ?? 0;
  const list = series ?? [];
  // Values past the label count are ignored; non-finite entries are gaps.
  const valueAt = (item: KpiSeries, point: number) => point < count && finite(item.values[point]) ? item.values[point] as number : null;
  const values = list.flatMap((item) => item.values.slice(0, count).filter(finite));
  const base = percentBase(format);
  const hasData = count > 0 && values.length > 0 && (base !== null || values.some((amount) => amount !== 0));
  const most = Math.max(0, ...values);
  const least = Math.min(0, ...values);
  const high = base !== null ? most === 0 ? base : niceCeiling(Math.max(most, base / 10)) : niceCeiling(most);
  const low = least < 0 ? -niceCeiling(-least) : 0;
  const idle = finite(defaultIndex) && count > 0 ? Math.max(0, Math.min(count - 1, Math.round(defaultIndex))) : null;
  const selected = hasData ? userIndex === null ? idle : Math.min(userIndex, count - 1) : null;
  const visible = selected !== null;
  useEffect(() => {
    if (userIndex !== null && (!hasData || count === 0)) setUserIndex(null);
    else if (userIndex !== null && userIndex >= count) setUserIndex(count - 1);
  }, [count, hasData, userIndex]);
  const x = (point: number) => count < 2 ? 50 : point / (count - 1) * 100;
  const y = (amount: number) => (high - amount) / (high - low) * 100;
  const snapshot = (point: number) => ({
    index: point, x: x(point),
    label: tooltipLabels === undefined ? labels[point] ?? '' : tooltipLabels[point] ?? '',
    rows: list.map((item, order) => ({ ...item, order, amount: valueAt(item, point) }))
      .sort((a, b) => a.amount === null || b.amount === null ? (a.amount === null ? 1 : 0) - (b.amount === null ? 1 : 0) || a.order - b.order : b.amount - a.amount || a.order - b.order),
    points: list.map((item, order) => ({ key: `${item.id}-${order}`, color: item.color, amount: valueAt(item, point) })),
  });
  // Keep the last date and geometry mounted through the 160ms closing fade.
  const last = useRef<ReturnType<typeof snapshot> | null>(null);
  if (selected !== null) last.current = snapshot(selected);
  else if (last.current && last.current.index >= count) last.current = count ? snapshot(count - 1) : null;
  const shown = last.current ?? snapshot(0);
  const side = shown.x > 50 ? 'left' : 'right';
  const show = (amount: number | null) => amount === null ? '—' : format(amount);
  const expandedReading = list.length > 6 || list.some((item) => item.label.length > 18) || (tooltipLabels ?? labels ?? []).some((label) => label.length > 38) || values.some((amount) => format(amount).length > 10);
  const axisIndices = count ? [...new Set([0, Math.round(count / 2 - .5), count - 1])] : [];
  const longestValue = Math.max(1, ...values.map((amount) => format(amount).length));
  const guidance = '그래프를 탭하거나 방향키로 날짜를 고르면 모든 계열의 값을 확인할 수 있습니다.';
  const line = (item: KpiSeries) => {
    let drawing = false;
    return Array.from({ length: count }, (_, point) => {
      const amount = valueAt(item, point);
      if (amount === null) { drawing = false; return ''; }
      const segment = `${drawing ? 'L' : 'M'}${x(point) * 10},${y(amount) * 1.16}`;
      drawing = true;
      return segment;
    }).join(' ');
  };
  // A value with no finite neighbour on either side has no segment, so it is drawn as a standalone dot.
  const isolated = list.flatMap((item, order) => Array.from({ length: count }, (_, point) => point).filter((point) => valueAt(item, point) !== null && valueAt(item, point - 1) === null && valueAt(item, point + 1) === null).map((point) => ({ key: `${item.id}-${order}-${point}`, color: item.color, left: x(point), top: y(valueAt(item, point)!) })));
  const nearest = (event: PointerEvent<HTMLDivElement>) => {
    const bounds = event.currentTarget.getBoundingClientRect();
    return Math.round(Math.max(0, Math.min(1, (event.clientX - bounds.left) / Math.max(1, bounds.width))) * (count - 1));
  };
  // Place the floating readout beside the guide, then keep it 8px inside the card edges.
  useLayoutEffect(() => {
    const place = () => {
      const node = tip.current; const host = wrap.current; const frame = card.current;
      if (!node || !host || !frame) return;
      const hostBox = host.getBoundingClientRect(); const frameBox = frame.getBoundingClientRect();
      const width = node.offsetWidth; const guide = hostBox.width * shown.x / 100;
      const preferred = side === 'left' ? guide - 14 - width : guide + 14;
      const min = frameBox.left + 8 - hostBox.left; const max = frameBox.right - 8 - hostBox.left - width;
      node.style.left = `${Math.round(Math.max(min, Math.min(preferred, max)))}px`;
    };
    place();
    const observer = new ResizeObserver(place);
    if (card.current) observer.observe(card.current);
    return () => observer.disconnect();
  });
  const clear = () => setUserIndex(null);
  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (event.key === 'Escape') { event.preventDefault(); clear(); return; }
    if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
    event.preventDefault();
    setUserIndex(event.key === 'Home' ? 0 : event.key === 'End' ? count - 1 : event.key === 'ArrowLeft' ? selected === null ? count - 1 : Math.max(0, selected - 1) : selected === null ? 0 : Math.min(count - 1, selected + 1));
  };
  const rows = <ul className="aui-kpi-chart-tooltip-list">{shown.rows.map((item) => <li className="aui-kpi-chart-tooltip-row" key={`${item.id}-${item.order}`}>
    <span className="aui-kpi-chart-tooltip-swatch" style={{ background: item.color }} />
    <span className="aui-kpi-chart-tooltip-label" dir="auto" title={item.label}>{item.label}</span>
    <span className="aui-kpi-chart-tooltip-value" style={{ minWidth: `${longestValue + 2}ch` }}>{show(item.amount)}</span>
  </li>)}</ul>;
  const readout = visible ? `${shown.label}. ${shown.rows.map((item) => `${item.label}: ${item.amount === null ? '값 없음' : format(item.amount)}`).join(', ')}` : '';
  return <section className={`aui-kpi-chart ${className}`} data-expanded-reading={expandedReading} aria-labelledby={`${id}-title`}>
    <style>{styles}</style>
    <div className="aui-kpi-chart-card" ref={card}>
      <h3 id={`${id}-title`} className="aui-kpi-chart-title" dir="auto" title={title}>{title}</h3>
      <div className="aui-kpi-chart-summary"><span className="aui-kpi-chart-number">{value}</span>{delta !== undefined && finite(delta) && delta !== 0 && <span className="aui-kpi-chart-delta" data-negative={delta < 0}>{delta > 0 ? '+' : '−'}{format(Math.abs(delta))}<span className="aui-kpi-chart-sr">{delta > 0 ? '증가' : '감소'}</span></span>}</div>
      <div className="aui-kpi-chart-graph" data-empty={!hasData || undefined}>
        <div className="aui-kpi-chart-yaxis" aria-hidden="true">{hasData ? [high, (high + low) / 2, low].map((tick, order) => <span key={order}>{format(tick)}</span>) : <span>{' '}</span>}</div>
        <div className="aui-kpi-chart-plot-wrap" ref={wrap}>
          {hasData ? <div className="aui-kpi-chart-interactive" data-selected={visible} tabIndex={0} role="group" aria-label={`${title}. 좌우 방향키로 ${count}개 날짜의 계열값을 읽습니다.`} aria-describedby={`${id}-help`} onKeyDown={onKeyDown} onBlur={clear}
            onPointerDown={(event) => {
              if (event.button !== 0) return;
              const point = nearest(event);
              press.current = { id: event.pointerId, x: event.clientX };
              if (event.pointerType !== 'mouse' && userIndex === point) clear(); else setUserIndex(point);
            }}
            onPointerMove={(event) => {
              const active = press.current;
              if (event.pointerType === 'mouse') setUserIndex(nearest(event));
              else if (active?.id === event.pointerId && Math.abs(event.clientX - active.x) > 6) setUserIndex(nearest(event));
            }}
            onPointerUp={() => { press.current = null; }}
            onPointerCancel={() => { press.current = null; }}
            onPointerLeave={(event) => { if (event.pointerType === 'mouse' || press.current !== null) { press.current = null; clear(); } }}>
            <svg className="aui-kpi-chart-svg" viewBox="0 0 1000 116" preserveAspectRatio="none" aria-hidden="true">
              {[0, 58, 116].map((position) => <line className="aui-kpi-chart-grid" key={position} x1={0} y1={position} x2={1000} y2={position} />)}
              <g className="aui-kpi-chart-lines">{list.map((item, order) => <path className="aui-kpi-chart-series" key={`${item.id}-${order}`} d={line(item)} stroke={item.color} />)}</g>
            </svg>
            {isolated.map((dot) => <span key={dot.key} className="aui-kpi-chart-dot" aria-hidden="true" style={{ left: `${dot.left}%`, top: `${dot.top}%`, background: dot.color }} />)}
            <span className="aui-kpi-chart-guide" aria-hidden="true" style={{ left: `${shown.x}%` }} />
            {shown.points.map((point) => point.amount === null ? null : <span key={point.key} className="aui-kpi-chart-point" aria-hidden="true" style={{ left: `${shown.x}%`, top: `${y(point.amount)}%`, background: point.color }} />)}
          </div> : <div className="aui-kpi-chart-interactive" role="img" aria-label={`${title}. ${emptyLabel}`}>
            <svg className="aui-kpi-chart-svg" viewBox="0 0 1000 116" preserveAspectRatio="none" aria-hidden="true">{[0, 58, 116].map((position) => <line className="aui-kpi-chart-grid" key={position} x1={0} y1={position} x2={1000} y2={position} />)}</svg>
            <p className="aui-kpi-chart-empty" title={emptyLabel}>{emptyLabel}</p>
          </div>}
          {hasData && <div ref={tip} className="aui-kpi-chart-tooltip" aria-hidden="true" data-side={side} data-visible={visible}><p className="aui-kpi-chart-tooltip-heading">{shown.label}</p>{rows}</div>}
        </div>
        <div className="aui-kpi-chart-xaxis" aria-hidden="true">{hasData && axisIndices.map((point) => <span key={point} title={labels[point]} style={{ left: `${x(point)}%`, transform: point === 0 ? 'none' : point === count - 1 ? 'translateX(-100%)' : 'translateX(-50%)' }}>{labels[point]}</span>)}</div>
      </div>
      {hasData && <div className="aui-kpi-chart-reading" aria-hidden="true" data-visible={visible}>
        <div className="aui-kpi-chart-reading-content"><p className="aui-kpi-chart-tooltip-heading">{shown.label || ' '}</p>{rows}</div>
        <p className="aui-kpi-chart-reading-hint">{guidance}</p>
      </div>}
      {hasData && <p id={`${id}-help`} className="aui-kpi-chart-sr">초점만으로 날짜를 선택하지 않습니다. 좌우 방향키로 날짜를 이동하고 Home과 End로 처음과 끝을 선택합니다. Escape, 초점 이탈, 포인터 이탈은 {idle === null ? '상세를 숨깁니다' : `${labels[idle]} 대기 날짜로 돌아갑니다`}. 터치로 날짜를 고르면 손을 떼도 유지하며 같은 날짜를 다시 탭하면 대기 상태로 돌아갑니다.</p>}
      <span className="aui-kpi-chart-sr" role="status" aria-live="polite" aria-atomic="true">{readout}</span>
      {footer && <p className="aui-kpi-chart-footer" dir="auto">{footer}</p>}
    </div>
  </section>;
}

export type KpiChartDemoState = 'default' | 'empty' | 'long' | 'pinned' | 'negative' | 'zero' | 'currency' | 'single' | 'all-zero' | 'narrow' | 'live' | 'invalid' | 'format-boundaries' | 'gaps' | 'percent-zero' | 'small-ratio' | 'duplicates' | 'negative-round';
const demoColors = ['#8066ff', '#b1bdcf', '#20b5a8', '#ed68ab', '#6289ff', '#d8a245', '#87948b'];

/** Host examples replace snapshots and idle settings without controlling the input selection. */
export function KpiChartDemo({ state = 'default' }: { state?: KpiChartDemoState }) {
  const [revision, setRevision] = useState(0);
  const [shortened, setShortened] = useState(false);
  const [idle, setIdle] = useState<number | undefined>(6);
  useEffect(() => {
    if (state !== 'live') return;
    const timer = window.setInterval(() => setRevision((current) => (current + 1) % 4), 3500);
    return () => window.clearInterval(timer);
  }, [state]);
  const labels = ['10/12', '10/13', '10/14', '10/15', '10/16', '10/17', '10/18'];
  const long = state === 'long';
  const names = long ? ['한국어 문서와 아키텍처를 검토하는 연구 에이전트', 'agentdesk_runtime_verification_worker_long_identifier', 'Discord 릴레이 운영 자동화', '독립 코드 리뷰와 테스트', '릴리스 준비', '도구 응답 지연 분석', '사용량 집계'] : ['Claude · 구현', 'Codex · 도구', 'Research'];
  const numbers = [[4100, 4700, 3900, 4500, 5200, 4900, 5800], [3500, 3200, 4200, 4600, 3900, 4300, 4000], [1800, 2300, 2000, 1400, 2400, 2200, 2700], [1200, 1100, 1700, 2000, 1600, 1300, 1500], [600, 900, 700, 800, 1000, 1400, 1100], [500, 700, 800, 300, 500, 600, 700], [200, 400, 300, 500, 200, 100, 400]];
  if (state === 'gaps' || state === 'percent-zero' || state === 'small-ratio' || state === 'duplicates') return <KpiChartEdgeDemo state={state} />;
  if (state === 'negative-round') return <KpiChartRoundDemo />;
  const lengths = state === 'single' ? 1 : state === 'zero' || state === 'all-zero' ? 4 : state === 'format-boundaries' ? 7 : state === 'negative' || state === 'currency' ? 6 : shortened ? 3 : 7;
  const empty = state === 'empty';
  const points = empty ? [] : labels.slice(0, lengths);
  const allZero = state === 'all-zero';
  const series = empty ? [] : names.map((label, order) => ({ id: `series-${order}`, label, color: demoColors[order], values: points.map((_, point) => allZero ? 0 : state === 'format-boundaries' ? [0.004, 0.25, 999, 1000, 12500, 2_500_000, 3_200_000_000][point] / (order + 1) : state === 'negative' ? [100, 1400, -500, 1100, -700, 900][point] - order * 20 : state === 'zero' ? [1000, 2400, 1500, 900][point] - order * 50 : state === 'currency' ? [1300, 2400, 7500, 5300, 6800, 7100][point] - order * 100 : numbers[order][point] + revision * (order % 2 ? 350 : 200)) }));
  return <div className="aui-kpi-chart-demo" style={state === 'narrow' ? { width: 300, maxWidth: '100%' } : undefined}>
    <KpiChart title={long ? 'AgentDesk 전체 에이전트의 일별 실행 크레딧 추이' : state === 'currency' ? '공급자별 실행 비용' : '일별 에이전트 실행량'} value={empty ? '수집 대기' : state === 'currency' ? '$8,130' : '82,400'} delta={empty ? undefined : state === 'negative' ? -1 : state === 'zero' || allZero ? 0 : state === 'format-boundaries' ? 0.004 : state === 'currency' ? 19 : 7900} labels={points} tooltipLabels={points.map((_, point) => `2026년 10월 ${12 + point}일${long ? ' · 정기 실행과 장기 연구 작업을 포함한 전체 기록' : ''}`)} series={series} format={state === 'currency' ? (amount) => `$${amount.toLocaleString('en-US')}` : undefined} defaultIndex={state === 'pinned' ? idle : state === 'invalid' ? 99 : long ? 3 : undefined} footer={long ? '집계 기준: agentdesk_long_running_research_and_release_verification_channel · 한국어와 영어 작업 기록을 함께 표시합니다.' : empty ? '연결된 에이전트의 첫 실행을 기다리고 있습니다.' : '최근 실행 기록을 비교합니다. 대표값은 전체 기간 집계입니다.'} />
    {state === 'pinned' && <div className="aui-kpi-chart-demo-controls"><button type="button" onClick={() => setIdle(undefined)}>대기 선택 없음</button><button type="button" onClick={() => setIdle(0)}>첫날 대기</button><button type="button" onClick={() => setIdle(6)}>마지막 날 대기</button></div>}
    {state === 'live' && <><div className="aui-kpi-chart-demo-controls" onPointerDown={(event) => event.preventDefault()}><button type="button" onClick={() => setRevision((current) => (current + 1) % 4)}>새 집계 반영</button><button type="button" onClick={() => setShortened((current) => !current)}>{shortened ? '7일 복원' : '3일로 축소'}</button></div><p className="aui-kpi-chart-demo-note">3.5초마다 집계가 갱신됩니다. 그래프를 선택한 채 날짜 수도 바꿔 보세요. 대표값은 유지되며 시스템의 동작 줄이기 설정을 따릅니다.</p></>}
  </div>;
}

/** Exact halves around ±12.5 in each abbreviated unit, with their immediate neighbours, under one magnitude switch. */
function KpiChartRoundDemo() {
  const scales = [{ unit: 1e3, label: '천 단위(k)' }, { unit: 1e6, label: '백만 단위(M)' }, { unit: 1e9, label: '십억 단위(B)' }] as const;
  const [scale, setScale] = useState(0);
  const unit = scales[scale].unit;
  const labels = ['−12.5', '+12.5', '−12.499', '−12.501', '+12.499', '+12.501'];
  return <div className="aui-kpi-chart-demo">
    <KpiChart title="세션 크레딧 순증감" value="정산 검증" labels={labels} tooltipLabels={labels.map((label) => `${label} × ${scales[scale].label}`)} series={[{ id: 'net', label: '릴레이 정산', color: demoColors[0], values: [-12.5, 12.5, -12.499, -12.501, 12.499, 12.501].map((amount) => Math.round(amount * unit)) }]} defaultIndex={0} footer="정확한 절반은 절댓값이 커지는 쪽으로 반올림합니다. −12.5k는 −13k, 12.5k는 13k로 읽습니다." />
    <div className="aui-kpi-chart-demo-controls" role="group" aria-label="축약 단위">{scales.map((item, order) => <button type="button" key={item.unit} aria-pressed={scale === order} onClick={() => setScale(order)}>{item.label}</button>)}</div>
  </div>;
}

/** Boundary data: gaps and standalone points, percentage axes and duplicate or many series. */
function KpiChartEdgeDemo({ state }: { state: 'gaps' | 'percent-zero' | 'small-ratio' | 'duplicates' }) {
  const labels = ['11/11', '11/12', '11/13', '11/14', '11/15', '11/16', '11/17'];
  const percent = (amount: number) => `${Math.round(amount * 10) / 10}%`;
  if (state === 'gaps') return <div className="aui-kpi-chart-demo"><KpiChart title="Discord 릴레이 응답 수" value="312건" delta={Number.NaN} labels={labels} series={[
    { id: 'relay', label: '릴레이', color: demoColors[0], values: [40, null, 52, Number.NaN, 61, 58, 66, 999] },
    { id: 'tools', label: '도구 호출', color: demoColors[2], values: [12, 18, null, 24] },
  ]} footer="결측 사이의 단독 값은 점으로 남고, 비유한 증감은 숨깁니다. 라벨보다 긴 배열의 마지막 값은 쓰지 않습니다." /></div>;
  if (state === 'percent-zero') return <div className="aui-kpi-chart-demo"><KpiChart title="실패한 턴 비율" value="0%" labels={labels} series={[{ id: 'fail', label: '실패율', color: demoColors[3], values: labels.map(() => 0) }]} format={percent} defaultIndex={6} footer="백분율 축은 모두 0이어도 0~100% 축과 0 선을 보여 줍니다." /></div>;
  if (state === 'small-ratio') return <div className="aui-kpi-chart-demo"><KpiChart title="재시도 비율" value="0.8%" delta={0.3} labels={labels} series={[{ id: 'retry', label: '재시도', color: demoColors[4], values: [0.2, 0.5, 0.4, 0.9, 0.6, 0.7, 0.8] }, { id: 'abort', label: '중단', color: demoColors[1], values: [0.1, 0.1, 0.3, 0.2, 0.1, 0.4, 0.2] }]} format={percent} footer="작은 비율도 최소 0~10% 범위로 그립니다." /></div>;
  return <div className="aui-kpi-chart-demo"><KpiChart title="에이전트별 응답량" value="9,140" delta={-240} labels={labels} series={[
    { id: 'a', label: 'Claude', color: demoColors[0], values: [820, 900, 760, 980, 1010, 940, 1100] },
    { id: 'b', label: 'Claude', color: demoColors[1], values: [400, 380, 520, 610, 450, 480, 530] },
    { id: 'c', label: 'Codex', color: demoColors[2], values: [300, 420, 380, 360, 510, 470, 490] },
    { id: 'd', label: '연구', color: demoColors[3], values: [120, 160, 140, 200, 180, 210, 230] },
    { id: 'e', label: '릴리스', color: demoColors[4], values: [90, 70, 110, 130, 100, 120, 140] },
    { id: 'f', label: '리뷰', color: demoColors[5], values: [60, 80, 40, 70, 90, 100, 80] },
    { id: 'g', label: '루틴', color: demoColors[6], values: [30, 20, 50, 40, 30, 60, 50] },
    { id: 'h', label: 'Discord 채널 알림', color: '#4fb0fe', values: [10, 30, 20, 40, 30, 20, 60] },
  ]} defaultIndex={2.6} footer="같은 이름의 계열도 각각 남고, 대기 순번 2.6은 반올림해 3번째 날짜를 고정합니다." /></div>;
}
