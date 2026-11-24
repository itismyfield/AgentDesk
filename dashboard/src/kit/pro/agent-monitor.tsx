import { useCallback, useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type KeyboardEvent as ReactKeyboardEvent, type ReactNode } from "react";
import { ArrowUpRight, Check, ChevronsUpDown, FileText, Globe, Pause, Pencil, Play, RotateCcw, Search, Sparkles, Terminal, TriangleAlert, X } from "lucide-react";

// Clean-room implementation from clean-specs/agent-monitor (11/17 revision).
// No affectUI Pro source, registry payload, package, site asset or original copy is used.

export type AgentMonitorStatus = "idle" | "running" | "waiting" | "paused" | "done" | "failed" | "cancelled";
export type AgentMonitorStepStatus = "pending" | "running" | "done" | "failed";
export type AgentMonitorStepKind = "read" | "search" | "edit" | "run" | "think" | "web";

export interface AgentMonitorStep {
  id: string;
  label: string;
  status?: AgentMonitorStepStatus;
  /** Earlier kit shape: done · current · upcoming. */
  state?: "done" | "current" | "upcoming";
  kind?: AgentMonitorStepKind;
  /** Neutral mark that overrides the kind icon. Reuse the same element to group a tool. */
  icon?: ReactNode;
  /** Epoch milliseconds. */
  started?: number;
  /** Finished duration in milliseconds. */
  duration?: number;
  /** Tool name. Defaults to the generic name of the kind. */
  tool?: string;
  description?: string;
  output?: string;
}

export interface AgentMonitorAgent { name: string; role?: string; mark?: ReactNode }
export interface AgentMonitorApproval { title: string; command?: string; icon?: ReactNode }
export interface AgentMonitorResult { added?: number; removed?: number; files?: number; note?: string }

export interface AgentMonitorProps {
  status?: AgentMonitorStatus;
  agent?: AgentMonitorAgent;
  /** Oldest first. */
  steps?: AgentMonitorStep[];
  /** Overrides the well only; the bar, tools and position still come from steps. */
  current?: { label: string; kind?: AgentMonitorStepKind; icon?: ReactNode };
  started?: number;
  ended?: number;
  /** Accumulated paused milliseconds, kept by the host. */
  pausedMs?: number;
  /** Reference clock (epoch ms). Without it running and waiting tick every 100ms. */
  now?: number;
  /** USD. A string is shown verbatim (earlier kit shape). */
  cost?: number | string;
  approval?: AgentMonitorApproval;
  result?: AgentMonitorResult;
  error?: string;
  defaultFolded?: boolean;
  onPause?: () => void;
  onResume?: () => void;
  onCancel?: () => void;
  onApprove?: () => void;
  onDeny?: () => void;
  onRetry?: () => void;
  onOpenLog?: () => void;
  /** Every visible and spoken text. Missing keys keep the Korean defaults. */
  labels?: Partial<AgentMonitorLabels>;
  /** Earlier kit shape: agent name. */
  name?: string;
  /** Earlier kit shape: current step text. */
  step?: string;
  /** Earlier kit shape: preformatted elapsed text. */
  elapsed?: string;
  /** Earlier kit shape: approval title. */
  needsApproval?: string;
  className?: string;
  style?: CSSProperties;
}

export interface AgentMonitorLabels {
  /** Name used when the agent has none. */
  agent: string;
  statusIdle: string;
  statusRunning: string;
  statusWaiting: string;
  statusPaused: string;
  statusDone: string;
  statusFailed: string;
  statusCancelled: string;
  stepPending: string;
  stepRunning: string;
  stepDone: string;
  stepFailed: string;
  kindRead: string;
  kindSearch: string;
  kindEdit: string;
  kindRun: string;
  kindThink: string;
  kindWeb: string;
  /** Well text while waiting without an approval title. */
  waiting: string;
  /** Announced while waiting without an approval title. */
  waitingAnnounce: string;
  /** Well text of a failure without an error message. */
  failed: string;
  /** Announced for a failure without an error message. */
  failedAnnounce: string;
  /** Accessible name of the approval command. */
  command: (command: string) => string;
  /** Well prefix while paused. */
  paused: string;
  /** Done well line, from the formatted span. */
  done: (span: string) => string;
  files: (count: number) => string;
  /** Cancelled well line, from the formatted span. */
  cancelled: (span: string) => string;
  idle: string;
  cancel: string;
  deny: string;
  approve: string;
  retry: string;
  pause: string;
  resume: string;
  log: string;
  /** Accessible name of the folded bar, from the card label. */
  expand: (label: string) => string;
  /** Accessible name of the fold button, from the agent name. */
  fold: (name: string) => string;
  /** Accessible name of the folded tools button. */
  tools: (count: number) => string;
  /** Heading of the tools sheet. */
  toolsTitle: (count: number) => string;
  close: string;
  closeStep: string;
  factStart: string;
  factDuration: string;
  factShare: string;
  factStatus: string;
  /** Accessible name of a step output. */
  output: (output: string) => string;
  /** Accessible name of the waiting card. */
  request: (name: string, title: string) => string;
  cost: string;
  elapsed: string;
  /** Accessible name of the step bar, from "label time" parts joined by commas. */
  timeline: (steps: string) => string;
  /** Accessible name of the finished-step list. */
  recent: string;
  /** Accessible name of a finished-step row. */
  stepDetail: (label: string, time: string) => string;
  /** Spoken before "3/5". */
  position: string;
  /** Spoken keyboard hint for the shortcuts the host enabled. */
  keys: (approve: boolean, deny: boolean) => string;
  /** Done and cancelled spans. */
  span: (ms: number) => string;
  /** Step durations. */
  stepTime: (ms: number) => string;
}

const statusText: Record<AgentMonitorStatus, string> = { idle: "준비", running: "실행 중", waiting: "확인 필요", paused: "일시정지", done: "완료", failed: "실패", cancelled: "취소됨" };
const stepStatusText: Record<AgentMonitorStepStatus, string> = { pending: "대기", running: "진행 중", done: "완료", failed: "실패" };
const kindName: Record<AgentMonitorStepKind, string> = { read: "읽기", search: "검색", edit: "수정", run: "실행", think: "생각", web: "웹" };
const kindIcon: Record<AgentMonitorStepKind, typeof FileText> = { read: FileText, search: Search, edit: Pencil, run: Terminal, think: Sparkles, web: Globe };
const MOVE = "cubic-bezier(0.32, 0.72, 0.24, 1)";

const pad = (value: number) => String(value).padStart(2, "0");
/** Running clock: floor seconds, m:ss under an hour, then h:mm:ss. */
export function formatAgentMonitorClock(ms: number) {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  if (seconds < 3600) return `${Math.floor(seconds / 60)}:${pad(seconds % 60)}`;
  return `${Math.floor(seconds / 3600)}:${pad(Math.floor((seconds % 3600) / 60))}:${pad(seconds % 60)}`;
}
/** Done and cancelled spans: seconds under a minute, then minutes with two-digit seconds. */
export function formatAgentMonitorSpan(ms: number) {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  return seconds < 60 ? `${seconds}초` : `${Math.floor(seconds / 60)}분 ${pad(seconds % 60)}초`;
}
/** Step durations: one decimal under 10s, otherwise the span format. */
export function formatAgentMonitorStep(ms: number) {
  return ms < 10_000 ? `${(Math.max(0, ms) / 1000).toFixed(1)}초` : formatAgentMonitorSpan(ms);
}
/** Under a tenth of a cent reads "<$0.001", under $1 keeps three decimals, otherwise two. */
export function formatAgentMonitorCost(value: number) {
  if (value > 0 && value < 0.001) return "<$0.001";
  return value < 1 ? `$${value.toFixed(3)}` : `$${value.toFixed(2)}`;
}

export const agentMonitorDefaultLabels: AgentMonitorLabels = {
  agent: "에이전트",
  statusIdle: statusText.idle,
  statusRunning: statusText.running,
  statusWaiting: statusText.waiting,
  statusPaused: statusText.paused,
  statusDone: statusText.done,
  statusFailed: statusText.failed,
  statusCancelled: statusText.cancelled,
  stepPending: stepStatusText.pending,
  stepRunning: stepStatusText.running,
  stepDone: stepStatusText.done,
  stepFailed: stepStatusText.failed,
  kindRead: kindName.read,
  kindSearch: kindName.search,
  kindEdit: kindName.edit,
  kindRun: kindName.run,
  kindThink: kindName.think,
  kindWeb: kindName.web,
  waiting: "확인을 기다리는 중",
  waitingAnnounce: "확인을 기다립니다",
  failed: "실행 중 오류가 발생했습니다",
  failedAnnounce: "오류가 발생했습니다",
  command: (command) => `명령: ${command}`,
  paused: "일시정지됨",
  done: (span) => `완료 · ${span}`,
  files: (count) => `파일 ${count}개`,
  cancelled: (span) => `취소됨 · ${span}까지 실행`,
  idle: "아직 작업 전입니다",
  cancel: "취소",
  deny: "거부",
  approve: "허용",
  retry: "다시 시도",
  pause: "일시정지",
  resume: "재개",
  log: "로그",
  expand: (label) => `${label} — 펼치기`,
  fold: (name) => `${name} 모니터 접기`,
  tools: (count) => `사용한 도구 ${count}개 정보`,
  toolsTitle: (count) => `사용한 도구 ${count}개`,
  close: "닫기",
  closeStep: "단계 상세 닫기",
  factStart: "시작",
  factDuration: "소요",
  factShare: "비중",
  factStatus: "상태",
  output: (output) => `출력: ${output}`,
  request: (name, title) => `${name} 확인 요청: ${title}`,
  cost: "지금까지 든 비용",
  elapsed: "경과 시간",
  timeline: (steps) => `실행한 단계: ${steps}`,
  recent: "완료한 단계",
  stepDetail: (label, time) => `${label}, ${time} — 단계 상세`,
  position: "진행 단계",
  keys: (approve, deny) => approve && deny ? "카드에 초점이 있을 때 Enter는 허용, Esc는 거부입니다." : approve ? "카드에 초점이 있으면 Enter로 허용해요." : deny ? "카드에 초점이 있으면 Esc로 거부해요." : "",
  span: formatAgentMonitorSpan,
  stepTime: formatAgentMonitorStep,
};

function useMedia(query: string) {
  const [matches, setMatches] = useState(() => typeof window !== "undefined" && window.matchMedia?.(query).matches === true);
  useEffect(() => {
    const media = window.matchMedia?.(query);
    if (!media) return;
    const sync = () => setMatches(media.matches);
    sync();
    media.addEventListener("change", sync);
    return () => media.removeEventListener("change", sync);
  }, [query]);
  return matches;
}

/** Keeps removed keyed items for one exit animation, at their old index, without input. */
function usePresence<T extends { key: string }>(items: T[], exitMs: number, reduced: boolean) {
  const committed = useRef<T[]>(items);
  const [leaving, setLeaving] = useState<{ item: T; index: number }[]>([]);
  const keys = new Set(items.map((item) => item.key));
  const removed = reduced ? [] : committed.current
    .map((item, index) => ({ item, index }))
    .filter(({ item }) => !keys.has(item.key) && !leaving.some((entry) => entry.item.key === item.key));
  if (removed.length) setLeaving((current) => [...current.filter((entry) => !keys.has(entry.item.key)), ...removed]);
  useLayoutEffect(() => { committed.current = items; });
  useEffect(() => {
    if (!leaving.length) return;
    const timer = window.setTimeout(() => setLeaving([]), exitMs);
    return () => window.clearTimeout(timer);
  }, [leaving, exitMs]);
  const out: { item: T; leaving: boolean }[] = items.map((item) => ({ item, leaving: false }));
  for (const entry of [...leaving, ...removed]) {
    if (keys.has(entry.item.key)) continue;
    out.splice(Math.min(entry.index, out.length), 0, { item: entry.item, leaving: true });
  }
  return out;
}

function Swap({ id, children, className }: { id: string; children: ReactNode; className: string }) {
  const reduced = useMedia("(prefers-reduced-motion: reduce)");
  const items = usePresence([{ key: id, node: children }], 380, reduced);
  return <span className={`aui-agent-monitor-swap ${className}`}>
    {items.map(({ item, leaving }) => <span key={item.key} className="aui-agent-monitor-swap-item" data-leaving={leaving || undefined} aria-hidden={leaving || undefined} inert={leaving || undefined}>{item.node}</span>)}
  </span>;
}

function KindIcon({ kind, icon, size }: { kind?: AgentMonitorStepKind; icon?: ReactNode; size: number }) {
  if (icon) return <>{icon}</>;
  const Icon = kindIcon[kind ?? "think"];
  return <Icon size={size} strokeWidth={2} aria-hidden="true" />;
}

interface Normal {
  status: AgentMonitorStatus;
  name: string;
  role?: string;
  mark?: ReactNode;
  steps: (AgentMonitorStep & { status: AgentMonitorStepStatus })[];
  current?: { label: string; kind?: AgentMonitorStepKind; icon?: ReactNode };
  approval?: AgentMonitorApproval;
}

function normalize(props: AgentMonitorProps, fallbackName = "에이전트"): Normal {
  const steps = (props.steps ?? []).map((step) => ({
    ...step,
    status: step.status ?? (step.state === "done" ? "done" : step.state === "current" ? "running" : "pending") as AgentMonitorStepStatus,
    tool: step.tool,
  }));
  const approval = props.approval ?? (props.needsApproval ? { title: props.needsApproval } : undefined);
  const legacy = props.status === undefined && (props.step !== undefined || props.needsApproval !== undefined || props.elapsed !== undefined);
  const status = props.status ?? (legacy ? (approval ? "waiting" : "running") : "idle");
  const name = (props.agent?.name ?? props.name ?? "").trim() || fallbackName;
  return {
    status,
    name,
    role: props.agent?.role,
    mark: props.agent?.mark,
    steps,
    current: props.current ?? (props.step ? { label: props.step } : undefined),
    approval,
  };
}

