// Kit provenance: itismyfield/agentdesk-ui-kit@b6d2ab1
import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState, type CSSProperties, type FocusEvent as ReactFocusEvent, type KeyboardEvent as ReactKeyboardEvent, type ReactNode } from "react";
import { Check, ChevronDown, ChevronRight, ClipboardCheck, Pause, Play, X } from "lucide-react";

// Clean-room implementation from clean-specs/running-now (11/17 revision).
// No affectUI Pro source, registry payload, package, site asset or original copy is used.

export type RunningNowStatus = "running" | "paused" | "waiting" | "done";

export interface RunningNowTask {
  id: string;
  title: string;
  /** Current step, or the reason the task is waiting for an answer. */
  step: string;
  /** Finished steps. Omit `done` or `total` when the host has no step count: the row then hides the progress ring and count. */
  done?: number;
  /** All steps. */
  total?: number;
  status: RunningNowStatus;
}

export interface RunningNowLabels {
  title: string;
  running: string;
  waitingOne: string;
  waitingMany: string;
  paused: string;
  done: string;
  open: string;
  pause: string;
  resume: string;
  empty: string;
  idle: string;
  viewAll: string;
  close: string;
  /** Spoken step count in the row name, from formatted numbers. */
  steps: (done: string, total: string) => string;
}

export interface RunningNowProps {
  tasks: RunningNowTask[];
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  /** Panel edge aligned with the trigger. Defaults to end. */
  align?: "start" | "end";
  /** Every visible and spoken text. Missing keys keep the Korean defaults. */
  labels?: Partial<RunningNowLabels>;
  /** Shows the pause button on running tasks. Without it there is no pause button. */
  onPause?: (id: string) => void;
  /** Shows the resume button on paused tasks. Without it there is no resume button. */
  onResume?: (id: string) => void;
  /** Shows the open button on waiting tasks and the detail request from a row. Without it there is no open button. */
  onOpenTask?: (id: string) => void;
  /** Without it the footer link is omitted. */
  onViewAll?: () => void;
  className?: string;
  style?: CSSProperties;
}

const defaults: RunningNowLabels = {
  title: "백그라운드 작업",
  running: "실행 중",
  waitingOne: "응답 필요",
  waitingMany: "응답 필요",
  paused: "일시정지",
  done: "완료",
  open: "열기",
  pause: "일시정지",
  resume: "재개",
  empty: "지금 백그라운드에서 도는 작업이 없습니다.",
  idle: "백그라운드 작업 없음",
  viewAll: "모든 작업 보기",
  close: "닫기",
  steps: (done, total) => `단계 ${done}/${total} 완료`,
};

const numberFormat = new Intl.NumberFormat("en-US");
const fmt = (value: number) => numberFormat.format(value);
const finite = (value: unknown) => typeof value === "number" && Number.isFinite(value) ? value : 0;

interface Normal extends RunningNowTask { stepsTotal: number; stepsDone: number; ratio: number; hasProgress: boolean }

/** Steps: T = max(0, total), D = min(T, max(0, done)); done tasks are full. A missing done or total means no progress. */
function normalize(tasks: unknown): Normal[] {
  if (!Array.isArray(tasks)) return [];
  return tasks.filter((task): task is RunningNowTask => !!task && typeof task === "object" && typeof (task as RunningNowTask).id === "string" && (task as RunningNowTask).id !== "").map((task) => {
    const hasProgress = task.done !== undefined && task.done !== null && task.total !== undefined && task.total !== null;
    const total = Math.max(0, finite(task.total));
    const doneRaw = finite(task.done);
    const done = Math.min(total, Math.max(0, doneRaw));
    const ratio = task.status === "done" ? 1 : total > 0 ? Math.min(1, Math.max(0, doneRaw / total)) : 0;
    return { ...task, stepsTotal: total, stepsDone: task.status === "done" ? total : done, ratio, hasProgress };
  });
}

/** Done blue → paused muted → ≤25% orange → ≤50% amber → teal. */
export function runningNowTone(status: RunningNowStatus, ratio: number) {
  if (status === "done") return "done";
  if (status === "paused") return "paused";
  if (ratio <= 0.25) return "low";
  if (ratio <= 0.5) return "mid";
  return "high";
}

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

/** One outgoing copy at most: the old text leaves upward while the new one enters from below. */
function Swap({ id, children, className = "" }: { id: string; children: ReactNode; className?: string }) {
  const reduced = useMedia("(prefers-reduced-motion: reduce)");
  const committed = useRef<{ id: string; node: ReactNode }>({ id, node: children });
  const [leaving, setLeaving] = useState<{ id: string; node: ReactNode } | null>(null);
  if (!reduced && committed.current.id !== id && leaving?.id !== committed.current.id) setLeaving(committed.current);
  useLayoutEffect(() => { committed.current = { id, node: children }; });
  useEffect(() => {
    if (!leaving) return;
    const timer = window.setTimeout(() => setLeaving(null), 360);
    return () => window.clearTimeout(timer);
  }, [leaving]);
  const old = leaving && leaving.id !== id ? leaving : null;
  return <span className={`aui-running-now-swap ${className}`}>
    {old && <span key={`old-${old.id}`} className="aui-running-now-swap-item" data-leaving aria-hidden="true">{old.node}</span>}
    <span key={id} className="aui-running-now-swap-item" data-entering={old ? true : undefined}>{children}</span>
  </span>;
}

