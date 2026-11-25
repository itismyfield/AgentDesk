// Clean-room implementation from the 2026-11-25 clean-spec handoff. No affectUI Pro source is used.
import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type KeyboardEvent as ReactKeyboardEvent } from 'react';
import { Check, Diamond, LayoutGrid, X } from 'lucide-react';

export type HandoffLaneKind = 'person' | 'agent' | 'tool';
export type HandoffStatus = 'queued' | 'active' | 'waiting' | 'done' | 'failed' | 'skipped';
export type HandoffKind = 'task' | 'gate' | 'start' | 'end';

export interface HandoffLane {
  id: string;
  name: string;
  kind: HandoffLaneKind;
  role?: string;
  color?: string;
  description?: string;
}

export interface HandoffItem {
  id: string;
  laneId: string;
  title: string;
  status: HandoffStatus;
  kind: HandoffKind;
  predecessors?: string[];
  duration?: string;
  detail?: string;
}

export interface HandoffSwimlanesLabels {
  progress: (done: number, total: number) => string;
  waiting: string;
  approve: string;
  reject: string;
  close: string;
  empty: string;
  loading: string;
  after: (titles: string) => string;
  status: Record<HandoffStatus, string>;
  kind: Record<HandoffKind, string>;
}

export interface HandoffSwimlanesProps {
  lanes?: HandoffLane[];
  /** Work items. `items` is the same list. */
  tasks?: HandoffItem[];
  items?: HandoffItem[];
  title?: string;
  loading?: boolean;
  labels?: Partial<HandoffSwimlanesLabels>;
  onSelect?: (id: string | null) => void;
  onApprove?: (id: string) => void;
  onReject?: (id: string) => void;
  className?: string;
  style?: CSSProperties;
}

const NO_LANES: HandoffLane[] = [];
const NO_ITEMS: HandoffItem[] = [];

const defaultLabels: HandoffSwimlanesLabels = {
  progress: (done, total) => `${done} of ${total}`,
  waiting: 'Waiting for you',
  approve: 'Approve',
  reject: 'Reject',
  close: 'Close',
  empty: 'No handoffs yet.',
  loading: 'Loading handoffs',
  after: (titles) => `After ${titles}`,
  status: { queued: 'Queued', active: 'In progress', waiting: 'Waiting', done: 'Done', failed: 'Failed', skipped: 'Skipped' },
  kind: { task: 'Task', gate: 'Gate', start: 'Start', end: 'End' },
};

interface Edge { id: string; d: string; kind: 'done' | 'gate' | 'tail' | 'bad' }
interface Rail { id: string; d: string }

function depthMap(items: HandoffItem[]) {
  const byId = new Map(items.map((item) => [item.id, item]));
  const memo = new Map<string, number>();
  const visiting = new Set<string>();
  const depth = (id: string): number => {
    if (memo.has(id)) return memo.get(id)!;
    if (visiting.has(id)) return 0;
    visiting.add(id);
    const preds = (byId.get(id)?.predecessors ?? []).filter((pred) => pred !== id && byId.has(pred));
    const value = preds.length ? Math.max(...preds.map(depth)) + 1 : 0;
    visiting.delete(id);
    memo.set(id, value);
    return value;
  };
  items.forEach((item) => depth(item.id));
  return memo;
}

function edgeKind(source: HandoffItem, target: HandoffItem): Edge['kind'] {
  if (source.status === 'failed' || target.status === 'failed') return 'bad';
  if (target.kind === 'gate' && target.status === 'waiting') return 'gate';
  if (source.status === 'done' && (target.status === 'done' || target.status === 'skipped')) return 'done';
  return 'tail';
}

function elbow(x1: number, y1: number, x2: number, y2: number) {
  if (Math.abs(y1 - y2) < 3) return `M ${x1} ${y1} L ${x2} ${y2}`;
  const mid = (x1 + x2) / 2;
  const dir = y2 > y1 ? 1 : -1;
  const sx = x2 >= x1 ? 1 : -1;
  const radius = Math.max(0, Math.min(10, Math.abs(x2 - x1) / 2 - 1, Math.abs(y2 - y1) / 2 - 1));
  if (radius < 2) return `M ${x1} ${y1} H ${mid} V ${y2} H ${x2}`;
  return `M ${x1} ${y1} H ${mid - radius * sx} Q ${mid} ${y1} ${mid} ${y1 + radius * dir} V ${y2 - radius * dir} Q ${mid} ${y2} ${mid + radius * sx} ${y2} H ${x2}`;
}

function useElementWidth(ref: { current: HTMLElement | null }) {
  const [width, setWidth] = useState(() => (typeof window === 'undefined' ? 940 : window.innerWidth));
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const apply = () => {
      const next = Math.round(el.clientWidth);
      setWidth((prev) => (prev === next ? prev : next));
    };
    apply();
    const observer = new ResizeObserver(apply);
    observer.observe(el);
    return () => observer.disconnect();
  }, [ref]);
  return width;
}

function useMedia(query: string) {
  const [matches, setMatches] = useState(() => typeof window !== 'undefined' && !!window.matchMedia?.(query).matches);
  useEffect(() => {
    const media = window.matchMedia?.(query);
    if (!media) return;
    const sync = () => setMatches(media.matches);
    sync();
    media.addEventListener('change', sync);
    return () => media.removeEventListener('change', sync);
  }, [query]);
  return matches;
}

