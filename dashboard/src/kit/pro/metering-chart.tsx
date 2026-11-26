import { useRef, useState, type CSSProperties, type KeyboardEvent as ReactKeyboardEvent, type PointerEvent as ReactPointerEvent } from 'react';

// Clean-room bars from the leak-checked metering-chart spec. No Pro source.
export interface MeterSlice { name: string; value: number }
export interface MeterDay { label: string; total: number; slices?: MeterSlice[] }
export interface MeterTotal { name: string; value: number; text?: string }
export interface MeteringChartProps {
  title: string;
  days?: MeterDay[];
  totals?: MeterTotal[];
  period?: string;
  live?: boolean;
  badge?: string;
  format?: (value: number) => string;
  height?: number;
  accent?: string;
  emptyLabel?: string;
  sliceLimit?: number;
  restLabel?: string;
  className?: string;
  style?: CSSProperties;
}
export function meterSlices(day: MeterDay, limit = 4, rest = '기타') {
  const slices = [...(day.slices ?? [])].filter((slice) => slice.value > 0).sort((a, b) => b.value - a.value);
  if (slices.length <= limit) return slices;
  const head = slices.slice(0, limit);
  const extra = slices.slice(limit).reduce((sum, slice) => sum + slice.value, 0);
  return [...head, { name: rest, value: extra }];
}
export function meterAxis(peak: number) {
  if (peak <= 0) return { max: 15, ticks: [15, 10, 5, 0] };
  if (peak <= 20) {
    const max = Math.max(5, Math.ceil(peak / 5) * 5);
    return max >= 15 ? { max, ticks: [max, Math.max(0, max - 5), Math.max(0, max - 10), 0] } : { max: 15, ticks: [15, 10, 5, 0] };
  }
  const raw = peak / 3;
  const pow = 10 ** Math.floor(Math.log10(raw));
  const step = ([1, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10].find((n) => n * pow >= raw) ?? 10) * pow;
  const max = step * 3;
  return { max, ticks: [max, step * 2, step, 0] };
}
const money = (value: number) => `$${value.toFixed(1)}`;
const styles = `
.aui-metering-chart { --aui-meter-surface:var(--aui-bg-1,#fff); --aui-meter-ink:var(--aui-fg,#171717); --aui-meter-muted:var(--aui-fg-muted,#8f8f8f); --aui-meter-line:var(--aui-line,#ebebeb); --aui-meter-grid:var(--aui-line-soft,#e6e6e6); --aui-meter-badge:#e0338f; --aui-meter-badge-bg:rgb(238 62 158 / .1); --aui-meter-focus:var(--aui-focus,rgba(0,96,226,.7)); --aui-meter-shadow:0 1px 2px rgb(0 0 0 / .03), 0 8px 24px rgb(0 0 0 / .035); width:100%; min-width:0; padding:20px 20px 28px; border:1px solid var(--aui-meter-line); border-radius:20px; background:var(--aui-meter-surface); color:var(--aui-meter-ink); box-shadow:var(--aui-meter-shadow); font:13px/20px var(--aui-font-sans, Inter, ui-sans-serif, system-ui, sans-serif); font-variant-numeric:tabular-nums; }
:where(.dark,[data-theme="dark"]) .aui-metering-chart { --aui-meter-surface:var(--aui-bg-1,#232326); --aui-meter-ink:var(--aui-fg,#ededed); --aui-meter-muted:var(--aui-fg-muted,#8f8f8f); --aui-meter-line:var(--aui-line,rgb(255 255 255 / .08)); --aui-meter-grid:var(--aui-line-soft,rgb(255 255 255 / .1)); --aui-meter-badge:#f472b6; --aui-meter-badge-bg:rgb(238 62 158 / .18); --aui-meter-focus:var(--aui-focus,rgba(77,141,255,.8)); --aui-meter-shadow:0 1px 2px rgb(0 0 0 / .4), 0 8px 24px rgb(0 0 0 / .35); }
.aui-metering-chart-head { display:flex; gap:12px; align-items:center; min-width:0; }
.aui-metering-chart-head h2 { margin:0; overflow:hidden; font-size:16px; font-weight:600; line-height:24px; text-overflow:ellipsis; white-space:nowrap; }
.aui-metering-chart-badge { display:inline-flex; gap:4px; align-items:center; height:22px; padding:0 8px; border-radius:99px; background:var(--aui-meter-badge-bg); color:var(--aui-meter-badge); font-size:12px; line-height:16px; }
.aui-metering-chart-period { margin-left:auto; color:var(--aui-meter-muted); font-size:12px; line-height:16px; white-space:nowrap; }
.aui-metering-chart-totals { display:flex; flex-wrap:wrap; gap:4px 24px; margin:8px 0 4px; }
.aui-metering-chart-totals span { color:var(--aui-meter-muted); font-size:14px; line-height:26px; }
.aui-metering-chart-totals b { color:var(--aui-meter-ink); font-size:17px; font-weight:600; }
.aui-metering-chart-plot { position:relative; display:grid; grid-template-columns:minmax(36px,max-content) minmax(0,1fr); gap:12px; min-width:0; outline:none; touch-action:pan-y; }
.aui-metering-chart-plot:focus-visible { outline:2px solid var(--aui-meter-focus); outline-offset:2px; }
.aui-metering-chart-y { display:flex; flex-direction:column; justify-content:space-between; color:var(--aui-meter-muted); font-size:11px; line-height:16px; text-align:right; white-space:nowrap; }
.aui-metering-chart-bars { position:relative; display:flex; align-items:flex-end; justify-content:space-between; min-width:0; }
.aui-metering-chart-bars > div { display:flex; flex:1; align-items:flex-end; justify-content:center; min-width:0; height:100%; }
.aui-metering-chart-bar { width:18px; max-width:42%; border-radius:3.5px 3.5px 0 0; background:linear-gradient(180deg, var(--aui-meter-top, #8b7cff), color-mix(in srgb, var(--aui-meter-top, #8b7cff) 28%, #fff)); }
:where(.dark,[data-theme="dark"]) .aui-metering-chart-bar { background:linear-gradient(180deg, var(--aui-meter-top-dark, var(--aui-meter-top, #8b7cff)), color-mix(in srgb, var(--aui-meter-top-dark, var(--aui-meter-top, #8b7cff)) 58%, #232326)); }
.aui-metering-chart-bar[data-on] { outline:2px solid #5e5cff; }
.aui-metering-chart-x { display:flex; justify-content:space-between; gap:4px; margin-top:6px; color:var(--aui-meter-muted); font-size:11px; white-space:nowrap; }
.aui-metering-chart-empty { position:absolute; inset:0; display:grid; place-items:center; margin:0; color:var(--aui-meter-muted); font-size:12px; }
.aui-metering-chart-tip { position:absolute; z-index:2; min-width:140px; padding:8px 12px 10px; border-radius:12px; background:#171717; color:#fff; font-size:12px; line-height:16px; }
:where(.dark,[data-theme="dark"]) .aui-metering-chart-tip { background:#ededed; color:#171717; }
@media (prefers-reduced-motion: reduce) { .aui-metering-chart-bar { animation:none !important; } }
`;
const tones = ['#bbdbfa', '#cad5f6', '#d7cff4', '#e2d7f4', '#e8e0ec', '#ddddf0', '#6e6cff'];
const darkTones = ['#6db2c8', '#879ec0', '#a493c0', '#9c87ab', '#bdaac7', '#ababc8', '#5856e9'];
export function MeteringChart({ title, days = [], totals = [], period, live = false, badge = 'Live', format = money, height = 196, accent = '#5e5cff', emptyLabel = '표시할 사용량이 없습니다.', sliceLimit = 4, restLabel = '나머지', className = '', style }: MeteringChartProps) {
  const plot = useRef<HTMLDivElement>(null);
  const [selected, setSelected] = useState<number | null>(null);
  const peak = Math.max(0, ...days.map((day) => Math.max(0, day.total)));
  const { max, ticks } = meterAxis(peak);
  const xStep = days.length > 14 ? Math.ceil(days.length / 6) : 2;
  const empty = days.length === 0;
  const pick = (index: number | null) => { if (!empty) setSelected(index); };
  const keys = (event: ReactKeyboardEvent) => {
    if (empty) return;
    if (event.key === 'Escape') { pick(null); return; }
    const next = event.key === 'ArrowRight' ? Math.min(days.length - 1, (selected ?? -1) + 1) : event.key === 'ArrowLeft' ? Math.max(0, (selected ?? days.length) - 1) : event.key === 'Home' ? 0 : event.key === 'End' ? days.length - 1 : null;
    if (next === null) return;
    event.preventDefault();
    pick(next);
  };
  const day = selected === null ? null : days[selected];
  const slices = day ? meterSlices(day, sliceLimit, restLabel) : [];
  return (
    <section className={`aui-metering-chart ${className}`} style={style}>
      <style>{styles}</style>
      <div className="aui-metering-chart-head">
        <h2>{title}</h2>
        {live && <span className="aui-metering-chart-badge">{badge}</span>}
        {period && <span className="aui-metering-chart-period">{period}</span>}
      </div>
      {!!totals.length && <div className="aui-metering-chart-totals">{totals.map((item) => <span key={item.name}>{item.name} <b>{item.text ?? format(item.value)}</b></span>)}</div>}
      <div ref={plot} className="aui-metering-chart-plot" role="group" aria-label={title} tabIndex={empty ? undefined : 0} style={{ height }} onKeyDown={keys} onPointerLeave={() => { if (matchMedia('(hover:hover)').matches) pick(null); }}>
        <div className="aui-metering-chart-y" aria-hidden="true">{ticks.map((tick, index) => <span key={index}>{format(tick)}</span>)}</div>
        <div>
          <div className="aui-metering-chart-bars" style={{ height: height - 22 }} onPointerDown={(event: ReactPointerEvent<HTMLDivElement>) => { const rect = event.currentTarget.getBoundingClientRect(); const index = Math.min(days.length - 1, Math.max(0, Math.floor((event.clientX - rect.left) / rect.width * days.length))); pick(index); }}>
            {days.map((item, index) => (
              <div key={item.label} aria-hidden="true">
                <span className="aui-metering-chart-bar" data-on={selected === index || undefined} style={{ height: max ? `${Math.max(0, item.total) / max * 100}%` : 0, ['--aui-meter-top' as string]: tones[index % tones.length], ['--aui-meter-top-dark' as string]: darkTones[index % darkTones.length] }} />
              </div>
            ))}
          </div>
          <div className="aui-metering-chart-x" aria-hidden="true">
            {days.length > 14
              ? days.filter((_, index) => index % xStep === 0 || index === days.length - 1).map((item) => <span key={item.label}>{item.label}</span>)
              : days.map((item, index) => <span key={item.label} style={{ visibility: index % 2 === 0 ? 'visible' : 'hidden' }}>{item.label}</span>)}
          </div>
          {empty && <p className="aui-metering-chart-empty">{emptyLabel}</p>}
          {day && <div className="aui-metering-chart-tip" role="status" style={{ accentColor: accent }}><strong>{day.label} {format(day.total)}</strong>{slices.map((slice) => <div key={slice.name}>{slice.name} {format(slice.value)}</div>)}</div>}
        </div>
      </div>
    </section>
  );
}
const demoDays: MeterDay[] = [
  ['11/18', 22.7, [12.4, 8.2, 2.1]], ['11/19', 30.9, [18.1, 11, 1.8]], ['11/20', 18.4, [9.6, 6.4, 2.4]], ['11/21', 37.5, [21, 13.5, 3]], ['11/22', 28.7, [16.4, 10.1, 2.2]], ['11/23', 25.8, [14.8, 9.4, 1.6]], ['11/24', 34.7, [19.2, 12.8, 2.7]],
].map(([label, total, parts]) => ({ label: String(label), total: Number(total), slices: ['Claude', 'Codex', 'Other'].map((name, index) => ({ name, value: (parts as number[])[index] })) }));
export function MeteringChartDemo({ state = 'default' }: { state?: 'default' | 'empty' | 'plain' | 'long' }) {
  if (state === 'empty') return <div style={{ width: '100%', maxWidth: 640 }}><MeteringChart title="Provider spend" days={[]} period="Last 7 days" /></div>;
  return <div style={{ width: '100%', maxWidth: state === 'long' ? 320 : 640 }}><MeteringChart title={state === 'long' ? '아주 긴 공급자 지출 제목 provider-spend-weekly-ledger' : 'Provider spend'} days={demoDays} totals={[{ name: 'Total', value: 198.7 }, { name: 'Daily avg', value: 28.4 }]} period="Last 7 days" live={state !== 'plain'} /></div>;
}
