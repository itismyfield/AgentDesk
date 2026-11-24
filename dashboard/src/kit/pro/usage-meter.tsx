// Kit provenance: itismyfield/agentdesk-ui-kit@b6d2ab1
import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type KeyboardEvent, type PointerEvent as ReactPointerEvent, type ReactNode, type RefObject } from 'react';
import { ArrowLeft, ArrowRight, CalendarDays } from 'lucide-react';

// Clean-room implementation from clean-specs/usage-meter (11/17 revision).
// No affectUI Pro source, registry, package, site assets, or original copy is used.

export interface UsageCategory { label: string; value: number; color?: string }
export interface UsageLimit {
  label: string;
  /**
   * null is an unmeasured limit (unsupported or stale source): no fill and a dash instead of a number. Earlier versions
   * typed this as `number`; code that reads `used` back must handle null.
   */
  used: number | null;
  /** null, undefined or a non-finite value is unlimited. */
  total?: number | null;
  unit?: string;
  decimals?: number;
  /** `limits` layout: rows with the same group sit under one heading, e.g. a provider name. Unique per group + label. */
  group?: string;
  /** `limits` layout: short line under the row, e.g. when the limit resets. */
  detail?: string;
}
export interface UsageDay { label: string; /** null is an unmeasured day, rather than a measured zero. */ value: number | null }
export type UsageMeterState = 'ok' | 'warn' | 'limit';
export interface UsageMeterLabels {
  /** Warning dot name and footer heading at or over the limit. */
  limitReached: string;
  /** Warning dot name close to the limit. */
  nearLimit: string;
  /** Footer heading close to the limit. */
  nearLimitStatus: string;
  /** Warning dot name and spoken suffix at or over `dangerAt`. */
  danger: string;
  /** Footer heading at or over `dangerAt`. */
  dangerStatus: string;
  /** Polite announcement when the reading reaches the `dangerAt` level. */
  announceDanger: (title: string, actual: string, percent: number) => string;
  /** Footer tail over the limit, from the formatted amount with unit. */
  over: (amount: ReactNode) => ReactNode;
  /** Footer tail with what is left, from the formatted amount with unit. */
  remaining: (amount: ReactNode) => ReactNode;
  resetToday: string;
  resetTomorrow: string;
  resetInDays: (days: number) => string;
  /** Reset more than 60 days away, or the absolute reset date in the tooltip. */
  resetOn: (date: string) => string;
  /** Polite announcement when the state changes. */
  announce: (state: UsageMeterState, title: string, actual: string, percent: number) => string;
  /** Accessible name of the period switch. */
  periods: string;
  /** Accessible name of the stacked bar, from "used / total". */
  barHint: (actual: string) => string;
  /** Accessible name of the total meter. */
  meter: string;
  /** Accessible name of the bar when the plan total is zero. */
  zeroTotal: (amount: string) => string;
  /** Accessible name of the category list. */
  categories: string;
  empty: string;
  daily: string;
  /** Daily heading readout without a selected day, from the formatted average. */
  average: (amount: string) => ReactNode;
  /** Accessible name of the daily bars. */
  dailyHint: (title: string, average: string) => string;
  dayTools: string;
  previousDay: string;
  nextDay: string;
  daySelect: string;
  averageOption: string;
  /** Added to a day option without a value. */
  unmeasuredDay: string;
  /** Accessible name of the limit list. */
  limits: string;
  unlimited: string;
  notMeasured: string;
}
export interface UsageMeterProps {
  /** Plan total of the `meter` layout. Ignored by the `limits` layout. */
  total?: number;
  /** Usage by category of the `meter` layout. Ignored by the `limits` layout. */
  used?: UsageCategory[];
  limits?: UsageLimit[];
  /**
   * `meter` (default): plan total, stacked bar, categories, daily bars and limit tiles. `limits`: only the header and a
   * compact list of small limit bars (for example 5-hour and weekly ratios per provider, grouped by `group`).
   */
  layout?: 'meter' | 'limits';
  periods?: { id: string; label: string }[];
  period?: string;
  defaultPeriod?: string;
  onPeriodChange?: (id: string) => void;
  resetAt?: Date | string | number;
  daily?: UsageDay[];
  /** Shows the upgrade / top-up button. Without it there is no button. */
  onUpgrade?: (state: UsageMeterState) => void;
  title?: string;
  plan?: string;
  unit?: string;
  /** Ratio (0–1) where the warning level starts. Defaults to 0.8. */
  warnAt?: number;
  /**
   * Optional ratio between `warnAt` and the limit where a stronger danger level starts, for example an auto-pause rule at
   * 0.9: red fill, value and dot, a threshold tick on limit bars, its own footer heading and announcement. Values at or below
   * `warnAt` start it at `warnAt`; 1 or more never reach it. Omitted keeps the two levels. `onUpgrade` and
   * `labels.announce` still receive `'warn'` for it.
   */
  dangerAt?: number;
  upgradeLabel?: string;
  topUpLabel?: string;
  /** Number and absolute-date formatting locale. Defaults to ko-KR. */
  locale?: string;
  /** Every other visible and spoken text. Missing keys keep the Korean defaults. */
  labels?: Partial<UsageMeterLabels>;
  className?: string;
}

type UsageState = UsageMeterState;
/** `danger` only exists with `dangerAt`; it is a stronger `warn`. */
type UsageLevel = UsageState | 'danger';

export const usageMeterDefaultLabels: UsageMeterLabels = {
  limitReached: '한도 도달',
  nearLimit: '한도에 가까움',
  nearLimitStatus: '한도 임박',
  danger: '위험 기준 이상',
  dangerStatus: '위험 기준 이상',
  announceDanger: (title, _actual, percent) => `${title}, 위험 기준 이상이에요. ${percent}%.`,
  over: (amount) => <>{amount} 초과</>,
  remaining: (amount) => <>{amount} 남음</>,
  resetToday: '오늘 초기화',
  resetTomorrow: '내일 초기화',
  resetInDays: (days) => `${days}일 후 초기화`,
  resetOn: (date) => `${date} 초기화`,
  announce: (state, title, actual, percent) => state === 'limit' ? `${title}, 한도 도달. ${actual}.` : state === 'warn' ? `${title}, 한도가 얼마 남지 않았습니다. ${percent}%.` : `${title}, ${actual} 사용.`,
  periods: '사용량 집계 기간',
  barHint: (actual) => `${actual} 사용 — 좌우 화살표로 용도별로 읽을 수 있음`,
  meter: '총 사용량',
  zeroTotal: (amount) => `총량 0 · ${amount} 사용`,
  categories: '용도별',
  empty: '아직 사용 기록이 없습니다.',
  daily: '하루별',
  average: (amount) => <>평균 <strong>{amount}</strong> / 하루</>,
  dailyHint: (title, average) => `${title} 하루별, 평균 ${average} — 좌우 화살표로 날별로 읽을 수 있음`,
  dayTools: '날짜 선택 도구',
  previousDay: '이전 날짜',
  nextDay: '다음 날짜',
  daySelect: '일별 사용량 날짜',
  averageOption: '평균 보기',
  unmeasuredDay: '집계 전',
  limits: '요금제 한도',
  unlimited: '무제한',
  notMeasured: '측정 안 됨',
};
type UsageRow = UsageCategory & { color: string; exiting: boolean; entering: boolean };
type NumericSnapshot = Record<string, number>;
const palette = Array.from({ length: 5 }, (_, i) => `var(--aui-usage-meter-color-${i + 1})`);
const clean = (value: number) => Number.isFinite(value) ? Math.max(0, value) : 0;
/** Non-finite becomes 0; negatives are kept (used for limit usage and day values). */
const finiteOrZero = (value: number | null | undefined) => typeof value === 'number' && Number.isFinite(value) ? value : 0;
const clamp = (value: number, low: number, high: number) => Math.min(high, Math.max(low, value));
const limitIsInfinite = (value: number | null | undefined) => value == null || !Number.isFinite(value);
const usageState = (ratio: number, threshold: number): UsageState => ratio >= 1 ? 'limit' : ratio >= threshold ? 'warn' : 'ok';
const nkey = (kind: string, label: string) => `${kind}:${label}`;
/** Group-qualified identity, so "5h" under two providers stays two rows. */
const limitKey = (limit: UsageLimit) => `${limit.group ?? ''}\u0001${limit.label}`;

function useReducedMotion() {
  const [reduced, setReduced] = useState(() => typeof window !== 'undefined' && window.matchMedia?.('(prefers-reduced-motion: reduce)').matches === true);
  useEffect(() => {
    const media = window.matchMedia?.('(prefers-reduced-motion: reduce)');
    if (!media) return;
    const sync = () => setReduced(media.matches);
    sync();
    media.addEventListener('change', sync);
    return () => media.removeEventListener('change', sync);
  }, []);
  return reduced;
}