function trapTab(event: ReactKeyboardEvent, root: HTMLElement | null) {
  if (event.key !== 'Tab' || !root) return;
  const items = Array.from(root.querySelectorAll<HTMLElement>('button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])')).filter((el) => !el.hasAttribute('disabled'));
  if (!items.length) { event.preventDefault(); return; }
  const first = items[0];
  const last = items[items.length - 1];
  if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); }
  else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
}

function Dots({ color }: { color: string }) {
  return <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true">
    <circle cx="4.2" cy="4.2" r="1.7" fill={color} />
    <circle cx="9.8" cy="4.2" r="1.7" fill={color} />
    <circle cx="4.2" cy="9.8" r="1.7" fill={color} />
    <circle cx="9.8" cy="9.8" r="1.7" fill={color} />
  </svg>;
}

export function HandoffSwimlanes({
  lanes = NO_LANES,
  tasks,
  items,
  title = 'Handoffs',
  loading = false,
  labels,
  onSelect,
  onApprove,
  onReject,
  className,
  style,
}: HandoffSwimlanesProps) {
  const text: HandoffSwimlanesLabels = useMemo(() => ({
    ...defaultLabels,
    ...labels,
    status: { ...defaultLabels.status, ...labels?.status },
    kind: { ...defaultLabels.kind, ...labels?.kind },
  }), [labels]);
  const rootRef = useRef<HTMLElement>(null);
  const boardRef = useRef<HTMLDivElement>(null);
  const panelRef = useRef<HTMLDivElement>(null);
  const dialogRef = useRef<HTMLDialogElement>(null);
  const itemRefs = useRef(new Map<string, HTMLButtonElement>());
  const returnFocus = useRef<HTMLElement | null>(null);
  const ignoreClose = useRef(false);
  const titleId = useId();
  const markerId = useId().replace(/:/g, '');
  const rows = tasks ?? items ?? NO_ITEMS;
  const width = useElementWidth(rootRef);
  const viewportSheet = useMedia('(max-width: 899px)');
  const sheet = width < 700 || viewportSheet;
  const depths = useMemo(() => depthMap(rows), [rows]);
  const cols = Math.max(1, ...Array.from(depths.values(), (value) => value + 1), 1);
  const byId = useMemo(() => new Map(rows.map((item) => [item.id, item])), [rows]);
  const lanesById = useMemo(() => new Map(lanes.map((lane) => [lane.id, lane])), [lanes]);
  const [openId, setOpenId] = useState<string | null>(null);
  const [live, setLive] = useState('');
  const [edges, setEdges] = useState<Edge[]>([]);
  const [rails, setRails] = useState<Rail[]>([]);
  const [boardSize, setBoardSize] = useState({ w: 0, h: 0 });
  const [panelPos, setPanelPos] = useState<{ left: number; top: number } | null>(null);
  const work = rows.filter((item) => item.kind !== 'start' && item.kind !== 'end');
  const done = work.filter((item) => item.status === 'done').length;
  const needsPerson = rows.some((item) => item.status === 'waiting');
  const openItem = openId ? byId.get(openId) ?? null : null;

  const dismiss = (restore: boolean) => {
    setOpenId(null);
    onSelect?.(null);
    if (!restore) return;
    const el = returnFocus.current;
    requestAnimationFrame(() => { if (el?.isConnected) el.focus(); });
  };

  const choose = (item: HandoffItem, el: HTMLElement) => {
    if (openId === item.id) { dismiss(true); return; }
    returnFocus.current = el;
    setOpenId(item.id);
    onSelect?.(item.id);
    const lane = lanesById.get(item.laneId);
    setLive(`${item.title}, ${text.kind[item.kind]}, ${text.status[item.status]}${lane ? `, ${lane.name}` : ''}`);
  };

  useLayoutEffect(() => {
    if (loading) return;
    const board = boardRef.current;
    if (!board) return;
    const measure = () => {
      const origin = board.getBoundingClientRect();
      const nextRails: Rail[] = [];
      board.querySelectorAll<HTMLElement>('[data-track]').forEach((track) => {
        const rect = track.getBoundingClientRect();
        const y = rect.top - origin.top + rect.height / 2;
        const x1 = rect.left - origin.left;
        const x2 = rect.right - origin.left - 8;
        nextRails.push({ id: track.dataset.track || y.toFixed(0), d: `M ${x1} ${y} L ${x2} ${y}` });
      });
      const nextEdges: Edge[] = [];
      rows.forEach((item) => {
        const target = itemRefs.current.get(item.id);
        if (!target) return;
        (item.predecessors ?? []).forEach((pred) => {
          const sourceEl = itemRefs.current.get(pred);
          const source = byId.get(pred);
          if (!sourceEl || !source) return;
          const a = sourceEl.getBoundingClientRect();
          const b = target.getBoundingClientRect();
          const x1 = a.right - origin.left - 1;
          const y1 = a.top - origin.top + a.height / 2;
          const x2 = b.left - origin.left + 1;
          const y2 = b.top - origin.top + b.height / 2;
          nextEdges.push({ id: `${pred}-${item.id}`, d: elbow(x1, y1, x2, y2), kind: edgeKind(source, item) });
        });
      });
      const size = { w: Math.round(origin.width), h: Math.round(origin.height) };
      setBoardSize((prev) => (prev.w === size.w && prev.h === size.h ? prev : size));
      const railSig = nextRails.map((rail) => rail.d).join('|');
      const edgeSig = nextEdges.map((edge) => `${edge.kind}:${edge.d}`).join('|');
      setRails((prev) => (prev.map((rail) => rail.d).join('|') === railSig ? prev : nextRails));
      setEdges((prev) => (prev.map((edge) => `${edge.kind}:${edge.d}`).join('|') === edgeSig ? prev : nextEdges));
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(board);
    return () => observer.disconnect();
  }, [rows, lanes, byId, loading, cols, width, openId]);

  useLayoutEffect(() => {
    if (!openItem || sheet) { setPanelPos(null); return; }
    const root = rootRef.current;
    const button = itemRefs.current.get(openItem.id);
    if (!root || !button) return;
    const place = () => {
      const host = root.getBoundingClientRect();
      const rect = button.getBoundingClientRect();
      const panelWidth = Math.min(300, Math.max(180, host.width - 16));
      let left = rect.left - host.left;
      if (left + panelWidth > host.width - 8) left = Math.max(8, host.width - panelWidth - 8);
      setPanelPos({ left, top: rect.bottom - host.top + 8 });
    };
    place();
  }, [openItem, sheet, edges]);

  useEffect(() => {
    if (!openId || sheet) return;
    const onPointer = (event: PointerEvent) => {
      const target = event.target as Node | null;
      if (!target) return;
      if (panelRef.current?.contains(target)) return;
      if ((target as Element).closest?.('[data-item]')) return;
      dismiss(false);
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return;
      event.preventDefault();
      dismiss(true);
    };
    document.addEventListener('pointerdown', onPointer);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('pointerdown', onPointer);
      document.removeEventListener('keydown', onKey);
    };
  }, [openId, sheet]);

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog || !sheet || !openId) return;
    if (!dialog.open) {
      try {
        if (typeof dialog.showModal === 'function') dialog.showModal();
        else dialog.setAttribute('open', '');
      } catch { /* already open */ }
    }
    dialog.querySelector<HTMLElement>('[data-autofocus]')?.focus();
    return () => {
      if (!dialog.open) return;
      ignoreClose.current = true;
      dialog.close();
    };
  }, [sheet, openId]);

  const detail = (item: HandoffItem) => {
    const lane = lanesById.get(item.laneId);
    const previous = (item.predecessors ?? []).map((id) => byId.get(id)?.title).filter((value): value is string => !!value);
    return <>
      <p className="aui-handoff-swimlanes-meta">
        <span>{text.kind[item.kind]}</span>
        <span>{text.status[item.status]}</span>
        {lane && <span>{lane.name}</span>}
        {item.duration && <span>{item.duration}</span>}
      </p>
      {previous.length > 0 && <p className="aui-handoff-swimlanes-after">{text.after(previous.join(', '))}</p>}
      {item.detail && <p className="aui-handoff-swimlanes-copy">{item.detail}</p>}
      {item.kind === 'gate' && <div className="aui-handoff-swimlanes-actions">
        <button type="button" className="aui-handoff-swimlanes-action" data-kind="approve" aria-label={text.approve === 'Approve' ? '승인' : text.approve} onClick={() => onApprove?.(item.id)}>{text.approve}</button>
        <button type="button" className="aui-handoff-swimlanes-action" data-kind="reject" onClick={() => onReject?.(item.id)}>{text.reject}</button>
      </div>}
    </>;
  };

  const rootClass = ['aui-handoff-swimlanes', className].filter(Boolean).join(' ');

  return <section ref={rootRef} className={rootClass} style={style} aria-busy={loading || undefined}>
    <style>{css}</style>
    <header className="aui-handoff-swimlanes-head">
      <h2>{title}</h2>
      {!loading && <div className="aui-handoff-swimlanes-stats">
        <span className="aui-handoff-swimlanes-progress">{text.progress(done, work.length)}</span>
        {needsPerson && <span className="aui-handoff-swimlanes-wait">{text.waiting}</span>}
      </div>}
    </header>
    {loading ? <div className="aui-handoff-swimlanes-loading"><span className="aui-handoff-swimlanes-sr">{text.loading}</span><i /><i /><i /></div> : lanes.length === 0 || rows.length === 0 ? <p className="aui-handoff-swimlanes-empty">{text.empty}</p> : <div className="aui-handoff-swimlanes-board" ref={boardRef} role="table" aria-label={title}>
      <svg className="aui-handoff-swimlanes-wires" viewBox={`0 0 ${Math.max(boardSize.w, 1)} ${Math.max(boardSize.h, 1)}`} aria-hidden="true">
        <defs>
          <marker id={markerId} markerWidth="7" markerHeight="7" refX="6" refY="3.5" orient="auto">
            <path d="M0 0 L7 3.5 L0 7 Z" />
          </marker>
        </defs>
        {rails.map((rail) => <path key={rail.id} d={rail.d} data-rail />)}
        {edges.map((edge) => <path key={edge.id} d={edge.d} data-kind={edge.kind} markerEnd={edge.kind === 'tail' ? `url(#${markerId})` : undefined} />)}
      </svg>
      {lanes.map((lane) => <div key={lane.id} className="aui-handoff-swimlanes-lane" role="row" style={{ '--tone': lane.color } as CSSProperties}>
        <div className="aui-handoff-swimlanes-label" role="rowheader" title={lane.description || lane.role}>
          <span className="aui-handoff-swimlanes-logo" data-kind={lane.kind} aria-hidden="true">
            {lane.kind === 'person' ? <LayoutGrid size={15} strokeWidth={2} /> : <Dots color={lane.color || 'currentColor'} />}
          </span>
          <span className="aui-handoff-swimlanes-label-copy">
            <strong>{lane.name}</strong>
            {lane.role && <small>{lane.role}</small>}
          </span>
        </div>
        <div className="aui-handoff-swimlanes-track" role="cell" data-track={lane.id} data-fit={width < 700 ? 'squeeze' : 'content'} style={{ gridTemplateColumns: `repeat(${cols}, minmax(${width < 700 ? '0, 1fr' : 'max-content, 1fr'}))` }}>
          {Array.from({ length: cols }, (_, depth) => {
            const cell = rows.filter((item) => item.laneId === lane.id && (depths.get(item.id) ?? 0) === depth);
            return <div key={depth} className="aui-handoff-swimlanes-cell">
              {cell.map((item) => {
                const marker = item.kind === 'start' || item.kind === 'end';
                return <button
                  key={item.id}
                  type="button"
                  className={marker ? 'aui-handoff-swimlanes-marker' : 'aui-handoff-swimlanes-pill'}
                  data-item={item.id}
                  data-kind={item.kind}
                  data-status={item.status}
                  data-open={openId === item.id || undefined}
                  title={item.title}
                  aria-label={item.title}
                  aria-expanded={openId === item.id}
                  ref={(el) => { if (el) itemRefs.current.set(item.id, el); else itemRefs.current.delete(item.id); }}
                  onClick={(event) => choose(item, event.currentTarget)}
                >
                  {marker ? <span className="aui-handoff-swimlanes-sr">{item.title}</span> : <>
                    {item.status === 'done' && <Check size={14} aria-hidden="true" />}
                    {item.kind === 'gate' && <Diamond size={13} aria-hidden="true" />}
                    {item.status === 'failed' && <X size={14} aria-hidden="true" />}
                    {item.status === 'active' && <i className="aui-handoff-swimlanes-pip" aria-hidden="true" />}
                    <span className="aui-handoff-swimlanes-label-text">{item.title}</span>
                  </>}
                </button>;
              })}
            </div>;
          })}
        </div>
      </div>)}
    </div>}
    {openItem && !sheet && panelPos && <div ref={panelRef} className="aui-handoff-swimlanes-detail" role="dialog" aria-labelledby={titleId} style={{ left: panelPos.left, top: panelPos.top }} onKeyDown={(event) => { if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); dismiss(true); } }}>
      <div className="aui-handoff-swimlanes-detail-bar">
        <h3 id={titleId}>{openItem.title}</h3>
        <button type="button" className="aui-handoff-swimlanes-close" aria-label={text.close} onClick={() => dismiss(true)}><X size={16} aria-hidden="true" /></button>
      </div>
      {detail(openItem)}
    </div>}
    {openItem && sheet && <dialog
      ref={dialogRef}
      className="aui-handoff-swimlanes-dialog"
      aria-labelledby={titleId}
      onClose={() => { if (ignoreClose.current) { ignoreClose.current = false; return; } dismiss(true); }}
      onClick={(event) => { if (event.target === event.currentTarget) dismiss(true); }}
      onKeyDown={(event) => { if (event.key === 'Escape') { event.preventDefault(); dismiss(true); return; } trapTab(event, dialogRef.current); }}
    >
      <div className="aui-handoff-swimlanes-sheet">
        <div className="aui-handoff-swimlanes-detail-bar">
          <h3 id={titleId}>{openItem.title}</h3>
          <button type="button" className="aui-handoff-swimlanes-close" data-autofocus aria-label={text.close} onClick={() => dismiss(true)}><X size={18} aria-hidden="true" /></button>
        </div>
        {detail(openItem)}
      </div>
    </dialog>}
    <div className="aui-handoff-swimlanes-sr" aria-live="polite">{live}</div>
  </section>;
}

