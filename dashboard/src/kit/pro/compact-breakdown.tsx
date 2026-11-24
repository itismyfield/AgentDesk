import { useEffect, useId, useLayoutEffect, useRef, useState, type FocusEvent, type KeyboardEvent, type PointerEvent as ReactPointerEvent, type ReactNode } from 'react';
import { ArrowDown, ArrowUp } from 'lucide-react';

// Clean-room implementation from clean-specs/compact-breakdown (11/17 sync).
// All code, copy and demo data are original; no affectUI Pro source was consulted.

export interface CompactBreakdownItem {
  /** Stable across periods; a missing id falls back to the display label. Repeats stay separate rows by occurrence. */
  id?: string;
  label: string;
  value: number;
  color?: string;
}

export interface CompactBreakdownProps {
  items: CompactBreakdownItem[];
  /** Defaults to `labels.title`. */
  title?: string;
  periods?: { id: string; label: string }[];
  period?: string;
  defaultPeriod?: string;
  onPeriodChange?: (id: string) => void;
  delta?: number;
  format?: (value: number) => string;
  /** Defaults to `labels.total`. */
  totalLabel?: string;
  /** undefined: automatic leader; null: hide; other React nodes replace the automatic content. */
  footer?: ReactNode | null;
  hideFooter?: boolean;
  /**
   * Draws each row as a button that hands the item's `id` (its label without one) to the host, for example to open its
   * details. Without it rows are not buttons: hover, keyboard focus and a touch tap only change the reading.
   */
  onSelect?: (id: string) => void;
  /** Marks the row with this id as the chosen one (`aria-current`), with or without `onSelect`. Repeats of an id are all marked. */
  selectedId?: string | null;
  /** Every other visible and spoken text. Missing keys keep the Korean defaults. */
  labels?: Partial<CompactBreakdownLabels>;
  className?: string;
}
export interface CompactBreakdownLabels {
  /** Heading when `title` is not given. */
  title: string;
  /** Total name when `totalLabel` is not given. */
  total: string;
  /** Accessible name of the period switch. */
  periods: string;
  /** Accessible name of the ranked list. */
  list: string;
  /** Name of a row and its spoken reading, from the label, the formatted value and the share text. */
  row: (label: string, value: string, share: string) => string;
  /** Spoken summary; `change` is null without a finite delta. */
  summary: (total: string, amount: string, change: { percent: string; decrease: boolean } | null) => string;
  /** Shown without items. */
  empty: string;
  /** Automatic footer text before the leading item's name. */
  leader: string;
  /** Description of a row button with `onSelect`, e.g. what it opens. An empty string leaves it out. */
  selectHint: string;
}
export const compactBreakdownDefaultLabels: CompactBreakdownLabels = {
  title: '에이전트별 사용량',
  total: '전체 실행 크레딧',
  periods: '사용량 집계 기간',
  list: '사용량의 순위와 비중',
  row: (label, value, share) => `${label}, ${value}, 전체의 ${share}`,
  summary: (total, amount, change) => `${total}: ${amount}.${change ? ` 이전 기간과 비교해 ${change.percent} ${change.decrease ? '감소' : '증가'}.` : ''}`,
  empty: '첫 에이전트 실행이 도착하면 구성별 사용량을 표시합니다.',
  leader: '가장 많은 사용량:',
  selectHint: '누르면 자세히 볼 수 있어요.',
};

