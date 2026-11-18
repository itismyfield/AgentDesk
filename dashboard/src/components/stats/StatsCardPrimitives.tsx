import type { ComponentPropsWithoutRef, CSSProperties, ReactNode } from "react";
import { Badge } from "../../kit/free/badge-clean";
import { EmptyState } from "../../kit/free/empty-state-clean";
import { cx } from "../dashboard/ui";
import type { MetricDeltaTone } from "./statsModel";

export const NUMERIC_STYLE: CSSProperties = {
  fontVariantNumeric: "tabular-nums",
};

/** Kit-style section: a quiet 12px heading row (icon, title, actions) above its card. */
export function StatsSection({
  id,
  title,
  description,
  icon,
  actions,
  className,
  children,
  testId,
}: {
  id: string;
  title: string;
  description?: string;
  icon?: ReactNode;
  actions?: ReactNode;
  className?: string;
  children: ReactNode;
  testId?: string;
}) {
  return (
    <section aria-labelledby={id} className={cx("flex min-w-0 flex-col gap-2", className)} data-testid={testId}>
      <div className="flex min-h-7 flex-wrap items-center justify-between gap-x-3 gap-y-2">
        <div className="min-w-0">
          <h2 id={id} className="flex min-w-0 items-center gap-1.5 text-xs font-medium text-[var(--fg-dim)] [&_svg]:size-[13px] [&_svg]:shrink-0">
            {icon ? <span aria-hidden="true" className="inline-flex text-[var(--fg-muted)]">{icon}</span> : null}
            <span className="min-w-0 [word-break:keep-all]">{title}</span>
          </h2>
          {description ? <p className="mt-0.5 text-xs leading-5 text-[var(--fg-muted)] [word-break:keep-all]">{description}</p> : null}
        </div>
        {actions ? <div className="flex min-w-0 flex-wrap items-center gap-2">{actions}</div> : null}
      </div>
      {children}
    </section>
  );
}

/** Card surface shared by the stats sections. */
export function StatsCard({ className, children, ...rest }: ComponentPropsWithoutRef<"div">) {
  return (
    <div
      {...rest}
      className={cx(
        "min-w-0 overflow-hidden rounded-[var(--r-lg)] border border-[var(--line-soft)] bg-[var(--bg-1)]",
        className,
      )}
    >
      {children}
    </div>
  );
}

export function StatsSwatch({ color, className }: { color: string; className?: string }) {
  return <span aria-hidden="true" className={cx("inline-block size-2 shrink-0 rounded-[2px]", className)} style={{ background: color }} />;
}

export function DeltaBadge({ tone, children }: { tone?: MetricDeltaTone; children: ReactNode }) {
  return (
    <Badge tone={tone === "up" ? "success" : tone === "down" ? "danger" : "neutral"} style={NUMERIC_STYLE}>
      {children}
    </Badge>
  );
}

/** Empty, zero or loading note inside a card (kit EmptyState without its frame). */
export function StatsEmpty({
  icon,
  title,
  text,
  action,
  testId,
}: {
  icon: ReactNode;
  title: ReactNode;
  text?: ReactNode;
  action?: ReactNode;
  testId?: string;
}) {
  return (
    <div className="px-4 py-8" data-testid={testId}>
      <EmptyState bare icon={icon} title={title} text={text} action={action} />
    </div>
  );
}

/** Thin share meter: relative width against the largest value. */
export function StatsMeter({ ratio, color, className }: { ratio: number; color: string; className?: string }) {
  const width = Math.max(0, Math.min(1, ratio)) * 100;
  return (
    <span aria-hidden="true" className={cx("block h-1.5 min-w-10 overflow-hidden rounded-full bg-[var(--line-soft)]", className)}>
      <span
        className="block h-full rounded-full transition-[width] duration-500 motion-reduce:transition-none"
        style={{ width: `${width > 0 ? Math.max(width, 2) : 0}%`, background: color }}
      />
    </span>
  );
}