export function AgentMonitor(props: AgentMonitorProps) {
  const { started, ended, pausedMs = 0, now: nowProp, cost, result, error, defaultFolded = false, onPause, onResume, onCancel, onApprove, onDeny, onRetry, onOpenLog, elapsed: legacyElapsed, className, style } = props;
  const text: AgentMonitorLabels = { ...agentMonitorDefaultLabels, ...props.labels };
  const statusWord: Record<AgentMonitorStatus, string> = { idle: text.statusIdle, running: text.statusRunning, waiting: text.statusWaiting, paused: text.statusPaused, done: text.statusDone, failed: text.statusFailed, cancelled: text.statusCancelled };
  const stepWord: Record<AgentMonitorStepStatus, string> = { pending: text.stepPending, running: text.stepRunning, done: text.stepDone, failed: text.stepFailed };
  const kindWord: Record<AgentMonitorStepKind, string> = { read: text.kindRead, search: text.kindSearch, edit: text.kindEdit, run: text.kindRun, think: text.kindThink, web: text.kindWeb };
  const model = normalize(props, text.agent);
  const { status, name, role, mark, steps, approval } = model;
  const uid = useId().replace(/[^a-zA-Z0-9_-]/g, "");
  const reduced = useMedia("(prefers-reduced-motion: reduce)");
  const sheet = useMedia("(max-width: 899px)");
  const coarse = useMedia("(pointer: coarse)");
  const live = status === "running" || status === "waiting";

  const [tick, setTick] = useState(() => Date.now());
  useEffect(() => {
    if (nowProp !== undefined || !live) return;
    setTick(Date.now());
    const timer = window.setInterval(() => setTick(Date.now()), 100);
    return () => window.clearInterval(timer);
  }, [nowProp, live]);
  const clock = nowProp ?? tick;
  const elapsedMs = started !== undefined && Number.isFinite(started) ? Math.max(0, (ended ?? clock) - started - pausedMs) : 0;

  const executed = steps.filter((step) => step.status !== "pending");
  const weightOf = (step: Normal["steps"][number]) => step.status === "running" ? (step.started !== undefined ? Math.max(0, clock - step.started) : 0) : step.duration ?? 0;
  const weights = executed.map((step) => Math.max(weightOf(step), 1));
  const totalWeight = weights.reduce((sum, value) => sum + value, 0);
  const recent = executed.filter((step) => step.status !== "running").reverse();
  const runningStep = steps.find((step) => step.status === "running");
  const position = status === "idle" || !steps.length ? "" : `${Math.min(steps.length, executed.length)}/${steps.length}`;

  // Unique tools by kind or by the identical supplied mark, in first-use order, keeping the last use.
  const tools = useMemo(() => {
    const iconKeys = new Map<unknown, string>();
    const list: { key: string; kind?: AgentMonitorStepKind; icon?: ReactNode; tool: string; step: Normal["steps"][number] }[] = [];
    executed.forEach((step) => {
      let key = `kind:${step.kind ?? "think"}`;
      if (step.icon) {
        if (!iconKeys.has(step.icon)) iconKeys.set(step.icon, `icon:${iconKeys.size}`);
        key = iconKeys.get(step.icon)!;
      }
      const tool = step.tool ?? kindWord[step.kind ?? "think"];
      const found = list.find((item) => item.key === key);
      if (found) { found.step = step; found.tool = tool; } else list.push({ key, kind: step.kind, icon: step.icon, tool, step });
    });
    return list;
  }, [executed]);

  const [folded, setFolded] = useState(defaultFolded);
  const foldButton = useRef<HTMLButtonElement>(null);
  const expandButton = useRef<HTMLButtonElement>(null);
  const pendingFocus = useRef<"fold" | "expand" | null>(null);
  useEffect(() => {
    if (pendingFocus.current === "fold") foldButton.current?.focus();
    if (pendingFocus.current === "expand") expandButton.current?.focus();
    pendingFocus.current = null;
  }, [folded]);

  // Shared highlight between the bar and the recent rows.
  const [hoverId, setHoverId] = useState<string | null>(null);
  const leaveTimer = useRef(0);
  const enterStep = (id: string) => { window.clearTimeout(leaveTimer.current); setHoverId(id); };
  const leaveStep = () => { window.clearTimeout(leaveTimer.current); leaveTimer.current = window.setTimeout(() => setHoverId(null), 70); };
  useEffect(() => () => window.clearTimeout(leaveTimer.current), []);

  // Step detail and tool info share one dialog: a sheet below 900px, an overlay otherwise.
  const [panel, setPanel] = useState<{ kind: "step"; id: string; anchor: "row" | "bar" } | { kind: "tools" } | null>(null);
  const openId = panel?.kind === "step" ? panel.id : null;
  const highlight = hoverId ?? openId;
  const openStep = openId ? executed.find((step) => step.id === openId) : undefined;
  useEffect(() => { if (panel?.kind === "step" && !openStep) setPanel(null); }, [panel, openStep]);
  useEffect(() => { if (folded && panel?.kind === "step") setPanel(null); }, [folded, panel]);

  const rootRef = useRef<HTMLElement>(null);
  const cardRef = useRef<HTMLDivElement>(null);
  const barRef = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const dialogRef = useRef<HTMLDialogElement>(null);
  const rowButtons = useRef(new Map<string, HTMLButtonElement>());
  const toolsButton = useRef<HTMLButtonElement>(null);
  const toolsPointer = useRef("keyboard");
  const [panelTop, setPanelTop] = useState(0);

  const placePanel = useCallback(() => {
    if (!panel || panel.kind !== "step" || !cardRef.current) return;
    const anchor = panel.anchor === "row" ? rowButtons.current.get(panel.id) : barRef.current;
    if (!anchor) return;
    const card = cardRef.current.getBoundingClientRect();
    const rect = anchor.getBoundingClientRect();
    setPanelTop(Math.round(rect.bottom - card.top + 6));
  }, [panel]);
  useLayoutEffect(placePanel, [placePanel]);

  const restoreFocus = useCallback((closing: NonNullable<typeof panel>) => {
    if (closing.kind === "tools") { (toolsButton.current?.isConnected ? toolsButton.current : rootRef.current)?.focus(); return; }
    const row = rowButtons.current.get(closing.id);
    (row?.isConnected ? row : rootRef.current)?.focus();
  }, []);
  const closePanel = useCallback((restore: boolean) => {
    setPanel((current) => {
      if (current && restore) window.setTimeout(() => restoreFocus(current), 0);
      return null;
    });
  }, [restoreFocus]);

  useLayoutEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    if (!panel) { if (dialog.open) dialog.close(); return; }
    let modal = false;
    try { modal = dialog.matches(":modal"); } catch { modal = false; }
    if (dialog.open && modal !== sheet) dialog.close();
    if (!dialog.open) {
      try { if (sheet) dialog.showModal(); else dialog.show(); } catch { dialog.setAttribute("open", ""); }
    }
    dialog.querySelector<HTMLButtonElement>("[data-autofocus]")?.focus({ preventScroll: true });
  }, [panel, sheet]);
  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    const cancel = (event: Event) => { event.preventDefault(); closePanel(true); };
    dialog.addEventListener("cancel", cancel);
    return () => dialog.removeEventListener("cancel", cancel);
  }, [closePanel]);
  useEffect(() => {
    if (!panel) return;
    const outside = (event: PointerEvent) => {
      const dialog = dialogRef.current;
      const target = event.target as Element;
      if (dialog && target === dialog) {
        const rect = dialog.getBoundingClientRect();
        if (event.clientX < rect.left || event.clientX > rect.right || event.clientY < rect.top || event.clientY > rect.bottom) closePanel(true);
        return;
      }
      if (dialog?.contains(target)) return;
      if (target.closest?.("[data-step-target]") && rootRef.current?.contains(target)) return;
      if (panel.kind === "tools" && toolsButton.current?.contains(target)) return;
      closePanel(false);
    };
    document.addEventListener("pointerdown", outside);
    return () => document.removeEventListener("pointerdown", outside);
  }, [panel, closePanel]);

  const toggleStep = (id: string, anchor: "row" | "bar") => setPanel((current) => current?.kind === "step" && current.id === id ? null : { kind: "step", id, anchor });

  // Bar hover reveals the matching row inside the list only.
  const revealRow = (id: string) => {
    const list = listRef.current;
    const row = rowButtons.current.get(id)?.parentElement;
    if (!list || !row) return;
    const top = row.offsetTop;
    if (top >= list.scrollTop && top + row.offsetHeight <= list.scrollTop + list.clientHeight) return;
    list.scrollTo({ top: top - (list.clientHeight - row.offsetHeight) / 2, behavior: reduced ? "auto" : "smooth" });
  };
  const [fade, setFade] = useState({ top: false, bottom: false });
  const syncFade = useCallback(() => {
    const list = listRef.current;
    if (!list) return;
    const next = { top: list.scrollTop > 2, bottom: list.scrollHeight - list.clientHeight - list.scrollTop > 2 };
    setFade((current) => current.top === next.top && current.bottom === next.bottom ? current : next);
  }, []);
  useLayoutEffect(syncFade, [syncFade, recent.length, folded]);

  const initialRows = useRef(new Set(recent.map((step) => step.id)));

  const [announcement, setAnnouncement] = useState("");
  const announceKey = `${status}|${approval?.title ?? ""}|${approval?.command ?? ""}|${status === "done" ? JSON.stringify(result ?? {}) : ""}|${status === "failed" ? error ?? "" : ""}`;
  const firstAnnounce = useRef(true);
  useEffect(() => {
    if (firstAnnounce.current) { firstAnnounce.current = false; return; }
    setAnnouncement(`${name} ${statusWord[status]}${status === "waiting" ? `: ${approval?.title ?? text.waitingAnnounce}` : ""}${status === "failed" ? `: ${error ?? text.failedAnnounce}` : ""}${status === "done" ? `, ${text.span(elapsedMs)}` : ""}`);
    // Only status, request and result changes are announced, never the clock.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [announceKey]);

  const costShown = cost !== undefined && status !== "idle";
  const costLabel = typeof cost === "string" ? cost : cost !== undefined ? formatAgentMonitorCost(cost) : "";
  const clockShown = legacyElapsed !== undefined || (started !== undefined && (status === "running" || status === "waiting" || status === "paused" || status === "failed"));
  const clockLabel = legacyElapsed ?? formatAgentMonitorClock(elapsedMs);
  const runningTask = model.current ?? (runningStep ? { label: runningStep.label, kind: runningStep.kind, icon: runningStep.icon } : undefined);
  const initial = Array.from(name)[0]?.toUpperCase() ?? "A";
  const label = `${name}, ${statusWord[status]}${status === "running" && runningTask ? `, ${runningTask.label}` : ""}${status === "waiting" && approval ? `, ${approval.title}` : ""}`;

  const added = result?.added;
  const removed = result?.removed;
  const hasDiff = added !== undefined || removed !== undefined;
  const wellTone = status === "waiting" ? "waiting" : status === "failed" ? "failed" : status === "done" ? "done" : status === "running" && runningTask ? "running" : "neutral";
  const wellKey = status === "running" ? `running:${runningTask?.label ?? ""}` : status === "waiting" ? `waiting:${approval?.title ?? ""}:${approval?.command ?? ""}` : status === "failed" ? `failed:${error ?? ""}` : status;
  const wellContent = (() => {
    if (status === "running" && runningTask) return <span className="aui-agent-monitor-well-line">
      <span className="aui-agent-monitor-well-icon" aria-hidden="true"><KindIcon kind={runningTask.kind} icon={runningTask.icon} size={15} /></span>
      <span className="aui-agent-monitor-task" data-live title={runningTask.label} dir="auto">{runningTask.label}</span>
    </span>;
    if (status === "waiting") return <span className="aui-agent-monitor-well-line">
      <span className="aui-agent-monitor-well-icon" data-tone="waiting" aria-hidden="true">{approval?.icon ?? <TriangleAlert size={15} strokeWidth={2} />}</span>
      <span className="aui-agent-monitor-ask">
        <span className="aui-agent-monitor-task" title={approval?.title ?? text.waiting} dir="auto">{approval?.title ?? text.waiting}</span>
        {approval?.command && <code className="aui-agent-monitor-command" tabIndex={coarse ? undefined : 0} title={approval.command} aria-label={text.command(approval.command)}>{approval.command}</code>}
      </span>
    </span>;
    if (status === "paused") return <span className="aui-agent-monitor-well-line">
      <span className="aui-agent-monitor-well-icon" aria-hidden="true"><Pause size={15} strokeWidth={2} /></span>
      <span className="aui-agent-monitor-task" title={runningTask?.label} dir="auto"><span className="aui-agent-monitor-muted">{text.paused}</span>{runningTask ? ` · ${runningTask.label}` : ""}</span>
    </span>;
    if (status === "done") return <span className="aui-agent-monitor-well-line aui-agent-monitor-result">
      <span className="aui-agent-monitor-well-icon" data-tone="done" aria-hidden="true"><Check size={15} strokeWidth={2.4} /></span>
      <span className="aui-agent-monitor-task">{text.done(text.span(elapsedMs))}</span>
      {hasDiff && <span className="aui-agent-monitor-diff"><span data-tone="add">+{(added ?? 0).toLocaleString("en-US")}</span><span data-tone="remove">−{(removed ?? 0).toLocaleString("en-US")}</span></span>}
      {result?.files !== undefined && <span className="aui-agent-monitor-muted aui-agent-monitor-nowrap">{text.files(result.files)}</span>}
      {result?.note && <span className="aui-agent-monitor-muted aui-agent-monitor-note" title={result.note}>{result.note}</span>}
    </span>;
    if (status === "failed") return <span className="aui-agent-monitor-well-line">
      <span className="aui-agent-monitor-fail-dot" aria-hidden="true" />
      <span className="aui-agent-monitor-task" title={error ?? text.failed} dir="auto">{error ?? text.failed}</span>
    </span>;
    if (status === "cancelled") return <span className="aui-agent-monitor-well-line"><span className="aui-agent-monitor-task aui-agent-monitor-muted">{text.cancelled(text.span(elapsedMs))}</span></span>;
    return <span className="aui-agent-monitor-well-line"><span className="aui-agent-monitor-idle">{text.idle}</span></span>;
  })();

  const onCardKey = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    if (event.target !== event.currentTarget || status !== "waiting") return;
    if (event.key === "Enter" && onApprove) { event.preventDefault(); onApprove(); }
    if (event.key === "Escape" && onDeny && !panel) { event.preventDefault(); onDeny(); }
  };
  const onRootKey = (event: ReactKeyboardEvent<HTMLElement>) => {
    if (event.key === "Escape" && panel) { event.preventDefault(); event.stopPropagation(); closePanel(true); }
  };

  type Action = { key: string; node: ReactNode };
  const actions: Action[] = [];
  if (status === "running" && onCancel) actions.push({ key: "cancel", node: <button type="button" className="aui-agent-monitor-action" onClick={onCancel}><X size={14} aria-hidden="true" />{text.cancel}</button> });
  if (status === "paused" && onCancel) actions.push({ key: "cancel", node: <button type="button" className="aui-agent-monitor-action" onClick={onCancel}><X size={14} aria-hidden="true" />{text.cancel}</button> });
  if (status === "waiting" && onDeny) actions.push({ key: "deny", node: <button type="button" className="aui-agent-monitor-action" onClick={onDeny} aria-describedby={`${uid}-keys`}>{text.deny}<kbd className="aui-agent-monitor-key" aria-hidden="true">Esc</kbd></button> });
  if (status === "waiting" && onApprove) actions.push({ key: "approve", node: <button type="button" className="aui-agent-monitor-action" data-tone="primary" onClick={onApprove} aria-describedby={`${uid}-keys`}><Check size={14} aria-hidden="true" />{text.approve}<kbd className="aui-agent-monitor-key" aria-hidden="true">↵</kbd></button> });
  if (status === "failed" && onRetry) actions.push({ key: "retry", node: <button type="button" className="aui-agent-monitor-action" data-tone="primary" onClick={onRetry}><RotateCcw size={14} aria-hidden="true" />{text.retry}</button> });
  const shownActions = usePresence(actions, 240, reduced);
  const toggleAction = status === "running" && onPause ? "pause" : status === "paused" && onResume ? "resume" : null;

  if (folded) {
    const visibleTools = tools.slice(-4);
    const extra = tools.length - visibleTools.length;
    const activeKey = status === "running" && runningStep ? tools.find((item) => item.step.id === runningStep.id)?.key : undefined;
    const endKey = status;
    const end = status === "done" ? <span className="aui-agent-monitor-end-badge" data-tone="done"><Check size={13} strokeWidth={2.4} aria-hidden="true" />{text.span(elapsedMs)}</span>
      : status === "waiting" ? <span className="aui-agent-monitor-end-badge" data-tone="waiting"><TriangleAlert size={13} aria-hidden="true" />{statusWord.waiting}</span>
      : status === "failed" ? <span className="aui-agent-monitor-end-badge" data-tone="failed"><X size={13} strokeWidth={2.4} aria-hidden="true" />{statusWord.failed}</span>
      : status === "running" || status === "paused" ? <span className="aui-agent-monitor-end-clock" data-live={status === "running" || undefined}>{clockLabel}</span>
      : <span className="aui-agent-monitor-end-text">{statusWord[status]}</span>;
    return <section ref={rootRef} tabIndex={-1} className={["aui-agent-monitor", className].filter(Boolean).join(" ")} style={style} data-folded aria-label={label} onKeyDown={onRootKey}>
      <style>{agentMonitorStyles}</style>
      <div className="aui-agent-monitor-bar-folded" data-status={status}>
        <button ref={expandButton} type="button" className="aui-agent-monitor-expand" aria-expanded={false} aria-controls={`${uid}-body`} aria-label={text.expand(label)} onClick={() => { pendingFocus.current = "fold"; setFolded(false); }} />
        <span className="aui-agent-monitor-chip" aria-hidden="true">
          <span className="aui-agent-monitor-mark" data-size="small">{mark ?? initial}</span>
          <span className="aui-agent-monitor-chip-name">{name}</span>
        </span>
        {(visibleTools.length > 0 || (status === "waiting" && approval)) && <button
          ref={toolsButton}
          type="button"
          className="aui-agent-monitor-tools"
          aria-label={text.tools(tools.length)}
          aria-haspopup="dialog"
          aria-expanded={panel?.kind === "tools"}
          onPointerDown={(event) => { toolsPointer.current = event.pointerType; }}
          onClick={() => {
            const pointer = toolsPointer.current;
            toolsPointer.current = "keyboard";
            if (pointer === "mouse") { pendingFocus.current = "fold"; setFolded(false); return; }
            setPanel((current) => current?.kind === "tools" ? null : { kind: "tools" });
          }}
        >
          {extra > 0 && <span className="aui-agent-monitor-tool-more">+{extra}</span>}
          {visibleTools.map((item) => {
            const active = status === "done" || item.key === activeKey;
            return <span key={item.key} className="aui-agent-monitor-tile" data-active={active || undefined} onPointerEnter={(event) => { if (event.pointerType === "mouse") setHoverId(`tool:${item.key}`); }} onPointerLeave={(event) => { if (event.pointerType === "mouse") setHoverId(null); }}>
              <KindIcon kind={item.kind} icon={item.icon} size={13} />
              {hoverId === `tool:${item.key}` && <span className="aui-agent-monitor-tip" role="tooltip">
                <span className="aui-agent-monitor-tip-title">{item.step.label}</span>
                <span className="aui-agent-monitor-tip-sub">{item.tool} · {item.step.status === "failed" ? stepWord.failed : item.step.status === "running" ? stepWord.running : text.stepTime(weightOf(item.step))}</span>
              </span>}
            </span>;
          })}
          {status === "waiting" && approval && <span className="aui-agent-monitor-tile" data-tone="waiting" data-active>{approval.icon ?? <TriangleAlert size={13} />}</span>}
        </button>}
        <Swap id={endKey} className="aui-agent-monitor-end">{end}</Swap>
      </div>
      {renderDialog()}
    </section>;
  }

  function renderDialog() {
    const isTools = panel?.kind === "tools";
    const step = openStep;
    const index = step ? executed.indexOf(step) : -1;
    const weight = index >= 0 ? weights[index] : 0;
    const startOffset = step ? (step.started !== undefined && started !== undefined ? Math.max(0, step.started - started) : executed.slice(0, index).reduce((sum, item) => sum + (item.duration ?? 0), 0)) : 0;
    return <dialog
      ref={dialogRef}
      id={`${uid}-panel`}
      className="aui-agent-monitor-panel"
      data-kind={panel?.kind}
      aria-labelledby={`${uid}-panel-title`}
      style={{ "--aui-agent-monitor-panel-top": `${panelTop}px` } as CSSProperties}
      onKeyDown={(event) => { if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); closePanel(true); } }}
    >
      {isTools && <>
        <div className="aui-agent-monitor-panel-head">
          <div className="aui-agent-monitor-panel-heading"><h4 id={`${uid}-panel-title`} className="aui-agent-monitor-panel-title">{text.toolsTitle(tools.length)}</h4></div>
          <button type="button" className="aui-agent-monitor-close" aria-label={text.close} data-autofocus onClick={() => closePanel(true)}><X size={14} aria-hidden="true" /></button>
        </div>
        <ul className="aui-agent-monitor-tool-list">
          {tools.map((item) => <li key={item.key}>
            <span className="aui-agent-monitor-row-icon" aria-hidden="true"><KindIcon kind={item.kind} icon={item.icon} size={12} /></span>
            <span className="aui-agent-monitor-tool-text"><strong>{item.tool}</strong><span>{item.step.label}</span></span>
            <span className="aui-agent-monitor-row-time" data-failed={item.step.status === "failed" || undefined}>{item.step.status === "failed" ? stepWord.failed : item.step.status === "running" ? stepWord.running : text.stepTime(weightOf(item.step))}</span>
          </li>)}
        </ul>
      </>}
      {step && <>
        <div className="aui-agent-monitor-panel-head">
          <div className="aui-agent-monitor-panel-heading">
            <h4 id={`${uid}-panel-title`} className="aui-agent-monitor-panel-title" title={step.label} dir="auto">{step.label}</h4>
            <span className="aui-agent-monitor-panel-tool">{step.tool ?? kindWord[step.kind ?? "think"]}</span>
          </div>
          <button type="button" className="aui-agent-monitor-close" aria-label={text.closeStep} data-autofocus onClick={() => closePanel(true)}><X size={14} aria-hidden="true" /></button>
        </div>
        <dl className="aui-agent-monitor-facts">
          <div><dt>{text.factStart}</dt><dd>{formatAgentMonitorClock(startOffset)}</dd></div>
          <div><dt>{text.factDuration}</dt><dd data-tone={step.status}>{text.stepTime(weightOf(step))}</dd></div>
          <div><dt>{text.factShare}</dt><dd>{Math.max(1, Math.round((weight / Math.max(totalWeight, 1)) * 100))}%</dd></div>
          <div><dt>{text.factStatus}</dt><dd data-tone={step.status}>{stepWord[step.status]}</dd></div>
        </dl>
        {step.description && <p className="aui-agent-monitor-panel-text" dir="auto">{step.description}</p>}
        {step.output && <code className="aui-agent-monitor-output" tabIndex={coarse ? undefined : 0} title={step.output} aria-label={text.output(step.output)}>{step.output}</code>}
      </>}
    </dialog>;
  }

  return <section
    ref={rootRef}
    tabIndex={-1}
    className={["aui-agent-monitor", className].filter(Boolean).join(" ")}
    style={style}
    aria-label={label}
    data-status={status}
    onKeyDown={onRootKey}
  >
    <style>{agentMonitorStyles}</style>
    <div className="aui-agent-monitor-tray" id={`${uid}-body`}>
      <div
        ref={cardRef}
        className="aui-agent-monitor-card"
        tabIndex={status === "waiting" && (onApprove || onDeny) ? 0 : undefined}
        aria-label={status === "waiting" ? text.request(name, approval?.title ?? text.waiting) : undefined}
        aria-describedby={status === "waiting" && (onApprove || onDeny) ? `${uid}-keys` : undefined}
        role={status === "waiting" && (onApprove || onDeny) ? "group" : undefined}
        onKeyDown={onCardKey}
      >
        <header className="aui-agent-monitor-head">
          <span className="aui-agent-monitor-who">
            <span className="aui-agent-monitor-mark" aria-hidden="true">{mark ?? initial}</span>
            <span className="aui-agent-monitor-names">
              <span className="aui-agent-monitor-name" title={name} dir="auto">{name}</span>
              {role && <span className="aui-agent-monitor-role" title={role} dir="auto">{role}</span>}
            </span>
          </span>
          <span className="aui-agent-monitor-head-end">
            {costShown && <span className="aui-agent-monitor-cost" data-live={live || undefined} title={text.cost}><span className="aui-agent-monitor-sr">{text.cost} </span>{costLabel}</span>}
            <span className="aui-agent-monitor-pill" data-status={status}>
              <span className="aui-agent-monitor-pill-dot" aria-hidden="true" />
              <Swap id={status} className="aui-agent-monitor-pill-text">{statusWord[status]}</Swap>
            </span>
            <button ref={foldButton} type="button" className="aui-agent-monitor-fold" aria-expanded={true} aria-controls={`${uid}-body`} aria-label={text.fold(name)} onClick={() => { pendingFocus.current = "expand"; setFolded(true); }}><ChevronsUpDown size={12} strokeWidth={2.2} aria-hidden="true" /></button>
          </span>
        </header>
        <div className="aui-agent-monitor-well" data-tone={wellTone}>
          <Swap id={wellKey} className="aui-agent-monitor-well-swap">{wellContent}</Swap>
          {clockShown && <span className="aui-agent-monitor-clock" data-paused={!live || undefined} title={text.elapsed}><span className="aui-agent-monitor-sr">{text.elapsed} </span>{clockLabel}</span>}
        </div>
        {executed.length > 0 && <div
          ref={barRef}
          className="aui-agent-monitor-timeline"
          role="img"
          aria-label={text.timeline(executed.map((step) => `${step.label} ${step.status === "failed" ? stepWord.failed : text.stepTime(weightOf(step))}`).join(", "))}
          data-complete={status === "done" || undefined}
        >
          {executed.map((step, index) => <span
            key={step.id}
            className="aui-agent-monitor-seg"
            data-step-target
            data-status={step.status}
            data-paused={status === "paused" || undefined}
            data-live={status === "running" || undefined}
            data-highlight={highlight === step.id || undefined}
            data-dim={!!highlight && !highlight.startsWith("tool:") && highlight !== step.id || undefined}
            style={{ flexGrow: weights[index] / Math.max(totalWeight, 1) * 1000 }}
            onPointerEnter={(event) => { if (event.pointerType === "mouse") { enterStep(step.id); revealRow(step.id); } }}
            onPointerLeave={(event) => { if (event.pointerType === "mouse") leaveStep(); }}
            onClick={() => toggleStep(step.id, "bar")}
          />)}
          {status === "waiting" && <span className="aui-agent-monitor-seg" data-status="waiting-end" aria-hidden="true" />}
        </div>}
        {recent.length > 0 && <div
          ref={listRef}
          className="aui-agent-monitor-recent"
          role={recent.length > 3 ? "region" : undefined}
          aria-label={text.recent}
          tabIndex={recent.length > 3 ? 0 : undefined}
          data-fade-top={fade.top || undefined}
          data-fade-bottom={fade.bottom || undefined}
          style={{ "--aui-agent-monitor-rows": Math.min(recent.length, 3) } as CSSProperties}
          onScroll={syncFade}
        >
          <ul className="aui-agent-monitor-rows">
            {recent.map((step) => <li key={step.id} className="aui-agent-monitor-row-item" data-enter={!initialRows.current.has(step.id) || undefined}>
              <button
                ref={(element) => { if (element) rowButtons.current.set(step.id, element); else rowButtons.current.delete(step.id); }}
                type="button"
                className="aui-agent-monitor-row"
                data-step-target
                data-highlight={highlight === step.id || undefined}
                data-dim={!!highlight && !highlight.startsWith("tool:") && highlight !== step.id || undefined}
                aria-expanded={openId === step.id}
                aria-controls={`${uid}-panel`}
                aria-label={text.stepDetail(step.label, step.status === "failed" ? stepWord.failed : text.stepTime(weightOf(step)))}
                title={step.label}
                onPointerEnter={(event) => { if (event.pointerType === "mouse") enterStep(step.id); }}
                onPointerLeave={(event) => { if (event.pointerType === "mouse") leaveStep(); }}
                onClick={() => toggleStep(step.id, "row")}
              >
                <span className="aui-agent-monitor-row-icon" aria-hidden="true"><KindIcon kind={step.kind} icon={step.icon} size={12} /></span>
                <span className="aui-agent-monitor-row-label" dir="auto">{step.label}</span>
                <span className="aui-agent-monitor-row-time" data-failed={step.status === "failed" || undefined}>{step.status === "failed" ? stepWord.failed : text.stepTime(weightOf(step))}</span>
              </button>
            </li>)}
          </ul>
        </div>}
        {renderDialog()}
      </div>
      <footer className="aui-agent-monitor-foot">
        <span className="aui-agent-monitor-actions">
          {toggleAction && <button type="button" className="aui-agent-monitor-action" data-tone={toggleAction === "resume" ? "primary" : undefined} onClick={toggleAction === "pause" ? onPause : onResume}>
            <Swap id={toggleAction} className="aui-agent-monitor-action-swap">{toggleAction === "pause" ? <><Pause size={14} aria-hidden="true" />{text.pause}</> : <><Play size={14} aria-hidden="true" />{text.resume}</>}</Swap>
          </button>}
          {shownActions.map(({ item, leaving }) => <span key={item.key} className="aui-agent-monitor-action-slot" data-leaving={leaving || undefined} inert={leaving || undefined} aria-hidden={leaving || undefined}>{item.node}</span>)}
        </span>
        {position && <span className="aui-agent-monitor-position"><span className="aui-agent-monitor-sr">{text.position} </span>{position}</span>}
        {onOpenLog && <button type="button" className="aui-agent-monitor-log" onClick={onOpenLog}>{text.log}<ArrowUpRight size={14} aria-hidden="true" /></button>}
      </footer>
    </div>
    <span id={`${uid}-keys`} className="aui-agent-monitor-sr">{text.keys(!!onApprove, !!onDeny)}</span>
    <span className="aui-agent-monitor-sr" role="status" aria-live="polite" aria-atomic="true">{announcement}</span>
  </section>;
}