const demoLanes: HandoffLane[] = [
  { id: 'minseo', name: 'Minseo', kind: 'person', role: 'Owner' },
  { id: 'adk-cc', name: 'adk-cc', kind: 'agent', role: 'Sonnet 4.5', color: '#0190fe' },
  { id: 'codex', name: 'Codex', kind: 'agent', role: 'gpt-5.1', color: '#a651f0' },
];

const demoItems: HandoffItem[] = [
  { id: 'start', laneId: 'minseo', title: 'Open 6082', status: 'done', kind: 'start' },
  { id: 'read', laneId: 'adk-cc', title: 'Read the nightly log', status: 'done', kind: 'task', predecessors: ['start'] },
  { id: 'approve', laneId: 'minseo', title: 'Approve the clock fix', status: 'waiting', kind: 'gate', predecessors: ['read'], detail: 'Needs a person' },
  { id: 'edit', laneId: 'codex', title: 'Edit the test helper', status: 'active', kind: 'task', predecessors: ['approve'], duration: '24m' },
  { id: 'land', laneId: 'codex', title: 'Land on C10e', status: 'queued', kind: 'end', predecessors: ['edit'] },
];

const longLanes: HandoffLane[] = [
  { id: 'minseo', name: '민서', kind: 'person', role: '소유자' },
  { id: 'reader', name: '야간 기록 도우미', kind: 'agent', role: 'Sonnet 4.5', color: '#0190fe' },
  { id: 'codex', name: '코덱스', kind: 'agent', role: 'gpt-5.1', color: '#a651f0' },
];

