import { useEffect, useRef, useState, type CSSProperties, type KeyboardEvent as ReactKeyboardEvent, type PointerEvent as ReactPointerEvent } from 'react';

// Clean-room chart from the leak-checked trend-chart spec. No Pro source.
export interface TrendSeries { name: string; values: (number | null)[]; color?: string; previous?: boolean }
export interface TrendChartProps {
  labels: string[];
  series: TrendSeries[];
  tooltipLabels?: string[];
  variant?: 'line' | 'area' | 'bar' | 'spark';
  format?: (value: number) => string;
  height?: number;
  axes?: 'both' | 'x' | 'none';
  tooltip?: boolean;
  zeroLine?: boolean;
  index?: number | null;
  defaultIndex?: number | null;
  onIndexChange?: (index: number | null) => void;
  label?: string;
  emptyLabel?: string;
  className?: string;
  style?: CSSProperties;
}
export const trendPalette = ['#0190fe', '#a651f0', '#13b3a0', '#e8b000', '#ee3e9e', '#5fc88a'];
export const trendFormat = (value: number) => {
  const absolute = Math.abs(value);
  if (absolute >= 1e9) return `${trim(value / 1e9)}B`;
  if (absolute >= 1e6) return `${trim(value / 1e6)}M`;
  if (absolute >= 1e3) return `${trim(value / 1e3)}k`;
  if (absolute > 0 && absolute < 10) return trim(value);
  return String(Math.round(value));
};
const trim = (value: number) => String(Number(value.toFixed(1)));
const finite = (value: unknown): value is number => typeof value === 'number' && Number.isFinite(value);
export function trendExtent(series: TrendSeries[], zeroLine: boolean) {
  const values = series.flatMap((item) => item.values.filter(finite));
  if (!values.length) return null;
  let min = Math.min(...values);
  let max = Math.max(...values);
  if (zeroLine && min >= 0) min = 0;
  if (zeroLine && max <= 0) max = 0;
  if (max === min) { max += 1; min -= min === 0 ? 0 : 1; }
  const span = max - min;
  const step = span <= 10 ? 2 : span <= 40 ? 10 : span <= 100 ? 20 : Math.pow(10, Math.floor(Math.log10(span / 3)));
  const top = Math.ceil(max / step) * step;
  const bottom = Math.floor(min / step) * step;
  return { min: bottom, max: top, step };
}
export function trendTicks(extent: { min: number; max: number; step: number }) {
  const ticks: number[] = [];
  for (let value = extent.min; value <= extent.max + extent.step / 100; value += extent.step) ticks.push(Math.round(value * 1000) / 1000);
  return ticks;
}

const styles = `
.aui-trend-chart { --aui-trend-surface: var(--aui-bg, #fff); --aui-trend-ink: var(--aui-fg, #171717); --aui-trend-dim: #525252; --aui-trend-muted: var(--aui-fg-muted, #8f8f8f); --aui-trend-line: var(--aui-line, #ebebeb); --aui-trend-grid: #f0f0f0; --aui-trend-zero: #d4d4d4; --aui-trend-guide: #d9d9d9; --aui-trend-focus: rgba(0,96,226,.7); position: relative; width: 100%; min-width: 0; color: var(--aui-trend-ink); font: 13px/20px Inter, ui-sans-serif, system-ui, sans-serif; font-variant-numeric: tabular-nums; }
:where(.dark,[data-theme="dark"]) .aui-trend-chart { --aui-trend-surface: #232326; --aui-trend-ink: #ededed; --aui-trend-dim: #c4c4c4; --aui-trend-muted: #8f8f8f; --aui-trend-line: rgb(255 255 255 / .1); --aui-trend-grid: rgb(255 255 255 / .06); --aui-trend-zero: rgb(255 255 255 / .2); --aui-trend-guide: rgb(255 255 255 / .2); --aui-trend-focus: rgba(77,141,255,.8); }
.aui-trend-chart-frame { display: grid; grid-template-columns: auto minmax(0,1fr); column-gap: 12px; row-gap: 8px; min-width: 0; }
.aui-trend-chart-frame[data-axes="x"] { grid-template-columns: minmax(0,1fr); }
.aui-trend-chart-y { display: flex; flex-direction: column; justify-content: space-between; min-width: 28px; color: var(--aui-trend-muted); font-size: 12px; line-height: 18px; text-align: right; }
.aui-trend-chart-plot { position: relative; min-width: 0; border-radius: 4px; outline: none; touch-action: pan-y; }
.aui-trend-chart-plot:focus-visible { outline: 2px solid var(--aui-trend-focus); outline-offset: 2px; }
.aui-trend-chart svg { display: block; width: 100%; height: 100%; overflow: visible; }
.aui-trend-chart-x { display: flex; justify-content: space-between; height: 20px; color: var(--aui-trend-muted); font-size: 12px; line-height: 20px; }
.aui-trend-chart-x span:nth-child(2) { text-align: center; }
.aui-trend-chart-x span:last-child { text-align: right; }
.aui-trend-chart-empty { position: absolute; inset: 0; display: grid; place-items: center; margin: 0; padding: 0 10px; color: var(--aui-trend-muted); font-size: 13px; line-height: 18px; text-align: center; }
.aui-trend-chart-tip { position: absolute; z-index: 2; min-width: 140px; max-width: min(240px, 100%); padding: 8px 10px; border-radius: 10px; background: var(--aui-trend-surface); color: var(--aui-trend-ink); box-shadow: 0 8px 24px -6px rgb(0 0 0 / .12), 0 2px 6px rgb(0 0 0 / .04); pointer-events: none; }
:where(.dark,[data-theme="dark"]) .aui-trend-chart-tip { background: #2a2a2d; box-shadow: 0 8px 24px -6px rgb(0 0 0 / .5), 0 2px 6px rgb(0 0 0 / .4); }
.aui-trend-chart-tip strong { display: block; margin-bottom: 4px; font-size: 12px; font-weight: 500; }
.aui-trend-chart-tip li { display: flex; gap: 6px; align-items: center; font-size: 12px; line-height: 18px; }
.aui-trend-chart-tip i { width: 8px; height: 8px; border-radius: 99px; }
.aui-trend-chart[data-spark] .aui-trend-chart-frame { display: block; }
@container aui-trend (max-width: 360px) { .aui-trend-chart-y { display: none; } .aui-trend-chart-frame { grid-template-columns: minmax(0,1fr); } }
@media (prefers-reduced-motion: reduce) { .aui-trend-chart * { animation: none !important; transition: none !important; } }
`;