/** A single animation clock keeps every numeric value in the same telemetry snapshot. */
function useUsageNumbers(target: NumericSnapshot, root: RefObject<HTMLElement | null>, reduced: boolean) {
  const key = JSON.stringify(target);
  const [entered] = useState(true);
  const [numbers, setNumbers] = useState<NumericSnapshot>(() => ({ ...target }));
  const displayed = useRef(numbers);
  const hasStarted = useRef(false);
  useEffect(() => {
    if (!entered) return;
    const destination = JSON.parse(key) as NumericSnapshot;
    const publish = (next: NumericSnapshot) => { displayed.current = next; setNumbers(next); };
    if (!hasStarted.current || reduced) { hasStarted.current = true; publish(destination); return; }
    const origin = displayed.current;
    const duration = 650;
    let frame = 0;
    const start = performance.now();
    const tick = (now: number) => {
      const t = clamp((now - start) / duration, 0, 1);
      const eased = 1 - (1 - t) ** 4;
      const next: NumericSnapshot = {};
      for (const name of Object.keys(destination)) next[name] = (origin[name] ?? 0) + (destination[name] - (origin[name] ?? 0)) * eased;
      publish(next);
      if (t < 1) frame = requestAnimationFrame(tick);
    };
    frame = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(frame);
  }, [key, entered, reduced]);
  return { numbers, entered };
}

const levelName = (level: UsageLevel, labels: UsageMeterLabels) => level === 'limit' ? labels.limitReached : level === 'danger' ? labels.danger : level === 'warn' ? labels.nearLimit : '';

function WarningDot({ state, decorative = false, labels }: { state: UsageLevel; decorative?: boolean; labels: UsageMeterLabels }) {
  if (state === 'ok') return null;
  return <span key={state} className="aui-usage-meter-warning-dot" data-state={state} role={decorative ? undefined : 'img'} aria-hidden={decorative || undefined} aria-label={decorative ? undefined : levelName(state, labels)} />;
}

