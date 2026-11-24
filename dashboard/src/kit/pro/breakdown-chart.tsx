// Kit provenance: itismyfield/agentdesk-ui-kit@b6d2ab1
import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type KeyboardEvent, type ReactNode } from 'react';
import { createPortal } from 'react-dom';
import { BarChart3, Check, ChevronDown, CircleSlash, Clock3, Eye, Inbox, Square, SquareCheck, X } from 'lucide-react';

// Clean-room implementation from the supplied v2 specification and PNG comparisons.
export type BreakdownRecord = Record<string, string | number | null | undefined>;
export interface BreakdownValue { id: string; label: string; color?: string; icon?: ReactNode }
export interface BreakdownDimension { id: string; label: string; field?: string; values: BreakdownValue[] }
export interface BreakdownMeasure {
  id: string;
  label: string;
  unit?: string;
  /** Omit or leave empty to count records. Numeric strings are accepted. */
  field?: string;
}
export interface BreakdownChartProps {
  records: BreakdownRecord[];
  dimensions: BreakdownDimension[];
  measures: BreakdownMeasure[];
  title?: string;
  defaultMeasure?: string;
  defaultSlice?: string;
  /** null or an empty string selects the unsegmented view. */
  defaultSegment?: string | null;
  className?: string;
  style?: CSSProperties;
}
const palette = ['#4fb0fe', '#a651f0', '#ee3e9e', '#7bc4fe', '#d7bcf6', '#a3a3a3', '#f6abd4'];
const oneDecimal = new Intl.NumberFormat('en-US', { maximumFractionDigits: 1 });
/** Table, total and tooltip numbers: grouping, at most one decimal, and <0.1 for tiny positives. */
const format = (value: number) => value > 0 && value < 0.05 ? '<0.1' : oneDecimal.format(value);
const shortScale = new Intl.NumberFormat('en-US', { notation: 'compact', maximumFractionDigits: 1 });
/** Right-hand ticks only: K/M/B with at most one decimal. */
const formatTick = (value: number) => shortScale.format(value);
const keyOf = (value: string | number | null | undefined) => value === null || value === undefined ? null : String(value);
function axisMaximum(maximum: number) {
  if (maximum <= 0) return 5;
  const raw = maximum / 5, power = 10 ** Math.floor(Math.log10(raw));
  return 5 * ([1, 2, 2.5, 5, 10].find(value => value * power >= raw) ?? 10) * power;
}
/** Freeze small pieces at 2px; sums below one deliberately leave unused space. */
function segmentHeights(amounts: number[], height: number) {
  const room = Math.max(0, height - Math.max(0, amounts.length - 1));
  const frozen = new Set<number>();
  let sizes = amounts.map(() => 0);
  for (let iteration = 0; iteration <= amounts.length; iteration++) {
    const sum = amounts.reduce((total, value, index) => total + (frozen.has(index) ? 0 : value), 0);
    const available = Math.max(0, room - frozen.size * 2);
    const distributable = sum >= 1 ? available : Math.min(available, room * sum);
    sizes = amounts.map((value, index) => frozen.has(index) ? 2 : sum > 0 ? distributable * value / sum : 0);
    const small = sizes.flatMap((size, index) => !frozen.has(index) && size < 2 ? [index] : []);
    if (!small.length) break;
    small.forEach(index => frozen.add(index));
  }
  return sizes;
}

function Picker({ label, value, options, onChange, disabled }: { label: string; value: string; options: { id: string; label: string }[]; onChange: (value: string) => void; disabled: boolean }) {
  const id = useId(), trigger = useRef<HTMLButtonElement>(null), panel = useRef<HTMLDivElement>(null);
  const [open, setOpen] = useState(false), [mobile, setMobile] = useState(false);
  const [themeStyle, setThemeStyle] = useState<CSSProperties>({});
  const [position, setPosition] = useState({ left: 0, top: 0, width: 0 });
  const close = (restore: boolean) => { setOpen(false); if (restore) trigger.current?.focus(); };
  useLayoutEffect(() => {
    if (!open) return;
    const place = () => {
      const rect = trigger.current?.getBoundingClientRect();
      if (rect) setPosition({ left: rect.left, top: rect.bottom + 6, width: rect.width });
      setMobile(window.matchMedia('(max-width: 899px)').matches);
      // Portals retain tokens and nested dark themes from the adopting panel.
      const scope = trigger.current?.closest('.aui-breakdown-chart');
      if (scope) {
        const computed = getComputedStyle(scope), tokens: Record<string, string> = {};
        for (let index = 0; index < computed.length; index++) {
          const name = computed[index];
          if (name.startsWith('--aui-')) tokens[name] = computed.getPropertyValue(name);
        }
        setThemeStyle(tokens as CSSProperties);
      }
    };
    place();
    (panel.current?.querySelector<HTMLButtonElement>('[aria-selected="true"]') ?? panel.current?.querySelector<HTMLButtonElement>('[role="option"]'))?.focus();
    const outside = (event: PointerEvent) => {
      const target = event.target as Node;
      if (!panel.current?.contains(target) && !trigger.current?.contains(target)) setOpen(false);
    };
    document.addEventListener('pointerdown', outside);
    window.addEventListener('resize', place);
    window.addEventListener('scroll', place, true);
    return () => { document.removeEventListener('pointerdown', outside); window.removeEventListener('resize', place); window.removeEventListener('scroll', place, true); };
  }, [open]);
  useEffect(() => {
    if (!open || !mobile) return;
    const previous = document.body.style.overflow;
    document.body.style.overflow = 'hidden';
    return () => { document.body.style.overflow = previous; };
  }, [open, mobile]);
  function keyDown(event: KeyboardEvent<HTMLDivElement>) {
    const buttons = Array.from(panel.current?.querySelectorAll<HTMLButtonElement>('[role="option"]') ?? []);
    const at = buttons.indexOf(document.activeElement as HTMLButtonElement);
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      buttons[Math.max(0, Math.min(buttons.length - 1, at + (event.key === 'ArrowDown' ? 1 : -1)))]?.focus();
    } else if (event.key === 'ArrowLeft' || event.key === 'ArrowRight') event.preventDefault();
    else if (event.key === 'Escape') { event.preventDefault(); close(true); }
    else if (mobile && event.key === 'Tab') {
      const all = Array.from(panel.current?.querySelectorAll<HTMLButtonElement>('button') ?? []);
      const current = all.indexOf(document.activeElement as HTMLButtonElement);
      if (event.shiftKey && current === 0) { event.preventDefault(); all.at(-1)?.focus(); }
      else if (!event.shiftKey && current === all.length - 1) { event.preventDefault(); all[0]?.focus(); }
    }
  }
  return <div className="aui-breakdown-chart-picker">
    <span id={id + '-label'} title={label}>{label}</span>
    <button ref={trigger} className="aui-breakdown-chart-picker-button" type="button" aria-labelledby={id + '-label ' + id + '-value'} aria-haspopup="listbox" aria-controls={open ? id + '-panel' : undefined} aria-expanded={open} disabled={disabled} onClick={() => setOpen(!open)}>
      <span id={id + '-value'} dir="auto" title={options.find(option => option.id === value)?.label ?? value}>{(options.find(option => option.id === value)?.label ?? value) || '설정 없음'}</span><ChevronDown size={14} strokeWidth={1.5} aria-hidden="true" />
    </button>
    {open && createPortal(<div className={'aui-breakdown-chart aui-breakdown-chart-overlay' + (mobile ? ' aui-breakdown-chart-overlay-mobile' : '')} style={themeStyle} onKeyDown={keyDown}>
      {mobile && <div className="aui-breakdown-chart-backdrop" onClick={() => close(true)} />}
      <div id={id + '-panel'} ref={panel} className="aui-breakdown-chart-menu" role={mobile ? 'dialog' : undefined} aria-modal={mobile || undefined} aria-labelledby={mobile ? id + '-sheet' : undefined} style={mobile ? undefined : position}>
        {mobile && <header className="aui-breakdown-chart-sheet-heading"><strong id={id + '-sheet'}>{label}</strong><button type="button" aria-label={label + ' 닫기'} onClick={() => close(true)}><X size={18} aria-hidden="true" /></button></header>}
        <div className="aui-breakdown-chart-list" role="listbox" aria-labelledby={id + '-label'}>{options.map(option => <button type="button" role="option" aria-selected={value === option.id} className="aui-breakdown-chart-option" key={option.id} title={option.label} onClick={() => { onChange(option.id); close(true); }}><span dir="auto">{option.label}</span>{value === option.id && <Check size={14} strokeWidth={1.5} aria-hidden="true" />}</button>)}</div>
      </div>
    </div>, document.body)}
  </div>;
}

