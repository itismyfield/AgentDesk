// Kit provenance: itismyfield/agentdesk-ui-kit@b6d2ab1
import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState, type CSSProperties, type KeyboardEvent as ReactKeyboardEvent, type PointerEvent as ReactPointerEvent, type ReactNode } from 'react';
import { createPortal } from 'react-dom';
import { CircleCheck, CircleDot, Files, GitPullRequest, Hand, Link, MessageSquare, Sparkles, X } from 'lucide-react';

// Clean-room implementation from clean-specs/scheduled-runs/spec.md.
// No affectUI Pro source, registry, package, site assets, or original copy is used.

export type TaskStatKind = 'files' | 'pr' | 'tests' | 'messages' | 'issues' | 'links';
export interface TaskStat { kind: TaskStatKind; value: number | string; label?: string }
export interface ScheduledTask { id: string; title: string; usage: number; agent?: string; description?: string; stats?: TaskStat[]; /** Brand mark slot; a neutral letter replaces vendor art. */ mark?: string; reserved?: { day: number; session: number } }
export interface ScheduleDay { label: string; date?: string | number }
export interface ScheduleAgent { name: string; mark?: string }
export interface SchedulePiece { taskId: string; day: number; session: number; amount: number; part: number; parts: number }
export interface ScheduledRunsProps {
  tasks: ScheduledTask[];
  days?: (string | ScheduleDay)[];
  sessions?: string[];
  current?: { day: number; session: number };
  limit?: number;
  minSplit?: number;
  agents?: ScheduleAgent[];
  defaultMode?: 'auto' | 'manual';
  /** false, true (800ms) or a delay in milliseconds. */
  autoStart?: boolean | number;
  loop?: boolean;
  title?: string;
  subtitle?: string;
  onAutoComplete?: (pieces: SchedulePiece[]) => void;
  onPlace?: (task: ScheduledTask, pieces: SchedulePiece[]) => void;
  onMove?: (piece: SchedulePiece, target: { day: number; session: number }) => void;
  className?: string;
  style?: CSSProperties;
}

interface Piece { taskId: string; pos: number; amount: number; reserved?: boolean; manual?: boolean }
type Source = { kind: 'queue'; taskId: string } | { kind: 'piece'; taskId: string; pos: number };
interface Plan { ok: boolean; cells: { pos: number; amount: number }[]; reason?: string }
const EPS = 1e-9;
const statIcon: Record<TaskStatKind, typeof Files> = { files: Files, pr: GitPullRequest, tests: CircleCheck, messages: MessageSquare, issues: CircleDot, links: Link };
const statName: Record<TaskStatKind, string> = { files: '파일', pr: '변경 요청', tests: '테스트', messages: '메시지', issues: '이슈', links: '링크' };
const fin = (value: unknown): value is number => typeof value === 'number' && Number.isFinite(value);
export const effectiveUsage = (task: ScheduledTask) => fin(task.usage) ? Math.max(1, task.usage) : 0;
const compact = new Intl.NumberFormat('en-US', { notation: 'compact', maximumFractionDigits: 1 });
export const formatStat = (value: number | string) => typeof value === 'number' && Math.abs(value) >= 10000 ? compact.format(value) : String(value);
const letter = (text: string) => (text.replace(/\s+/g, '')[0] ?? '').toUpperCase();

function cellOf(pieces: Piece[], pos: number) { let count = 0; let used = 0; for (const piece of pieces) if (piece.pos === pos) { count += 1; used += piece.amount; } return { count, used }; }
/** Ordered reason for a first target cell: past → card count → under 1% → remaining share. */
export function cellReason(pieces: Piece[], pos: number, amount: number, current: number, limit: number, minSplit: number) {
  if (pos < current) return '지난 세션';
  const { count, used } = cellOf(pieces, pos);
  const free = limit - used;
  if (count >= 2) return '카드 2개로 꽉 참';
  if (free < limit * 0.01) return '남은 용량 1% 미만';
  if (free + EPS < Math.min(minSplit, amount)) return `남은 용량 ${Math.round((free / limit) * 100)}%`;
  return null;
}
function fill(pieces: Piece[], order: number[], amount: number, limit: number, minSplit: number) {
  const out: { pos: number; amount: number }[] = [];
  let rest = amount;
  for (const pos of order) {
    const { count, used } = cellOf(pieces, pos);
    const free = limit - used;
    if (count < 2 && free > EPS && free + EPS >= Math.min(minSplit, rest)) { const take = Math.min(free, rest); out.push({ pos, amount: take }); rest -= take; }
    if (rest <= EPS) break;
  }
  return { out, rest };
}
export function planAutoPlacement(pieces: Piece[], amount: number, current: number, total: number, limit: number, minSplit: number): Plan {
  if (amount <= 0) return { ok: false, cells: [], reason: '사용량 값이 없어 배치할 수 없음' };
  if (amount <= limit + EPS) {
    for (let pos = current; pos < total; pos += 1) { const { count, used } = cellOf(pieces, pos); if (count < 2 && limit - used + EPS >= amount) return { ok: true, cells: [{ pos, amount }] }; }
    return { ok: false, cells: [], reason: '통째로 들어갈 세션이 없음' };
  }
  const { out, rest } = fill(pieces, Array.from({ length: Math.max(0, total - current) }, (_, i) => current + i), amount, limit, minSplit);
  return rest <= EPS ? { ok: true, cells: out } : { ok: false, cells: [], reason: '나눠도 남은 용량이 부족함' };
}
export function planManualPlacement(pieces: Piece[], amount: number, target: number, current: number, total: number, limit: number, minSplit: number): Plan {
  if (amount <= 0) return { ok: false, cells: [], reason: '사용량 값이 없어 배치할 수 없음' };
  const first = cellReason(pieces, target, amount, current, limit, minSplit);
  if (first) return { ok: false, cells: [], reason: first };
  const order = [...Array.from({ length: total - target }, (_, i) => target + i), ...Array.from({ length: Math.max(0, target - current) }, (_, i) => current + i)];
  const { out, rest } = fill(pieces, order, amount, limit, minSplit);
  return rest <= EPS ? { ok: true, cells: out } : { ok: false, cells: [], reason: '전체를 담을 용량이 부족함' };
}
function initialPieces(tasks: ScheduledTask[], days: number, sessions: number, limit: number, minSplit: number) {
  const pieces: Piece[] = [];
  const total = days * sessions;
  for (const task of tasks) {
    const r = task.reserved;
    if (!r || r.day < 0 || r.day >= days || r.session < 0 || r.session >= sessions) continue;
    const pos = r.day * sessions + r.session;
    const amount = effectiveUsage(task);
    if (amount <= 0) continue;
    const free = limit - cellOf(pieces, pos).used;
    const head = amount <= limit ? amount : Math.min(amount, Math.max(free, minSplit));
    pieces.push({ taskId: task.id, pos, amount: head, reserved: true });
    let rest = amount - head;
    if (rest > EPS) {
      // Trailing pieces follow the reservation in time order; the current position never pushes an existing reservation forward.
      const order = Array.from({ length: total - pos - 1 }, (_, i) => pos + 1 + i);
      for (const cell of fill(pieces, order, rest, limit, minSplit).out) { pieces.push({ taskId: task.id, pos: cell.pos, amount: cell.amount, reserved: true }); rest -= cell.amount; }
    }
  }
  return pieces;
}

function useMedia(query: string) {
  const [matches, setMatches] = useState(() => typeof window !== 'undefined' && window.matchMedia?.(query).matches === true);
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
const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));
const frame = () => new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));

function Mark({ src, text, className }: { src?: string; text: string; className: string }) {
  const [failed, setFailed] = useState(false);
  return <span className={className} aria-hidden="true">{src && !failed ? <img src={src} alt="" onError={() => setFailed(true)} /> : letter(text)}</span>;
}

function Sheet({ title, onClose, children, returnTo }: { title: string; onClose: () => void; children: ReactNode; returnTo: HTMLElement | null }) {
  const panel = useRef<HTMLDivElement>(null);
  const titleId = useId();
  useEffect(() => {
    panel.current?.querySelector<HTMLElement>('.aui-scheduled-runs-sheet-close')?.focus({ preventScroll: true });
    return () => { const back = returnTo?.isConnected ? returnTo : null; back?.focus({ preventScroll: true }); };
  }, [returnTo]);
  const keys = (event: ReactKeyboardEvent) => {
    if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); onClose(); return; }
    if (event.key !== 'Tab' || !panel.current) return;
    const items = Array.from(panel.current.querySelectorAll<HTMLElement>('button:not([disabled]),[tabindex="0"]'));
    const index = items.indexOf(document.activeElement as HTMLElement);
    event.preventDefault();
    items[(index + (event.shiftKey ? -1 : 1) + items.length) % items.length]?.focus();
  };
  if (typeof document === 'undefined') return null;
  return createPortal(<div className="aui-scheduled-runs-layer" onPointerDown={(event) => { if (event.target === event.currentTarget) onClose(); }}>
    <style>{styles}</style>
    <div ref={panel} className="aui-scheduled-runs-sheet" role="dialog" aria-modal="true" aria-labelledby={titleId} onKeyDown={keys}>
      <div className="aui-scheduled-runs-sheet-head"><h2 id={titleId}>{title}</h2><button type="button" className="aui-scheduled-runs-sheet-close" aria-label="닫기" onClick={onClose}><X size={18} aria-hidden="true" /></button></div>
      <div className="aui-scheduled-runs-sheet-body">{children}</div>
    </div>
  </div>, document.body);
}