const styles = `
.aui-compact-breakdown {
  --aui-compact-breakdown-surface: var(--aui-bg-1, #ffffff);
  --aui-compact-breakdown-tray: var(--aui-bg-2, #f0f0f0);
  --aui-compact-breakdown-fg: var(--aui-fg, #171717);
  --aui-compact-breakdown-dim: var(--aui-fg-dim, #404040);
  --aui-compact-breakdown-muted: var(--aui-fg-muted, #8f8f8f);
  --aui-compact-breakdown-line: var(--aui-line, #ececec);
  --aui-compact-breakdown-selected: var(--aui-bg-3, #f4f4f5);
  --aui-compact-breakdown-pill: var(--aui-bg-1, #ffffff);
  --aui-compact-breakdown-pill-line: var(--aui-line, #ececec);
  --aui-compact-breakdown-accent: var(--aui-accent, #4fb0fe);
  --aui-compact-breakdown-series-2: var(--aui-compact-breakdown-color-2, var(--aui-accent, #4fb0fe));
  --aui-compact-breakdown-series-3: var(--aui-compact-breakdown-color-3, #b4deff);
  --aui-compact-breakdown-series-4: var(--aui-compact-breakdown-color-4, #a8a8ae);
  --aui-compact-breakdown-series-5: var(--aui-compact-breakdown-color-5, #d4d4d8);
  --aui-compact-breakdown-ok: var(--aui-ok, #1e8a4f);
  --aui-compact-breakdown-ok-bg: var(--aui-compact-breakdown-increase-bg, rgb(95 200 138 / 10%));
  --aui-compact-breakdown-err: var(--aui-err, #c4320a);
  --aui-compact-breakdown-err-bg: var(--aui-compact-breakdown-decrease-bg, rgb(240 68 56 / 8%));
  --aui-compact-breakdown-focus: var(--aui-compact-breakdown-focus-ring, var(--aui-focus, rgb(0 96 226 / 70%)));
  --aui-compact-breakdown-pill-shadow: 0 0 0 1px var(--aui-compact-breakdown-pill-line), 0 1px 2px rgb(0 0 0 / 6%);
  --aui-compact-breakdown-ease: var(--aui-ease, cubic-bezier(.22,1,.36,1));
  --aui-compact-breakdown-r: var(--aui-r-lg, 16px);
  --aui-compact-breakdown-s: var(--aui-r-md, 10px);
  --aui-compact-breakdown-t: var(--aui-r-sm, 7px);
  --aui-compact-breakdown-row-height: 30px;
  width: 100%; max-width: 360px; min-width: 0; container-type: inline-size;
  color: var(--aui-compact-breakdown-fg); font: 13px/1.45 var(--aui-font-sans, Inter, ui-sans-serif, system-ui, sans-serif);
  word-break: keep-all; overflow-wrap: anywhere; font-variant-numeric: tabular-nums;
}
:where(.dark, [data-theme="dark"]) .aui-compact-breakdown {
  --aui-compact-breakdown-surface: var(--aui-bg-1, #19191b);
  --aui-compact-breakdown-tray: var(--aui-bg-2, #111113);
  --aui-compact-breakdown-fg: var(--aui-fg, #ededed);
  --aui-compact-breakdown-dim: var(--aui-fg-dim, #c8c8cc);
  --aui-compact-breakdown-muted: var(--aui-fg-muted, #8b8b92);
  --aui-compact-breakdown-line: var(--aui-line, #2a2a2e);
  --aui-compact-breakdown-selected: var(--aui-bg-3, #222225);
  --aui-compact-breakdown-pill: var(--aui-compact-breakdown-pill-color, #2a2a2e);
  --aui-compact-breakdown-pill-line: var(--aui-compact-breakdown-pill-border, rgb(255 255 255 / 6%));
  --aui-compact-breakdown-accent: var(--aui-accent, #4fb0fe);
  --aui-compact-breakdown-series-2: var(--aui-compact-breakdown-color-2, var(--aui-accent, #0190fe));
  --aui-compact-breakdown-series-3: var(--aui-compact-breakdown-color-3, #0070d6);
  --aui-compact-breakdown-series-4: var(--aui-compact-breakdown-color-4, #6b6b73);
  --aui-compact-breakdown-series-5: var(--aui-compact-breakdown-color-5, #3d3d42);
  --aui-compact-breakdown-ok: var(--aui-ok, #7dcf9f);
  --aui-compact-breakdown-ok-bg: var(--aui-compact-breakdown-increase-bg, rgb(125 207 159 / 12%));
  --aui-compact-breakdown-err: var(--aui-err, #ff8a7a);
  --aui-compact-breakdown-err-bg: var(--aui-compact-breakdown-decrease-bg, rgb(255 107 95 / 12%));
  --aui-compact-breakdown-focus: var(--aui-compact-breakdown-focus-ring, var(--aui-focus, rgb(77 141 255 / 80%)));
  --aui-compact-breakdown-pill-shadow: 0 0 0 1px var(--aui-compact-breakdown-pill-line);
}
.aui-compact-breakdown *, .aui-compact-breakdown *::before, .aui-compact-breakdown *::after { box-sizing: border-box; }
.aui-compact-breakdown-tray { padding: 4px; border-radius: calc(var(--aui-compact-breakdown-r) + 4px); background: var(--aui-compact-breakdown-tray); }
.aui-compact-breakdown-card { min-width: 0; padding: 12px 14px 8px; border-radius: var(--aui-compact-breakdown-r); background: var(--aui-compact-breakdown-surface); box-shadow: var(--aui-shadow-sm, 0 0 0 1px var(--aui-compact-breakdown-line), 0 1px 2px rgb(0 0 0 / 4%)); }
.aui-compact-breakdown-header { display: flex; flex-wrap: wrap; align-items: center; justify-content: space-between; gap: 8px 10px; min-height: 30px; }
.aui-compact-breakdown-title { flex: 1 1 96px; margin: 0; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 13.5px; line-height: 20px; font-weight: 600; }
.aui-compact-breakdown-period-scroll { flex: 0 1 auto; min-width: 0; max-width: 100%; margin-left: auto; overflow-x: auto; padding: 2px; border-radius: var(--aui-compact-breakdown-s); background: var(--aui-compact-breakdown-selected); box-shadow: inset 0 0 0 1px var(--aui-compact-breakdown-line); }
.aui-compact-breakdown-periods { position: relative; display: flex; width: max-content; min-width: 100%; }
.aui-compact-breakdown button { font: inherit; cursor: pointer; touch-action: pan-y; -webkit-tap-highlight-color: transparent; }
.aui-compact-breakdown button:focus-visible { outline: 2px solid var(--aui-compact-breakdown-focus); outline-offset: 2px; }
.aui-compact-breakdown-period { position: relative; z-index: 1; flex: 1 0 auto; max-width: 160px; min-height: 24px; padding: 0 8px; border: 0; border-radius: calc(var(--aui-compact-breakdown-s) - 2px); background: transparent; color: var(--aui-compact-breakdown-muted); font-size: 12px !important; font-weight: 500 !important; line-height: 24px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; transition: color 200ms ease; touch-action: pan-x pan-y !important; }
.aui-compact-breakdown-period[aria-checked="true"], .aui-compact-breakdown-period:hover { color: var(--aui-compact-breakdown-fg); }
.aui-compact-breakdown-pill { position: absolute; top: 0; left: 0; pointer-events: none; border-radius: calc(var(--aui-compact-breakdown-s) - 2px); background: var(--aui-compact-breakdown-pill); box-shadow: var(--aui-compact-breakdown-pill-shadow); transition: transform 420ms var(--aui-compact-breakdown-ease), width 420ms var(--aui-compact-breakdown-ease); }
.aui-compact-breakdown-summary { margin-top: 12px; }
.aui-compact-breakdown-headline { display: flex; align-items: center; flex-wrap: wrap; gap: 8px; min-height: 32px; }
.aui-compact-breakdown-number { min-width: 0; font-size: 26px; line-height: 32px; font-weight: 600; letter-spacing: -.02em; }
.aui-compact-breakdown-delta { display: inline-flex; align-items: center; gap: 2px; height: 20px; padding: 0 6px 0 4px; border-radius: 999px; color: var(--aui-compact-breakdown-ok); background: var(--aui-compact-breakdown-ok-bg); font-size: 12px; font-weight: 500; white-space: nowrap; animation: aui-compact-breakdown-badge-in 380ms var(--aui-compact-breakdown-ease) both; }
.aui-compact-breakdown-delta[data-negative="true"] { color: var(--aui-compact-breakdown-err); background: var(--aui-compact-breakdown-err-bg); }
.aui-compact-breakdown-description { display: flex; align-items: center; gap: 6px; height: 18px; line-height: 18px; overflow: hidden; color: var(--aui-compact-breakdown-muted); font-size: 12.5px; }
.aui-compact-breakdown-description-content { display: flex; min-width: 0; align-items: center; gap: 6px; animation: aui-compact-breakdown-content-in 260ms var(--aui-compact-breakdown-ease) both; }
.aui-compact-breakdown-description-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.aui-compact-breakdown-description-share { flex: none; white-space: nowrap; }
.aui-compact-breakdown-bar-scroll { overflow-x: auto; padding-block: 2px; margin-top: 10px; }
.aui-compact-breakdown-track { display: flex; gap: 2px; height: 8px; border-radius: 4px; }
.aui-compact-breakdown-segment { display: block; height: 100%; min-width: 2px; border-radius: 2.5px; transition: opacity 200ms ease, transform 260ms var(--aui-compact-breakdown-ease); }
.aui-compact-breakdown-segment:first-child { border-top-left-radius: 4px; border-bottom-left-radius: 4px; }
.aui-compact-breakdown-segment:last-child { border-top-right-radius: 4px; border-bottom-right-radius: 4px; }
.aui-compact-breakdown-segment[data-active="true"] { transform: scaleY(1.5); }
.aui-compact-breakdown-list { display: flex; flex-direction: column; margin: 6px -8px 0; padding: 0; list-style: none; }
.aui-compact-breakdown-list-item { max-height: var(--aui-compact-breakdown-row-height); flex: none; opacity: 1; transition: max-height 320ms var(--aui-compact-breakdown-ease), opacity 200ms ease; }
.aui-compact-breakdown-list-item[data-exiting="true"] { max-height: 0; overflow: hidden; opacity: 0; pointer-events: none; }
.aui-compact-breakdown-row { display: grid; grid-template-columns: 8px minmax(0, 1fr) auto minmax(34px, auto); align-items: center; gap: 9px; width: 100%; height: var(--aui-compact-breakdown-row-height); padding: 0 8px; border: 0; border-radius: calc(var(--aui-compact-breakdown-t) + 1px); color: var(--aui-compact-breakdown-dim); text-align: left; background: transparent; transition: opacity 200ms ease, background 160ms ease; }
.aui-compact-breakdown-row[data-active="true"] { background: var(--aui-compact-breakdown-selected); }
.aui-compact-breakdown-row[data-selected="true"] { background: var(--aui-compact-breakdown-selected); color: var(--aui-compact-breakdown-fg); box-shadow: inset 3px 0 0 var(--aui-compact-breakdown-accent); }
.aui-compact-breakdown-row[data-selected="true"] .aui-compact-breakdown-label { font-weight: 600; }
.aui-compact-breakdown-list-item, div.aui-compact-breakdown-row { touch-action: pan-y; -webkit-tap-highlight-color: transparent; }
.aui-compact-breakdown-list-item:focus-visible { outline: 2px solid var(--aui-compact-breakdown-focus); outline-offset: 2px; border-radius: calc(var(--aui-compact-breakdown-t) + 1px); }
.aui-compact-breakdown-dot { flex: none; width: 8px; height: 8px; border-radius: 2.5px; }
.aui-compact-breakdown-label { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.aui-compact-breakdown-value { color: var(--aui-compact-breakdown-fg); font-weight: 500; text-align: right; white-space: nowrap; }
.aui-compact-breakdown-share { min-width: 34px; color: var(--aui-compact-breakdown-muted); text-align: right; white-space: nowrap; }
.aui-compact-breakdown-full-readout { display: none; margin: 8px 0 0; padding: 7px 8px; border-radius: calc(var(--aui-compact-breakdown-t) + 1px); background: var(--aui-compact-breakdown-selected); color: var(--aui-compact-breakdown-dim); font-size: 12px; }
.aui-compact-breakdown-full-readout[data-long="true"] { display: block; }
.aui-compact-breakdown-footer { display: flex; align-items: center; gap: 4px; min-height: 34px; margin: 0; padding: 4px 10px 0; color: var(--aui-compact-breakdown-muted); font-size: 12.5px; }
.aui-compact-breakdown-footer-auto { display: flex; align-items: center; gap: 4px; min-width: 0; width: 100%; white-space: nowrap; animation: aui-compact-breakdown-content-in 320ms var(--aui-compact-breakdown-ease) both; }
.aui-compact-breakdown-footer-auto strong { display: block; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.aui-compact-breakdown-footer strong { color: var(--aui-compact-breakdown-fg); font-weight: 500; }
.aui-compact-breakdown-empty { margin: 10px 0 2px; color: var(--aui-compact-breakdown-muted); font-size: 12px; }
.aui-compact-breakdown-sr { position: absolute; width: 1px; height: 1px; margin: -1px; padding: 0; overflow: hidden; clip-path: inset(50%); white-space: nowrap; }
.aui-compact-breakdown-demo { width: 100%; max-width: 360px; min-width: 0; }
.aui-compact-breakdown-demo-controls { display: flex; flex-wrap: wrap; gap: 8px; margin-top: 12px; color: var(--aui-fg-muted, #737373); font: 12px/1.5 var(--aui-font-sans, system-ui, sans-serif); }
.aui-compact-breakdown-demo-controls button { min-width: 44px; min-height: 44px; padding: 8px 12px; border: 1px solid var(--aui-line, #d4d4d8); border-radius: 8px; background: var(--aui-bg-1, #fff); color: var(--aui-fg, #171717); cursor: pointer; font: inherit; }
.aui-compact-breakdown-demo-controls button:focus-visible { outline: 2px solid var(--aui-focus, rgb(0 96 226 / 70%)); outline-offset: 2px; }
:where(.dark, [data-theme="dark"]) .aui-compact-breakdown-demo-controls { color: var(--aui-fg-muted, #8b8b92); }
:where(.dark, [data-theme="dark"]) .aui-compact-breakdown-demo-controls button { background: var(--aui-bg-1, #19191b); color: var(--aui-fg, #ededed); border-color: var(--aui-line, #2a2a2e); }
@container (max-width: 300px) {
  .aui-compact-breakdown-header { align-items: stretch; flex-direction: column; }
  .aui-compact-breakdown-title { flex: none; }
  .aui-compact-breakdown-period-scroll { align-self: stretch; margin-left: 0; }
  .aui-compact-breakdown-row { gap: 7px; }
}
@container (max-width: 250px) {
  .aui-compact-breakdown-share { display: none; }
  .aui-compact-breakdown-row { grid-template-columns: 8px minmax(0, 1fr) auto; }
  .aui-compact-breakdown-period { padding-inline: 6px; }
}
@media (pointer: coarse) {
  .aui-compact-breakdown { --aui-compact-breakdown-row-height: 44px; }
  .aui-compact-breakdown-period { min-width: 44px; min-height: 44px; padding-inline: 10px; }
  .aui-compact-breakdown-full-readout { display: block; }
}
@media (prefers-reduced-motion: reduce) {
  .aui-compact-breakdown *, .aui-compact-breakdown *::before, .aui-compact-breakdown *::after { animation: none !important; transition: none !important; }
}
@keyframes aui-compact-breakdown-content-in { from { opacity: 0; transform: translateY(5px); filter: blur(2px); } to { opacity: 1; transform: none; filter: none; } }
@keyframes aui-compact-breakdown-badge-in { from { opacity: 0; transform: scale(.85); } to { opacity: 1; transform: scale(1); } }
`;