export function UsageMeter({ total = 0, used = [], limits = [], layout = 'meter', periods = [], period, defaultPeriod, onPeriodChange, resetAt, daily = [], onUpgrade, title = '사용량', plan, unit = '크레딧', warnAt = 0.8, dangerAt, upgradeLabel = '업그레이드', topUpLabel = '크레딧 추가', locale = 'ko-KR', labels: labelsProp, className = '' }: UsageMeterProps) {
  const text: UsageMeterLabels = { ...usageMeterDefaultLabels, ...labelsProp };
  const limitsLayout = layout === 'limits';
  const id = useId();
  const root = useRef<HTMLElement>(null);
  const card = useRef<HTMLDivElement>(null);
  const content = useRef<HTMLDivElement>(null);
  const periodTrack = useRef<HTMLDivElement>(null);
  const pill = useRef<HTMLSpanElement>(null);
  const upgrade = useRef<HTMLButtonElement>(null);
  const upgradeCopy = useRef<HTMLSpanElement>(null);
  const graphic = useRef<HTMLDivElement>(null);
  const dayTools = useRef<HTMLDivElement>(null);
  const dayToggle = useRef<HTMLButtonElement>(null);
  const reduced = useReducedMotion();
  const colorBook = useRef(new Map<string, string>());
  const categories = useMemo(() => used.map((item) => {
    if (!colorBook.current.has(item.label)) colorBook.current.set(item.label, palette[colorBook.current.size % palette.length]);
    return { ...item, value: clean(item.value), color: item.color ?? colorBook.current.get(item.label)!, exiting: false, entering: false };
  }), [used]);
  const categoryKey = JSON.stringify(categories);
  const [rows, setRows] = useState<UsageRow[]>(categories);
  const exitTimers = useRef(new Map<string, ReturnType<typeof setTimeout>>());
  const [internalPeriod, setInternalPeriod] = useState(defaultPeriod ?? periods[0]?.id ?? '');
  const currentPeriod = period ?? internalPeriod;
  const lastPeriod = useRef(currentPeriod);
  const [activeCategory, setActiveCategory] = useState<string | null>(null);
  const [selectedDay, setSelectedDay] = useState<number | null>(null);
  const [toolsOpen, setToolsOpen] = useState(false);
  const [announcement, setAnnouncement] = useState('');
  const [feedback, setFeedback] = useState('');
  const amount = categories.reduce((sum, item) => sum + item.value, 0);
  const maximum = clean(total);
  const knownTotal = maximum > 0;
  const threshold = Number.isFinite(warnAt) ? clamp(warnAt, 0, 1) : 0.8;
  // A zero plan with positive usage is already over its limit.
  const ratio = knownTotal ? amount / maximum : amount > 0 ? Infinity : 0;
  const meterState = usageState(ratio, threshold);
  const dangerLine = typeof dangerAt === 'number' && Number.isFinite(dangerAt) ? clamp(dangerAt, threshold, 1) : null;
  const levelOf = (state: UsageState, reading: number): UsageLevel => state === 'warn' && dangerLine !== null && reading >= dangerLine ? 'danger' : state;
  // Days without an explicit value are still to come; malformed numbers count as measured zeros.
  // Finite daily numbers keep their sign for the average and the selected reading; bar heights floor at 4%.
  const measured = daily.filter((day) => day.value !== null && day.value !== undefined);
  const dayValue = (day: UsageDay) => finiteOrZero(day.value);
  const average = measured.length ? measured.reduce((sum, day) => sum + dayValue(day), 0) / measured.length : 0;
  const maximumDay = Math.max(1, ...measured.map(dayValue));
  const format = (value: number, digits = 0, fixed = false) => {
    const places = Number.isFinite(digits) ? clamp(Math.floor(digits), 0, 20) : 0;
    return new Intl.NumberFormat(locale, { minimumFractionDigits: fixed ? places : 0, maximumFractionDigits: places }).format(finiteOrZero(value));
  };
  const withUnit = (value: number) => `${format(value)}\u00a0${unit}`;
  /** Category values between 0 and 1 read as <1 regardless of the animation frame. */
  const categoryText = (target: number, shown: number) => target > 0 && target < 1 ? '<1' : format(shown);
  const percentText = (shown: number) => shown > 999 ? '>999%' : `${format(shown)}%`;

  /** One reading per limit for both layouts; unmeasured and unlimited limits never warn. */
  const limitModels = limits.map((limit) => {
    const key = limitKey(limit);
    const measured = limit.used !== null && limit.used !== undefined;
    const infinite = limitIsInfinite(limit.total);
    const limitTotal = infinite ? 0 : clean(limit.total!);
    const usedValue = finiteOrZero(limit.used);
    const limitRatio = infinite || !measured ? 0 : usedValue / Math.max(limitTotal, 0.000000001);
    const limitState: UsageState = infinite || !measured ? 'ok' : usageState(limitRatio, threshold);
    const decimals = limit.decimals ?? (Number.isInteger(usedValue) && (infinite || Number.isInteger(limitTotal)) ? 0 : 1);
    const actualUsed = measured ? format(usedValue, decimals, true) : '–';
    const actualMaximum = infinite ? '∞' : format(limitTotal, Number.isInteger(limitTotal) ? 0 : decimals, true);
    const actualValue = `${actualUsed} / ${actualMaximum}${!infinite && limit.unit ? `\u00a0${limit.unit}` : ''}`;
    const limitLevel = levelOf(limitState, limitRatio);
    return { limit, key, measured, infinite, limitTotal, usedValue, limitRatio, limitState, limitLevel, decimals, actualUsed, actualMaximum, actualValue };
  });
  const worstLimit = limitModels.reduce<(typeof limitModels)[number] | null>((worst, model) => model.measured && !model.infinite && (!worst || model.limitRatio > worst.limitRatio) ? model : worst, null);
  const state: UsageState = limitsLayout ? worstLimit?.limitState ?? 'ok' : meterState;
  const level: UsageLevel = limitsLayout ? worstLimit?.limitLevel ?? 'ok' : levelOf(meterState, ratio);
  const previousLevel = useRef(level);
  /** Threshold tick on single-fill limit bars when a danger level is set. */
  const dangerTick = (finiteBar: boolean) => dangerLine !== null && dangerLine < 1 && finiteBar ? <span className="aui-usage-meter-danger-tick" aria-hidden="true" style={{ left: `${dangerLine * 100}%` }} /> : null;

  // Removed rows stay at their previous index. Reappearing names cancel their exit timer.
  useLayoutEffect(() => {
    const next = JSON.parse(categoryKey) as UsageRow[];
    const nextNames = new Set(next.map((item) => item.label));
    for (const [name, timer] of exitTimers.current) if (nextNames.has(name)) { clearTimeout(timer); exitTimers.current.delete(name); }
    setRows((previous) => {
      const oldNames = new Set(previous.map((item) => item.label));
      const merged = next.map((item) => ({ ...item, entering: !oldNames.has(item.label) }));
      previous.forEach((old, index) => {
        if (nextNames.has(old.label)) return;
        if (reduced) return;
        merged.splice(Math.min(index, merged.length), 0, { ...old, value: 0, exiting: true, entering: false });
        if (!exitTimers.current.has(old.label)) {
          const timer = setTimeout(() => {
            exitTimers.current.delete(old.label);
            setRows((current) => current.filter((row) => row.label !== old.label || !row.exiting));
          }, 460);
          exitTimers.current.set(old.label, timer);
        }
      });
      return merged;
    });
    setActiveCategory((name) => name && next.some((item) => item.label === name && item.value > 0) ? name : null);
  }, [categoryKey, reduced]);
  useEffect(() => () => { for (const timer of exitTimers.current.values()) clearTimeout(timer); }, []);

  const targets: NumericSnapshot = { amount, total: maximum, percent: Math.min(ratio * 100, 1000), remaining: Math.max(0, maximum - amount), over: Math.max(0, amount - maximum), average };
  rows.forEach((row) => { targets[nkey('category', row.label)] = row.value; });
  limitModels.forEach((model) => {
    targets[nkey('limit', model.key)] = model.usedValue;
    targets[nkey('limit-percent', model.key)] = Math.min(model.limitRatio * 100, 1000);
    if (!model.infinite) targets[nkey('maximum', model.key)] = model.limitTotal;
  });
  daily.forEach((day, index) => { targets[nkey('day', String(index))] = dayValue(day); });
  const { numbers, entered } = useUsageNumbers(targets, root, reduced);
  const value = (name: string) => numbers[name] ?? 0;
  const scale = Math.max(maximum, amount, 1);
  const sumOfRows = rows.reduce((sum, row) => sum + value(nkey('category', row.label)), 0);
  const current = categories.find((item) => item.label === activeCategory && item.value > 0);
  const selected = selectedDay !== null ? daily[selectedDay] : undefined;
  const toggledLabel = state === 'ok' ? upgradeLabel : topUpLabel;
  const resetDate = resetAt === undefined ? undefined : new Date(resetAt);
  const hasReset = !!resetDate && Number.isFinite(resetDate.getTime());
  const daysLeft = hasReset ? Math.ceil((resetDate!.getTime() - Date.now()) / 86_400_000) : 0;
  const resetLabel = daysLeft <= 0 ? text.resetToday : daysLeft === 1 ? text.resetTomorrow : daysLeft <= 60 ? text.resetInDays(daysLeft) : text.resetOn(new Intl.DateTimeFormat(locale, { year: 'numeric', month: 'long', day: 'numeric' }).format(resetDate));
  const absoluteReset = hasReset ? text.resetOn(new Intl.DateTimeFormat(locale, { month: 'long', day: 'numeric' }).format(resetDate)) : '';

  useEffect(() => {
    if (lastPeriod.current !== currentPeriod) {
      lastPeriod.current = currentPeriod;
      setActiveCategory(null); setSelectedDay(null); setFeedback('');
    }
  }, [currentPeriod]);
  useEffect(() => { setSelectedDay((index) => index !== null && index >= daily.length ? null : index); }, [daily.length]);
  useEffect(() => {
    if (previousLevel.current === level) return;
    previousLevel.current = level;
    const actual = limitsLayout ? worstLimit?.actualValue ?? '' : `${withUnit(amount)} / ${withUnit(maximum)}`;
    const percent = Math.round((limitsLayout ? worstLimit?.limitRatio ?? 0 : ratio) * 100);
    setAnnouncement(level === 'danger' ? text.announceDanger(title, actual, percent) : text.announce(state, title, actual, percent));
    // Announcements follow level changes only, never the numeric animation or period alone.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [level]);

  // An observed inner box gives the outer card an animatable explicit height.
  useLayoutEffect(() => {
    const outer = card.current;
    const inner = content.current;
    if (!outer || !inner) return;
    const sync = () => {
      const height = inner.getBoundingClientRect().height;
      if (outer.style.height !== `${height}px`) outer.style.height = `${height}px`;
    };
    sync();
    outer.dataset.ready = 'true';
    const observer = typeof ResizeObserver === 'undefined' ? undefined : new ResizeObserver(sync);
    observer?.observe(inner);
    return () => observer?.disconnect();
  }, []);

  useLayoutEffect(() => {
    const track = periodTrack.current;
    const indicator = pill.current;
    if (!track || !indicator) return;
    const place = () => {
      const button = track.querySelector<HTMLButtonElement>('[aria-checked="true"]');
      if (!button || !button.offsetWidth) { indicator.style.opacity = '0'; return; }
      const first = !indicator.dataset.ready;
      if (first) indicator.style.transition = 'none';
      indicator.style.left = `${button.offsetLeft}px`;
      indicator.style.top = `${button.offsetTop}px`;
      indicator.style.width = `${button.offsetWidth}px`;
      indicator.style.height = `${button.offsetHeight}px`;
      indicator.style.opacity = '1';
      if (first) { void indicator.offsetWidth; indicator.dataset.ready = 'true'; indicator.style.transition = ''; }
    };
    place();
    const observer = typeof ResizeObserver === 'undefined' ? undefined : new ResizeObserver(place);
    observer?.observe(track);
    track.querySelectorAll('button').forEach((button) => observer?.observe(button));
    document.fonts?.ready.then(place);
    return () => observer?.disconnect();
  }, [currentPeriod, periods.length]);

  useLayoutEffect(() => {
    const button = upgrade.current;
    const copy = upgradeCopy.current;
    if (!button || !copy) return;
    const measure = () => {
      const width = Math.ceil(copy.getBoundingClientRect().width) + 28;
      const first = !button.dataset.ready;
      if (first) button.style.transition = 'none';
      button.style.width = `${width}px`;
      if (first) { void button.offsetWidth; button.dataset.ready = 'true'; button.style.transition = ''; }
    };
    measure();
    const observer = typeof ResizeObserver === 'undefined' ? undefined : new ResizeObserver(measure);
    observer?.observe(copy);
    return () => observer?.disconnect();
  }, [toggledLabel, !!onUpgrade]);

  useEffect(() => {
    const clearOutside = (event: PointerEvent) => {
      const target = event.target as Node;
      if (!graphic.current?.contains(target) && !dayTools.current?.contains(target) && !dayToggle.current?.contains(target) && !periodTrack.current?.contains(target)) setSelectedDay(null);
    };
    document.addEventListener('pointerdown', clearOutside);
    return () => document.removeEventListener('pointerdown', clearOutside);
  }, []);

  const changePeriod = (index: number) => {
    const next = periods[index];
    if (!next || next.id === currentPeriod) return;
    if (period === undefined) setInternalPeriod(next.id);
    setActiveCategory(null); setSelectedDay(null); setFeedback('');
    onPeriodChange?.(next.id);
  };
  const periodKeys = (event: KeyboardEvent<HTMLButtonElement>, index: number) => {
    const delta = ['ArrowRight', 'ArrowDown'].includes(event.key) ? 1 : ['ArrowLeft', 'ArrowUp'].includes(event.key) ? -1 : 0;
    if (!delta && event.key !== 'Home' && event.key !== 'End') return;
    event.preventDefault();
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? periods.length - 1 : (index + delta + periods.length) % periods.length;
    changePeriod(next);
    periodTrack.current?.querySelectorAll<HTMLButtonElement>('button')[next]?.focus();
  };
  const categoryAt = (clientX: number, element: HTMLElement) => {
    const rect = element.getBoundingClientRect();
    const x = clamp((clientX - rect.left) / Math.max(rect.width, 1), 0, 1) * 100;
    let end = 0;
    for (const row of rows) {
      const width = value(nkey('category', row.label)) / scale * 100;
      const start = end; end += width;
      if (!row.exiting && row.value > 0 && width > 0 && x >= start && x <= end) return row.label;
    }
    return null;
  };
  const toggleCategory = (name: string | null) => setActiveCategory((previous) => name && previous !== name ? name : null);
  const categoryPress = useRef<{ x: number; y: number } | null>(null);
  const focusesCurrentPeriod = (target: EventTarget | null) => target instanceof HTMLElement && periodTrack.current?.contains(target) && target.getAttribute('aria-checked') === 'true';
  const categoryKeys = (event: KeyboardEvent<HTMLDivElement>) => {
    if (event.key === 'Escape') { event.preventDefault(); setActiveCategory(null); return; }
    if (!['ArrowLeft', 'ArrowRight'].includes(event.key)) return;
    const choices = rows.filter((row) => !row.exiting && row.value > 0);
    if (!choices.length) return;
    event.preventDefault();
    const index = choices.findIndex((row) => row.label === activeCategory);
    const next = index < 0 ? (event.key === 'ArrowRight' ? 0 : choices.length - 1) : (index + (event.key === 'ArrowRight' ? 1 : -1) + choices.length) % choices.length;
    setActiveCategory(choices[next].label);
  };
  let cursor = 0;
  let center = 50;
  rows.forEach((row) => {
    const width = value(nkey('category', row.label)) / scale * 100;
    if (row.label === activeCategory) center = clamp(cursor + width / 2, 0, 100);
    cursor += width;
  });
  const retainedTip = useRef<{ label: string; color: string; value: number; share: number; center: number } | null>(null);
  if (current) retainedTip.current = { ...current, share: amount ? current.value / amount * 100 : 0, center };
  const tip = retainedTip.current;
  const selectionCopy = tip ? <><span className="aui-usage-meter-swatch" style={{ background: tip.color }} /><span className="aui-usage-meter-selected-name" title={tip.label} dir="auto">{tip.label}</span><strong>{categoryText(tip.value, tip.value)}&nbsp;{unit}</strong><span className="aui-usage-meter-selected-share">{format(tip.share)}%</span></> : null;

  const dayAt = (clientX: number, element: HTMLElement) => {
    const rect = element.getBoundingClientRect();
    return clamp(Math.floor((clientX - rect.left) / Math.max(rect.width, 1) * daily.length), 0, daily.length - 1);
  };
  const dayPress = useRef<{ id: number; x: number; y: number; direction: 'pending' | 'horizontal' | 'vertical' } | null>(null);
  const dayPointerDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.pointerType === 'mouse') { setSelectedDay(dayAt(event.clientX, event.currentTarget)); return; }
    dayPress.current = { id: event.pointerId, x: event.clientX, y: event.clientY, direction: 'pending' };
  };
  const dayPointerMove = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.pointerType === 'mouse') { setSelectedDay(dayAt(event.clientX, event.currentTarget)); return; }
    const press = dayPress.current;
    if (!press || press.id !== event.pointerId) return;
    const dx = Math.abs(event.clientX - press.x), dy = Math.abs(event.clientY - press.y);
    if (press.direction === 'pending' && Math.max(dx, dy) > 5) press.direction = dx > dy ? 'horizontal' : 'vertical';
    if (press.direction !== 'horizontal') return;
    setSelectedDay(dayAt(event.clientX, event.currentTarget));
  };
  const dayKeys = (event: KeyboardEvent<HTMLDivElement>) => {
    if (event.key === 'Escape') { event.preventDefault(); setSelectedDay(null); return; }
    if (!['ArrowLeft', 'ArrowRight'].includes(event.key)) return;
    event.preventDefault();
    setSelectedDay((previous) => previous === null ? event.key === 'ArrowRight' ? 0 : daily.length - 1 : clamp(previous + (event.key === 'ArrowRight' ? 1 : -1), 0, daily.length - 1));
  };

  const limitTiles = limitsLayout ? null : limitModels.length > 0 && <ul className="aui-usage-meter-limits" aria-label={text.limits} style={{ '--aui-usage-meter-limit-columns': Math.min(3, limitModels.length) } as CSSProperties}>
    {limitModels.map(({ limit, key, measured, infinite, limitTotal, usedValue, limitLevel, decimals, actualUsed, actualValue }) => <li key={key} className="aui-usage-meter-limit" data-state={limitLevel}>
      <div className="aui-usage-meter-limit-heading"><span title={limit.label} dir="auto">{limit.label}</span><WarningDot state={limitLevel} labels={text} /></div>
      <div className="aui-usage-meter-limit-value">{measured ? format(value(nkey('limit', key)), decimals, true) : '–'}<span> / {infinite ? '∞' : format(value(nkey('maximum', key)), Number.isInteger(limitTotal) ? 0 : decimals, true)}{!infinite && limit.unit ? `\u00a0${limit.unit}` : ''}</span></div>
      <div className="aui-usage-meter-limit-track" data-unlimited={infinite || undefined} role={infinite || !measured ? 'img' : 'meter'} aria-label={!measured ? `${limit.label}: ${text.notMeasured}` : infinite ? `${limit.label}: ${actualUsed}, ${text.unlimited}` : limit.label} aria-valuemin={infinite || !measured ? undefined : 0} aria-valuemax={infinite || !measured ? undefined : limitTotal} aria-valuenow={infinite || !measured ? undefined : Math.min(usedValue, limitTotal)} aria-valuetext={infinite || !measured ? undefined : actualValue}>{!infinite && measured && <span className="aui-usage-meter-limit-fill" style={{ width: `${Math.min(100, value(nkey('limit', key)) / Math.max(limitTotal, 0.000000001) * 100)}%` }} />}{dangerTick(!infinite && measured)}</div>
    </li>)}
  </ul>;

  const limitGroups: { name: string; rows: typeof limitModels }[] = [];
  if (limitsLayout) limitModels.forEach((model) => {
    const name = model.limit.group ?? '';
    const group = limitGroups.find((item) => item.name === name);
    if (group) group.rows.push(model); else limitGroups.push({ name, rows: [model] });
  });
  const limitList = limitsLayout && <div className="aui-usage-meter-mini" role="group" aria-label={text.limits}>
    {limitModels.length === 0 && <p className="aui-usage-meter-empty">{text.empty}</p>}
    {limitGroups.map((group) => <div key={group.name} className="aui-usage-meter-mini-group" role={group.name ? 'group' : undefined} aria-label={group.name || undefined}>
      {group.name && <h4 className="aui-usage-meter-mini-name" dir="auto">{group.name}</h4>}
      <ul className="aui-usage-meter-mini-rows">
        {group.rows.map(({ limit, key, measured, infinite, limitTotal, usedValue, limitRatio, limitLevel, actualUsed, actualValue }) => {
          const name = group.name ? `${group.name} ${limit.label}` : limit.label;
          return <li key={key} className="aui-usage-meter-mini-row" data-state={limitLevel} data-detail={limit.detail ? '' : undefined} title={`${name}: ${measured ? actualValue : text.notMeasured}`}>
            <span className="aui-usage-meter-mini-label" dir="auto" aria-hidden="true">{limit.label}</span>
            <span className="aui-usage-meter-mini-track" data-unlimited={infinite || undefined} data-unmeasured={!measured || undefined} role={infinite || !measured ? 'img' : 'meter'} aria-label={!measured ? `${name}: ${text.notMeasured}` : infinite ? `${name}: ${actualUsed}, ${text.unlimited}` : name} aria-valuemin={infinite || !measured ? undefined : 0} aria-valuemax={infinite || !measured ? undefined : limitTotal} aria-valuenow={infinite || !measured ? undefined : Math.min(usedValue, limitTotal)} aria-valuetext={infinite || !measured ? undefined : `${actualValue} (${percentText(Math.min(limitRatio * 100, 1000))})${limitLevel === 'ok' ? '' : `, ${levelName(limitLevel, text)}`}`}>
              {!infinite && measured && <span className="aui-usage-meter-mini-fill" style={{ width: `${Math.min(100, value(nkey('limit-percent', key)))}%` }} />}
              {dangerTick(!infinite && measured)}
            </span>
            <span className="aui-usage-meter-mini-value" aria-hidden="true"><WarningDot state={limitLevel} decorative labels={text} />{!measured ? '–' : infinite ? '∞' : percentText(value(nkey('limit-percent', key)))}</span>
            {limit.detail && <span className="aui-usage-meter-mini-detail" dir="auto">{limit.detail}</span>}
          </li>;
        })}
      </ul>
    </div>)}
  </div>;

  return <section ref={root} className={`aui-usage-meter ${className}`} aria-labelledby={`${id}-title`} data-entered={entered || undefined} data-layout={layout} data-danger-scale={dangerLine !== null || undefined}>
    <style>{usageMeterStyles}</style>
    <div ref={card} className="aui-usage-meter-card">
      <div ref={content} className="aui-usage-meter-content">
        <header className="aui-usage-meter-header">
          <div className="aui-usage-meter-heading-box">
            <h3 id={`${id}-title`} className="aui-usage-meter-heading" title={title} dir="auto">{title}</h3>
            {(plan || hasReset) && <p className="aui-usage-meter-meta" title={[plan, hasReset ? resetLabel : ''].filter(Boolean).join(' · ')}>{plan && <span>{plan}</span>}{plan && hasReset && <span className="aui-usage-meter-meta-dot" aria-hidden="true">·</span>}{hasReset && <time dateTime={resetDate!.toISOString()} title={absoluteReset}>{resetLabel}</time>}</p>}
          </div>
          {periods.length >= 2 && <div ref={periodTrack} className="aui-usage-meter-periods" role="radiogroup" aria-label={text.periods}>
            <span ref={pill} className="aui-usage-meter-pill" aria-hidden="true" />
            {periods.map((item, index) => <button key={item.id} type="button" role="radio" className="aui-usage-meter-period" aria-checked={item.id === currentPeriod} tabIndex={item.id === currentPeriod || (!periods.some((choice) => choice.id === currentPeriod) && index === 0) ? 0 : -1} onClick={() => changePeriod(index)} onKeyDown={(event) => periodKeys(event, index)} title={item.label}>{item.label}</button>)}
          </div>}
        </header>
        {limitList}
        {!limitsLayout && <>
        <div className="aui-usage-meter-summary">
          <div className="aui-usage-meter-amount"><strong className="aui-usage-meter-number">{format(value('amount'))}</strong><span className="aui-usage-meter-total">/ {withUnit(value('total'))}</span></div>
          <span className="aui-usage-meter-ratio" data-state={level}><WarningDot state={level} decorative labels={text} />{percentText(value('percent'))}</span>
        </div>
        <div className="aui-usage-meter-bar-area" role="group" tabIndex={0} aria-label={text.barHint(`${withUnit(amount)} / ${withUnit(maximum)}`)} onKeyDown={categoryKeys} onBlur={(event) => { if (!focusesCurrentPeriod(event.relatedTarget)) setActiveCategory(null); }} onPointerDown={(event) => { categoryPress.current = { x: event.clientX, y: event.clientY }; }} onPointerCancel={() => { categoryPress.current = null; }} onPointerMove={(event) => { if (event.pointerType === 'mouse') setActiveCategory(categoryAt(event.clientX, event.currentTarget)); }} onPointerLeave={(event) => { if (event.pointerType === 'mouse') setActiveCategory(null); }} onPointerUp={(event) => {
          const press = categoryPress.current; categoryPress.current = null;
          if (event.pointerType !== 'mouse' && press && Math.hypot(event.clientX - press.x, event.clientY - press.y) < 10) toggleCategory(categoryAt(event.clientX, event.currentTarget));
        }}>
          <div className="aui-usage-meter-track" role={knownTotal ? 'meter' : 'img'} aria-label={knownTotal ? text.meter : text.zeroTotal(withUnit(amount))} aria-valuemin={knownTotal ? 0 : undefined} aria-valuemax={knownTotal ? maximum : undefined} aria-valuenow={knownTotal ? Math.min(amount, maximum) : undefined} aria-valuetext={knownTotal ? `${withUnit(amount)} / ${withUnit(maximum)}` : undefined}>
            {rows.map((row, index) => <span key={row.label} className="aui-usage-meter-segment" aria-hidden="true" data-last={index === rows.length - 1 || undefined} style={{ width: `${value(nkey('category', row.label)) / scale * 100}%`, background: row.color, opacity: current && current.label !== row.label ? 0.28 : 1 }} />)}
          </div>
          <div className="aui-usage-meter-tooltip" role="status" aria-live="polite" aria-atomic="true" aria-hidden={!current} data-visible={!!current} style={{ left: `${tip?.center ?? 50}%`, '--aui-usage-meter-tip-x': `${-(tip?.center ?? 50)}%` } as CSSProperties}>
            {tip && <><div className="aui-usage-meter-tooltip-name"><span className="aui-usage-meter-swatch" style={{ background: tip.color }} /><span dir="auto">{tip.label}</span></div><div className="aui-usage-meter-tooltip-value">{categoryText(tip.value, tip.value)}&nbsp;{unit}<span> · {format(tip.share)}%</span></div></>}
          </div>
        </div>
        <div className="aui-usage-meter-selected-line" role="status" aria-live="polite" aria-atomic="true" aria-hidden={!current} data-visible={!!current}>{selectionCopy}</div>
        <ul className="aui-usage-meter-legend" aria-label={text.categories}>
          {rows.map((row) => <li key={row.label} className="aui-usage-meter-row" data-exiting={row.exiting || undefined} data-entering={row.entering || undefined}>
            <div className="aui-usage-meter-row-clip"><button type="button" tabIndex={-1} className="aui-usage-meter-category" aria-pressed={current?.label === row.label} disabled={row.exiting} data-active={current?.label === row.label || undefined} style={{ opacity: current && current.label !== row.label ? 0.45 : 1 }} onPointerEnter={(event) => { if (event.pointerType === 'mouse' && !row.exiting && row.value > 0) setActiveCategory(row.label); }} onPointerLeave={(event) => { if (event.pointerType === 'mouse') setActiveCategory(null); }} onPointerUp={(event) => { if (event.pointerType !== 'mouse' && !row.exiting && row.value > 0) toggleCategory(row.label); }} onClick={(event) => { if (event.detail === 0 && !row.exiting && row.value > 0) toggleCategory(row.label); }}>
              <span className="aui-usage-meter-swatch" style={{ background: row.color }} aria-hidden="true" /><span className="aui-usage-meter-label" title={row.label} dir="auto">{row.label}</span><span className="aui-usage-meter-value">{categoryText(row.value, value(nkey('category', row.label)))}</span><span className="aui-usage-meter-share">{format(sumOfRows ? value(nkey('category', row.label)) / sumOfRows * 100 : 0)}%</span>
            </button></div>
          </li>)}
          {!rows.length && <li className="aui-usage-meter-empty">{text.empty}</li>}
        </ul>
        {daily.length > 0 && <div className="aui-usage-meter-daily">
          <div className="aui-usage-meter-section-heading"><span>{text.daily}</span><div className="aui-usage-meter-day-heading-right"><span key={selectedDay === null ? 'average' : `${currentPeriod}-${selectedDay}`} className="aui-usage-meter-day-readout" aria-live="polite" aria-atomic="true">{selected ? <>{selected.label} · <strong>{selected.value === null || !Number.isFinite(selected.value) ? '–' : withUnit(selected.value)}</strong></> : text.average(format(value('average')))}</span><button ref={dayToggle} type="button" className="aui-usage-meter-day-toggle" aria-label={text.dayTools} aria-expanded={toolsOpen} aria-controls={`${id}-day-tools`} onClick={() => setToolsOpen((open) => !open)}><CalendarDays size={14} aria-hidden="true" /></button></div></div>
          <div ref={graphic} className="aui-usage-meter-sparkline" tabIndex={0} role="img" aria-label={text.dailyHint(title, withUnit(average))} data-density={daily.length <= 14 ? 'short' : daily.length <= 45 ? 'medium' : daily.length <= 120 ? 'dense' : 'packed'} onKeyDown={dayKeys} onBlur={(event) => { if (!focusesCurrentPeriod(event.relatedTarget)) setSelectedDay(null); }} onPointerDown={dayPointerDown} onPointerMove={dayPointerMove} onPointerLeave={(event) => { if (event.pointerType === 'mouse') setSelectedDay(null); }} onPointerCancel={() => { dayPress.current = null; }} onPointerUp={(event) => {
            const press = dayPress.current; dayPress.current = null;
            if (event.pointerType !== 'mouse' && press?.direction !== 'vertical') setSelectedDay(dayAt(event.clientX, event.currentTarget));
          }}>
            {daily.map((day, index) => <span key={`${daily.length}-${index}`} className="aui-usage-meter-day-bar" aria-hidden="true" data-active={selectedDay === index || undefined} data-future={day.value === null || day.value === undefined || undefined} style={{ height: day.value === null || day.value === undefined ? '2px' : `calc(${Math.max(.04, value(nkey('day', String(index))) / maximumDay)} * var(--aui-usage-meter-day-height, 34px))`, opacity: selectedDay !== null && selectedDay !== index ? 0.6 : 1, animationDelay: `${index * 18}ms` }} />)}
          </div>
          {toolsOpen && <div ref={dayTools} id={`${id}-day-tools`} className="aui-usage-meter-day-controls" onKeyDown={(event) => { if (event.key === 'Escape') { setSelectedDay(null); setToolsOpen(false); dayToggle.current?.focus(); } }}>
            <button type="button" className="aui-usage-meter-day-button" aria-label={text.previousDay} disabled={selectedDay === 0} onClick={() => setSelectedDay((day) => day === null ? daily.length - 1 : Math.max(0, day - 1))}><ArrowLeft size={16} aria-hidden="true" /></button>
            <label className="aui-usage-meter-sr-only" htmlFor={`${id}-day`}>{text.daySelect}</label><select id={`${id}-day`} value={selectedDay ?? ''} onChange={(event) => setSelectedDay(event.target.value === '' ? null : Number(event.target.value))}><option value="">{text.averageOption}</option>{daily.map((day, index) => <option key={`${day.label}-${index}`} value={index}>{day.label}{day.value === null ? ` · ${text.unmeasuredDay}` : ''}</option>)}</select>
            <button type="button" className="aui-usage-meter-day-button" aria-label={text.nextDay} disabled={selectedDay === daily.length - 1} onClick={() => setSelectedDay((day) => day === null ? 0 : Math.min(daily.length - 1, day + 1))}><ArrowRight size={16} aria-hidden="true" /></button>
          </div>}
        </div>}
        {limitTiles}
        </>}
      </div>
    </div>
    {(!limitsLayout || onUpgrade) && <footer className="aui-usage-meter-footer">
      {!limitsLayout && <div key={level} className="aui-usage-meter-status" data-state={level}><WarningDot state={level} decorative labels={text} />{state === 'limit' ? <><strong>{text.limitReached}</strong>{amount > maximum && <span className="aui-usage-meter-status-tail">{text.over(withUnit(value('over')))}</span>}</> : state === 'warn' ? <><strong>{level === 'danger' ? text.dangerStatus : text.nearLimitStatus}</strong><span className="aui-usage-meter-status-tail">{text.remaining(withUnit(value('remaining')))}</span></> : <span className="aui-usage-meter-status-tail">{text.remaining(<><strong>{format(value('remaining'))}</strong> {unit}</>)}</span>}</div>}
      {onUpgrade && <button ref={upgrade} type="button" className="aui-usage-meter-upgrade" title={toggledLabel} data-urgent={state !== 'ok' || undefined} onClick={() => { onUpgrade(state); }}><span ref={upgradeCopy} key={toggledLabel} className="aui-usage-meter-upgrade-copy">{toggledLabel}</span></button>}
    </footer>}
    {feedback && <p className="aui-usage-meter-feedback" role="status">{feedback}</p>}
    <span className="aui-usage-meter-sr-only" role="status" aria-live="polite" aria-atomic="true">{announcement}</span>
  </section>;
}