const agentMonitorStyles = `
.aui-agent-monitor {
  --aui-agent-monitor-tray: var(--aui-agent-monitor-tray-bg, var(--aui-bg-2, #f0f0f0));
  --aui-agent-monitor-card: var(--aui-agent-monitor-card-bg, var(--aui-bg-1, #ffffff));
  --aui-agent-monitor-ink: var(--aui-fg, #171717);
  --aui-agent-monitor-text: var(--aui-fg-dim, #6b6b6b);
  --aui-agent-monitor-muted: var(--aui-fg-muted, #8a8a8a);
  --aui-agent-monitor-faint: var(--aui-fg-faint, #b8b8b8);
  --aui-agent-monitor-line: var(--aui-line, #ececec);
  --aui-agent-monitor-well: var(--aui-agent-monitor-well-bg, var(--aui-bg-3, #f5f5f5));
  --aui-agent-monitor-log-hover: var(--aui-agent-monitor-log-hover-bg, #e4e4e5);
  --aui-agent-monitor-run: var(--aui-agent-monitor-run-fg, var(--aui-accent, #4fb0fe));
  --aui-agent-monitor-run-bg: var(--aui-agent-monitor-run-soft, rgb(1 144 254 / .08));
  --aui-agent-monitor-ok: var(--aui-agent-monitor-ok-fg, var(--aui-ok, #1e8a4f));
  --aui-agent-monitor-ok-bg: var(--aui-agent-monitor-ok-soft, rgb(95 200 138 / .1));
  --aui-agent-monitor-err: var(--aui-agent-monitor-err-fg, var(--aui-err, #f04438));
  --aui-agent-monitor-err-bg: var(--aui-agent-monitor-err-soft, rgb(240 68 56 / .08));
  --aui-agent-monitor-warn: var(--aui-agent-monitor-warn-fg, #b25e00);
  --aui-agent-monitor-warn-bg: var(--aui-agent-monitor-warn-soft, rgb(232 176 0 / .14));
  --aui-agent-monitor-seg: var(--aui-agent-monitor-seg-bg, #e6e6e7);
  --aui-agent-monitor-seg-strong: var(--aui-agent-monitor-seg-highlight, #a3a3a8);
  --aui-agent-monitor-seg-live: var(--aui-agent-monitor-seg-running, rgb(47 107 255 / .3));
  --aui-agent-monitor-seg-shine: rgb(255 255 255 / .75);
  --aui-agent-monitor-glow: rgb(23 23 23 / .25);
  --aui-agent-monitor-primary: var(--aui-agent-monitor-primary-bg, var(--aui-fg, #171717));
  --aui-agent-monitor-primary-fg: var(--aui-agent-monitor-primary-text, #ffffff);
  --aui-agent-monitor-key-bg: rgb(255 255 255 / .16);
  --aui-agent-monitor-key-fg: rgb(255 255 255 / .8);
  --aui-agent-monitor-shadow: var(--aui-agent-monitor-card-shadow, 0 0 0 1px var(--aui-agent-monitor-line), 0 1px 2px rgb(0 0 0 / .04));
  --aui-agent-monitor-pop-shadow: var(--aui-agent-monitor-popover-shadow, 0 0 0 1px var(--aui-agent-monitor-line), 0 12px 24px -8px rgb(0 0 0 / .16), 0 4px 8px -4px rgb(0 0 0 / .06));
  --aui-agent-monitor-focus: var(--aui-agent-monitor-focus-ring, var(--aui-focus, rgb(0 96 226 / .7)));
  --aui-agent-monitor-ease: ${MOVE};
  --aui-agent-monitor-row-h: 30px;
  position: relative; box-sizing: border-box; display: block; width: 372px; max-width: 100%; min-width: 0; container: aui-agent-monitor / inline-size;
  color: var(--aui-agent-monitor-text); font: 14px/20px var(--aui-font-sans, Inter, ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif); letter-spacing: -0.006em; font-variant-numeric: tabular-nums;
  word-break: keep-all; overflow-wrap: anywhere; -webkit-tap-highlight-color: transparent; outline: none;
}
:where(.dark, [data-theme="dark"]) .aui-agent-monitor {
  --aui-agent-monitor-tray: var(--aui-agent-monitor-tray-bg, var(--aui-bg-2, #111113));
  --aui-agent-monitor-card: var(--aui-agent-monitor-card-bg, var(--aui-bg-1, #19191b));
  --aui-agent-monitor-ink: var(--aui-fg, #ededed);
  --aui-agent-monitor-text: var(--aui-fg-dim, #a1a1a1);
  --aui-agent-monitor-muted: var(--aui-fg-muted, #8b8b92);
  --aui-agent-monitor-faint: var(--aui-fg-faint, #5c5c63);
  --aui-agent-monitor-line: var(--aui-line, #2a2a2e);
  --aui-agent-monitor-well: var(--aui-agent-monitor-well-bg, var(--aui-bg-3, #232326));
  --aui-agent-monitor-log-hover: var(--aui-agent-monitor-log-hover-bg, #2a2a2e);
  --aui-agent-monitor-run-bg: var(--aui-agent-monitor-run-soft, rgb(79 176 254 / .14));
  --aui-agent-monitor-ok: var(--aui-agent-monitor-ok-fg, var(--aui-ok, #5fc88a));
  --aui-agent-monitor-ok-bg: var(--aui-agent-monitor-ok-soft, rgb(125 207 159 / .13));
  --aui-agent-monitor-err: var(--aui-agent-monitor-err-fg, var(--aui-err, #ff6b5f));
  --aui-agent-monitor-err-bg: var(--aui-agent-monitor-err-soft, rgb(255 107 95 / .12));
  --aui-agent-monitor-warn: var(--aui-agent-monitor-warn-fg, #e6c24d);
  --aui-agent-monitor-warn-bg: var(--aui-agent-monitor-warn-soft, rgb(230 194 77 / .14));
  --aui-agent-monitor-seg: var(--aui-agent-monitor-seg-bg, #2c2c30);
  --aui-agent-monitor-seg-strong: var(--aui-agent-monitor-seg-highlight, #6b6b72);
  --aui-agent-monitor-seg-live: var(--aui-agent-monitor-seg-running, rgb(91 140 255 / .3));
  --aui-agent-monitor-seg-shine: rgb(255 255 255 / .22);
  --aui-agent-monitor-glow: rgb(255 255 255 / .95);
  --aui-agent-monitor-primary: var(--aui-agent-monitor-primary-bg, var(--aui-fg, #ededed));
  --aui-agent-monitor-primary-fg: var(--aui-agent-monitor-primary-text, #111113);
  --aui-agent-monitor-key-bg: rgb(0 0 0 / .1);
  --aui-agent-monitor-key-fg: rgb(0 0 0 / .6);
  --aui-agent-monitor-shadow: var(--aui-agent-monitor-card-shadow, 0 0 0 1px var(--aui-agent-monitor-line), 0 1px 2px rgb(0 0 0 / .3));
  --aui-agent-monitor-pop-shadow: var(--aui-agent-monitor-popover-shadow, 0 0 0 1px var(--aui-agent-monitor-line), 0 12px 24px -8px rgb(0 0 0 / .5), 0 4px 8px -4px rgb(0 0 0 / .3));
  --aui-agent-monitor-focus: var(--aui-agent-monitor-focus-ring, var(--aui-focus, rgb(77 141 255 / .8)));
}
.aui-agent-monitor *, .aui-agent-monitor *::before, .aui-agent-monitor *::after { box-sizing: border-box; }
.aui-agent-monitor :where(button) { font: inherit; letter-spacing: inherit; color: inherit; cursor: pointer; touch-action: manipulation; -webkit-tap-highlight-color: transparent; }
.aui-agent-monitor :is(button, [tabindex="0"]):focus-visible { outline: 2px solid var(--aui-agent-monitor-focus); outline-offset: 2px; }
.aui-agent-monitor .aui-agent-monitor-row:focus-visible { outline-offset: -2px; }
.aui-agent-monitor-sr { position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px; overflow: hidden; clip-path: inset(50%); white-space: nowrap; border: 0; }
.aui-agent-monitor-tray { position: relative; padding: 4px; border-radius: 20px; background: var(--aui-agent-monitor-tray); animation: aui-agent-monitor-in 340ms var(--aui-agent-monitor-ease) both; transform-origin: 24px 24px; }
.aui-agent-monitor-card { position: relative; display: grid; gap: 12px; padding: 14px 14px 12px; border-radius: 16px; background: var(--aui-agent-monitor-card); box-shadow: var(--aui-agent-monitor-shadow); outline: none; }
.aui-agent-monitor-head { display: flex; align-items: center; gap: 8px; min-width: 0; }
.aui-agent-monitor-who { display: flex; align-items: center; gap: 8px; flex: 1 1 auto; min-width: 3em; }
.aui-agent-monitor-mark { display: inline-grid; flex: none; place-items: center; width: 30px; height: 30px; margin-right: 2px; overflow: hidden; border-radius: 9px; background: var(--aui-agent-monitor-card); box-shadow: inset 0 0 0 1px var(--aui-agent-monitor-line); color: var(--aui-agent-monitor-ink); font-size: 12px; font-weight: 600; line-height: 1; }
.aui-agent-monitor-mark svg { width: 16px; height: 16px; }
.aui-agent-monitor-mark img { width: 100%; height: 100%; object-fit: cover; }
.aui-agent-monitor-mark[data-size="small"] { width: 30px; height: 30px; margin: 0; }
.aui-agent-monitor-names { display: grid; min-width: 0; }
.aui-agent-monitor-name { overflow: hidden; color: var(--aui-agent-monitor-ink); font-size: 14px; font-weight: 600; line-height: 18px; text-overflow: ellipsis; white-space: nowrap; }
.aui-agent-monitor-role { overflow: hidden; color: var(--aui-agent-monitor-muted); font-size: 12.5px; line-height: 17px; text-overflow: ellipsis; white-space: nowrap; }
.aui-agent-monitor-head-end { display: flex; flex: none; align-items: center; gap: 8px; }
.aui-agent-monitor-cost { flex: none; color: var(--aui-agent-monitor-ink); font-size: 12.5px; font-weight: 500; white-space: nowrap; }
.aui-agent-monitor :is(.aui-agent-monitor-cost, .aui-agent-monitor-task, .aui-agent-monitor-end-clock)[data-live] { color: transparent; background: linear-gradient(100deg, var(--aui-agent-monitor-ink) 40%, var(--aui-agent-monitor-glow) 50%, var(--aui-agent-monitor-ink) 60%) 0 0 / 300% 100%; -webkit-background-clip: text; background-clip: text; animation: aui-agent-monitor-shine 1500ms linear infinite; }
.aui-agent-monitor-pill { position: relative; display: inline-flex; flex: none; align-items: center; gap: 6px; height: 26px; padding: 0 10px 0 9px; border-radius: 999px; background: var(--aui-agent-monitor-well); color: var(--aui-agent-monitor-text); font-size: 12.5px; font-weight: 500; white-space: nowrap; transition: background-color 300ms ease, color 300ms ease; }
.aui-agent-monitor-pill[data-status="running"] { background: var(--aui-agent-monitor-run-bg); color: var(--aui-agent-monitor-run); }
.aui-agent-monitor-pill[data-status="waiting"] { background: var(--aui-agent-monitor-warn-bg); color: var(--aui-agent-monitor-warn); }
.aui-agent-monitor-pill[data-status="done"] { background: var(--aui-agent-monitor-ok-bg); color: var(--aui-agent-monitor-ok); }
.aui-agent-monitor-pill[data-status="failed"] { background: var(--aui-agent-monitor-err-bg); color: var(--aui-agent-monitor-err); }
.aui-agent-monitor-pill-dot { position: relative; width: 6px; height: 6px; border-radius: 50%; background: currentColor; opacity: .6; }
.aui-agent-monitor-pill:is([data-status="running"], [data-status="waiting"]) .aui-agent-monitor-pill-dot { opacity: 1; }
.aui-agent-monitor-pill:is([data-status="running"], [data-status="waiting"]) .aui-agent-monitor-pill-dot::after { position: absolute; inset: -3px; border-radius: 50%; box-shadow: 0 0 0 3px currentColor; content: ""; opacity: 0; animation: aui-agent-monitor-pulse 1600ms ease-out infinite; }
.aui-agent-monitor-swap { display: inline-grid; min-width: 0; }
.aui-agent-monitor-swap-item { grid-area: 1 / 1; min-width: 0; animation: aui-agent-monitor-swap-in 380ms var(--aui-agent-monitor-ease) both; }
.aui-agent-monitor-swap-item[data-leaving] { animation: aui-agent-monitor-swap-out 380ms var(--aui-agent-monitor-ease) both; pointer-events: none; }
.aui-agent-monitor-fold { display: inline-grid; flex: none; place-items: center; width: 28px; height: 28px; margin-right: -4px; padding: 0; border: 0; border-radius: 9px; background: transparent; box-shadow: inset 0 0 0 1px var(--aui-agent-monitor-line); color: var(--aui-agent-monitor-muted); transition: background-color 160ms ease, color 160ms ease, transform 160ms ease; }
.aui-agent-monitor-fold:hover { background: var(--aui-agent-monitor-well); color: var(--aui-agent-monitor-ink); }
.aui-agent-monitor :is(.aui-agent-monitor-fold, .aui-agent-monitor-action, .aui-agent-monitor-close, .aui-agent-monitor-log):active { transform: scale(.96); }
.aui-agent-monitor-well { display: flex; align-items: center; gap: 10px; min-width: 0; height: 44px; padding: 0 12px 0 8px; border-radius: 12px; background: var(--aui-agent-monitor-well); color: var(--aui-agent-monitor-ink); font-size: 13px; transition: background-color 300ms ease, color 300ms ease; }
.aui-agent-monitor-well:is([data-tone="neutral"], [data-tone="failed"]) { padding-left: 12px; }
.aui-agent-monitor-well[data-tone="waiting"] { background: var(--aui-agent-monitor-warn-bg); color: var(--aui-agent-monitor-warn); }
.aui-agent-monitor-well-swap { display: grid; grid-template-columns: minmax(0, 1fr); flex: 1 1 auto; min-width: 0; }
.aui-agent-monitor-well-line { display: flex; align-items: center; gap: 8px; min-width: 0; }
.aui-agent-monitor-well-icon { display: inline-grid; flex: none; place-items: center; width: 28px; height: 28px; border-radius: 8px; background: var(--aui-agent-monitor-card); box-shadow: var(--aui-agent-monitor-shadow); color: var(--aui-agent-monitor-text); }
.aui-agent-monitor-well-icon svg { width: 15px; height: 15px; }
.aui-agent-monitor-well-icon[data-tone="waiting"] { color: var(--aui-agent-monitor-warn); }
.aui-agent-monitor-well-icon[data-tone="done"] { color: var(--aui-agent-monitor-ok); }
.aui-agent-monitor-task { min-width: 0; overflow: hidden; font-weight: 500; text-overflow: ellipsis; white-space: nowrap; }
.aui-agent-monitor-ask { display: flex; flex: 1 1 auto; align-items: center; gap: 8px; min-width: 0; }
.aui-agent-monitor-ask > .aui-agent-monitor-task { flex: 0 1 auto; }
.aui-agent-monitor-command { flex: 0 1 auto; min-width: 3em; padding: 0 7px; overflow: hidden; border-radius: 7px; background: var(--aui-agent-monitor-card); box-shadow: var(--aui-agent-monitor-shadow); color: var(--aui-agent-monitor-ink); font: 11.5px/22px var(--aui-font-mono, ui-monospace, SFMono-Regular, Menlo, monospace); text-overflow: ellipsis; white-space: nowrap; }
.aui-agent-monitor-command:focus { white-space: pre-wrap; overflow-wrap: anywhere; }
.aui-agent-monitor-idle, .aui-agent-monitor-muted { color: var(--aui-agent-monitor-muted); font-weight: 400; }
.aui-agent-monitor-nowrap { flex: none; white-space: nowrap; }
.aui-agent-monitor-note { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.aui-agent-monitor-diff { display: inline-flex; flex: none; gap: 6px; font-weight: 500; }
.aui-agent-monitor-diff [data-tone="add"] { color: var(--aui-agent-monitor-ok); }
.aui-agent-monitor-diff [data-tone="remove"] { color: var(--aui-agent-monitor-err); }
.aui-agent-monitor-fail-dot { flex: none; width: 6px; height: 6px; border-radius: 50%; background: var(--aui-agent-monitor-err); }
.aui-agent-monitor-clock { flex: none; margin-left: auto; color: var(--aui-agent-monitor-text); font-size: 12.5px; white-space: nowrap; }
.aui-agent-monitor-clock[data-paused] { color: var(--aui-agent-monitor-muted); }
.aui-agent-monitor-timeline { display: flex; gap: 3px; height: 12px; padding: 2px 0; }
.aui-agent-monitor-seg { position: relative; flex: 1 1 0; min-width: 4px; height: 8px; overflow: hidden; border-radius: 4px; background: var(--aui-agent-monitor-seg); cursor: pointer; transition: flex-grow 300ms linear, background-color 400ms ease, opacity 200ms ease; animation: aui-agent-monitor-reveal 420ms var(--aui-agent-monitor-ease) both; }
.aui-agent-monitor-seg::before { position: absolute; inset: -6px -1px; content: ""; }
.aui-agent-monitor-seg[data-status="running"] { background: var(--aui-agent-monitor-seg-live); }
.aui-agent-monitor-seg[data-status="running"][data-live]::after { position: absolute; inset: 0 auto 0 -50%; width: 50%; background: linear-gradient(90deg, transparent, var(--aui-agent-monitor-seg-shine), transparent); content: ""; animation: aui-agent-monitor-sweep 1600ms ease-in-out infinite; }
.aui-agent-monitor-seg[data-status="running"][data-paused] { opacity: .45; }
.aui-agent-monitor-seg[data-status="failed"] { background: repeating-linear-gradient(-45deg, var(--aui-agent-monitor-err-bg) 0 2px, transparent 2px 4px), var(--aui-agent-monitor-seg); }
.aui-agent-monitor-timeline[data-complete] .aui-agent-monitor-seg:not([data-status="failed"]) { background: var(--aui-agent-monitor-ok-bg); }
.aui-agent-monitor-seg[data-highlight]:not([data-status="failed"]) { background-color: var(--aui-agent-monitor-seg-strong); }
.aui-agent-monitor-seg[data-dim] { opacity: .35 !important; }
.aui-agent-monitor-seg[data-status="waiting-end"] { flex: 0 0 14px; background: var(--aui-agent-monitor-warn); opacity: .6; cursor: default; animation: aui-agent-monitor-breathe 1600ms ease-in-out infinite; }
.aui-agent-monitor-recent { position: relative; height: calc(var(--aui-agent-monitor-rows) * var(--aui-agent-monitor-row-h)); margin: -2px -6px 0; overflow-y: auto; overscroll-behavior: contain; scrollbar-width: none; border-radius: 9px; transition: height 420ms var(--aui-agent-monitor-ease); }
.aui-agent-monitor-recent::-webkit-scrollbar { display: none; }
.aui-agent-monitor-recent[data-fade-top] { -webkit-mask-image: linear-gradient(to bottom, transparent, #000 14px); mask-image: linear-gradient(to bottom, transparent, #000 14px); }
.aui-agent-monitor-recent[data-fade-bottom] { -webkit-mask-image: linear-gradient(to top, transparent, #000 14px); mask-image: linear-gradient(to top, transparent, #000 14px); }
.aui-agent-monitor-recent[data-fade-top][data-fade-bottom] { -webkit-mask-image: linear-gradient(to bottom, transparent, #000 14px, #000 calc(100% - 14px), transparent); mask-image: linear-gradient(to bottom, transparent, #000 14px, #000 calc(100% - 14px), transparent); }
.aui-agent-monitor-rows { margin: 0; padding: 0; list-style: none; }
.aui-agent-monitor-row-item[data-enter] { animation: aui-agent-monitor-row-in 460ms var(--aui-agent-monitor-ease) both; }
.aui-agent-monitor-row { display: flex; align-items: center; gap: 10px; width: 100%; height: var(--aui-agent-monitor-row-h); padding: 0 8px; border: 0; border-radius: 9px; background: transparent; color: var(--aui-agent-monitor-text); font-size: 13px; text-align: left; transition: background-color 200ms ease, opacity 200ms ease; }
.aui-agent-monitor-row[data-highlight], .aui-agent-monitor-row[aria-expanded="true"] { background: var(--aui-agent-monitor-well); color: var(--aui-agent-monitor-ink); }
.aui-agent-monitor-row[data-dim] { opacity: .35; }
.aui-agent-monitor-row-icon { display: inline-grid; flex: none; place-items: center; width: 20px; height: 20px; border-radius: 6px; background: var(--aui-agent-monitor-card); box-shadow: var(--aui-agent-monitor-shadow); color: var(--aui-agent-monitor-text); }
.aui-agent-monitor-row-icon svg { width: 12px; height: 12px; }
.aui-agent-monitor-row-label { flex: 1 1 auto; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.aui-agent-monitor-row-time { flex: none; color: var(--aui-agent-monitor-muted); font-size: 12.5px; white-space: nowrap; }
.aui-agent-monitor-row-time[data-failed] { color: var(--aui-agent-monitor-err); }
.aui-agent-monitor-foot { display: flex; align-items: center; gap: 6px; min-height: 46px; padding: 8px 6px 4px 4px; }
.aui-agent-monitor-actions { display: flex; flex: 1 1 auto; flex-wrap: wrap; align-items: center; gap: 6px; min-width: 0; }
.aui-agent-monitor-action-slot { display: inline-flex; animation: aui-agent-monitor-pop 380ms var(--aui-agent-monitor-ease) both; }
.aui-agent-monitor-action-slot[data-leaving] { animation: aui-agent-monitor-pop-out 240ms ease both; pointer-events: none; }
.aui-agent-monitor-action { display: inline-flex; align-items: center; gap: 5px; height: 32px; padding: 0 12px 0 10px; border: 0; border-radius: 10px; background: var(--aui-agent-monitor-card); box-shadow: var(--aui-agent-monitor-shadow); color: var(--aui-agent-monitor-ink); font-size: 13px; font-weight: 500; white-space: nowrap; transition: background-color 160ms ease, opacity 160ms ease, transform 160ms ease; }
.aui-agent-monitor-action:hover { background: var(--aui-agent-monitor-well); }
.aui-agent-monitor-action[data-tone="primary"] { padding: 0 10px 0 12px; background: var(--aui-agent-monitor-primary); box-shadow: none; color: var(--aui-agent-monitor-primary-fg); }
.aui-agent-monitor-action[data-tone="primary"]:hover { opacity: .86; }
.aui-agent-monitor-action-swap .aui-agent-monitor-swap-item { display: inline-flex; align-items: center; gap: 5px; }
.aui-agent-monitor-key { display: inline-grid; min-width: 18px; height: 18px; place-items: center; padding: 0 4px; border-radius: 5px; background: var(--aui-agent-monitor-card); box-shadow: inset 0 0 0 1px var(--aui-agent-monitor-line); color: var(--aui-agent-monitor-muted); font: 400 10.5px/1 var(--aui-font-sans, Inter, ui-sans-serif, system-ui, sans-serif); }
.aui-agent-monitor-action[data-tone="primary"] .aui-agent-monitor-key { background: var(--aui-agent-monitor-key-bg); box-shadow: none; color: var(--aui-agent-monitor-key-fg); }
.aui-agent-monitor-position { flex: none; margin-left: auto; padding-right: 4px; color: var(--aui-agent-monitor-muted); font-size: 12px; }
.aui-agent-monitor-log { display: inline-flex; flex: none; align-items: center; gap: 3px; height: 30px; padding: 0 8px; border: 0; border-radius: 10px; background: transparent; color: var(--aui-agent-monitor-text); font-size: 13px; font-weight: 500; transition: background-color 160ms ease, color 160ms ease; }
.aui-agent-monitor-log:hover { background: var(--aui-agent-monitor-log-hover); color: var(--aui-agent-monitor-ink); }
.aui-agent-monitor-panel { position: absolute; inset: auto; top: var(--aui-agent-monitor-panel-top, 120px); left: 12px; right: 12px; z-index: 2; width: auto; max-width: none; margin: 0; padding: 12px; border: 0; border-radius: 14px; background: var(--aui-agent-monitor-card); box-shadow: var(--aui-agent-monitor-pop-shadow); color: var(--aui-agent-monitor-text); font: inherit; animation: aui-agent-monitor-detail 260ms var(--aui-agent-monitor-ease) both; transform-origin: top center; }
.aui-agent-monitor-panel:not([open]) { display: none; }
.aui-agent-monitor-panel[open] { display: grid; gap: 10px; }
.aui-agent-monitor-panel[data-kind="tools"] { top: auto; bottom: calc(100% + 10px); left: 0; right: auto; width: min(280px, 100%); }
.aui-agent-monitor-panel-head { display: flex; align-items: flex-start; gap: 10px; min-width: 0; }
.aui-agent-monitor-panel-heading { display: grid; flex: 1 1 auto; min-width: 0; }
.aui-agent-monitor-panel-title { margin: 0; overflow: hidden; color: var(--aui-agent-monitor-ink); font-size: 14px; font-weight: 600; line-height: 18px; text-overflow: ellipsis; white-space: nowrap; }
.aui-agent-monitor-panel-tool { color: var(--aui-agent-monitor-muted); font-size: 12px; line-height: 16px; }
.aui-agent-monitor-close { display: inline-grid; flex: none; place-items: center; width: 26px; height: 26px; margin: -2px -2px 0 0; padding: 0; border: 0; border-radius: 8px; background: transparent; color: var(--aui-agent-monitor-muted); transition: background-color 160ms ease, color 160ms ease, transform 160ms ease; }
.aui-agent-monitor-close:hover { background: var(--aui-agent-monitor-well); color: var(--aui-agent-monitor-ink); }
.aui-agent-monitor-facts { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 4px; margin: 0; }
.aui-agent-monitor-facts > div { display: grid; gap: 1px; min-width: 0; padding: 6px 8px; border-radius: 9px; background: var(--aui-agent-monitor-well); }
.aui-agent-monitor-facts dt { color: var(--aui-agent-monitor-muted); font-size: 11px; line-height: 14px; }
.aui-agent-monitor-facts dd { margin: 0; overflow: hidden; color: var(--aui-agent-monitor-ink); font-size: 12.5px; font-weight: 500; line-height: 17px; text-overflow: ellipsis; white-space: nowrap; }
.aui-agent-monitor-facts dd[data-tone="running"] { color: var(--aui-agent-monitor-run); }
.aui-agent-monitor-facts dd[data-tone="failed"] { color: var(--aui-agent-monitor-err); }
.aui-agent-monitor-panel-text { margin: 0; color: var(--aui-agent-monitor-text); font-size: 13px; line-height: 19px; }
.aui-agent-monitor-output { display: block; padding: 6px 9px; overflow: hidden; border-radius: 8px; background: var(--aui-agent-monitor-well); color: var(--aui-agent-monitor-ink); font: 11.5px/16px var(--aui-font-mono, ui-monospace, SFMono-Regular, Menlo, monospace); text-overflow: ellipsis; white-space: nowrap; }
.aui-agent-monitor-output:focus { white-space: pre-wrap; overflow-wrap: anywhere; }
.aui-agent-monitor-tool-list { display: grid; gap: 2px; margin: 0; padding: 0; list-style: none; }
.aui-agent-monitor-tool-list li { display: flex; align-items: center; gap: 10px; min-height: 36px; }
.aui-agent-monitor-tool-text { display: grid; flex: 1 1 auto; min-width: 0; font-size: 12.5px; line-height: 16px; }
.aui-agent-monitor-tool-text strong { color: var(--aui-agent-monitor-ink); font-weight: 500; }
.aui-agent-monitor[data-folded] { width: fit-content; max-width: 100%; container-type: normal; }
.aui-agent-monitor-bar-folded { position: relative; display: flex; align-items: center; gap: 12px; height: 48px; max-width: 100%; padding: 4px 14px 4px 4px; border-radius: 16px; background: var(--aui-agent-monitor-tray); animation: aui-agent-monitor-in 300ms var(--aui-agent-monitor-ease) both; transform-origin: 24px 50%; }
.aui-agent-monitor-bar-folded:has(.aui-agent-monitor-expand:focus-visible) { outline: 2px solid var(--aui-agent-monitor-focus); outline-offset: 2px; }
.aui-agent-monitor-expand { position: absolute; inset: 0; z-index: 0; width: 100%; height: 100%; padding: 0; border: 0; border-radius: inherit; background: transparent; }
.aui-agent-monitor .aui-agent-monitor-expand:focus-visible { outline: none; }
.aui-agent-monitor-chip { position: relative; z-index: 1; display: inline-flex; flex: 0 1 auto; align-items: center; gap: 8px; min-width: 0; max-width: 190px; height: 40px; padding: 0 12px 0 5px; border-radius: 12px; background: var(--aui-agent-monitor-card); box-shadow: var(--aui-agent-monitor-shadow); pointer-events: none; }
.aui-agent-monitor-chip-name { min-width: 0; max-width: 140px; overflow: hidden; color: var(--aui-agent-monitor-ink); font-size: 13px; font-weight: 600; text-overflow: ellipsis; white-space: nowrap; }
.aui-agent-monitor-tools { position: relative; z-index: 2; display: inline-flex; flex: none; align-items: center; height: 40px; padding: 0 2px; border: 0; border-radius: 10px; background: transparent; }
.aui-agent-monitor-tool-more { margin-right: 4px; color: var(--aui-agent-monitor-muted); font-size: 11px; font-weight: 500; }
.aui-agent-monitor-tile { position: relative; display: inline-grid; place-items: center; width: 26px; height: 26px; border-radius: 8px; background: var(--aui-agent-monitor-card); box-shadow: var(--aui-agent-monitor-shadow); color: var(--aui-agent-monitor-text); transition: transform 220ms var(--aui-agent-monitor-ease), color 300ms ease; animation: aui-agent-monitor-tile-in 480ms var(--aui-agent-monitor-ease) both; }
.aui-agent-monitor-tile + .aui-agent-monitor-tile { margin-left: -6px; }
.aui-agent-monitor-tile > svg { width: 13px; height: 13px; opacity: .4; filter: grayscale(1); transition: opacity 300ms ease, filter 300ms ease; }
.aui-agent-monitor-tile[data-active] > svg { opacity: 1; filter: none; color: var(--aui-agent-monitor-ink); }
.aui-agent-monitor-tile[data-tone="waiting"] > svg { color: var(--aui-agent-monitor-warn); }
@media (hover: hover) { .aui-agent-monitor-tile:hover { z-index: 3; transform: translateY(-4px); box-shadow: inset 0 0 0 1px var(--aui-agent-monitor-line), 0 0 0 2px var(--aui-agent-monitor-tray), 0 6px 12px -2px rgb(0 0 0 / .14); } }
.aui-agent-monitor-tip { position: absolute; bottom: calc(100% + 10px); left: 50%; z-index: 4; display: grid; gap: 1px; width: max-content; max-width: 220px; padding: 8px 11px; border-radius: 11px; background: var(--aui-agent-monitor-card); box-shadow: var(--aui-agent-monitor-pop-shadow); text-align: left; transform: translateX(-50%); animation: aui-agent-monitor-tip 220ms var(--aui-agent-monitor-ease) both; pointer-events: none; }
.aui-agent-monitor-tip-title { overflow: hidden; color: var(--aui-agent-monitor-ink); font-size: 13px; font-weight: 500; line-height: 18px; text-overflow: ellipsis; white-space: nowrap; }
.aui-agent-monitor-tip-sub { color: var(--aui-agent-monitor-muted); font-size: 12px; line-height: 16px; }
.aui-agent-monitor-end { position: relative; z-index: 1; flex: none; margin-left: auto; pointer-events: none; }
/* Full name and role without hover: keyboard focus anywhere in the monitor (or on the folded bar) wraps them in place, so fold, step and request state stay as they are. */
.aui-agent-monitor:has(:focus-visible) :is(.aui-agent-monitor-name, .aui-agent-monitor-role) { overflow: visible; white-space: normal; overflow-wrap: anywhere; text-overflow: clip; }
.aui-agent-monitor:has(:focus-visible) .aui-agent-monitor-head { flex-wrap: wrap; row-gap: 10px; }
.aui-agent-monitor:has(:focus-visible) .aui-agent-monitor-head-end { margin-left: auto; }
.aui-agent-monitor-bar-folded:has(.aui-agent-monitor-expand:focus-visible) { height: auto; min-height: 48px; }
.aui-agent-monitor-bar-folded:has(.aui-agent-monitor-expand:focus-visible) .aui-agent-monitor-chip { max-width: none; height: auto; min-height: 40px; padding-block: 5px; }
.aui-agent-monitor-bar-folded:has(.aui-agent-monitor-expand:focus-visible) .aui-agent-monitor-chip-name { max-width: none; overflow: visible; white-space: normal; overflow-wrap: anywhere; text-overflow: clip; line-height: 17px; }
.aui-agent-monitor-end-clock { display: inline-block; min-width: 36px; color: var(--aui-agent-monitor-muted); font-size: 13px; font-weight: 500; text-align: right; }
.aui-agent-monitor-end-clock[data-live] { color: transparent; }
.aui-agent-monitor-end-text { color: var(--aui-agent-monitor-muted); font-size: 13px; font-weight: 500; }
.aui-agent-monitor-end-badge { display: inline-flex; align-items: center; gap: 5px; height: 26px; padding: 0 10px 0 8px; border-radius: 999px; font-size: 12.5px; font-weight: 500; white-space: nowrap; }
.aui-agent-monitor-end-badge[data-tone="done"] { background: var(--aui-agent-monitor-ok-bg); color: var(--aui-agent-monitor-ok); }
.aui-agent-monitor-end-badge[data-tone="waiting"] { background: var(--aui-agent-monitor-warn-bg); color: var(--aui-agent-monitor-warn); }
.aui-agent-monitor-end-badge[data-tone="failed"] { background: var(--aui-agent-monitor-err-bg); color: var(--aui-agent-monitor-err); }
@container aui-agent-monitor (max-width: 330px) {
  .aui-agent-monitor-head { flex-wrap: wrap; }
  .aui-agent-monitor-who { flex-basis: 100%; }
  .aui-agent-monitor :is(.aui-agent-monitor-name, .aui-agent-monitor-role) { overflow: visible; white-space: normal; overflow-wrap: anywhere; text-overflow: clip; }
  .aui-agent-monitor-head-end { flex: 1 1 auto; justify-content: flex-end; }
  .aui-agent-monitor-well { height: auto; min-height: 44px; padding-block: 8px; }
  .aui-agent-monitor-well-line { flex-wrap: wrap; }
  .aui-agent-monitor-task, .aui-agent-monitor-command { white-space: normal; overflow-wrap: anywhere; }
  .aui-agent-monitor-facts { grid-template-columns: repeat(2, minmax(0, 1fr)); }
}
@media (max-width: 899px) {
  @container aui-agent-monitor (max-width: 400px) {
    .aui-agent-monitor-head { flex-wrap: wrap; row-gap: 10px; }
    .aui-agent-monitor-who { flex-basis: 100%; }
    .aui-agent-monitor :is(.aui-agent-monitor-name, .aui-agent-monitor-role) { overflow: visible; white-space: normal; overflow-wrap: anywhere; text-overflow: clip; }
    .aui-agent-monitor-head-end { flex: 1 1 auto; justify-content: flex-end; }
    .aui-agent-monitor-well { height: auto; min-height: 44px; padding-block: 8px; align-items: flex-start; flex-wrap: wrap; }
    .aui-agent-monitor-well-line { align-items: flex-start; }
    .aui-agent-monitor-well-line.aui-agent-monitor-result { flex-wrap: wrap; }
    .aui-agent-monitor-well-swap { flex-basis: 100%; }
    .aui-agent-monitor-ask { flex: 1 1 0; flex-wrap: wrap; }
    .aui-agent-monitor-task, .aui-agent-monitor-command, .aui-agent-monitor-note { white-space: normal; overflow-wrap: anywhere; }
    .aui-agent-monitor-task { padding-top: 4px; }
    .aui-agent-monitor-clock { margin-left: 36px; }
  }
  .aui-agent-monitor-panel, .aui-agent-monitor-panel[data-kind="tools"], .aui-agent-monitor-panel:modal { position: fixed; inset: auto 0 0 0; top: auto; bottom: 0; width: 100%; max-width: 100%; max-height: min(85dvh, calc(100dvh - env(safe-area-inset-top) - 24px)); padding: 14px max(14px, env(safe-area-inset-right)) max(16px, env(safe-area-inset-bottom)) max(14px, env(safe-area-inset-left)); overflow-y: auto; overscroll-behavior: contain; border-radius: 20px 20px 0 0; animation: aui-agent-monitor-sheet 260ms var(--aui-agent-monitor-ease) both; }
  .aui-agent-monitor-panel::backdrop { background: rgb(0 0 0 / .36); }
  .aui-agent-monitor-panel-title { white-space: normal; }
  .aui-agent-monitor-output { white-space: pre-wrap; overflow-wrap: anywhere; }
  .aui-agent-monitor-facts { grid-template-columns: repeat(2, minmax(0, 1fr)); }
}
@media (pointer: coarse) {
  .aui-agent-monitor { --aui-agent-monitor-row-h: 44px; }
  .aui-agent-monitor-fold { width: 44px; height: 44px; margin-right: 0; }
  .aui-agent-monitor-close { width: 44px; height: 44px; margin: -8px -8px 0 0; }
  .aui-agent-monitor-action, .aui-agent-monitor-log { min-width: 44px; height: 44px; }
  .aui-agent-monitor-tools { min-width: 44px; height: 44px; padding: 0 6px; }
  .aui-agent-monitor-key { display: none; }
  .aui-agent-monitor-tool-list li { min-height: 44px; }
  .aui-agent-monitor-well { height: auto; min-height: 44px; padding-block: 8px; }
  .aui-agent-monitor-ask { flex-wrap: wrap; }
  .aui-agent-monitor-command, .aui-agent-monitor-output { white-space: pre-wrap; overflow-wrap: anywhere; }
  /* Touch has no hover tooltip: the name and role (and the folded chip name) wrap to their last letter. */
  .aui-agent-monitor :is(.aui-agent-monitor-name, .aui-agent-monitor-role) { overflow: visible; white-space: normal; overflow-wrap: anywhere; text-overflow: clip; }
  .aui-agent-monitor-who { align-items: flex-start; }
  /* A name that does not fit on one line pushes cost, status and fold to a second row instead of squeezing into a narrow column. */
  .aui-agent-monitor-head { flex-wrap: wrap; row-gap: 10px; }
  .aui-agent-monitor-head-end { margin-left: auto; }
  .aui-agent-monitor-bar-folded { height: auto; min-height: 52px; }
  .aui-agent-monitor-chip { max-width: none; height: auto; min-height: 44px; padding-block: 6px; }
  .aui-agent-monitor-chip-name { max-width: none; overflow: visible; white-space: normal; overflow-wrap: anywhere; text-overflow: clip; line-height: 17px; }
}
@keyframes aui-agent-monitor-in { from { opacity: 0; transform: scale(.92); } to { opacity: 1; transform: none; } }
@keyframes aui-agent-monitor-swap-in { from { opacity: 0; transform: translateY(70%); filter: blur(2px); } to { opacity: 1; transform: none; filter: none; } }
@keyframes aui-agent-monitor-swap-out { from { opacity: 1; transform: none; filter: none; } to { opacity: 0; transform: translateY(-70%); filter: blur(2px); } }
@keyframes aui-agent-monitor-pop { from { opacity: 0; transform: scale(.88); filter: blur(3px); } to { opacity: 1; transform: none; filter: none; } }
@keyframes aui-agent-monitor-pop-out { from { opacity: 1; transform: none; filter: none; } to { opacity: 0; transform: scale(.88); filter: blur(3px); } }
@keyframes aui-agent-monitor-shine { from { background-position: 100% 0; } to { background-position: 0 0; } }
@keyframes aui-agent-monitor-pulse { 0% { opacity: .45; transform: scale(.6); } 100% { opacity: 0; transform: scale(1.8); } }
@keyframes aui-agent-monitor-reveal { from { clip-path: inset(0 100% 0 0); } to { clip-path: inset(0); } }
@keyframes aui-agent-monitor-sweep { from { left: -50%; } to { left: 100%; } }
@keyframes aui-agent-monitor-breathe { 0%, 100% { opacity: .6; } 50% { opacity: .35; } }
@keyframes aui-agent-monitor-row-in { from { opacity: 0; transform: translateY(-30px); filter: blur(2px); } to { opacity: 1; transform: none; filter: none; } }
@keyframes aui-agent-monitor-detail { from { opacity: 0; transform: translateY(-4px) scale(.97); } to { opacity: 1; transform: none; } }
@keyframes aui-agent-monitor-sheet { from { opacity: 0; transform: translateY(24px); } to { opacity: 1; transform: none; } }
@keyframes aui-agent-monitor-tile-in { 0% { opacity: 0; width: 0; transform: scale(.5); } 40% { opacity: 0; } 100% { opacity: 1; width: 26px; transform: none; } }
@keyframes aui-agent-monitor-tip { from { opacity: 0; transform: translate(-50%, 4px) scale(.97); } to { opacity: 1; transform: translateX(-50%); } }
@media (prefers-reduced-motion: reduce) {
  .aui-agent-monitor *, .aui-agent-monitor *::before, .aui-agent-monitor *::after { animation: none !important; transition: none !important; }
  .aui-agent-monitor-swap-item[data-leaving], .aui-agent-monitor-action-slot[data-leaving] { display: none; }
}
.aui-agent-monitor-demo { display: grid; grid-template-columns: minmax(0, 1fr); gap: 14px; width: 100%; min-width: 0; justify-items: start; color: var(--aui-fg-dim, #525252); font: 13px/1.5 var(--aui-font-sans, Inter, ui-sans-serif, system-ui, sans-serif); word-break: keep-all; overflow-wrap: anywhere; }
:where(.dark, [data-theme="dark"]) .aui-agent-monitor-demo { color: var(--aui-fg-dim, #a1a1a6); }
.aui-agent-monitor-demo-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(min(100%, 372px), 1fr)); gap: 16px; width: 100%; align-items: start; }
.aui-agent-monitor-demo-cell { display: grid; grid-template-columns: minmax(0, 1fr); justify-items: start; gap: 6px; min-width: 0; }
.aui-agent-monitor-demo-cell > p, .aui-agent-monitor-demo-log { margin: 0; color: var(--aui-fg-muted, #737373); font-size: 12.5px; }
:where(.dark, [data-theme="dark"]) :is(.aui-agent-monitor-demo-cell > p, .aui-agent-monitor-demo-log) { color: var(--aui-fg-muted, #8b8b92); }
.aui-agent-monitor-demo-bar { display: flex; flex-wrap: wrap; gap: 8px; }
.aui-agent-monitor-demo-button { min-height: 34px; padding: 0 12px; border: 1px solid var(--aui-line, #e4e4e7); border-radius: 10px; background: var(--aui-bg-1, #fff); color: var(--aui-fg, #171717); font: 500 13px/1 var(--aui-font-sans, Inter, ui-sans-serif, system-ui, sans-serif); cursor: pointer; touch-action: manipulation; }
:where(.dark, [data-theme="dark"]) .aui-agent-monitor-demo-button { border-color: var(--aui-line, #2a2a2e); background: var(--aui-bg-1, #232326); color: var(--aui-fg, #ededed); }
.aui-agent-monitor-demo-button:focus-visible { outline: 2px solid var(--aui-focus, rgb(0 96 226 / .7)); outline-offset: 2px; }
@media (pointer: coarse) { .aui-agent-monitor-demo-button { min-height: 44px; } }
`;