const colors = ['var(--aui-compact-breakdown-accent)', 'var(--aui-compact-breakdown-series-2)', 'var(--aui-compact-breakdown-series-3)', 'var(--aui-compact-breakdown-series-4)', 'var(--aui-compact-breakdown-series-5)'];
const safeValue = (value: number) => Number.isFinite(value) ? Math.max(0, value) : 0;
const defaultFormat = (value: number) => new Intl.NumberFormat('en-US', { maximumFractionDigits: 0 }).format(value);
const shareOf = (value: number, total: number) => value / Math.max(total, .0001) * 100;
/** Rounded shares, except open intervals near the ends: (0,1) → <1% and (99,100) → >99%. */
const shareText = (share: number) => share > 0 && share < 1 ? '<1%' : share > 99 && share < 100 ? '>99%' : `${Math.round(share)}%`;
const reducedMotion = () => typeof window !== 'undefined' && window.matchMedia('(prefers-reduced-motion: reduce)').matches;
type RememberedItem = { id: string; source: string; label: string; value: number; color: string; order: number; present: boolean };
type AmountFrame = { values: Record<string, number>; total: number; headline: number };

/** One quartic clock drives the headline, total, row values and the bar's ratios. */
function useAmounts(items: RememberedItem[], activeId: string | undefined) {
  const target: AmountFrame = {
    values: Object.fromEntries(items.map((item) => [item.id, item.present ? item.value : 0])),
    total: items.reduce((sum, item) => sum + (item.present ? item.value : 0), 0),
    headline: 0,
  };
  target.headline = activeId ? target.values[activeId] ?? 0 : target.total;
  const signature = JSON.stringify(target);
  const [frame, setFrame] = useState<AmountFrame>(() => ({ values: {}, total: 0, headline: 0 }));
  const current = useRef(frame);
  const hasAnimated = useRef(false);
  useLayoutEffect(() => {
    const to = JSON.parse(signature) as AmountFrame;
    const from = current.current;
    let animation = 0;
    let start: number | undefined;
    const duration = hasAnimated.current ? 620 : 900;
    const publish = (next: AmountFrame) => { current.current = next; setFrame(next); };
    const media = window.matchMedia('(prefers-reduced-motion: reduce)');
    const finish = () => { cancelAnimationFrame(animation); hasAnimated.current = true; publish(to); };
    const tick = (time: number) => {
      start ??= time;
      hasAnimated.current = true;
      const t = Math.min(1, (time - start) / duration);
      const progress = 1 - Math.pow(1 - t, 4);
      const interpolate = (a: number, b: number) => a + (b - a) * progress;
      publish({
        values: Object.fromEntries(Object.entries(to.values).map(([key, value]) => [key, interpolate(from.values[key] ?? 0, value)])),
        total: interpolate(from.total, to.total),
        headline: interpolate(from.headline, to.headline),
      });
      if (t < 1) animation = requestAnimationFrame(tick);
    };
    if (media.matches) finish(); else animation = requestAnimationFrame(tick);
    const onMotionChange = () => { if (media.matches) finish(); };
    media.addEventListener('change', onMotionChange);
    return () => { cancelAnimationFrame(animation); media.removeEventListener('change', onMotionChange); };
  }, [signature]);
  return frame;
}