const longItems: HandoffItem[] = [
  { id: 'start', laneId: 'minseo', title: '이슈 6082를 열고 야간 작업을 시작한다', status: 'done', kind: 'start' },
  { id: 'read', laneId: 'reader', title: '어젯밤 기록을 읽고 기한 테스트가 깨진 까닭을 표시한다', status: 'done', kind: 'task', predecessors: ['start'] },
  { id: 'approve', laneId: 'minseo', title: '시계 수정안을 사람이 확인한 뒤에만 화물 검사를 다시 돌린다', status: 'waiting', kind: 'gate', predecessors: ['read'], detail: '사람이 필요합니다. WSL에서 벽시계를 쓰던 도우미를 고정하기 전에 확인을 받아야 합니다.' },
  { id: 'edit', laneId: 'codex', title: '테스트 도우미를 고치면서 패치를 작게 유지하고 있다', status: 'active', kind: 'task', predecessors: ['approve'], duration: '24분' },
  { id: 'land', laneId: 'codex', title: 'C10e 갈래에 올리고 밤 검사를 마친다', status: 'queued', kind: 'end', predecessors: ['edit'] },
];

const longLabels: Partial<HandoffSwimlanesLabels> = {
  progress: (done, total) => `${total} 중 ${done}`,
  waiting: '사람 확인을 기다리는 중',
  approve: '승인',
  reject: '거절',
  close: '닫기',
  empty: '아직 인수인계가 없습니다.',
  loading: '인수인계를 불러오는 중',
  after: (titles) => `${titles} 다음`,
  status: { queued: '줄 대기', active: '진행', waiting: '대기', done: '끝', failed: '실패', skipped: '건너뜀' },
  kind: { task: '일', gate: '문', start: '시작', end: '끝' },
};