const usageMeterStyles = `
@layer kit {
.aui-usage-meter {
  --aui-usage-meter-tray: var(--aui-bg-2, #f0f0f0); --aui-usage-meter-card: var(--aui-bg-1, #fff);
  --aui-usage-meter-line: var(--aui-line, #ececec); --aui-usage-meter-ink: var(--aui-fg, #171717);
  --aui-usage-meter-text: var(--aui-fg-dim, #404040); --aui-usage-meter-muted: var(--aui-fg-muted, #8f8f8f);
  --aui-usage-meter-faint: var(--aui-fg-faint, #b5b5b5); --aui-usage-meter-well: var(--aui-bg-3, #f4f4f5);
  --aui-usage-meter-hover: var(--aui-usage-meter-hover-bg, #f4f4f5); --aui-usage-meter-track: var(--aui-usage-meter-track-bg, #efeff1);
  --aui-usage-meter-fill: var(--aui-usage-meter-meter-fill, #4a4a4f); --aui-usage-meter-day: var(--aui-usage-meter-day-bg, #dcdce0);
  --aui-usage-meter-accent: var(--aui-accent, #4fb0fe); --aui-usage-meter-warning: var(--aui-warn, #e8b000);
  --aui-usage-meter-danger: var(--aui-err, #d92d20); --aui-usage-meter-danger-text: var(--aui-usage-meter-danger-fg, var(--aui-err, #b42318));
  --aui-usage-meter-danger-ring: color-mix(in srgb, var(--aui-usage-meter-danger) 22%, transparent);
  --aui-usage-meter-warning-text: var(--aui-usage-meter-warn-fg, #b25e00); --aui-usage-meter-warning-ring: var(--aui-usage-meter-warn-ring, color-mix(in srgb, var(--aui-usage-meter-warning) 22%, transparent));
  --aui-usage-meter-color-1: var(--aui-accent, #4fb0fe); --aui-usage-meter-color-2: var(--aui-usage-meter-category-2, #4fb0fe);
  --aui-usage-meter-color-3: var(--aui-usage-meter-category-3, #b4deff); --aui-usage-meter-color-4: var(--aui-usage-meter-category-4, #a8a8ae); --aui-usage-meter-color-5: var(--aui-usage-meter-category-5, #d4d4d8);
  --aui-usage-meter-pill-bg: var(--aui-bg-1, #fff); --aui-usage-meter-pill-border: var(--aui-line, #ececec);
  --aui-usage-meter-tooltip-bg: var(--aui-bg-1, #fff); --aui-usage-meter-tooltip-border: var(--aui-line, #ececec);
  --aui-usage-meter-tooltip-shadow: 0 8px 24px -6px #00000029, 0 2px 6px #0000000f;
  --aui-usage-meter-focus: var(--aui-usage-meter-focus-ring, var(--aui-focus, rgb(0 96 226 / .7))); --aui-usage-meter-ease: var(--aui-ease, cubic-bezier(.22,1,.36,1)); --aui-usage-meter-bounce: cubic-bezier(.34,1.56,.64,1);
  --aui-usage-meter-r: var(--aui-r-lg, 16px); --aui-usage-meter-m: var(--aui-r-md, 10px); --aui-usage-meter-s: var(--aui-r-sm, 7px);
  --aui-usage-meter-primary: var(--aui-primary, var(--aui-usage-meter-ink)); --aui-usage-meter-primary-fg: var(--aui-primary-fg, var(--aui-usage-meter-card));
  box-sizing: border-box; width: 100%; max-width: 460px; min-width: 0; padding: 4px; border-radius: calc(var(--aui-usage-meter-r) + 4px); background: var(--aui-usage-meter-tray); container: aui-usage-meter / inline-size;
  color: var(--aui-usage-meter-text); font: 13px/1.45 var(--aui-font-sans, Inter, ui-sans-serif, -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif); font-variant-numeric: tabular-nums; word-break: keep-all; overflow-wrap: anywhere; -webkit-tap-highlight-color: transparent;
}
:where(.dark,[data-theme="dark"]) .aui-usage-meter {
  --aui-usage-meter-danger: var(--aui-err, #ff6b5f); --aui-usage-meter-danger-text: var(--aui-usage-meter-danger-fg, var(--aui-err, #ff8a7a));
  --aui-usage-meter-tray: var(--aui-bg-2, #111113); --aui-usage-meter-card: var(--aui-bg-1, #19191b); --aui-usage-meter-line: var(--aui-line, #2a2a2e); --aui-usage-meter-ink: var(--aui-fg, #ededed); --aui-usage-meter-text: var(--aui-fg-dim, #c8c8cc); --aui-usage-meter-muted: var(--aui-fg-muted, #8b8b92); --aui-usage-meter-faint: var(--aui-fg-faint, #5c5c63); --aui-usage-meter-well: var(--aui-bg-3, #111113); --aui-usage-meter-hover: var(--aui-usage-meter-hover-bg, #232326); --aui-usage-meter-track: var(--aui-usage-meter-track-bg, #2a2a2e); --aui-usage-meter-fill: var(--aui-usage-meter-meter-fill, #b4b4ba); --aui-usage-meter-day: var(--aui-usage-meter-day-bg, #3a3a40); --aui-usage-meter-accent: var(--aui-accent, #4fb0fe); --aui-usage-meter-warning: var(--aui-warn, #e6c24d); --aui-usage-meter-warning-text: var(--aui-usage-meter-warn-fg, #f2a15c); --aui-usage-meter-color-1: var(--aui-accent, #4fb0fe); --aui-usage-meter-color-2: var(--aui-usage-meter-category-2, #0190fe); --aui-usage-meter-color-3: var(--aui-usage-meter-category-3, #0070d6); --aui-usage-meter-color-4: var(--aui-usage-meter-category-4, #6b6b72); --aui-usage-meter-color-5: var(--aui-usage-meter-category-5, #45454b); --aui-usage-meter-pill-bg: var(--aui-usage-meter-pill, #2a2a2e); --aui-usage-meter-pill-border: var(--aui-usage-meter-pill-line, #ffffff0f); --aui-usage-meter-tooltip-bg: var(--aui-usage-meter-tooltip, #2a2a2d); --aui-usage-meter-tooltip-border: var(--aui-usage-meter-tooltip-line, #ffffff1a); --aui-usage-meter-tooltip-shadow: 0 8px 24px -6px #00000080, 0 2px 6px #00000059; --aui-usage-meter-focus: var(--aui-usage-meter-focus-ring, var(--aui-focus, rgb(77 141 255 / .8)));
}
.aui-usage-meter *, .aui-usage-meter *::before, .aui-usage-meter *::after { box-sizing: border-box; }
.aui-usage-meter button, .aui-usage-meter select { font: inherit; }
.aui-usage-meter button { cursor: pointer; touch-action: manipulation; }
.aui-usage-meter button:disabled { cursor: default; }
.aui-usage-meter :is(button,select,[tabindex="0"]):focus-visible { outline: 2px solid var(--aui-usage-meter-focus); outline-offset: 2px; }
.aui-usage-meter .aui-usage-meter-period:focus-visible { outline-offset: -2px; }
.aui-usage-meter-card { overflow: hidden; background: var(--aui-usage-meter-card); border-radius: var(--aui-usage-meter-r); box-shadow: 0 0 0 1px var(--aui-usage-meter-line), var(--aui-shadow-sm, 0 1px 2px #0000000a); }
.aui-usage-meter-card[data-ready] { transition: height 360ms var(--aui-usage-meter-ease); }
.aui-usage-meter-content { padding: 14px 16px 18px; }
.aui-usage-meter-header { display: flex; align-items: flex-start; justify-content: space-between; gap: 12px; }
.aui-usage-meter-heading-box { min-width: min(120px, 40%); padding-top: 2px; flex: 1 1 0; }
.aui-usage-meter-heading { margin: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--aui-usage-meter-ink); font-size: 14px; font-weight: 600; line-height: 20px; }
.aui-usage-meter-meta { display: flex; min-width: 0; align-items: center; margin: 0; color: var(--aui-usage-meter-muted); font-size: 12.5px; white-space: nowrap; }
.aui-usage-meter-meta > :is(span,time) { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.aui-usage-meter-meta > time { flex: 0 1 auto; min-width: 3em; }
.aui-usage-meter-meta > span:first-child:not(:only-child) { flex: 0 3 auto; min-width: 2.5em; }
.aui-usage-meter-meta-dot { flex: none; margin: 0 6px; color: var(--aui-usage-meter-faint); }
.aui-usage-meter-periods { position: relative; display: flex; flex: 0 1 auto; min-width: 0; max-width: 100%; padding: 2px; border-radius: var(--aui-usage-meter-m); background: var(--aui-usage-meter-well); box-shadow: inset 0 0 0 1px var(--aui-usage-meter-line); }
.aui-usage-meter-period { position: relative; z-index: 1; min-width: 0; height: 26px; padding: 0 9px; border: 0; border-radius: calc(var(--aui-usage-meter-m) - 2px); background: transparent; color: var(--aui-usage-meter-muted); font-size: 12px; font-weight: 500; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; transition: color 200ms ease; }
.aui-usage-meter-period:hover, .aui-usage-meter-period[aria-checked="true"] { color: var(--aui-usage-meter-ink); }
.aui-usage-meter-pill { position: absolute; z-index: 0; border-radius: calc(var(--aui-usage-meter-m) - 2px); background: var(--aui-usage-meter-pill-bg); box-shadow: 0 0 0 1px var(--aui-usage-meter-pill-border), 0 1px 2px #0000000f; pointer-events: none; opacity: 0; transition: left 320ms var(--aui-usage-meter-ease), top 320ms var(--aui-usage-meter-ease), width 320ms var(--aui-usage-meter-ease), height 320ms var(--aui-usage-meter-ease), opacity 200ms ease; }
.aui-usage-meter-summary { display: flex; align-items: baseline; justify-content: space-between; gap: 12px; margin-top: 18px; }
.aui-usage-meter-amount { display: flex; min-width: 0; align-items: baseline; flex-wrap: wrap; gap: 0 6px; }
.aui-usage-meter-number { color: var(--aui-usage-meter-ink); font-size: 28px; font-weight: 600; line-height: 34px; letter-spacing: -.02em; white-space: nowrap; }
.aui-usage-meter-total { color: var(--aui-usage-meter-muted); font-size: 13px; white-space: nowrap; }
.aui-usage-meter-ratio { display: inline-flex; flex: none; align-items: center; gap: 6px; color: var(--aui-usage-meter-text); font-size: 13px; font-weight: 500; white-space: nowrap; transition: color 260ms ease; }
.aui-usage-meter-ratio[data-state="warn"] { color: var(--aui-usage-meter-warning-text); }
.aui-usage-meter-ratio[data-state="limit"] { color: var(--aui-usage-meter-ink); }
.aui-usage-meter-warning-dot { display: inline-block; flex: 0 0 6px; width: 6px; height: 6px; border-radius: 50%; background: var(--aui-usage-meter-warning); animation: aui-usage-meter-dot-in 420ms var(--aui-usage-meter-bounce) both; }
.aui-usage-meter-warning-dot[data-state="limit"] { box-shadow: 0 0 0 3px var(--aui-usage-meter-warning-ring); }
.aui-usage-meter-bar-area { position: relative; margin-top: 10px; padding: 4px 0; border-radius: 6px; touch-action: pan-y; }
.aui-usage-meter-track { display: flex; width: 100%; height: 10px; overflow: hidden; border-radius: 5px; background: var(--aui-usage-meter-track); }
.aui-usage-meter-segment { position: relative; flex: none; min-width: 0; height: 100%; transition: opacity 200ms ease; }
.aui-usage-meter-segment:not([data-last])::after { position: absolute; content: ''; top: 0; bottom: 0; right: 0; width: 2px; background: var(--aui-usage-meter-card); }
.aui-usage-meter-tooltip { position: absolute; z-index: 3; bottom: calc(100% + 6px); width: max-content; max-width: 100%; padding: 7px 10px 8px; border: 1px solid var(--aui-usage-meter-tooltip-border); border-radius: calc(var(--aui-usage-meter-r) - 6px); background: var(--aui-usage-meter-tooltip-bg); box-shadow: var(--aui-usage-meter-tooltip-shadow); opacity: 0; pointer-events: none; transform: translateX(var(--aui-usage-meter-tip-x, -50%)) translateY(4px) scale(.97); transform-origin: bottom; transition: left 260ms var(--aui-usage-meter-ease), transform 260ms var(--aui-usage-meter-ease), opacity 160ms ease; white-space: nowrap; }
.aui-usage-meter-tooltip[data-visible="true"] { opacity: 1; transform: translateX(var(--aui-usage-meter-tip-x, -50%)) translateY(0) scale(1); }
.aui-usage-meter-tooltip-name { display: flex; align-items: center; gap: 7px; color: var(--aui-usage-meter-muted); font-size: 12px; }
.aui-usage-meter-tooltip-name > span:last-child { min-width: 0; overflow: hidden; text-overflow: ellipsis; }
.aui-usage-meter-tooltip-value { margin-top: 2px; overflow: hidden; text-overflow: ellipsis; color: var(--aui-usage-meter-ink); font-size: 13px; font-weight: 500; }
.aui-usage-meter-tooltip-value > span { color: var(--aui-usage-meter-muted); font-weight: 400; }
.aui-usage-meter-selected-line { display: none; align-items: center; gap: 7px; height: 28px; color: var(--aui-usage-meter-muted); font-size: 12px; white-space: nowrap; opacity: 0; }
.aui-usage-meter-selected-line[data-visible="true"] { opacity: 1; }
.aui-usage-meter-selected-line > strong { flex: none; color: var(--aui-usage-meter-ink); font-weight: 500; }
.aui-usage-meter-selected-name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; }
.aui-usage-meter-selected-share { flex: none; }
.aui-usage-meter-legend { display: block; padding: 0; margin: 10px -8px 0; list-style: none; }
.aui-usage-meter-row { display: grid; grid-template-rows: 1fr; opacity: 1; transition: grid-template-rows 320ms 80ms var(--aui-usage-meter-ease), opacity 200ms ease; }
.aui-usage-meter-row[data-entering] { animation: aui-usage-meter-row-in 380ms var(--aui-usage-meter-ease) both; }
.aui-usage-meter-row[data-exiting] { grid-template-rows: 0fr; opacity: 0; pointer-events: none; }
.aui-usage-meter-row-clip { min-height: 0; overflow: hidden; }
.aui-usage-meter-category { display: flex; align-items: center; gap: 10px; width: 100%; min-width: 0; height: 30px; padding: 0 8px; border: 0; border-radius: calc(var(--aui-usage-meter-s) + 1px); color: var(--aui-usage-meter-text); background: transparent; text-align: left; transition: background 160ms ease, opacity 200ms ease; }
.aui-usage-meter-category[data-active] { background: var(--aui-usage-meter-hover); }
.aui-usage-meter-row[data-exiting] .aui-usage-meter-category { background: transparent; }
.aui-usage-meter-swatch { display: inline-block; flex: 0 0 8px; width: 8px; height: 8px; border-radius: 2.5px; }
.aui-usage-meter-label { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.aui-usage-meter-value { flex: none; color: var(--aui-usage-meter-ink); font-weight: 500; white-space: nowrap; }
.aui-usage-meter-share { flex: 0 0 40px; width: 40px; color: var(--aui-usage-meter-muted); text-align: right; white-space: nowrap; }
.aui-usage-meter-empty { padding: 8px; color: var(--aui-usage-meter-muted); }
.aui-usage-meter-daily { margin-top: 12px; }
.aui-usage-meter-section-heading { display: flex; align-items: center; justify-content: space-between; gap: 8px; min-width: 0; color: var(--aui-usage-meter-muted); font-size: 12px; white-space: nowrap; }
.aui-usage-meter-day-heading-right { display: flex; align-items: center; min-width: 0; gap: 5px; }
.aui-usage-meter-day-readout { min-width: 0; overflow: hidden; text-overflow: ellipsis; animation: aui-usage-meter-rise-in 220ms var(--aui-usage-meter-ease) both; }
.aui-usage-meter-day-readout > strong { color: var(--aui-usage-meter-ink); font-weight: 500; }
.aui-usage-meter-day-toggle { display: inline-flex; flex: none; align-items: center; justify-content: center; width: 24px; height: 24px; padding: 0; border: 0; border-radius: 6px; background: transparent; color: var(--aui-usage-meter-muted); }
.aui-usage-meter-day-toggle:hover, .aui-usage-meter-day-toggle[aria-expanded="true"] { background: var(--aui-usage-meter-hover); color: var(--aui-usage-meter-ink); }
.aui-usage-meter-sparkline { display: flex; align-items: flex-end; gap: 2px; height: 34px; margin-top: 8px; border-radius: 2px; touch-action: pan-y; user-select: none; }
.aui-usage-meter-sparkline[data-density="short"] { gap: 5px; }
.aui-usage-meter-sparkline[data-density="dense"] { gap: 1px; }
.aui-usage-meter-sparkline[data-density="packed"] { gap: 0; }
.aui-usage-meter-day-bar { display: block; flex: 1 1 0; min-width: 0; max-height: var(--aui-usage-meter-day-height, 34px); border-radius: 2px; background: var(--aui-usage-meter-day); transform-origin: bottom; animation: aui-usage-meter-day-grow 520ms var(--aui-usage-meter-ease) both; transition: background 160ms ease, opacity 160ms ease; }
.aui-usage-meter-day-bar[data-future] { background: var(--aui-usage-meter-track); }
.aui-usage-meter-day-bar[data-active] { background: var(--aui-usage-meter-accent); }
.aui-usage-meter-day-controls { display: flex; align-items: center; gap: 6px; margin-top: 10px; }
.aui-usage-meter-day-controls select { flex: 1; width: 100%; min-width: 0; min-height: 44px; padding: 7px 10px; border: 1px solid var(--aui-usage-meter-line); border-radius: 8px; background: var(--aui-usage-meter-card); color: var(--aui-usage-meter-ink); font-size: 16px; }
.aui-usage-meter-day-button { display: inline-flex; flex: 0 0 44px; align-items: center; justify-content: center; width: 44px; height: 44px; padding: 0; border: 1px solid var(--aui-usage-meter-line); border-radius: 8px; background: var(--aui-usage-meter-card); color: var(--aui-usage-meter-text); }
.aui-usage-meter-day-button:disabled { opacity: .35; }
.aui-usage-meter-limits { display: grid; grid-template-columns: repeat(var(--aui-usage-meter-limit-columns, 3), minmax(0, 1fr)); gap: 12px 18px; margin: 16px 0 0; padding: 14px 0 0; border-top: 1px solid var(--aui-usage-meter-line); list-style: none; }
.aui-usage-meter-limit { min-width: 0; }
.aui-usage-meter-limit-heading { display: flex; min-width: 0; align-items: center; gap: 6px; color: var(--aui-usage-meter-muted); font-size: 12px; }
.aui-usage-meter-limit-heading > span:first-child { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.aui-usage-meter-limit-value { margin-top: 2px; color: var(--aui-usage-meter-ink); font-weight: 500; overflow-wrap: anywhere; }
.aui-usage-meter-limit-value > span { color: var(--aui-usage-meter-muted); font-weight: 400; }
.aui-usage-meter-limit-track { height: 4px; margin-top: 7px; overflow: hidden; border-radius: 2px; background: var(--aui-usage-meter-track); }
.aui-usage-meter-limit-fill { display: block; height: 100%; border-radius: inherit; background: var(--aui-usage-meter-fill); }
.aui-usage-meter-limit-track[data-unlimited] { background: repeating-linear-gradient(-45deg, var(--aui-usage-meter-track) 0 3px, transparent 3px 6px); }
.aui-usage-meter-mini { display: grid; gap: 14px; margin-top: 14px; min-width: 0; }
.aui-usage-meter-mini-group { display: grid; gap: 6px; min-width: 0; }
.aui-usage-meter-mini-name { margin: 0; color: var(--aui-usage-meter-ink); font-size: 12.5px; font-weight: 600; line-height: 18px; overflow-wrap: anywhere; }
.aui-usage-meter-mini-rows { display: grid; gap: 8px; margin: 0; padding: 0; list-style: none; }
.aui-usage-meter-mini-row { display: grid; grid-template-columns: minmax(0, 7.5em) minmax(48px, 1fr) minmax(3.6em, auto); grid-template-areas: "label track value"; align-items: center; gap: 3px 10px; min-width: 0; }
.aui-usage-meter-mini-row[data-detail] { grid-template-areas: "label track value" "detail detail detail"; }
.aui-usage-meter-mini-label { grid-area: label; min-width: 0; overflow: hidden; color: var(--aui-usage-meter-text); font-size: 12.5px; line-height: 18px; text-overflow: ellipsis; white-space: nowrap; }
.aui-usage-meter-mini-track { grid-area: track; display: block; height: 6px; overflow: hidden; border-radius: 3px; background: var(--aui-usage-meter-track); }
.aui-usage-meter-mini-track[data-unlimited] { background: repeating-linear-gradient(-45deg, var(--aui-usage-meter-track) 0 3px, transparent 3px 6px); }
.aui-usage-meter-mini-track[data-unmeasured] { background: transparent; box-shadow: inset 0 0 0 1px var(--aui-usage-meter-track); }
.aui-usage-meter-mini-fill { display: block; height: 100%; border-radius: inherit; background: var(--aui-usage-meter-fill); transition: background 260ms ease; }
.aui-usage-meter-mini-row[data-state="warn"] .aui-usage-meter-mini-fill { background: var(--aui-usage-meter-warning); }
.aui-usage-meter-mini-row[data-state="limit"] .aui-usage-meter-mini-fill { background: var(--aui-usage-meter-warning-text); }
.aui-usage-meter-mini-value { grid-area: value; display: inline-flex; align-items: center; justify-content: flex-end; gap: 6px; color: var(--aui-usage-meter-ink); font-size: 12.5px; font-weight: 500; line-height: 18px; white-space: nowrap; }
.aui-usage-meter-mini-row[data-state="warn"] .aui-usage-meter-mini-value { color: var(--aui-usage-meter-warning-text); }
.aui-usage-meter-ratio[data-state="danger"], .aui-usage-meter-mini-row[data-state="danger"] .aui-usage-meter-mini-value { color: var(--aui-usage-meter-danger-text); }
.aui-usage-meter-warning-dot[data-state="danger"] { background: var(--aui-usage-meter-danger); }
.aui-usage-meter[data-danger-scale] .aui-usage-meter-warning-dot[data-state="limit"] { background: var(--aui-usage-meter-danger); box-shadow: 0 0 0 3px var(--aui-usage-meter-danger-ring); }
.aui-usage-meter-mini-row[data-state="danger"] .aui-usage-meter-mini-fill, .aui-usage-meter[data-danger-scale] .aui-usage-meter-mini-row[data-state="limit"] .aui-usage-meter-mini-fill { background: var(--aui-usage-meter-danger); }
.aui-usage-meter-limit[data-state="danger"] .aui-usage-meter-limit-fill, .aui-usage-meter[data-danger-scale] .aui-usage-meter-limit[data-state="limit"] .aui-usage-meter-limit-fill { background: var(--aui-usage-meter-danger); }
.aui-usage-meter-mini-track, .aui-usage-meter-limit-track { position: relative; }
.aui-usage-meter-danger-tick { position: absolute; top: 0; bottom: 0; width: 2px; margin-left: -1px; background: var(--aui-usage-meter-card); pointer-events: none; }
.aui-usage-meter-mini-detail { grid-area: detail; min-width: 0; color: var(--aui-usage-meter-muted); font-size: 11.5px; line-height: 16px; overflow-wrap: anywhere; }
.aui-usage-meter-footer { display: flex; flex-wrap: wrap; align-items: center; justify-content: space-between; gap: 6px 12px; min-height: 44px; padding: 6px 2px 2px 12px; }
.aui-usage-meter-status { display: flex; align-items: center; flex: 1 1 auto; min-width: 0; gap: 7px; color: var(--aui-usage-meter-muted); font-size: 12.5px; white-space: nowrap; animation: aui-usage-meter-rise-in 260ms var(--aui-usage-meter-ease) both; }
.aui-usage-meter-status strong { flex: none; color: var(--aui-usage-meter-ink); font-weight: 500; }
.aui-usage-meter-status-tail { overflow: hidden; text-overflow: ellipsis; }
.aui-usage-meter-upgrade { display: inline-flex; flex: none; align-items: center; justify-content: center; max-width: 100%; margin-left: auto; height: 32px; padding: 0 14px; border: 0; border-radius: calc(var(--aui-usage-meter-m) + 4px); background: var(--aui-usage-meter-card); box-shadow: 0 0 0 1px var(--aui-usage-meter-line), 0 1px 2px #0000000a; color: var(--aui-usage-meter-ink); font-size: 13px; font-weight: 500; transition: width 320ms var(--aui-usage-meter-ease), background 280ms ease, color 280ms ease, box-shadow 280ms ease, transform 140ms ease; }
.aui-usage-meter-upgrade:hover { background: var(--aui-usage-meter-hover); }
.aui-usage-meter-upgrade[data-urgent] { background: var(--aui-usage-meter-primary); color: var(--aui-usage-meter-primary-fg); box-shadow: 0 1px 2px #0000001f, 0 6px 14px -6px #00000059; animation: aui-usage-meter-button-pop 520ms var(--aui-usage-meter-bounce) both; }
.aui-usage-meter-upgrade[data-urgent]:hover { opacity: .88; }
.aui-usage-meter-upgrade:active { transform: scale(.97); }
.aui-usage-meter-upgrade-copy { display: inline-block; max-width: 100%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; animation: aui-usage-meter-rise-in 260ms var(--aui-usage-meter-ease) both; }
.aui-usage-meter-feedback { margin: 0; padding: 4px 12px; font-size: 12px; color: var(--aui-usage-meter-text); }
.aui-usage-meter-sr-only { position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px; overflow: hidden; clip-path: inset(50%); white-space: nowrap; border: 0; }
@container aui-usage-meter (max-width: 400px) {
  .aui-usage-meter-content { padding: 12px 14px 16px; }
  .aui-usage-meter-header { flex-direction: column; gap: 12px; }
  .aui-usage-meter-heading-box { width: 100%; }
  .aui-usage-meter-periods { width: 100%; }
  .aui-usage-meter-period { flex: 1; padding-inline: 6px; }
  .aui-usage-meter-number { font-size: 26px; }
  .aui-usage-meter-limits { grid-template-columns: minmax(0, 1fr); }
  .aui-usage-meter-limit { display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: 0 12px; }
  .aui-usage-meter-limit-value { max-width: 60cqw; margin-top: 0; font-size: 12.5px; text-align: end; }
  .aui-usage-meter-limit-track { grid-column: 1 / -1; margin-top: 6px; }
}
@container aui-usage-meter (max-width: 300px) {
  .aui-usage-meter-mini-row { grid-template-columns: minmax(0, 1fr) auto; grid-template-areas: "label value" "track track"; }
  .aui-usage-meter-mini-row[data-detail] { grid-template-areas: "label value" "track track" "detail detail"; }
  .aui-usage-meter-mini-label { white-space: normal; }
  .aui-usage-meter-total { font-size: 12px; }
  .aui-usage-meter-share { flex-basis: 34px; width: 34px; }
  .aui-usage-meter-category { gap: 7px; }
  .aui-usage-meter-selected-line { gap: 5px; font-size: 11px; }
}
@media (max-width: 899px) {
  @container aui-usage-meter (max-width: 400px) {
    .aui-usage-meter-tooltip { display: none; }
    .aui-usage-meter-selected-line { display: flex; }
  }
}
@media (pointer: coarse) {
  .aui-usage-meter-period { min-width: 44px; height: 44px; }
  .aui-usage-meter-category { min-height: 44px; height: 44px; }
  .aui-usage-meter-bar-area { display: flex; align-items: center; min-height: 44px; margin-top: 4px; padding: 10px 0; }
  /* Keep the touch target intact when a host restores the 34px visual height. */
  .aui-usage-meter-sparkline { min-height: 44px; height: 44px; padding-top: 10px; }
  .aui-usage-meter-day-toggle { width: 44px; height: 44px; }
  .aui-usage-meter-upgrade { min-width: 44px; height: 44px; border-radius: calc(var(--aui-usage-meter-m) + 3px); }
  .aui-usage-meter-mini-label { white-space: normal; }
  .aui-usage-meter-footer { min-height: 50px; }
}
@keyframes aui-usage-meter-rise-in { from { opacity: 0; transform: translateY(3px); filter: blur(2px); } to { opacity: 1; transform: translateY(0); filter: blur(0); } }
@keyframes aui-usage-meter-row-in { from { grid-template-rows: 0fr; opacity: 0; } to { grid-template-rows: 1fr; opacity: 1; } }
@keyframes aui-usage-meter-day-grow { from { transform: scaleY(0); } to { transform: scaleY(1); } }
@keyframes aui-usage-meter-dot-in { 0% { transform: scale(.3); } 60% { transform: scale(1.25); } 100% { transform: scale(1); } }
@keyframes aui-usage-meter-button-pop { 0% { scale: .94; } 60% { scale: 1.04; } 100% { scale: 1; } }
@media (prefers-reduced-motion: reduce) {
  .aui-usage-meter *, .aui-usage-meter *::before, .aui-usage-meter *::after { transition: none !important; animation: none !important; }
}
}
`;
