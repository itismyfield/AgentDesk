import { ChevronDown, Folder, PanelRight } from "lucide-react";
import { useId, useLayoutEffect, useMemo, useRef, useState, type ButtonHTMLAttributes, type CSSProperties, type KeyboardEvent, type ReactNode, type Ref, type RefObject } from "react";
import { createPortal } from "react-dom";
import { EmptyState } from "../../kit/free/empty-state-clean";
import { ROW_HOVER_CLASS, SELECTED_ROW_STYLE, cn } from "../common/KitPrimitives";
import { useIsMobile } from "../common/overlay/useBreakpoint";

// Shared by the campaign and queue tabs: the kit task-list layout (group rows, task rows, a pinned detail pane).
// Lists are `@container/tasks`, so a row folds to two lines when its list is 35rem or narrower.

const DOCK_GAP = 16;

interface DockFrame { portal: HTMLElement; top: number; bottom: number; right: number; width: number; pad: number }

function scrollParent(node: HTMLElement): HTMLElement | null {
  for (let element = node.parentElement; element; element = element.parentElement) {
    const { overflowY } = getComputedStyle(element);
    if (overflowY === "auto" || overflowY === "scroll") return element;
  }
  return null;
}

interface DockState { frame: DockFrame | null; measuredNarrow: boolean }

const EMPTY_DOCK_STATE: DockState = { frame: null, measuredNarrow: false };

/** Where the detail pane sits on wide viewports: pinned to the right of the page's scroll area, from the list's top
 * (or the area's top once scrolled) to its bottom, with the list padded clear of it. */
function useDockFrame(root: RefObject<HTMLElement | null>, enabled: boolean, minContentWidth = 520): DockState {
  const [state, setState] = useState<DockState>(EMPTY_DOCK_STATE);
  useLayoutEffect(() => {
    const node = root.current;
    if (!enabled || !node) {
      setState((current) => (current.frame === null && !current.measuredNarrow ? current : EMPTY_DOCK_STATE));
      return;
    }
    const scroller = scrollParent(node);
    let raf = 0;
    const measure = () => {
      raf = 0;
      const area = scroller?.getBoundingClientRect();
      const clientRight = scroller && area ? area.left + scroller.clientLeft + scroller.clientWidth : document.documentElement.clientWidth;
      const box = node.getBoundingClientRect();
      const width = Math.round(Math.min(480, Math.max(340, window.innerWidth * 0.3)));
      if (box.width > 0 && clientRight - width - DOCK_GAP - box.left < minContentWidth) {
        setState((current) => (current.frame === null && current.measuredNarrow ? current : { frame: null, measuredNarrow: true }));
        return;
      }
      const next: DockFrame = {
        portal: scroller ?? document.body,
        top: Math.max(area?.top ?? 0, box.top),
        bottom: area ? Math.max(0, window.innerHeight - area.bottom) : 0,
        right: Math.max(0, window.innerWidth - clientRight),
        width,
        pad: Math.max(0, Math.round(box.right - (clientRight - width) + DOCK_GAP)),
      };
      setState((current) =>
        !current.measuredNarrow &&
        current.frame &&
        (Object.keys(next) as Array<keyof DockFrame>).every((key) => current.frame![key] === next[key])
          ? current
          : { frame: next, measuredNarrow: false },
      );
    };
    const schedule = () => { if (!raf) raf = requestAnimationFrame(measure); };
    measure();
    scroller?.addEventListener("scroll", schedule, { passive: true });
    window.addEventListener("resize", schedule);
    const observer = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(schedule);
    observer?.observe(node);
    if (scroller) observer?.observe(scroller);
    return () => {
      if (raf) cancelAnimationFrame(raf);
      scroller?.removeEventListener("scroll", schedule);
      window.removeEventListener("resize", schedule);
      observer?.disconnect();
    };
  }, [root, enabled, minContentWidth]);
  return state;
}

/** Wide viewports dock the detail in a pane beside the list (put `dock` in DrawerDockContext); narrower ones open
 * the same Drawer as a slide-over or bottom sheet. Give `root` to the list's wrapper and `rootStyle` to keep it clear of the pane. */
export function useDetailDock(minContentWidth = 520) {
  const docked = !useIsMobile();
  const root = useRef<HTMLDivElement>(null);
  const { frame: rawFrame, measuredNarrow } = useDockFrame(root, docked, minContentWidth);
  const [host, setHost] = useState<HTMLElement | null>(null);
  const canDock = docked && !measuredNarrow;
  const frame = canDock ? rawFrame : null;
  const dock = useMemo(() => (canDock ? { host } : null), [canDock, host]);
  const id = useId();
  const rootStyle: CSSProperties | undefined = frame ? { paddingRight: frame.pad } : undefined;
  return { root, rootStyle, frame, dock, id, setHost };
}

/** The pinned pane itself; until something is opened it says how to fill it. */
export function DetailDockPane({ dock, label, empty }: {
  dock: ReturnType<typeof useDetailDock>; label: string; empty: { title: string; text: string } | null;
}) {
  const { frame, id, setHost } = dock;
  return frame && createPortal(<aside ref={setHost} id={id} aria-label={label}
    className="fixed z-10 flex flex-col overflow-hidden border-l border-[var(--line-soft)] bg-[var(--bg-1)] shadow-[var(--shadow-sm)]"
    style={{ top: frame.top, bottom: frame.bottom, right: frame.right, width: frame.width }}>
    {empty && <div className="grid flex-1 place-items-center p-6">
      <EmptyState bare icon={<PanelRight aria-hidden />} title={empty.title} text={empty.text} />
    </div>}
  </aside>, frame.portal);
}