export function BreakdownChart(props: BreakdownChartProps) {
  // Without at least one dimension and one measure there is nothing to explore, so nothing renders.
  if (!props.dimensions.length || !props.measures.length) return null;
  return <BreakdownChartBody {...props} />;
}

function BreakdownChartBody({ records, dimensions, measures, title = '에이전트 실행 분포', defaultMeasure, defaultSlice, defaultSegment, className = '', style }: BreakdownChartProps) {
  const id = useId(), tableContent = useRef<HTMLDivElement>(null);
  const card = useRef<HTMLDivElement>(null), plot = useRef<HTMLDivElement>(null), tip = useRef<HTMLDivElement>(null);
  const [measureId, setMeasureId] = useState(defaultMeasure ?? measures[0]?.id ?? '');
  const [sliceId, setSliceId] = useState(defaultSlice ?? dimensions[0]?.id ?? '');
  const [segmentId, setSegmentId] = useState(defaultSegment === undefined ? dimensions[1]?.id ?? '' : defaultSegment ?? '');
  const [excluded, setExcluded] = useState<string[]>([]);
  const [selected, setSelected] = useState<string | null>(null), [pinned, setPinned] = useState(false);
  const lastPointer = useRef('mouse');
  const [tableHeight, setTableHeight] = useState<number>(), [heightReady, setHeightReady] = useState(false);
  const measure = measures.find(item => item.id === measureId) ?? measures[0];
  const slice = dimensions.find(item => item.id === sliceId) ?? dimensions[0];
  // An unknown segment, or one equal to the slice, means the unsegmented view.
  const segment = dimensions.find(item => item.id === segmentId && item.id !== slice?.id);
  const configured = !!measure && !!slice;
  const aggregation = useMemo(() => {
    const segments = segment?.values.map((value, index) => ({ ...value, color: value.color ?? 'var(--aui-breakdown-chart-color-' + (index % 7 + 1) + ', ' + palette[index % 7] + ')' })) ?? [];
    const rows = (slice?.values ?? []).map(value => ({ ...value, values: Object.fromEntries((segment ? segments : [{ id: '' }]).map(item => [item.id, 0])) as Record<string, number> }));
    if (!configured) return { segments, rows: [] };
    const rowMap = new Map(rows.map(row => [row.id, row]));
    for (const record of records) {
      // Categories compare as strings, so the number 1 and the string "1" share a row.
      const sliceKey = keyOf(record[slice.field ?? slice.id]);
      const row = sliceKey === null ? undefined : rowMap.get(sliceKey);
      if (!row) continue;
      const segmentKey = segment ? keyOf(record[segment.field ?? segment.id]) : '';
      if (segmentKey === null || !Object.hasOwn(row.values, segmentKey)) continue;
      const raw = measure.field ? record[measure.field] : 1;
      const amount = raw === null || raw === undefined ? 0 : Number(raw);
      row.values[segmentKey] += Number.isFinite(amount) ? amount : 0;
    }
    return { segments, rows };
  }, [records, slice, segment, measure, configured]);
  // Cross-check hidden ids against the current segment values: ids retired by an external definition change no longer count as hidden.
  const segmentKey = aggregation.segments.map(value => value.id).join('\u0000');
  const hidden = excluded.filter(id => aggregation.segments.some(value => value.id === id));
  useEffect(() => {
    setExcluded(previous => { const kept = previous.filter(id => aggregation.segments.some(value => value.id === id)); return kept.length === previous.length ? previous : kept; });
  }, [segmentKey]);
  const visible = aggregation.segments.filter(value => !hidden.includes(value.id));
  const rows = aggregation.rows.map(row => ({ ...row, total: segment ? visible.reduce((sum, value) => sum + row.values[value.id], 0) : row.values[''] }));
  const total = rows.reduce((sum, row) => sum + row.total, 0);
  const ceiling = axisMaximum(Math.max(0, ...rows.map(row => row.total)));
  const activeIndex = rows.findIndex(row => row.id === selected), active = rows[activeIndex];
  const details = active ? visible.filter(value => active.values[value.id] !== 0).sort((a, b) => active.values[b.id] - active.values[a.id]) : [];
  const clear = () => { setSelected(null); setPinned(false); };
  useEffect(() => {
    const outside = (event: PointerEvent) => { if (!(event.target as Element).closest('[data-breakdown-target], .aui-breakdown-chart-readout')) clear(); };
    const keyboard = () => { lastPointer.current = 'keyboard'; };
    document.addEventListener('pointerdown', outside);
    document.addEventListener('keydown', keyboard, true);
    return () => { document.removeEventListener('pointerdown', outside); document.removeEventListener('keydown', keyboard, true); };
  }, []);
  useLayoutEffect(() => {
    const content = tableContent.current;
    if (!content) return;
    const measureHeight = () => setTableHeight(content.getBoundingClientRect().height);
    measureHeight();
    const ready = requestAnimationFrame(() => setHeightReady(true));
    const observer = new ResizeObserver(measureHeight);
    observer.observe(content);
    return () => { observer.disconnect(); cancelAnimationFrame(ready); };
  }, [configured, rows.length]);
  // Keep the tooltip and highlighted axis pill 4px inside the card after their side or centre placement.
  useLayoutEffect(() => {
    const place = () => {
      const frame = card.current?.getBoundingClientRect(), area = plot.current?.getBoundingClientRect();
      if (!frame || !area) return;
      const node = tip.current;
      if (node && activeIndex >= 0) {
        const width = node.offsetWidth, anchor = (activeIndex + .5) / rows.length;
        const preferred = anchor <= .5 ? area.width * anchor + 22 : area.width * anchor - 22 - width;
        const min = frame.left + 4 - area.left, max = frame.right - 4 - area.left - width;
        node.style.left = Math.round(Math.max(min, Math.min(preferred, max))) + 'px';
      }
      const pill = plot.current?.querySelector<HTMLElement>('.aui-breakdown-chart-axis-pill');
      const cell = pill?.parentElement?.getBoundingClientRect();
      if (pill && cell) {
        const width = pill.offsetWidth, centred = cell.width / 2 - width / 2;
        const min = frame.left + 4 - cell.left, max = frame.right - 4 - cell.left - width;
        pill.style.left = Math.round(Math.max(min, Math.min(centred, max))) + 'px';
      }
    };
    place();
    const observer = new ResizeObserver(place);
    if (card.current) observer.observe(card.current);
    return () => observer.disconnect();
  });
  const toggleTarget = (rowId: string) => { if (pinned && selected === rowId) clear(); else { setSelected(rowId); setPinned(true); } };
  function moveBar(event: KeyboardEvent<HTMLButtonElement>, index: number) {
    if (event.key === 'Escape') { clear(); return; }
    let next = index;
    if (event.key === 'ArrowRight' || event.key === 'ArrowDown') next = Math.min(rows.length - 1, index + 1);
    else if (event.key === 'ArrowLeft' || event.key === 'ArrowUp') next = Math.max(0, index - 1);
    else if (event.key === 'Home') next = 0;
    else if (event.key === 'End') next = rows.length - 1;
    else return;
    event.preventDefault();
    setPinned(false); setSelected(rows[next].id);
    event.currentTarget.parentElement?.querySelectorAll<HTMLButtonElement>('.aui-breakdown-chart-bar-button')[next]?.focus();
  }
  const detailContent = active && <><div className="aui-breakdown-chart-detail-heading"><span title={active.label}>{active.icon && <span className="aui-breakdown-chart-icon" aria-hidden="true">{active.icon}</span>}<span className="aui-breakdown-chart-name" dir="auto">{active.label}</span></span><strong>{format(active.total)}</strong></div>{segment && <div className="aui-breakdown-chart-detail-values">{details.map(value => <div key={value.id}><i style={{ background: value.color }} /><span className="aui-breakdown-chart-name" dir="auto" title={value.label}>{value.label}</span><b>{format(active.values[value.id])}</b></div>)}</div>}</>;
  return <section className={'aui-breakdown-chart ' + className} style={style} aria-labelledby={id + '-title'} onPointerDownCapture={event => { lastPointer.current = event.pointerType; }}>
    <style>{styles}</style>
    <div className="aui-breakdown-chart-tray">
      <header className="aui-breakdown-chart-heading"><h2 id={id + '-title'} title={title} dir="auto">{title}</h2><span><strong>{format(total)}</strong> {measure?.unit ?? ''}</span></header>
      <div className="aui-breakdown-chart-card" ref={card}>
        <div className="aui-breakdown-chart-pickers">
          <Picker label="집계 방식" value={measure?.id ?? ''} options={measures} onChange={setMeasureId} disabled={!configured} />
          <Picker label="가로축 분류" value={slice?.id ?? ''} options={dimensions} onChange={next => { setSliceId(next); if (next === segment?.id) { setSegmentId(''); setExcluded([]); } }} disabled={!configured} />
          <Picker label="누적 구분" value={segment?.id ?? ''} options={[{ id: '', label: '분할 없음' }, ...dimensions.filter(value => value.id !== slice?.id)]} onChange={next => { if (next !== (segment?.id ?? '')) { setSegmentId(next); setExcluded([]); } }} disabled={!configured} />
        </div>
        {!configured ? <div className="aui-breakdown-chart-empty" role="status"><BarChart3 size={28} aria-hidden="true" /><strong>차트 설정을 확인해 주세요</strong><p>분류와 집계 정의가 각각 하나 이상 필요합니다.</p></div> : rows.length ? <>
          {!records.length && <p className="aui-breakdown-chart-zero-note">실행 기록을 기다리는 중입니다. 정의된 분류의 0값을 표시합니다.</p>}
          <div className="aui-breakdown-chart-chart">
            <div className="aui-breakdown-chart-scroll">
              <div className="aui-breakdown-chart-plot" ref={plot} style={{ minWidth: (rows.length * 44 + Math.max(0, rows.length - 1) * 4) + 'px' }}>
                <div className="aui-breakdown-chart-grid" aria-hidden="true">{Array.from({ length: 6 }, (_, index) => <span key={index} style={{ top: (index * 20) + '%' }} />)}</div>
                <div className="aui-breakdown-chart-bars" onPointerLeave={() => { if (!pinned) setSelected(null); }}>
                  {rows.map((row, index) => {
                    const height = Math.max(0, row.total / ceiling * 200);
                    const pieces = segment ? visible.filter(value => row.values[value.id] > 0) : [{ id: '', color: 'var(--aui-breakdown-chart-solid, var(--aui-breakdown-chart-accent))' }].filter(() => row.total > 0);
                    const heights = segmentHeights(pieces.map(value => Math.max(0, row.values[value.id])), height);
                    return <button type="button" key={row.id} data-breakdown-target={row.id} className="aui-breakdown-chart-bar-button" aria-label={row.label + ': ' + format(row.total) + ' ' + (measure.unit ?? '')} aria-pressed={selected === row.id} data-active={selected === row.id} data-dim={!!active && selected !== row.id} onPointerEnter={event => { if (event.pointerType === 'mouse' && !pinned) setSelected(row.id); }} onFocus={() => { if (lastPointer.current !== 'touch') { setPinned(false); setSelected(row.id); } }} onBlur={() => { if (!pinned) setSelected(null); }} onClick={() => { if (lastPointer.current === 'touch') toggleTarget(row.id); }} onKeyDown={event => moveBar(event, index)}>
                      <span className="aui-breakdown-chart-empty-bar" style={{ bottom: height + 'px' }} aria-hidden="true" />
                      <span className="aui-breakdown-chart-stack" style={{ height: height + 'px' }} aria-hidden="true">{pieces.map((value, pieceIndex) => <span key={value.id} className="aui-breakdown-chart-segment" style={{ height: heights[pieceIndex] + 'px', background: value.color }} />)}</span>
                    </button>;
                  })}
                </div>
                {active && <div ref={tip} className="aui-breakdown-chart-tooltip" role="status" aria-live="polite">{detailContent}</div>}
                <div className="aui-breakdown-chart-axis" aria-hidden="true">{rows.map(row => <div key={row.id} title={row.label}><span className="aui-breakdown-chart-axis-base" dir="auto">{row.icon ? <span className="aui-breakdown-chart-icon">{row.icon}</span> : row.label}</span>{selected === row.id && <span className="aui-breakdown-chart-axis-pill">{row.icon && <span className="aui-breakdown-chart-icon">{row.icon}</span>}<span className="aui-breakdown-chart-name" dir="auto">{row.label}</span></span>}</div>)}</div>
              </div>
            </div>
            <div className="aui-breakdown-chart-ticks" aria-hidden="true">{Array.from({ length: 6 }, (_, index) => <span key={index} style={{ top: (index * 40 - 8) + 'px' }}>{formatTick(ceiling * (5 - index) / 5)}</span>)}</div>
          </div>
          <div className="aui-breakdown-chart-readout" role="status" aria-live="polite">{active ? <>{detailContent}<button type="button" onClick={clear}>선택 해제</button></> : <span>막대나 아래 행을 탭해 상세 수치를 확인하세요.</span>}</div>
          <div className="aui-breakdown-chart-table-shell" style={{ height: tableHeight }} data-ready={heightReady}>
            <div ref={tableContent}>
              <div className="aui-breakdown-chart-table-scroll"><table className="aui-breakdown-chart-table">
                <caption className="aui-breakdown-chart-sr">{title} 상세 수치. 합계는 현재 보이는 구분만 포함합니다. 흐린 열은 감춘 구분의 원래 수치입니다.</caption>
                <thead><tr><th scope="col">{slice.label}</th><th scope="col">{measure.label}</th>{aggregation.segments.map(value => <th scope="col" key={value.id} data-hidden={hidden.includes(value.id)} title={value.label}><span><i style={{ background: value.color }} /><span className="aui-breakdown-chart-name" dir="auto">{value.label}</span></span></th>)}</tr></thead>
                <tbody>{rows.map(row => <tr key={row.id} data-breakdown-target={row.id} data-active={selected === row.id} onPointerEnter={event => { if (event.pointerType === 'mouse' && !pinned) setSelected(row.id); }} onPointerLeave={() => { if (!pinned) setSelected(null); }} onClick={() => { if (lastPointer.current === 'touch') toggleTarget(row.id); }}><th scope="row" title={row.label}><span>{row.icon && <span className="aui-breakdown-chart-icon" aria-hidden="true">{row.icon}</span>}<span className="aui-breakdown-chart-name" dir="auto">{row.label}</span></span></th><td>{format(row.total)}</td>{aggregation.segments.map(value => <td key={value.id} data-hidden={hidden.includes(value.id)}>{format(row.values[value.id])}</td>)}</tr>)}</tbody>
              </table></div>
              <div className="aui-breakdown-chart-mobile-rows">{rows.map(row => <article key={row.id} data-active={selected === row.id} className="aui-breakdown-chart-mobile-row"><button type="button" data-breakdown-target={row.id} aria-pressed={selected === row.id} onClick={() => toggleTarget(row.id)}><span>{row.icon && <span className="aui-breakdown-chart-icon" aria-hidden="true">{row.icon}</span>}{row.label}</span><strong>{format(row.total)}</strong></button>{segment && <dl>{aggregation.segments.map(value => <div key={value.id} data-hidden={hidden.includes(value.id)}><dt><i style={{ background: value.color }} />{value.label}{hidden.includes(value.id) && <span className="aui-breakdown-chart-sr"> (차트에서 감춤)</span>}</dt><dd>{format(row.values[value.id])}</dd></div>)}</dl>}</article>)}</div>
            </div>
          </div>
        </> : <div className="aui-breakdown-chart-empty" role="status"><BarChart3 size={28} aria-hidden="true" /><strong>가로축에 표시할 분류값이 없습니다</strong><p>분류 정의에 값을 추가하면 막대와 표를 표시합니다.</p></div>}
      </div>
      <footer className="aui-breakdown-chart-footer">{segment ? <div className="aui-breakdown-chart-legend" role="group" aria-label={segment.label + ' 표시 토글'}>{aggregation.segments.map(value => <button type="button" key={value.id} title={value.label} aria-pressed={!hidden.includes(value.id)} onClick={() => setExcluded(previous => previous.includes(value.id) ? previous.filter(item => item !== value.id) : visible.length > 1 ? [...previous.filter(id => aggregation.segments.some(item => item.id === id)), value.id] : previous)}><i style={{ background: value.color }} /><span className="aui-breakdown-chart-name" dir="auto">{value.label}</span></button>)}</div> : <span className="aui-breakdown-chart-hint">누적 구분을 선택해 작업의 구성을 살펴보세요.</span>}{hidden.length > 0 && <button className="aui-breakdown-chart-restore" type="button" onClick={() => setExcluded([])}>전체 복원</button>}</footer>
    </div>
  </section>;
}