export function CompactBreakdown({ items, title: titleProp, periods = [], period, defaultPeriod, onPeriodChange, delta, format = defaultFormat, totalLabel: totalProp, footer, hideFooter = false, onSelect, selectedId, labels, className = '' }: CompactBreakdownProps) {
  const id = useId();
  const text: CompactBreakdownLabels = { ...compactBreakdownDefaultLabels, ...labels };
  const title = titleProp ?? text.title;
  const totalLabel = totalProp ?? text.total;
  const rootRef = useRef<HTMLElement>(null);
  const listRef = useRef<HTMLOListElement>(null);
  const periodRef = useRef<HTMLDivElement>(null);
  const pillRef = useRef<HTMLSpanElement>(null);
  const history = useRef(new Map<string, RememberedItem>());
  const previousRows = useRef(new Map<string, number>());
  const previousRowStates = useRef(new Map<string, { present: boolean; height: number }>());
  const preserveHover = useRef(false);
  const pointerPosition = useRef<{ x: number; y: number } | null>(null);
  const [innerPeriod, setInnerPeriod] = useState(defaultPeriod ?? periods[0]?.id ?? '');
  const [hovered, setHovered] = useState<string | null>(null);
  const [focused, setFocused] = useState<string | null>(null);
  const [pinned, setPinned] = useState<string | null>(null);
  const [pointerKind, setPointerKind] = useState('');

  // Repeated ids (or labels) remain separate entries keyed by their occurrence within the snapshot.
  const occurrences = new Map<string, number>();
  const keyed = items.map((item) => {
    const base = item.id ?? item.label;
    const index = occurrences.get(base) ?? 0;
    occurrences.set(base, index + 1);
    return { item, key: index ? `${base}\u241f${index}` : base };
  });
  const present = new Set(keyed.map(({ key }) => key));
  for (const old of history.current.values()) old.present = false;
  for (const { item, key } of keyed) {
    const old = history.current.get(key);
    const order = old?.order ?? history.current.size;
    history.current.set(key, { id: key, source: item.id ?? item.label, label: item.label, value: safeValue(item.value), color: item.color ?? old?.color ?? colors[Math.min(order, 4)], order, present: true });
  }
  const remembered = Array.from(history.current.values());
  const activeMemory = hovered ?? focused ?? pinned;
  const active = remembered.find((item) => item.id === activeMemory && present.has(item.id));
  const amounts = useAmounts(remembered, active?.id);
  const total = items.reduce((sum, item) => sum + safeValue(item.value), 0);
  const sorted = remembered.filter((item) => item.present || (amounts.values[item.id] ?? 0) > .5)
    .sort((a, b) => Number(b.present) - Number(a.present) || (b.present ? b.value : 0) - (a.present ? a.value : 0) || a.order - b.order);
  const leader = sorted.find((item) => item.present && item.value > 0);
  const segments = remembered.filter((item) => shareOf(amounts.values[item.id] ?? 0, amounts.total) > .05);
  const selectedPeriod = period ?? innerPeriod;
  const activePeriod = Math.max(0, periods.findIndex((item) => item.id === selectedPeriod));
  const currentPeriod = periods[activePeriod]?.id ?? '';
  const rowOrder = JSON.stringify(sorted.map((item) => [item.id, item.present]));

  useLayoutEffect(() => {
    const next = new Map<string, number>();
    const states = new Map<string, { present: boolean; height: number }>();
    const reduce = reducedMotion();
    listRef.current?.querySelectorAll<HTMLElement>('.aui-compact-breakdown-list-item').forEach((row) => {
      const key = row.dataset.itemId!;
      const present = row.dataset.exiting !== 'true';
      const oldState = previousRowStates.current.get(key);
      const height = present ? row.getBoundingClientRect().height : oldState?.height ?? 0;
      states.set(key, { present, height });
      // Moving an existing DOM node to the exit group can discard CSS's previous style.
      // Explicit animations preserve its measured height while it is reordered below current rows.
      if (!reduce && oldState?.present && !present && typeof row.animate === 'function') {
        const collapse = row.animate([{ maxHeight: `${oldState.height}px` }, { maxHeight: '0px' }], { duration: 320, easing: 'cubic-bezier(.22,1,.36,1)', fill: 'both' });
        collapse.id = 'aui-compact-breakdown-collapse';
        const fade = row.animate([{ opacity: 1 }, { opacity: 0 }], { duration: 200, easing: 'ease', fill: 'both' });
        fade.id = 'aui-compact-breakdown-fade';
      } else if (present && oldState && !oldState.present) {
        row.getAnimations().filter((animation) => ['aui-compact-breakdown-collapse', 'aui-compact-breakdown-fade'].includes(animation.id)).forEach((animation) => animation.cancel());
      }
      const top = row.offsetTop;
      const oldTop = previousRows.current.get(key);
      next.set(key, top);
      if (!reduce && oldTop !== undefined && oldTop !== top && typeof row.animate === 'function') {
        row.getAnimations().filter((animation) => animation.id === 'aui-compact-breakdown-rank').forEach((animation) => animation.cancel());
        const move = row.animate([{ transform: `translateY(${oldTop - top}px)` }, { transform: 'translateY(0)' }], { duration: 560, easing: 'cubic-bezier(.22,1,.36,1)' });
        move.id = 'aui-compact-breakdown-rank';
      }
    });
    previousRows.current = next;
    previousRowStates.current = states;
  }, [rowOrder]);

  const periodSignature = JSON.stringify(periods);
  useLayoutEffect(() => {
    const track = periodRef.current;
    const pill = pillRef.current;
    if (!track || !pill) return;
    let frame = 0;
    const measure = () => {
      const selected = track.querySelector<HTMLButtonElement>('[aria-checked="true"]');
      if (!selected) return;
      const first = !pill.dataset.placed;
      if (first) pill.style.transition = 'none';
      pill.style.transform = `translateX(${selected.offsetLeft}px)`;
      pill.style.width = `${selected.offsetWidth}px`;
      pill.style.height = `${selected.offsetHeight}px`;
      if (first) { pill.dataset.placed = 'true'; void pill.offsetWidth; pill.style.transition = ''; }
      const scroller = track.parentElement;
      if (scroller && (selected.offsetLeft < scroller.scrollLeft || selected.offsetLeft + selected.offsetWidth > scroller.scrollLeft + scroller.clientWidth)) {
        scroller.scrollTo({ left: Math.max(0, selected.offsetLeft - 2), behavior: reducedMotion() || first ? 'auto' : 'smooth' });
      }
    };
    measure();
    const observer = new ResizeObserver(() => { cancelAnimationFrame(frame); frame = requestAnimationFrame(measure); });
    observer.observe(track);
    track.querySelectorAll('button').forEach((button) => observer.observe(button));
    return () => { observer.disconnect(); cancelAnimationFrame(frame); };
  }, [currentPeriod, periodSignature]);

  useEffect(() => {
    const outside = (event: PointerEvent) => {
      if (rootRef.current && !rootRef.current.contains(event.target as Node)) { setPinned(null); setFocused(null); setHovered(null); }
    };
    document.addEventListener('pointerdown', outside);
    const media = window.matchMedia('(prefers-reduced-motion: reduce)');
    const stopMotion = () => { if (media.matches) rootRef.current?.getAnimations({ subtree: true }).forEach((animation) => animation.cancel()); };
    media.addEventListener('change', stopMotion);
    return () => { document.removeEventListener('pointerdown', outside); media.removeEventListener('change', stopMotion); };
  }, []);

  const changePeriod = (index: number) => {
    if (!periods[index]) return;
    // Reordered DOM rows can generate pointerenter beneath a stationary pointer.
    // Keep the highlighted identity until the user actually moves that pointer.
    if (hovered) preserveHover.current = true;
    if (period === undefined) setInnerPeriod(periods[index].id);
    onPeriodChange?.(periods[index].id);
  };
  const enterItem = (event: { pointerType: string; clientX: number; clientY: number }, item: RememberedItem) => {
    if (event.pointerType !== 'mouse' || !item.present || preserveHover.current) return;
    pointerPosition.current = { x: event.clientX, y: event.clientY };
    setHovered(item.id);
  };
  const moveItem = (event: { pointerType: string; clientX: number; clientY: number }, item: RememberedItem) => {
    if (event.pointerType !== 'mouse' || !item.present) return;
    const old = pointerPosition.current;
    if (!old || old.x !== event.clientX || old.y !== event.clientY) {
      preserveHover.current = false;
      pointerPosition.current = { x: event.clientX, y: event.clientY };
      setHovered(item.id);
    }
  };
  // Keyboard focus owns the reading: a stationary pointer's earlier hover yields until that pointer really moves again.
  const focusRow = (itemId: string) => {
    if (hovered) { preserveHover.current = true; setHovered(null); }
    setFocused(itemId);
  };
  const leaveItems = () => { preserveHover.current = false; pointerPosition.current = null; setHovered(null); };
  const periodKey = (event: KeyboardEvent<HTMLButtonElement>, index: number) => {
    const offset = ['ArrowRight', 'ArrowDown'].includes(event.key) ? 1 : ['ArrowLeft', 'ArrowUp'].includes(event.key) ? -1 : 0;
    if (!offset && !['Home', 'End'].includes(event.key)) return;
    event.preventDefault();
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? periods.length - 1 : (index + offset + periods.length) % periods.length;
    changePeriod(next);
    requestAnimationFrame(() => periodRef.current?.querySelector<HTMLButtonElement>('[aria-checked="true"]')?.focus({ preventScroll: true }));
  };
  const hasDelta = delta !== undefined && Number.isFinite(delta);
  const deltaText = hasDelta ? `${Math.abs(delta).toLocaleString('en-US', { maximumFractionDigits: 1 })}%` : '';
  const summary = text.summary(totalLabel, format(total), hasDelta ? { percent: deltaText, decrease: delta < 0 } : null);
  const readingName = (item: RememberedItem) => text.row(item.label, format(item.value), shareText(shareOf(item.value, total)));
  const activeShare = active ? shareText(shareOf(amounts.values[active.id] ?? 0, amounts.total)) : '0%';

  return <section ref={rootRef} className={`aui-compact-breakdown ${className}`} role="group" aria-labelledby={`${id}-title`} onKeyDown={(event) => { if (event.key === 'Escape') { setPinned(null); setHovered(null); setFocused(null); } }}>
    <style>{styles}</style>
    <div className="aui-compact-breakdown-tray">
      <div className="aui-compact-breakdown-card">
        <header className="aui-compact-breakdown-header">
          <h3 id={`${id}-title`} className="aui-compact-breakdown-title" title={title} dir="auto">{title}</h3>
          {periods.length > 1 && <div className="aui-compact-breakdown-period-scroll"><div ref={periodRef} className="aui-compact-breakdown-periods" role="radiogroup" aria-label={text.periods}>
            <span ref={pillRef} className="aui-compact-breakdown-pill" aria-hidden="true" />
            {periods.map((item, index) => <button key={item.id} type="button" className="aui-compact-breakdown-period" role="radio" aria-checked={activePeriod === index} tabIndex={activePeriod === index ? 0 : -1} onClick={() => changePeriod(index)} onKeyDown={(event) => periodKey(event, index)} title={item.label}>{item.label}</button>)}
          </div></div>}
        </header>
        <div className="aui-compact-breakdown-summary" aria-hidden="true">
          <div className="aui-compact-breakdown-headline"><span className="aui-compact-breakdown-number">{format(amounts.headline)}</span>{hasDelta && !active && <span key={`${currentPeriod}-${delta}`} className="aui-compact-breakdown-delta" data-negative={delta < 0}>{delta < 0 ? <ArrowDown size={12} strokeWidth={1.6} /> : <ArrowUp size={12} strokeWidth={1.6} />}{deltaText}</span>}</div>
          <div className="aui-compact-breakdown-description"><span key={active?.id ?? 'total'} className="aui-compact-breakdown-description-content">{active ? <><span className="aui-compact-breakdown-dot" style={{ background: active.color }} /><span className="aui-compact-breakdown-description-name" dir="auto">{active.label}</span><span className="aui-compact-breakdown-description-share">{activeShare}</span></> : <span className="aui-compact-breakdown-description-name">{totalLabel}</span>}</span></div>
        </div>
        <div className="aui-compact-breakdown-bar-scroll" aria-hidden="true"><div className="aui-compact-breakdown-track" style={{ minWidth: segments.length ? segments.length * 4 - 2 : 0 }} onPointerLeave={leaveItems}>{segments.map((item) => <span key={item.id} className="aui-compact-breakdown-segment" data-item-id={item.id} data-active={active?.id === item.id} style={{ flex: `${amounts.values[item.id] ?? 0} 1 0px`, background: item.color, opacity: active && active.id !== item.id ? .25 : 1 }} onPointerEnter={(event) => enterItem(event, item)} onPointerMove={(event) => moveItem(event, item)} />)}</div></div>
        <ol ref={listRef} className="aui-compact-breakdown-list" aria-label={text.list} onPointerLeave={leaveItems}>{sorted.map((item) => {
          const value = amounts.values[item.id] ?? 0;
          const chosen = selectedId != null && item.source === selectedId;
          // The keyboard reaches the row button with onSelect; without it the list item itself takes focus, so a row is never
          // a button (or a toggle) that does nothing for the host.
          const reading = {
            tabIndex: item.present ? 0 : -1, 'aria-label': readingName(item), 'aria-current': chosen || undefined,
            onFocus: (event: FocusEvent<HTMLElement>) => { if (event.currentTarget.matches(':focus-visible')) focusRow(item.id); },
            onBlur: () => setFocused(null),
          };
          const row = {
            className: 'aui-compact-breakdown-row', 'data-item-id': item.id, 'data-active': active?.id === item.id, 'data-selected': chosen || undefined,
            style: { opacity: active && active.id !== item.id ? .45 : 1 },
            onPointerEnter: (event: ReactPointerEvent<HTMLElement>) => enterItem(event, item), onPointerMove: (event: ReactPointerEvent<HTMLElement>) => moveItem(event, item),
            onPointerDown: (event: ReactPointerEvent<HTMLElement>) => setPointerKind(event.pointerType),
          };
          const cells = <><span className="aui-compact-breakdown-dot" style={{ background: item.color }} aria-hidden="true" /><span className="aui-compact-breakdown-label" title={item.label} dir="auto">{item.label}</span><span className="aui-compact-breakdown-value">{format(value)}</span><span className="aui-compact-breakdown-share">{shareText(shareOf(value, amounts.total))}</span></>;
          return <li key={item.id} className="aui-compact-breakdown-list-item" data-item-id={item.id} data-exiting={!item.present} aria-hidden={!item.present || undefined} {...(onSelect ? {} : reading)}>
            {onSelect ? <button type="button" {...row} {...reading} disabled={!item.present} aria-describedby={text.selectHint ? `${id}-select-hint` : undefined} onClick={() => { if (item.present) onSelect(item.source); }}>{cells}</button>
              : <div {...row} onClick={(event) => { if (event.detail > 0 && (pointerKind === 'touch' || pointerKind === 'pen' || window.matchMedia('(pointer: coarse)').matches)) { setPinned((old) => old === item.id ? null : item.id); setFocused(null); setHovered(null); } }}>{cells}</div>}
          </li>;
        })}</ol>
        {onSelect && text.selectHint && <span id={`${id}-select-hint`} className="aui-compact-breakdown-sr">{text.selectHint}</span>}
        {active && <p className="aui-compact-breakdown-full-readout" data-long={active.label.length > 24} aria-hidden="true">{active.label} · {format(active.value)} · {shareText(shareOf(active.value, total))}</p>}
        {!items.length && <p className="aui-compact-breakdown-empty">{text.empty}</p>}
      </div>
      {!hideFooter && footer !== null && <div className="aui-compact-breakdown-footer">{footer !== undefined ? footer : leader ? <span key={leader.id} className="aui-compact-breakdown-footer-auto" title={`${text.leader} ${leader.label}`}>{text.leader} <strong dir="auto">{leader.label}</strong></span> : null}</div>}
      <span className="aui-compact-breakdown-sr" aria-live="polite" aria-atomic="true">{summary}</span>
      <span className="aui-compact-breakdown-sr" aria-live="polite" aria-atomic="true">{active ? readingName(active) : ''}</span>
    </div>
  </section>;
}