/** Arrow keys, j/k, Home and End move between the group and row buttons of one list. */
export function moveListFocus(event: KeyboardEvent<HTMLElement>) {
  if (event.altKey || event.metaKey || event.ctrlKey || event.nativeEvent.isComposing) return;
  const target = event.target as HTMLElement;
  if (!target.matches("[data-list-nav]")) return;
  const items = [...event.currentTarget.querySelectorAll<HTMLElement>("[data-list-nav]")];
  const index = items.indexOf(target);
  const key = event.key.length === 1 ? event.key.toLowerCase() : event.key;
  const next = key === "ArrowDown" || key === "j" ? index + 1 : key === "ArrowUp" || key === "k" ? index - 1
    : key === "Home" ? 0 : key === "End" ? items.length - 1 : null;
  if (next === null) return;
  event.preventDefault();
  items[Math.max(0, Math.min(items.length - 1, next))]?.focus();
}

/** A collapsible group row: chevron, icon, name and count, then whatever the group adds at the end. */
export function TaskGroupHeading({ open, controls, label, title, count, icon, className = "", onToggle, children }: {
  open: boolean; controls: string; label: ReactNode; title?: string; count: ReactNode; icon?: ReactNode; className?: string;
  onToggle: () => void; children?: ReactNode;
}) {
  return <button type="button" data-list-nav aria-expanded={open} aria-controls={controls} onClick={onToggle}
    className={`!m-0 flex w-full min-w-0 items-center gap-1.5 !rounded-[var(--r-sm)] !border-0 !bg-transparent !px-2 !py-1.5 text-left !text-[13px] font-medium text-[var(--fg)] hover:!bg-[color-mix(in_oklch,var(--fg)_5%,transparent)] ${className}`}>
    <ChevronDown aria-hidden className={`size-3.5 shrink-0 text-[var(--fg-muted)] transition-transform motion-reduce:transition-none ${open ? "" : "-rotate-90"}`} />
    {icon ?? <Folder aria-hidden className="size-3.5 shrink-0 text-[var(--fg-muted)]" />}
    <span className="min-w-0 truncate" title={title}>{label}</span>
    {/* The space keeps the count a separate word for readers; a flex item drops it visually. */}
    <span className="shrink-0 text-xs font-normal tabular-nums text-[var(--fg-faint)]"> {count}</span>
    {children}
  </button>;
}

/** One task as a list row: marker, number and title, a muted line (beside the title, or under it in a narrow list),
 * an optional note under both, then the row's flags and its quiet trailing value. */
export function TaskRow({ marker, number, title, fullTitle, meta, note, flags, trailing, selected, action, buttonRef, className = "", numberClassName = "", textClassName = "", ...button }:
  Omit<ButtonHTMLAttributes<HTMLButtonElement>, "title"> & {
    marker: ReactNode; number?: string | null; title: string; fullTitle?: string; meta?: ReactNode; note?: ReactNode;
    flags?: ReactNode; trailing?: ReactNode; selected: boolean; action?: ReactNode; buttonRef?: Ref<HTMLButtonElement>;
    numberClassName?: string; textClassName?: string;
  }) {
  return <div className={cn("flex min-w-0 items-center gap-1 rounded-[var(--r-sm)] pr-1.5 motion-reduce:transition-none", !selected && ROW_HOVER_CLASS)} style={selected ? SELECTED_ROW_STYLE : undefined}>
    <button ref={buttonRef} type="button" data-list-nav title={fullTitle ?? title} {...button}
      className={cn("!m-0 flex min-w-0 flex-1 gap-3 !rounded-[var(--r-sm)] !border-0 !bg-transparent !px-3 !py-2 text-left !text-[13px] leading-5 text-[var(--fg)] @max-[35rem]/tasks:items-start @max-[35rem]/tasks:!px-2.5", note ? "items-start" : "items-center", className)}>
      <span className={`flex shrink-0 ${note ? "mt-[3px]" : `@max-[35rem]/tasks:mt-[3px]`}`}>{marker}</span>
      <span className="grid min-w-0 flex-1 gap-1">
        <span className={`flex min-w-0 items-baseline gap-2.5 @max-[35rem]/tasks:flex-col @max-[35rem]/tasks:gap-0.5 ${textClassName}`}>
          <span className={`flex min-w-0 shrink items-baseline gap-2 @max-[35rem]/tasks:max-w-full`}>
            {number && <span className={`shrink-0 text-xs tabular-nums text-[var(--fg-faint)] ${numberClassName}`}>{number}</span>}
            <strong className={`min-w-0 truncate font-medium [overflow-wrap:anywhere] [word-break:keep-all] @max-[35rem]/tasks:line-clamp-2 @max-[35rem]/tasks:whitespace-normal`}>{title}</strong>
          </span>
          {meta && <span className={`min-w-0 shrink-[100] truncate text-xs text-[var(--fg-muted)] @max-[35rem]/tasks:max-w-full`}>{meta}</span>}
        </span>
        {note}
      </span>
      {flags}
      {trailing}
    </button>
    {action}
  </div>;
}
