import type { ReactNode } from "react";
import { useSortable } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { GripVertical } from "lucide-react";

export function HomeMetricTile({
  icon,
  title,
  value,
  sub,
  delta,
  deltaTone = "flat",
  accent,
  trend,
}: {
  icon: ReactNode;
  title: string;
  value: string;
  sub: string;
  delta?: string;
  deltaTone?: "up" | "down" | "flat";
  accent: string;
  trend?: number[];
}) {
  const strokePoints =
    trend && trend.length > 1
      ? trend
          .map((point, index) => {
            const max = Math.max(...trend, 1);
            const min = Math.min(...trend, 0);
            const x = (index / (trend.length - 1)) * 100;
            const normalized = max === min ? 0.5 : (point - min) / (max - min);
            const y = 26 - normalized * 20;
            return `${x},${y}`;
          })
          .join(" ")
      : null;
  return (
    <section className="flex h-full min-w-0 flex-col rounded-lg border border-th-border-subtle bg-th-bg-surface px-4 py-4 sm:px-5">
      <div className="flex items-center gap-2 text-xs text-th-text-muted">{icon}<span>{title}</span></div>
      <div className="mt-3 break-words text-[28px] font-semibold leading-9 tracking-tight tabular-nums text-th-text-heading">{value}</div>
      <div className="mt-1 text-xs leading-5 text-th-text-muted">{sub}</div>
      <div className="mt-auto flex flex-wrap items-end justify-between gap-1 pt-3">
        {delta ? <span className={`text-[11px] leading-5 ${deltaTone === "up" ? "text-[var(--th-accent-success)]" : deltaTone === "down" ? "text-[var(--th-accent-danger)]" : "text-th-text-muted"}`}>{delta}</span> : null}
        {strokePoints ? (
          <svg viewBox="0 0 100 30" preserveAspectRatio="none" className="ml-auto h-5 w-16 shrink-0 sm:w-24" aria-hidden="true">
            <polyline fill="none" stroke={accent} strokeWidth="1.8" strokeLinejoin="round" strokeLinecap="round" points={strokePoints} />
          </svg>
        ) : null}
      </div>
    </section>
  );
}

export function HomeWidgetShell({
  title,
  subtitle,
  action,
  children,
}: {
  title: string;
  subtitle: string;
  action?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div
      className="h-full min-w-0 overflow-hidden rounded-lg border"
      style={{
        borderColor: "var(--th-border-subtle)",
        background: "var(--th-bg-surface)",
      }}
    >
      <div className="flex items-start justify-between gap-3 border-b px-4 py-3 sm:px-5" style={{ borderColor: "var(--th-border-subtle)" }}>
        <div className="min-w-0">
          <h2 className="text-base font-semibold" style={{ color: "var(--th-text-secondary)" }}>
            {title}
          </h2>
          <div
            className="mt-1 line-clamp-1 text-[11px] leading-5"
            title={subtitle}
            style={{ color: "var(--th-text-muted)" }}
          >
            {subtitle}
          </div>
        </div>
        {action}
      </div>
      <div className="px-4 py-4 sm:px-5">{children}</div>
    </div>
  );
}

export function HomeSortableWidget({
  widgetId,
  className,
  disabled,
  showHandle,
  activeWidgetId,
  overWidgetId,
  handleLabel,
  children,
}: {
  widgetId: string;
  className: string;
  disabled: boolean;
  showHandle: boolean;
  activeWidgetId: string | null;
  overWidgetId: string | null;
  handleLabel: string;
  children: ReactNode;
}) {
  const {
    attributes,
    isDragging,
    listeners,
    setActivatorNodeRef,
    setNodeRef,
    transform,
    transition,
  } = useSortable({ id: widgetId, disabled });
  const isOver = overWidgetId === widgetId && activeWidgetId !== widgetId;

  return (
    <div
      ref={setNodeRef}
      data-testid={`home-widget-${widgetId}`}
      className={[
        "min-w-0",
        className,
        isDragging ? "opacity-70" : "",
        isOver
          ? "rounded-lg ring-2 ring-[color:var(--th-accent-primary)] ring-offset-2 ring-offset-transparent"
          : "",
      ]
        .filter(Boolean)
        .join(" ")}
      style={{
        transform: CSS.Transform.toString(transform),
        transition: transition ?? undefined,
      }}
    >
      <div className={`relative h-full ${showHandle ? "pt-14" : ""}`}>
        {showHandle ? (
          <button
            ref={setActivatorNodeRef}
            type="button"
            data-testid={`home-drag-handle-${widgetId}`}
            aria-label={handleLabel}
            className="absolute right-2 top-2 z-10 flex h-11 w-11 cursor-grab items-center justify-center rounded-md border transition-colors hover:bg-white/10 active:cursor-grabbing"
            style={{
              borderColor: "var(--th-border-subtle)",
              background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
              color: "var(--th-text-muted)",
              touchAction: "none",
            }}
            {...attributes}
            {...listeners}
          >
            <GripVertical size={14} />
          </button>
        ) : null}
        {children}
      </div>
    </div>
  );
}