export function ScheduledRuns({ tasks, days: dayInput = ['월', '화', '수'], sessions = ['09:00', '14:00', '20:00'], current: currentInput = { day: 0, session: 0 }, limit = 100, minSplit = 15, agents: agentInput, defaultMode = 'auto', autoStart = false, loop = false, title = '주간 실행 배치', subtitle, onAutoComplete, onPlace, onMove, className = '', style }: ScheduledRunsProps) {
  const id = useId();
  // Narrow viewports and touch tablets use the sheet so the tap-to-move path is always reachable.
  const sheetMode = useMedia('(max-width: 899px), (pointer: coarse)');
  const days: ScheduleDay[] = dayInput.map((day) => typeof day === 'string' ? { label: day } : day);
  const D = days.length;
  const S = sessions.length;
  const total = D * S;
  const current = Math.max(0, Math.min(total, currentInput.day * S + currentInput.session));
  const signature = JSON.stringify([tasks.map((task) => [task.id, task.usage, task.reserved]), D, S, limit, minSplit]);
  const make = () => initialPieces(tasks, D, S, limit, minSplit);
  const [pieces, setPiecesState] = useState<Piece[]>(make);
  const piecesRef = useRef(pieces);
  const setPieces = useCallback((next: Piece[]) => { piecesRef.current = next; setPiecesState(next); }, []);
  const [mode, setMode] = useState(defaultMode);
  const [running, setRunning] = useState(false);
  const [failed, setFailed] = useState<Record<string, string>>({});
  const [flying, setFlying] = useState<string | null>(null);
  const [flight, setFlight] = useState<{ task: ScheduledTask; from: DOMRect; to: DOMRect } | null>(null);
  const [leaving, setLeaving] = useState<string[]>([]);
  const [selectedAgent, setSelectedAgent] = useState<string | null>(null);
  const [detail, setDetail] = useState<string | null>(null);
  const [detailPos, setDetailPos] = useState<{ left: number; top: number } | null>(null);
  type Drag = { source: Source; x: number; y: number; dx: number; dy: number; w: number; h: number; target: number | null };
  const [drag, setDragState] = useState<Drag | null>(null);
  const dragRef = useRef<Drag | null>(null);
  const setDrag = (next: Drag | null) => { dragRef.current = next; setDragState(next); };
  // A failed or cancelled pointer drag flies the copy back to the card it left (260ms E); reduced motion snaps back.
  type Return = { source: Source; left: number; top: number; w: number; h: number; to: { left: number; top: number; width: number; height: number }; phase: 'fly' | 'land' };
  const [returning, setReturning] = useState<Return | null>(null);
  const returnRef = useRef<HTMLDivElement>(null);
  const [pick, setPick] = useState<{ source: Source; target: number } | null>(null);
  const [tapTarget, setTapTarget] = useState<number | null>(null);
  const [announce, setAnnounce] = useState('');
  const [pendingFocus, setPendingFocus] = useState<string | null>(null);
  const root = useRef<HTMLDivElement>(null);
  const boardRef = useRef<HTMLDivElement>(null);
  const cloneRef = useRef<HTMLDivElement>(null);
  const token = useRef(0);
  const paused = useRef(false);
  const stopAfterLanding = useRef(false);
  const interacted = useRef(false);
  const everPlaced = useRef(false);
  const pointerInside = useRef(false);
  const flightDone = useRef<(() => void) | null>(null);
  const press = useRef<{ source: Source; x: number; y: number; pointerId: number; el: HTMLElement; started: boolean; type: string } | null>(null);
  const invoker = useRef<HTMLElement | null>(null);
  const taskMap = new Map(tasks.map((task) => [task.id, task]));
  const posLabel = (pos: number) => `${days[Math.floor(pos / S)]?.label ?? ''} ${sessions[pos % S] ?? ''}`.trim();
  const toPublic = (list: Piece[]): SchedulePiece[] => {
    const byTask = new Map<string, Piece[]>();
    list.forEach((piece) => byTask.set(piece.taskId, [...(byTask.get(piece.taskId) ?? []), piece]));
    return list.map((piece) => { const own = byTask.get(piece.taskId)!.slice().sort((a, b) => a.pos - b.pos); return { taskId: piece.taskId, day: Math.floor(piece.pos / S), session: piece.pos % S, amount: piece.amount, part: own.indexOf(piece) + 1, parts: own.length }; });
  };
  const outOfRange = (task: ScheduledTask) => !!task.reserved && !(task.reserved.day >= 0 && task.reserved.day < D && task.reserved.session >= 0 && task.reserved.session < S);
  const placedIds = new Set(pieces.map((piece) => piece.taskId));
  const queue = tasks.filter((task) => !outOfRange(task) && (!placedIds.has(task.id) || task.id === flying || leaving.includes(task.id)));
  const remaining = tasks.filter((task) => !outOfRange(task) && !placedIds.has(task.id)).length;
  const agents: ScheduleAgent[] = agentInput ?? [...new Set(tasks.map((task) => task.agent).filter(Boolean) as string[])].map((name) => ({ name }));

  // Inputs that change capacity or the task list restore the reserved-only layout.
  const firstSignature = useRef(signature);
  useEffect(() => {
    if (firstSignature.current === signature) return;
    firstSignature.current = signature;
    token.current += 1;
    setPieces(initialPieces(tasks, D, S, limit, minSplit)); setRunning(false); setFlying(null); setFlight(null); setFailed({}); setLeaving([]); setDrag(null); setPick(null); setDetail(null);
    everPlaced.current = false; interacted.current = false;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [signature]);

  const runAuto = useCallback(async () => {
    const mine = ++token.current;
    setRunning(true);
    stopAfterLanding.current = false;
    const tried = new Set<string>();
    const failures: Record<string, string> = {};
    let placed = 0;
    for (;;) {
      while (paused.current) { await sleep(50); if (mine !== token.current) return; }
      if (mine !== token.current) return;
      const onBoard = new Set(piecesRef.current.map((piece) => piece.taskId));
      const next = tasks.find((task) => !outOfRange(task) && !onBoard.has(task.id) && !tried.has(task.id));
      if (!next) break;
      tried.add(next.id);
      const plan = planAutoPlacement(piecesRef.current, effectiveUsage(next), current, total, limit, minSplit);
      if (!plan.ok) { failures[next.id] = plan.reason ?? '용량 부족'; setFailed((prev) => ({ ...prev, [next.id]: failures[next.id] })); continue; }
      setFailed((prev) => { const copy = { ...prev }; delete copy[next.id]; return copy; });
      setFlying(next.id);
      setPieces([...piecesRef.current, ...plan.cells.map((cell) => ({ taskId: next.id, pos: cell.pos, amount: cell.amount }))]);
      everPlaced.current = true;
      placed += 1;
      await frame(); await frame();
      if (mine !== token.current) return;
      const fromEl = root.current?.querySelector<HTMLElement>(`[data-queue-card="${CSS.escape(next.id)}"]`);
      const toEl = root.current?.querySelector<HTMLElement>(`[data-piece-key="${CSS.escape(`${next.id}#${plan.cells[0].pos}`)}"]`);
      const reduce = window.matchMedia?.('(prefers-reduced-motion: reduce)').matches;
      if (!fromEl || !toEl || reduce) await sleep(120);
      else { await new Promise<void>((resolve) => { flightDone.current = resolve; setFlight({ task: next, from: fromEl.getBoundingClientRect(), to: toEl.getBoundingClientRect() }); }); }
      if (mine !== token.current) return;
      setFlight(null); setFlying(null);
      setLeaving((list) => [...list, next.id]);
      setTimeout(() => setLeaving((list) => list.filter((item) => item !== next.id)), 560);
      await sleep(160);
      if (stopAfterLanding.current) { setRunning(false); return; }
    }
    if (mine !== token.current) return;
    setRunning(false);
    const all = toPublic(piecesRef.current);
    onAutoComplete?.(all);
    setAnnounce(`자동 배치 완료: 작업 ${placed}개 배치${Object.keys(failures).length ? `, ${Object.keys(failures).length}개는 용량 부족` : ''}`);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [signature, current, onAutoComplete]);

  // Auto start only runs before any placement and before the user touches the board.
  useEffect(() => {
    if (autoStart === false || mode !== 'auto' || running || everPlaced.current || interacted.current) return;
    const delay = autoStart === true ? 800 : Math.max(0, autoStart);
    const timer = setTimeout(() => { if (!interacted.current && !everPlaced.current) void runAuto(); }, delay);
    return () => clearTimeout(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [autoStart, signature, mode]);
  useEffect(() => {
    if (!loop || mode !== 'auto' || running || interacted.current || !everPlaced.current) return;
    const timer = setInterval(() => {
      if (interacted.current || pointerInside.current || running) return;
      const onBoard = new Set(piecesRef.current.map((piece) => piece.taskId));
      const placeable = tasks.some((task) => !outOfRange(task) && !onBoard.has(task.id) && planAutoPlacement(piecesRef.current, effectiveUsage(task), current, total, limit, minSplit).ok);
      if (placeable) return;
      clearInterval(timer);
      setPieces(initialPieces(tasks, D, S, limit, minSplit)); setFailed({});
      void runAuto();
    }, 3200);
    return () => clearInterval(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [loop, mode, running, signature]);

  useLayoutEffect(() => {
    if (!flight || !cloneRef.current) return;
    const { from, to } = flight;
    const animation = cloneRef.current.animate([
      { transform: 'translate(0px, 0px)', width: `${from.width}px`, height: `${from.height}px`, boxShadow: '0 12px 28px -10px rgb(166 81 240 / .28)' },
      { offset: 0.55, boxShadow: '0 16px 32px -12px rgb(166 81 240 / .3)' },
      { transform: `translate(${to.left - from.left}px, ${to.top - from.top}px)`, width: `${to.width}px`, height: `${to.height}px`, boxShadow: '0 0 0 .5px rgb(0 0 0 / .06), 0 1px 2px rgb(0 0 0 / .05)' },
    ], { duration: 640, easing: 'cubic-bezier(.32,.72,.24,1)', fill: 'forwards' });
    animation.onfinish = () => { flightDone.current?.(); flightDone.current = null; };
    return () => animation.cancel();
  }, [flight]);

  useLayoutEffect(() => {
    if (!returning || returning.phase !== 'fly' || !returnRef.current) return;
    const { left, top, to } = returning;
    const options: KeyframeAnimationOptions = { duration: 260, easing: 'cubic-bezier(.32,.72,.24,1)', fill: 'forwards' };
    const animation = returnRef.current.animate([
      { transform: 'translate(0px, 0px) rotate(-1.5deg)' },
      { transform: `translate(${to.left - left}px, ${to.top - top}px) rotate(0deg)`, width: `${to.width}px`, height: `${to.height}px` },
    ], options);
    const card = returnRef.current.firstElementChild?.nextElementSibling as HTMLElement | null;
    const shadow = card?.animate([{}, { boxShadow: '0 0 0 1px rgb(0 0 0 / .07), 0 1px 2px rgb(0 0 0 / .04)' }], options);
    animation.onfinish = () => setReturning((now) => now && now.phase === 'fly' ? { ...now, phase: 'land' } : now);
    return () => { animation.cancel(); shadow?.cancel(); };
  }, [returning]);
  // The landed card shows at full strength in the same frame the copy disappears, then the settle flag drops.
  useEffect(() => {
    if (returning?.phase !== 'land') return;
    const raf = requestAnimationFrame(() => requestAnimationFrame(() => setReturning(null)));
    return () => cancelAnimationFrame(raf);
  }, [returning]);
  const cardSelector = (source: Source) => source.kind === 'queue' ? `[data-queue-card="${CSS.escape(source.taskId)}"]` : `[data-piece-key="${CSS.escape(`${source.taskId}#${source.pos}`)}"]`;
  /** Ends a pointer drag without changing the board: the copy returns to its card. */
  const cancelDrag = () => {
    const last = dragRef.current;
    setDrag(null);
    if (!last) return;
    const el = root.current?.querySelector<HTMLElement>(cardSelector(last.source));
    if (!el || window.matchMedia?.('(prefers-reduced-motion: reduce)').matches) { setReturning(null); return; }
    const rect = el.getBoundingClientRect();
    setReturning({ source: last.source, left: last.x - last.dx, top: last.y - last.dy, w: last.w, h: last.h, to: { left: rect.left, top: rect.top, width: rect.width, height: rect.height }, phase: 'fly' });
  };

  const interrupt = () => { interacted.current = true; };
  const toManual = (returnAuto: boolean) => {
    if (returnAuto) {
      token.current += 1;
      flightDone.current?.(); flightDone.current = null;
      setRunning(false); setFlying(null); setFlight(null); setFailed({});
      setPieces(piecesRef.current.filter((piece) => piece.reserved || piece.manual));
    }
    setMode('manual');
  };
  const chooseMode = (next: 'auto' | 'manual', focus: boolean) => {
    if (focus) document.getElementById(`${id}-mode-${next}`)?.focus();
    if (next === mode) return;
    interrupt();
    if (next === 'manual') toManual(true);
    else { setMode('auto'); setFailed({}); void runAuto(); }
  };

  const sourceAmount = (source: Source) => source.kind === 'queue' ? effectiveUsage(taskMap.get(source.taskId)!) : (piecesRef.current.find((piece) => piece.taskId === source.taskId && piece.pos === source.pos)?.amount ?? 0);
  const evaluate = (source: Source, target: number): Plan => {
    if (source.kind === 'piece' && source.pos === target) return { ok: false, cells: [], reason: '이미 같은 위치' };
    const base = source.kind === 'queue' ? piecesRef.current : piecesRef.current.filter((piece) => !(piece.taskId === source.taskId && piece.pos === source.pos));
    return planManualPlacement(base, sourceAmount(source), target, current, total, limit, minSplit);
  };
  const apply = (source: Source, target: number) => {
    const plan = evaluate(source, target);
    if (!plan.ok) return null;
    const task = taskMap.get(source.taskId)!;
    const before = toPublic(piecesRef.current).find((piece) => source.kind === 'piece' && piece.taskId === source.taskId && piece.day * S + piece.session === source.pos);
    const others = piecesRef.current.filter((piece) => piece.taskId !== source.taskId);
    const ownKept = source.kind === 'piece' ? piecesRef.current.filter((piece) => piece.taskId === source.taskId && piece.pos !== source.pos) : [];
    const merged = new Map<number, Piece>();
    for (const piece of [...ownKept, ...plan.cells.map((cell) => ({ taskId: source.taskId, pos: cell.pos, amount: cell.amount }))]) {
      const prev = merged.get(piece.pos);
      merged.set(piece.pos, { taskId: source.taskId, pos: piece.pos, amount: (prev?.amount ?? 0) + piece.amount, manual: true, reserved: !!task.reserved || undefined });
    }
    const next = [...others, ...[...merged.values()].sort((a, b) => a.pos - b.pos)];
    setPieces(next);
    everPlaced.current = true;
    setFailed((prev) => { const copy = { ...prev }; delete copy[source.taskId]; return copy; });
    const publicPieces = toPublic(next).filter((piece) => piece.taskId === source.taskId);
    if (source.kind === 'queue') onPlace?.(task, publicPieces);
    else if (before) onMove?.(before, { day: Math.floor(target / S), session: target % S });
    return plan;
  };
  const planText = (plan: Plan) => plan.ok ? `${plan.cells.length > 1 ? `${plan.cells.length}조각으로 나눠 ` : ''}${plan.cells.map((cell) => `${posLabel(cell.pos)} ${Math.round((cell.amount / limit) * 100)}%`).join(', ')}` : `불가: ${plan.reason}`;
  const movable = (source: Source) => source.kind === 'queue' ? effectiveUsage(taskMap.get(source.taskId)!) > 0 : source.pos >= current;
  const startDrag = () => { interrupt(); if (running) { stopAfterLanding.current = true; paused.current = false; } if (mode === 'auto') toManual(false); setDetail(null); };

  // Pointer drag (mouse and pen); touch uses the tap-to-move sheet so page scrolling keeps working.
  useEffect(() => {
    const move = (event: PointerEvent) => {
      const p = press.current;
      if (!p || event.pointerId !== p.pointerId) return;
      if (!p.started) {
        if (Math.hypot(event.clientX - p.x, event.clientY - p.y) < 4 || p.type === 'touch' || !movable(p.source)) return;
        p.started = true;
        const rect = p.el.getBoundingClientRect();
        startDrag();
        setReturning(null);
        setDrag({ source: p.source, x: event.clientX, y: event.clientY, dx: p.x - rect.left, dy: p.y - rect.top, w: rect.width, h: rect.height, target: null });
      }
      const cell = (document.elementFromPoint(event.clientX, event.clientY) as HTMLElement | null)?.closest<HTMLElement>('[data-cell-pos]');
      const target = cell && root.current?.contains(cell) ? Number(cell.dataset.cellPos) : null;
      if (dragRef.current) setDrag({ ...dragRef.current, x: event.clientX, y: event.clientY, target });
    };
    const finish = (event: PointerEvent, cancel: boolean) => {
      const p = press.current;
      if (!p || event.pointerId !== p.pointerId) return;
      press.current = null;
      paused.current = false;
      if (!p.started) { if (!cancel) toggleDetail(p.source, p.el); return; }
      const last = dragRef.current;
      // Outside, unavailable cell, same cell or pointer cancel: nothing is committed and the copy flies back.
      if (last && !cancel && last.target !== null && apply(last.source, last.target)) { setDrag(null); return; }
      cancelDrag();
    };
    const up = (event: PointerEvent) => finish(event, false);
    const cancel = (event: PointerEvent) => finish(event, true);
    const blur = () => { const started = press.current?.started; press.current = null; paused.current = false; if (started) cancelDrag(); };
    const key = (event: KeyboardEvent) => { if (event.key === 'Escape' && press.current?.started) { press.current = null; cancelDrag(); } };
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
    window.addEventListener('pointercancel', cancel);
    window.addEventListener('blur', blur);
    window.addEventListener('keydown', key);
    return () => { window.removeEventListener('pointermove', move); window.removeEventListener('pointerup', up); window.removeEventListener('pointercancel', cancel); window.removeEventListener('blur', blur); window.removeEventListener('keydown', key); };
  });
  const onCardDown = (event: ReactPointerEvent<HTMLButtonElement>, source: Source) => {
    if (event.button !== 0) return;
    if (press.current?.started) cancelDrag();
    press.current = { source, x: event.clientX, y: event.clientY, pointerId: event.pointerId, el: event.currentTarget, started: false, type: event.pointerType };
    if (running && source.kind === 'queue') paused.current = true;
  };
  const detailKey = (source: Source) => source.kind === 'queue' ? `q:${source.taskId}` : `p:${source.taskId}#${source.pos}`;
  const toggleDetail = (source: Source, el: HTMLElement) => {
    interrupt();
    const key = detailKey(source);
    setTapTarget(null);
    setDetail((open) => { if (open === key) return null; invoker.current = el; return key; });
  };
  useLayoutEffect(() => {
    if (!detail || sheetMode || !root.current || !invoker.current) { setDetailPos(null); return; }
    const base = root.current.getBoundingClientRect();
    const card = invoker.current.getBoundingClientRect();
    setDetailPos({ left: Math.max(0, Math.min(card.left - base.left, base.width - 264)), top: card.bottom - base.top + 6 });
  }, [detail, sheetMode, pieces]);
  useEffect(() => {
    if (!detail || sheetMode) return;
    const away = (event: PointerEvent) => { if (!(event.target as HTMLElement).closest?.('[data-card]')) setDetail(null); };
    document.addEventListener('pointerdown', away);
    return () => document.removeEventListener('pointerdown', away);
  }, [detail, sheetMode]);
  useEffect(() => {
    if (!pendingFocus) return;
    root.current?.querySelector<HTMLElement>(`[data-piece-key="${CSS.escape(pendingFocus)}"]`)?.focus();
    setPendingFocus(null);
  }, [pendingFocus, pieces]);

  const describeTarget = (source: Source, target: number) => { const plan = evaluate(source, target); return `${posLabel(target)}: ${plan.ok ? `놓을 수 있음 · ${planText(plan)}` : `놓을 수 없음 · ${plan.reason}`}`; };
  const cardKeys = (event: ReactKeyboardEvent<HTMLButtonElement>, source: Source) => {
    const task = taskMap.get(source.taskId)!;
    if (pick && pick.source.taskId === source.taskId && (pick.source.kind === source.kind) && (source.kind === 'queue' || (pick.source.kind === 'piece' && pick.source.pos === source.pos))) {
      const day = Math.floor(pick.target / S);
      const session = pick.target % S;
      let target = pick.target;
      if (event.key === 'ArrowLeft') target = Math.max(0, day - 1) * S + session;
      else if (event.key === 'ArrowRight') target = Math.min(D - 1, day + 1) * S + session;
      else if (event.key === 'ArrowUp') target = Math.max(0, pick.target - 1);
      else if (event.key === 'ArrowDown') target = Math.min(total - 1, pick.target + 1);
      else if (event.key === ' ' || event.key === 'Enter') {
        event.preventDefault();
        const plan = apply(source, pick.target);
        if (!plan) { setAnnounce(`놓을 수 없음: ${evaluate(source, pick.target).reason}. 계속 옮길 곳을 고르세요.`); return; }
        setPick(null);
        setAnnounce(`${task.title} 배치 완료: ${planText(plan)}`);
        setPendingFocus(`${source.taskId}#${plan.cells[0].pos}`);
        return;
      } else if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); setPick(null); setAnnounce(`${task.title} 옮기기 취소`); return; }
      else if (event.key === 'Tab') { setPick(null); setAnnounce(`${task.title} 옮기기 취소`); return; }
      else return;
      event.preventDefault();
      setPick({ source, target });
      setAnnounce(describeTarget(source, target));
      return;
    }
    if (event.key === 'Enter') { event.preventDefault(); toggleDetail(source, event.currentTarget); return; }
    if (event.key === ' ') {
      event.preventDefault();
      if (!movable(source)) { setAnnounce(source.kind === 'piece' ? '지난 세션의 작업은 옮길 수 없습니다. Enter로 상세만 볼 수 있습니다.' : '사용량 값이 없어 배치할 수 없습니다.'); return; }
      startDrag();
      let start = source.kind === 'piece' ? source.pos : current;
      if (source.kind === 'queue') { for (let pos = current; pos < total; pos += 1) if (evaluate(source, pos).ok) { start = pos; break; } }
      start = Math.min(total - 1, start);
      setPick({ source, target: start });
      setAnnounce(`${task.title} 집음. 방향키로 옮길 곳을 고르고 Space나 Enter로 놓습니다. ${describeTarget(source, start)}`);
    }
    if (event.key === 'Escape' && detail) { event.preventDefault(); setDetail(null); }
  };

  const activeSource = drag?.source ?? pick?.source ?? null;
  const activeTarget = drag ? drag.target : pick ? pick.target : null;
  const targetPlan = activeSource && activeTarget !== null ? evaluate(activeSource, activeTarget) : null;
  const byTask = new Map<string, Piece[]>();
  pieces.forEach((piece) => byTask.set(piece.taskId, [...(byTask.get(piece.taskId) ?? []), piece].sort((a, b) => a.pos - b.pos)));
  const dimmed = (task: ScheduledTask) => selectedAgent !== null && task.agent !== selectedAgent;
  const agentCount = selectedAgent ? tasks.filter((task) => task.agent === selectedAgent).length : tasks.length;

  const cardBody = (task: ScheduledTask, piece?: Piece) => {
    const U = effectiveUsage(task);
    const own = piece ? byTask.get(task.id) ?? [] : [];
    const part = piece ? own.indexOf(piece) + 1 : 0;
    const reason = !piece ? failed[task.id] : undefined;
    const pct = piece ? Math.round((piece.amount / limit) * 100) : U > 0 ? Math.round((U / limit) * 100) : null;
    const sub = [task.agent, task.description].filter(Boolean).join(' · ');
    return <>
      <span className="aui-scheduled-runs-card-top"><Mark src={task.mark} text={task.title} className="aui-scheduled-runs-card-mark" /><span className="aui-scheduled-runs-card-title">{task.title}</span></span>
      <span className="aui-scheduled-runs-card-sub" data-warn={reason || (!piece && U === 0) || undefined}>{reason ? `용량 부족 · ${reason}` : !piece && U === 0 ? '사용량 값이 없어 배치할 수 없음' : sub}</span>
      <span className="aui-scheduled-runs-card-foot">
        <span className="aui-scheduled-runs-stats">{(task.stats ?? []).slice(0, 2).map((stat, index) => { const Icon = statIcon[stat.kind]; return <span key={index} className="aui-scheduled-runs-stat"><Icon size={12} strokeWidth={1.5} aria-hidden="true" />{formatStat(stat.value)}</span>; })}</span>
        {piece && own.length > 1 && <span className="aui-scheduled-runs-part">{part}/{own.length}</span>}
        <span className="aui-scheduled-runs-tag" data-warn={reason || pct === null || undefined}>{reason ? '배치 불가' : pct === null ? '—' : `${pct}%`}</span>
      </span>
    </>;
  };
  const cardLabel = (task: ScheduledTask, piece?: Piece) => {
    const own = piece ? byTask.get(task.id) ?? [] : [];
    const stats = (task.stats ?? []).slice(0, 2).map((stat) => `${stat.label ?? statName[stat.kind]} ${stat.value}`).join(', ');
    const where = piece ? `${posLabel(piece.pos)}${own.length > 1 ? `, ${own.indexOf(piece) + 1}/${own.length} 조각` : ''}, ${Math.round((piece.amount / limit) * 100)}%` : failed[task.id] ? `배치 불가: ${failed[task.id]}` : effectiveUsage(task) > 0 ? `필요량 ${Math.round((effectiveUsage(task) / limit) * 100)}%` : '사용량 값 없음';
    return `${task.title}${task.agent ? `, ${task.agent}` : ''}, ${where}${stats ? `, ${stats}` : ''}`;
  };
  const hintFor = (source: Source) => source.kind === 'piece' && source.pos < current ? '지난 세션이라 옮길 수 없습니다. Enter로 상세를 봅니다.' : 'Enter 상세, Space로 집어 방향키로 옮긴 뒤 Space로 놓기. 마우스로 끌어 옮길 수 있습니다.';
  const renderCard = (task: ScheduledTask, source: Source, piece?: Piece) => {
    const key = detailKey(source);
    const past = source.kind === 'piece' && source.pos < current;
    const lifted = (drag && drag.source.taskId === task.id && detailKey(drag.source) === key) || (pick && detailKey(pick.source) === key) || (returning?.phase === 'fly' && detailKey(returning.source) === key);
    const settling = returning?.phase === 'land' && detailKey(returning.source) === key;
    return <button type="button" data-card className="aui-scheduled-runs-card" data-queue-card={source.kind === 'queue' ? task.id : undefined} data-piece-key={piece ? `${task.id}#${piece.pos}` : undefined}
      data-dim={dimmed(task) || undefined} data-lifted={lifted || undefined} data-settle={settling || undefined} data-past={past || undefined} data-open={detail === key || undefined}
      aria-label={cardLabel(task, piece)} aria-expanded={detail === key} aria-describedby={`${id}-hint-${past ? 'past' : 'move'}`}
      onPointerDown={(event) => onCardDown(event, source)} onClick={(event) => { if (event.detail === 0 && (event.nativeEvent as PointerEvent).pointerType === undefined) toggleDetail(source, event.currentTarget); }} onKeyDown={(event) => cardKeys(event, source)}>
      {cardBody(task, piece)}
    </button>;
  };
  const detailFor = (key: string) => {
    const isQueue = key.startsWith('q:');
    const raw = key.slice(2);
    const taskId = isQueue ? raw : raw.slice(0, raw.lastIndexOf('#'));
    const task = taskMap.get(taskId);
    if (!task) return null;
    const pos = isQueue ? null : Number(raw.slice(raw.lastIndexOf('#') + 1));
    const own = byTask.get(taskId) ?? [];
    const piece = pos === null ? undefined : own.find((item) => item.pos === pos);
    if (!isQueue && !piece) return null;
    const source: Source = isQueue ? { kind: 'queue', taskId } : { kind: 'piece', taskId, pos: pos! };
    return { task, piece, own, source };
  };
  const detailBody = (info: NonNullable<ReturnType<typeof detailFor>>) => {
    const { task, piece, own, source } = info;
    const U = effectiveUsage(task);
    return <>
      <p className="aui-scheduled-runs-detail-title">{task.title}</p>
      {task.description && <p className="aui-scheduled-runs-detail-desc">{task.description}</p>}
      <dl className="aui-scheduled-runs-detail-table">
        {task.agent && <><dt>에이전트</dt><dd>{task.agent}</dd></>}
        <dt>세션</dt><dd>{piece ? `${posLabel(piece.pos)}${own.length > 1 ? ` · ${own.indexOf(piece) + 1}/${own.length} 조각` : ''}` : failed[task.id] ? `배치 불가 · ${failed[task.id]}` : '대기 중'}</dd>
        {(task.stats ?? []).slice(0, 2).map((stat, index) => <span key={index} style={{ display: 'contents' }}><dt>{stat.label ?? statName[stat.kind]}</dt><dd>{typeof stat.value === 'number' ? stat.value.toLocaleString('en-US') : stat.value}</dd></span>)}
        <dt>필요량</dt><dd>{Math.round((U / limit) * 100)}%{piece && own.length > 1 ? ` (이 조각 ${Math.round((piece.amount / limit) * 100)}%)` : ''}</dd>
      </dl>
      <p className="aui-scheduled-runs-detail-hint">{source.kind === 'piece' && source.pos < current ? '지난 세션이라 옮길 수 없습니다.' : U <= 0 ? '사용량 값이 없어 배치할 수 없습니다.' : sheetMode ? '아래에서 옮길 세션을 고르면 나뉘는 결과를 먼저 보여 줍니다.' : '끌어서 옮기거나 Space로 집어 방향키로 옮깁니다.'}</p>
    </>;
  };
  const openInfo = detail ? detailFor(detail) : null;
  useEffect(() => { if (detail && !openInfo) setDetail(null); }, [detail, openInfo]);

  const header = <div className="aui-scheduled-runs-head">
    <div className="aui-scheduled-runs-heading"><h3 className="aui-scheduled-runs-title">{title}</h3>{subtitle && <span className="aui-scheduled-runs-subtitle" title={subtitle}>{subtitle}</span>}</div>
    <div className="aui-scheduled-runs-tools">
      {agents.length > 0 && <div className="aui-scheduled-runs-agents" role="group" aria-label="에이전트 강조" data-open={selectedAgent !== null || undefined}>
        {agents.map((agent, index) => <button key={agent.name} type="button" className="aui-scheduled-runs-agent" aria-pressed={selectedAgent === agent.name} data-other={selectedAgent !== null && selectedAgent !== agent.name || undefined} style={{ zIndex: agents.length - index }} title={agent.name} aria-label={`${agent.name} 작업 강조`}
          onClick={() => { interrupt(); setSelectedAgent((now) => now === agent.name ? null : agent.name); }}>
          <Mark src={agent.mark} text={agent.name} className="aui-scheduled-runs-agent-mark" /><span className="aui-scheduled-runs-agent-name">{agent.name}</span>
        </button>)}
      </div>}
      <span className="aui-scheduled-runs-count">{selectedAgent ? `${selectedAgent} ${agentCount}개` : `작업 ${agentCount}개`}</span>
      <div className="aui-scheduled-runs-modes" role="radiogroup" aria-label="배치 방식">
        {(['auto', 'manual'] as const).map((value) => <button key={value} id={`${id}-mode-${value}`} type="button" role="radio" aria-checked={mode === value} tabIndex={mode === value ? 0 : -1} className="aui-scheduled-runs-mode" onClick={() => chooseMode(value, false)}
          onKeyDown={(event) => { if (['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown'].includes(event.key)) { event.preventDefault(); chooseMode(value === 'auto' ? 'manual' : 'auto', true); } }}>
          {value === 'auto' ? <Sparkles size={12} strokeWidth={1.5} aria-hidden="true" /> : <Hand size={12} strokeWidth={1.5} aria-hidden="true" />}{value === 'auto' ? '자동' : '수동'}
        </button>)}
      </div>
    </div>
  </div>;

  return <div ref={root} className={`aui-scheduled-runs ${className}`} style={style} data-dragging={drag ? true : undefined} onKeyDown={(event) => { if (event.key === 'Escape' && detail && !event.defaultPrevented) { event.preventDefault(); setDetail(null); } }} onPointerEnter={() => { pointerInside.current = true; }} onPointerLeave={() => { pointerInside.current = false; }}>
    <style>{styles}</style>
    {header}
    <div className="aui-scheduled-runs-body">
      <section className="aui-scheduled-runs-queue" aria-label="배치 대기">
        <div className="aui-scheduled-runs-queue-head"><span>배치 대기</span><span className="aui-scheduled-runs-queue-count" aria-label={`남은 작업 ${remaining}개`}>{remaining}</span></div>
        <div className="aui-scheduled-runs-queue-list">
          {queue.map((task) => <div key={task.id} className="aui-scheduled-runs-slot" data-flying={flying === task.id || undefined} data-leaving={leaving.includes(task.id) || undefined}>{renderCard(task, { kind: 'queue', taskId: task.id })}</div>)}
          {remaining === 0 && <p className="aui-scheduled-runs-queue-empty">{tasks.length ? '모든 작업을 배치했습니다.' : '배치할 작업이 없습니다.'}</p>}
        </div>
      </section>
      <div className="aui-scheduled-runs-board-scroll" ref={boardRef}>
        <div className="aui-scheduled-runs-board" role="grid" aria-label="날짜별 세션 배치" aria-rowcount={S + 1} aria-colcount={D + 1} style={{ '--aui-scheduled-runs-days': D, minWidth: 38 + D * 128 } as CSSProperties}>
          <div role="row" className="aui-scheduled-runs-row">
            <span role="columnheader" className="aui-scheduled-runs-corner"><span className="aui-scheduled-runs-sr">세션</span></span>
            {days.map((day, index) => <span key={index} role="columnheader" className="aui-scheduled-runs-day"><span className="aui-scheduled-runs-day-label">{day.label}</span>{day.date !== undefined && <span className="aui-scheduled-runs-day-date">{day.date}</span>}</span>)}
          </div>
          {sessions.map((session, s) => <div key={s} role="row" className="aui-scheduled-runs-row">
            <span role="rowheader" className="aui-scheduled-runs-time">{session}</span>
            {days.map((_, d) => {
              const pos = d * S + s;
              const here = pieces.filter((piece) => piece.pos === pos);
              const meter = Math.min(100, (here.filter((piece) => !(piece.taskId === flying && byTask.get(piece.taskId)?.[0] === piece)).reduce((sum, piece) => sum + piece.amount, 0) / limit) * 100);
              // While a task is lifted every cell reads its availability. The lifted piece's own cell stays neutral:
              // dropping there is a cancel, the source card marks it, and the live region still says "이미 같은 위치".
              const origin = activeSource?.kind === 'piece' && activeSource.pos === pos;
              const isTarget = activeTarget === pos && !!targetPlan && !origin;
              const cellPlan = origin ? null : isTarget ? targetPlan : activeSource ? evaluate(activeSource, pos) : null;
              const drop = cellPlan ? (cellPlan.ok ? (isTarget ? 'ok' : 'open') : 'no') : undefined;
              const shown = here.length > 2 ? here.slice(0, 1) : here;
              return <div key={d} role="gridcell" className="aui-scheduled-runs-cell" data-cell-pos={pos} data-past={pos < current || undefined} data-current={pos === current || undefined} data-drop={drop} data-target={isTarget || undefined}
                aria-label={`${days[d].label} ${session}, 사용 ${Math.round(meter)}%, 카드 ${here.length}개${pos < current ? ', 지난 세션' : ''}${cellPlan ? cellPlan.ok ? ', 놓을 수 있음' : `, 놓을 수 없음: ${cellPlan.reason}` : ''}`}>
                {pos === current && <span className="aui-scheduled-runs-now" aria-hidden="true" />}
                <div className="aui-scheduled-runs-cell-cards">
                  {shown.map((piece) => { const task = taskMap.get(piece.taskId); if (!task) return null; const own = byTask.get(piece.taskId) ?? []; const hidden = flying === piece.taskId; return <div key={`${piece.taskId}#${piece.pos}`} className="aui-scheduled-runs-piece" data-hidden={hidden || undefined} data-first={own[0] === piece || undefined}>{renderCard(task, { kind: 'piece', taskId: task.id, pos }, piece)}</div>; })}
                  {here.length > 2 && <div className="aui-scheduled-runs-more">외 {here.length - 1}개 조각</div>}
                </div>
                <span className="aui-scheduled-runs-meter" aria-hidden="true"><i style={{ width: `${meter}%` }} /></span>
                {cellPlan && !cellPlan.ok && <span className="aui-scheduled-runs-reason" aria-hidden="true">{cellPlan.reason}</span>}
              </div>;
            })}
          </div>)}
        </div>
      </div>
    </div>
    {openInfo && !sheetMode && detailPos && <div className="aui-scheduled-runs-detail" role="dialog" aria-label={openInfo.task.title} style={{ left: detailPos.left, top: detailPos.top }}>{detailBody(openInfo)}</div>}
    {openInfo && sheetMode && <Sheet title={openInfo.task.title} returnTo={invoker.current} onClose={() => setDetail(null)}>
      {detailBody(openInfo)}
      {movable(openInfo.source) && <div className="aui-scheduled-runs-tapmove">
        <p className="aui-scheduled-runs-tapmove-title">옮길 세션</p>
        <div className="aui-scheduled-runs-targets" role="radiogroup" aria-label="옮길 세션">
          {Array.from({ length: total - current }, (_, i) => current + i).map((pos) => { const plan = evaluate(openInfo.source, pos); return <button key={pos} type="button" role="radio" aria-checked={tapTarget === pos} className="aui-scheduled-runs-target" data-ok={plan.ok || undefined} onClick={() => setTapTarget(pos)}>
            <span className="aui-scheduled-runs-target-name">{posLabel(pos)}</span><span className="aui-scheduled-runs-target-plan">{plan.ok ? planText(plan) : `불가 · ${plan.reason}`}</span>
          </button>; })}
        </div>
        <button type="button" className="aui-scheduled-runs-confirm" disabled={tapTarget === null || !evaluate(openInfo.source, tapTarget).ok} onClick={() => {
          if (tapTarget === null) return;
          startDrag();
          const plan = apply(openInfo.source, tapTarget);
          if (!plan) return;
          setAnnounce(`${openInfo.task.title} 배치 완료: ${planText(plan)}`);
          invoker.current = null;
          setDetail(null); setTapTarget(null);
          setPendingFocus(`${openInfo.task.id}#${plan.cells[0].pos}`);
        }}>{tapTarget === null ? '세션을 고르세요' : '여기로 옮기기'}</button>
      </div>}
    </Sheet>}
    {drag && typeof document !== 'undefined' && createPortal(<div className="aui-scheduled-runs-float" style={{ left: drag.x - drag.dx, top: drag.y - drag.dy, width: drag.w, height: drag.h }}><style>{styles}</style><div className="aui-scheduled-runs-card" data-ghost>{cardBody(taskMap.get(drag.source.taskId)!, drag.source.kind === 'piece' ? pieces.find((piece) => piece.taskId === drag.source.taskId && piece.pos === (drag.source as { pos: number }).pos) : undefined)}</div></div>, document.body)}
    {returning?.phase === 'fly' && typeof document !== 'undefined' && createPortal(<div ref={returnRef} className="aui-scheduled-runs-float" data-return style={{ left: returning.left, top: returning.top, width: returning.w, height: returning.h }}><style>{styles}</style><div className="aui-scheduled-runs-card" data-ghost>{cardBody(taskMap.get(returning.source.taskId)!, returning.source.kind === 'piece' ? pieces.find((piece) => piece.taskId === returning.source.taskId && piece.pos === (returning.source as { pos: number }).pos) : undefined)}</div></div>, document.body)}
    {flight && typeof document !== 'undefined' && createPortal(<div ref={cloneRef} className="aui-scheduled-runs-float" data-flight style={{ left: flight.from.left, top: flight.from.top, width: flight.from.width, height: flight.from.height }}><style>{styles}</style><div className="aui-scheduled-runs-card" data-ghost>{cardBody(flight.task)}</div></div>, document.body)}
    <span id={`${id}-hint-move`} className="aui-scheduled-runs-sr">{hintFor({ kind: 'queue', taskId: '' })}</span>
    <span id={`${id}-hint-past`} className="aui-scheduled-runs-sr">{hintFor({ kind: 'piece', taskId: '', pos: -1 })}</span>
    <span className="aui-scheduled-runs-sr" role="status" aria-live="polite">{announce}</span>
  </div>;
}

const styles = `
@layer kit {
.aui-scheduled-runs, .aui-scheduled-runs-layer, .aui-scheduled-runs-float {
  --aui-scheduled-runs-base: var(--aui-bg-2, #f0f0f0); --aui-scheduled-runs-card: var(--aui-bg-1, #ffffff); --aui-scheduled-runs-strong: #171717; --aui-scheduled-runs-ink: var(--aui-fg, #171717); --aui-scheduled-runs-body: #6b6b6b; --aui-scheduled-runs-muted: var(--aui-fg-muted, #8a8a8a); --aui-scheduled-runs-faint: #b5b5b5;
  --aui-scheduled-runs-line: var(--aui-line, #ececec); --aui-scheduled-runs-queue: #f4f4f4; --aui-scheduled-runs-well: #fafafa; --aui-scheduled-runs-ring: #f0f0f0; --aui-scheduled-runs-blue: #4fb0fe; --aui-scheduled-runs-drop-line: rgb(1 144 254 / .45); --aui-scheduled-runs-drop: #edf7ff;
  --aui-scheduled-runs-hatch: rgb(0 0 0 / .05); --aui-scheduled-runs-meter: #c2c2c2; --aui-scheduled-runs-track: #ededed; --aui-scheduled-runs-now: #f04438; --aui-scheduled-runs-tag: rgb(0 0 0 / .06); --aui-scheduled-runs-warn: #b25e00; --aui-scheduled-runs-focus: rgb(0 96 226 / .7); --aui-scheduled-runs-violet: #a651f0;
  --aui-scheduled-runs-count-face: color-mix(in srgb, var(--aui-scheduled-runs-violet) 6%, transparent); --aui-scheduled-runs-mode-track: rgb(0 0 0 / .05);
  --aui-scheduled-runs-shadow: 0 0 0 1px #ececec, 0 1px 2px rgb(0 0 0 / .04); --aui-scheduled-runs-mode-shadow: 0 0 0 1px #e4e4e4, 0 1px 2px rgb(0 0 0 / .06);
  --aui-scheduled-runs-ease: cubic-bezier(.32,.72,.24,1);
}
:where(.dark,[data-theme="dark"]) :is(.aui-scheduled-runs, .aui-scheduled-runs-layer, .aui-scheduled-runs-float) {
  --aui-scheduled-runs-base: var(--aui-bg-2, #141416); --aui-scheduled-runs-card: var(--aui-bg-1, #1c1c1f); --aui-scheduled-runs-strong: #ffffff; --aui-scheduled-runs-ink: rgb(255 255 255 / .9); --aui-scheduled-runs-body: rgb(255 255 255 / .62); --aui-scheduled-runs-muted: rgb(255 255 255 / .45); --aui-scheduled-runs-faint: rgb(255 255 255 / .28);
  --aui-scheduled-runs-line: rgb(255 255 255 / .08); --aui-scheduled-runs-queue: #161618; --aui-scheduled-runs-well: rgb(255 255 255 / .03); --aui-scheduled-runs-ring: rgb(255 255 255 / .06); --aui-scheduled-runs-drop-line: rgb(79 176 254 / .5); --aui-scheduled-runs-drop: rgb(79 176 254 / .1);
  --aui-scheduled-runs-hatch: rgb(255 255 255 / .06); --aui-scheduled-runs-meter: #55555c; --aui-scheduled-runs-track: rgb(255 255 255 / .06); --aui-scheduled-runs-now: #ff6b5f; --aui-scheduled-runs-tag: rgb(255 255 255 / .07); --aui-scheduled-runs-warn: #e6c24d; --aui-scheduled-runs-focus: rgb(77 141 255 / .8);
  --aui-scheduled-runs-count-face: rgb(255 255 255 / .08); --aui-scheduled-runs-mode-track: rgb(255 255 255 / .07);
  --aui-scheduled-runs-shadow: 0 0 0 .5px rgb(255 255 255 / .1), 0 1px 2px rgb(0 0 0 / .4); --aui-scheduled-runs-mode-shadow: 0 0 0 .5px rgb(255 255 255 / .12), 0 1px 2px rgb(0 0 0 / .4);
}
.aui-scheduled-runs { position: relative; box-sizing: border-box; width: 100%; min-width: 0; padding: 4px; border-radius: 20px; background: var(--aui-scheduled-runs-base); container: aui-scheduled-runs / inline-size; color: var(--aui-scheduled-runs-body); font: 400 14px/20px var(--aui-font-sans, Inter, ui-sans-serif, system-ui, -apple-system, 'Segoe UI', sans-serif); letter-spacing: -.006em; word-break: keep-all; overflow-wrap: anywhere; -webkit-tap-highlight-color: transparent; }
.aui-scheduled-runs *, .aui-scheduled-runs-layer *, .aui-scheduled-runs-float * { box-sizing: border-box; }
.aui-scheduled-runs :is(p, h3, dl, dd) { margin: 0; }
.aui-scheduled-runs button, .aui-scheduled-runs-layer button { font: inherit; letter-spacing: inherit; color: inherit; cursor: pointer; touch-action: manipulation; }
.aui-scheduled-runs button:focus-visible, .aui-scheduled-runs-layer button:focus-visible { outline: 2px solid var(--aui-scheduled-runs-focus); outline-offset: 2px; }
.aui-scheduled-runs .aui-scheduled-runs-mode:focus-visible { outline-offset: -2px; }
.aui-scheduled-runs-head { display: flex; min-height: 48px; flex-wrap: wrap; align-items: center; gap: 12px; padding: 6px 8px 8px 12px; }
.aui-scheduled-runs-heading { display: flex; min-width: 0; flex: 1 1 auto; align-items: baseline; gap: 8px; }
.aui-scheduled-runs-title { flex: none; color: var(--aui-scheduled-runs-strong); font-size: 15px; font-weight: 600; line-height: 24px; white-space: nowrap; }
.aui-scheduled-runs-subtitle { min-width: 0; overflow: hidden; color: var(--aui-scheduled-runs-muted); font-size: 12.5px; text-overflow: ellipsis; white-space: nowrap; }
.aui-scheduled-runs-tools { display: flex; flex-wrap: wrap; align-items: center; gap: 12px; margin-left: auto; }
.aui-scheduled-runs-agents { display: flex; align-items: center; padding-left: 9px; }
.aui-scheduled-runs-agent { position: relative; display: inline-flex; height: 26px; align-items: center; margin-left: -9px; padding: 0 5px; border: 0; border-radius: 7px; background: var(--aui-scheduled-runs-card); box-shadow: var(--aui-scheduled-runs-shadow); color: var(--aui-scheduled-runs-ink); transition: margin 380ms var(--aui-scheduled-runs-ease), transform 160ms ease, opacity 200ms ease; }
.aui-scheduled-runs-agent:hover { transform: translateY(-1px); }
.aui-scheduled-runs-agents[data-open] .aui-scheduled-runs-agent { margin-left: 4px; }
.aui-scheduled-runs-agent[data-other] { opacity: .6; }
.aui-scheduled-runs-agent-mark { display: grid; width: 16px; height: 16px; place-items: center; border-radius: 4px; color: var(--aui-scheduled-runs-body); font-size: 10px; font-weight: 600; }
.aui-scheduled-runs-agent-mark img, .aui-scheduled-runs-card-mark img { width: 12px; height: 12px; border-radius: 3px; }
.aui-scheduled-runs-agent-name { display: inline-block; max-width: 0; overflow: hidden; margin: 0; font-size: 12px; font-weight: 500; line-height: 16px; opacity: 0; text-overflow: ellipsis; white-space: nowrap; transition: max-width 380ms var(--aui-scheduled-runs-ease), margin 380ms var(--aui-scheduled-runs-ease), opacity 240ms ease; }
.aui-scheduled-runs-agent[aria-pressed="true"] .aui-scheduled-runs-agent-name { max-width: 120px; margin: 0 3px 0 6px; opacity: 1; }
.aui-scheduled-runs-count { color: var(--aui-scheduled-runs-muted); font-size: 12px; font-variant-numeric: tabular-nums; white-space: nowrap; }
.aui-scheduled-runs-modes { display: inline-flex; padding: 3px; border-radius: 10px; background: var(--aui-scheduled-runs-mode-track); }
.aui-scheduled-runs-mode { display: inline-flex; height: 26px; align-items: center; gap: 5px; padding: 0 11px; border: 0; border-radius: 7px; background: transparent; color: var(--aui-scheduled-runs-muted) !important; font-size: 13px !important; font-weight: 500 !important; transition: background-color 160ms ease, color 160ms ease, box-shadow 160ms ease; }
.aui-scheduled-runs-mode[aria-checked="true"] { background: var(--aui-scheduled-runs-card); box-shadow: var(--aui-scheduled-runs-mode-shadow); color: var(--aui-scheduled-runs-ink) !important; }
.aui-scheduled-runs-body { display: grid; grid-template-columns: 220px minmax(0, 1fr); gap: 12px; padding: 10px; border-radius: 16px; background: var(--aui-scheduled-runs-card); box-shadow: var(--aui-scheduled-runs-shadow); }
.aui-scheduled-runs-queue { position: relative; display: flex; min-width: 0; min-height: 240px; flex-direction: column; padding: 4px; border-radius: 12px; background: var(--aui-scheduled-runs-queue); }
.aui-scheduled-runs-queue-head { display: flex; height: 30px; flex: none; align-items: center; justify-content: space-between; padding: 0 6px 0 8px; color: var(--aui-scheduled-runs-muted); font-size: 11.5px; font-weight: 500; }
.aui-scheduled-runs-queue-count { display: grid; min-width: 20px; height: 18px; padding: 0 5px; place-items: center; border-radius: 999px; background: var(--aui-scheduled-runs-count-face); font-size: 11px; font-variant-numeric: tabular-nums; }
.aui-scheduled-runs-queue-list { min-height: 0; flex: 1 1 0; overflow-y: auto; overscroll-behavior: contain; }
.aui-scheduled-runs-slot { height: 70px; }
.aui-scheduled-runs-slot[data-flying] .aui-scheduled-runs-card { visibility: hidden; }
.aui-scheduled-runs-slot[data-leaving] { overflow: hidden; animation: aui-scheduled-runs-collapse 420ms var(--aui-scheduled-runs-ease) 140ms both; }
.aui-scheduled-runs-slot[data-leaving] .aui-scheduled-runs-card { visibility: hidden; }
.aui-scheduled-runs-queue-empty { padding: 4px; color: var(--aui-scheduled-runs-faint); font-size: 12px; text-align: center; animation: aui-scheduled-runs-fade 300ms ease 300ms both; }
.aui-scheduled-runs-card { display: grid; width: 100%; height: 66px; min-width: 0; align-content: center; gap: 3px; padding: 0 8px 0 6px; border: 0; border-radius: 10px; background: var(--aui-scheduled-runs-card); box-shadow: var(--aui-scheduled-runs-shadow); color: var(--aui-scheduled-runs-ink); text-align: left; transition: opacity 240ms ease, box-shadow 160ms ease; user-select: none; -webkit-user-select: none; }
.aui-scheduled-runs-card[data-dim] { opacity: .35; }
.aui-scheduled-runs-card[data-lifted] { opacity: .35; }
.aui-scheduled-runs-card[data-settle] { transition: none; }
.aui-scheduled-runs-card[data-open] { box-shadow: 0 0 0 1.5px var(--aui-scheduled-runs-drop-line), var(--aui-scheduled-runs-shadow); }
.aui-scheduled-runs-card-top { display: flex; min-width: 0; align-items: center; gap: 6px; }
.aui-scheduled-runs-card-mark { display: grid; flex: none; width: 18px; height: 18px; place-items: center; border-radius: 5px; box-shadow: inset 0 0 0 1px var(--aui-scheduled-runs-line); color: var(--aui-scheduled-runs-body); font-size: 10px; font-weight: 600; }
.aui-scheduled-runs-card-title { min-width: 0; overflow: hidden; font-size: 12px; font-weight: 500; line-height: 15px; text-overflow: ellipsis; white-space: nowrap; }
.aui-scheduled-runs-card-sub { min-width: 0; overflow: hidden; padding-left: 24px; color: var(--aui-scheduled-runs-muted); font-size: 11.5px; line-height: 16px; text-overflow: ellipsis; white-space: nowrap; }
.aui-scheduled-runs-card-sub[data-warn] { color: var(--aui-scheduled-runs-warn); }
.aui-scheduled-runs-card-foot { display: flex; min-width: 0; height: 16px; align-items: center; gap: 6px; padding-left: 24px; }
.aui-scheduled-runs-stats { display: flex; min-width: 0; flex: 1; gap: 10px; overflow: hidden; color: var(--aui-scheduled-runs-muted); font-size: 11.5px; line-height: 16px; font-variant-numeric: tabular-nums; }
.aui-scheduled-runs-stat { display: inline-flex; flex: none; align-items: center; gap: 3px; }
.aui-scheduled-runs-part { flex: none; color: var(--aui-scheduled-runs-muted); font-size: 10px; line-height: 15px; font-variant-numeric: tabular-nums; }
.aui-scheduled-runs-tag { flex: none; margin-left: auto; padding: 0 5px; border-radius: 999px; background: var(--aui-scheduled-runs-tag); color: var(--aui-scheduled-runs-body); font-size: 10px; line-height: 15px; font-variant-numeric: tabular-nums; }
.aui-scheduled-runs-part + .aui-scheduled-runs-tag { margin-left: 0; }
.aui-scheduled-runs-tag[data-warn] { color: var(--aui-scheduled-runs-warn); }
.aui-scheduled-runs-board-scroll { min-width: 0; overflow-x: auto; overscroll-behavior-x: contain; scrollbar-width: thin; scrollbar-color: var(--aui-scheduled-runs-line) transparent; }
.aui-scheduled-runs-board { display: grid; gap: 4px; }
.aui-scheduled-runs-row { display: grid; grid-template-columns: 34px repeat(var(--aui-scheduled-runs-days), minmax(0, 1fr)); gap: 4px; }
.aui-scheduled-runs-row:first-child { height: 30px; }
.aui-scheduled-runs-day { display: flex; height: 26px; align-items: baseline; gap: 4px; padding: 5px 8px 0; border-radius: 10px; box-shadow: inset 0 0 0 1px var(--aui-scheduled-runs-ring); }
.aui-scheduled-runs-day-label { color: var(--aui-scheduled-runs-muted); font-size: 10.5px; font-weight: 500; letter-spacing: .5px; line-height: 16px; text-transform: uppercase; }
.aui-scheduled-runs-day-date { color: var(--aui-scheduled-runs-ink); font-size: 12px; font-weight: 500; letter-spacing: 0; font-variant-numeric: tabular-nums; }
.aui-scheduled-runs-time { padding-top: 6px; color: var(--aui-scheduled-runs-faint); font-size: 11px; line-height: 16px; font-variant-numeric: tabular-nums; }
.aui-scheduled-runs-cell { position: relative; height: 160px; min-width: 0; padding: 4px 4px 20px; border-radius: 12px; background: var(--aui-scheduled-runs-well); box-shadow: inset 0 0 0 1px var(--aui-scheduled-runs-ring); transition: background-color 160ms ease, box-shadow 160ms ease; }
.aui-scheduled-runs-cell[data-past] { background: transparent; }
.aui-scheduled-runs-cell[data-past] .aui-scheduled-runs-cell-cards { opacity: .45; }
.aui-scheduled-runs-cell[data-past] .aui-scheduled-runs-meter i { opacity: .5; }
.aui-scheduled-runs-cell[data-drop="ok"] { background: var(--aui-scheduled-runs-drop); box-shadow: inset 0 0 0 1px var(--aui-scheduled-runs-drop-line); }
.aui-scheduled-runs-cell[data-drop="no"] { background: repeating-linear-gradient(135deg, var(--aui-scheduled-runs-well) 0 6px, var(--aui-scheduled-runs-hatch) 6px 7px); }
.aui-scheduled-runs-cell[data-drop="no"] > :not(.aui-scheduled-runs-reason) { opacity: .45; }
.aui-scheduled-runs-cell[data-drop="no"][data-target] { box-shadow: inset 0 0 0 1.5px var(--aui-scheduled-runs-faint); }
.aui-scheduled-runs-cell[data-drop="no"][data-target] .aui-scheduled-runs-reason { box-shadow: 0 0 0 1px var(--aui-scheduled-runs-warn), var(--aui-scheduled-runs-shadow); }
.aui-scheduled-runs-cell-cards { display: grid; gap: 3px; transition: opacity 160ms ease; }
.aui-scheduled-runs-piece[data-hidden] { visibility: hidden; }
/* Trailing pieces stay hidden through the flight; the appear animation starts when they are revealed with the landing. */
.aui-scheduled-runs-piece:not([data-first]):not([data-hidden]) { animation: aui-scheduled-runs-appear 460ms var(--aui-scheduled-runs-ease) both; }
.aui-scheduled-runs-more { height: 66px; padding-left: 6px; color: var(--aui-scheduled-runs-muted); font-size: 11px; line-height: 66px; }
.aui-scheduled-runs-now { position: absolute; top: -2.75px; right: 0; left: 0; z-index: 2; height: 1.5px; background: var(--aui-scheduled-runs-now); }
.aui-scheduled-runs-now::before { position: absolute; top: -3.25px; left: -4px; width: 8px; height: 8px; border-radius: 50%; background: var(--aui-scheduled-runs-now); content: ''; }
.aui-scheduled-runs-meter { position: absolute; right: 8px; bottom: 9px; left: 8px; height: 4px; overflow: hidden; border-radius: 2px; background: var(--aui-scheduled-runs-track); }
.aui-scheduled-runs-meter i { display: block; height: 100%; border-radius: 2px; background: var(--aui-scheduled-runs-meter); transition: width 520ms var(--aui-scheduled-runs-ease); }
.aui-scheduled-runs-reason { position: absolute; top: 50%; left: 50%; z-index: 3; max-width: calc(100% - 8px); padding: 2px 8px; border-radius: 999px; background: var(--aui-scheduled-runs-card); box-shadow: var(--aui-scheduled-runs-shadow); color: var(--aui-scheduled-runs-warn); font-size: 11px; font-weight: 500; line-height: 16px; text-align: center; transform: translate(-50%, -50%); animation: aui-scheduled-runs-reason 140ms ease both; }
.aui-scheduled-runs-detail { position: absolute; z-index: 7; display: grid; width: 264px; gap: 8px; padding: 12px; border-radius: 14px; background: var(--aui-scheduled-runs-card); box-shadow: 0 0 0 1px var(--aui-scheduled-runs-line), 0 14px 32px -10px rgb(166 81 240 / .22); color: var(--aui-scheduled-runs-body); pointer-events: none; animation: aui-scheduled-runs-detail 180ms var(--aui-scheduled-runs-ease) both; }
.aui-scheduled-runs-detail-title { color: var(--aui-scheduled-runs-ink); font-size: 13px; font-weight: 500; line-height: 18px; }
.aui-scheduled-runs-detail-desc { font-size: 12px; line-height: 17px; }
.aui-scheduled-runs-detail-table { display: grid; grid-template-columns: auto minmax(0, 1fr); gap: 4px 12px; padding-top: 8px; border-top: .5px solid var(--aui-scheduled-runs-line); font-size: 12px; line-height: 16px; }
.aui-scheduled-runs-detail-table dt { color: var(--aui-scheduled-runs-muted); }
.aui-scheduled-runs-detail-table dd { color: var(--aui-scheduled-runs-ink); font-variant-numeric: tabular-nums; }
.aui-scheduled-runs-detail-hint { color: var(--aui-scheduled-runs-faint); font-size: 11px; line-height: 15px; }
.aui-scheduled-runs-float { position: fixed; z-index: 1100; pointer-events: none; }
.aui-scheduled-runs-float:not([data-flight]) { transform: rotate(-1.5deg); }
.aui-scheduled-runs-float .aui-scheduled-runs-card { height: 100%; box-shadow: 0 0 0 .5px rgb(0 0 0 / .08), 0 12px 24px -8px rgb(166 81 240 / .28); font: 400 14px/20px var(--aui-font-sans, Inter, ui-sans-serif, system-ui, sans-serif); letter-spacing: -.006em; }
.aui-scheduled-runs-float[data-flight] { border-radius: 10px; }
.aui-scheduled-runs-sr { position: absolute; width: 1px; height: 1px; overflow: hidden; clip-path: inset(50%); white-space: nowrap; }
.aui-scheduled-runs-layer { position: fixed; inset: 0; z-index: 1000; display: flex; align-items: flex-end; padding: env(safe-area-inset-top) env(safe-area-inset-right) 0 env(safe-area-inset-left); background: rgb(0 0 0 / .36); color: var(--aui-scheduled-runs-body); font: 400 14px/20px var(--aui-font-sans, Inter, ui-sans-serif, system-ui, sans-serif); word-break: keep-all; overflow-wrap: anywhere; }
.aui-scheduled-runs-layer :is(p, dl, dd) { margin: 0; }
.aui-scheduled-runs-sheet { display: flex; flex-direction: column; width: 100%; max-height: calc(100dvh - 40px - env(safe-area-inset-top)); border-radius: 20px 20px 0 0; background: var(--aui-scheduled-runs-card); box-shadow: 0 -8px 32px rgb(0 0 0 / .18); animation: aui-scheduled-runs-sheet 220ms var(--aui-scheduled-runs-ease) both; }
.aui-scheduled-runs-sheet-head { display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 6px 6px 0 18px; }
.aui-scheduled-runs-sheet-head h2 { min-width: 0; margin: 0; color: var(--aui-scheduled-runs-ink); font-size: 15px; font-weight: 600; }
.aui-scheduled-runs-sheet-close { display: grid; flex: none; width: 44px; height: 44px; place-items: center; border: 0; border-radius: 12px; background: transparent; color: var(--aui-scheduled-runs-muted) !important; }
.aui-scheduled-runs-sheet-body { display: grid; gap: 10px; min-height: 0; overflow-y: auto; overscroll-behavior: contain; padding: 4px 18px calc(18px + env(safe-area-inset-bottom)); }
.aui-scheduled-runs-sheet-body .aui-scheduled-runs-detail-title { display: none; }
.aui-scheduled-runs-sheet-body .aui-scheduled-runs-detail-table, .aui-scheduled-runs-sheet-body .aui-scheduled-runs-detail-desc { font-size: 14px; line-height: 20px; }
.aui-scheduled-runs-sheet-body .aui-scheduled-runs-detail-hint { font-size: 13px; line-height: 18px; }
.aui-scheduled-runs-tapmove { display: grid; gap: 6px; }
.aui-scheduled-runs-tapmove-title { color: var(--aui-scheduled-runs-ink); font-size: 13px; font-weight: 600; }
.aui-scheduled-runs-targets { display: grid; gap: 4px; }
.aui-scheduled-runs-target { display: grid; min-height: 52px; align-content: center; gap: 2px; padding: 6px 12px; border: 0; border-radius: 12px; background: var(--aui-scheduled-runs-well); box-shadow: inset 0 0 0 1px var(--aui-scheduled-runs-ring); text-align: left; }
.aui-scheduled-runs-target[aria-checked="true"] { background: var(--aui-scheduled-runs-drop); box-shadow: inset 0 0 0 1.5px var(--aui-scheduled-runs-drop-line); }
.aui-scheduled-runs-target-name { color: var(--aui-scheduled-runs-ink); font-size: 14px; font-weight: 500; }
.aui-scheduled-runs-target-plan { color: var(--aui-scheduled-runs-muted); font-size: 13px; line-height: 18px; }
.aui-scheduled-runs-target:not([data-ok]) .aui-scheduled-runs-target-plan { color: var(--aui-scheduled-runs-warn); }
.aui-scheduled-runs-confirm { min-height: 48px; margin-top: 4px; border: 0; border-radius: 12px; background: var(--aui-scheduled-runs-ink); color: var(--aui-scheduled-runs-card) !important; font-size: 15px !important; font-weight: 600 !important; }
.aui-scheduled-runs-confirm:disabled { opacity: .4; cursor: default; }
@container aui-scheduled-runs (max-width: 640px) {
  .aui-scheduled-runs-body { grid-template-columns: minmax(0, 1fr); }
  .aui-scheduled-runs-queue { min-height: 0; }
  .aui-scheduled-runs-queue-list { display: grid; max-height: 320px; flex: none; grid-template-columns: repeat(auto-fill, minmax(180px, 1fr)); column-gap: 4px; align-content: start; }
}
@media (pointer: coarse), (max-width: 899px) {
  .aui-scheduled-runs-agent, .aui-scheduled-runs-agents[data-open] .aui-scheduled-runs-agent { min-width: 44px; height: 44px; justify-content: center; margin-left: 4px; }
  .aui-scheduled-runs-agents { padding-left: 0; }
  .aui-scheduled-runs-mode { height: 44px; padding: 0 14px; }
  .aui-scheduled-runs-card { touch-action: manipulation; }
}
@keyframes aui-scheduled-runs-collapse { from { height: 70px; } to { height: 0; } }
@keyframes aui-scheduled-runs-fade { from { opacity: 0; } to { opacity: 1; } }
@keyframes aui-scheduled-runs-appear { from { opacity: 0; transform: translateY(-6px) scale(.96); } to { opacity: 1; transform: none; } }
@keyframes aui-scheduled-runs-detail { from { opacity: 0; transform: translateY(-4px) scale(.98); } to { opacity: 1; transform: none; } }
@keyframes aui-scheduled-runs-reason { from { opacity: 0; transform: translate(-50%, calc(-50% - 4px)) scale(.98); } to { opacity: 1; transform: translate(-50%, -50%); } }
@keyframes aui-scheduled-runs-sheet { from { opacity: 0; transform: translateY(24px); } to { opacity: 1; transform: none; } }
@media (prefers-reduced-motion: reduce) {
  :is(.aui-scheduled-runs, .aui-scheduled-runs-layer, .aui-scheduled-runs-float) *, :is(.aui-scheduled-runs, .aui-scheduled-runs-layer) *::before { transition: none !important; animation: none !important; }
  .aui-scheduled-runs-slot[data-leaving] { display: none; }
}
:where(.dark,[data-theme="dark"]) .aui-scheduled-runs-demo-bar button { border-color: var(--aui-line, #3a3a40); background: var(--aui-bg-1, #232326); color: var(--aui-fg, #ededed); }
@media (pointer: coarse) { .aui-scheduled-runs-demo-bar button { min-height: 44px; } }
}
`;