export function TrendChart({ labels, series, tooltipLabels, variant = 'line', format = trendFormat, height, axes, tooltip = true, zeroLine = true, index, defaultIndex = null, onIndexChange, label = '추이', emptyLabel = '표시할 값이 없습니다.', className = '', style }: TrendChartProps) {
  const spark = variant === 'spark';
  const plotHeight = height ?? (spark ? 44 : 156);
  const axisMode = axes ?? (spark ? 'none' : 'both');
  const host = useRef<HTMLDivElement>(null);
  const plot = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(320);
  const [inner, setInner] = useState<number | null>(defaultIndex);
  const selected = index !== undefined ? index : inner;
  const setSelected = (next: number | null) => { onIndexChange?.(next); if (index === undefined) setInner(next); };
  useEffect(() => {
    const node = plot.current; if (!node) return;
    const measure = () => setWidth(Math.max(32, node.clientWidth));
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(node);
    return () => observer.disconnect();
  }, []);
  const count = labels.length;
  const extent = trendExtent(series, zeroLine && !spark);
  const empty = !extent || !count;
  const yOf = (value: number) => {
    if (!extent) return plotHeight;
    const span = extent.max - extent.min || 1;
    return plotHeight - ((value - extent.min) / span) * plotHeight;
  };
  const xOf = (point: number) => count <= 1 ? width / 2 : (point / (count - 1)) * width;
  const move = (clientX: number) => {
    const rect = plot.current?.getBoundingClientRect();
    if (!rect || empty) return;
    const ratio = Math.min(1, Math.max(0, (clientX - rect.left) / rect.width));
    setSelected(Math.round(ratio * (count - 1)));
  };
  const keys = (event: ReactKeyboardEvent) => {
    if (empty) return;
    if (event.key === 'Escape') { setSelected(defaultIndex); return; }
    const current = selected ?? (event.key === 'ArrowLeft' || event.key === 'End' ? count - 1 : 0);
    const next = event.key === 'ArrowRight' ? Math.min(count - 1, (selected ?? -1) + 1) : event.key === 'ArrowLeft' ? Math.max(0, (selected ?? count) - 1) : event.key === 'Home' ? 0 : event.key === 'End' ? count - 1 : null;
    if (next === null) return;
    event.preventDefault();
    setSelected(next === current && selected === null ? next : next);
  };
  const ticks = extent ? trendTicks(extent) : [];
  const shownTicks = axisMode === 'both' ? [...ticks].reverse().filter((_, index, all) => all.length <= 3 || index % 2 === 0) : [];
  const xLabels = count ? [0, Math.floor((count - 1) / 2), count - 1].filter((value, position, all) => all.indexOf(value) === position) : [];
  const paths = series.map((item, order) => {
    const color = item.color ?? trendPalette[order % trendPalette.length];
    let path = '';
    let pen = false;
    item.values.slice(0, count).forEach((value, point) => {
      if (!finite(value)) { pen = false; return; }
      path += `${pen ? 'L' : 'M'}${xOf(point).toFixed(1)},${yOf(value).toFixed(1)}`;
      pen = true;
    });
    const area = variant === 'area' && !item.previous && path ? `${path} L${xOf(count - 1).toFixed(1)},${plotHeight} L${xOf(0).toFixed(1)},${plotHeight} Z` : '';
    return { item, color, path, area };
  });
  return (
    <div ref={host} className={`aui-trend-chart ${className}`} data-spark={spark || undefined} style={{ containerType: 'inline-size', containerName: 'aui-trend', ...style }}>
      <style>{styles}</style>
      <div className="aui-trend-chart-frame" data-axes={axisMode}>
        {axisMode === 'both' && <div className="aui-trend-chart-y" aria-hidden="true">{shownTicks.map((tick, index) => <span key={index}>{format(tick)}</span>)}</div>}
        <div>
          <div ref={plot} className="aui-trend-chart-plot" tabIndex={spark && !tooltip ? undefined : 0} role={spark && !tooltip ? 'img' : 'group'} aria-label={label} style={{ height: plotHeight }} onPointerMove={(event: ReactPointerEvent) => { if (event.pointerType !== 'touch') move(event.clientX); }} onPointerLeave={() => { if (matchMedia('(hover: hover)').matches) setSelected(defaultIndex); }} onPointerDown={(event) => move(event.clientX)} onKeyDown={keys}>
            <svg viewBox={`0 0 ${width} ${plotHeight}`} preserveAspectRatio="none" aria-hidden="true">
              <defs>
                {paths.map(({ color }, order) => <linearGradient key={order} id={`aui-trend-fill-${order}`} x1="0" y1="0" x2="0" y2="1"><stop offset="0" stopColor={color} stopOpacity="0.28" /><stop offset="1" stopColor={color} stopOpacity="0.02" /></linearGradient>)}
              </defs>
              {extent && !spark && ticks.map((tick, index) => <line key={index} x1="0" x2={width} y1={yOf(tick)} y2={yOf(tick)} stroke={tick === 0 ? 'var(--aui-trend-zero)' : 'var(--aui-trend-grid)'} strokeWidth="1" />)}
              {variant === 'bar' && extent && series.map((item, order) => item.values.slice(0, count).map((value, point) => finite(value) ? <rect key={`${order}-${point}`} x={xOf(point) - width / count / 4} y={Math.min(yOf(value), yOf(0))} width={Math.max(2, width / count / 3)} height={Math.abs(yOf(value) - yOf(0))} fill={item.color ?? trendPalette[order % trendPalette.length]} opacity={item.previous ? .45 : 1} rx="2" /> : null))}
              {variant !== 'bar' && paths.map(({ color, path, area, item }, order) => (
                <g key={item.name + order}>
                  {area && <path d={area} fill={`url(#aui-trend-fill-${order})`} />}
                  {path && <path d={path} fill="none" stroke={item.previous ? 'var(--aui-trend-muted)' : color} strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" strokeDasharray={item.previous ? '4 4' : undefined} />}
                </g>
              ))}
              {selected !== null && extent && <line x1={xOf(selected)} x2={xOf(selected)} y1="0" y2={plotHeight} stroke="var(--aui-trend-guide)" />}
              {selected !== null && paths.map(({ item, color }, order) => {
                const value = item.values[selected];
                return finite(value) ? <circle key={order} cx={xOf(selected)} cy={yOf(value)} r="4" fill={color} /> : null;
              })}
            </svg>
            {empty && <p className="aui-trend-chart-empty">{emptyLabel}</p>}
            {tooltip && !spark && selected !== null && !empty && (
              <div className="aui-trend-chart-tip" style={{ left: Math.min(width - 150, Math.max(0, xOf(selected) + 8)), top: 8 }} role="status">
                <strong>{tooltipLabels?.[selected] ?? labels[selected]}</strong>
                <ul>{series.map((item, order) => <li key={item.name + order}><i style={{ background: item.color ?? trendPalette[order % trendPalette.length] }} />{item.name}<span>{finite(item.values[selected]) ? format(item.values[selected] as number) : '—'}</span></li>)}</ul>
              </div>
            )}
          </div>
          {axisMode !== 'none' && <div className="aui-trend-chart-x" aria-hidden="true">{xLabels.map((point) => <span key={point}>{labels[point]}</span>)}</div>}
        </div>
      </div>
    </div>
  );
}

const days = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];
const longDays = ['Mon 11/18', 'Tue 11/19', 'Wed 11/20', 'Thu 11/21', 'Fri 11/22', 'Sat 11/23', 'Sun 11/24'];
const sample = [
  { name: 'adk-cc', values: [42, 55, 38, 61, 49, 22, 18], color: '#0190fe' },
  { name: 'Codex', values: [21, 28, 19, 33, 26, 8, 6], color: '#a651f0' },
  { name: 'Previous week', values: [36, 40, 44, 48, 41, 20, 15], color: '#8b8b92', previous: true },
];
export function TrendChartDemo({ state = 'default' }: { state?: 'default' | 'empty' | 'line' | 'bar' | 'spark' | 'gap' | 'long' }) {
  const width = '100%';
  const maxWidth = state === 'spark' ? 280 : state === 'long' ? 320 : 640;
  if (state === 'empty') return <div style={{ width, maxWidth }}><TrendChart labels={[]} series={[]} variant="area" height={180} /></div>;
  return <div style={{ width, maxWidth }}><TrendChart label="Turns per day" labels={days} tooltipLabels={longDays} series={state === 'gap' ? sample.map((item, index) => index ? item : { ...item, values: [42, null, 38, 61, 49, 22, 18] }) : sample} variant={state === 'line' || state === 'bar' || state === 'spark' ? state : 'area'} height={state === 'spark' ? 44 : 180} /></div>;
}