export type CompactBreakdownDemoState = 'default' | 'empty' | 'long' | 'transitions' | 'zero' | 'tiny' | 'many' | 'custom-footer' | 'hidden-footer' | 'rapid' | 'duplicates' | 'selectable' | 'english';

const demoPeriods = [{ id: 'week', label: '7일' }, { id: 'month', label: '30일' }, { id: 'quarter', label: '90일' }];
const money = (value: number) => `$${value.toLocaleString('en-US', { maximumFractionDigits: 0 })}`;

const demoModels: CompactBreakdownItem[] = [
  { id: 'claude', label: 'Claude · 구현', value: 620 }, { id: 'codex', label: 'Codex · 도구 호출', value: 548 },
  { id: 'research', label: '연구 모델', value: 252 }, { id: 'review', label: '검수 모델', value: 168 }, { id: 'ops', label: '운영 모델', value: 92 },
];

/** The host opens a row: onSelect hands the id over and selectedId marks the open row; choosing it again closes it here. */
function CompactBreakdownSelectDemo() {
  const [selected, setSelected] = useState<string | null>('codex');
  const open = demoModels.find((item) => item.id === selected);
  return <div className="aui-compact-breakdown-demo">
    <CompactBreakdown items={demoModels} title="모델별 실행 비용" format={money} totalLabel="전체 에이전트 실행 비용" onSelect={(id) => setSelected((current) => current === id ? null : id)} selectedId={selected} />
    <div className="aui-compact-breakdown-demo-controls"><span role="status">{open ? `${open.label} 상세를 열었어요. 같은 줄을 다시 누르면 닫혀요.` : '줄을 누르면 호스트가 상세를 열어요.'}</span></div>
  </div>;
}