/* ------------------------------------------------------------------ demos */

export type AgentMonitorDemoState = "default" | "approval" | "empty" | "long" | "loading" | "error" | "states" | "pause-resume" | "history" | "folded" | "outcome" | "boundaries" | "narrow" | "labels";

const NOW = 1_763_380_800_000;
const relay: AgentMonitorAgent = { name: "Relay 감사", role: "AgentDesk 세션 로그 점검" };
const s = (id: string, label: string, kind: AgentMonitorStepKind, status: AgentMonitorStepStatus, duration?: number, extra: Partial<AgentMonitorStep> = {}): AgentMonitorStep => ({ id, label, kind, status, duration, ...extra });

const baseSteps = (): AgentMonitorStep[] => [
  s("read", "세션 로그 읽기", "read", "done", 9_000, { tool: "파일 읽기", started: NOW - 95_000, description: "어제 저장된 relay 세션 ndjson 3개를 읽었습니다.", output: "read ~/.adk/logs/relay-2026-11-16.ndjson (1,284 lines)" }),
  s("group", "채널별로 분류", "think", "done", 21_400, { started: NOW - 86_000, description: "채널과 스레드 기준으로 전달 지연을 묶었습니다." }),
  s("grep", "지연 30초 이상 찾기", "search", "done", 4_200, { tool: "코드 검색", started: NOW - 64_000, output: "rg 'delay_ms\": [3-9][0-9]{4}' relay.ndjson → 7 matches" }),
  s("verify", "누락 메시지 검증", "run", "running", undefined, { tool: "터미널", started: NOW - 38_000 }),
  s("report", "요약 보고", "edit", "pending"),
];