/** Status-only mark for rows without a step count: same 14px box as the ring, no progress implied. */
function StateMark({ status }: { status: RunningNowStatus }) {
  return <span className="aui-running-now-state" data-status={status} aria-hidden="true" />;
}

function Progress({ status, ratio }: { status: RunningNowStatus; ratio: number }) {
  const circumference = 2 * Math.PI * 2;
  return <svg className="aui-running-now-progress" data-tone={runningNowTone(status, ratio)} width="14" height="14" viewBox="0 0 14 14" aria-hidden="true" focusable="false">
    <circle cx="7" cy="7" r="6" fill="none" strokeWidth="1.5" />
    <circle className="aui-running-now-progress-fill" cx="7" cy="7" r="2" fill="none" strokeWidth="4" strokeDasharray={`${ratio * circumference} ${circumference}`} transform="rotate(-90 7 7)" />
  </svg>;
}

export function RunningNow({ tasks: rawTasks, open: openProp, defaultOpen = false, onOpenChange, align = "end", labels: labelProp, onPause, onResume, onOpenTask, onViewAll, className, style }: RunningNowProps) {
  const labels: RunningNowLabels = { ...defaults, ...labelProp, waitingOne: labelProp?.waitingOne ?? labelProp?.waitingMany ?? defaults.waitingOne };
  const tasks = normalize(rawTasks);
  const uid = useId().replace(/[^a-zA-Z0-9_-]/g, "");
  const sheet = useMedia("(max-width: 899px)");
  const narrow359 = useMedia("(max-width: 359px)");
  const narrow419 = useMedia("(max-width: 419px)");
  const coarse = useMedia("(pointer: coarse)");
  const [innerOpen, setInnerOpen] = useState(defaultOpen);
  const open = openProp ?? innerOpen;
  const request = useCallback((next: boolean) => {
    onOpenChange?.(next);
    if (openProp === undefined) setInnerOpen(next);
  }, [onOpenChange, openProp]);

  const running = tasks.filter((task) => task.status === "running").length;
  const waiting = tasks.filter((task) => task.status === "waiting").length;
  const paused = tasks.filter((task) => task.status === "paused").length;
  const both = running > 0 && waiting > 0;
  const hideWords = both && (narrow359 || (narrow419 && fmt(running).length + fmt(waiting).length > 4));
  const waitingWord = waiting === 1 ? labels.waitingOne : labels.waitingMany;
  const summaryText = running || waiting
    ? [running ? `${fmt(running)} ${labels.running}` : "", waiting ? `${fmt(waiting)} ${waitingWord}` : ""].filter(Boolean).join(", ")
    : paused ? `${fmt(paused)} ${labels.paused}` : labels.idle;

  const rootRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const dialogRef = useRef<HTMLDialogElement>(null);
  const listRef = useRef<HTMLUListElement>(null);
  const bodyRefs = useRef(new Map<string, HTMLButtonElement>());
  const pendingFocus = useRef<"first" | "last" | null>(null);
  const lastFocus = useRef<{ id: string; index: number } | null>(null);
  const [shift, setShift] = useState<number | null>(null);
  // Touch full reading: a clipped title or step expands in place on tap; detail stays a separate request.
  const [expandedId, setExpandedId] = useState<string | null>(null);
  const [clipped, setClipped] = useState<string>("");

  // Desktop: W = min(348, viewport − 32) at the preferred edge, nudged to stay 16px inside both edges.
  const place = useCallback(() => {
    const root = rootRef.current;
    const trigger = triggerRef.current;
    if (!root || !trigger) return;
    const width = Math.min(348, window.innerWidth - 32);
    const rect = trigger.getBoundingClientRect();
    const rootRect = root.getBoundingClientRect();
    let left = align === "end" ? rect.right - width : rect.left;
    if (left + width > window.innerWidth - 16) left = window.innerWidth - 16 - width;
    if (left < 16) left = 16;
    setShift(Math.round(left - rootRect.left));
  }, [align]);
  useLayoutEffect(() => {
    if (!open || sheet) return;
    place();
    window.addEventListener("resize", place);
    const observer = typeof ResizeObserver === "undefined" ? undefined : new ResizeObserver(place);
    if (triggerRef.current) observer?.observe(triggerRef.current);
    return () => { window.removeEventListener("resize", place); observer?.disconnect(); };
  }, [open, sheet, place]);

  const focusRow = useCallback((which: "first" | "last" | number) => {
    const bodies = [...(listRef.current?.querySelectorAll<HTMLButtonElement>("[data-row-body]") ?? [])];
    const target = which === "first" ? bodies[0] : which === "last" ? bodies[bodies.length - 1] : bodies[Math.max(0, Math.min(bodies.length - 1, which))];
    if (target) { target.focus(); return; }
    const dialog = dialogRef.current;
    const firstButton = dialog?.querySelector<HTMLButtonElement>("button:not([data-close])") ?? dialog?.querySelector<HTMLButtonElement>("button");
    if (firstButton && !sheet) firstButton.focus();
    else (firstButton ?? dialog)?.focus();
  }, [sheet]);

  useLayoutEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    if (!open) { if (dialog.open) dialog.close(); return; }
    let modal = false;
    try { modal = dialog.matches(":modal"); } catch { modal = false; }
    if (dialog.open && modal !== sheet) dialog.close();
    if (!dialog.open) {
      const keep = document.activeElement as HTMLElement | null;
      try { if (sheet) dialog.showModal(); else dialog.show(); } catch { dialog.setAttribute("open", ""); }
      // Pointer opening does not move focus into a desktop list.
      if (!sheet && !pendingFocus.current && keep && keep !== document.body) keep.focus({ preventScroll: true });
    }
    if (pendingFocus.current) { focusRow(pendingFocus.current); pendingFocus.current = null; }
  }, [open, sheet, focusRow]);

  const close = useCallback((restore: boolean) => {
    request(false);
    if (restore) window.setTimeout(() => triggerRef.current?.focus(), 0);
  }, [request]);

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    const cancel = (event: Event) => { event.preventDefault(); close(true); };
    dialog.addEventListener("cancel", cancel);
    return () => dialog.removeEventListener("cancel", cancel);
  }, [close]);

  useEffect(() => {
    if (!open) return;
    const outside = (event: PointerEvent) => {
      const dialog = dialogRef.current;
      const target = event.target as Node;
      if (dialog && target === dialog) {
        const rect = dialog.getBoundingClientRect();
        if (event.clientX < rect.left || event.clientX > rect.right || event.clientY < rect.top || event.clientY > rect.bottom) close(true);
        return;
      }
      if (rootRef.current?.contains(target)) return;
      request(false);
    };
    document.addEventListener("pointerdown", outside);
    return () => document.removeEventListener("pointerdown", outside);
  }, [open, close, request]);

  // Focus moving to a real target outside the root closes without taking focus back.
  const onBlur = (event: ReactFocusEvent<HTMLDivElement>) => {
    const next = event.relatedTarget as Node | null;
    if (!open || !next || rootRef.current?.contains(next)) return;
    request(false);
  };

  // Restore focus when the focused row or action disappears after an update.
  useLayoutEffect(() => {
    if (!open || !lastFocus.current) return;
    const active = document.activeElement;
    if (active && active !== document.body && active.isConnected) return;
    const { id, index } = lastFocus.current;
    const same = bodyRefs.current.get(id);
    if (same?.isConnected) { same.focus(); return; }
    focusRow(index);
  });

  const onTriggerKey = (event: ReactKeyboardEvent<HTMLButtonElement>) => {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      const which = event.key === "ArrowDown" ? "first" : "last";
      if (open) focusRow(which);
      else { pendingFocus.current = which; request(true); }
    } else if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      if (open) request(false);
      else { pendingFocus.current = "first"; request(true); }
    } else if (event.key === "Escape" && open) {
      event.preventDefault();
      request(false);
    }
  };
  const onPanelKey = (event: ReactKeyboardEvent<HTMLElement>) => {
    if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); close(true); return; }
    const bodies = [...(listRef.current?.querySelectorAll<HTMLButtonElement>("[data-row-body]") ?? [])];
    if (!bodies.length) return;
    const row = (event.target as HTMLElement).closest<HTMLElement>("[data-row]");
    const index = row ? bodies.findIndex((body) => row.contains(body)) : -1;
    let next = -1;
    if (event.key === "ArrowDown") next = index < 0 ? 0 : Math.min(bodies.length - 1, index + 1);
    else if (event.key === "ArrowUp") next = index < 0 ? bodies.length - 1 : Math.max(0, index - 1);
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = bodies.length - 1;
    else return;
    event.preventDefault();
    bodies[next]?.focus();
  };

  // Only an existing task turning to waiting or done is announced.
  const previous = useRef<Map<string, RunningNowStatus> | null>(null);
  const [announcement, setAnnouncement] = useState("");
  const statusKey = tasks.map((task) => `${task.id}:${task.status}`).join("|");
  useEffect(() => {
    const before = previous.current;
    const now = new Map(tasks.map((task) => [task.id, task.status]));
    previous.current = now;
    if (!before) return;
    const lines = tasks.filter((task) => before.has(task.id) && before.get(task.id) !== task.status && (task.status === "waiting" || task.status === "done"))
      .map((task) => task.status === "waiting" ? `${task.title}: ${labels.waitingOne}.` : `${task.title}: ${labels.done}.`);
    if (lines.length) setAnnouncement(lines.join(" "));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [statusKey]);

  useLayoutEffect(() => {
    if (!open || !coarse) { if (clipped) setClipped(""); return; }
    const ids: string[] = [];
    bodyRefs.current.forEach((body, id) => {
      if (id === expandedId) { ids.push(id); return; }
      const parts = body.querySelectorAll<HTMLElement>(".aui-running-now-task-title, .aui-running-now-step > span > span");
      if ([...parts].some((part) => part.scrollHeight > part.clientHeight + 1 || part.scrollWidth > part.clientWidth + 1)) ids.push(id);
    });
    const key = ids.sort().join("|");
    if (key !== clipped) setClipped(key);
  });
  const clippedIds = new Set(clipped ? clipped.split("|") : []);
  useEffect(() => { if (!open) setExpandedId(null); }, [open]);
  const activateBody = (id: string) => {
    if (coarse && (expandedId === id || clippedIds.has(id))) { setExpandedId((current) => current === id ? null : id); return; }
    onOpenTask?.(id);
  };

  const rowH = sheet ? 56 : undefined;
  const panelStyle = { "--aui-running-now-shift": shift === null ? undefined : `${shift}px`, "--aui-running-now-row-h": rowH ? `${rowH}px` : undefined } as CSSProperties;

  return <div ref={rootRef} className={["aui-running-now", className].filter(Boolean).join(" ")} style={style} data-align={align} onBlur={onBlur}>
    <style>{runningNowStyles}</style>
    <button
      ref={triggerRef}
      type="button"
      className="aui-running-now-trigger"
      aria-haspopup="dialog"
      aria-expanded={open}
      aria-controls={`${uid}-panel`}
      aria-label={`${labels.title}: ${summaryText}`}
      onClick={(event) => { if (event.detail !== 0) request(!open); }}
      onKeyDown={onTriggerKey}
    >
      {running > 0 || waiting > 0 ? <>
        {running > 0 && <span className="aui-running-now-part"><span className="aui-running-now-ring" data-tone="running" aria-hidden="true" /><Swap id={`r${running}`} className="aui-running-now-count">{fmt(running)}</Swap>{!hideWords && <span className="aui-running-now-word">{labels.running}</span>}</span>}
        {both && <span className="aui-running-now-sep" aria-hidden="true">·</span>}
        {waiting > 0 && <span className="aui-running-now-part"><span className="aui-running-now-dot" aria-hidden="true" /><Swap id={`w${waiting}`} className="aui-running-now-count">{fmt(waiting)}</Swap>{!hideWords && <span className="aui-running-now-word">{waitingWord}</span>}</span>}
      </> : paused > 0 ? <span className="aui-running-now-part"><span className="aui-running-now-ring" data-tone="paused" aria-hidden="true" /><Swap id={`p${paused}`} className="aui-running-now-count">{fmt(paused)}</Swap><span className="aui-running-now-word">{labels.paused}</span></span>
        : <span className="aui-running-now-part"><span className="aui-running-now-ring" data-tone="idle" aria-hidden="true" /><span className="aui-running-now-word">{labels.idle}</span></span>}
      <ChevronDown className="aui-running-now-chevron" size={12} strokeWidth={2.2} aria-hidden="true" />
    </button>
    <dialog ref={dialogRef} id={`${uid}-panel`} className="aui-running-now-panel" aria-labelledby={`${uid}-title`} tabIndex={-1} style={panelStyle} onKeyDown={onPanelKey}>
      <div className="aui-running-now-head">
        <h3 id={`${uid}-title`} className="aui-running-now-title">{labels.title}</h3>
        <span className="aui-running-now-total">{fmt(tasks.length)}</span>
        <button type="button" data-close className="aui-running-now-close" aria-label={labels.close} onClick={() => close(true)}><X size={16} aria-hidden="true" /></button>
      </div>
      <div className="aui-running-now-card">
        {tasks.length === 0 ? <p className="aui-running-now-empty">{labels.empty}</p> : <ul ref={listRef} className="aui-running-now-list">
          {tasks.map((task, index) => {
            const stepText = task.status === "done" ? labels.done : task.status === "paused" ? `${labels.paused} · ${task.step}` : task.step;
            const counts = `${fmt(task.stepsDone)}/${fmt(task.stepsTotal)}`;
            const name = [task.title, task.status === "waiting" ? labels.waitingOne : task.status === "done" ? labels.done : task.status === "paused" ? labels.paused : labels.running, task.status !== "done" && task.step ? task.step : "", task.hasProgress ? labels.steps(fmt(task.stepsDone), fmt(task.stepsTotal)) : ""].filter(Boolean).join(", ");
            const toggle = task.status === "running" ? onPause : task.status === "paused" ? onResume : undefined;
            const expanded = coarse && expandedId === task.id;
            const expandable = coarse && (expanded || clippedIds.has(task.id));
            return <li key={task.id} className="aui-running-now-row" data-row data-status={task.status} data-expanded={expanded || undefined} onFocus={() => { lastFocus.current = { id: task.id, index }; }}>
              <button
                ref={(element) => { if (element) bodyRefs.current.set(task.id, element); else bodyRefs.current.delete(task.id); }}
                type="button"
                data-row-body
                className="aui-running-now-body"
                aria-label={name}
                aria-expanded={expandable ? expanded : undefined}
                aria-controls={expandable && expanded && onOpenTask && task.status !== "waiting" ? `${uid}-more-${index}` : undefined}
                title={`${task.title}\n${stepText}`}
                onClick={() => activateBody(task.id)}
              >
                {task.hasProgress ? <Progress status={task.status} ratio={task.ratio} /> : <StateMark status={task.status} />}
                <span className="aui-running-now-copy">
                  <span className="aui-running-now-task-title" dir="auto">{task.title}</span>
                  <Swap id={`${task.status}:${stepText}`} className="aui-running-now-step"><span dir="auto">{stepText}</span></Swap>
                </span>
                {task.status === "waiting" ? <span className="aui-running-now-badge">{labels.waitingOne}</span> : task.hasProgress ? <Swap id={counts} className="aui-running-now-steps">{counts}</Swap> : null}
              </button>
              <span className="aui-running-now-action-cell">
                {toggle && <button
                  type="button"
                  className="aui-running-now-icon-button"
                  aria-label={`${task.status === "running" ? labels.pause : labels.resume}: ${task.title}`}
                  onClick={() => toggle(task.id)}
                >{task.status === "running" ? <Pause size={14} aria-hidden="true" /> : <Play size={14} aria-hidden="true" />}</button>}
                {task.status === "waiting" && onOpenTask && <button type="button" className="aui-running-now-open" aria-label={`${labels.open}: ${task.title}`} onClick={() => onOpenTask(task.id)}>{labels.open}</button>}
                {task.status === "done" && <Check className="aui-running-now-check" size={14} strokeWidth={2.4} aria-hidden="true" />}
              </span>
              {expanded && onOpenTask && task.status !== "waiting" && <span className="aui-running-now-more" id={`${uid}-more-${index}`}>
                <button type="button" className="aui-running-now-detail" aria-label={`${labels.open}: ${task.title}`} onClick={() => onOpenTask(task.id)}>{labels.open}<ChevronRight size={12} aria-hidden="true" /></button>
              </span>}
            </li>;
          })}
        </ul>}
      </div>
      {onViewAll && <button type="button" className="aui-running-now-all" onClick={onViewAll}>
        <ClipboardCheck size={14} aria-hidden="true" /><span>{labels.viewAll}</span><ChevronRight className="aui-running-now-all-arrow" size={12} aria-hidden="true" />
      </button>}
    </dialog>
    <span className="aui-running-now-sr" role="status" aria-live="polite" aria-atomic="true">{announcement}</span>
  </div>;
}