const styles = `
@layer kit {
.aui-breakdown-chart { --aui-breakdown-chart-tray:var(--aui-bg-2,#f0f0f0); --aui-breakdown-chart-card:var(--aui-bg-1,#fff); --aui-breakdown-chart-line:var(--aui-line-soft,#ececec); --aui-breakdown-chart-border:var(--aui-line,#e5e5e5); --aui-breakdown-chart-grid:var(--aui-line-soft,#ebebeb); --aui-breakdown-chart-zero:var(--aui-line,#dcdcdc); --aui-breakdown-chart-fg:var(--aui-fg,#171717); --aui-breakdown-chart-body:var(--aui-fg-dim,#404040); --aui-breakdown-chart-muted:var(--aui-fg-muted,#8f8f8f); --aui-breakdown-chart-accent:var(--aui-accent,#4fb0fe); --aui-breakdown-chart-focus:var(--aui-focus,rgb(0 96 226 / .7)); --aui-breakdown-chart-restore-hover:rgb(1 144 254 / .08); --aui-breakdown-chart-r:var(--aui-r-lg,16px); --aui-breakdown-chart-m:var(--aui-r-md,10px); --aui-breakdown-chart-s:var(--aui-r-sm,7px); --aui-breakdown-chart-button:var(--aui-bg-1,#fff); --aui-breakdown-chart-hover:var(--aui-bg-2,#f8f8f8); --aui-breakdown-chart-option-hover:var(--aui-bg-2,#f5f5f5); --aui-breakdown-chart-empty:var(--aui-bg-2,#f2f2f2); --aui-breakdown-chart-head:var(--aui-bg-1,#fcfcfc); --aui-breakdown-chart-row:var(--aui-bg-2,#f6f6f6); --aui-breakdown-chart-legend-hover:#0000000d; --aui-breakdown-chart-float:var(--aui-bg-1,#fff); --aui-breakdown-chart-shadow:0 10px 28px -8px #0000002e; --aui-breakdown-chart-card-shadow:0 1px 2px #0000000f; --aui-breakdown-chart-button-shadow:0 1px 2px #0000000a; --aui-breakdown-chart-pill-shadow:0 2px 6px #0000000f; --aui-breakdown-chart-ease:var(--aui-ease,cubic-bezier(.22,1,.36,1)); width:100%; max-width:560px; min-width:0; container-type:inline-size; color:var(--aui-breakdown-chart-body); font:13px/1.4 var(--aui-font-sans,Inter,ui-sans-serif,system-ui,sans-serif); font-variant-numeric:tabular-nums; word-break:keep-all; overflow-wrap:anywhere; }
:where(.dark,[data-theme="dark"]) .aui-breakdown-chart { --aui-breakdown-chart-tray:var(--aui-bg-2,#161618); --aui-breakdown-chart-card:var(--aui-bg-1,#232326); --aui-breakdown-chart-line:var(--aui-line-soft,#ffffff14); --aui-breakdown-chart-border:var(--aui-line,#ffffff14); --aui-breakdown-chart-grid:var(--aui-line-soft,#ffffff17); --aui-breakdown-chart-zero:var(--aui-line,#ffffff29); --aui-breakdown-chart-fg:var(--aui-fg,#ededed); --aui-breakdown-chart-body:var(--aui-fg-dim,#c4c4c4); --aui-breakdown-chart-muted:var(--aui-fg-muted,#8f8f8f); --aui-breakdown-chart-accent:var(--aui-accent,#4fb0fe); --aui-breakdown-chart-focus:var(--aui-focus,rgb(77 141 255 / .8)); --aui-breakdown-chart-restore-hover:rgb(79 176 254 / .16); --aui-breakdown-chart-button:var(--aui-bg-2,#2a2a2d); --aui-breakdown-chart-hover:var(--aui-bg-3,#313135); --aui-breakdown-chart-option-hover:var(--aui-bg-3,#313135); --aui-breakdown-chart-empty:var(--aui-bg-2,#ffffff0f); --aui-breakdown-chart-head:var(--aui-bg-1,#262629); --aui-breakdown-chart-row:var(--aui-bg-2,#2a2a2d); --aui-breakdown-chart-legend-hover:#ffffff0f; --aui-breakdown-chart-float:var(--aui-bg-2,#2a2a2d); --aui-breakdown-chart-shadow:0 12px 30px -8px #00000080; --aui-breakdown-chart-card-shadow:0 1px 2px #00000066; --aui-breakdown-chart-button-shadow:0 1px 2px #00000059; --aui-breakdown-chart-pill-shadow:0 12px 30px -8px #00000080; }
.aui-breakdown-chart *, .aui-breakdown-chart *::before, .aui-breakdown-chart *::after { box-sizing:border-box; }
.aui-breakdown-chart button { margin:0; font:inherit; color:inherit; cursor:pointer; touch-action:manipulation; }
.aui-breakdown-chart button:disabled { cursor:default; opacity:.5; }
.aui-breakdown-chart button:focus-visible { outline:2px solid var(--aui-breakdown-chart-focus); outline-offset:2px; }
.aui-breakdown-chart-tray { padding:6px; background:var(--aui-breakdown-chart-tray); border-radius:calc(var(--aui-breakdown-chart-r) + 6px); }
.aui-breakdown-chart-heading { display:flex; flex-wrap:wrap; align-items:baseline; justify-content:space-between; gap:2px 12px; padding:11px 18px 10px; }
.aui-breakdown-chart-heading h2 { flex:1 1 min(120px,100%); min-width:0; margin:0; overflow:hidden; font-size:14px; font-weight:500; line-height:20px; white-space:nowrap; text-overflow:ellipsis; color:var(--aui-breakdown-chart-fg); }
.aui-breakdown-chart-heading > span { min-width:0; max-width:100%; font-size:14px; color:var(--aui-breakdown-chart-muted); text-align:right; line-height:20px; }
.aui-breakdown-chart-heading strong { font-size:15px; font-weight:600; white-space:nowrap; color:var(--aui-breakdown-chart-fg); }
.aui-breakdown-chart-card { overflow:hidden; border:1px solid var(--aui-breakdown-chart-border); border-radius:var(--aui-breakdown-chart-r); background:var(--aui-breakdown-chart-card); box-shadow:var(--aui-shadow-sm,var(--aui-breakdown-chart-card-shadow)); }
.aui-breakdown-chart-pickers { display:grid; grid-template-columns:repeat(3,minmax(0,1fr)); gap:10px; padding:16px 18px 4px; }
.aui-breakdown-chart-picker { display:grid; gap:6px; min-width:0; }
.aui-breakdown-chart-picker > span { overflow:hidden; white-space:nowrap; text-overflow:ellipsis; }
.aui-breakdown-chart-picker { color:var(--aui-breakdown-chart-muted); font-size:12px; line-height:16px; }
.aui-breakdown-chart-picker-button { display:flex; align-items:center; justify-content:space-between; gap:8px; width:100%; min-width:0; height:34px; padding:0 10px 0 12px; border:1px solid var(--aui-breakdown-chart-border); border-radius:var(--aui-breakdown-chart-m); background:var(--aui-breakdown-chart-button); color:var(--aui-breakdown-chart-fg)!important; font-size:13px!important; font-weight:500!important; text-align:left; box-shadow:var(--aui-breakdown-chart-button-shadow); transition:background 140ms ease; }
.aui-breakdown-chart-picker-button > span { overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
.aui-breakdown-chart-picker-button svg { flex:none; color:var(--aui-breakdown-chart-muted); transition:transform 200ms var(--aui-breakdown-chart-ease); }
.aui-breakdown-chart-picker-button[aria-expanded="true"] svg { transform:rotate(180deg); }
.aui-breakdown-chart-picker-button:hover { background:var(--aui-breakdown-chart-hover); }
.aui-breakdown-chart-overlay { position:fixed; inset:0; max-width:none; z-index:1000; pointer-events:none; container-type:normal; }
.aui-breakdown-chart-menu { position:fixed; z-index:6; padding:4px; max-height:calc(100dvh - 24px); overflow-y:auto; border:1px solid var(--aui-breakdown-chart-border); border-radius:calc(var(--aui-breakdown-chart-r) - 4px); background:var(--aui-breakdown-chart-float); box-shadow:var(--aui-breakdown-chart-shadow); pointer-events:auto; animation:aui-breakdown-chart-menu-in 180ms var(--aui-breakdown-chart-ease); }
.aui-breakdown-chart-list { display:grid; gap:2px; }
.aui-breakdown-chart-option { display:flex; align-items:center; justify-content:space-between; gap:8px; min-height:30px; padding:5px 10px; border:0; border-radius:calc(var(--aui-breakdown-chart-s) + 1px); background:transparent; font-size:13px!important; text-align:left; }
.aui-breakdown-chart-option svg { flex:none; }
.aui-breakdown-chart-option > span, .aui-breakdown-chart-name { min-width:0; overflow:hidden; white-space:nowrap; text-overflow:ellipsis; }
.aui-breakdown-chart-option[aria-selected="true"] { color:var(--aui-breakdown-chart-accent); font-weight:500; }
.aui-breakdown-chart-option:hover, .aui-breakdown-chart-option:focus { background:var(--aui-breakdown-chart-option-hover); outline:none!important; }
.aui-breakdown-chart-chart { position:relative; margin:22px 18px 12px; padding-right:30px; }
.aui-breakdown-chart-scroll { overflow-x:auto; overscroll-behavior-x:contain; scrollbar-width:thin; touch-action:pan-x pan-y; padding:4px; margin:-4px; }
.aui-breakdown-chart-plot { position:relative; }
.aui-breakdown-chart-grid { position:absolute; top:0; width:100%; height:200px; pointer-events:none; }
.aui-breakdown-chart-grid > span { position:absolute; width:100%; border-top:1px dashed var(--aui-breakdown-chart-grid); }
.aui-breakdown-chart-grid > span:last-child { border-top:1px solid var(--aui-breakdown-chart-zero); }
.aui-breakdown-chart-bars { position:relative; display:flex; gap:4px; height:200px; }
.aui-breakdown-chart-bar-button { position:relative; flex:1 0 44px; min-width:44px; height:200px; border:0; padding:0; background:none; transition:opacity 200ms ease; touch-action:pan-x pan-y!important; }
.aui-breakdown-chart-bar-button:focus-visible { outline-offset:2px!important; }
.aui-breakdown-chart-bar-button[data-dim="true"] { opacity:.38; }
.aui-breakdown-chart-stack { position:absolute; bottom:0; left:50%; translate:-50% 0; display:flex; flex-direction:column-reverse; gap:1px; width:min(10px,100%); overflow:hidden; transition:height 520ms var(--aui-breakdown-chart-ease); }
.aui-breakdown-chart-segment { flex:none; width:100%; min-height:2px; transition:height 520ms var(--aui-breakdown-chart-ease),background-color 300ms ease; }
.aui-breakdown-chart-empty-bar { position:absolute; top:0; left:50%; translate:-50% 0; width:min(10px,100%); background:var(--aui-breakdown-chart-empty); opacity:0; transition:opacity 180ms ease,bottom 520ms var(--aui-breakdown-chart-ease); }
.aui-breakdown-chart-bar-button[data-active="true"] .aui-breakdown-chart-empty-bar { opacity:1; }
.aui-breakdown-chart-ticks { position:absolute; top:0; right:0; width:26px; height:200px; color:var(--aui-breakdown-chart-muted); font-size:11px; line-height:16px; }
.aui-breakdown-chart-ticks > span { position:absolute; right:0; white-space:nowrap; }
.aui-breakdown-chart-axis { display:flex; gap:4px; margin-top:8px; height:24px; color:var(--aui-breakdown-chart-muted); font-size:11px; }
.aui-breakdown-chart-axis > div { position:relative; flex:1 0 44px; min-width:44px; text-align:center; }
.aui-breakdown-chart-axis-base { display:block; height:24px; line-height:24px; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
.aui-breakdown-chart-axis-pill { position:absolute; z-index:2; left:0; top:0; display:flex; align-items:center; gap:6px; max-width:min(220px,80cqw); height:24px; padding:0 10px 0 7px; border-radius:999px; background:var(--aui-breakdown-chart-float); color:var(--aui-breakdown-chart-fg); box-shadow:0 0 0 1px var(--aui-breakdown-chart-border),var(--aui-breakdown-chart-pill-shadow); font-size:12px; font-weight:500; white-space:nowrap; animation:aui-breakdown-chart-pill-in 200ms var(--aui-breakdown-chart-ease); }
.aui-breakdown-chart-icon { display:inline-flex; flex:none; align-items:center; justify-content:center; width:15px; height:15px; }
.aui-breakdown-chart-icon svg { width:15px; height:15px; stroke-width:1.4px; }
.aui-breakdown-chart-tooltip { position:absolute; z-index:4; top:0; left:0; width:max-content; min-width:150px; max-width:min(260px,100%); padding:9px 11px 10px; border:1px solid var(--aui-breakdown-chart-border); border-radius:calc(var(--aui-breakdown-chart-r) - 4px); background:var(--aui-breakdown-chart-float); box-shadow:var(--aui-breakdown-chart-shadow); pointer-events:none; transition:left 200ms var(--aui-breakdown-chart-ease); animation:aui-breakdown-chart-tooltip-in 160ms var(--aui-breakdown-chart-ease); }
.aui-breakdown-chart-detail-heading { display:flex; align-items:flex-start; justify-content:space-between; gap:14px; font-size:13px; font-weight:500; line-height:20px; color:var(--aui-breakdown-chart-fg); }
.aui-breakdown-chart-detail-heading > span { display:flex; align-items:center; gap:6px; min-width:0; }
.aui-breakdown-chart-detail-heading strong { flex:none; font-weight:700; }
.aui-breakdown-chart-detail-values { display:grid; gap:2px; margin-top:6px; padding-top:6px; border-top:1px solid var(--aui-breakdown-chart-line); }
.aui-breakdown-chart-detail-values > div { display:flex; align-items:center; gap:7px; font-size:12px; line-height:18px; }
.aui-breakdown-chart-detail-values > div > span { flex:1; min-width:0; }
.aui-breakdown-chart-detail-values b { font-weight:400; }
.aui-breakdown-chart i { display:inline-block; flex:0 0 8px; width:8px; height:8px; border-radius:2px; }
.aui-breakdown-chart-readout { display:none; }
.aui-breakdown-chart-table-shell { border-top:1px solid var(--aui-breakdown-chart-line); overflow:clip; }
.aui-breakdown-chart-table-shell[data-ready="true"] { transition:height 380ms var(--aui-breakdown-chart-ease); }
.aui-breakdown-chart-table-scroll { overflow-x:auto; scrollbar-width:thin; }
.aui-breakdown-chart-table { width:100%; border-collapse:separate; border-spacing:0; font-size:13px; line-height:18px; white-space:nowrap; }
.aui-breakdown-chart-table th, .aui-breakdown-chart-table td { height:38px; padding:9px 14px; border-top:1px solid var(--aui-breakdown-chart-line); text-align:right; font-weight:400; }
.aui-breakdown-chart-table thead th { border-top:0; color:var(--aui-breakdown-chart-muted); background:var(--aui-breakdown-chart-head); font-weight:450; }
.aui-breakdown-chart-table th:first-child { position:sticky; left:0; z-index:1; text-align:left; background:var(--aui-breakdown-chart-card); font-weight:450; }
.aui-breakdown-chart-table thead th:first-child { background:var(--aui-breakdown-chart-head); }
.aui-breakdown-chart-table th > span { display:flex; align-items:center; gap:8px; }
.aui-breakdown-chart-table tbody th > span { max-width:220px; }
.aui-breakdown-chart-table thead th[data-hidden] > span, .aui-breakdown-chart-table thead th:not(:first-child) > span { max-width:160px; margin-left:auto; }
.aui-breakdown-chart-table tbody tr, .aui-breakdown-chart-table tbody th { transition:background 140ms ease; }
.aui-breakdown-chart-table tbody tr[data-active="true"], .aui-breakdown-chart-table tbody tr[data-active="true"] th { background:var(--aui-breakdown-chart-row); }
.aui-breakdown-chart [data-hidden="true"] { opacity:.35; }
.aui-breakdown-chart-mobile-rows { display:none; }
.aui-breakdown-chart-footer { display:flex; align-items:flex-start; justify-content:space-between; gap:10px; min-height:44px; padding:6px 10px 7px; }
.aui-breakdown-chart-legend { display:flex; flex-wrap:wrap; gap:2px; min-width:0; }
.aui-breakdown-chart-legend button, .aui-breakdown-chart-restore { display:inline-flex; align-items:center; justify-content:center; gap:6px; min-height:28px; max-width:100%; padding:5px 9px; border:0; border-radius:calc(var(--aui-breakdown-chart-s) + 1px); background:transparent; font-size:12.5px!important; transition:background 140ms ease,opacity 160ms ease; }
.aui-breakdown-chart-legend button:hover { background:var(--aui-breakdown-chart-legend-hover); }
.aui-breakdown-chart-legend button[aria-pressed="false"] { opacity:.45; text-decoration:line-through; }
.aui-breakdown-chart-restore { flex:none; padding-inline:10px; color:var(--aui-breakdown-chart-accent)!important; font-weight:500!important; }
.aui-breakdown-chart-restore:hover { background:var(--aui-breakdown-chart-restore-hover); }
.aui-breakdown-chart-hint { padding:5px 8px; color:var(--aui-breakdown-chart-muted); font-size:12.5px; }
.aui-breakdown-chart-empty { display:grid; justify-items:center; gap:10px; padding:45px 22px; text-align:center; color:var(--aui-breakdown-chart-muted); }
.aui-breakdown-chart-empty strong { color:var(--aui-breakdown-chart-fg); }
.aui-breakdown-chart-empty p { margin:0; }
.aui-breakdown-chart-zero-note { margin:12px 18px 0; color:var(--aui-breakdown-chart-muted); font-size:12px; }
.aui-breakdown-chart-sr { position:absolute; width:1px; height:1px; padding:0; margin:-1px; overflow:hidden; clip-path:inset(50%); white-space:nowrap; }
@container (max-width:440px) {
 .aui-breakdown-chart-pickers { grid-template-columns:1fr; padding-inline:14px; }
 .aui-breakdown-chart-picker-button { height:44px; font-size:16px!important; }
 .aui-breakdown-chart-chart { margin-inline:14px; }
 .aui-breakdown-chart-tooltip { display:none; }
 .aui-breakdown-chart-readout { display:grid; gap:8px; min-height:52px; padding:10px 14px; color:var(--aui-breakdown-chart-muted); font-size:12px; }
 .aui-breakdown-chart-readout button { min-width:44px; min-height:44px; justify-self:start; padding:8px 10px; border:1px solid var(--aui-breakdown-chart-border); border-radius:8px; background:var(--aui-breakdown-chart-button); }
 .aui-breakdown-chart-table-scroll { display:none; }
 .aui-breakdown-chart-mobile-rows { display:grid; }
 .aui-breakdown-chart-mobile-row { padding:8px 14px 12px; border-top:1px solid var(--aui-breakdown-chart-line); }
 .aui-breakdown-chart-mobile-row:first-child { border-top:0; }
 .aui-breakdown-chart-mobile-row[data-active="true"] { background:var(--aui-breakdown-chart-row); }
 .aui-breakdown-chart-mobile-row > button { display:flex; justify-content:space-between; align-items:center; gap:12px; width:100%; min-height:44px; padding:4px 0; border:0; background:none; text-align:left; }
 .aui-breakdown-chart-mobile-row > button > span { display:flex; align-items:center; gap:6px; min-width:0; }
 .aui-breakdown-chart-mobile-row > button strong { flex:none; color:var(--aui-breakdown-chart-fg); }
 .aui-breakdown-chart-mobile-row dl { display:grid; grid-template-columns:repeat(2,minmax(0,1fr)); gap:6px 14px; margin:4px 0 0; font-size:12px; }
 .aui-breakdown-chart-mobile-row dl > div { display:flex; justify-content:space-between; gap:7px; min-width:0; }
 .aui-breakdown-chart-mobile-row dt { display:flex; align-items:center; gap:6px; min-width:0; }
 .aui-breakdown-chart-mobile-row dd { flex:none; margin:0; }
 .aui-breakdown-chart-footer { flex-wrap:wrap; }
}
@media (pointer:coarse) { .aui-breakdown-chart-picker-button, .aui-breakdown-chart-option, .aui-breakdown-chart-legend button, .aui-breakdown-chart-restore { min-width:44px; min-height:44px; } .aui-breakdown-chart-picker-button { font-size:16px!important; } .aui-breakdown-chart-table th, .aui-breakdown-chart-table td { height:44px; padding-block:12px; } }
.aui-breakdown-chart-overlay-mobile { pointer-events:auto; }
.aui-breakdown-chart-backdrop { position:absolute; inset:0; background:var(--aui-breakdown-chart-backdrop,#0006); }
.aui-breakdown-chart-overlay-mobile .aui-breakdown-chart-menu { top:auto; left:max(8px,env(safe-area-inset-left)); right:max(8px,env(safe-area-inset-right)); bottom:0; max-height:calc(100dvh - env(safe-area-inset-top) - 24px); padding:8px 8px max(12px,env(safe-area-inset-bottom)); border-radius:18px 18px 0 0; }
.aui-breakdown-chart-sheet-heading { display:flex; align-items:center; justify-content:space-between; gap:12px; padding:0 8px 8px; font-size:16px; color:var(--aui-breakdown-chart-fg); }
.aui-breakdown-chart-sheet-heading button { display:grid; place-items:center; width:44px; min-height:44px; padding:0; border:0; border-radius:8px; background:transparent; }
.aui-breakdown-chart-overlay-mobile .aui-breakdown-chart-option { min-height:44px; font-size:16px!important; }
@keyframes aui-breakdown-chart-menu-in { from { opacity:0; translate:0 -4px; scale:.98; } to { opacity:1; translate:0 0; scale:1; } }
@keyframes aui-breakdown-chart-tooltip-in { from { opacity:0; } to { opacity:1; } }
@keyframes aui-breakdown-chart-pill-in { from { opacity:0; scale:.92; } to { opacity:1; scale:1; } }
@media (prefers-reduced-motion:reduce) { .aui-breakdown-chart *, .aui-breakdown-chart *::before, .aui-breakdown-chart *::after { transition:none!important; animation:none!important; } }
}
`;