function Cell({ note, children }: { note: string; children: ReactNode }) {
  return <div className="aui-agent-monitor-demo-cell"><p>{note}</p>{children}</div>;
}

function DefaultDemo() {
  const [log, setLog] = useState("단계 띠나 최근 행을 누르면 단계 상세가 열립니다.");
  return <div className="aui-agent-monitor-demo">
    <AgentMonitor status="running" agent={relay} steps={baseSteps()} started={NOW - 96_000} now={NOW} cost={0.06} onPause={() => setLog("일시정지 요청을 호스트에 보냈습니다.")} onCancel={() => setLog("취소 요청을 호스트에 보냈습니다.")} onOpenLog={() => setLog("로그 열기 요청")} />
    <p className="aui-agent-monitor-demo-log" role="status">{log}</p>
  </div>;
}

function ApprovalDemo() {
  const [status, setStatus] = useState<AgentMonitorStatus>("waiting");
  const [log, setLog] = useState("카드에 초점을 두고 Enter는 허용, Esc는 거부입니다. 버튼도 같은 요청을 보냅니다.");
  const steps = baseSteps().map((step) => step.id === "verify" ? { ...step, status: (status === "waiting" ? "running" : status === "running" ? "running" : "done") as AgentMonitorStepStatus, duration: status === "done" ? 12_000 : undefined } : step);
  return <div className="aui-agent-monitor-demo">
    <AgentMonitor status={status} agent={relay} steps={steps} started={NOW - 96_000} now={NOW} cost={0.071}
      approval={{ title: "운영 채널에 재전송할까요?", command: "adk relay replay --channel ops-relay --since 2026-11-16T09:00:00+09:00 --dry-run=false", icon: <Terminal size={15} strokeWidth={2} /> }}
      onApprove={() => { setStatus("running"); setLog("허용 요청 → 호스트가 실행 중으로 바꿨습니다."); }}
      onDeny={() => { setStatus("cancelled"); setLog("거부 요청 → 호스트가 취소됨으로 바꿨습니다."); }}
      onCancel={() => setLog("취소 요청")} onOpenLog={() => setLog("로그 열기 요청")} />
    <div className="aui-agent-monitor-demo-bar"><button type="button" className="aui-agent-monitor-demo-button" onClick={() => { setStatus("waiting"); setLog("다시 확인 대기로 돌렸습니다."); }}>확인 대기로 되돌리기</button></div>
    <p className="aui-agent-monitor-demo-log" role="status">{log}</p>
  </div>;
}