const runningNowStyles = `
@layer kit {
.aui-running-now {
  --aui-running-now-tray: var(--aui-running-now-tray-bg, var(--aui-bg-2, #f0f0f0));
  --aui-running-now-tray-hover: var(--aui-running-now-tray-hover-bg, #e8e8e8);
  --aui-running-now-card: var(--aui-running-now-card-bg, var(--aui-bg-1, #ffffff));
  --aui-running-now-ink: var(--aui-fg, #171717);
  --aui-running-now-text: var(--aui-fg-dim, #6b6b6b);
  --aui-running-now-muted: var(--aui-fg-muted, #8a8a8a);
  --aui-running-now-faint: var(--aui-fg-faint, #b8b8b8);
  --aui-running-now-icon: var(--aui-running-now-icon-color, #8a8a8a);
  --aui-running-now-line: var(--aui-line, #ececec);
  --aui-running-now-row-hover: var(--aui-running-now-row-hover-bg, #f5f5f5);
  --aui-running-now-button-hover: var(--aui-running-now-button-hover-bg, #ededed);
  --aui-running-now-open-bg: var(--aui-running-now-open-button-bg, #f2f2f2);
  --aui-running-now-teal: var(--aui-running-now-high, #13b3a0);
  --aui-running-now-amber: var(--aui-running-now-mid, #e8b000);
  --aui-running-now-orange: var(--aui-running-now-low, #f28a2e);
  --aui-running-now-blue: var(--aui-running-now-done-color, #0190fe);
  --aui-running-now-badge-fg: var(--aui-running-now-waiting-fg, #8f6c00);
  --aui-running-now-badge-bg: var(--aui-running-now-waiting-bg, #f9f1d6);
  --aui-running-now-focus: var(--aui-running-now-focus-ring, var(--aui-focus, rgb(0 96 226 / .7)));
  --aui-running-now-shadow: var(--aui-running-now-panel-shadow, 0 0 0 1px var(--aui-running-now-line), 0 1px 2px rgb(0 0 0 / .04), 0 14px 32px -12px rgb(0 0 0 / .18));
  --aui-running-now-card-shadow: 0 0 0 1px var(--aui-running-now-line), 0 1px 2px rgb(0 0 0 / .04);
  --aui-running-now-ease: cubic-bezier(0.32, 0.72, 0.24, 1);
  --aui-running-now-r: var(--aui-r-xl, 16px);
  --aui-running-now-r-md: var(--aui-r-md, 10px);
  --aui-running-now-r-sm: var(--aui-r-sm, 7px);
  --aui-running-now-row-h: 52px;
  position: relative; display: inline-block; flex: none; max-width: 100%; box-sizing: border-box; vertical-align: top;
  color: var(--aui-running-now-text); font: 400 13px/20px var(--aui-font-sans, Inter, ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif); font-variant-numeric: tabular-nums;
  word-break: keep-all; overflow-wrap: anywhere; -webkit-tap-highlight-color: transparent;
}
:where(.dark, [data-theme="dark"]) .aui-running-now {
  --aui-running-now-tray: var(--aui-running-now-tray-bg, var(--aui-bg-2, #19191c));
  --aui-running-now-tray-hover: var(--aui-running-now-tray-hover-bg, #222226);
  --aui-running-now-card: var(--aui-running-now-card-bg, var(--aui-bg-1, #232326));
  --aui-running-now-ink: var(--aui-fg, #ededed);
  --aui-running-now-text: var(--aui-fg-dim, #a1a1a6);
  --aui-running-now-muted: var(--aui-fg-muted, #7c7c82);
  --aui-running-now-faint: var(--aui-fg-faint, #4a4a50);
  --aui-running-now-icon: var(--aui-running-now-icon-color, #8a8a90);
  --aui-running-now-line: var(--aui-line, #2f2f34);
  --aui-running-now-row-hover: var(--aui-running-now-row-hover-bg, #2a2a2e);
  --aui-running-now-button-hover: var(--aui-running-now-button-hover-bg, #323237);
  --aui-running-now-open-bg: var(--aui-running-now-open-button-bg, #2c2c30);
  --aui-running-now-teal: var(--aui-running-now-high, #4fc3b4);
  --aui-running-now-amber: var(--aui-running-now-mid, #e6c24d);
  --aui-running-now-orange: var(--aui-running-now-low, #f2a15c);
  --aui-running-now-blue: var(--aui-running-now-done-color, #4fb0fe);
  --aui-running-now-badge-fg: var(--aui-running-now-waiting-fg, #e6c24d);
  --aui-running-now-badge-bg: var(--aui-running-now-waiting-bg, #332a0f);
  --aui-running-now-focus: var(--aui-running-now-focus-ring, var(--aui-focus, rgb(77 141 255 / .8)));
  --aui-running-now-shadow: var(--aui-running-now-panel-shadow, 0 0 0 1px var(--aui-running-now-line), 0 16px 32px -12px rgb(0 0 0 / .7));
  --aui-running-now-card-shadow: 0 0 0 1px var(--aui-running-now-line), 0 1px 2px rgb(0 0 0 / .3);
}
.aui-running-now *, .aui-running-now *::before, .aui-running-now *::after { box-sizing: border-box; }
.aui-running-now :where(button) { font: inherit; letter-spacing: inherit; color: inherit; cursor: pointer; touch-action: manipulation; -webkit-tap-highlight-color: transparent; }
.aui-running-now :is(button, [tabindex="0"]):focus-visible { outline: 2px solid var(--aui-running-now-focus); outline-offset: 2px; }
.aui-running-now .aui-running-now-body:focus-visible { outline-offset: -2px; background: var(--aui-running-now-row-hover); }
.aui-running-now-sr { position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px; overflow: hidden; clip-path: inset(50%); white-space: nowrap; border: 0; }
.aui-running-now-trigger { display: inline-flex; align-items: center; gap: 8px; max-width: 100%; height: 30px; padding: 0 9px 0 11px; border: 0; border-radius: calc(var(--aui-running-now-r-md) - 2px); background: var(--aui-running-now-tray); color: var(--aui-running-now-text); font-size: 12px; line-height: 16px; white-space: nowrap; transition: background-color 140ms ease, color 140ms ease; }
.aui-running-now-trigger:hover, .aui-running-now-trigger[aria-expanded="true"] { background: var(--aui-running-now-tray-hover); }
.aui-running-now-part { display: inline-flex; align-items: center; gap: 4px; min-width: 0; }
.aui-running-now-count { color: var(--aui-running-now-ink); font-weight: 500; }
.aui-running-now-word { overflow: hidden; text-overflow: ellipsis; }
.aui-running-now-ring { flex: none; width: 8px; height: 8px; margin-right: 2px; border: 1.5px solid var(--aui-running-now-teal); border-radius: 50%; }
.aui-running-now-ring[data-tone="paused"] { border-color: var(--aui-running-now-muted); }
.aui-running-now-ring[data-tone="idle"] { border-color: var(--aui-running-now-faint); }
.aui-running-now-dot { flex: none; width: 6px; height: 6px; margin: 0 3px 0 1px; border-radius: 50%; background: var(--aui-running-now-amber); }
.aui-running-now-sep { color: var(--aui-running-now-faint); }
.aui-running-now-chevron { flex: none; margin-left: -2px; color: var(--aui-running-now-icon); transition: transform 200ms var(--aui-running-now-ease); }
.aui-running-now-trigger[aria-expanded="true"] .aui-running-now-chevron { transform: rotate(180deg); }
.aui-running-now-swap { display: inline-grid; min-width: 0; overflow: hidden; vertical-align: bottom; }
.aui-running-now-swap-item { grid-area: 1 / 1; min-width: 0; }
.aui-running-now-swap-item[data-entering] { animation: aui-running-now-in 340ms var(--aui-running-now-ease) both; }
.aui-running-now-swap-item[data-leaving] { animation: aui-running-now-out 340ms var(--aui-running-now-ease) both; pointer-events: none; }
.aui-running-now-panel { position: absolute; inset: auto; top: calc(100% + 6px); left: var(--aui-running-now-shift, auto); right: auto; z-index: 50; display: none; width: min(348px, calc(100vw - 32px)); max-width: none; margin: 0; padding: 4px; border: 0; border-radius: var(--aui-running-now-r); background: var(--aui-running-now-tray); color: var(--aui-running-now-text); box-shadow: var(--aui-running-now-shadow); font: inherit; transform-origin: top right; animation: aui-running-now-pop 200ms var(--aui-running-now-ease) both; outline: none; }
.aui-running-now[data-align="start"] .aui-running-now-panel { transform-origin: top left; }
.aui-running-now-panel[open] { display: flex; flex-direction: column; }
.aui-running-now-head { display: flex; flex: none; align-items: center; gap: 6px; height: 32px; margin-bottom: 2px; padding: 0 10px; }
.aui-running-now-title { margin: 0; color: var(--aui-running-now-ink); font-size: 13px; font-weight: 500; line-height: 20px; }
.aui-running-now-total { color: var(--aui-running-now-muted); font-size: 12px; line-height: 16px; }
.aui-running-now-close { display: none; }
.aui-running-now-card { flex: 0 1 auto; min-height: 0; padding: 4px; overflow: hidden; border-radius: calc(var(--aui-running-now-r) - 4px); background: var(--aui-running-now-card); box-shadow: var(--aui-running-now-card-shadow); }
.aui-running-now-list { max-height: min(calc(var(--aui-running-now-row-h) * 5.5), max(calc(var(--aui-running-now-row-h) * 2.5), calc(100dvh - 200px))); margin: 0; padding: 0; overflow-y: auto; overscroll-behavior: contain; list-style: none; }
.aui-running-now-row { display: flex; align-items: stretch; height: var(--aui-running-now-row-h); border-radius: calc(var(--aui-running-now-r-sm) + 1px); transition: background-color 140ms ease; }
.aui-running-now-row:hover { background: var(--aui-running-now-row-hover); }
.aui-running-now-body { display: flex; flex: 1 1 auto; align-items: center; gap: 10px; min-width: 0; height: 100%; padding: 0 6px 0 9px; border: 0; border-radius: calc(var(--aui-running-now-r-sm) + 1px); background: transparent; text-align: left; }
.aui-running-now-progress { flex: none; }
.aui-running-now-state { display: inline-grid; flex: none; place-items: center; width: 14px; height: 14px; }
.aui-running-now-state::before { content: ""; width: 8px; height: 8px; border-radius: 50%; background: var(--aui-running-now-teal); }
.aui-running-now-state[data-status="paused"]::before { background: var(--aui-running-now-muted); }
.aui-running-now-state[data-status="waiting"]::before { background: var(--aui-running-now-amber); }
.aui-running-now-state[data-status="done"]::before { background: var(--aui-running-now-blue); }
.aui-running-now-progress circle:first-child { stroke: var(--aui-running-now-teal); transition: stroke 400ms ease; }
.aui-running-now-progress-fill { stroke: var(--aui-running-now-teal); transition: stroke-dasharray 700ms var(--aui-running-now-ease), stroke 400ms ease; }
.aui-running-now-progress[data-tone="mid"] circle { stroke: var(--aui-running-now-amber); }
.aui-running-now-progress[data-tone="low"] circle { stroke: var(--aui-running-now-orange); }
.aui-running-now-progress[data-tone="paused"] circle { stroke: var(--aui-running-now-muted); }
.aui-running-now-progress[data-tone="done"] circle { stroke: var(--aui-running-now-blue); }
.aui-running-now-copy { display: grid; flex: 1 1 auto; min-width: 0; }
.aui-running-now-task-title { overflow: hidden; color: var(--aui-running-now-ink); font-size: 13px; font-weight: 500; line-height: 20px; text-overflow: ellipsis; white-space: nowrap; }
.aui-running-now-row[data-status="done"] .aui-running-now-task-title { color: var(--aui-running-now-text); }
.aui-running-now-step { display: grid; color: var(--aui-running-now-muted); font-size: 12px; line-height: 16px; }
.aui-running-now-step > span > span { display: block; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.aui-running-now-steps { flex: none; color: var(--aui-running-now-muted); font-size: 12px; line-height: 16px; }
.aui-running-now-badge { flex: none; height: 20px; padding: 0 7px; border-radius: calc(var(--aui-running-now-r-sm) - 2px); background: var(--aui-running-now-badge-bg); color: var(--aui-running-now-badge-fg); font-size: 11.5px; font-weight: 500; line-height: 20px; white-space: nowrap; animation: aui-running-now-appear 260ms var(--aui-running-now-ease) both; }
.aui-running-now-action-cell { display: inline-flex; flex: none; align-items: center; justify-content: center; min-width: 40px; padding-right: 6px; }
.aui-running-now-icon-button { display: inline-grid; place-items: center; width: 28px; height: 28px; padding: 0; border: 0; border-radius: var(--aui-running-now-r-sm); background: transparent; color: var(--aui-running-now-icon); transition: background-color 140ms ease, color 140ms ease; }
.aui-running-now-icon-button:hover { background: var(--aui-running-now-button-hover); color: var(--aui-running-now-ink); }
.aui-running-now-open { height: 28px; padding: 0 9px; border: 0; border-radius: var(--aui-running-now-r-sm); background: var(--aui-running-now-open-bg); color: var(--aui-running-now-ink); font-size: 12px; font-weight: 500; line-height: 16px; white-space: nowrap; transition: background-color 140ms ease; animation: aui-running-now-appear 260ms var(--aui-running-now-ease) both; }
.aui-running-now-open:hover { background: var(--aui-running-now-button-hover); }
.aui-running-now-check { color: var(--aui-running-now-blue); }
.aui-running-now-row[data-expanded] { flex-wrap: wrap; height: auto; background: var(--aui-running-now-row-hover); }
.aui-running-now-row[data-expanded] .aui-running-now-body { flex: 1 1 0; height: auto; min-height: var(--aui-running-now-row-h); padding-block: 8px; align-items: flex-start; }
.aui-running-now-row[data-expanded] :is(.aui-running-now-task-title, .aui-running-now-step > span > span) { display: block; overflow: visible; white-space: normal; overflow-wrap: anywhere; text-overflow: clip; -webkit-line-clamp: unset; }
.aui-running-now-row[data-expanded] .aui-running-now-action-cell { align-items: flex-start; padding-top: 6px; }
.aui-running-now-more { display: flex; flex: 1 1 100%; padding: 0 6px 8px 33px; }
.aui-running-now-detail { display: inline-flex; align-items: center; gap: 4px; height: 44px; padding: 0 14px; border: 0; border-radius: var(--aui-running-now-r-sm); background: var(--aui-running-now-open-bg); color: var(--aui-running-now-ink); font-size: 13px; font-weight: 500; line-height: 16px; white-space: nowrap; }
.aui-running-now-detail:hover { background: var(--aui-running-now-button-hover); }
.aui-running-now-empty { margin: 0; padding: 14px 8px 16px; color: var(--aui-running-now-muted); font-size: 12px; line-height: 16px; text-align: center; }
.aui-running-now-all { display: flex; flex: none; align-items: center; gap: 8px; width: 100%; height: 32px; margin-top: 4px; padding: 0 9px 0 13px; border: 0; border-radius: calc(var(--aui-running-now-r-sm) + 3px); background: transparent; color: var(--aui-running-now-text); font-size: 12px; font-weight: 500; transition: background-color 140ms ease, color 140ms ease; }
.aui-running-now-all > span { flex: 1 1 auto; text-align: left; }
.aui-running-now-all:hover { background: var(--aui-running-now-tray-hover); color: var(--aui-running-now-ink); }
.aui-running-now-all-arrow { transition: transform 200ms var(--aui-running-now-ease); }
.aui-running-now-all:hover .aui-running-now-all-arrow { transform: translateX(2px); }
@media (max-width: 899px) {
  .aui-running-now-panel, .aui-running-now-panel:modal { position: fixed; inset: auto 0 0 0; top: auto; width: 100%; max-height: min(85dvh, calc(100dvh - env(safe-area-inset-top) - 24px)); padding: 6px max(8px, env(safe-area-inset-right)) max(10px, env(safe-area-inset-bottom)) max(8px, env(safe-area-inset-left)); border-radius: 20px 20px 0 0; animation: aui-running-now-sheet 240ms var(--aui-running-now-ease) both; }
  .aui-running-now-panel::backdrop { background: rgb(0 0 0 / .36); }
  .aui-running-now-head { height: 48px; }
  .aui-running-now-title { font-size: 15px; font-weight: 600; }
  .aui-running-now-close { display: inline-grid; place-items: center; width: 44px; height: 44px; margin-left: auto; margin-right: -6px; padding: 0; border: 0; border-radius: 999px; background: transparent; color: var(--aui-running-now-muted); }
  .aui-running-now-card { display: flex; flex-direction: column; flex: 1 1 auto; }
  .aui-running-now-list { flex: 1 1 auto; max-height: none; }
  .aui-running-now-task-title, .aui-running-now-step > span > span { white-space: normal; overflow-wrap: anywhere; display: -webkit-box; -webkit-box-orient: vertical; -webkit-line-clamp: 2; }
  .aui-running-now-row { height: auto; min-height: var(--aui-running-now-row-h); }
  .aui-running-now-body { min-height: var(--aui-running-now-row-h); padding-block: 6px; }
}
@media (hover: none) and (pointer: coarse) {
  .aui-running-now { --aui-running-now-row-h: 56px; }
}
@media (pointer: coarse), (max-width: 899px) {
  .aui-running-now-trigger { height: 44px; padding: 0 12px 0 14px; }
  .aui-running-now-icon-button { width: 44px; height: 44px; }
  .aui-running-now-open { height: 44px; padding: 0 14px; }
  .aui-running-now-action-cell { min-width: 44px; padding-right: 4px; }
  .aui-running-now-all { height: 44px; }
  .aui-running-now-row, .aui-running-now-body { min-height: 56px; }
}
@keyframes aui-running-now-in { from { opacity: 0; transform: translateY(60%); } to { opacity: 1; transform: none; } }
@keyframes aui-running-now-out { from { opacity: 1; transform: none; } to { opacity: 0; transform: translateY(-60%); } }
@keyframes aui-running-now-pop { from { opacity: 0; transform: translateY(-4px) scale(.98); } to { opacity: 1; transform: none; } }
@keyframes aui-running-now-sheet { from { opacity: 0; transform: translateY(24px); } to { opacity: 1; transform: none; } }
@keyframes aui-running-now-appear { from { opacity: 0; transform: scale(.92); } to { opacity: 1; transform: none; } }
@media (prefers-reduced-motion: reduce) {
  .aui-running-now *, .aui-running-now *::before, .aui-running-now *::after { animation: none !important; transition: none !important; }
  .aui-running-now-swap-item[data-leaving] { display: none; }
}
}
`;