const englishBreakdownLabels: Partial<CompactBreakdownLabels> = {
  title: 'Usage by agent', total: 'All run credits', periods: 'Usage period', list: 'Usage rank and share',
  row: (label, value, share) => `${label}, ${value}, ${share} of the total`,
  summary: (total, amount, change) => `${total}: ${amount}.${change ? ` ${change.decrease ? 'Down' : 'Up'} ${change.percent} from the previous period.` : ''}`,
  empty: 'Usage by part shows up after the first agent run.', leader: 'Top usage:', selectHint: 'Opens the details.',
};

/** An English host passes every label; the second card is the empty state. */
function CompactBreakdownEnglishDemo() {
  const [period, setPeriod] = useState('month');
  const values = period === 'week' ? [126, 218, 84] : [620, 548, 252];
  const items = ['Claude · build', 'Codex · tool calls', 'Research'].map((label, index) => ({ id: `en-${index}`, label, value: values[index] }));
  return <div className="aui-compact-breakdown-demo" style={{ display: 'grid', gap: 12 }}>
    <CompactBreakdown items={items} periods={[{ id: 'week', label: '7 days' }, { id: 'month', label: '30 days' }]} period={period} onPeriodChange={setPeriod} delta={period === 'week' ? -2.4 : 6.1} format={money} labels={englishBreakdownLabels} />
    <CompactBreakdown items={[]} labels={englishBreakdownLabels} />
  </div>;
}