const englishMonitorLabels: Partial<AgentMonitorLabels> = {
  agent: "Agent", statusIdle: "Ready", statusRunning: "Running", statusWaiting: "Needs you", statusPaused: "Paused", statusDone: "Done", statusFailed: "Failed", statusCancelled: "Cancelled",
  stepPending: "Pending", stepRunning: "Running", stepDone: "Done", stepFailed: "Failed",
  kindRead: "Read", kindSearch: "Search", kindEdit: "Edit", kindRun: "Run", kindThink: "Think", kindWeb: "Web",
  waiting: "Waiting for you", waitingAnnounce: "waiting for you", failed: "The run failed", failedAnnounce: "the run failed", command: (command) => `Command: ${command}`,
  paused: "Paused", done: (span) => `Done · ${span}`, files: (count) => `${count} files`, cancelled: (span) => `Cancelled after ${span}`, idle: "Not started yet",
  cancel: "Cancel", deny: "Deny", approve: "Allow", retry: "Retry", pause: "Pause", resume: "Resume", log: "Log",
  expand: (label) => `${label} — expand`, fold: (name) => `Collapse ${name} monitor`, tools: (count) => `${count} tools used`, toolsTitle: (count) => `${count} tools used`,
  close: "Close", closeStep: "Close step details", factStart: "Start", factDuration: "Took", factShare: "Share", factStatus: "Status",
  output: (output) => `Output: ${output}`, request: (name, title) => `${name} asks: ${title}`, cost: "Cost so far", elapsed: "Elapsed",
  timeline: (steps) => `Steps run: ${steps}`, recent: "Finished steps", stepDetail: (label, time) => `${label}, ${time} — step details`, position: "Step",
  keys: (approve, deny) => [approve ? "Enter allows" : "", deny ? "Esc denies" : ""].filter(Boolean).join(", "),
  span: (ms) => { const seconds = Math.max(0, Math.floor(ms / 1000)); return seconds < 60 ? `${seconds}s` : `${Math.floor(seconds / 60)}m ${String(seconds % 60).padStart(2, "0")}s`; },
  stepTime: (ms) => ms < 10_000 ? `${(Math.max(0, ms) / 1000).toFixed(1)}s` : `${Math.floor(ms / 60_000)}m ${String(Math.floor(ms / 1000) % 60).padStart(2, "0")}s`,
};