export function HandoffSwimlanesDemo({ state = 'default' }: { state?: string }) {
  if (state === 'loading') return <HandoffSwimlanes loading title="Crate split handoffs" />;
  if (state === 'empty') return <HandoffSwimlanes title="Crate split handoffs" lanes={[]} items={[]} />;
  if (state === 'long') return <HandoffSwimlanes title="화물 나누기 인수인계" lanes={longLanes} items={longItems} labels={longLabels} />;
  return <HandoffSwimlanes title="Crate split handoffs" lanes={demoLanes} items={demoItems} />;
}

const css = `
.aui-handoff-swimlanes, .aui-handoff-swimlanes-dialog {
  --aui-handoff-swimlanes-card: var(--aui-surface, #ffffff);
  --aui-handoff-swimlanes-strong: var(--aui-fg, #171717);
  --aui-handoff-swimlanes-ink: var(--aui-ink, #171717);
  --aui-handoff-swimlanes-text: var(--aui-fg-dim, #6b6b6b);
  --aui-handoff-swimlanes-muted: var(--aui-fg-muted, #8a8a8a);
  --aui-handoff-swimlanes-faint: var(--aui-fg-faint, rgba(0, 0, 0, 0.27));
  --aui-handoff-swimlanes-line: var(--aui-line, #ececec);
  --aui-handoff-swimlanes-track: var(--aui-subtle, #f3f3f3);
  --aui-handoff-swimlanes-stroke: rgba(0, 0, 0, 0.3);
  --aui-handoff-swimlanes-soft: rgba(0, 0, 0, 0.16);
  --aui-handoff-swimlanes-info: var(--aui-info, rgba(0, 96, 226, 0.78));
  --aui-handoff-swimlanes-ok: #007d2ee0;
  --aui-handoff-swimlanes-ok-face: #eef6ff;
  --aui-handoff-swimlanes-warn: var(--aui-warning, #e8a200);
  --aui-handoff-swimlanes-warn-text: #b25e00;
  --aui-handoff-swimlanes-warn-face: #fff1d6;
  --aui-handoff-swimlanes-danger: var(--aui-danger, #e5484d);
  --aui-handoff-swimlanes-focus: var(--aui-focus, rgba(0, 96, 226, 0.7));
  --aui-handoff-swimlanes-shadow: 0 0 0 1px rgba(0, 0, 0, 0.06), 0 16px 32px -18px rgba(0, 0, 0, 0.2);
  --aui-handoff-swimlanes-pill: 0 0 0 1px rgba(0, 0, 0, 0.06), 0 1px 2px rgba(0, 0, 0, 0.04);
  --aui-handoff-swimlanes-float: 0 0 0 1px rgba(0, 0, 0, 0.06), 0 16px 32px -12px rgba(0, 0, 0, 0.2);
  --aui-handoff-swimlanes-ease: cubic-bezier(0.22, 1, 0.36, 1);
  color: var(--aui-handoff-swimlanes-ink);
  font-family: Inter, "Noto Sans KR", "Apple SD Gothic Neo", "Malgun Gothic", ui-sans-serif, system-ui, sans-serif;
  font-size: 13px;
  line-height: 20px;
  letter-spacing: -0.15px;
}
:where(.dark, [data-theme="dark"]) :is(.aui-handoff-swimlanes, .aui-handoff-swimlanes-dialog) {
  --aui-handoff-swimlanes-card: var(--aui-surface, #232326);
  --aui-handoff-swimlanes-strong: var(--aui-fg, #ededed);
  --aui-handoff-swimlanes-ink: var(--aui-ink, #ededed);
  --aui-handoff-swimlanes-text: var(--aui-fg-dim, #a1a1a6);
  --aui-handoff-swimlanes-muted: var(--aui-fg-muted, #7c7c82);
  --aui-handoff-swimlanes-faint: var(--aui-fg-faint, rgba(255, 255, 255, 0.28));
  --aui-handoff-swimlanes-line: var(--aui-line, #2a2a2e);
  --aui-handoff-swimlanes-track: var(--aui-subtle, #2f2f33);
  --aui-handoff-swimlanes-stroke: rgba(255, 255, 255, 0.32);
  --aui-handoff-swimlanes-soft: rgba(255, 255, 255, 0.16);
  --aui-handoff-swimlanes-info: var(--aui-info, rgba(77, 141, 255, 0.85));
  --aui-handoff-swimlanes-ok: #6fd398;
  --aui-handoff-swimlanes-ok-face: rgba(70, 200, 120, 0.14);
  --aui-handoff-swimlanes-warn: var(--aui-warning, #e6c24d);
  --aui-handoff-swimlanes-warn-text: #ffb35c;
  --aui-handoff-swimlanes-warn-face: rgba(255, 170, 60, 0.16);
  --aui-handoff-swimlanes-danger: var(--aui-danger, #ff6369);
  --aui-handoff-swimlanes-focus: var(--aui-focus, rgba(77, 141, 255, 0.8));
  --aui-handoff-swimlanes-shadow: 0 0 0 1px rgba(255, 255, 255, 0.08), 0 16px 32px -12px rgba(0, 0, 0, 0.6);
  --aui-handoff-swimlanes-pill: 0 0 0 1px rgba(255, 255, 255, 0.08), 0 1px 2px rgba(0, 0, 0, 0.3);
  --aui-handoff-swimlanes-float: 0 0 0 1px rgba(255, 255, 255, 0.1), 0 16px 32px -12px rgba(0, 0, 0, 0.6);
}
.aui-handoff-swimlanes {
  container: aui-handoff-swimlanes / inline-size;
  position: relative;
  box-sizing: border-box;
  width: min(100%, 940px);
  margin-inline: auto;
  min-width: 0;
  padding: 16px 18px 14px;
  border-radius: 16px;
  background: var(--aui-handoff-swimlanes-card);
  box-shadow: var(--aui-handoff-swimlanes-shadow);
  word-break: keep-all;
  overflow-wrap: anywhere;
  -webkit-tap-highlight-color: transparent;
}
.aui-handoff-swimlanes *, .aui-handoff-swimlanes *::before, .aui-handoff-swimlanes *::after, .aui-handoff-swimlanes-dialog *, .aui-handoff-swimlanes-dialog *::before, .aui-handoff-swimlanes-dialog *::after { box-sizing: border-box; }
.aui-handoff-swimlanes button, .aui-handoff-swimlanes-dialog button { font: inherit; letter-spacing: inherit; cursor: pointer; touch-action: manipulation; }
.aui-handoff-swimlanes button:focus-visible, .aui-handoff-swimlanes-dialog button:focus-visible { outline: 2px solid var(--aui-handoff-swimlanes-focus); outline-offset: 2px; }
.aui-handoff-swimlanes-head { display: flex; flex-wrap: wrap; align-items: center; gap: 8px 12px; min-width: 0; }
.aui-handoff-swimlanes-head h2 { margin: 0; min-width: 0; flex: 1 1 auto; color: var(--aui-handoff-swimlanes-strong); font-size: 14px; line-height: 20px; font-weight: 650; }
.aui-handoff-swimlanes-stats { display: flex; align-items: center; gap: 8px; margin-left: auto; }
.aui-handoff-swimlanes-progress { color: var(--aui-handoff-swimlanes-text); font-variant-numeric: tabular-nums; white-space: nowrap; }
.aui-handoff-swimlanes-wait { display: inline-flex; align-items: center; height: 22px; padding: 0 8px; border-radius: 999px; background: var(--aui-handoff-swimlanes-warn-face); color: var(--aui-handoff-swimlanes-warn-text); font-size: 12px; font-weight: 650; white-space: nowrap; }
.aui-handoff-swimlanes-board { position: relative; margin-top: 8px; min-width: 0; }
.aui-handoff-swimlanes-wires { position: absolute; inset: 0; z-index: 0; width: 100%; height: 100%; overflow: visible; pointer-events: none; }
.aui-handoff-swimlanes-wires path[data-rail] { fill: none; stroke: var(--aui-handoff-swimlanes-line); stroke-width: 1; }
.aui-handoff-swimlanes-wires path[data-kind="done"] { fill: none; stroke: var(--aui-handoff-swimlanes-stroke); stroke-width: 1.5; }
.aui-handoff-swimlanes-wires path[data-kind="gate"] { fill: none; stroke: var(--aui-handoff-swimlanes-warn); stroke-width: 1.75; }
.aui-handoff-swimlanes-wires path[data-kind="tail"] { fill: none; stroke: var(--aui-handoff-swimlanes-soft); stroke-width: 1.5; stroke-dasharray: 4 4; }
.aui-handoff-swimlanes-wires path[data-kind="bad"] { fill: none; stroke: var(--aui-handoff-swimlanes-danger); stroke-width: 1.5; }
.aui-handoff-swimlanes-wires marker path { fill: var(--aui-handoff-swimlanes-soft); }
.aui-handoff-swimlanes-lane { position: relative; z-index: 1; display: grid; grid-template-columns: 120px minmax(0, 1fr); align-items: center; min-height: 76px; min-width: 0; }
.aui-handoff-swimlanes-label { display: flex; align-items: center; gap: 8px; min-width: 0; padding-right: 8px; background: var(--aui-handoff-swimlanes-card); }
.aui-handoff-swimlanes-logo { display: grid; place-items: center; width: 28px; height: 28px; flex: none; border-radius: 8px; background: color-mix(in srgb, var(--tone, var(--aui-handoff-swimlanes-track)) 16%, var(--aui-handoff-swimlanes-card)); color: var(--tone, var(--aui-handoff-swimlanes-ink)); }
.aui-handoff-swimlanes-logo[data-kind="person"] { background: var(--aui-handoff-swimlanes-track); color: var(--aui-handoff-swimlanes-ink); }
.aui-handoff-swimlanes-label-copy { display: flex; flex-direction: column; min-width: 0; }
.aui-handoff-swimlanes-label-copy strong { color: var(--aui-handoff-swimlanes-strong); font-size: 13px; line-height: 16px; font-weight: 650; }
.aui-handoff-swimlanes-label-copy small { color: var(--aui-handoff-swimlanes-muted); font-size: 12px; line-height: 16px; }
.aui-handoff-swimlanes-track { display: grid; align-items: center; min-width: 0; min-height: 72px; }
.aui-handoff-swimlanes-cell { display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 6px; min-width: 0; padding: 8px 4px; }
.aui-handoff-swimlanes-pill { display: inline-flex; align-items: center; gap: 6px; max-width: 100%; min-height: 30px; padding: 4px 10px 4px 8px; border: 0; border-radius: 999px; background: var(--aui-handoff-swimlanes-card); box-shadow: var(--aui-handoff-swimlanes-pill); color: var(--aui-handoff-swimlanes-strong); text-align: left; }
.aui-handoff-swimlanes-pill[data-status="done"] { background: var(--aui-handoff-swimlanes-ok-face); }
.aui-handoff-swimlanes-pill[data-status="done"] svg { color: var(--aui-handoff-swimlanes-ok); flex: none; }
.aui-handoff-swimlanes-pill[data-kind="gate"] { background: var(--aui-handoff-swimlanes-warn-face); color: var(--aui-handoff-swimlanes-warn-text); box-shadow: none; }
.aui-handoff-swimlanes-pill[data-kind="gate"] svg { flex: none; }
.aui-handoff-swimlanes-pill[data-status="active"] { background: color-mix(in srgb, var(--tone, #a651f0) 14%, var(--aui-handoff-swimlanes-card)); }
.aui-handoff-swimlanes-pill[data-status="failed"] { background: color-mix(in srgb, var(--aui-handoff-swimlanes-danger) 12%, var(--aui-handoff-swimlanes-card)); color: var(--aui-handoff-swimlanes-danger); }
.aui-handoff-swimlanes-pill[data-status="skipped"] { color: var(--aui-handoff-swimlanes-muted); }
.aui-handoff-swimlanes-pill[data-status="skipped"] .aui-handoff-swimlanes-label-text { text-decoration: line-through; }
.aui-handoff-swimlanes-pill[data-open], .aui-handoff-swimlanes-marker[data-open] { box-shadow: 0 0 0 2px var(--aui-handoff-swimlanes-focus); }
.aui-handoff-swimlanes-label-text { min-width: 0; max-width: 12rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.aui-handoff-swimlanes-pill[data-kind="gate"] .aui-handoff-swimlanes-label-text { max-width: 6.6rem; }
.aui-handoff-swimlanes-pip { width: 7px; height: 7px; flex: none; border-radius: 50%; background: var(--tone, #a651f0); }
.aui-handoff-swimlanes-marker { position: relative; width: 16px; height: 16px; padding: 0; border: 1.5px solid var(--aui-handoff-swimlanes-stroke); border-radius: 50%; background: var(--aui-handoff-swimlanes-card); }
.aui-handoff-swimlanes-marker[data-status="queued"] { border-style: solid; }
.aui-handoff-swimlanes-detail { position: absolute; z-index: 5; width: min(300px, calc(100% - 16px)); padding: 12px; border-radius: 12px; background: var(--aui-handoff-swimlanes-card); box-shadow: var(--aui-handoff-swimlanes-float); }
.aui-handoff-swimlanes-detail-bar { display: flex; align-items: flex-start; gap: 8px; }
.aui-handoff-swimlanes-detail-bar h3 { flex: 1; min-width: 0; margin: 0; color: var(--aui-handoff-swimlanes-strong); font-size: 14px; line-height: 20px; font-weight: 650; }
.aui-handoff-swimlanes-close { display: inline-grid; place-items: center; width: 28px; height: 28px; flex: none; border: 0; border-radius: 8px; background: var(--aui-handoff-swimlanes-track); color: var(--aui-handoff-swimlanes-strong); }
.aui-handoff-swimlanes-meta, .aui-handoff-swimlanes-after, .aui-handoff-swimlanes-copy { margin: 8px 0 0; }
.aui-handoff-swimlanes-meta { display: flex; flex-wrap: wrap; gap: 4px 8px; color: var(--aui-handoff-swimlanes-text); font-size: 12px; }
.aui-handoff-swimlanes-meta span + span::before { content: "·"; margin-right: 8px; color: var(--aui-handoff-swimlanes-faint); }
.aui-handoff-swimlanes-after, .aui-handoff-swimlanes-copy { color: var(--aui-handoff-swimlanes-text); }
.aui-handoff-swimlanes-actions { display: flex; flex-wrap: wrap; gap: 8px; margin-top: 12px; }
.aui-handoff-swimlanes-action { height: 32px; padding: 0 12px; border: 0; border-radius: 8px; background: var(--aui-handoff-swimlanes-track); color: var(--aui-handoff-swimlanes-strong); font-weight: 650; }
.aui-handoff-swimlanes-action[data-kind="approve"] { background: #171717; color: #fff; }
.aui-handoff-swimlanes-action[data-kind="reject"] { background: var(--aui-handoff-swimlanes-track); }
.aui-handoff-swimlanes-empty { margin: 28px 0 8px; color: var(--aui-handoff-swimlanes-muted); text-align: center; }
.aui-handoff-swimlanes-loading { display: grid; gap: 10px; margin-top: 16px; }
.aui-handoff-swimlanes-loading i { display: block; height: 48px; border-radius: 10px; background: linear-gradient(90deg, var(--aui-handoff-swimlanes-track), color-mix(in srgb, var(--aui-handoff-swimlanes-card) 40%, var(--aui-handoff-swimlanes-track)), var(--aui-handoff-swimlanes-track)); background-size: 200% 100%; animation: aui-handoff-swimlanes-shimmer 1.2s linear infinite; }
.aui-handoff-swimlanes-dialog { position: fixed; inset: 0; width: 100%; height: 100%; max-width: none; max-height: none; margin: 0; padding: 0; border: 0; background: transparent; }
.aui-handoff-swimlanes-dialog::backdrop { background: rgba(0, 0, 0, 0.36); }
.aui-handoff-swimlanes-sheet { position: absolute; right: 0; bottom: 0; left: 0; max-height: min(80dvh, 100%); overflow: auto; padding: 14px 16px max(16px, env(safe-area-inset-bottom)); border-radius: 16px 16px 0 0; background: var(--aui-handoff-swimlanes-card); box-shadow: var(--aui-handoff-swimlanes-float); color: var(--aui-handoff-swimlanes-ink); }
.aui-handoff-swimlanes-sr { position: absolute; width: 1px; height: 1px; margin: -1px; padding: 0; overflow: hidden; clip: rect(0 0 0 0); white-space: nowrap; border: 0; }
@media (pointer: coarse), (max-width: 1399px) {
  .aui-handoff-swimlanes-pill, .aui-handoff-swimlanes-action, .aui-handoff-swimlanes-close { min-height: 44px; min-width: 44px; }
  .aui-handoff-swimlanes-close { width: 44px; }
  .aui-handoff-swimlanes-marker { width: 44px; height: 44px; min-width: 44px; min-height: 44px; }
}
@media (prefers-reduced-motion: no-preference) {
  .aui-handoff-swimlanes-wires path[data-kind] { animation: aui-handoff-swimlanes-in 480ms var(--aui-handoff-swimlanes-ease) both; }
}
@media (prefers-reduced-motion: reduce) {
  .aui-handoff-swimlanes, .aui-handoff-swimlanes *, .aui-handoff-swimlanes-dialog, .aui-handoff-swimlanes-dialog * { animation: none !important; transition: none !important; }
}
@keyframes aui-handoff-swimlanes-in { from { opacity: 0; } to { opacity: 1; } }
@keyframes aui-handoff-swimlanes-shimmer { from { background-position: 100% 0; } to { background-position: -100% 0; } }
:where(.dark, [data-theme="dark"]) .aui-handoff-swimlanes-action[data-kind="approve"] { background: #f4f4f5; color: #171717; }
@container aui-handoff-swimlanes (max-width: 699px) {
  .aui-handoff-swimlanes-lane { grid-template-columns: 40px minmax(0, 1fr); min-height: 84px; }
  .aui-handoff-swimlanes-label-copy { position: absolute; width: 1px; height: 1px; margin: -1px; padding: 0; overflow: hidden; clip: rect(0 0 0 0); }
  .aui-handoff-swimlanes-cell { padding: 6px 2px; }
  .aui-handoff-swimlanes-pill { position: relative; width: 100%; min-height: 44px; padding: 16px 4px 6px; border-radius: 10px; }
  .aui-handoff-swimlanes-pill svg, .aui-handoff-swimlanes-pip { position: absolute; top: 4px; left: 50%; transform: translateX(-50%); }
  .aui-handoff-swimlanes-label-text, .aui-handoff-swimlanes-pill[data-kind="gate"] .aui-handoff-swimlanes-label-text { max-width: none; width: 100%; overflow: hidden; white-space: normal; overflow-wrap: break-word; word-break: keep-all; text-align: center; font-size: 11px; line-height: 13px; display: -webkit-box; -webkit-box-orient: vertical; -webkit-line-clamp: 4; }
  .aui-handoff-swimlanes-head { align-items: flex-start; }
}
`;
