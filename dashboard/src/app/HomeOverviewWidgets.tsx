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
    <div
      className="h-full overflow-hidden rounded-[1.15rem] border"
      style={{
        borderColor: "var(--th-border-subtle)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      <div className="px-4 py-4 sm:px-5">
        <div className="flex items-center justify-between gap-3">
          <div className="flex items-center gap-2 text-[11.5px] font-medium uppercase tracking-[0.08em]" style={{ color: "var(--th-text-muted)" }}>
            {icon}
            <span>{title}</span>
          </div>
          {delta ? (
            <span
              className="rounded-md px-1.5 py-0.5 text-[11px] font-medium"
              style={{
                background:
                  deltaTone === "up"
                    ? "color-mix(in srgb, var(--th-accent-success) 14%, transparent)"
                    : deltaTone === "down"
                      ? "color-mix(in srgb, var(--th-accent-danger) 14%, transparent)"
                      : "var(--th-overlay-medium)",
                color:
                  deltaTone === "up"
                    ? "var(--th-accent-success)"
                    : deltaTone === "down"
                      ? "var(--th-accent-danger)"
                      : "var(--th-text-muted)",
              }}
            >
              {delta}
            </span>
          ) : null}
        </div>
        <div
          className="mt-3 text-[26px] font-semibold tracking-tight"
          style={{ color: "var(--th-text-heading)" }}
        >
          {value}
        </div>
        <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
          {sub}
        </div>
        <svg
          viewBox="0 0 100 30"
          preserveAspectRatio="none"
          className="mt-3 h-8 w-full"
          aria-hidden="true"
        >
          {strokePoints ? (
            <polyline
              fill="none"
              stroke={accent}
              strokeWidth="2"
              strokeLinejoin="round"
              strokeLinecap="round"
              points={strokePoints}
            />
          ) : (
            <line
              x1="0"
              x2="100"
              y1="16"
              y2="16"
              stroke={accent}
              strokeWidth="1.4"
              strokeDasharray="2.5 4"
              strokeLinecap="round"
              opacity="0.55"
            />
          )}
        </svg>
      </div>
    </div>
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
      className="h-full overflow-hidden rounded-[1.15rem] border"
      style={{
        borderColor: "var(--th-border-subtle)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      <div className="flex items-start justify-between gap-3 border-b px-4 py-3 sm:px-5" style={{ borderColor: "var(--th-border-subtle)" }}>
        <div className="min-w-0">
          <div className="truncate text-[12.5px] font-medium" style={{ color: "var(--th-text-secondary)" }}>
            {title}
          </div>
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
        className,
        isDragging ? "opacity-70" : "",
        isOver
          ? "rounded-[2rem] ring-2 ring-[color:var(--th-accent-primary)] ring-offset-2 ring-offset-transparent"
          : "",
      ]
        .filter(Boolean)
        .join(" ")}
      style={{
        transform: CSS.Transform.toString(transform),
        transition: transition ?? undefined,
      }}
    >
      <div className="relative h-full">
        {showHandle ? (
          <button
            ref={setActivatorNodeRef}
            type="button"
            data-testid={`home-drag-handle-${widgetId}`}
            aria-label={handleLabel}
            className="absolute right-4 top-4 z-10 flex h-8 w-8 cursor-grab items-center justify-center rounded-full border transition-colors hover:bg-white/10 active:cursor-grabbing"
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