/** labels: every visible and spoken text from the host, here in English, folded and unfolded. */
function LabelsDemo() {
  const [log, setLog] = useState("labels로 상태·버튼·접기/펼치기 이름까지 바꿔요.");
  const steps = baseSteps().map((step) => ({ ...step, tool: undefined, description: undefined }));
  return <div className="aui-agent-monitor-demo">
    <AgentMonitor status="running" agent={{ name: "Relay audit", role: "AgentDesk session log check" }} steps={steps} started={NOW - 96_000} now={NOW} cost={0.06} labels={englishMonitorLabels} onPause={() => setLog("pause")} onCancel={() => setLog("cancel")} onOpenLog={() => setLog("log")} />
    <AgentMonitor status="waiting" defaultFolded agent={{ name: "Relay audit" }} steps={steps} started={NOW - 96_000} now={NOW} labels={englishMonitorLabels} approval={{ title: "Replay to the ops channel?" }} onApprove={() => setLog("allow")} />
    <p className="aui-agent-monitor-demo-log" role="status">{log}</p>
  </div>;
}

function PauseResumeDemo() {
  const [start] = useState(() => Date.now() - 42_000);
  const [status, setStatus] = useState<AgentMonitorStatus>("running");
  const [paused, setPaused] = useState(0);
  const pausedAt = useRef(0);
  const steps: AgentMonitorStep[] = [
    s("read", "세션 로그 읽기", "read", "done", 6_400, { started: start }),
    s("scan", "토큰 사용량 집계", "run", "running", undefined, { started: start + 6_400 + paused }),
    s("write", "요약 작성", "edit", "pending"),
  ];
  return <div className="aui-agent-monitor-demo">
    <AgentMonitor status={status} agent={relay} steps={steps} started={start} pausedMs={paused} cost={0.012}
      onPause={() => { pausedAt.current = Date.now(); setStatus("paused"); }}
      onResume={() => { setPaused((value) => value + (Date.now() - pausedAt.current)); setStatus("running"); }}
      onCancel={() => setStatus("cancelled")} />
    <p className="aui-agent-monitor-demo-log" role="status">상태: {statusText[status]} · 누적 정지 {Math.round(paused / 100) / 10}초. 일시정지/재개는 같은 자리라 초점이 유지됩니다.</p>
    <div className="aui-agent-monitor-demo-bar"><button type="button" className="aui-agent-monitor-demo-button" onClick={() => { setPaused(0); setStatus("running"); }}>실행 중으로 되돌리기</button></div>
  </div>;
}

function HistoryDemo() {
  const steps: AgentMonitorStep[] = [
    s("h1", "저장소 구조 읽기", "read", "done", 3_200, { started: NOW - 120_000, output: "ls -R src/relay (42 files)" }),
    s("h2", "turn_bridge 경로 찾기", "search", "done", 5_800, { started: NOW - 116_000 }),
    s("h3", "재시도 정책 비교", "think", "done", 14_000, { started: NOW - 110_000, description: "고정 간격과 지수 백오프를 비교했습니다." }),
    s("h4", "패치 초안", "edit", "done", 22_500, { started: NOW - 95_000, output: "src/relay/turn_bridge.rs | 18 ++++++++++------" }),
    s("h5", "단위 테스트", "run", "failed", 9_100, { started: NOW - 72_000, description: "queue::drain 테스트가 시간 초과로 실패했습니다.", output: "test queue::drain ... FAILED (timeout 5s)" }),
    s("h6", "테스트 재실행", "run", "done", 11_200, { started: NOW - 62_000 }),
    s("h7", "릴리스 노트 검색", "web", "running", undefined, { started: NOW - 20_000 }),
  ];
  return <div className="aui-agent-monitor-demo">
    <AgentMonitor status="running" agent={{ name: "수정 에이전트", role: "claude-sonnet · 패치" }} steps={steps} started={NOW - 121_000} now={NOW} cost={0.214} onPause={() => undefined} onOpenLog={() => undefined} />
    <p className="aui-agent-monitor-demo-log">완료 5·실패 1이 최신부터 세 줄 보이고 나머지는 목록 안에서 스크롤합니다. 띠에서 아래쪽 단계를 가리키면 그 행이 목록 가운데로 옵니다.</p>
  </div>;
}