/** The host owns period snapshots; the control only announces a period selection. */
export function CompactBreakdownDemo({ state = 'default' }: { state?: CompactBreakdownDemoState }) {
  if (state === 'selectable') return <CompactBreakdownSelectDemo />;
  if (state === 'english') return <CompactBreakdownEnglishDemo />;
  return <CompactBreakdownStateDemo state={state} />;
}

function CompactBreakdownStateDemo({ state }: { state: Exclude<CompactBreakdownDemoState, 'selectable' | 'english'> }) {
  const [period, setPeriod] = useState('month');
  const [notifications, setNotifications] = useState(0);
  const [refresh, setRefresh] = useState(0);
  const timers = useRef<ReturnType<typeof setTimeout>[]>([]);
  useEffect(() => () => timers.current.forEach(clearTimeout), []);
  const values = period === 'week' ? [126, 218, 84, 46, 32] : period === 'quarter' ? [1480, 1830, 720, 920, 350] : [620, 548, 252, 168, 92];
  let data: CompactBreakdownItem[] = ['Claude · 구현', 'Codex · 도구 호출', '연구 모델', '검수 모델', '운영 모델'].map((label, index) => ({ id: `model-${index}`, label, value: values[index] }));
  if (state === 'empty') data = [];
  if (state === 'long') data = [
    { id: 'research', label: '한국어 문서와 릴레이 실행 기록을 모두 검토하는 연구 모델', value: 284 },
    { id: 'release', label: 'agentdesk_release_verification_worker_20261018_extended', value: 242 },
    { id: 'review', label: '독립 리뷰와 회귀 검수', value: 96 },
    { id: 'relay', label: 'Discord 릴레이 분석', value: 62 },
    { id: 'archive', label: '장기 실행 기록 보관', value: 38 },
    { id: 'sixth', label: '여섯 번째 보조 모델', value: 22 },
    { id: 'seventh', label: '일곱 번째 대기 모델', value: 12 },
  ];
  if (state === 'transitions') {
    const snapshots: Record<string, CompactBreakdownItem[]> = {
      month: [{ id: 'a', label: '구현 워커', value: 520, color: '#8b5cf6' }, { id: 'b', label: '도구 호출', value: 410 }, { id: 'c', label: '릴레이 분석', value: 240 }, { id: 'd', label: '독립 검수', value: 130 }, { id: 'e', label: '장기 보관', value: 80 }, { id: 'f', label: '대기 워커', value: 40 }],
      week: [{ id: 'c', label: '릴레이 분석 · 이름 갱신', value: 610 }, { id: 'b', label: '도구 호출', value: 185, color: '#e09b35' }, { id: 'a', label: '구현 워커 · 이름 갱신', value: 210 }, { id: 'g', label: '새 실험 워커', value: 155 }, { id: 'e', label: '장기 보관', value: 80 }, { id: 'f', label: '대기 워커', value: 0 }],
      quarter: [{ id: 'f', label: '대기 워커', value: 80 }, { id: 'e', label: '장기 보관', value: 80 }, { id: 'd', label: '독립 검수 · 재등장', value: 820 }, { id: 'c', label: '릴레이 분석', value: 220 }, { id: 'b', label: '도구 호출 · 색 기억', value: 310 }, { id: 'a', label: '구현 워커', value: 740 }],
    };
    data = snapshots[period];
  }
  if (state === 'zero') data = [{ id: 'zero', label: '측정된 0', value: 0 }, { id: 'negative', label: '음수는 0으로 정규화', value: -42 }, { id: 'invalid', label: '비정상 입력 방어', value: Number.NaN }];
  if (state === 'tiny') data = [{ id: 'large', label: '전체 실행', value: 9989 }, { id: 'threshold', label: '정확히 0.05% · 숨긴 조각', value: 5 }, { id: 'above', label: '0.06% · 최소 2px 조각', value: 6 }, { id: 'zero', label: '0인 현재 항목', value: 0 }];
  if (state === 'many') data = Array.from({ length: 28 }, (_, index) => ({ id: `worker-${index}`, label: `분산 실행 워커 ${String(index + 1).padStart(2, '0')}`, value: 280 - index * 7 }));
  if (state === 'duplicates') {
    const shift = period === 'week' ? 3 : period === 'quarter' ? -2 : 0;
    data = [
      { label: '독립 검수', value: 4990 - shift }, { label: '독립 검수', value: 6 + shift },
      { id: 'relay', label: '릴레이 분석 · 1차', value: 4 }, { id: 'relay', label: '릴레이 분석 · 재시도', value: 0 },
    ];
  }
  if (state === 'rapid') data = data.map((item, index) => ({ ...item, value: item.value * (1 + ((refresh + index) % 3) * .4) }));
  const choose = (next: string) => { setPeriod(next); setNotifications((count) => count + 1); };
  const rapid = () => {
    timers.current.forEach(clearTimeout);
    [0, 160, 320, 480].forEach((delay, index) => timers.current.push(setTimeout(() => { setRefresh((value) => value + 1); choose(demoPeriods[index % 3].id); }, delay)));
  };
  return <div className="aui-compact-breakdown-demo" style={state === 'long' ? { maxWidth: 250 } : undefined}>
    <CompactBreakdown items={data} title={state === 'long' ? '모델별 실행 비용과 긴 식별자' : '모델별 실행 비용'} periods={state === 'custom-footer' ? [{ id: 'month', label: '이번 달' }] : state === 'duplicates' ? [{ id: 'week', label: '최근 7일 · 재시도 포함' }, { id: 'month', label: '최근 30일 · 모든 에이전트 실행' }, { id: 'quarter', label: '이번 분기 전체 집계' }] : demoPeriods} period={period} onPeriodChange={choose} delta={state === 'zero' ? 0 : state === 'duplicates' ? Number.NaN : period === 'quarter' ? -3.8 : period === 'week' ? 0 : 8.4} format={money} totalLabel="전체 에이전트 실행 비용" footer={state === 'custom-footer' ? 0 : state === 'hidden-footer' ? null : undefined} />
    {(state === 'rapid' || state === 'transitions' || state === 'custom-footer') && <div className="aui-compact-breakdown-demo-controls">{state === 'rapid' && <button type="button" onClick={rapid}>빠른 연속 갱신</button>}<span role="status">{state === 'custom-footer' ? '기간 1개는 선택기를 숨기고, 사용자 받침 0은 표시합니다.' : `기간 알림 ${notifications}회`}</span></div>}
  </div>;
}