const replayIcon = <RotateCcw size={13} strokeWidth={2} />;
function FoldedDemo() {
  const steps: AgentMonitorStep[] = [
    s("f1", "로그 읽기", "read", "done", 2_000),
    s("f2", "패턴 찾기", "search", "done", 3_000),
    s("f3", "설정 수정", "edit", "done", 4_000),
    s("f4", "웹 문서 확인", "web", "done", 5_000),
    s("f5", "다시 보내기", "run", "done", 6_000, { icon: replayIcon, tool: "relay replay" }),
    s("f6", "원인 정리", "think", "done", 7_000),
    s("f7", "로그 다시 읽기", "read", "done", 1_500),
    s("f8", "재전송 확인", "run", "running", undefined, { icon: replayIcon, tool: "relay replay", started: NOW - 4_000 }),
  ];
  const finished = steps.map((step) => ({ ...step, status: "done" as const, duration: step.duration ?? 4_000 }));
  const failed = steps.map((step, index) => index === steps.length - 1 ? { ...step, status: "failed" as const, duration: 2_000 } : step);
  return <div className="aui-agent-monitor-demo">
    <p className="aui-agent-monitor-demo-log">고유 도구 6개(읽기·다시 보내기는 재사용) 중 마지막 4개와 +2. 마우스는 타일 위에 마지막 사용 단계를 띄우고, 터치·키보드는 도구 버튼으로 목록을 엽니다. 막대의 나머지를 누르면 펼칩니다.</p>
    <div className="aui-agent-monitor-demo-grid">
      <Cell note="실행 중"><AgentMonitor defaultFolded status="running" agent={relay} steps={steps} started={NOW - 40_000} now={NOW} cost={0.03} onPause={() => undefined} /></Cell>
      <Cell note="일시정지"><AgentMonitor defaultFolded status="paused" agent={relay} steps={steps} started={NOW - 40_000} now={NOW} cost={0.03} /></Cell>
      <Cell note="확인 대기"><AgentMonitor defaultFolded status="waiting" agent={relay} steps={steps} started={NOW - 40_000} now={NOW} approval={{ title: "재전송할까요?" }} onApprove={() => undefined} onDeny={() => undefined} /></Cell>
      <Cell note="완료"><AgentMonitor defaultFolded status="done" agent={relay} steps={finished} started={NOW - 252_000} ended={NOW} cost={0.04} /></Cell>
      <Cell note="실패"><AgentMonitor defaultFolded status="failed" agent={relay} steps={failed} started={NOW - 40_000} ended={NOW} error="재전송 권한이 없습니다." /></Cell>
      <Cell note="준비·취소"><AgentMonitor defaultFolded status="idle" agent={relay} /><AgentMonitor defaultFolded status="cancelled" agent={relay} steps={finished.slice(0, 2)} started={NOW - 30_000} ended={NOW} /></Cell>
    </div>
  </div>;
}

function StatesDemo() {
  const [log, setLog] = useState("요청을 보내도 모니터는 상태를 바꾸지 않습니다. 처리기가 있는 행동만 보입니다.");
  const req = (what: string) => () => setLog(`${what} 요청`);
  const done = baseSteps().map((step) => ({ ...step, status: "done" as const, duration: step.duration ?? 8_000 }));
  const failSteps = baseSteps().map((step) => step.id === "verify" ? { ...step, status: "failed" as const, duration: 7_300 } : step);
  return <div className="aui-agent-monitor-demo">
    <p className="aui-agent-monitor-demo-log" role="status">{log}</p>
    <div className="aui-agent-monitor-demo-grid">
      <Cell note="준비: 단계는 있지만 아직 시작 전"><AgentMonitor status="idle" agent={relay} steps={baseSteps().map((step) => ({ ...step, status: "pending" as const }))} cost={0.01} onOpenLog={req("로그")} /></Cell>
      <Cell note="실행: 정지·취소"><AgentMonitor status="running" agent={relay} steps={baseSteps()} started={NOW - 96_000} now={NOW} cost={0.06} onPause={req("정지")} onCancel={req("취소")} onOpenLog={req("로그")} /></Cell>
      <Cell note="실행이지만 진행 단계 없음: 준비 안내"><AgentMonitor status="running" agent={relay} steps={baseSteps().slice(0, 3)} started={NOW - 40_000} now={NOW} cost={0.02} onPause={req("정지")} /></Cell>
      <Cell note="확인 대기: 거부·허용"><AgentMonitor status="waiting" agent={relay} steps={baseSteps()} started={NOW - 96_000} now={NOW} cost={0.06} approval={{ title: "브랜치를 강제로 덮어쓸까요?" }} onApprove={req("허용")} onDeny={req("거부")} /></Cell>
      <Cell note="일시정지: 재개·취소"><AgentMonitor status="paused" agent={relay} steps={baseSteps()} started={NOW - 96_000} now={NOW} pausedMs={12_000} cost={0.06} onResume={req("재개")} onCancel={req("취소")} /></Cell>
      <Cell note="완료: 결과와 소요"><AgentMonitor status="done" agent={relay} steps={done} started={NOW - 252_000} ended={NOW} cost={0.084} result={{ added: 1284, removed: 14, files: 3, note: "PR #2141 열림" }} onOpenLog={req("로그")} /></Cell>
      <Cell note="실패: 재시도"><AgentMonitor status="failed" agent={relay} steps={failSteps} started={NOW - 96_000} ended={NOW - 20_000} cost={0.05} error="Discord API가 429로 거절했습니다." onRetry={req("재시도")} onOpenLog={req("로그")} /></Cell>
      <Cell note="취소: 걸린 시간"><AgentMonitor status="cancelled" agent={relay} steps={baseSteps()} started={NOW - 75_000} ended={NOW} cost={0.03} onRetry={req("재시도")} /></Cell>
    </div>
  </div>;
}

function OutcomeDemo() {
  const done = baseSteps().map((step) => ({ ...step, status: "done" as const, duration: step.duration ?? 8_000 }));
  return <div className="aui-agent-monitor-demo"><div className="aui-agent-monitor-demo-grid">
    <Cell note="파일 0 · 비용 $0.0005"><AgentMonitor status="done" agent={relay} steps={done} started={NOW - 45_000} ended={NOW} cost={0.0005} result={{ files: 0 }} /></Cell>
    <Cell note="추가만 · 비용 $0.001"><AgentMonitor status="done" agent={relay} steps={done} started={NOW - 61_000} ended={NOW} cost={0.001} result={{ added: 42 }} /></Cell>
    <Cell note="제거만 · 비용 $1"><AgentMonitor status="done" agent={relay} steps={done} started={NOW - 3_725_000} ended={NOW} cost={1} result={{ removed: 12_345 }} /></Cell>
    <Cell note="보조 문구만 · 비용 0"><AgentMonitor status="done" agent={relay} steps={done} started={NOW - 9_000} ended={NOW} cost={0} result={{ note: "변경 없음, 보고서만 남김" }} /></Cell>
  </div></div>;
}

function BoundariesDemo() {
  const [removedIds, setRemovedIds] = useState<string[]>([]);
  const all: AgentMonitorStep[] = [
    s("b1", "9999ms", "read", "done", 9_999),
    s("b2", "10000ms", "read", "done", 10_000),
    s("b3", "59초", "think", "done", 59_000),
    s("b4", "60초", "think", "done", 60_000),
    s("b5", "1시간", "run", "done", 3_600_000),
    s("b6", "음수 소요", "edit", "done", -500),
    s("b7", "미래 시작(진행)", "web", "running", undefined, { started: NOW + 60_000 }),
    s("b8", "두 번째 진행", "search", "running", undefined, { started: NOW - 2_000 }),
  ];
  const steps = all.filter((step) => !removedIds.includes(step.id));
  return <div className="aui-agent-monitor-demo">
    <div className="aui-agent-monitor-demo-grid">
      <Cell note="소요 경계·진행 다수(우물은 첫 진행)"><AgentMonitor status="running" agent={relay} steps={steps} started={NOW - 3_725_000} now={NOW} cost={0.0009} onOpenLog={() => undefined} /></Cell>
      <Cell note="시작 0 · 시계 0:00"><AgentMonitor status="running" agent={relay} steps={[s("z1", "시작 시각 0", "think", "running", undefined, { started: 0 })]} started={0} ended={0} cost={0} /></Cell>
      <Cell note="시작 없음 · 시계 숨김"><AgentMonitor status="running" agent={{ name: "  " }} steps={[s("z2", "시작 없는 진행", "run", "running")]} /></Cell>
    </div>
    <div className="aui-agent-monitor-demo-bar">
      <button type="button" className="aui-agent-monitor-demo-button" onClick={() => setRemovedIds((current) => [...current, steps.find((step) => step.status === "done")?.id ?? ""])}>완료 단계 하나 제거(열린 상세는 닫힘)</button>
      <button type="button" className="aui-agent-monitor-demo-button" onClick={() => setRemovedIds([])}>되돌리기</button>
    </div>
  </div>;
}

function NarrowDemo() {
  return <div className="aui-agent-monitor-demo">
    <Cell note="320px"><AgentMonitor status="waiting" agent={{ name: "릴리스 검증을 맡은 아주 긴 이름의 에이전트", role: "claude-opus-long-context-evaluation" }} steps={baseSteps()} started={NOW - 96_000} now={NOW} cost={0.06} approval={{ title: "운영 채널에 재전송할까요?", command: "adk relay replay --channel ops-relay --since 2026-11-16" }} onApprove={() => undefined} onDeny={() => undefined} onOpenLog={() => undefined} style={{ width: 320 }} /></Cell>
    <Cell note="402px"><AgentMonitor status="running" agent={relay} steps={baseSteps()} started={NOW - 96_000} now={NOW} cost={0.06} onPause={() => undefined} onCancel={() => undefined} onOpenLog={() => undefined} style={{ width: 402 }} /></Cell>
  </div>;
}

function LongDemo() {
  const steps: AgentMonitorStep[] = [
    s("l1", "AgentDesk 대시보드의 통계 화면에서 일별 토큰 구성 차트가 빈 날짜를 만나면 무너지는지 확인하기", "read", "done", 12_400, { tool: "아주 긴 이름의 내부 파일 읽기 도구 (workspace-file-reader)", description: "통계 화면의 데이터 소스와 차트 렌더러를 끝까지 읽었습니다. 빈 날짜는 null 로 들어오고 차트는 0으로 바꾸지 않습니다.", output: "read dashboard/src/components/stats/DailyTokenCompositionChart.tsx (412 lines) · session_01JD8Z4X9QK3M7V2N5R8T6W1Y0" }),
    s("l2", "https://github.com/itismyfield/AgentDesk/issues/2140 의 재현 절차 따라가기", "web", "done", 31_000),
    s("l3", "긴 명령으로 재현", "run", "running", undefined, { started: NOW - 12_000 }),
  ];
  return <div className="aui-agent-monitor-demo">
    <AgentMonitor status="waiting" agent={{ name: "통계 화면 회귀를 끝까지 추적하는 아주 긴 이름의 검증 에이전트", role: "claude-sonnet-agentdesk-long-context-evaluation-2026-11-17T090000Z" }} steps={steps} started={NOW - 3_725_000} now={NOW} cost={1.2847}
      approval={{ title: "운영 Discord 서버의 #general 채널에 실제 사용자에게 보이는 재현 메시지를 세 번 보내도 될까요?", command: "adk relay send --guild agentdesk-prod --channel general --repeat 3 --message 'relay regression probe session_01JD8Z4X9QK3M7V2N5R8T6W1Y0'" }}
      onApprove={() => undefined} onDeny={() => undefined} onOpenLog={() => undefined} />
    <p className="aui-agent-monitor-demo-log">명령에 초점을 두거나 탭하면 전체 명령이 줄바꿈되어 보입니다. 터치 화면과 폰의 두 행 머리에서는 이름·역할이 마지막 글자까지 줄바꿈되고, 키보드 초점이 모니터 안에 있으면 데스크톱에서도 같은 자리에서 펼쳐집니다. 단계 상세는 폰에서 바텀시트로 열립니다.</p>
    <AgentMonitor defaultFolded status="running" agent={{ name: "통계 화면 회귀를 끝까지 추적하는 아주 긴 이름의 검증 에이전트", role: "claude-sonnet-agentdesk-long-context-evaluation-2026-11-17T090000Z" }} steps={steps} started={NOW - 3_725_000} now={NOW} cost={1.2847} onPause={() => undefined} />
    <p className="aui-agent-monitor-demo-log">접힌 막대의 흰 칩도 터치에서는 이름 전체를 줄바꿈해 보이고, 키보드로 막대에 초점을 두면 펼치지 않고 그 자리에서 전체 이름을 읽습니다.</p>
  </div>;
}

export function AgentMonitorDemo({ state = "default" }: { state?: AgentMonitorDemoState }) {
  if (state === "approval") return <ApprovalDemo />;
  if (state === "labels") return <LabelsDemo />;
  if (state === "empty") return <div className="aui-agent-monitor-demo"><AgentMonitor status="idle" agent={relay} /></div>;
  if (state === "long") return <LongDemo />;
  if (state === "loading") return <div className="aui-agent-monitor-demo"><AgentMonitor status="running" agent={relay} steps={[s("read", "세션 로그 읽기", "read", "done", 9_000), s("load", "이벤트 묶음 불러오기", "read", "running", undefined, { started: NOW - 300 }), s("sum", "요약", "edit", "pending")]} started={NOW - 9_300} now={NOW} cost={0.06} onPause={() => undefined} /></div>;
  if (state === "error") return <div className="aui-agent-monitor-demo"><AgentMonitor status="failed" agent={relay} steps={[s("read", "세션 로그 읽기", "read", "failed", 3_000)]} started={NOW - 3_000} ended={NOW} cost={0.06} error="로컬 이벤트 묶음을 열 수 없습니다: 권한이 없습니다." onRetry={() => undefined} /></div>;
  if (state === "states") return <StatesDemo />;
  if (state === "pause-resume") return <PauseResumeDemo />;
  if (state === "history") return <HistoryDemo />;
  if (state === "folded") return <FoldedDemo />;
  if (state === "outcome") return <OutcomeDemo />;
  if (state === "boundaries") return <BoundariesDemo />;
  if (state === "narrow") return <NarrowDemo />;
  return <DefaultDemo />;
}
